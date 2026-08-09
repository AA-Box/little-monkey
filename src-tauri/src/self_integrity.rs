//! Startup self-integrity check (roadmap K22).
//!
//! Before anything native is executed, the app answers two questions about
//! itself: is *this* binary the one that was signed, and is every managed
//! runtime component on disk byte-for-byte the tree its trusted manifest
//! describes? A mismatch is a refusal to load, not a warning — the gate below
//! is consulted by every path that resolves or publishes a managed runtime
//! binary, so a tampered tree cannot be launched even by a caller that never
//! heard of this module.
//!
//! # The three answers that are not failures
//!
//! Tampering is one outcome; the other three have to stay distinguishable from
//! it or the refusal is useless.
//!
//! - **Absent** — nothing to execute, so nothing to refuse. A host that never
//!   installed the stable-diffusion runtime is not a compromised host.
//! - **Unsupported** — upstream publishes no binary for this target, so the
//!   feature is unavailable by construction (see `managed_runtime`'s
//!   `supported_targets`).
//! - **Unverified** — the check itself could not be performed: a source build
//!   with no code signature, a developer's `LITTLE_MONKEY_*_RUNTIME` override,
//!   or a tree staged without a trusted digest baked in. Reported honestly and
//!   loudly, but it is the absence of evidence, not evidence of tampering, and
//!   refusing here would mean no one could run this app from source.
//!
//! Only **mismatch** — a signature that is present and invalid, or a file whose
//! digest disagrees with an authenticated manifest — latches the refusal.
//!
//! # Ordering
//!
//! [`report`] is a `OnceLock`, so the first caller runs the check and every
//! other caller blocks until it finishes. `lib.rs` warms it on a blocking
//! thread during `setup`, which means the common case is warm before a user can
//! ask for anything; a launch that beats the warm-up waits for the verdict
//! rather than racing past it. Nothing inside the check may call back into the
//! gate (that would deadlock on the same `OnceLock`), which is why it uses
//! `managed_runtime::verify_runtime_installation` — the un-gated primitive —
//! rather than `find_managed_server`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::managed_runtime::{
    self, ManagedRuntimeSpec, RuntimeIntegrity, LLAMA, LLAMA_TTS, STABLE_DIFFUSION,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IntegrityStatus {
    /// Authenticated: a valid signature, or every file matching a trusted
    /// manifest digest.
    Verified,
    /// Present and provably wrong. This is the only status that refuses.
    Mismatch,
    /// Nothing installed here, so nothing to authenticate or execute.
    Absent,
    /// This host has no such component by design.
    Unsupported,
    /// The check could not be performed — see `detail`.
    Unverified,
}

impl IntegrityStatus {
    pub fn code(self) -> &'static str {
        match self {
            IntegrityStatus::Verified => "verified",
            IntegrityStatus::Mismatch => "mismatch",
            IntegrityStatus::Absent => "absent",
            IntegrityStatus::Unsupported => "unsupported",
            IntegrityStatus::Unverified => "unverified",
        }
    }
}

/// One thing that was checked: the app's own signature, or one managed runtime.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComponentIntegrity {
    /// Stable identifier — `app` for the bundle itself, otherwise the
    /// `ManagedRuntimeSpec::id` (`llama`, `llama-tts`, `sd`).
    pub id: String,
    /// `signature` or `runtime` — what kind of evidence was asked for.
    pub kind: String,
    pub status: IntegrityStatus,
    /// Why, in one sentence, for every status including `verified`.
    pub detail: String,
    /// What was checked, when there is a path to name.
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntegrityReport {
    pub checked_at_ms: u64,
    /// True when at least one component is a `Mismatch`. While this is true no
    /// managed runtime binary can be resolved, published, or launched.
    pub refused: bool,
    pub components: Vec<ComponentIntegrity>,
}

impl IntegrityReport {
    fn new(components: Vec<ComponentIntegrity>) -> Self {
        IntegrityReport {
            checked_at_ms: now_ms(),
            refused: components
                .iter()
                .any(|component| component.status == IntegrityStatus::Mismatch),
            components,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

static REPORT: OnceLock<IntegrityReport> = OnceLock::new();

/// The startup verdict, computed once per process.
///
/// The first caller pays for the check (hashing every managed runtime file);
/// concurrent callers block on the same `OnceLock` until it lands.
pub fn report() -> &'static IntegrityReport {
    REPORT.get_or_init(|| IntegrityReport::new(check_all(crate::app_paths::data_dir().as_deref())))
}

/// The gate every managed-runtime path goes through.
///
/// `Err` names the components that failed and is user-facing: it is the whole
/// reason the feature the user just asked for is unavailable.
pub fn ensure_loadable() -> Result<(), String> {
    ensure_loadable_with(report())
}

/// [`ensure_loadable`] against a supplied report — the test seam, and the only
/// place the refusal message is written.
pub fn ensure_loadable_with(report: &IntegrityReport) -> Result<(), String> {
    let failed: Vec<&ComponentIntegrity> = report
        .components
        .iter()
        .filter(|component| component.status == IntegrityStatus::Mismatch)
        .collect();
    if failed.is_empty() {
        return Ok(());
    }
    let detail = failed
        .iter()
        .map(|component| format!("{}: {}", component.id, component.detail))
        .collect::<Vec<_>>()
        .join("; ");
    Err(format!(
        "Startup integrity check failed, so Little Monkey refuses to load its native runtimes ({detail}). \
         Reinstall Little Monkey from a trusted download to restore a verified installation."
    ))
}

/// The startup verdict, for the Updates & integrity panel.
///
/// Runs on a blocking thread because the first caller hashes every managed
/// runtime file; once the report exists this returns a clone of it immediately.
#[tauri::command]
pub async fn self_integrity_report() -> Result<IntegrityReport, String> {
    tauri::async_runtime::spawn_blocking(|| report().clone())
        .await
        .map_err(|error| format!("Self-integrity worker failed: {error}"))
}

/// Runs every check. Pure of global state apart from the OS calls it makes, so
/// it can be exercised directly.
fn check_all(app_data_dir: Option<&Path>) -> Vec<ComponentIntegrity> {
    let mut components = vec![app_signature()];
    for spec in [&LLAMA, &LLAMA_TTS, &STABLE_DIFFUSION] {
        components.push(runtime_component(spec, app_data_dir));
    }
    components
}

fn runtime_component(spec: &ManagedRuntimeSpec, app_data_dir: Option<&Path>) -> ComponentIntegrity {
    let (status, detail, path) =
        match managed_runtime::verify_runtime_installation(spec, app_data_dir) {
            RuntimeIntegrity::Verified { server } => (
                IntegrityStatus::Verified,
                format!(
                    "Every file matches the trusted {} {} manifest digest",
                    spec.id, spec.version
                ),
                Some(server),
            ),
            RuntimeIntegrity::Mismatch { path, reason } => {
                (IntegrityStatus::Mismatch, reason, Some(path))
            }
            RuntimeIntegrity::Unverified { path, reason } => {
                (IntegrityStatus::Unverified, reason, path)
            }
            RuntimeIntegrity::Absent => (
                IntegrityStatus::Absent,
                format!("No {} runtime is installed on this host", spec.id),
                None,
            ),
            RuntimeIntegrity::Unsupported => (
                IntegrityStatus::Unsupported,
                format!("No {} build is published for this target", spec.id),
                None,
            ),
        };
    ComponentIntegrity {
        id: spec.id.to_string(),
        kind: "runtime".to_string(),
        status,
        detail,
        path: path.map(|path| path.to_string_lossy().into_owned()),
    }
}

/// The `.app` bundle this executable lives in, if it lives in one at all.
/// A `cargo run`/`cargo test` binary does not, which is exactly the
/// "unverified, not tampered" case.
fn macos_bundle_root() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    executable
        .ancestors()
        .find(|ancestor| {
            ancestor
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app"))
        })
        .map(Path::to_path_buf)
}

fn signature_component(
    status: IntegrityStatus,
    detail: String,
    path: Option<String>,
) -> ComponentIntegrity {
    ComponentIntegrity {
        id: "app".to_string(),
        kind: "signature".to_string(),
        status,
        detail,
        path,
    }
}

/// How `codesign --verify`'s output reads as a verdict.
///
/// Separated from the process spawn so the classification — the part that
/// decides whether the app refuses to load — is testable without a signed
/// bundle to hand.
fn classify_codesign(success: bool, output: &str) -> (IntegrityStatus, String) {
    if success {
        return (
            IntegrityStatus::Verified,
            "The app bundle's code signature is valid".to_string(),
        );
    }
    let lowered = output.to_ascii_lowercase();
    if lowered.contains("not signed at all") || lowered.contains("is not signed") {
        return (
            IntegrityStatus::Unverified,
            "This build carries no code signature (expected for a build from source)".to_string(),
        );
    }
    (
        IntegrityStatus::Mismatch,
        format!(
            "The app bundle's code signature does not verify: {}",
            first_line(output)
        ),
    )
}

/// How PowerShell's `Get-AuthenticodeSignature` status reads as a verdict.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn classify_authenticode(status: &str) -> (IntegrityStatus, String) {
    match status.trim() {
        "Valid" => (
            IntegrityStatus::Verified,
            "The executable's Authenticode signature is valid".to_string(),
        ),
        "NotSigned" | "" => (
            IntegrityStatus::Unverified,
            "This build carries no Authenticode signature (expected for a build from source)"
                .to_string(),
        ),
        other => (
            IntegrityStatus::Mismatch,
            format!("The executable's Authenticode signature is {other}"),
        ),
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no detail reported")
        .to_string()
}

#[cfg(target_os = "macos")]
fn app_signature() -> ComponentIntegrity {
    let Some(bundle) = macos_bundle_root() else {
        return signature_component(
            IntegrityStatus::Unverified,
            "Not running from an .app bundle, so there is no signature to verify".to_string(),
            std::env::current_exe()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
        );
    };
    // `--strict` (not `--deep`): the seal already covers every nested file, and
    // `--deep` re-verifies each one, which is seconds of startup for an answer
    // the seal has already given.
    let output = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--"])
        .arg(&bundle)
        .output();
    let path = Some(bundle.to_string_lossy().into_owned());
    match output {
        Ok(output) => {
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            let (status, detail) = classify_codesign(output.status.success(), &text);
            signature_component(status, detail, path)
        }
        Err(error) => signature_component(
            IntegrityStatus::Unverified,
            format!("codesign could not be run: {error}"),
            path,
        ),
    }
}

#[cfg(target_os = "windows")]
fn app_signature() -> ComponentIntegrity {
    let Ok(executable) = std::env::current_exe() else {
        return signature_component(
            IntegrityStatus::Unverified,
            "The running executable's path could not be resolved".to_string(),
            None,
        );
    };
    let path = Some(executable.to_string_lossy().into_owned());
    // `-LiteralPath` so a path containing `[` or `]` is not read as a wildcard.
    let script = format!(
        "(Get-AuthenticodeSignature -LiteralPath '{}').Status",
        executable.to_string_lossy().replace('\'', "''")
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output();
    match output {
        Ok(output) if output.status.success() => {
            let (status, detail) = classify_authenticode(&String::from_utf8_lossy(&output.stdout));
            signature_component(status, detail, path)
        }
        Ok(output) => signature_component(
            IntegrityStatus::Unverified,
            format!(
                "Get-AuthenticodeSignature failed: {}",
                first_line(&String::from_utf8_lossy(&output.stderr))
            ),
            path,
        ),
        Err(error) => signature_component(
            IntegrityStatus::Unverified,
            format!("PowerShell could not be run: {error}"),
            path,
        ),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn app_signature() -> ComponentIntegrity {
    // Linux has no OS-level executable signature to check: distribution
    // packages are signed by the repository, not the binary, and an AppImage's
    // own signature is not verified by any component of the running system.
    // The managed runtime digests below are the whole of the evidence here.
    signature_component(
        IntegrityStatus::Unverified,
        "Linux has no OS-level code signature to verify; managed runtime digests are the evidence on this platform"
            .to_string(),
        std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().into_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(id: &str, status: IntegrityStatus) -> ComponentIntegrity {
        ComponentIntegrity {
            id: id.to_string(),
            kind: "runtime".to_string(),
            status,
            detail: format!("{id} is {}", status.code()),
            path: None,
        }
    }

    #[test]
    fn only_a_mismatch_refuses() {
        for status in [
            IntegrityStatus::Verified,
            IntegrityStatus::Absent,
            IntegrityStatus::Unsupported,
            IntegrityStatus::Unverified,
        ] {
            let report = IntegrityReport::new(vec![component("llama", status)]);
            assert!(!report.refused, "{status:?} must not refuse");
            assert!(
                ensure_loadable_with(&report).is_ok(),
                "{status:?} must load"
            );
        }
        let report = IntegrityReport::new(vec![component("llama", IntegrityStatus::Mismatch)]);
        assert!(report.refused);
        let error = ensure_loadable_with(&report).unwrap_err();
        assert!(error.contains("refuses to load"), "{error}");
        assert!(error.contains("llama"), "{error}");
    }

    #[test]
    fn one_mismatched_component_refuses_every_runtime() {
        // The refusal is global on purpose: a tampered llama tree is evidence
        // about the installation, not about llama, so the stable-diffusion
        // runtime beside it does not get to launch either.
        let report = IntegrityReport::new(vec![
            component("app", IntegrityStatus::Verified),
            component("llama", IntegrityStatus::Verified),
            component("sd", IntegrityStatus::Mismatch),
        ]);
        assert!(ensure_loadable_with(&report).is_err());
    }

    #[test]
    fn an_unsigned_build_is_unverified_but_a_broken_seal_is_a_mismatch() {
        assert_eq!(
            classify_codesign(false, "test.app: code object is not signed at all").0,
            IntegrityStatus::Unverified
        );
        assert_eq!(
            classify_codesign(
                false,
                "test.app: a sealed resource is missing or invalid\nfile modified: Contents/MacOS/app"
            )
            .0,
            IntegrityStatus::Mismatch
        );
        assert_eq!(classify_codesign(true, "").0, IntegrityStatus::Verified);
    }

    #[test]
    fn authenticode_reports_a_hash_mismatch_as_tampering() {
        assert_eq!(
            classify_authenticode("Valid\r\n").0,
            IntegrityStatus::Verified
        );
        assert_eq!(
            classify_authenticode("NotSigned").0,
            IntegrityStatus::Unverified
        );
        let (status, detail) = classify_authenticode("HashMismatch");
        assert_eq!(status, IntegrityStatus::Mismatch);
        assert!(detail.contains("HashMismatch"), "{detail}");
    }

    #[test]
    fn the_real_check_runs_and_never_refuses_a_source_build() {
        // The check must be safe to run anywhere, including a test binary that
        // is not signed and has no runtimes installed beside it.
        let report = IntegrityReport::new(check_all(None));
        assert_eq!(report.components.len(), 4);
        assert_eq!(report.components[0].kind, "signature");
        assert!(
            !report.refused,
            "a source build must not refuse to load: {:?}",
            report.components
        );
    }
}

//! Tauri-free security posture audit shared by the desktop Security Doctor
//! and `monkey security audit`.
//!
//! The audit is deliberately conservative. A normal run is read-only. The
//! optional fix pass can only tighten Unix permissions on known app-owned
//! paths or disable an enabled, clearly unsafe app-owned MCP/remote-listener
//! configuration. It never deletes files, follows symlinks, rewrites a
//! workspace, rotates credentials, or enables a capability.

use std::collections::BTreeMap;
#[cfg(unix)]
use std::collections::BTreeSet;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;
use uuid::Uuid;
#[cfg(unix)]
use walkdir::WalkDir;

use crate::executable_extensions::{
    extension_security_snapshots, CapabilityKind, ExtensionSecuritySnapshot, HealthState,
    PermissionKind, PermissionRisk, TrustState,
};
use crate::sandbox::SandboxEnforcement;

pub const SECURITY_AUDIT_SCHEMA_VERSION: u32 = 1;
const MAX_DEEP_PATHS: usize = 8_192;
const MAX_CONFIG_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CAPTURE_GRANT_MS: u64 = 60 * 60 * 1_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Pass,
    Info,
    Warning,
    Critical,
    Fixed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecurityFinding {
    pub id: String,
    pub category: String,
    pub title: String,
    pub detail: String,
    pub status: FindingStatus,
    pub fixable: bool,
    pub path: Option<String>,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecuritySummary {
    pub passed: usize,
    pub informational: usize,
    pub warnings: usize,
    pub critical: usize,
    pub fixed: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SecurityAuditReport {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub deep: bool,
    pub fix_requested: bool,
    pub summary: SecuritySummary,
    pub findings: Vec<SecurityFinding>,
}

/// One paired physical device, as the runner's own state describes it.
///
/// Passed in rather than read here: the queue and the grants live in the
/// daemon's SQLite database, whose schema `monkey-cli` owns. A second reader in
/// this library would be a second copy of that schema, and the first migration
/// would make the audit quietly wrong. The CLI, which already owns the store,
/// collects this and hands it over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceGrantSnapshot {
    pub device_id: String,
    pub device_name: String,
    /// Physical capabilities the operator granted, as wire tokens.
    pub granted_physical: Vec<String>,
    /// Of those, the ones currently effective.
    pub effective_physical: Vec<String>,
    pub revoked: bool,
    /// When the device last reported its surface, if ever.
    pub last_seen_at_ms: Option<u64>,
    /// Whether this device can be woken by push.
    pub push_registered: bool,
}

/// One device command that has not finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCommandSnapshot {
    pub command_id: String,
    pub device_id: String,
    pub capability: String,
    pub state: String,
}

/// The operator's push configuration, reduced to what the audit asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushPrivacySnapshot {
    pub configured: bool,
    pub enabled: bool,
    /// True when notifications are allowed to carry specifics onto a lock
    /// screen.
    pub include_detail: bool,
    pub registered_devices: usize,
}

/// The operator's voice configuration, reduced to the three questions the audit
/// asks: is anything listening without being asked, is it opt-in, and could
/// what it hears leave the machine.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VoicePrivacySnapshot {
    pub wake_phrase_enabled: bool,
    pub always_listening: bool,
    /// True when transcription runs on this machine. False means audio is
    /// uploaded to a provider the operator configured.
    pub local_only: bool,
}

/// Everything about the machine's security posture that only the daemon can
/// see, as one value.
///
/// # Why this type exists at all
///
/// Three subsystems — paired devices, messaging accounts, phone numbers and
/// peers — keep their state in databases whose schemas `monkey-cli` owns, and
/// [`run_security_audit`] runs in this library, which cannot open them. The CLI
/// therefore collected that half itself and appended it to its own report,
/// which meant `monkey security audit` and the desktop Security Doctor were
/// answering different questions: the desktop panel saw no device, no channel,
/// no number and no peer, and said so by omission rather than by saying
/// anything. An operator reading a clean page had no way to know a whole class
/// of check had not run.
///
/// So the daemon-owned half became a value with a wire form. The CLI produces
/// it in one place (`monkey security daemon-state --json`), the desktop reads
/// exactly that, and both then run the same audit over the same inputs. Adding
/// a daemon-owned check now reaches both surfaces or neither.
///
/// # Findings, not just inputs
///
/// Two kinds of thing travel here and they are deliberately not merged. The
/// snapshot fields are *inputs* the library's own audit functions reason about.
/// `findings` are already-decided results from the audits that live in the CLI
/// because their state does. Asking the library to re-derive those would mean
/// teaching it the schemas this type exists to avoid teaching it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonSecurityState {
    pub schema_version: u32,
    pub devices: Vec<DeviceGrantSnapshot>,
    #[serde(default)]
    pub device_commands: Vec<DeviceCommandSnapshot>,
    pub device_state_observed: bool,
    #[serde(default)]
    pub device_state_error: Option<String>,
    #[serde(default)]
    pub push: Option<PushPrivacySnapshot>,
    #[serde(default)]
    pub transport: Option<TransportSnapshot>,
    /// Findings the daemon's own audits produced: channels, telephony, peers.
    #[serde(default)]
    pub findings: Vec<SecurityFinding>,
}

impl DaemonSecurityState {
    /// Fold the input half into a runtime snapshot, and hand back the findings.
    ///
    /// Returns the findings rather than swallowing them so the caller decides
    /// when they join the report — they must be appended *after*
    /// [`run_security_audit`] has produced its summary, and
    /// [`append_findings`] is what keeps that summary honest.
    #[must_use]
    pub fn apply(self, runtime: &mut SecurityRuntimeSnapshot) -> Vec<SecurityFinding> {
        runtime.devices = self.devices;
        runtime.device_commands = self.device_commands;
        runtime.device_state_observed = self.device_state_observed;
        runtime.device_state_error = self.device_state_error;
        runtime.push = self.push;
        runtime.transport = self.transport;
        self.findings
    }
}

/// Add findings produced outside [`run_security_audit`] to its report, keeping
/// the summary counts true.
///
/// Shared rather than copied into each caller: a summary that disagrees with
/// the list under it is the one bug in a report nobody notices, because both
/// halves look plausible on their own.
pub fn append_findings(report: &mut SecurityAuditReport, findings: Vec<SecurityFinding>) {
    for finding in findings {
        match finding.status {
            FindingStatus::Pass => report.summary.passed += 1,
            FindingStatus::Info => report.summary.informational += 1,
            FindingStatus::Warning => report.summary.warnings += 1,
            FindingStatus::Critical => report.summary.critical += 1,
            FindingStatus::Fixed => report.summary.fixed += 1,
        }
        report.findings.push(finding);
    }
}

#[derive(Debug, Clone, Default)]
pub struct SecurityRuntimeSnapshot {
    pub browser_grants: Vec<BrowserGrantSnapshot>,
    pub browser_observed: bool,
    pub browser_error: Option<String>,
    pub companion_grants: Vec<CompanionGrantSnapshot>,
    pub companion_observed: bool,
    pub companion_error: Option<String>,
    pub native_skills: Vec<NativeSkillSnapshot>,
    pub native_skills_error: Option<String>,
    pub devices: Vec<DeviceGrantSnapshot>,
    pub device_commands: Vec<DeviceCommandSnapshot>,
    pub device_state_observed: bool,
    pub device_state_error: Option<String>,
    pub push: Option<PushPrivacySnapshot>,
    /// How a device reaches this runner, as the runner advertises it.
    ///
    /// Separate from `audit_remote_host`'s own checks because the question this
    /// answers is a different one: that function asks whether the *listener* is
    /// configured safely, and this asks whether the transport a phone with a
    /// camera grant is talking over is pinned at all. A development listener on
    /// plain loopback is a reasonable thing to have and an unreasonable thing to
    /// hand a microphone.
    pub transport: Option<TransportSnapshot>,
    pub voice: Option<VoicePrivacySnapshot>,
}

/// The advertised transport, reduced to what the device audit asks about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransportSnapshot {
    pub enabled: bool,
    pub advertise_url: String,
    /// Whether the runner holds a certificate fingerprint for devices to pin.
    pub pinned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserGrantSnapshot {
    pub session_id: String,
    pub run_id: String,
    pub allowed_origins: Vec<String>,
    pub allow_loopback: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionGrantSnapshot {
    pub grant_id: String,
    pub kind: String,
    pub application_id: Option<String>,
    pub expires_at_ms: u64,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSkillSnapshot {
    pub command: String,
    pub source: String,
    pub enabled: bool,
    pub eligible: bool,
    pub missing_bins: Vec<String>,
    pub missing_env: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SecurityAuditRequest {
    pub app_data_dir: PathBuf,
    pub workspace: Option<PathBuf>,
    pub deep: bool,
    pub fix: bool,
    pub runtime: SecurityRuntimeSnapshot,
}

pub fn run_security_audit(request: &SecurityAuditRequest) -> Result<SecurityAuditReport, String> {
    let app_data = &request.app_data_dir;
    match fs::symlink_metadata(app_data) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            return Err(format!(
                "Little Monkey app data '{}' must be a real directory",
                app_data.display()
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Could not inspect Little Monkey app data '{}': {error}",
                app_data.display()
            ));
        }
    }

    let mut findings = Vec::new();
    audit_owned_permissions(app_data, request.deep, request.fix, &mut findings);
    audit_loopback_services(app_data, &mut findings);
    audit_remote_host(app_data, request.deep, request.fix, &mut findings);
    audit_mcp_origins(app_data, request.fix, &mut findings);
    audit_executable_extensions(app_data, &mut findings);
    audit_native_skills(&request.runtime, &mut findings);
    audit_runtime_grants(&request.runtime, &mut findings);
    audit_paired_devices(&request.runtime, &mut findings);
    audit_voice_privacy(&request.runtime, &mut findings);
    audit_workspace_skill_root(request.workspace.as_deref(), &mut findings);
    audit_sandbox_enforcement(&mut findings);

    let summary = summarize(&findings);
    Ok(SecurityAuditReport {
        schema_version: SECURITY_AUDIT_SCHEMA_VERSION,
        generated_at_ms: now_ms(),
        deep: request.deep,
        fix_requested: request.fix,
        summary,
        findings,
    })
}

fn audit_executable_extensions(app_data: &Path, findings: &mut Vec<SecurityFinding>) {
    match extension_security_snapshots(app_data) {
        Ok(snapshots) => {
            audit_extension_snapshots(&snapshots, findings);
            audit_extension_provider_selections(app_data, &snapshots, findings);
        }
        Err(error) => findings.push(finding(
            "extensions.store_invalid",
            "extensions",
            "Executable extension state failed closed",
            &error,
            FindingStatus::Critical,
            false,
            None,
            Some("Keep executable extensions disabled and repair or remove the corrupt app-owned extension registry."),
        )),
    }
}

/// Persisted provider selections that no longer resolve to an owner.
///
/// Every native subsystem that can be pointed at an extension records *which
/// installation* it chose, and re-checks that ownership on every use — so a
/// stale selection is safe: it fails closed. Safe is not the same as
/// harmless, though. A transcription backend that silently stopped
/// transcribing, or a knowledge stack that will not re-embed, reads to an
/// operator as a broken feature rather than as an uninstalled extension, and
/// this is the one place that says which it is.
fn audit_extension_provider_selections(
    app_data: &Path,
    snapshots: &[ExtensionSecuritySnapshot],
    findings: &mut Vec<SecurityFinding>,
) {
    let owned: std::collections::BTreeSet<(CapabilityKind, String)> = snapshots
        .iter()
        // Only a healthy extension actually serves a provider registry, so an
        // unhealthy owner is an orphaned selection too.
        .filter(|snapshot| snapshot.health.state == HealthState::Healthy)
        .flat_map(|snapshot| snapshot.capabilities.iter().cloned())
        .collect();

    let mut orphaned: Vec<String> = Vec::new();
    let mut check = |kind: CapabilityKind, selection: Option<(String, String)>, label: &str| {
        if let Some((extension_id, capability_id)) = selection {
            if !owned.contains(&(kind, capability_id.clone())) {
                orphaned.push(format!("{label} → {extension_id}:{capability_id}"));
            }
        }
    };

    let voice = crate::m7_companion::persisted_voice_selections(app_data);
    check(CapabilityKind::Stt, voice.transcription, "Transcription");
    check(CapabilityKind::Tts, voice.speech, "Speech synthesis");
    check(
        CapabilityKind::RealtimeVoice,
        voice.realtime,
        "Realtime voice",
    );
    let web = crate::web::persisted_extension_selections(app_data);
    check(CapabilityKind::WebSearch, web.search, "Web search");
    check(CapabilityKind::WebFetch, web.fetch, "Web fetch");
    for (label, selection) in crate::knowledge_core::persisted_embedding_selections(app_data) {
        check(CapabilityKind::EmbeddingProvider, Some(selection), &label);
    }

    if orphaned.is_empty() {
        return;
    }
    findings.push(finding(
        "extensions.provider_orphaned",
        "extensions",
        "A feature is pointed at an extension that no longer owns its capability",
        &format!(
            "These selections resolve to nothing healthy and therefore do nothing: {}.",
            orphaned.join("; ")
        ),
        FindingStatus::Warning,
        false,
        None,
        Some("Reinstall or re-enable the owning extension, or choose a different provider in that feature's settings."),
    ));
}

fn audit_extension_snapshots(
    snapshots: &[ExtensionSecuritySnapshot],
    findings: &mut Vec<SecurityFinding>,
) {
    if snapshots.is_empty() {
        findings.push(finding(
            "extensions.none",
            "extensions",
            "No executable extensions are installed",
            "The Wasm extension runtime has no installed third-party components.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    for snapshot in snapshots {
        let suffix = short_hash(snapshot.extension_id.as_bytes());
        match snapshot.trust {
            TrustState::Verified => findings.push(finding(
                &format!("extensions.trust.{suffix}"),
                "extensions",
                "Extension signature is verified",
                &format!(
                    "{} {} was verified against an authorized publisher key.",
                    snapshot.extension_id, snapshot.version
                ),
                FindingStatus::Pass,
                false,
                None,
                None,
            )),
            TrustState::Unsigned => findings.push(finding(
                &format!("extensions.unsigned.{suffix}"),
                "extensions",
                "Unsigned executable extension is installed",
                &format!(
                    "{} {} is unsigned: {}",
                    snapshot.extension_id, snapshot.version, snapshot.trust_reason
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Install a publisher-signed build from a trusted source, or disable and uninstall this extension."),
            )),
            TrustState::Untrusted | TrustState::Invalid => findings.push(finding(
                &format!("extensions.untrusted.{suffix}"),
                "extensions",
                "Executable extension is not trusted",
                &format!(
                    "{} {}: {}",
                    snapshot.extension_id, snapshot.version, snapshot.trust_reason
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Disable the extension and verify its publisher, provenance, signature, and checksums before use."),
            )),
        }

        if !snapshot.compatible {
            findings.push(finding(
                &format!("extensions.incompatible.{suffix}"),
                "extensions",
                "Installed extension is incompatible",
                snapshot
                    .compatibility_reason
                    .as_deref()
                    .unwrap_or("The host compatibility contract does not match."),
                if snapshot.health.enabled {
                    FindingStatus::Critical
                } else {
                    FindingStatus::Warning
                },
                false,
                None,
                Some("Keep the extension disabled and install a host-compatible version."),
            ));
        }

        let elevated = snapshot
            .permissions
            .iter()
            .filter(|permission| {
                permission.granted
                    && matches!(
                        permission.risk,
                        PermissionRisk::High | PermissionRisk::Critical
                    )
            })
            .count();
        if elevated > 0 {
            findings.push(finding(
                &format!("extensions.elevated_grants.{suffix}"),
                "extensions",
                "Extension has elevated resource grants",
                &format!(
                    "{} has {elevated} exact high/critical-risk grant(s). Review each origin, workspace handle, secret slot, and device capability in Settings.",
                    snapshot.extension_id
                ),
                if snapshot.health.enabled {
                    FindingStatus::Warning
                } else {
                    FindingStatus::Info
                },
                false,
                None,
                Some("Remove grants that are no longer required; permission-expanding updates require a new exact approval."),
            ));
        }

        let insecure_origins = snapshot
            .permissions
            .iter()
            .filter(|permission| {
                permission.granted
                    && permission.kind == PermissionKind::NetworkOrigin
                    && Url::parse(&permission.scope).is_ok_and(|url| url.scheme() == "http")
            })
            .count();
        if insecure_origins > 0 {
            findings.push(finding(
                &format!("extensions.plaintext_origins.{suffix}"),
                "extensions",
                "Extension can use plaintext HTTP origins",
                &format!(
                    "{} has {insecure_origins} exact plaintext origin grant(s).",
                    snapshot.extension_id
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Prefer exact HTTPS origins and remove plaintext grants."),
            ));
        }

        let has_network = snapshot.permissions.iter().any(|permission| {
            permission.granted && permission.kind == PermissionKind::NetworkOrigin
        });
        let has_secret = snapshot.configured_secret_slots > 0
            && snapshot.permissions.iter().any(|permission| {
                permission.granted && permission.kind == PermissionKind::SecretUse
            });
        if has_network && has_secret {
            findings.push(finding(
                &format!("extensions.secret_network.{suffix}"),
                "extensions",
                "Extension combines secret-backed authentication with network access",
                &format!(
                    "{} has configured secret slots and exact network origins. Secret bytes remain host-owned, but this combined authority deserves review.",
                    snapshot.extension_id
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Confirm every origin and secret slot is required and remove stale credentials."),
            ));
        }

        if snapshot.health.undeclared_attempts > 0 {
            findings.push(finding(
                &format!("extensions.undeclared.{suffix}"),
                "extensions",
                "Extension attempted undeclared resource access",
                &format!(
                    "{} recorded {} denied undeclared attempt(s).",
                    snapshot.extension_id, snapshot.health.undeclared_attempts
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Disable the extension and inspect its bounded runtime logs before trusting it again."),
            ));
        }

        if !snapshot.component_intact {
            findings.push(finding(
                &format!("extensions.component_missing.{suffix}"),
                "extensions",
                "Installed extension component is missing or modified",
                &format!(
                    "{} {} is registered, but its component file is absent or no longer matches the digest its manifest promised.",
                    snapshot.extension_id, snapshot.version
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Nothing will run from this version — every invocation re-verifies the digest. Reinstall the extension from a verified bundle, or uninstall it."),
            ));
        }

        match snapshot.health.state {
            HealthState::ProtectiveDisabled | HealthState::Unhealthy => findings.push(finding(
                &format!("extensions.health.{suffix}"),
                "extensions",
                "Extension runtime is unhealthy",
                &format!(
                    "{} is {:?} after {} consecutive failure(s) and {} trap(s).",
                    snapshot.extension_id,
                    snapshot.health.state,
                    snapshot.health.consecutive_failures,
                    snapshot.health.trap_count
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Keep it stopped; validate or roll back to a previously verified cached version before re-enabling."),
            )),
            HealthState::Degraded => findings.push(finding(
                &format!("extensions.health.{suffix}"),
                "extensions",
                "Extension runtime is degraded",
                &format!(
                    "{} has {} consecutive failure(s) and {} trap(s).",
                    snapshot.extension_id,
                    snapshot.health.consecutive_failures,
                    snapshot.health.trap_count
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Review the bounded logs and stop or roll back the extension if failures continue."),
            )),
            _ => {}
        }
    }
}

fn audit_owned_permissions(
    app_data: &Path,
    deep: bool,
    fix: bool,
    findings: &mut Vec<SecurityFinding>,
) {
    #[cfg(not(unix))]
    {
        let _ = (app_data, deep, fix);
        findings.push(finding(
            "storage.platform_acl",
            "storage",
            "Platform-managed access controls",
            "Unix mode-bit checks are not applicable on this platform. Little Monkey continues to rely on the operating system's per-user application-data ACLs.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    }

    #[cfg(unix)]
    {
        let mut paths = BTreeSet::<PathBuf>::new();
        paths.insert(app_data.to_path_buf());
        for name in [
            "api_server.json",
            "mcp_servers.json",
            "providers.json",
            "web_settings.json",
            "memories.json",
            "sessions.json",
            "prompts.json",
            "automations.json",
            "profile-v1.sqlite3",
            "profile-v1.sqlite3-wal",
            "profile-v1.sqlite3-shm",
        ] {
            paths.insert(app_data.join(name));
        }
        for relative in [
            "daemon/config.json",
            "daemon/daemon-v1.sqlite3",
            "daemon/daemon.lock",
            "daemon/remote-host.json",
            "daemon/remote-server-cert.pem",
            "daemon/remote-server-key.pem",
            "native-skills-v1/global/.littlemonkey-skills-state-v1.json",
            "m7-companion-v1/companion-config-v1.json",
            "m7-companion-v1/image-gallery-v1.json",
        ] {
            paths.insert(app_data.join(relative));
        }
        let sensitive_roots = [
            app_data.join("daemon"),
            app_data.join("native-skills-v1"),
            app_data.join("browser-v1"),
            app_data.join("m7-companion-v1"),
            app_data.join("content-v1"),
        ];
        for root in &sensitive_roots {
            paths.insert(root.clone());
            if deep && root.exists() {
                for entry in WalkDir::new(root)
                    .follow_links(false)
                    .max_depth(16)
                    .into_iter()
                    .take(MAX_DEEP_PATHS)
                {
                    match entry {
                        Ok(entry) => {
                            paths.insert(entry.path().to_path_buf());
                        }
                        Err(error) => findings.push(finding(
                            &format!("storage.walk.{}", short_hash(error.to_string().as_bytes())),
                            "storage",
                            "Could not inspect a protected directory",
                            &error.to_string(),
                            FindingStatus::Warning,
                            false,
                            None,
                            Some("Check ownership and access permissions for the reported application-data path."),
                        )),
                    }
                }
            }
        }

        let mut checked = 0usize;
        let mut issues = 0usize;
        for path in paths {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    issues += 1;
                    findings.push(path_finding(
                        "storage.inspect",
                        "storage",
                        "Could not inspect an app-owned path",
                        &error.to_string(),
                        FindingStatus::Warning,
                        false,
                        &path,
                        Some("Restore ownership of this path to the current user."),
                    ));
                    continue;
                }
            };
            checked += 1;
            if metadata.file_type().is_symlink() {
                issues += 1;
                findings.push(path_finding(
                    "storage.symlink",
                    "storage",
                    "Symlink found in a protected app-data location",
                    "Security Doctor will not follow or modify this symlink.",
                    FindingStatus::Critical,
                    false,
                    &path,
                    Some("Inspect the link target manually, stop Little Monkey, and replace it with an owned regular file or directory if it is unexpected."),
                ));
                continue;
            }
            if !metadata.is_dir() && !metadata.is_file() {
                issues += 1;
                findings.push(path_finding(
                    "storage.special",
                    "storage",
                    "Special file found in protected app data",
                    "Only regular files and directories are expected in this location.",
                    FindingStatus::Critical,
                    false,
                    &path,
                    Some(
                        "Inspect this path manually. Security Doctor never removes special files.",
                    ),
                ));
                continue;
            }
            use std::os::unix::fs::PermissionsExt;
            let current = metadata.permissions().mode() & 0o777;
            let expected = if metadata.is_dir() { 0o700 } else { 0o600 };
            if current & 0o077 == 0 {
                continue;
            }
            issues += 1;
            let detail = format!(
                "Mode {:03o} allows group or other users to access this app-owned {}.",
                current,
                if metadata.is_dir() {
                    "directory"
                } else {
                    "file"
                }
            );
            if fix && path.starts_with(app_data) {
                match fs::set_permissions(&path, fs::Permissions::from_mode(expected)) {
                    Ok(()) => findings.push(path_finding(
                        "storage.mode",
                        "storage",
                        "Restricted an app-owned path",
                        &format!("{detail} Security Doctor changed it to {expected:03o}."),
                        FindingStatus::Fixed,
                        true,
                        &path,
                        None,
                    )),
                    Err(error) => findings.push(path_finding(
                        "storage.mode",
                        "storage",
                        "App-owned path permissions are too broad",
                        &format!("{detail} The safe fix failed: {error}"),
                        FindingStatus::Critical,
                        true,
                        &path,
                        Some("Restrict this path to the current user (0700 for directories or 0600 for files)."),
                    )),
                }
            } else {
                findings.push(path_finding(
                    "storage.mode",
                    "storage",
                    "App-owned path permissions are too broad",
                    &detail,
                    FindingStatus::Warning,
                    true,
                    &path,
                    Some("Run Security Doctor with safe fixes to restrict this path to the current user."),
                ));
            }
        }
        if issues == 0 {
            findings.push(finding(
                "storage.private",
                "storage",
                "App-owned sensitive paths are private",
                &format!(
                    "Checked {checked} existing path(s){}; none grant group or other-user access.",
                    if deep { " recursively" } else { "" }
                ),
                FindingStatus::Pass,
                false,
                None,
                None,
            ));
        }
    }
}

fn audit_loopback_services(app_data: &Path, findings: &mut Vec<SecurityFinding>) {
    findings.push(finding(
        "network.local_api_loopback",
        "network",
        "Local API is loopback-only",
        "The desktop and headless local API listener binds to 127.0.0.1; the saved configuration has no public bind-address option.",
        FindingStatus::Pass,
        false,
        None,
        None,
    ));
    match crate::server::load_config_impl(&app_data.join("api_server.json")) {
        Ok(config) if config.autostart && !config.require_token => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Autostarted local API does not require a token",
            "The listener remains loopback-only, but any process running as this user can call it while it is active.",
            FindingStatus::Warning,
            false,
            Some(app_data.join("api_server.json").to_string_lossy().as_ref()),
            Some("Enable token authentication in Settings > API server."),
        )),
        Ok(config) if config.require_token => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Local API authentication is enabled",
            "Saved tokens are stored as digests and requests require a bearer token.",
            FindingStatus::Pass,
            false,
            None,
            None,
        )),
        Ok(_) => findings.push(finding(
            "network.local_api_auth",
            "network",
            "Local API token authentication is optional",
            "The API is loopback-only and is not configured to autostart. Enable token authentication before leaving it running for other local applications.",
            FindingStatus::Info,
            false,
            None,
            None,
        )),
        Err(error) => findings.push(path_finding(
            "network.local_api_config",
            "network",
            "Local API configuration is invalid",
            &error,
            FindingStatus::Critical,
            false,
            &app_data.join("api_server.json"),
            Some("Repair the configuration in Settings before starting the API server."),
        )),
    }

    let daemon_config = app_data.join("daemon").join("config.json");
    match read_bounded_json(&daemon_config) {
        Ok(Some(value)) => {
            let webhook = value.get("webhook_port").and_then(Value::as_u64);
            if let Some(port) = webhook {
                findings.push(finding(
                    "network.webhook_loopback",
                    "network",
                    "Signed webhook listener is loopback-only",
                    &format!("The configured webhook listener on port {port} binds only to 127.0.0.1 and verifies each delivery signature."),
                    FindingStatus::Pass,
                    false,
                    None,
                    None,
                ));
            } else {
                findings.push(finding(
                    "network.webhook_disabled",
                    "network",
                    "Webhook listener is disabled",
                    "No daemon webhook port is configured.",
                    FindingStatus::Info,
                    false,
                    None,
                    None,
                ));
            }
        }
        Ok(None) => findings.push(finding(
            "network.webhook_not_installed",
            "network",
            "Background webhook listener is not installed",
            "No daemon configuration exists.",
            FindingStatus::Info,
            false,
            None,
            None,
        )),
        Err(error) => findings.push(path_finding(
            "network.webhook_config",
            "network",
            "Daemon configuration could not be audited",
            &error,
            FindingStatus::Warning,
            false,
            &daemon_config,
            Some("Repair or reinstall the background-agent service."),
        )),
    }
}

fn audit_remote_host(app_data: &Path, deep: bool, fix: bool, findings: &mut Vec<SecurityFinding>) {
    let config_path = app_data.join("daemon").join("remote-host.json");
    let mut value = match read_bounded_json(&config_path) {
        Ok(Some(value)) => value,
        Ok(None) => {
            findings.push(finding(
                "remote.not_configured",
                "remote",
                "Remote host is not configured",
                "No user-owned remote runner listener is enabled.",
                FindingStatus::Info,
                false,
                None,
                None,
            ));
            return;
        }
        Err(error) => {
            findings.push(path_finding(
                "remote.config_invalid",
                "remote",
                "Remote host configuration is unreadable",
                &error,
                FindingStatus::Critical,
                false,
                &config_path,
                Some("Disable the remote host from Settings or repair this app-owned JSON file."),
            ));
            return;
        }
    };
    let enabled = match value.get("enabled").and_then(Value::as_bool) {
        Some(enabled) => enabled,
        None => {
            findings.push(path_finding(
                "remote.enabled_invalid",
                "remote",
                "Remote host enabled state is invalid",
                "The configuration does not contain a Boolean enabled field.",
                FindingStatus::Critical,
                false,
                &config_path,
                Some("Reconfigure the remote host from Settings."),
            ));
            return;
        }
    };
    if !enabled {
        findings.push(finding(
            "remote.disabled",
            "remote",
            "Remote host is disabled",
            "The persisted listener configuration is inert until explicitly enabled again.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    let daemon_root = app_data.join("daemon");
    let mut unsafe_reasons = Vec::<String>::new();
    let advertise = value
        .get("advertise_url")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match Url::parse(advertise) {
        Ok(url)
            if url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && matches!(url.path(), "" | "/")
                && url.query().is_none()
                && url.fragment().is_none() => {}
        _ => unsafe_reasons
            .push("the advertised runner URL is not a credential-free HTTPS origin".to_string()),
    }
    let listen = value
        .get("listen")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if listen.parse::<std::net::SocketAddr>().is_err() {
        unsafe_reasons.push("the listener address is invalid".to_string());
    }

    let certificate_path = value
        .get("certificate_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    let private_key_path = value
        .get("private_key_path")
        .and_then(Value::as_str)
        .map(PathBuf::from);
    for (label, path) in [
        ("TLS certificate", certificate_path.as_deref()),
        ("TLS private key", private_key_path.as_deref()),
    ] {
        let Some(path) = path else {
            unsafe_reasons.push(format!("the {label} path is missing"));
            continue;
        };
        match owned_regular_file(path, &daemon_root) {
            Ok(()) => {}
            Err(error) => unsafe_reasons.push(format!("{label}: {error}")),
        }
    }
    #[cfg(unix)]
    if let Some(path) = private_key_path.as_deref() {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            let mode = metadata.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                unsafe_reasons.push(format!(
                    "the TLS private key mode {mode:03o} permits group or other-user access"
                ));
            }
        }
    }
    let configured_pin = value
        .get("certificate_sha256")
        .and_then(Value::as_str)
        .filter(|pin| pin.len() == 64 && pin.bytes().all(|byte| byte.is_ascii_hexdigit()));
    if configured_pin.is_none() {
        unsafe_reasons.push("the TLS certificate fingerprint is missing or invalid".to_string());
    }

    if deep && unsafe_reasons.is_empty() {
        if let (Some(certificate_path), Some(expected)) =
            (certificate_path.as_deref(), configured_pin)
        {
            match fs::read(certificate_path)
                .map_err(|error| error.to_string())
                .and_then(|pem| certificate_fingerprint(&pem))
            {
                Ok(actual) if actual.eq_ignore_ascii_case(expected) => {}
                Ok(actual) => unsafe_reasons.push(format!(
                    "the TLS certificate pin changed (expected {expected}, found {actual})"
                )),
                Err(error) => unsafe_reasons.push(format!(
                    "the TLS certificate could not be fingerprinted: {error}"
                )),
            }
        }
    }

    if unsafe_reasons.is_empty() {
        findings.push(finding(
            "remote.tls",
            "remote",
            "Remote host uses an owned TLS identity",
            &format!(
                "The enabled listener at {listen} advertises {advertise}; its certificate and private key are regular files inside Little Monkey's daemon directory{}.",
                if deep { " and the certificate pin matches" } else { "" }
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    let detail = unsafe_reasons.join("; ");
    if fix {
        if let Some(object) = value.as_object_mut() {
            object.insert("enabled".to_string(), Value::Bool(false));
        }
        match atomic_write_private_json(&config_path, &value, app_data) {
            Ok(()) => findings.push(path_finding(
                "remote.disabled_unsafe",
                "remote",
                "Disabled an unsafe remote host",
                &format!("The listener was disabled because {detail}. No certificate, key, or pairing data was deleted."),
                FindingStatus::Fixed,
                true,
                &config_path,
                None,
            )),
            Err(error) => findings.push(path_finding(
                "remote.unsafe",
                "remote",
                "Enabled remote host is unsafe",
                &format!("{detail}. Security Doctor could not disable it: {error}"),
                FindingStatus::Critical,
                true,
                &config_path,
                Some("Disable and reconfigure the remote host before exposing it to a network."),
            )),
        }
    } else {
        findings.push(path_finding(
            "remote.unsafe",
            "remote",
            "Enabled remote host is unsafe",
            &detail,
            FindingStatus::Critical,
            true,
            &config_path,
            Some("Run the safe fix to disable this listener without deleting its configuration, then reconfigure TLS."),
        ));
    }
}

fn audit_mcp_origins(app_data: &Path, fix: bool, findings: &mut Vec<SecurityFinding>) {
    let path = app_data.join("mcp_servers.json");
    let mut config = match crate::mcp::load_config_impl(&path) {
        Ok(config) => config,
        Err(error) => {
            findings.push(path_finding(
                "mcp.config_invalid",
                "mcp",
                "MCP server configuration is invalid",
                &error,
                FindingStatus::Critical,
                false,
                &path,
                Some("Repair the MCP configuration in Settings."),
            ));
            return;
        }
    };
    let mut unsafe_enabled = BTreeMap::<String, String>::new();
    let mut safe_http = 0usize;
    for server in &config.servers {
        let crate::mcp::McpTransport::Http { url } = &server.transport else {
            continue;
        };
        let reason = insecure_mcp_reason(url);
        match (server.enabled, reason) {
            (true, Some(reason)) => {
                unsafe_enabled.insert(server.id.clone(), reason);
            }
            (false, Some(reason)) => findings.push(finding(
                &format!("mcp.insecure_disabled.{}", short_hash(server.id.as_bytes())),
                "mcp",
                "Disabled MCP server has an insecure origin",
                &format!(
                    "MCP server '{}' is currently inert: {reason}.",
                    server.label
                ),
                FindingStatus::Info,
                false,
                Some(path.to_string_lossy().as_ref()),
                Some("Use HTTPS or a loopback HTTP endpoint before enabling this server."),
            )),
            (_, None) => safe_http += 1,
        }
    }

    if unsafe_enabled.is_empty() {
        findings.push(finding(
            "mcp.origins",
            "mcp",
            "Enabled MCP HTTP origins are protected",
            &format!(
                "Checked {} enabled or configured HTTP transport(s); enabled endpoints use HTTPS or loopback HTTP.",
                safe_http
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }

    if fix {
        for server in &mut config.servers {
            if unsafe_enabled.contains_key(&server.id) {
                server.enabled = false;
            }
        }
        match serde_json::to_value(&config)
            .map_err(|error| error.to_string())
            .and_then(|value| atomic_write_private_json(&path, &value, app_data))
        {
            Ok(()) => {
                for (id, reason) in unsafe_enabled {
                    findings.push(finding(
                        &format!("mcp.disabled_unsafe.{}", short_hash(id.as_bytes())),
                        "mcp",
                        "Disabled an insecure MCP server",
                        &format!("Server '{id}' was disabled because {reason}. Its configuration and keychain credential were preserved."),
                        FindingStatus::Fixed,
                        true,
                        Some(path.to_string_lossy().as_ref()),
                        None,
                    ));
                }
            }
            Err(error) => findings.push(path_finding(
                "mcp.fix_failed",
                "mcp",
                "Unsafe MCP servers could not be disabled",
                &error,
                FindingStatus::Critical,
                true,
                &path,
                Some("Disable these servers in Settings before using MCP tools."),
            )),
        }
    } else {
        for (id, reason) in unsafe_enabled {
            findings.push(finding(
                &format!("mcp.insecure.{}", short_hash(id.as_bytes())),
                "mcp",
                "Enabled MCP server uses an insecure origin",
                &format!("Server '{id}' {reason}."),
                FindingStatus::Critical,
                true,
                Some(path.to_string_lossy().as_ref()),
                Some("Run the safe fix to disable it, then configure HTTPS or a loopback endpoint before re-enabling."),
            ));
        }
    }
}

fn audit_native_skills(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    if let Some(error) = &runtime.native_skills_error {
        findings.push(finding(
            "skills.discovery",
            "skills",
            "Native skill integrity check failed closed",
            error,
            FindingStatus::Critical,
            false,
            None,
            Some("Review the reported skill collision, symlink, manifest, or digest mismatch before invoking slash skills."),
        ));
        return;
    }
    let mut active = BTreeMap::<&str, &str>::new();
    let mut ineligible = 0usize;
    for skill in &runtime.native_skills {
        if skill.enabled {
            if let Some(existing) = active.insert(&skill.command, &skill.source) {
                findings.push(finding(
                    &format!("skills.collision.{}", short_hash(skill.command.as_bytes())),
                    "skills",
                    "Enabled slash skill command is ambiguous",
                    &format!("/{} is provided by both {existing} and {}.", skill.command, skill.source),
                    FindingStatus::Critical,
                    false,
                    None,
                    Some("Disable or rename one provider. Ambiguous slash commands are never executed."),
                ));
            }
        }
        if skill.enabled && !skill.eligible {
            ineligible += 1;
            findings.push(finding(
                &format!("skills.ineligible.{}", short_hash(skill.command.as_bytes())),
                "skills",
                "Enabled skill is ineligible on this machine",
                &format!(
                    "/{} from {} is missing binaries [{}] or environment entries [{}]. Environment values were not read or reported.",
                    skill.command,
                    skill.source,
                    skill.missing_bins.join(", "),
                    skill.missing_env.join(", ")
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Install the declared dependencies, provide the declared environment entries, or disable this skill."),
            ));
        }
    }
    if !findings
        .iter()
        .any(|finding| finding.id.starts_with("skills.collision"))
    {
        findings.push(finding(
            "skills.integrity",
            "skills",
            "Native skills passed integrity and collision checks",
            &format!(
                "Discovered {} skill(s); managed digests, reserved commands, symlinks, manifests, and active command uniqueness were checked. {ineligible} enabled skill(s) are currently ineligible.",
                runtime.native_skills.len()
            ),
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    }
}

fn audit_runtime_grants(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    if let Some(error) = &runtime.browser_error {
        findings.push(finding(
            "grants.browser_unavailable",
            "grants",
            "Browser grants could not be inspected",
            error,
            FindingStatus::Warning,
            false,
            None,
            Some("Stop active browser-verification runs and retry the audit."),
        ));
    } else if runtime.browser_grants.is_empty() && !runtime.browser_observed {
        findings.push(finding(
            "grants.browser_unobservable",
            "grants",
            "Live browser grants are not visible to this process",
            "Use Security Doctor in the running desktop app to inspect its in-memory browser sessions.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
    } else if runtime.browser_grants.is_empty() {
        findings.push(finding(
            "grants.browser_none",
            "grants",
            "No active browser-control grants",
            "Browser sessions are disposable and no run currently holds an origin grant.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    } else {
        for grant in &runtime.browser_grants {
            let insecure = grant
                .allowed_origins
                .iter()
                .filter(|origin| {
                    Url::parse(origin).is_ok_and(|url| {
                        url.scheme() == "http" && !url.host_str().is_some_and(is_loopback_host)
                    })
                })
                .count();
            let status = if insecure > 0 || grant.allow_loopback {
                FindingStatus::Warning
            } else {
                FindingStatus::Info
            };
            findings.push(finding(
                &format!("grants.browser.{}", short_hash(grant.session_id.as_bytes())),
                "grants",
                "Active browser origin grant",
                &format!(
                    "Run {} session {} can navigate to {} exact origin(s); loopback access is {} and {} granted origin(s) use remote plaintext HTTP.",
                    grant.run_id,
                    grant.session_id,
                    grant.allowed_origins.len(),
                    if grant.allow_loopback { "allowed" } else { "blocked" },
                    insecure
                ),
                status,
                false,
                None,
                Some("Stop the browser session when verification is complete; prefer HTTPS origins and grant loopback only when required."),
            ));
        }
    }

    if let Some(error) = &runtime.companion_error {
        findings.push(finding(
            "grants.companion_unavailable",
            "grants",
            "Companion capture grants could not be inspected",
            error,
            FindingStatus::Warning,
            false,
            None,
            Some("Use the companion emergency stop, then retry the audit."),
        ));
        return;
    }
    if runtime.companion_grants.is_empty() && !runtime.companion_observed {
        findings.push(finding(
            "grants.companion_unobservable",
            "grants",
            "Live companion grants are not visible to this process",
            "Use Security Doctor in the running desktop app to inspect its in-memory capture grants.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    }
    let now = now_ms();
    let active = runtime
        .companion_grants
        .iter()
        .filter(|grant| grant.active && grant.expires_at_ms > now)
        .collect::<Vec<_>>();
    if active.is_empty() {
        findings.push(finding(
            "grants.companion_none",
            "grants",
            "No active companion capture grants",
            "Screen, window, microphone, meeting, file, and text capture all require a short-lived explicit grant.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
        return;
    }
    for grant in active {
        let remaining = grant.expires_at_ms.saturating_sub(now);
        let broad = matches!(grant.kind.as_str(), "screen" | "microphone" | "meeting")
            || (grant.kind == "window" && grant.application_id.is_none());
        let invalid_lifetime = remaining > MAX_CAPTURE_GRANT_MS;
        findings.push(finding(
            &format!("grants.companion.{}", short_hash(grant.grant_id.as_bytes())),
            "grants",
            "Active companion capture grant",
            &format!(
                "{} capture is active for at most {} more second(s){}.",
                grant.kind,
                remaining / 1_000,
                grant.application_id.as_ref().map(|id| format!(" and is scoped to {id}")).unwrap_or_default()
            ),
            if invalid_lifetime {
                FindingStatus::Critical
            } else if broad {
                FindingStatus::Warning
            } else {
                FindingStatus::Info
            },
            false,
            None,
            Some("Revoke the grant in Desktop companion or use Emergency stop when capture is no longer needed."),
        ));
    }
}

/// How long a paired device may go without reporting itself before its grants
/// are treated as stale. A phone that has not been seen in a month may have
/// been sold, wiped, or simply lost; either way the operator should be told
/// that a camera grant is still sitting on it.
const STALE_DEVICE_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// The physical capabilities that see or hear the room the device is in. A
/// grant of any of these is worth naming individually rather than counting.
const INTIMATE_CAPABILITIES: &[&str] = &["microphone_capture", "screen_capture", "voice_stream"];

/// Paired phones and tablets: what they may do to their own hardware, whether
/// anything is doing it right now, and whether push would carry private text.
fn audit_paired_devices(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    if let Some(error) = &runtime.device_state_error {
        findings.push(finding(
            "devices.unreadable",
            "devices",
            "Paired device state could not be read",
            error,
            FindingStatus::Warning,
            false,
            None,
            Some("Run `monkey daemon remote device-list` to see the underlying error."),
        ));
        return;
    }
    if !runtime.device_state_observed {
        return;
    }
    let active: Vec<&DeviceGrantSnapshot> = runtime
        .devices
        .iter()
        .filter(|device| !device.revoked)
        .collect();
    if active.is_empty() {
        findings.push(finding(
            "devices.none_paired",
            "devices",
            "No paired physical devices",
            "Nothing on this machine can reach a phone's camera, microphone, screen or location.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    }

    for device in &active {
        let intimate: Vec<&str> = device
            .granted_physical
            .iter()
            .map(String::as_str)
            .filter(|capability| INTIMATE_CAPABILITIES.contains(capability))
            .collect();
        if !intimate.is_empty() {
            findings.push(finding(
                "devices.intimate_grant",
                "devices",
                "A device may capture its surroundings",
                &format!(
                    "'{}' ({}) is granted {}. Anything that can drive a run on this machine can ask for it.",
                    device.device_name,
                    device.device_id,
                    intimate.join(", ")
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Withdraw what is not in use: `monkey daemon remote device-grant <device-id> --capability <kept>…`, listing only the capabilities to keep."),
            ));
        }
        if device.granted_physical.len() >= 4 {
            findings.push(finding(
                "devices.broad_grant",
                "devices",
                "A device holds a broad hardware grant",
                &format!(
                    "'{}' ({}) is granted {} physical capabilities: {}.",
                    device.device_name,
                    device.device_id,
                    device.granted_physical.len(),
                    device.granted_physical.join(", ")
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("Grant only what a workflow actually uses; each capability is independent and none implies another."),
            ));
        }
        let stale = match device.last_seen_at_ms {
            None => !device.granted_physical.is_empty(),
            Some(last_seen) => now_ms().saturating_sub(last_seen) > STALE_DEVICE_MS,
        };
        if stale {
            findings.push(finding(
                "devices.stale_grant",
                "devices",
                "A device holds grants but has not been seen",
                &format!(
                    "'{}' ({}) still holds {} and {}.",
                    device.device_name,
                    device.device_id,
                    if device.granted_physical.is_empty() {
                        "no hardware grant".to_string()
                    } else {
                        device.granted_physical.join(", ")
                    },
                    match device.last_seen_at_ms {
                        None => "has never reported what it is".to_string(),
                        Some(last_seen) => format!("last reported at {last_seen} ms"),
                    }
                ),
                FindingStatus::Warning,
                false,
                None,
                Some("If that device is gone, revoke it: `monkey daemon remote pair-revoke <device-id>`."),
            ));
        }
    }

    // The transport those grants are exercised over.
    //
    // A hardware grant is only as private as the connection that carries the
    // photograph back. `pair-create` refuses a non-HTTPS advertised URL, so this
    // is not a hole an operator can open by accident — but a certificate can be
    // replaced, a fingerprint can go missing from the configuration, and a
    // development listener set up for a laptop can outlive the afternoon it was
    // meant for. The finding names the devices, because "your transport is
    // unpinned" and "your transport is unpinned and three phones can hear the
    // room over it" are different sentences.
    if let Some(transport) = &runtime.transport {
        let hardware: Vec<&str> = active
            .iter()
            .filter(|device| !device.granted_physical.is_empty())
            .map(|device| device.device_name.as_str())
            .collect();
        let insecure = !transport.advertise_url.starts_with("https://");
        if transport.enabled && !hardware.is_empty() && (insecure || !transport.pinned) {
            findings.push(finding(
                "devices.transport_unpinned",
                "devices",
                "Hardware grants are reachable over an unpinned transport",
                &format!(
                    "{} is {}, and {} hold hardware grants over it.",
                    transport.advertise_url,
                    if insecure {
                        "not HTTPS"
                    } else {
                        "advertised without a certificate fingerprint for devices to pin"
                    },
                    hardware.join(", ")
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Reconfigure the remote host with a certificate valid for the advertised name, then re-pair: `monkey daemon remote host-configure --advertise-url https://… --tls-certificate … --tls-private-key …`."),
            ));
        }
    }

    // A revoked device keeps nothing, including an address. This catches the
    // one row that could outlive a revocation and quietly keep a wiped phone
    // on the notification list.
    for device in runtime.devices.iter().filter(|device| device.revoked) {
        if device.push_registered {
            findings.push(finding(
                "devices.revoked_still_reachable",
                "devices",
                "A revoked device still has a push address",
                &format!(
                    "'{}' ({}) is revoked but still has a registered push token.",
                    device.device_name, device.device_id
                ),
                FindingStatus::Critical,
                false,
                None,
                Some("Re-run the revocation so the registration is cleared."),
            ));
        }
    }

    // Something happening right now, on hardware, in a room.
    for command in &runtime.device_commands {
        if !INTIMATE_CAPABILITIES.contains(&command.capability.as_str()) {
            continue;
        }
        findings.push(finding(
            "devices.capture_in_flight",
            "devices",
            "A capture is in progress on a device",
            &format!(
                "Command {} on {} is {} and is a {}.",
                command.command_id, command.device_id, command.state, command.capability
            ),
            if command.state == "running" {
                FindingStatus::Critical
            } else {
                FindingStatus::Warning
            },
            false,
            None,
            Some("Stop it with `monkey daemon remote device-cancel <command-id>`. A capture already taken cannot be untaken."),
        ));
    }

    match &runtime.push {
        None => {}
        Some(push) if !push.configured => findings.push(finding(
            "devices.push_absent",
            "devices",
            "Push is not configured",
            "Devices are only reachable while the app is open. Little Monkey ships no push project of its own.",
            FindingStatus::Info,
            false,
            None,
            None,
        )),
        Some(push) => {
            if push.enabled && push.include_detail {
                findings.push(finding(
                    "devices.push_detail",
                    "devices",
                    "Push notifications carry specifics",
                    &format!(
                        "Detail is switched on, so run and message specifics reach the lock screens of {} registered device(s) before anyone unlocks them.",
                        push.registered_devices
                    ),
                    FindingStatus::Warning,
                    false,
                    None,
                    Some("Turn detail off unless every registered device is trusted while locked; the app can always fetch the specifics after unlock."),
                ));
            } else if push.enabled {
                findings.push(finding(
                    "devices.push_private",
                    "devices",
                    "Push notifications withhold content",
                    "Notifications say what kind of thing happened, not what it said.",
                    FindingStatus::Pass,
                    false,
                    None,
                    None,
                ));
            }
        }
    }
}

fn audit_workspace_skill_root(workspace: Option<&Path>, findings: &mut Vec<SecurityFinding>) {
    let Some(workspace) = workspace else {
        findings.push(finding(
            "skills.workspace_none",
            "skills",
            "No workspace skill root is in scope",
            "Open a workspace to include .littlemonkey/skills in the integrity audit.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
        return;
    };
    let root = workspace.join(".littlemonkey").join("skills");
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => findings.push(path_finding(
            "skills.workspace_inspect",
            "skills",
            "Workspace skill root could not be inspected",
            &error.to_string(),
            FindingStatus::Warning,
            false,
            &root,
            Some("Check workspace ownership. Security Doctor never changes workspace permissions."),
        )),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => findings.push(path_finding(
            "skills.workspace_root",
            "skills",
            "Workspace skill root is not a real directory",
            "Workspace skills fail closed when this path is a symlink, file, or special node.",
            FindingStatus::Critical,
            false,
            &root,
            Some("Replace the path with an owned directory after manually reviewing its current target or contents."),
        )),
        Ok(_) => {}
    }
}

/// Reports whether this machine can actually enforce the sandbox it offers.
///
/// K3's acceptance says "a platform without enforcement reports itself as
/// unenforced in Security Doctor", and nothing did: the audit had no isolation
/// check of any kind. Post-run reporting was already honest — a run comes back
/// labelled `ProcessOnly` — but that is after the fact, and the one place a user
/// goes to ask "what is protecting me" said nothing about the boundary that is
/// absent on two of three platforms.
///
/// A `Warning` and not `Critical`: the sandbox is opt-in, so a machine with no
/// kernel boundary is a real limit on a feature the user chose to invoke rather
/// than a live compromise. It is also not `Info`, because
/// `probeGeneratedMcpArtifact` runs **model-authored** MCP server code through
/// this path — the case where the difference between a scrubbed environment and a
/// kernel boundary matters most.
fn audit_sandbox_enforcement(findings: &mut Vec<SecurityFinding>) {
    match crate::sandbox::sandbox_enforcement() {
        SandboxEnforcement::OsEnforced => findings.push(finding(
            "isolation.os_enforced",
            "isolation",
            "Sandboxed runs are confined by the OS",
            "Sandbox runs execute under a generated macOS Seatbelt profile: deny-by-default, \
             writes confined to the run directory, and network denied unless the run opts in. \
             The agent's own shell tool is a separate path and is not sandboxed.",
            FindingStatus::Pass,
            false,
            None,
            None,
        )),
        // A Warning and not a Pass, for the reason the variant exists: the kernel
        // holds the process tree, and the filesystem is still wide open. Anyone
        // reading a green check here would draw exactly the wrong conclusion about
        // running generated MCP server code.
        SandboxEnforcement::ProcessContained => findings.push(finding(
            "isolation.process_contained",
            "isolation",
            "Sandboxed runs are contained but not confined",
            "A job object bounds the run's process count, its committed memory and its reach \
             across the window station, and kills the whole tree when the run ends — so a \
             sandboxed command cannot outlive its run or exhaust the machine. It can still read \
             and write your real files by absolute path: this platform has no filesystem \
             boundary, and none is claimed.",
            FindingStatus::Warning,
            false,
            None,
            Some(
                "Review commands and generated MCP servers before running them here. A \
                 filesystem boundary on Windows needs a restricted token or an AppContainer, \
                 both of which must be supplied at process creation — see sandbox_windows.rs \
                 for what that would take.",
            ),
        )),
        SandboxEnforcement::ProcessOnly => findings.push(finding(
            "isolation.process_only",
            "isolation",
            "This platform has no OS sandbox",
            "Sandbox runs get a copied workspace, a restricted working directory and a scrubbed \
             environment, but no kernel boundary — a command can still read or write your real \
             files by absolute path. Generated MCP server code is probed through this path, so \
             treat a sandboxed run here as untrusted-code-with-guardrails, not as containment.",
            FindingStatus::Warning,
            false,
            None,
            Some(
                "Review commands and generated MCP servers before running them here. This \
                 machine reports no boundary at all: on Linux that means a kernel without \
                 Landlock, and on Windows that a job object could not be created.",
            ),
        )),
        SandboxEnforcement::Unavailable => findings.push(finding(
            "isolation.unavailable",
            "isolation",
            "The OS sandbox mechanism is missing",
            "This platform sandboxes through /usr/bin/sandbox-exec and it is not present, so a \
             sandboxed run will fail to start rather than run unconfined. Nothing runs with less \
             isolation than it reports.",
            FindingStatus::Warning,
            false,
            Some("/usr/bin/sandbox-exec"),
            Some(
                "Restore the system binary. Until then the Sandbox panel cannot run; the agent's \
                 own tools are unaffected because they never used it.",
            ),
        )),
    }
}

fn insecure_mcp_reason(raw: &str) -> Option<String> {
    let url = match Url::parse(raw) {
        Ok(url) => url,
        Err(error) => return Some(format!("has an invalid URL: {error}")),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return Some("embeds credentials in its URL".to_string());
    }
    match url.scheme() {
        "https" if url.host_str().is_some() => None,
        "http" if url.host_str().is_some_and(is_loopback_host) => None,
        "http" => Some("uses plaintext HTTP to a non-loopback host".to_string()),
        scheme => Some(format!("uses unsupported scheme '{scheme}'")),
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn owned_regular_file(path: &Path, owned_root: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "'{}' is not a regular non-symlink file",
            path.display()
        ));
    }
    let canonical_path = path
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize '{}': {error}", path.display()))?;
    let canonical_root = owned_root
        .canonicalize()
        .map_err(|error| format!("cannot canonicalize '{}': {error}", owned_root.display()))?;
    if !canonical_path.starts_with(canonical_root) {
        return Err(format!(
            "'{}' is outside the app-owned daemon directory",
            path.display()
        ));
    }
    Ok(())
}

fn read_bounded_json(path: &Path) -> Result<Option<Value>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Could not inspect '{}': {error}", path.display())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "'{}' is not a regular non-symlink file",
            path.display()
        ));
    }
    if metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "'{}' exceeds the configuration size limit",
            path.display()
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("Could not read '{}': {error}", path.display()))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Invalid JSON in '{}': {error}", path.display()))
}

fn atomic_write_private_json(path: &Path, value: &Value, app_data: &Path) -> Result<(), String> {
    if !path.starts_with(app_data) {
        return Err("refusing to modify a path outside Little Monkey app data".to_string());
    }
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect '{}': {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("refusing to replace a symlink or non-regular configuration".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "configuration has no parent directory".to_string())?;
    let parent_meta = fs::symlink_metadata(parent)
        .map_err(|error| format!("Could not inspect '{}': {error}", parent.display()))?;
    if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
        return Err("configuration parent is not a real directory".to_string());
    }
    let temporary = parent.join(format!(".security-doctor-{}.tmp", Uuid::new_v4().simple()));
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create safe-fix file: {error}"))?;
    set_private_file_permissions(&temporary)?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write safe-fix file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not sync safe-fix file: {error}"))?;
        fs::rename(&temporary, path)
            .map_err(|error| format!("Could not publish safe fix: {error}"))?;
        set_private_file_permissions(path)?;
        #[cfg(unix)]
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| format!("Could not sync configuration directory: {error}"))?;
        Ok::<(), String>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("Could not protect '{}': {error}", path.display()))
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn certificate_fingerprint(pem: &[u8]) -> Result<String, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "certificate PEM is not UTF-8".to_string())?;
    let begin = "-----BEGIN CERTIFICATE-----";
    let end = "-----END CERTIFICATE-----";
    let start = text
        .find(begin)
        .ok_or_else(|| "certificate PEM block is missing".to_string())?
        + begin.len();
    let finish = text[start..]
        .find(end)
        .ok_or_else(|| "certificate PEM block is incomplete".to_string())?
        + start;
    let encoded = text[start..finish]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    let der = STANDARD
        .decode(encoded)
        .map_err(|error| format!("certificate PEM base64 is invalid: {error}"))?;
    Ok(format!("{:x}", Sha256::digest(der)))
}

fn summarize(findings: &[SecurityFinding]) -> SecuritySummary {
    let mut summary = SecuritySummary::default();
    for finding in findings {
        match finding.status {
            FindingStatus::Pass => summary.passed += 1,
            FindingStatus::Info => summary.informational += 1,
            FindingStatus::Warning => summary.warnings += 1,
            FindingStatus::Critical => summary.critical += 1,
            FindingStatus::Fixed => summary.fixed += 1,
        }
    }
    summary
}

/// The voice surface: a microphone that opens itself, and where what it hears
/// goes.
///
/// **Why this is its own audit rather than a line in the device one.** The
/// device audit asks what a *phone* was granted. This asks what *this machine*
/// does on its own — a wake phrase and always-listening are desktop settings,
/// not grants, and nothing in the grant model would ever surface them. The
/// combination that matters most is the one neither half sees alone:
/// always-listening plus a hosted transcription provider is a room whose audio
/// leaves the machine without anyone pressing anything.
fn audit_voice_privacy(runtime: &SecurityRuntimeSnapshot, findings: &mut Vec<SecurityFinding>) {
    let Some(voice) = &runtime.voice else {
        return;
    };
    if voice.always_listening && !voice.local_only {
        findings.push(finding(
            "voice.passive_cloud_upload",
            "voice",
            "An always-on microphone is uploading to a provider",
            "Always-listening is on and transcription is a hosted provider, so audio captured \
             without anyone pressing anything can leave this machine.",
            FindingStatus::Critical,
            false,
            None,
            Some(
                "Either turn always-listening off, or switch transcription to local Whisper in \
                 Settings → Companion → Voice.",
            ),
        ));
    } else if voice.always_listening {
        findings.push(finding(
            "voice.always_listening",
            "voice",
            "The microphone is always listening",
            "This machine listens for a wake phrase continuously. Detection is local and no \
             audio is uploaded until the phrase is heard, but the microphone is open.",
            FindingStatus::Warning,
            false,
            None,
            Some(
                "Turn always-listening off in Settings → Companion → Voice when it is not in use.",
            ),
        ));
    } else if voice.wake_phrase_enabled {
        findings.push(finding(
            "voice.wake_phrase_enabled",
            "voice",
            "A wake phrase is enabled",
            "The wake phrase is armed but the microphone only opens when Talk is started.",
            FindingStatus::Info,
            false,
            None,
            None,
        ));
    } else {
        findings.push(finding(
            "voice.wake_disabled",
            "voice",
            "Nothing is listening on its own",
            "The wake phrase and always-listening are both off; the microphone opens only when \
             it is pressed.",
            FindingStatus::Pass,
            false,
            None,
            None,
        ));
    }
    if !voice.local_only {
        findings.push(finding(
            "voice.hosted_transcription",
            "voice",
            "Speech is transcribed by a hosted provider",
            "What is said into Talk, the companion overlay and answered calls is uploaded to the \
             transcription provider configured in Settings.",
            FindingStatus::Info,
            false,
            None,
            Some("Local Whisper keeps every recording on this machine."),
        ));
    }
}

fn finding(
    id: &str,
    category: &str,
    title: &str,
    detail: &str,
    status: FindingStatus,
    fixable: bool,
    path: Option<&str>,
    remediation: Option<&str>,
) -> SecurityFinding {
    SecurityFinding {
        id: id.to_string(),
        category: category.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status,
        fixable,
        path: path.map(str::to_string),
        remediation: remediation.map(str::to_string),
    }
}

#[allow(clippy::too_many_arguments)]
fn path_finding(
    id_prefix: &str,
    category: &str,
    title: &str,
    detail: &str,
    status: FindingStatus,
    fixable: bool,
    path: &Path,
    remediation: Option<&str>,
) -> SecurityFinding {
    let text = path.to_string_lossy();
    finding(
        &format!("{id_prefix}.{}", short_hash(text.as_bytes())),
        category,
        title,
        detail,
        status,
        fixable,
        Some(&text),
        remediation,
    )
}

fn short_hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))[..12].to_string()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-security-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    fn device(name: &str, granted: &[&str], last_seen_at_ms: Option<u64>) -> DeviceGrantSnapshot {
        DeviceGrantSnapshot {
            device_id: format!("device-{name}"),
            device_name: name.to_string(),
            granted_physical: granted.iter().map(|value| value.to_string()).collect(),
            effective_physical: granted.iter().map(|value| value.to_string()).collect(),
            revoked: false,
            last_seen_at_ms,
            push_registered: false,
        }
    }

    fn device_findings(runtime: SecurityRuntimeSnapshot) -> Vec<SecurityFinding> {
        let mut findings = Vec::new();
        audit_paired_devices(&runtime, &mut findings);
        findings
    }

    fn has(findings: &[SecurityFinding], id: &str) -> bool {
        findings.iter().any(|finding| finding.id == id)
    }

    fn voice_findings(voice: Option<VoicePrivacySnapshot>) -> Vec<SecurityFinding> {
        let mut findings = Vec::new();
        audit_voice_privacy(
            &SecurityRuntimeSnapshot {
                voice,
                ..SecurityRuntimeSnapshot::default()
            },
            &mut findings,
        );
        findings
    }

    /// The voice surface graded by what it can actually do, worst case first.
    ///
    /// The combination the operator most needs named is always-listening plus a
    /// hosted transcription backend: neither is alarming alone, and together
    /// they are a microphone that uploads a room nobody opened.
    #[test]
    fn the_doctor_grades_always_listening_by_where_the_audio_goes() {
        let leaking = voice_findings(Some(VoicePrivacySnapshot {
            wake_phrase_enabled: true,
            always_listening: true,
            local_only: false,
        }));
        assert!(has(&leaking, "voice.passive_cloud_upload"));
        assert_eq!(
            leaking
                .iter()
                .find(|finding| finding.id == "voice.passive_cloud_upload")
                .unwrap()
                .status,
            FindingStatus::Critical
        );

        // The same setting, kept on this machine, is a warning rather than a
        // critical: the microphone is open, but nothing leaves.
        let local = voice_findings(Some(VoicePrivacySnapshot {
            wake_phrase_enabled: true,
            always_listening: true,
            local_only: true,
        }));
        assert!(has(&local, "voice.always_listening"));
        assert!(!has(&local, "voice.passive_cloud_upload"));
        assert!(!has(&local, "voice.hosted_transcription"));

        // Armed but not listening, and the default: neither is a problem, and
        // both are stated rather than left silent.
        let armed = voice_findings(Some(VoicePrivacySnapshot {
            wake_phrase_enabled: true,
            always_listening: false,
            local_only: true,
        }));
        assert!(has(&armed, "voice.wake_phrase_enabled"));
        let quiet = voice_findings(Some(VoicePrivacySnapshot {
            wake_phrase_enabled: false,
            always_listening: false,
            local_only: true,
        }));
        assert!(has(&quiet, "voice.wake_disabled"));
        assert_eq!(quiet[0].status, FindingStatus::Pass);

        // Nothing observed says nothing, rather than claiming the microphone is
        // quiet on evidence it does not have.
        assert!(voice_findings(None).is_empty());
    }

    /// An open Talk socket is a running `voice_stream` command, so the device
    /// audit already names it. This pins that, because the property is what
    /// makes "an unexpected active stream" visible at all.
    #[test]
    fn a_live_voice_stream_is_reported_as_a_capture_in_flight() {
        let findings = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            devices: vec![device("phone", &["voice_stream"], Some(now_ms()))],
            device_commands: vec![DeviceCommandSnapshot {
                command_id: "cmd-1".into(),
                device_id: "device-phone".into(),
                capability: "voice_stream".into(),
                state: "running".into(),
            }],
            ..SecurityRuntimeSnapshot::default()
        });
        let in_flight = findings
            .iter()
            .find(|finding| finding.id == "devices.capture_in_flight")
            .expect("a running stream is named");
        assert_eq!(in_flight.status, FindingStatus::Critical);
        assert!(in_flight.detail.contains("voice_stream"));
        assert!(has(&findings, "devices.intimate_grant"));
    }

    #[test]
    fn the_doctor_reports_extension_trust_authority_and_protective_disable() {
        let mut findings = Vec::new();
        audit_extension_snapshots(
            &[ExtensionSecuritySnapshot {
                extension_id: "dev.example.risky".into(),
                version: "1.0.0".into(),
                trust: TrustState::Unsigned,
                trust_reason: "local unsigned bundle".into(),
                compatible: true,
                compatibility_reason: None,
                permissions: vec![
                    crate::executable_extensions::PermissionView {
                        permission_id: "network".into(),
                        kind: PermissionKind::NetworkOrigin,
                        scope: "http://example.com".into(),
                        reason: "fixture".into(),
                        risk: PermissionRisk::High,
                        granted: true,
                        binding_label: None,
                    },
                    crate::executable_extensions::PermissionView {
                        permission_id: "api_token".into(),
                        kind: PermissionKind::SecretUse,
                        scope: "api_token".into(),
                        reason: "fixture".into(),
                        risk: PermissionRisk::High,
                        granted: true,
                        binding_label: None,
                    },
                ],
                configured_secret_slots: 1,
                health: crate::executable_extensions::RuntimeHealth {
                    state: HealthState::ProtectiveDisabled,
                    validated: true,
                    enabled: false,
                    running: false,
                    consecutive_failures: 3,
                    trap_count: 3,
                    undeclared_attempts: 1,
                    last_error: Some("guest trapped".into()),
                    last_invocation_at_ms: Some(now_ms()),
                },
                component_intact: false,
                capabilities: vec![(CapabilityKind::Tool, "risky".into())],
            }],
            &mut findings,
        );

        let ids = findings
            .iter()
            .map(|finding| finding.id.as_str())
            .collect::<Vec<_>>();
        for prefix in [
            "extensions.unsigned.",
            "extensions.elevated_grants.",
            "extensions.plaintext_origins.",
            "extensions.secret_network.",
            "extensions.undeclared.",
            "extensions.component_missing.",
            "extensions.health.",
        ] {
            assert!(
                ids.iter().any(|id| id.starts_with(prefix)),
                "missing {prefix}"
            );
        }
        assert!(findings.iter().any(|finding| {
            finding.id.starts_with("extensions.health.")
                && finding.status == FindingStatus::Critical
        }));
    }

    /// The four things an operator most needs told about a phone they granted
    /// hardware to: it can hear the room, it can do a lot, it has not been seen
    /// in a month, and something is capturing right now.
    #[test]
    fn the_doctor_names_broad_stale_and_in_flight_device_grants() {
        let now = now_ms();
        let findings = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            devices: vec![
                device("kitchen tablet", &["microphone_capture"], Some(now)),
                device(
                    "old phone",
                    &[
                        "camera_capture",
                        "location_read",
                        "notification_post",
                        "audio_playback",
                    ],
                    Some(now - STALE_DEVICE_MS - 1),
                ),
            ],
            device_commands: vec![DeviceCommandSnapshot {
                command_id: "dcmd-one".into(),
                device_id: "device-kitchen tablet".into(),
                capability: "microphone_capture".into(),
                state: "running".into(),
            }],
            ..Default::default()
        });
        assert!(has(&findings, "devices.intimate_grant"));
        assert!(has(&findings, "devices.broad_grant"));
        assert!(has(&findings, "devices.stale_grant"));
        let capture = findings
            .iter()
            .find(|finding| finding.id == "devices.capture_in_flight")
            .expect("a running microphone must be reported");
        assert_eq!(capture.status, FindingStatus::Critical);
        assert!(capture
            .remediation
            .as_ref()
            .is_some_and(|text| text.contains("cannot be untaken")));

        // A quiet, narrowly-granted, recently-seen device produces none of them.
        let quiet = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            devices: vec![device("phone", &["notification_post"], Some(now))],
            ..Default::default()
        });
        assert!(!has(&quiet, "devices.intimate_grant"));
        assert!(!has(&quiet, "devices.broad_grant"));
        assert!(!has(&quiet, "devices.stale_grant"));
    }

    /// A hardware grant is only as private as the connection carrying the
    /// photograph back, and a development listener can outlive the afternoon it
    /// was set up for.
    #[test]
    fn the_doctor_flags_hardware_grants_reachable_over_an_unpinned_transport() {
        let now = now_ms();
        let with_transport = |advertise_url: &str, pinned: bool, granted: &[&str]| {
            device_findings(SecurityRuntimeSnapshot {
                device_state_observed: true,
                devices: vec![device("phone", granted, Some(now))],
                transport: Some(TransportSnapshot {
                    enabled: true,
                    advertise_url: advertise_url.to_string(),
                    pinned,
                }),
                ..Default::default()
            })
        };
        let plain = with_transport("http://192.168.1.4:8443", true, &["camera_capture"]);
        let finding = plain
            .iter()
            .find(|finding| finding.id == "devices.transport_unpinned")
            .expect("an unencrypted transport carrying a camera grant must be reported");
        assert_eq!(finding.status, FindingStatus::Critical);
        // The devices are named: "unpinned" and "unpinned, and this phone can
        // see through its camera over it" are different sentences.
        assert!(finding.detail.contains("phone"));

        assert!(has(
            &with_transport("https://runner.example.net", false, &["camera_capture"]),
            "devices.transport_unpinned"
        ));
        // A pinned HTTPS transport, and a plain one that no hardware grant is
        // reachable over, are both silent — the second because a development
        // listener is a reasonable thing to have until a camera is behind it.
        assert!(!has(
            &with_transport("https://runner.example.net", true, &["camera_capture"]),
            "devices.transport_unpinned"
        ));
        assert!(!has(
            &with_transport("http://127.0.0.1:8443", false, &[]),
            "devices.transport_unpinned"
        ));
    }

    /// A revoked device must keep nothing, an address included.
    #[test]
    fn the_doctor_flags_a_revoked_device_that_can_still_be_woken() {
        let mut revoked = device("sold phone", &[], Some(now_ms()));
        revoked.revoked = true;
        revoked.push_registered = true;
        let findings = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            devices: vec![revoked],
            ..Default::default()
        });
        let finding = findings
            .iter()
            .find(|finding| finding.id == "devices.revoked_still_reachable")
            .expect("a revoked device with a push address must be reported");
        assert_eq!(finding.status, FindingStatus::Critical);
    }

    #[test]
    fn the_doctor_reports_whether_push_would_put_specifics_on_a_lock_screen() {
        let leaky = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            push: Some(PushPrivacySnapshot {
                configured: true,
                enabled: true,
                include_detail: true,
                registered_devices: 2,
            }),
            ..Default::default()
        });
        assert!(has(&leaky, "devices.push_detail"));

        let private = device_findings(SecurityRuntimeSnapshot {
            device_state_observed: true,
            push: Some(PushPrivacySnapshot {
                configured: true,
                enabled: true,
                include_detail: false,
                registered_devices: 2,
            }),
            ..Default::default()
        });
        assert!(has(&private, "devices.push_private"));
        assert!(!has(&private, "devices.push_detail"));
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(root: &Path) -> SecurityAuditRequest {
        SecurityAuditRequest {
            app_data_dir: root.to_path_buf(),
            workspace: None,
            deep: false,
            fix: false,
            runtime: SecurityRuntimeSnapshot::default(),
        }
    }

    /// K3's acceptance clause, which nothing implemented: "a platform without
    /// enforcement reports itself as unenforced in Security Doctor". The audit had
    /// no isolation check at all — post-run labelling was honest, but the one
    /// screen a user consults to ask what is protecting them said nothing about a
    /// boundary that is absent on two of three platforms.
    #[test]
    fn the_audit_reports_this_platforms_isolation_and_matches_the_probe() {
        let temp = TestDirectory::new("isolation");
        let report = run_security_audit(&request(&temp.0)).unwrap();

        let isolation: Vec<&SecurityFinding> = report
            .findings
            .iter()
            .filter(|item| item.category == "isolation")
            .collect();
        assert_eq!(
            isolation.len(),
            1,
            "exactly one isolation finding, whatever the platform"
        );
        let finding = isolation[0];

        // Tied to the probe rather than to a hardcoded platform expectation, so
        // the audit and the Sandbox panel cannot drift into disagreeing about the
        // same machine.
        let (expected_id, expected_status) = match crate::sandbox::sandbox_enforcement() {
            SandboxEnforcement::OsEnforced => ("isolation.os_enforced", FindingStatus::Pass),
            SandboxEnforcement::ProcessContained => {
                ("isolation.process_contained", FindingStatus::Warning)
            }
            SandboxEnforcement::ProcessOnly => ("isolation.process_only", FindingStatus::Warning),
            SandboxEnforcement::Unavailable => ("isolation.unavailable", FindingStatus::Warning),
        };
        assert_eq!(finding.id, expected_id);
        assert_eq!(finding.status, expected_status);

        // A warning with no remediation is a dead end for the user, and the
        // unenforced states are precisely the ones that need one.
        if expected_status == FindingStatus::Warning {
            assert!(finding.remediation.is_some());
            assert_eq!(report.summary.warnings.min(1), 1);
        }
    }

    #[test]
    fn insecure_remote_mcp_is_disabled_without_deleting_configuration() {
        let temp = TestDirectory::new("mcp-fix");
        let path = temp.0.join("mcp_servers.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "servers": [{
                    "id":"remote",
                    "label":"Remote",
                    "transport":{"type":"http","url":"http://example.com/mcp"},
                    "enabled":true
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id.starts_with("mcp.disabled_unsafe") && finding.status == FindingStatus::Fixed
        }));
        let saved = crate::mcp::load_config_impl(&path).unwrap();
        assert!(!saved.servers[0].enabled);
        assert_eq!(saved.servers[0].id, "remote");
    }

    #[test]
    fn unsafe_remote_host_is_only_disabled() {
        let temp = TestDirectory::new("remote-fix");
        let daemon = temp.0.join("daemon");
        fs::create_dir_all(&daemon).unwrap();
        let path = daemon.join("remote-host.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&serde_json::json!({
                "protocol_version":1,
                "runner_id":"runner-test",
                "listen":"0.0.0.0:9443",
                "advertise_url":"http://example.com",
                "certificate_path":daemon.join("missing-cert.pem"),
                "private_key_path":daemon.join("missing-key.pem"),
                "certificate_sha256":"00",
                "enabled":true
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id.starts_with("remote.disabled_unsafe")
                && finding.status == FindingStatus::Fixed
        }));
        let saved: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.get("enabled").and_then(Value::as_bool), Some(false));
        assert_eq!(
            saved.get("runner_id").and_then(Value::as_str),
            Some("runner-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn permission_fix_restricts_known_app_owned_files() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TestDirectory::new("mode-fix");
        let path = temp.0.join("api_server.json");
        fs::write(&path, b"{}").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let mut audit = request(&temp.0);
        audit.fix = true;
        let report = run_security_audit(&audit).unwrap();
        assert!(report.summary.fixed >= 1);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn tampered_or_colliding_skill_failure_is_critical() {
        let temp = TestDirectory::new("skill-error");
        let mut audit = request(&temp.0);
        audit.runtime.native_skills_error = Some(
            "skill conflict: managed skill /review changed outside the approved install flow"
                .to_string(),
        );
        let report = run_security_audit(&audit).unwrap();
        assert!(report.findings.iter().any(|finding| {
            finding.id == "skills.discovery" && finding.status == FindingStatus::Critical
        }));
    }

    #[test]
    fn broad_runtime_grants_are_visible() {
        let temp = TestDirectory::new("grants");
        let mut audit = request(&temp.0);
        audit.runtime.browser_grants.push(BrowserGrantSnapshot {
            session_id: "browser-test".to_string(),
            run_id: "run-test".to_string(),
            allowed_origins: vec!["http://127.0.0.1:3000".to_string()],
            allow_loopback: true,
        });
        audit.runtime.companion_grants.push(CompanionGrantSnapshot {
            grant_id: "capture-test".to_string(),
            kind: "screen".to_string(),
            application_id: None,
            expires_at_ms: now_ms() + 30_000,
            active: true,
        });
        let report = run_security_audit(&audit).unwrap();
        assert!(report.summary.warnings >= 2);
    }
}

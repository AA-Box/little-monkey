//! Sandboxed execution environments.
//!
//! Runs a caller-supplied shell command inside a disposable copy of the
//! primary workspace instead of the real one: the copy lives under
//! `<app_data>/sandbox-runs/<run_id>/workspace`, the spawned process only
//! ever sees that directory as its cwd, and it never receives the parent
//! process's environment — only `PATH`, sandbox-owned `HOME`/temporary
//! directories, a computed read-only toolchain locator when needed, plus
//! whatever extra variable names the caller explicitly approved. On macOS
//! the command additionally runs under a generated
//! Seatbelt (`sandbox-exec`) profile that limits reads to the sandbox and
//! explicit system/toolchain roots, denies writes outside the ephemeral run
//! directory, and denies network access unless it was explicitly enabled. On
//! Linux the equivalent boundary is a Landlock filesystem ruleset plus a
//! seccomp-BPF network filter installed in `pre_exec` (see
//! `crate::sandbox_linux`); Windows gets the restricted-cwd/env isolation
//! only. Every run reports which of the two actually applied (see
//! [`Isolation`]) — never more than what was really enforced.
//!
//! Nothing the sandboxed command writes ever reaches the real workspace
//! automatically. Copying files back out is a separate, explicit two-phase
//! action mirroring `m5_delivery`'s prepare-digest/confirm-phrase pattern:
//! [`build_promote_preview`] (exposed as `sandbox_prepare_promote`) hashes
//! the exact files the caller selected and returns a digest plus a
//! `CONFIRM <digest prefix>` phrase; [`sandbox_execute_promote`] refuses to
//! touch the real workspace unless the exact digest and phrase are replayed
//! back, then re-hashes the sandbox copy to confirm nothing changed since
//! the preview was built.
//!
//! Every run is modeled as an ordinary [`crate::run_protocol::RunSpec`] of
//! [`RunKind::Sandboxed`] and recorded through the existing
//! [`crate::run_ledger::RunLedger`] — `Queued`/`Started` on launch,
//! `CheckpointLinked` once the ephemeral copy exists, `ArtifactAdded` for
//! captured stdout/stderr, `VerificationFinished` for the exit outcome, and
//! (only once a promote is confirmed) `ExternalMutationPrepared` /
//! `ExternalMutationConfirmed` / `Completed`. The run intentionally stays
//! non-terminal after execution — the whole point is that the workspace
//! stays untouched until a human decides to promote or discard.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::Serialize;
use sha2::{Digest, Sha256};
use tauri::Manager;

use crate::run_protocol::{
    ArtifactKind, CapabilityAssessment, CapabilityState, CheckpointKind, ClientIdentity,
    ModelCapabilitiesSnapshot, ModelTargetSnapshot, MutationKind, PermissionMode,
    PermissionPolicySnapshot, RootAccess, RootGrant, RunBudgets, RunEvent, RunKind, RunSpec,
    ToolPolicyDecision, UsageSnapshot, WorkspaceContext, RUN_PROTOCOL_SCHEMA_VERSION,
};
use crate::{permissions, workspace, AppState};

const SANDBOX_RUNS_DIR: &str = "sandbox-runs";
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 5 * 60 * 1_000;
const MAX_TIMEOUT_MS: u64 = 30 * 60 * 1_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_APPROVED_ENV_KEYS: usize = 16;
const MAX_ARTIFACT_BYTES_BUDGET: u64 = 128 * 1024 * 1024;
const MAX_EVENT_TEXT_EXCERPT: usize = 4_096;
const PROMOTE_PREVIEW_TTL_MS: u64 = 5 * 60 * 1_000;
const MAX_PROMOTE_FILES: usize = 500;
const SANDBOX_HOME_DIR: &str = "home";
const SANDBOX_TMP_DIR: &str = "tmp";

/// Directory/build-artifact names that are never worth copying into the
/// ephemeral sandbox: they are large, regenerable, and (for `.git`)
/// irrelevant to "run this command against these files". Comparison is
/// case-insensitive, matching `permissions::path_risk_floor`'s reasoning
/// for the same platforms.
const SKIP_DIR_NAMES: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    "out",
    ".venv",
    "venv",
    "__pycache__",
    ".turbo",
    ".cache",
];

/// Parent-process env vars a sandboxed process may inherit. HOME and all
/// temporary-directory variables are deliberately absent: they are always
/// replaced with sandbox-owned paths by [`allowlisted_env`].
#[cfg(not(target_os = "windows"))]
const BASE_ENV_KEYS: &[&str] = &["PATH"];
#[cfg(target_os = "windows")]
const BASE_ENV_KEYS: &[&str] = &["PATH", "SystemRoot"];

const SANDBOX_OWNED_ENV_KEYS: &[&str] = &["HOME", "USERPROFILE", "TMPDIR", "TMP", "TEMP"];

/// Content roots needed by ordinary command-line programs on macOS. These
/// intentionally avoid broad data-bearing trees such as `/System` (which
/// also contains `/System/Volumes/Data`), `/usr`, `/Library`, `/private`,
/// and the user's home. Additional executable roots come only from PATH and
/// are filtered against the real workspace and whole-home roots below.
///
/// `cfg`-gated (with `test`, which [`readable_roots`]' own test needs) so the
/// platform that cannot use a list is not left holding it as dead code.
#[cfg(any(target_os = "macos", test))]
const MACOS_SYSTEM_READ_ROOTS: &[&str] = &[
    "/System/Library",
    "/System/Cryptexes/App/usr",
    "/usr/bin",
    "/usr/lib",
    "/usr/libexec",
    "/usr/share",
    "/bin",
    "/sbin",
    "/Library/Apple/System/Library",
    "/Library/Developer/CommandLineTools",
    "/Library/Developer/Toolchains",
    "/Applications/Xcode.app/Contents/Developer",
    "/private/var/db/dyld",
    "/private/var/select",
    "/private/etc/ssl",
    "/private/etc/hosts",
    "/private/etc/resolv.conf",
    "/private/etc/services",
    "/private/etc/protocols",
    "/private/etc/localtime",
    "/Library/Keychains/System.keychain",
    "/dev/null",
    "/dev/random",
    "/dev/urandom",
];

/// The same policy as [`MACOS_SYSTEM_READ_ROOTS`], entry for entry, expressed in
/// Linux's layout: executable and library roots, the dynamic loader's cache
/// (`/etc/ld.so.*`, whose macOS counterpart is `/private/var/db/dyld`), the
/// resolver/TLS/timezone configuration a network-enabled command needs, and the
/// three character devices. `/etc/passwd`, `/etc/group` and `/etc/nsswitch.conf`
/// stand in for the macOS profile's `(allow mach-lookup)` — they are how
/// `getpwuid` answers on Linux — and `/etc/shadow` is deliberately not among
/// them.
///
/// Not here, deliberately: `/proc`, `/sys`, `/tmp`, `/var`, `/home`, `/root`,
/// and `/etc` as a whole. `/usr/local` is absent for the same reason
/// `/opt/homebrew` is absent from the macOS list — [`readable_roots`] adds its
/// executable/library subdirectories only when PATH actually points there. See
/// `sandbox_linux`'s module docs for why `/proc` in particular stays out.
#[cfg(target_os = "linux")]
const LINUX_SYSTEM_READ_ROOTS: &[&str] = &[
    "/bin",
    "/sbin",
    "/lib",
    "/lib32",
    "/lib64",
    "/usr/bin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/lib32",
    "/usr/lib64",
    "/usr/libexec",
    "/usr/share",
    "/usr/include",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/alternatives",
    "/etc/ssl/certs",
    "/etc/ssl/openssl.cnf",
    "/etc/pki/tls/certs",
    "/etc/ca-certificates",
    "/etc/nsswitch.conf",
    "/etc/passwd",
    "/etc/group",
    "/etc/hosts",
    "/etc/resolv.conf",
    "/etc/services",
    "/etc/protocols",
    "/etc/localtime",
    "/dev/null",
    "/dev/random",
    "/dev/urandom",
];

/// Per-process, in-memory registry of prepared-but-not-yet-confirmed promote
/// previews, keyed by digest. Unlike `m5_delivery`'s durable SQLite preview
/// store, this is deliberately not persisted: the ephemeral sandbox copy a
/// preview points at lives only under this process's app-data directory for
/// this run, so a restart already leaves nothing meaningful to promote.
#[derive(Default)]
pub struct SandboxState {
    previews: std::sync::Mutex<HashMap<String, PendingPromote>>,
}

#[derive(Debug, Clone)]
struct PendingPromote {
    run_id: String,
    files: Vec<PromoteFileEntry>,
    expires_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    /// A kernel-enforced filesystem boundary applied in addition to the
    /// restricted cwd/env: a generated macOS Seatbelt profile (`sandbox-exec`),
    /// or a Landlock ruleset on Linux.
    OsSandboxed,
    /// The kernel bounded the process *tree* but not its filesystem: a Windows
    /// job object confined the run's process count, committed memory and
    /// window-station reach, and killed the whole tree on exit.
    ///
    /// Deliberately not [`Isolation::OsSandboxed`], because the real workspace is
    /// still reachable by absolute path. Windows has no filesystem boundary this
    /// app can reach without owning its own `CreateProcess` — see
    /// `crate::sandbox_windows`.
    ProcessContained,
    /// Only the restricted cwd + allowlisted env applied — either no OS-level
    /// sandbox exists for this platform, or this kernel could not enforce one.
    ProcessOnly,
}

/// Path to the only OS enforcement mechanism this app has.
#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// What isolation this machine can actually apply, answerable *before* a run.
///
/// [`Isolation`] reports what a run got, which is honest but arrives too late to
/// inform the decision to start one. The Sandbox panel offers the same button on
/// every platform, and `probeGeneratedMcpArtifact` sends **model-authored MCP
/// server code** through it — so on Windows, and on a Linux kernel without
/// Landlock, that code runs with a restricted cwd and a scrubbed environment and
/// no kernel boundary, free to read or write the real workspace by absolute
/// path. That is worth knowing first.
///
/// `Unavailable` is a third state, not a pessimistic reading of `ProcessOnly`: on
/// macOS `execute_in_sandbox` spawns `sandbox-exec` unconditionally, so if the
/// binary is missing the run fails outright rather than degrading. A user who sees
/// only "no OS sandbox" would go looking for the wrong problem.
///
/// `Unavailable` has no meaning on Linux, and forcing the symmetry would be a
/// lie in the other direction. There is no separate binary to be missing: the
/// mechanism is a syscall, and when the kernel does not have it
/// `crate::sandbox_linux` installs no ruleset and the run proceeds with the
/// restricted-cwd/env isolation. Nothing fails, so `ProcessOnly` — the state
/// that means "this ran without a kernel boundary" — is the whole truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    /// A kernel-enforced boundary applies: macOS Seatbelt via `sandbox-exec`, or
    /// Landlock on Linux.
    OsEnforced,
    /// A Windows job object will bound the run's process tree, committed memory
    /// and window-station reach, and kill the tree when the run ends — but no
    /// filesystem or network boundary applies, so the real workspace stays
    /// reachable by absolute path.
    ///
    /// Between [`SandboxEnforcement::OsEnforced`] and
    /// [`SandboxEnforcement::ProcessOnly`] on purpose, and closer to the latter
    /// for any decision about running untrusted code.
    ProcessContained,
    /// Restricted cwd and allowlisted environment only. No kernel boundary.
    ProcessOnly,
    /// This platform has an enforcement mechanism and it is not usable here, so a
    /// sandboxed run will fail rather than run unconfined.
    Unavailable,
}

/// This machine's enforcement capability.
///
/// Deliberately a probe rather than a constant. On macOS the answer depends on
/// `sandbox-exec` being present, and on Linux on whether this kernel can enforce
/// the Landlock baseline — neither of which a `cfg!` can know, and reporting
/// `OsEnforced` from the target triple alone is exactly the kind of claim this
/// function exists to stop making. A kernel built without Landlock, booted with
/// it disabled, or running inside a container whose own policy blocks the
/// syscall all answer `ProcessOnly` here.
pub fn sandbox_enforcement() -> SandboxEnforcement {
    #[cfg(target_os = "macos")]
    {
        if Path::new(SANDBOX_EXEC).is_file() {
            SandboxEnforcement::OsEnforced
        } else {
            SandboxEnforcement::Unavailable
        }
    }
    // Only the filesystem boundary is reported. A network-denied run on a kernel
    // without Landlock still gets the seccomp filter, so its egress is denied
    // even here — but its filesystem is not, and this answer is about the
    // boundary that keeps the real workspace out of reach.
    #[cfg(target_os = "linux")]
    {
        if crate::sandbox_linux::landlock_is_enforceable() {
            SandboxEnforcement::OsEnforced
        } else {
            SandboxEnforcement::ProcessOnly
        }
    }
    // `Unavailable` is wrong here for the reason it is wrong on Linux: there is no
    // separate binary to be missing, and a machine that can create neither
    // mechanism degrades to `ProcessOnly` rather than failing.
    //
    // Two mechanisms, so three answers. An AppContainer is the filesystem
    // boundary and earns `OsEnforced` alongside Seatbelt and Landlock; a machine
    // that can only give us a job object gets `ProcessContained`; one that can
    // give neither gets `ProcessOnly`. Probed rather than assumed from the target
    // triple, because group policy can refuse either one.
    #[cfg(target_os = "windows")]
    {
        if crate::sandbox_windows::app_containers_are_enforceable() {
            SandboxEnforcement::OsEnforced
        } else if crate::sandbox_windows::job_objects_are_enforceable() {
            SandboxEnforcement::ProcessContained
        } else {
            SandboxEnforcement::ProcessOnly
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        SandboxEnforcement::ProcessOnly
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CopyStats {
    /// Every file placed in the sandbox, however its bytes got there.
    ///
    /// Deliberately still the total rather than "files copied the slow way":
    /// callers and the ledger label have always read it as "how big is this
    /// sandbox", and narrowing it now would silently change what an existing
    /// record means.
    pub files_copied: u64,
    pub bytes_copied: u64,
    pub skipped: u64,
    /// Of [`Self::files_copied`], how many were copy-on-write clones rather
    /// than byte copies. Zero on a filesystem or platform that cannot clone —
    /// see [`clone_file`].
    pub files_cloned: u64,
}

impl CopyStats {
    /// How the bytes got in, for a ledger label a human reads.
    ///
    /// Three states and not two, because "nothing was cloned" and "there was
    /// nothing to clone" are different facts: an empty workspace must not read
    /// as a filesystem that refused to clone.
    #[must_use]
    pub fn placement_mode(&self) -> &'static str {
        match (self.files_copied, self.files_cloned) {
            (0, _) => "no files",
            (total, cloned) if cloned == total => "copy-on-write",
            (_, 0) => "full copy",
            _ => "copy-on-write where the filesystem allowed it",
        }
    }
}

/// Clones one file copy-on-write, or reports that this platform could not.
///
/// # Why per file rather than per tree
///
/// macOS `clonefile` can clone a whole directory in one call, and it is
/// tempting. It would also clone the two things
/// [`copy_workspace_into_sandbox`] exists to leave out — `.env` files and
/// `node_modules`-shaped directories — and deleting them afterwards is a
/// strictly worse version of never copying them: the window in which a secret
/// exists inside the sandbox would be real, and a crash inside that window
/// leaves it there. Keeping the walk and swapping only the per-file placement
/// means the skip rules, the symlink handling and the resulting tree are
/// unchanged by construction, which is what makes "byte-for-byte identical to
/// the copy implementation" true rather than tested-and-hoped.
///
/// # Failure is never an error
///
/// Returns `false` for every refusal — a filesystem without copy-on-write, a
/// cross-device destination, a destination that already exists — because each
/// one means "copy it the ordinary way", not "fail the run". The caller falls
/// back to `fs::copy`, so the only thing lost is the saving.
#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dest: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let (Ok(source), Ok(destination)) = (
        CString::new(src.as_os_str().as_bytes()),
        CString::new(dest.as_os_str().as_bytes()),
    ) else {
        // An interior NUL, which no path this walk produced can contain.
        return false;
    };
    // SAFETY: both pointers are NUL-terminated C strings that outlive the call,
    // and `clonefile` neither retains them nor writes through them.
    unsafe { libc::clonefile(source.as_ptr(), destination.as_ptr(), 0) == 0 }
}

/// Every other platform copies, and says so rather than pretending.
///
/// Linux reflink (`FICLONE`, on btrfs and XFS) and Windows ReFS block cloning
/// are both real and both deliberately absent: neither can be exercised on this
/// project's machines, and an untested ioctl that silently degrades to a copy
/// would look identical to this in every test while being harder to read.
/// Overlayfs, which the roadmap names for Linux, needs mount privileges a
/// desktop application does not have.
///
/// Hard-linking a staging tree is not a substitute on any platform and is not a
/// deferral: a write through a hard link mutates the workspace file itself,
/// which is the one thing an ephemeral sandbox exists to prevent.
#[cfg(not(target_os = "macos"))]
fn clone_file(_src: &Path, _dest: &Path) -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromoteFileEntry {
    pub path: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPromotePreview {
    pub run_id: String,
    pub digest: String,
    pub confirmation_phrase: String,
    pub files: Vec<PromoteFileEntry>,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxPromoteResult {
    pub run_id: String,
    pub promoted_files: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDiffEntry {
    pub path: String,
    /// `"added"` (present in the sandbox copy only) or `"modified"`
    /// (present in both, different content). Unchanged files are omitted.
    /// Deletions are never represented here and promote never deletes real
    /// files — this feature only ever copies forward.
    pub status: String,
    pub sandbox_sha256: String,
    pub workspace_sha256: Option<String>,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRunSummary {
    pub run_id: String,
    pub isolation: Isolation,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub passed: bool,
    pub duration_ms: u64,
    pub stdout_artifact_id: String,
    pub stderr_artifact_id: String,
    pub stdout_excerpt: String,
    pub stderr_excerpt: String,
    pub files_copied: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxRunListEntry {
    pub run_id: String,
    pub status: crate::run_protocol::RunStatus,
    pub task: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

#[derive(Debug, Clone)]
struct SandboxRunRequest {
    command: String,
    timeout_ms: Option<u64>,
    allow_network: bool,
    approved_env: Vec<String>,
}

impl SandboxRunRequest {
    fn validate(&self) -> Result<(), String> {
        if self.command.trim().is_empty() {
            return Err("Enter a command to run in the sandbox".to_string());
        }
        if self.command.len() > MAX_COMMAND_BYTES {
            return Err(format!(
                "Command exceeds the {MAX_COMMAND_BYTES}-byte limit"
            ));
        }
        if self.command.contains('\0') {
            return Err("Command must not contain NUL bytes".to_string());
        }
        if self.approved_env.len() > MAX_APPROVED_ENV_KEYS {
            return Err(format!(
                "At most {MAX_APPROVED_ENV_KEYS} approved environment variables are allowed"
            ));
        }
        for key in &self.approved_env {
            let valid = !key.is_empty()
                && key.len() <= 128
                && key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                && !key.as_bytes()[0].is_ascii_digit();
            if !valid {
                return Err(format!("Invalid environment variable name: '{key}'"));
            }
        }
        Ok(())
    }

    fn timeout(&self) -> Duration {
        let ms = self
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS);
        Duration::from_millis(ms)
    }
}

fn bounded(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    let mut end = max;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn hash_file(path: &Path) -> io::Result<(String, u64)> {
    let bytes = fs::read(path)?;
    let size = bytes.len() as u64;
    Ok((sha256_hex_bytes(&bytes), size))
}

fn confirmation_phrase_for(digest: &str) -> String {
    format!("CONFIRM {}", &digest[..12])
}

/// True for directory names that are never worth copying wholesale into the
/// ephemeral sandbox (see [`SKIP_DIR_NAMES`]).
fn is_skippable_dir_name(name: &str) -> bool {
    SKIP_DIR_NAMES.iter().any(|skip| skip.eq_ignore_ascii_case(name))
}

/// True for files whose content is secret-shaped (currently: `.env*`, via
/// `permissions::path_risk_floor`). Only the secrets category is excluded
/// here — script-executing manifests/lockfiles (`package.json`,
/// `Cargo.toml`, ...) and shell rc files are also flagged by that function
/// for *edit*-risk purposes, but excluding them from the sandbox copy would
/// make it impossible to actually build or test the copy, defeating the
/// point of this feature. Secrets are excluded unconditionally: the
/// sandboxed process already never inherits them via its environment (see
/// [`allowlisted_env`]), and a copied `.env` file on disk would silently
/// undo that protection for any command that reads it directly.
fn secret_shaped(path: &Path, root: &Path) -> bool {
    matches!(
        permissions::path_risk_floor(path, root),
        Some(reason) if reason.starts_with("environment/secrets file")
    )
}

/// Copies `root`'s files into `dest`, skipping [`SKIP_DIR_NAMES`]
/// directories, secret-shaped files (see [`secret_shaped`]), and symlinks
/// (never followed, so a symlink pointing outside `root` can never smuggle
/// unrelated files into the copy).
pub fn copy_workspace_into_sandbox(root: &Path, dest: &Path) -> io::Result<CopyStats> {
    fs::create_dir_all(dest)?;
    let mut stats = CopyStats::default();

    let walker = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && is_skippable_dir_name(&entry.file_name().to_string_lossy()))
        });

    for entry in walker {
        let entry = entry.map_err(io::Error::other)?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(path);

        if entry.file_type().is_dir() {
            fs::create_dir_all(dest.join(rel))?;
            continue;
        }
        if !entry.file_type().is_file() {
            // Symlinks and other special files are never copied.
            stats.skipped += 1;
            continue;
        }
        if secret_shaped(path, root) {
            stats.skipped += 1;
            continue;
        }

        let dest_path = dest.join(rel);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Cloned where the filesystem allows it, copied where it does not. The
        // resulting file is the same either way — `bytes_copied` stays the size
        // of the tree, not the number of bytes the disk actually wrote, because
        // it is read as "how big is this sandbox" and a clone does not make the
        // sandbox smaller.
        let bytes = if clone_file(path, &dest_path) {
            stats.files_cloned += 1;
            entry.metadata().map_err(io::Error::other)?.len()
        } else {
            fs::copy(path, &dest_path)?
        };
        stats.files_copied += 1;
        stats.bytes_copied += bytes;
    }

    Ok(stats)
}

fn is_sandbox_owned_env_key(key: &str) -> bool {
    SANDBOX_OWNED_ENV_KEYS
        .iter()
        .any(|owned| owned.eq_ignore_ascii_case(key))
}

fn set_env_value(env: &mut Vec<(String, String)>, key: &str, value: String) {
    if let Some((_, current)) = env
        .iter_mut()
        .find(|(current_key, _)| current_key.eq_ignore_ascii_case(key))
    {
        *current = value;
    } else {
        env.push((key.to_string(), value));
    }
}

/// Builds an env list containing only the platform's non-sensitive base
/// keys plus whatever extra names the caller explicitly approved. HOME and
/// every conventional temporary-directory variable are forcibly bound to
/// sandbox-owned directories; approving one of those names can never
/// restore the parent process's value.
pub fn allowlisted_env(
    sandbox_home: &Path,
    sandbox_tmp: &Path,
    approved_extra: &[String],
) -> Vec<(String, String)> {
    let mut keys: Vec<String> = BASE_ENV_KEYS.iter().map(|k| k.to_string()).collect();
    for extra in approved_extra {
        if !is_sandbox_owned_env_key(extra) && !keys.iter().any(|k| k == extra) {
            keys.push(extra.clone());
        }
    }
    let mut env: Vec<(String, String)> = keys
        .into_iter()
        .filter_map(|key| std::env::var(&key).ok().map(|value| (key, value)))
        .collect();

    let home = sandbox_home.to_string_lossy().into_owned();
    let tmp = sandbox_tmp.to_string_lossy().into_owned();
    set_env_value(&mut env, "HOME", home.clone());
    set_env_value(&mut env, "USERPROFILE", home);
    set_env_value(&mut env, "TMPDIR", tmp.clone());
    set_env_value(&mut env, "TMP", tmp.clone());
    set_env_value(&mut env, "TEMP", tmp);
    env.sort_by(|a, b| a.0.cmp(&b.0));
    env
}

fn policy_comparison_path(path: &Path) -> PathBuf {
    // APFS exposes the writable data volume through both ordinary paths
    // (`/Users/...`) and `/System/Volumes/Data/...`. Normalize the latter so
    // a PATH entry cannot smuggle the real workspace in through that alias.
    match path.strip_prefix("/System/Volumes/Data") {
        Ok(suffix) => Path::new("/").join(suffix),
        Err(_) => path.to_path_buf(),
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    let left = policy_comparison_path(left);
    let right = policy_comparison_path(right);
    left.starts_with(&right) || right.starts_with(&left)
}

fn looks_like_whole_user_home(path: &Path) -> bool {
    let normalized = policy_comparison_path(path);
    let parts: Vec<_> = normalized.components().collect();
    (parts.len() <= 3
        && matches!(
            parts.as_slice(),
            [
                std::path::Component::RootDir,
                std::path::Component::Normal(base),
                std::path::Component::Normal(_)
            ] if *base == OsStr::new("Users") || *base == OsStr::new("home")
        ))
        || normalized == Path::new("/Users")
        || normalized == Path::new("/home")
}

fn readable_root_is_safe(
    candidate: &Path,
    real_home: Option<&Path>,
    real_workspace: &Path,
) -> bool {
    if !candidate.is_absolute()
        || candidate == Path::new("/")
        || looks_like_whole_user_home(candidate)
        || paths_overlap(candidate, real_workspace)
    {
        return false;
    }
    if let Some(home) = real_home {
        let candidate = policy_comparison_path(candidate);
        let home = policy_comparison_path(home);
        // A directory *inside* HOME may be an explicit PATH/toolchain root.
        // HOME itself and any ancestor of it must never become a read root.
        if candidate == home || home.starts_with(&candidate) {
            return false;
        }
    }
    true
}

fn insert_existing_read_root(
    roots: &mut BTreeSet<PathBuf>,
    candidate: &Path,
    real_home: Option<&Path>,
    real_workspace: &Path,
) {
    let Ok(candidate) = fs::canonicalize(candidate) else {
        return;
    };
    if readable_root_is_safe(&candidate, real_home, real_workspace) {
        roots.insert(candidate);
    }
}

/// The read boundary for one platform, given that platform's system roots
/// ([`MACOS_SYSTEM_READ_ROOTS`] or [`LINUX_SYSTEM_READ_ROOTS`]).
///
/// Shared rather than one function per platform because everything interesting
/// here is platform-independent policy — PATH entries become roots, Homebrew and
/// rustup get narrowed to their executable/library subtrees, and the real
/// workspace and whole-home roots are filtered out — and two copies of that
/// would be two chances for one of them to drift wider than the other.
fn readable_roots(
    system_roots: &[&str],
    path_env: Option<&OsStr>,
    real_home: Option<&Path>,
    real_workspace: &Path,
) -> Vec<PathBuf> {
    let canonical_home = real_home.and_then(|path| fs::canonicalize(path).ok());
    let real_home = canonical_home.as_deref().or(real_home);
    let canonical_workspace =
        fs::canonicalize(real_workspace).unwrap_or_else(|_| real_workspace.to_path_buf());
    let mut candidates: Vec<PathBuf> = system_roots.iter().map(PathBuf::from).collect();
    let path_entries: Vec<PathBuf> = path_env
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|path| path.is_absolute())
        .collect();
    candidates.extend(path_entries.iter().cloned());

    // Homebrew's PATH entries are mostly symlinks into Cellar/opt. Permit
    // only executable/library/share roots, never its `etc` or `var` trees.
    for prefix in [Path::new("/opt/homebrew"), Path::new("/usr/local")] {
        if path_entries.iter().any(|entry| entry.starts_with(prefix)) {
            for child in ["bin", "sbin", "Cellar", "opt", "lib", "share"] {
                candidates.push(prefix.join(child));
            }
        }
    }

    // rustup's proxies live in ~/.cargo/bin while their executable
    // toolchains live elsewhere. Cargo registries, git caches, config, and
    // credentials remain outside the read boundary; network-enabled runs
    // can populate a fresh cache under the sandbox-owned HOME instead.
    if let Some(home) = real_home {
        let cargo_bin = home.join(".cargo/bin");
        if path_entries
            .iter()
            .any(|entry| entry.starts_with(&cargo_bin))
        {
            candidates.push(home.join(".rustup/toolchains"));
            candidates.push(home.join(".rustup/settings.toml"));
        }
    }

    let mut roots = BTreeSet::new();
    for candidate in candidates {
        insert_existing_read_root(&mut roots, &candidate, real_home, &canonical_workspace);
    }
    roots.into_iter().collect()
}

/// Gated because only the macOS branch sets `RUSTUP_HOME`. The Linux branch
/// still picks up `~/.rustup/toolchains` as a read root through
/// [`readable_roots`], but it does not point a sandboxed Cargo at it.
#[cfg(target_os = "macos")]
fn macos_path_uses_rustup(path_env: Option<&OsStr>, real_home: &Path) -> bool {
    let cargo_bin = real_home.join(".cargo/bin");
    path_env
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .any(|entry| entry.is_absolute() && entry.starts_with(&cargo_bin))
}

fn seatbelt_escape(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn seatbelt_filters(paths: &BTreeSet<PathBuf>, operator: &str) -> String {
    paths
        .iter()
        .map(|path| format!("  ({operator} \"{}\")\n", seatbelt_escape(path)))
        .collect()
}

fn traversal_ancestors(paths: &BTreeSet<PathBuf>) -> BTreeSet<PathBuf> {
    let mut ancestors = BTreeSet::new();
    for path in paths {
        let mut parent = path.parent();
        while let Some(current) = parent {
            if current == Path::new("/") {
                break;
            }
            ancestors.insert(current.to_path_buf());
            parent = current.parent();
        }
    }
    ancestors
}

/// Pure string builder for a deny-by-default macOS Seatbelt profile.
/// Content reads and executable mappings are scoped to `sandbox_root` plus
/// explicit system/toolchain roots. Writes remain scoped to the sandbox
/// root. Enabling network changes only the network clause and can never
/// widen filesystem access.
pub fn build_seatbelt_profile(
    sandbox_root: &Path,
    readable_roots: &[PathBuf],
    allow_network: bool,
) -> String {
    let mut read_roots = BTreeSet::new();
    read_roots.insert(sandbox_root.to_path_buf());
    read_roots.extend(
        readable_roots
            .iter()
            // `has_root` rather than `is_absolute`: identical on Unix, but keeps
            // this pure builder deterministic on Windows, where Unix-style
            // Seatbelt roots like "/System/Library" have no drive prefix and
            // `is_absolute()` would drop them (the profile is only ever
            // consumed by sandbox-exec on macOS).
            .filter(|path| path.has_root() && path.as_path() != Path::new("/"))
            .cloned(),
    );
    let ancestors = traversal_ancestors(&read_roots);
    let read_filters = seatbelt_filters(&read_roots, "subpath");
    let ancestor_filters = seatbelt_filters(&ancestors, "literal");
    let sandbox_root = seatbelt_escape(sandbox_root);
    let network_clause = if allow_network {
        "(allow network*)"
    } else {
        "(deny network*)"
    };
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-fork)\n\
         (allow process-exec\n\
         {read_filters})\n\
         (allow file-read*\n\
           (literal \"/\")\n\
         {read_filters})\n\
         (allow file-read-metadata\n\
         {ancestor_filters})\n\
         (allow file-write* (subpath \"{sandbox_root}\"))\n\
         (allow file-ioctl (subpath \"{sandbox_root}\"))\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target self))\n\
         {network_clause}\n"
    )
}

pub struct SandboxExecOutcome {
    pub isolation: Isolation,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub duration_ms: u64,
}

/// Spawns `shell_command` with `cwd` set to `workspace_dir`, an allowlisted
/// env (see [`allowlisted_env`]), and a wall-clock `timeout`. `sandbox_root`
/// owns the copied workspace, HOME, TMP, and Seatbelt profile; the
/// `real_workspace_root` is an explicit forbidden read boundary. On macOS the
/// command is additionally wrapped in `sandbox-exec` with a generated
/// Seatbelt profile written to `profile_path` (a sibling of `workspace_dir`,
/// never inside it, so it never shows up as an unexpected file when diffing
/// the copy against the real workspace). On timeout the child is killed and
/// any output captured so far is discarded (matching `tools::tool_run_shell`'s
/// existing timeout behavior) — `timed_out` is still reported accurately.
pub async fn execute_in_sandbox(
    sandbox_root: &Path,
    workspace_dir: &Path,
    real_workspace_root: &Path,
    profile_path: &Path,
    shell_command: &str,
    timeout: Duration,
    allow_network: bool,
    approved_env: &[String],
) -> io::Result<SandboxExecOutcome> {
    let sandbox_root = fs::canonicalize(sandbox_root)?;
    let workspace_dir = fs::canonicalize(workspace_dir)?;
    let real_workspace_root = fs::canonicalize(real_workspace_root)?;
    if !workspace_dir.starts_with(&sandbox_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox workspace must be inside the sandbox root",
        ));
    }
    if paths_overlap(&sandbox_root, &real_workspace_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "sandbox root must not overlap the real workspace",
        ));
    }

    let profile_parent = profile_path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Seatbelt profile must have a parent directory",
        )
    })?;
    let profile_parent = fs::canonicalize(profile_parent)?;
    if !profile_parent.starts_with(&sandbox_root) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Seatbelt profile must be inside the sandbox root",
        ));
    }

    let sandbox_home = sandbox_root.join(SANDBOX_HOME_DIR);
    let sandbox_tmp = sandbox_root.join(SANDBOX_TMP_DIR);
    fs::create_dir_all(&sandbox_home)?;
    fs::create_dir_all(&sandbox_tmp)?;
    let env = allowlisted_env(&sandbox_home, &sandbox_tmp, approved_env);
    let started = std::time::Instant::now();

    #[cfg(target_os = "macos")]
    let mut env = env;
    // Needed by both OS boundaries: it is the one path that must never become a
    // read root, and the anchor for the toolchain roots that may.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let real_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .and_then(|path| fs::canonicalize(path).ok());
    #[cfg(target_os = "macos")]
    if let Some(home) = real_home.as_deref() {
        let path_env = env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| OsStr::new(value));
        let rustup_home = home.join(".rustup");
        if macos_path_uses_rustup(path_env, home) && rustup_home.is_dir() {
            // This is a computed, read-only toolchain location, not inherited
            // user configuration. CARGO_HOME remains under sandbox HOME, so
            // Cargo credentials/config/registries are never exposed.
            set_env_value(
                &mut env,
                "RUSTUP_HOME",
                rustup_home.to_string_lossy().into_owned(),
            );
        }
    }

    #[cfg(target_os = "macos")]
    let (program, args, isolation) = {
        let path_env = env
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| OsStr::new(value));
        let readable_roots = readable_roots(
            MACOS_SYSTEM_READ_ROOTS,
            path_env,
            real_home.as_deref(),
            &real_workspace_root,
        );
        let profile = build_seatbelt_profile(&sandbox_root, &readable_roots, allow_network);
        fs::write(profile_path, profile)?;
        (
            SANDBOX_EXEC.to_string(),
            vec![
                "-f".to_string(),
                profile_path.to_string_lossy().to_string(),
                "--".to_string(),
                "/bin/sh".to_string(),
                "-c".to_string(),
                shell_command.to_string(),
            ],
            Isolation::OsSandboxed,
        )
    };

    // Windows leaves the shared `Command` path entirely: an AppContainer's
    // capabilities can only be handed to `CreateProcessW` through a
    // `STARTUPINFOEX` attribute list, which `Command` cannot build. So the whole
    // spawn, wait and timeout live in `sandbox_windows::run_confined`, and this
    // returns from here rather than falling through to code that would spawn a
    // second, unconfined child.
    #[cfg(target_os = "windows")]
    {
        let job = crate::sandbox_windows::create_job()?;
        // The container is the filesystem boundary; the job is the process-tree
        // one. A machine that cannot give us the container still gets the job,
        // and says so, rather than failing the run or overstating it.
        // A fresh name per run, so two concurrent sandboxed runs never share a
        // container and one finishing never deletes the other's profile.
        let container =
            crate::sandbox_windows::create_app_container(&uuid::Uuid::new_v4().simple().to_string());
        let container = match container {
            Ok(container) => {
                // The single grant that makes the sandbox copy reachable. Fatal:
                // without it the child is inside a container that cannot read its
                // own working directory, which is a broken run, not a confined
                // one.
                container.grant_tree_access(&sandbox_root)?;
                Some(container)
            }
            Err(error) => {
                // Degrade rather than fail, matching the Linux path on a kernel
                // without Landlock: the job still holds the process tree, and
                // `ProcessContained` is the honest name for that.
                eprintln!(
                    "sandbox: no AppContainer for this run, continuing without a \
                     filesystem boundary: {error}"
                );
                None
            }
        };
        let isolation = match container.is_some() {
            true => Isolation::OsSandboxed,
            false => Isolation::ProcessContained,
        };
        let output = crate::sandbox_windows::run_confined(
            container.as_ref(),
            &job,
            shell_command,
            &workspace_dir,
            &env,
            allow_network,
            timeout,
        )
        .await?;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        return Ok(SandboxExecOutcome {
            isolation,
            exit_code: output.exit_code,
            timed_out: output.timed_out,
            stdout: output.stdout,
            stderr: output.stderr,
            duration_ms,
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let (program, args, isolation) = (
        "sh".to_string(),
        vec!["-c".to_string(), shell_command.to_string()],
        Isolation::ProcessOnly,
    );

    // Every platform but Windows spawns through `tokio::process` from here.
    // Windows returned above from its own `CreateProcessW` path, and this is
    // `cfg`-gated rather than left unreachable so that `program`, `args` and
    // `isolation` — none of which the Windows arm defines — are not even named
    // there.
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(&workspace_dir)
            .env_clear()
            .envs(env.iter().cloned())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so the timeout below ends the whole tree. Without it
        // `kill_on_drop` reaps only `sandbox-exec` (or the bare `sh` on the platforms
        // with no Seatbelt), and a sandboxed command that spawns a build leaves that
        // build running — with write access to the sandbox copy of the workspace and
        // no remaining supervisor.
        #[cfg(unix)]
        command.process_group(0);
        // Kernel-held bounds, which matter most here: on the platforms with no
        // Seatbelt this is `Isolation::ProcessOnly`, so `os_limits` is the only
        // enforcement the OS applies to the child at all. Inherited across the
        // `sandbox-exec` exec, so it reaches the sandboxed program itself.
        crate::os_limits::apply(crate::os_limits::ChildLimits::baseline(), &mut command);
        // Linux's boundary is installed the same way, and *after* `os_limits` on
        // purpose: `pre_exec` closures run in registration order, so the `setrlimit`
        // bounds are already in place before anything starts denying syscalls.
        // Reported from `confine`'s own answer rather than from the target triple —
        // a kernel without Landlock keeps the `ProcessOnly` the branch above chose.
        #[cfg(target_os = "linux")]
        let isolation = {
            let path_env = env
                .iter()
                .find(|(key, _)| key == "PATH")
                .map(|(_, value)| OsStr::new(value));
            let readable_roots = readable_roots(
                LINUX_SYSTEM_READ_ROOTS,
                path_env,
                real_home.as_deref(),
                &real_workspace_root,
            );
            match crate::sandbox_linux::confine(
                &mut command,
                &sandbox_root,
                &readable_roots,
                allow_network,
            )? {
                true => Isolation::OsSandboxed,
                false => isolation,
            }
        };

        // Windows never reaches here — it returned above, from its own
        // `CreateProcessW` path.
        let child = command.spawn()?;

        // Captured before `wait_with_output` consumes the child; with
        // `process_group(0)` the child's own pid is also its group id.
        let child_pgid = child.id();
        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        if result.is_err() {
            if let Some(pgid) = child_pgid {
                if let Err(error) = crate::os_signal::terminate_process_group(pgid) {
                    eprintln!("sandbox: could not terminate process group {pgid}: {error}");
                }
            }
        }

        match result {
            Ok(Ok(output)) => Ok(SandboxExecOutcome {
                isolation,
                exit_code: output.status.code(),
                timed_out: false,
                stdout: output.stdout,
                stderr: output.stderr,
                duration_ms,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Ok(SandboxExecOutcome {
                isolation,
                exit_code: None,
                timed_out: true,
                stdout: Vec::new(),
                stderr: Vec::new(),
                duration_ms,
            }),
        }
    }
}

fn sandbox_target_snapshot() -> ModelTargetSnapshot {
    let evidence = "Sandboxed runs execute a shell command; no model inference occurs.".to_string();
    let unsupported = || CapabilityAssessment {
        state: CapabilityState::Unsupported,
        evidence: evidence.clone(),
    };
    ModelTargetSnapshot::Ollama {
        target_id: "sandbox-shell".to_string(),
        label: "Sandboxed shell execution".to_string(),
        base_url: "http://127.0.0.1:0".to_string(),
        model: "none".to_string(),
        is_cloud: false,
        capabilities: ModelCapabilitiesSnapshot {
            tool_calling: unsupported(),
            vision: unsupported(),
            embeddings: unsupported(),
            structured_output: unsupported(),
            image_generation: unsupported(),
            audio: unsupported(),
            runtime_lifecycle: unsupported(),
            fim: unsupported(),
            code_completion: unsupported(),
            inline_edit: unsupported(),
            fim_metadata: None,
        },
        estimated_memory_bytes: None,
    }
}

fn build_sandbox_run_spec(
    run_id: &str,
    submitted_by: ClientIdentity,
    root: &Path,
    request: &SandboxRunRequest,
    created_at_ms: u64,
) -> Result<RunSpec, String> {
    let spec = RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        idempotency_key: format!("sandbox/{run_id}"),
        created_at_ms,
        kind: RunKind::Sandboxed,
        submitted_by,
        task: format!("Sandboxed shell command:\n{}", request.command),
        instructions: None,
        input_artifact_ids: Vec::new(),
        target: sandbox_target_snapshot(),
        workspace: Some(WorkspaceContext {
            workspace_id: "sandbox".to_string(),
            primary_root_id: "root-primary".to_string(),
            roots: vec![RootGrant {
                root_id: "root-primary".to_string(),
                canonical_path: root.to_string_lossy().to_string(),
                access: RootAccess::ReadOnly,
                allow_symlinks_within_root: false,
            }],
            repository_policy: None,
        }),
        permission_policy: PermissionPolicySnapshot {
            mode: PermissionMode::Auto,
            unattended: true,
            approval_timeout_ms: 60_000,
            default_tool_decision: ToolPolicyDecision::Allow,
            tool_rules: Vec::new(),
            allow_network: request.allow_network,
            allow_external_mutations: false,
            egress_allowlist: None,
        },
        budgets: RunBudgets {
            wall_time_ms: request.timeout().as_millis() as u64,
            max_iterations: 1,
            max_model_calls: 1,
            max_tool_calls: 1,
            max_input_tokens: 1,
            max_output_tokens: 1,
            max_cost_micros: None,
            max_artifact_bytes: MAX_ARTIFACT_BYTES_BUDGET,
            max_event_count: 64,
        },
    };
    spec.validate().map_err(|error| error.to_string())?;
    Ok(spec)
}

fn sandbox_run_dir(app: &tauri::AppHandle, run_id: &str) -> Result<PathBuf, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data dir: {error}"))?;
    Ok(data_dir.join(SANDBOX_RUNS_DIR).join(run_id))
}

fn require_sandboxed_run(
    app: &tauri::AppHandle,
    state: &AppState,
    run_id: &str,
) -> Result<crate::run_ledger::StoredRun, String> {
    let run = crate::run_commands::with_ledger(app, state, |ledger| {
        ledger
            .load_run(run_id)?
            .ok_or_else(|| crate::run_ledger::LedgerError::NotFound {
                entity: "run",
                id: run_id.to_string(),
            })
    })?;
    if run.spec.kind != RunKind::Sandboxed {
        return Err("Run is not a sandboxed execution".to_string());
    }
    Ok(run)
}

fn expect_matching_root(run: &crate::run_ledger::StoredRun, current_root: &Path) -> Result<(), String> {
    let recorded = run
        .spec
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.roots.first())
        .map(|root| root.canonical_path.as_str())
        .ok_or_else(|| "Sandboxed run has no recorded workspace root".to_string())?;
    if recorded != current_root.to_string_lossy() {
        return Err(
            "The primary workspace has changed since this sandbox run started".to_string(),
        );
    }
    Ok(())
}

/// Rejects absolute paths, empty paths, and any `..`/root component — the
/// only components a promote path may contain are ordinary path segments.
fn validate_relative_promote_path(candidate: &str) -> Result<PathBuf, String> {
    if candidate.is_empty() || candidate.len() > 4_096 || candidate.contains('\0') {
        return Err(format!("Invalid file path: '{candidate}'"));
    }
    let path = Path::new(candidate);
    if path.is_absolute() {
        return Err(format!("File path must be relative: '{candidate}'"));
    }
    for component in path.components() {
        match component {
            std::path::Component::Normal(_) => {}
            _ => {
                return Err(format!(
                    "File path must not contain '..' or a root component: '{candidate}'"
                ))
            }
        }
    }
    Ok(path.to_path_buf())
}

fn compute_promote_digest(run_id: &str, files: &[PromoteFileEntry]) -> String {
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));
    let mut buffer = format!("run:{run_id}\n");
    for file in &sorted {
        buffer.push_str(&format!("{}:{}:{}\n", file.path, file.sha256, file.size_bytes));
    }
    sha256_hex_bytes(buffer.as_bytes())
}

/// Validates and hashes the requested files (as they currently exist in the
/// sandbox copy) and builds the exact preview a caller must replay back
/// (digest + confirmation phrase) to promote them. Pure filesystem read —
/// never touches the real workspace and never mutates any shared state.
pub fn build_promote_preview(
    run_id: &str,
    sandbox_workspace_dir: &Path,
    files: &[String],
    now_ms: u64,
    ttl_ms: u64,
) -> Result<SandboxPromotePreview, String> {
    if files.is_empty() {
        return Err("Select at least one file to promote".to_string());
    }
    if files.len() > MAX_PROMOTE_FILES {
        return Err(format!(
            "At most {MAX_PROMOTE_FILES} files can be promoted in a single action"
        ));
    }

    let mut seen = HashSet::new();
    let mut entries = Vec::with_capacity(files.len());
    for raw in files {
        let relative = validate_relative_promote_path(raw)?;
        let normalized = relative.to_string_lossy().replace('\\', "/");
        if !seen.insert(normalized.clone()) {
            return Err(format!("Duplicate file in promote request: '{normalized}'"));
        }
        let absolute = sandbox_workspace_dir.join(&relative);
        if !absolute.is_file() {
            return Err(format!(
                "'{normalized}' was not found in the sandbox copy"
            ));
        }
        let (sha256, size_bytes) = hash_file(&absolute).map_err(|error| {
            format!("Failed to read '{normalized}' from the sandbox copy: {error}")
        })?;
        entries.push(PromoteFileEntry {
            path: normalized,
            sha256,
            size_bytes,
        });
    }

    let digest = compute_promote_digest(run_id, &entries);
    let expires_at_ms = now_ms
        .checked_add(ttl_ms)
        .ok_or_else(|| "Confirmation expiry overflow".to_string())?;

    Ok(SandboxPromotePreview {
        run_id: run_id.to_string(),
        digest: digest.clone(),
        confirmation_phrase: confirmation_phrase_for(&digest),
        files: entries,
        expires_at_ms,
    })
}

/// Checks the digest shape, the exact confirmation phrase, that a pending
/// preview for this digest actually exists, that it belongs to the claimed
/// run, and that it has not expired — all before any file is ever touched.
/// On any failure this returns `Err` and the caller (see
/// `sandbox_execute_promote`) never proceeds to read or write anything.
fn validate_promote_confirmation(
    pending: Option<&PendingPromote>,
    run_id: &str,
    digest: &str,
    confirmation_phrase: &str,
    now_ms: u64,
) -> Result<PendingPromote, String> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("Invalid promote digest".to_string());
    }
    if confirmation_phrase != confirmation_phrase_for(digest) {
        return Err("Type the exact confirmation phrase shown in the preview".to_string());
    }
    let pending = pending.ok_or_else(|| {
        "This promote confirmation has expired or was already used; prepare it again".to_string()
    })?;
    if pending.run_id != run_id {
        return Err("This confirmation does not belong to the specified sandbox run".to_string());
    }
    if now_ms > pending.expires_at_ms {
        return Err("This promote confirmation has expired; prepare it again".to_string());
    }
    Ok(pending.clone())
}

/// Re-hashes the sandbox copy for exactly the files a preview covered and
/// confirms the digest still matches — i.e. nothing changed in the sandbox
/// between prepare and execute.
fn verify_unchanged_since_preview(
    run_id: &str,
    sandbox_workspace_dir: &Path,
    pending: &PendingPromote,
    digest: &str,
) -> Result<(), String> {
    let paths: Vec<String> = pending.files.iter().map(|file| file.path.clone()).collect();
    let fresh = build_promote_preview(run_id, sandbox_workspace_dir, &paths, 0, 0)?;
    if fresh.digest != digest {
        return Err(
            "Sandbox files changed since the promote preview was generated; prepare it again"
                .to_string(),
        );
    }
    Ok(())
}

fn atomic_write(dest: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = dest.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no parent directory")
    })?;
    fs::create_dir_all(parent)?;
    let file_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let tmp_path = parent.join(format!(
        ".{file_name}.sandbox-tmp-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, dest)
}

/// Copies exactly `files` from the sandbox copy into the real workspace,
/// re-verifying each file's hash immediately before writing it (defense in
/// depth on top of [`verify_unchanged_since_preview`]'s whole-set check) and
/// writing every destination atomically (temp file + rename). Stops at the
/// first failure — files already written are not rolled back, but nothing
/// is written at all unless every prior check in the caller already passed.
pub fn promote_files(
    sandbox_workspace_dir: &Path,
    real_root: &Path,
    files: &[PromoteFileEntry],
) -> Result<Vec<String>, String> {
    let mut promoted = Vec::with_capacity(files.len());
    for file in files {
        let relative = validate_relative_promote_path(&file.path)?;
        let source = sandbox_workspace_dir.join(&relative);
        let bytes = fs::read(&source).map_err(|error| {
            format!("Failed to read '{}' from the sandbox copy: {error}", file.path)
        })?;
        if sha256_hex_bytes(&bytes) != file.sha256 {
            return Err(format!(
                "'{}' changed in the sandbox copy since the preview was generated",
                file.path
            ));
        }
        let destination = real_root.join(&relative);
        atomic_write(&destination, &bytes).map_err(|error| {
            format!("Failed to write '{}' to the workspace: {error}", file.path)
        })?;
        promoted.push(file.path.clone());
    }
    Ok(promoted)
}

/// Lists files that differ between the sandbox copy and the real workspace
/// (added or modified only — see [`SandboxDiffEntry::status`]).
pub fn diff_sandbox_against_workspace(
    sandbox_workspace_dir: &Path,
    real_root: &Path,
) -> Result<Vec<SandboxDiffEntry>, String> {
    let mut entries = Vec::new();
    let walker = walkdir::WalkDir::new(sandbox_workspace_dir)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| {
            !(entry.file_type().is_dir()
                && is_skippable_dir_name(&entry.file_name().to_string_lossy()))
        });

    for entry in walker {
        let entry = entry.map_err(|error| error.to_string())?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(sandbox_workspace_dir)
            .unwrap_or(entry.path());
        let (sandbox_sha256, size_bytes) = hash_file(entry.path()).map_err(|e| e.to_string())?;
        let real_path = real_root.join(rel);
        let workspace_sha256 = if real_path.is_file() {
            Some(hash_file(&real_path).map_err(|e| e.to_string())?.0)
        } else {
            None
        };
        let status = match &workspace_sha256 {
            None => "added",
            Some(hash) if *hash == sandbox_sha256 => continue,
            Some(_) => "modified",
        };
        entries.push(SandboxDiffEntry {
            path: rel.to_string_lossy().replace('\\', "/"),
            status: status.to_string(),
            sandbox_sha256,
            workspace_sha256,
            size_bytes,
        });
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

async fn run_sandboxed_body(
    app: &tauri::AppHandle,
    state: &AppState,
    run_id: &str,
    root: &Path,
    sandbox_root: &Path,
    workspace_dir: &Path,
    profile_path: &Path,
    request: &SandboxRunRequest,
    engine: &ClientIdentity,
) -> Result<SandboxRunSummary, String> {
    let stats = copy_workspace_into_sandbox(root, workspace_dir)
        .map_err(|error| format!("Failed to create the ephemeral sandbox copy: {error}"))?;

    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::CheckpointLinked {
            checkpoint_id: format!("sandbox-copy-{run_id}"),
            kind: CheckpointKind::Workspace,
            // The placement mode is part of the record, not decoration: two runs
            // with the same file and byte counts can have cost wildly different
            // amounts of disk, and the ledger is where that is answerable later.
            label: bounded(
                &format!(
                    "Ephemeral copy: {} file(s), {} byte(s), {} ({} cloned)",
                    stats.files_copied,
                    stats.bytes_copied,
                    stats.placement_mode(),
                    stats.files_cloned
                ),
                1_024,
            ),
            content_sha256: None,
        },
        engine.clone(),
    )?;

    let outcome = execute_in_sandbox(
        sandbox_root,
        workspace_dir,
        root,
        profile_path,
        &request.command,
        request.timeout(),
        request.allow_network,
        &request.approved_env,
    )
    .await
    .map_err(|error| format!("Failed to execute the sandboxed command: {error}"))?;

    let store = crate::artifact_commands::store_for(app, state)?;
    let stdout_blob = store.put(&outcome.stdout).map_err(|error| error.to_string())?;
    let stderr_blob = store.put(&outcome.stderr).map_err(|error| error.to_string())?;

    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::ArtifactAdded {
            artifact_id: stdout_blob.id.clone(),
            kind: ArtifactKind::Report,
            name: "stdout.log".to_string(),
            media_type: "text/plain".to_string(),
            content_sha256: stdout_blob.id.clone(),
            size_bytes: stdout_blob.size,
        },
        engine.clone(),
    )?;
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::ArtifactAdded {
            artifact_id: stderr_blob.id.clone(),
            kind: ArtifactKind::Report,
            name: "stderr.log".to_string(),
            media_type: "text/plain".to_string(),
            content_sha256: stderr_blob.id.clone(),
            size_bytes: stderr_blob.size,
        },
        engine.clone(),
    )?;

    let passed = !outcome.timed_out && outcome.exit_code == Some(0);
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.to_string(),
        None,
        RunEvent::VerificationFinished {
            verification_id: format!("sandbox-exec-{run_id}"),
            name: "Sandboxed command execution".to_string(),
            passed,
            summary: bounded(
                &format!(
                    "isolation={:?} exit_code={:?} timed_out={} duration_ms={}",
                    outcome.isolation, outcome.exit_code, outcome.timed_out, outcome.duration_ms
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
            artifact_ids: vec![stdout_blob.id.clone(), stderr_blob.id.clone()],
            duration_ms: outcome.duration_ms,
        },
        engine.clone(),
    )?;

    Ok(SandboxRunSummary {
        run_id: run_id.to_string(),
        isolation: outcome.isolation,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        passed,
        duration_ms: outcome.duration_ms,
        stdout_artifact_id: stdout_blob.id,
        stderr_artifact_id: stderr_blob.id,
        stdout_excerpt: bounded(&String::from_utf8_lossy(&outcome.stdout), MAX_EVENT_TEXT_EXCERPT),
        stderr_excerpt: bounded(&String::from_utf8_lossy(&outcome.stderr), MAX_EVENT_TEXT_EXCERPT),
        files_copied: stats.files_copied,
    })
}

async fn run_sandboxed(
    app: &tauri::AppHandle,
    window: &tauri::Window,
    state: &AppState,
    request: SandboxRunRequest,
) -> Result<SandboxRunSummary, String> {
    request.validate()?;
    let root = workspace::primary_root_canon(state)?;
    let run_id = format!("sandbox-{}", uuid::Uuid::new_v4().simple());
    let sandbox_root = sandbox_run_dir(app, &run_id)?;
    fs::create_dir_all(&sandbox_root)
        .map_err(|error| format!("Failed to create the sandbox run directory: {error}"))?;
    let workspace_dir = sandbox_root.join("workspace");
    let profile_path = sandbox_root.join("seatbelt.sb");

    let identity = crate::run_commands::desktop_identity(app, window);
    let created_at_ms = crate::run_commands::unix_time_ms()?;
    let spec = build_sandbox_run_spec(&run_id, identity, &root, &request, created_at_ms)?;
    crate::run_commands::with_ledger(app, state, |ledger| ledger.submit_run(&spec))?;

    let engine = crate::run_commands::engine_identity(app, "sandbox");
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.clone(),
        None,
        RunEvent::Queued { queue: None },
        engine.clone(),
    )?;
    crate::run_commands::append_event_as(
        app,
        state,
        run_id.clone(),
        None,
        RunEvent::Started {
            engine_id: "sandbox".to_string(),
        },
        engine.clone(),
    )?;

    let outcome = run_sandboxed_body(
        app,
        state,
        &run_id,
        &root,
        &sandbox_root,
        &workspace_dir,
        &profile_path,
        &request,
        &engine,
    )
    .await;

    match outcome {
        Ok(summary) => Ok(summary),
        Err(error) => {
            let _ = crate::run_commands::append_event_as(
                app,
                state,
                run_id.clone(),
                None,
                RunEvent::Failed {
                    code: "sandbox_error".to_string(),
                    message: bounded(&error, MAX_EVENT_TEXT_EXCERPT),
                    retryable: false,
                },
                engine,
            );
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn sandbox_run(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    command: String,
    timeout_ms: Option<u64>,
    allow_network: bool,
    approved_env: Vec<String>,
) -> Result<SandboxRunSummary, String> {
    let request = SandboxRunRequest {
        command,
        timeout_ms,
        allow_network,
        approved_env,
    };
    run_sandboxed(&app, &window, state.inner(), request).await
}

#[tauri::command]
pub fn sandbox_list(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<SandboxRunListEntry>, String> {
    let runs = crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
        ledger.list_runs(200, false)
    })?;
    Ok(runs
        .into_iter()
        .filter(|run| run.spec.kind == RunKind::Sandboxed)
        .map(|run| SandboxRunListEntry {
            run_id: run.spec.run_id.clone(),
            status: run.status,
            task: run.spec.task.clone(),
            created_at_ms: run.spec.created_at_ms,
            updated_at_ms: run.updated_at_ms,
        })
        .collect())
}

#[tauri::command]
pub fn sandbox_diff(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
) -> Result<Vec<SandboxDiffEntry>, String> {
    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    if !workspace_dir.is_dir() {
        return Err("The sandbox copy for this run is no longer available".to_string());
    }
    diff_sandbox_against_workspace(&workspace_dir, &root)
}

/// What this machine can enforce, asked *before* a run rather than reported after.
///
/// The Sandbox panel shows the same Run button on every platform, and the isolation
/// label only appears once a run has already executed — which is the wrong order
/// for a decision about running untrusted code. This is the same probe Security
/// Doctor uses, so the panel and the audit cannot disagree.
#[tauri::command]
pub fn sandbox_enforcement_probe() -> SandboxEnforcement {
    sandbox_enforcement()
}

#[tauri::command]
pub fn sandbox_prepare_promote(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
    files: Vec<String>,
) -> Result<SandboxPromotePreview, String> {
    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    let now = crate::run_commands::unix_time_ms()?;
    let preview = build_promote_preview(&run_id, &workspace_dir, &files, now, PROMOTE_PREVIEW_TTL_MS)?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.insert(
            preview.digest.clone(),
            PendingPromote {
                run_id: run_id.clone(),
                files: preview.files.clone(),
                expires_at_ms: preview.expires_at_ms,
            },
        );
    }

    let identity = crate::run_commands::engine_identity(&app, "sandbox-promote");
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::ExternalMutationPrepared {
            mutation_id: preview.digest[..24].to_string(),
            tool_call_id: format!("promote-{run_id}"),
            kind: MutationKind::Filesystem,
            idempotency_key: Some(preview.digest.clone()),
            summary: bounded(
                &format!(
                    "Promote {} file(s) from the sandbox to the workspace",
                    preview.files.len()
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
        },
        identity,
    )?;

    Ok(preview)
}

#[tauri::command]
pub fn sandbox_execute_promote(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    run_id: String,
    digest: String,
    confirmation_phrase: String,
) -> Result<SandboxPromoteResult, String> {
    let now = crate::run_commands::unix_time_ms()?;
    let pending_snapshot = {
        let guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.get(&digest).cloned()
    };
    let pending = validate_promote_confirmation(
        pending_snapshot.as_ref(),
        &run_id,
        &digest,
        &confirmation_phrase,
        now,
    )?;

    let run = require_sandboxed_run(&app, state.inner(), &run_id)?;
    let root = workspace::primary_root_canon(state.inner())?;
    expect_matching_root(&run, &root)?;
    let workspace_dir = sandbox_run_dir(&app, &run_id)?.join("workspace");
    verify_unchanged_since_preview(&run_id, &workspace_dir, &pending, &digest)?;

    let promoted = promote_files(&workspace_dir, &root, &pending.files)?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.remove(&digest);
    }

    let identity = crate::run_commands::engine_identity(&app, "sandbox-promote");
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::ExternalMutationConfirmed {
            mutation_id: digest[..24].to_string(),
            confirmation_ref: None,
            summary: bounded(
                &format!(
                    "Promoted {} file(s) from the sandbox to the workspace",
                    promoted.len()
                ),
                MAX_EVENT_TEXT_EXCERPT,
            ),
        },
        identity.clone(),
    )?;
    crate::run_commands::append_event_as(
        &app,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::Completed {
            summary: Some(bounded(
                &format!("Promoted {} file(s): {}", promoted.len(), promoted.join(", ")),
                MAX_EVENT_TEXT_EXCERPT,
            )),
            result_artifact_ids: Vec::new(),
            usage: UsageSnapshot {
                input_tokens: 0,
                output_tokens: 0,
                cached_input_tokens: 0,
                model_calls: 0,
                tool_calls: 1,
                cost_micros: None,
            },
        },
        identity,
    )?;

    Ok(SandboxPromoteResult {
        run_id,
        promoted_files: promoted,
    })
}

#[tauri::command]
pub fn sandbox_discard(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    run_id: String,
    reason: Option<String>,
) -> Result<(), String> {
    require_sandboxed_run(&app, state.inner(), &run_id)?;
    crate::run_commands::append_host_event(
        &app,
        &window,
        state.inner(),
        run_id.clone(),
        None,
        RunEvent::Cancelled { reason },
    )?;

    {
        let mut guard = state
            .inner()
            .sandbox
            .previews
            .lock()
            .map_err(|_| "Sandbox preview lock poisoned".to_string())?;
        guard.retain(|_, pending| pending.run_id != run_id);
    }

    let dir = sandbox_run_dir(&app, &run_id)?;
    if dir.exists() {
        let _ = fs::remove_dir_all(&dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_millis() as u64
    }

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let counter = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "little-monkey-sandbox-test-{label}-{}-{counter}-{}",
            std::process::id(),
            now_ms()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create parent");
        }
        fs::write(path, content).expect("write fixture file");
    }

    // --- copy_workspace_into_sandbox -----------------------------------

    /// The acceptance clause this whole change has to earn: a cloned sandbox is
    /// byte-for-byte the tree the copy implementation produced.
    ///
    /// Asserted against a copy made from the *same* fixture rather than against
    /// a written-down expectation, so the two implementations are compared to
    /// each other and not to somebody's idea of what they do. On a filesystem
    /// that cannot clone, the two are the same code path and the test still
    /// holds — it just stops proving anything new, which is why the clone count
    /// is reported rather than asserted.
    #[test]
    fn a_cloned_sandbox_is_byte_for_byte_the_tree_a_copy_produces() {
        let root = temp_dir("clone-src");
        let cloned = temp_dir("clone-dest");

        write(&root.join("src/main.rs"), "fn main() { println!(\"hi\"); }");
        write(&root.join("src/nested/deep/file.txt"), "deep content");
        write(&root.join("package.json"), "{\"name\":\"fixture\"}");
        write(&root.join(".env"), "API_KEY=super-secret");
        write(
            &root.join("node_modules/pkg/index.js"),
            "module.exports = {};",
        );
        // An empty file and a large-ish one: clonefile's edge is zero-length
        // extents, and a multi-block file is the case a clone actually saves on.
        write(&root.join("empty"), "");
        write(&root.join("big.bin"), &"x".repeat(1_000_000));

        let stats = copy_workspace_into_sandbox(&root, &cloned).expect("the sandbox is created");

        // The same fixture through `fs::copy` alone, for comparison.
        let copied = temp_dir("clone-reference");
        for relative in [
            "src/main.rs",
            "src/nested/deep/file.txt",
            "package.json",
            "empty",
            "big.bin",
        ] {
            let destination = copied.join(relative);
            fs::create_dir_all(destination.parent().expect("a parent")).expect("create parent");
            fs::copy(root.join(relative), &destination).expect("reference copy");
        }

        let listing = |base: &Path| {
            let mut found: Vec<(String, Vec<u8>)> = walkdir::WalkDir::new(base)
                .min_depth(1)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_type().is_file())
                .map(|entry| {
                    (
                        entry
                            .path()
                            .strip_prefix(base)
                            .expect("under the base")
                            .to_string_lossy()
                            .into_owned(),
                        fs::read(entry.path()).expect("the file reads"),
                    )
                })
                .collect();
            found.sort();
            found
        };

        assert_eq!(
            listing(&cloned),
            listing(&copied),
            "a cloned sandbox must contain exactly the files a copy would, with exactly the same bytes"
        );
        assert_eq!(stats.files_copied, 5, "every non-skipped file is placed");
        assert!(stats.files_cloned <= stats.files_copied);
        // On macOS this is the assertion that the clone path is reached at all:
        // every temp directory this test uses is on the APFS data volume, so a
        // zero here means `clone_file` stopped working, not that the filesystem
        // declined. Left as an inequality elsewhere, where copying is the only
        // implemented path and asserting otherwise would fail honestly-written
        // code.
        #[cfg(target_os = "macos")]
        assert_eq!(
            stats.files_cloned, stats.files_copied,
            "APFS clones every file, so a copy here means the clone path was skipped"
        );
        assert_eq!(
            stats.placement_mode(),
            if stats.files_cloned == stats.files_copied {
                "copy-on-write"
            } else if stats.files_cloned == 0 {
                "full copy"
            } else {
                "copy-on-write where the filesystem allowed it"
            },
            "the ledger label must describe what actually happened"
        );

        // The saving is real only if the clone is independent: writing through
        // the sandbox copy must not reach back into the workspace. This is the
        // property a hard-linked staging tree would fail.
        fs::write(cloned.join("src/main.rs"), "fn main() {}")
            .expect("the sandbox file is writable");
        assert_eq!(
            fs::read_to_string(root.join("src/main.rs")).expect("the original reads"),
            "fn main() { println!(\"hi\"); }",
            "a write inside the sandbox must never mutate the workspace"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&cloned);
        let _ = fs::remove_dir_all(&copied);
    }

    /// An empty workspace must not read as a filesystem that refused to clone.
    #[test]
    fn placement_mode_separates_nothing_to_clone_from_nothing_cloned() {
        assert_eq!(CopyStats::default().placement_mode(), "no files");
        assert_eq!(
            CopyStats {
                files_copied: 3,
                files_cloned: 0,
                ..CopyStats::default()
            }
            .placement_mode(),
            "full copy"
        );
        assert_eq!(
            CopyStats {
                files_copied: 3,
                files_cloned: 3,
                ..CopyStats::default()
            }
            .placement_mode(),
            "copy-on-write"
        );
        assert_eq!(
            CopyStats {
                files_copied: 3,
                files_cloned: 1,
                ..CopyStats::default()
            }
            .placement_mode(),
            "copy-on-write where the filesystem allowed it"
        );
    }

    #[test]
    fn copy_excludes_git_node_modules_target_and_secrets() {
        let root = temp_dir("copy-src");
        let dest = temp_dir("copy-dest");

        write(&root.join(".git/HEAD"), "ref: refs/heads/main");
        write(&root.join("node_modules/pkg/index.js"), "module.exports = {};");
        write(&root.join("target/debug/app"), "binary");
        write(&root.join(".env"), "API_KEY=super-secret");
        write(&root.join("src/main.rs"), "fn main() {}");
        write(&root.join("package.json"), "{\"name\":\"fixture\"}");

        let stats = copy_workspace_into_sandbox(&root, &dest).expect("copy succeeds");

        assert!(!dest.join(".git").exists(), ".git must never be copied");
        assert!(!dest.join("node_modules").exists(), "node_modules must never be copied");
        assert!(!dest.join("target").exists(), "target must never be copied");
        assert!(!dest.join(".env").exists(), "secrets must never be copied");
        assert!(dest.join("src/main.rs").is_file(), "ordinary source files must be copied");
        assert!(
            dest.join("package.json").is_file(),
            "manifests must still be copied so the sandbox copy can actually build/test"
        );
        assert!(stats.files_copied >= 2);
        assert!(stats.skipped >= 1);

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&dest);
    }

    // --- allowlisted_env -------------------------------------------------

    #[test]
    fn allowlisted_env_excludes_unapproved_secrets() {
        std::env::set_var("SANDBOX_TEST_SECRET_TOKEN", "super-secret-value");
        let root = temp_dir("env-owned");
        let home = root.join("home");
        let tmp = root.join("tmp");
        let env = allowlisted_env(&home, &tmp, &[]);
        let home_text = home.to_string_lossy();
        let tmp_text = tmp.to_string_lossy();
        assert!(!env
            .iter()
            .any(|(key, _)| key == "SANDBOX_TEST_SECRET_TOKEN"));
        assert!(env.iter().any(|(key, _)| key == "PATH"));
        assert!(env
            .iter()
            .any(|(key, value)| key == "HOME" && value == home_text.as_ref()));
        assert!(env
            .iter()
            .any(|(key, value)| key == "TMPDIR" && value == tmp_text.as_ref()));
        std::env::remove_var("SANDBOX_TEST_SECRET_TOKEN");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn allowlisted_env_includes_only_extras_and_cannot_override_owned_paths() {
        std::env::set_var("SANDBOX_TEST_APPROVED", "yes");
        std::env::set_var("SANDBOX_TEST_UNAPPROVED", "no");
        let root = temp_dir("env-approved");
        let home = root.join("home");
        let tmp = root.join("tmp");
        let env = allowlisted_env(
            &home,
            &tmp,
            &[
                "SANDBOX_TEST_APPROVED".to_string(),
                "HOME".to_string(),
                "TMPDIR".to_string(),
            ],
        );
        let home_text = home.to_string_lossy();
        let tmp_text = tmp.to_string_lossy();
        assert!(env
            .iter()
            .any(|(key, value)| key == "SANDBOX_TEST_APPROVED" && value == "yes"));
        assert!(!env.iter().any(|(key, _)| key == "SANDBOX_TEST_UNAPPROVED"));
        assert!(env
            .iter()
            .any(|(key, value)| key == "HOME" && value == home_text.as_ref()));
        assert!(env
            .iter()
            .any(|(key, value)| key == "TMPDIR" && value == tmp_text.as_ref()));
        std::env::remove_var("SANDBOX_TEST_APPROVED");
        std::env::remove_var("SANDBOX_TEST_UNAPPROVED");
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn spawned_child_never_inherits_unapproved_secrets() {
        std::env::set_var("SANDBOX_TEST_CHILD_SECRET", "leak-me-not");
        let sandbox_root = temp_dir("exec-env");
        let workspace_dir = sandbox_root.join("workspace");
        fs::create_dir_all(&workspace_dir).expect("create sandbox workspace");
        let real_workspace = temp_dir("exec-env-real");
        let profile_path = sandbox_root.join("seatbelt.sb");

        let outcome = execute_in_sandbox(
            &sandbox_root,
            &workspace_dir,
            &real_workspace,
            &profile_path,
            "env",
            Duration::from_secs(10),
            false,
            &[],
        )
        .await
        .expect("command executes");

        let stdout = String::from_utf8_lossy(&outcome.stdout);
        assert!(
            !stdout.contains("SANDBOX_TEST_CHILD_SECRET"),
            "child env leaked an unapproved variable: {stdout}"
        );
        assert!(
            stdout.contains("PATH="),
            "child env should still contain PATH"
        );
        assert!(
            stdout.contains(&format!(
                "HOME={}",
                fs::canonicalize(&sandbox_root)
                    .expect("canonical sandbox")
                    .join(SANDBOX_HOME_DIR)
                    .display()
            )),
            "child HOME must be sandbox-owned: {stdout}"
        );

        std::env::remove_var("SANDBOX_TEST_CHILD_SECRET");
        let _ = fs::remove_dir_all(&sandbox_root);
        let _ = fs::remove_dir_all(&real_workspace);
    }

    // --- build_seatbelt_profile ------------------------------------------

    #[test]
    fn seatbelt_profile_has_no_global_read_and_scopes_reads_and_writes() {
        let dir = Path::new("/tmp/example-sandbox-dir");
        let profile = build_seatbelt_profile(
            dir,
            &[
                PathBuf::from("/System/Library"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/Users/example/.cargo/bin"),
            ],
            false,
        );
        assert!(profile.contains("(deny default)"));
        assert!(
            !profile
                .lines()
                .any(|line| line.trim() == "(allow file-read*)"),
            "profile must never grant unfiltered file reads:\n{profile}"
        );
        assert!(profile.contains("(subpath \"/tmp/example-sandbox-dir\")"));
        assert!(profile.contains("(subpath \"/System/Library\")"));
        assert!(profile.contains("(subpath \"/usr/bin\")"));
        assert!(
            !profile.contains("(subpath \"/Users/example\")"),
            "whole user home must not be readable"
        );
        assert!(profile.contains("(allow file-write* (subpath \"/tmp/example-sandbox-dir\"))"));
        assert_eq!(
            profile
                .lines()
                .filter(|line| line.contains("(allow file-write*"))
                .count(),
            1,
            "only the sandbox write grant is allowed"
        );
        assert!(profile.contains("(deny network*)"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn seatbelt_profile_network_toggle_does_not_change_filesystem_rules() {
        let dir = Path::new("/tmp/example-sandbox-dir");
        let roots = vec![PathBuf::from("/System/Library"), PathBuf::from("/usr/bin")];
        let denied = build_seatbelt_profile(dir, &roots, false);
        let allowed = build_seatbelt_profile(dir, &roots, true);
        assert!(allowed.contains("(allow network*)"));
        assert!(!allowed.contains("(deny network*)"));
        assert_eq!(
            denied.replace("(deny network*)", ""),
            allowed.replace("(allow network*)", ""),
            "network permission must not alter any file-read rule"
        );
    }

    /// The network denial, actually exercised.
    ///
    /// Everything else about `(deny network*)` was asserted as profile *text*: the
    /// sibling test above compares two generated strings, and the live Seatbelt
    /// test loops over `allow_network` while running a command that never opens a
    /// socket — so it proved the filesystem rules survive the toggle, not that the
    /// toggle does anything. A denied-network sandbox was a security claim with no
    /// test behind it.
    ///
    /// Asserted as a **contrast**, and that is the whole design: "the connection
    /// failed" is also what a machine with no network produces, so a one-armed
    /// test would pass vacuously in exactly the environment where it is least
    /// informative. The allow arm must succeed for the deny arm to mean anything,
    /// and if it does not, the test says so rather than reporting a pass.
    ///
    /// The target is a listener this test owns on loopback. Reaching the real
    /// internet would make a unit test depend on egress, and loopback is the
    /// stricter check anyway: a boundary that stops a connection to `127.0.0.1`
    /// is not merely failing to resolve DNS.
    ///
    /// Shared by every platform with a network boundary, so the contrast is
    /// asserted once and cannot drift into two differently-rigorous versions.
    /// `connect_command` is the only platform-specific part: what to run to make
    /// one TCP connection to a loopback port and exit non-zero if it fails.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    async fn assert_loopback_is_reachable_only_when_network_is_allowed(
        label: &str,
        connect_command: impl Fn(u16) -> String,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("listener address").port();
        let accepting = std::thread::spawn(move || {
            // Two arms, so two connection attempts at most; the deny arm never
            // arrives, and the accept loop ends with the listener's drop.
            for _ in 0..2 {
                match listener.accept() {
                    Ok(_) => {}
                    Err(_) => return,
                }
            }
        });

        let sandbox_root = temp_dir(&format!("{label}-network"));
        let workspace_dir = sandbox_root.join("workspace");
        fs::create_dir_all(&workspace_dir).expect("create sandbox workspace");
        let real_workspace = temp_dir(&format!("{label}-network-real"));
        let command = connect_command(port);

        let mut outcomes = Vec::new();
        for allow_network in [true, false] {
            let profile_path = sandbox_root.join(format!("network-{allow_network}.sb"));
            let outcome = execute_in_sandbox(
                &sandbox_root,
                &workspace_dir,
                &real_workspace,
                &profile_path,
                &command,
                Duration::from_secs(10),
                allow_network,
                &[],
            )
            .await
            .expect("the sandbox launches");
            outcomes.push((allow_network, outcome));
        }

        let (_, allowed) = &outcomes[0];
        let (_, denied) = &outcomes[1];
        assert_eq!(
            allowed.exit_code,
            Some(0),
            "a network-allowed sandbox could not reach a loopback listener, so this \
             environment cannot tell enforcement from absent networking; stderr={}",
            String::from_utf8_lossy(&allowed.stderr)
        );
        assert_ne!(
            denied.exit_code,
            Some(0),
            "denying network did not stop a connection the same sandbox makes \
             successfully when network is allowed; stderr={}",
            String::from_utf8_lossy(&denied.stderr)
        );
        assert!(!denied.timed_out, "the denied connection hung instead of failing");

        drop(accepting);
        let _ = fs::remove_dir_all(&sandbox_root);
        let _ = fs::remove_dir_all(&real_workspace);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_denies_a_real_connection_when_network_is_not_allowed() {
        if !Path::new(SANDBOX_EXEC).is_file() {
            eprintln!("skipping Seatbelt network test: sandbox-exec is unavailable");
            return;
        }
        if !Path::new("/usr/bin/nc").is_file() {
            eprintln!("skipping Seatbelt network test: /usr/bin/nc is unavailable");
            return;
        }
        // `-z` scans without sending data and `-w 2` bounds the wait, so a denied
        // connection fails fast instead of hanging until the run times out.
        assert_loopback_is_reachable_only_when_network_is_allowed("seatbelt", |port| {
            format!("/usr/bin/nc -w 2 -z 127.0.0.1 {port}")
        })
        .await;
    }

    /// The Linux arm of the contrast above, denied by the seccomp filter rather
    /// than by `(deny network*)`.
    ///
    /// Bash's `/dev/tcp` rather than `nc`, because `nc` is not installed by
    /// default on every distribution while `bash` effectively is, and because it
    /// removes a variable: the redirection is a plain `socket(2)` + `connect(2)`
    /// in the shell itself, which is exactly the pair the filter denies. There is
    /// no timeout flag to pass because there is nothing to wait for — the allow
    /// arm connects to a listening loopback socket immediately, and the deny arm
    /// fails at `socket(2)` with `EACCES` before any connect is attempted.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn seccomp_denies_a_real_connection_when_network_is_not_allowed() {
        if !Path::new("/bin/bash").is_file() {
            skip_locally_but_fail_in_ci(
                "bash",
                "/bin/bash is unavailable, and its `/dev/tcp` is what opens the \
                 socket this filter has to deny",
            );
            return;
        }
        assert_loopback_is_reachable_only_when_network_is_allowed("seccomp", |port| {
            format!("exec /bin/bash -c 'exec 3<>/dev/tcp/127.0.0.1/{port}'")
        })
        .await;
    }

    #[test]
    fn the_enforcement_probe_answers_for_this_platform_and_never_guesses_from_the_target() {
        let enforcement = sandbox_enforcement();

        #[cfg(target_os = "macos")]
        {
            // The distinction this exists for: `OsEnforced` is a claim about the
            // machine, not about the target triple, so it must track whether the
            // binary is actually there.
            let expected = if Path::new(SANDBOX_EXEC).is_file() {
                SandboxEnforcement::OsEnforced
            } else {
                SandboxEnforcement::Unavailable
            };
            assert_eq!(enforcement, expected);
        }
        #[cfg(target_os = "linux")]
        {
            // Same distinction, different mechanism: the answer tracks what this
            // kernel can enforce, and never `Unavailable` — a kernel without
            // Landlock degrades to the restricted-cwd/env isolation instead of
            // failing the run, so there is no "mechanism exists but is unusable"
            // state to report.
            let expected = if crate::sandbox_linux::landlock_is_enforceable() {
                SandboxEnforcement::OsEnforced
            } else {
                SandboxEnforcement::ProcessOnly
            };
            assert_eq!(enforcement, expected);
            assert_ne!(enforcement, SandboxEnforcement::Unavailable);
        }
        #[cfg(target_os = "windows")]
        {
            // Same shape again: tracks what this machine can hold rather than the
            // target triple, and never `Unavailable`, because a machine that
            // cannot create a container or a job degrades instead of failing.
            let expected = if crate::sandbox_windows::app_containers_are_enforceable() {
                SandboxEnforcement::OsEnforced
            } else if crate::sandbox_windows::job_objects_are_enforceable() {
                SandboxEnforcement::ProcessContained
            } else {
                SandboxEnforcement::ProcessOnly
            };
            assert_eq!(enforcement, expected);
            assert_ne!(enforcement, SandboxEnforcement::Unavailable);
            // `OsEnforced` here is a claim about a filesystem boundary, so it may
            // only be reported when a container is actually creatable.
            if enforcement == SandboxEnforcement::OsEnforced {
                assert!(crate::sandbox_windows::app_containers_are_enforceable());
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            // Not `Unavailable`: that would imply a mechanism this app has and
            // could not use, and this platform has none.
            assert_eq!(enforcement, SandboxEnforcement::ProcessOnly);
        }
    }

    #[test]
    fn read_roots_reject_whole_home_and_real_workspace() {
        let fixture = temp_dir("read-roots");
        let home = fixture.join("user-home");
        let workspace = home.join("Documents/project");
        let workspace_bin = workspace.join("bin");
        let cargo_bin = home.join(".cargo/bin");
        let external_bin = fixture.join("external-tool/bin");
        for path in [&workspace_bin, &cargo_bin, &external_bin] {
            fs::create_dir_all(path).expect("create PATH fixture");
        }
        let joined = std::env::join_paths([
            home.as_path(),
            workspace_bin.as_path(),
            cargo_bin.as_path(),
            external_bin.as_path(),
        ])
        .expect("join PATH");

        let roots = readable_roots(
            MACOS_SYSTEM_READ_ROOTS,
            Some(&joined),
            Some(&home),
            &workspace,
        );
        let canonical_home = fs::canonicalize(&home).expect("canonical home");
        let canonical_workspace = fs::canonicalize(&workspace).expect("canonical workspace");
        let canonical_cargo = fs::canonicalize(&cargo_bin).expect("canonical cargo bin");
        let canonical_external = fs::canonicalize(&external_bin).expect("canonical external bin");

        assert!(!roots.iter().any(|root| root == &canonical_home));
        assert!(
            !roots
                .iter()
                .any(|root| paths_overlap(root, &canonical_workspace)),
            "no real-workspace root or descendant may be readable: {roots:?}"
        );
        assert!(roots.contains(&canonical_cargo));
        assert!(roots.contains(&canonical_external));
        let _ = fs::remove_dir_all(&fixture);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn sh_quote(path: &Path) -> String {
        format!("'{}'", path.to_string_lossy().replace('\'', "'\"'\"'"))
    }

    /// The workspace boundary, exercised for real: one command that reads its own
    /// copy, fails to read the real workspace, fails to overwrite it, and writes
    /// to the sandbox-owned HOME and TMP — run once with network and once without,
    /// because a boundary that only holds in one of those states is not a
    /// boundary.
    ///
    /// Shared verbatim by macOS and Linux, which is the point: the two platforms
    /// enforce it with completely different kernel machinery (a Seatbelt profile
    /// versus a Landlock ruleset) and the assertion is that this is not
    /// observable from inside. Only the `label` differs.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    async fn assert_real_workspace_stays_out_of_reach(label: &str) {
        let sandbox_root = temp_dir(&format!("{label}-integration"));
        let workspace_dir = sandbox_root.join("workspace");
        fs::create_dir_all(&workspace_dir).expect("create sandbox workspace");
        let real_workspace = temp_dir(&format!("{label}-real-workspace"));
        let allowed_file = workspace_dir.join("allowed.txt");
        let forbidden_file = real_workspace.join("secret.txt");
        write(&allowed_file, "sandbox-visible");
        write(&forbidden_file, "must-stay-secret");

        let canonical_sandbox = fs::canonicalize(&sandbox_root).expect("canonical sandbox");
        let canonical_workspace =
            fs::canonicalize(&workspace_dir).expect("canonical sandbox workspace");
        let canonical_forbidden =
            fs::canonicalize(&forbidden_file).expect("canonical forbidden file");
        let expected_home = canonical_sandbox.join(SANDBOX_HOME_DIR);
        let expected_tmp = canonical_sandbox.join(SANDBOX_TMP_DIR);
        let command = format!(
            "set -eu\n\
             test \"$HOME\" = {}\n\
             test \"$TMPDIR\" = {}\n\
             /bin/cat {} > \"$TMPDIR/allowed-copy\"\n\
             if /bin/cat {} > \"$TMPDIR/forbidden-copy\"; then exit 71; fi\n\
             if printf overwritten > {}; then exit 72; fi\n\
             printf home-ok > \"$HOME/probe\"\n\
             printf tmp-ok > \"$TMPDIR/probe\"",
            sh_quote(&expected_home),
            sh_quote(&expected_tmp),
            sh_quote(&canonical_workspace.join("allowed.txt")),
            sh_quote(&canonical_forbidden),
            sh_quote(&canonical_forbidden),
        );

        for allow_network in [false, true] {
            let profile_path = sandbox_root.join(if allow_network {
                "boundary-network.sb"
            } else {
                "boundary-offline.sb"
            });
            let outcome = execute_in_sandbox(
                &sandbox_root,
                &workspace_dir,
                &real_workspace,
                &profile_path,
                &command,
                Duration::from_secs(10),
                allow_network,
                &[],
            )
            .await
            .expect("the sandbox launches");
            assert_eq!(
                outcome.exit_code,
                Some(0),
                "{label} boundary failed (network={allow_network}); stderr={}",
                String::from_utf8_lossy(&outcome.stderr)
            );
            assert_eq!(
                fs::read_to_string(&forbidden_file).expect("read real file after run"),
                "must-stay-secret"
            );
        }
        assert!(expected_home.join("probe").is_file());
        assert!(expected_tmp.join("probe").is_file());

        let _ = fs::remove_dir_all(&sandbox_root);
        let _ = fs::remove_dir_all(&real_workspace);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn sandbox_exec_cannot_read_or_write_real_workspace_with_or_without_network() {
        if !Path::new(SANDBOX_EXEC).is_file() {
            skip_locally_but_fail_in_ci(
                "Seatbelt",
                "sandbox-exec is unavailable, and it is the only thing enforcing \
                 confinement on this platform",
            );
            return;
        }
        assert_real_workspace_stays_out_of_reach("seatbelt").await;
    }

    /// The Linux arm of the same test.
    ///
    /// Skips rather than fails when the kernel cannot enforce Landlock — a
    /// developer on a kernel built without it, or a container whose own policy
    /// blocks the syscall, is not a regression in this code. On CI it fails
    /// instead, via `skip_locally_but_fail_in_ci`: green has to mean the
    /// assertions ran, and a captured `println!` cannot carry that difference.
    /// Skip locally, fail in CI — because a skip and a pass are the same colour.
    ///
    /// An earlier version of the two tests below claimed that printing the reason
    /// was enough to keep "green because it asserted" distinct from "green because
    /// it asserted nothing". That was wrong: `cargo test` captures a *passing*
    /// test's stdout and stderr, so the print never reaches the log that anyone
    /// reads. The distinction matters more here than anywhere else in this file,
    /// since the thing that would quietly stop being tested is a security
    /// boundary — so on CI the absence of the mechanism is a failure. A runner
    /// image that stops shipping Landlock must turn this red rather than silently
    /// stop enforcing it.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn skip_locally_but_fail_in_ci(mechanism: &str, reason: &str) {
        assert!(
            std::env::var_os("CI").is_none(),
            "{mechanism} is unavailable on this CI runner, so the isolation it \
             enforces went untested: {reason}"
        );
        eprintln!("skipping {mechanism} integration test: {reason}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn landlock_cannot_read_or_write_real_workspace_with_or_without_network() {
        if !crate::sandbox_linux::landlock_is_enforceable() {
            skip_locally_but_fail_in_ci(
                "Landlock",
                "this kernel cannot enforce the Landlock ABI v1 baseline (not built \
                 in, disabled at boot, or the syscall is blocked by an outer sandbox)",
            );
            return;
        }
        assert_real_workspace_stays_out_of_reach("landlock").await;
    }

    /// The network filter has to compile on the machine that will install it:
    /// [`crate::sandbox_linux::network_denial_filter`] fails on an architecture
    /// seccompiler has no audit value for, and that failure turns into a failed
    /// run rather than a network-allowed one. Cheap to assert, and it is the only
    /// part of the filter that can be checked without spawning anything.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_network_denial_filter_compiles_for_this_architecture() {
        let program =
            crate::sandbox_linux::network_denial_filter().expect("filter compiles for this arch");
        assert!(
            !program.is_empty(),
            "an empty BPF program is rejected by `apply_filter`, so it would fail the spawn"
        );
    }

    // --- promote digest / confirmation -----------------------------------

    #[test]
    fn promote_digest_changes_with_content_and_is_order_independent() {
        let a = vec![
            PromoteFileEntry { path: "a.txt".into(), sha256: "1".repeat(64), size_bytes: 1 },
            PromoteFileEntry { path: "b.txt".into(), sha256: "2".repeat(64), size_bytes: 2 },
        ];
        let shuffled = vec![a[1].clone(), a[0].clone()];
        assert_eq!(compute_promote_digest("run-1", &a), compute_promote_digest("run-1", &shuffled));

        let mut changed = a.clone();
        changed[0].sha256 = "3".repeat(64);
        assert_ne!(compute_promote_digest("run-1", &a), compute_promote_digest("run-1", &changed));

        assert_ne!(compute_promote_digest("run-1", &a), compute_promote_digest("run-2", &a));
    }

    #[test]
    fn build_promote_preview_rejects_path_traversal_and_missing_files() {
        let dir = temp_dir("preview-src");
        write(&dir.join("kept.txt"), "hello");

        let traversal = build_promote_preview("run-1", &dir, &["../escape.txt".to_string()], 0, 1_000);
        assert!(traversal.is_err());

        let missing = build_promote_preview("run-1", &dir, &["missing.txt".to_string()], 0, 1_000);
        assert!(missing.is_err());

        let ok = build_promote_preview("run-1", &dir, &["kept.txt".to_string()], 0, 1_000)
            .expect("valid file promotes");
        assert_eq!(ok.confirmation_phrase, confirmation_phrase_for(&ok.digest));

        let _ = fs::remove_dir_all(&dir);
    }

    // --- promote refuses without a valid digest+phrase, never touches disk

    #[test]
    fn validate_promote_confirmation_rejects_wrong_phrase_without_touching_anything() {
        let pending = PendingPromote {
            run_id: "run-1".to_string(),
            files: vec![PromoteFileEntry { path: "a.txt".into(), sha256: "a".repeat(64), size_bytes: 1 }],
            expires_at_ms: now_ms() + 60_000,
        };
        let digest = compute_promote_digest("run-1", &pending.files);

        let wrong_phrase = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &digest,
            "CONFIRM wrong-phrase",
            now_ms(),
        );
        assert!(wrong_phrase.is_err());

        let wrong_run = validate_promote_confirmation(
            Some(&pending),
            "some-other-run",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(wrong_run.is_err());

        let expired = validate_promote_confirmation(
            Some(&PendingPromote { expires_at_ms: 0, ..pending.clone() }),
            "run-1",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(expired.is_err());

        let missing = validate_promote_confirmation(None, "run-1", &digest, &confirmation_phrase_for(&digest), now_ms());
        assert!(missing.is_err());

        let ok = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &digest,
            &confirmation_phrase_for(&digest),
            now_ms(),
        );
        assert!(ok.is_ok());
    }

    #[test]
    fn promote_files_end_to_end_never_writes_on_prior_validation_failure() {
        let sandbox_dir = temp_dir("promote-sandbox");
        let real_root = temp_dir("promote-real");
        write(&sandbox_dir.join("app.txt"), "sandbox content");
        write(&real_root.join("app.txt"), "original content");

        let preview = build_promote_preview(
            "run-1",
            &sandbox_dir,
            &["app.txt".to_string()],
            now_ms(),
            60_000,
        )
        .expect("preview builds");

        // Wrong phrase: the caller-level flow must never call `promote_files`
        // at all — simulate that gate here and confirm the real file is
        // still untouched.
        let pending = PendingPromote {
            run_id: "run-1".to_string(),
            files: preview.files.clone(),
            expires_at_ms: preview.expires_at_ms,
        };
        let rejected = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &preview.digest,
            "CONFIRM 000000000000",
            now_ms(),
        );
        assert!(rejected.is_err());
        assert_eq!(
            fs::read_to_string(real_root.join("app.txt")).unwrap(),
            "original content",
            "the real file must be untouched after a rejected confirmation"
        );

        // Correct phrase: promote actually copies the sandbox content over.
        let accepted = validate_promote_confirmation(
            Some(&pending),
            "run-1",
            &preview.digest,
            &preview.confirmation_phrase,
            now_ms(),
        )
        .expect("valid confirmation is accepted");
        verify_unchanged_since_preview("run-1", &sandbox_dir, &accepted, &preview.digest)
            .expect("nothing changed since prepare");
        let promoted =
            promote_files(&sandbox_dir, &real_root, &accepted.files).expect("promote succeeds");
        assert_eq!(promoted, vec!["app.txt".to_string()]);
        assert_eq!(
            fs::read_to_string(real_root.join("app.txt")).unwrap(),
            "sandbox content"
        );

        let _ = fs::remove_dir_all(&sandbox_dir);
        let _ = fs::remove_dir_all(&real_root);
    }

    // --- diff --------------------------------------------------------------

    #[test]
    fn diff_reports_added_and_modified_but_not_unchanged() {
        let sandbox_dir = temp_dir("diff-sandbox");
        let real_root = temp_dir("diff-real");
        write(&sandbox_dir.join("added.txt"), "new");
        write(&sandbox_dir.join("modified.txt"), "changed");
        write(&real_root.join("modified.txt"), "original");
        write(&sandbox_dir.join("unchanged.txt"), "same");
        write(&real_root.join("unchanged.txt"), "same");

        let diff = diff_sandbox_against_workspace(&sandbox_dir, &real_root).expect("diff succeeds");
        let paths: Vec<&str> = diff.iter().map(|entry| entry.path.as_str()).collect();
        assert!(paths.contains(&"added.txt"));
        assert!(paths.contains(&"modified.txt"));
        assert!(!paths.contains(&"unchanged.txt"));

        let _ = fs::remove_dir_all(&sandbox_dir);
        let _ = fs::remove_dir_all(&real_root);
    }
}

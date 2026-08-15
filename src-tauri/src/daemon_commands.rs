//! Fixed-argument desktop bridge for the bundled resident daemon.
//!
//! This module deliberately does not expose an arbitrary CLI executor. Every
//! command has a typed argument set, invokes the app-owned sidecar without a
//! shell, bounds output, and leaves the daemon as the single authoritative
//! engine/ledger owner.

use rusqlite::{params, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use tauri::Emitter;

const MAX_CLI_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REMOTE_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REMOTE_RUN_SCOPES: usize = 1_024;
const MAX_REMOTE_WORKSPACE_SCOPES: usize = 128;
const MAX_RECIPE_SCHEDULES: usize = 1_024;
const MANAGED_RECIPE_TRIGGER_PREFIX: &str = "lm-managed-recipe-v1-";
const DAEMON_CHANGED_EVENT: &str = "daemon://changed";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct DaemonDesktopStatus {
    pub installed: bool,
    pub service_running: bool,
    pub heartbeat_fresh: bool,
    pub pid: Option<u32>,
    pub kill_switch: bool,
    pub queued: u32,
    pub active: u32,
    pub waiting_approval: u32,
    pub paused: u32,
    #[serde(default)]
    pub managed_run_ids: Vec<String>,
    pub platform: Value,
    /// The K8 scheduler backpressure signal, carried through verbatim.
    ///
    /// Not `Option`, deliberately. A missing signal must break loudly here
    /// rather than decode as "accepting": the bundled sidecar ships in lockstep
    /// with this binary (see [`cli_path`]), so an absent field means the two
    /// disagree about the payload, and defaulting a *safety* signal to its
    /// permissive value is how a producer ends up ignoring backpressure without
    /// anybody noticing. Every other queue counter above is required for the
    /// same reason.
    pub backpressure: DesktopBackpressure,
}

/// What the daemon is currently willing to accept.
///
/// Mirrors `monkey-cli`'s `daemon::scheduler::BackpressureState`. It cannot be
/// reused from there: `monkey-cli` is a binary that depends on this library, so
/// nothing under `src/bin/` is reachable from here.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
// Both directions, not just deserialize: every token is a single lowercase word,
// so snake_case and camelCase render identically and one attribute is honest for
// both. Do not "fix" this to a split serialize/deserialize pair — it would read
// as though the two sides differ when they do not.
#[serde(rename_all = "snake_case")]
pub enum DesktopBackpressureState {
    Accepting,
    /// Work is still accepted. Advisory: only a producer knows if it can wait.
    Slow,
    /// Refused. `enqueue` fails daemon-side, so a producer that submits anyway
    /// gets an error rather than an overfull queue.
    Closed,
}

/// The backpressure signal as `monkey daemon status --json` emits it.
///
/// The CLI emits **snake_case** inside this object (`retry_after_ms`,
/// `queue_depth`, `queue_capacity`), so this needs its parent's split
/// `rename_all` and not the plain `rename_all = "camelCase"` that the request
/// structs below use. serde container attributes do not inherit, and a plain
/// camelCase spelling here would look right while failing to decode three of
/// the eight fields — which is why the test asserts against a real CLI payload
/// rather than a round trip of this struct's own output.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
pub struct DesktopBackpressure {
    pub state: DesktopBackpressureState,
    /// Convenience mirror of `state != Closed`, carried through as sent.
    pub accepting: bool,
    /// Stable machine token (`kill_switch`, `queue_full`, `memory_saturated`,
    /// `queue_deep`), or `None` when accepting freely. A `String` rather than an
    /// enum because nothing on this side of the bridge branches on it — the
    /// desktop branches on `state` and displays `detail` — so mirroring the four
    /// tokens here would only add a second place for them to drift from
    /// `daemon::scheduler`, which owns them.
    pub reason: Option<String>,
    /// A human sentence. Display it; never branch on it.
    pub detail: Option<String>,
    /// Advisory delay, derived from poll interval × backlog. Not a prediction of
    /// when any particular job finishes.
    pub retry_after_ms: Option<u64>,
    pub queue_depth: u32,
    pub queue_capacity: u32,
    pub queued: u32,
    pub held: u32,
}

/// The K8 backpressure signal out of a raw `daemon status --json` value, or
/// `None` when the field is absent or does not decode.
///
/// `None` means **accepting**, and every caller must read it that way: a signal
/// that goes away must never block the app, and `closed` is enforced daemon-side
/// by `enqueue` regardless of what any producer checks. Branch on
/// [`DesktopBackpressure::state`] and `reason`; never on `detail`, which is prose
/// for a human.
///
/// Producers that already poll `daemon status` for other reasons — see
/// [`crate::m6a_desktop_bridge`] and `m5_delivery::reviewer` — use this instead
/// of re-spelling the field names, which is where the casing bugs come from.
pub fn backpressure_signal(status: &Value) -> Option<DesktopBackpressure> {
    serde_json::from_value(status.get("backpressure")?.clone()).ok()
}

/// One arbitration decision as `monkey daemon decisions --json` prints it.
///
/// Mirrors `SchedulerDecision` in the CLI's `daemon/store.rs`, which carries a
/// plain `rename_all = "camelCase"` — unlike [`DesktopBackpressure`] above, whose
/// CLI-side spelling is snake_case. So this one is camelCase in *both* directions
/// and the test below proves it against verbatim CLI bytes; the SQLite
/// `decision_id` column is deliberately not in the CLI's SELECT, so there is no
/// field for it here.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DesktopSchedulerDecision {
    pub decided_at_ms: u64,
    pub job_id: String,
    pub outcome: String,
    pub process_class: String,
    pub effective_class: String,
    pub workspace: Option<String>,
    pub passed_over: Vec<String>,
    pub detail: String,
    pub measurement: String,
    pub measured_value: Option<u64>,
    pub measured_at_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonInstallRequest {
    pub concurrency: u32,
    pub max_queue: u32,
    pub retention_days: u32,
    pub webhook_port: Option<u16>,
    pub notifications: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonRecipeSchedule {
    pub entry_id: String,
    pub recipe_name: String,
    pub recipe_path: Option<String>,
    pub cron: String,
    pub enabled: bool,
    pub permission_mode_override: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonRecipeScheduleSyncRequest {
    pub schedules: Vec<DaemonRecipeSchedule>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRecipeScheduleIssue {
    pub entry_id: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonRecipeScheduleSyncResult {
    pub authority: String,
    pub installed: bool,
    pub service_running: bool,
    pub synchronized_at_ms: u64,
    pub active_trigger_ids: Vec<String>,
    pub disabled_trigger_ids: Vec<String>,
    pub issues: Vec<DaemonRecipeScheduleIssue>,
    pub last_delivery_at_ms: BTreeMap<String, u64>,
}

#[derive(Clone, Debug)]
struct VisibleRecipe {
    canonical_path: String,
    permission_mode: String,
    required_params: Vec<String>,
}

#[derive(Clone, Debug)]
struct ManagedRecipeTriggerReplacement {
    trigger_id: String,
    config_json: Vec<u8>,
    next_fire_at_ms: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonQueueRequest {
    pub recipe: String,
    pub run_key: Option<String>,
    pub priority: i32,
    pub max_attempts: u32,
    pub max_runtime_seconds: u64,
    pub max_memory_mb: Option<u64>,
    pub owned_worktree: bool,
    pub repository: Option<String>,
    pub branch_prefix: String,
    pub allowed_remotes: Vec<String>,
    pub allow_commit: bool,
    pub allow_push: bool,
    pub allow_create_pull_request: bool,
    pub allow_review_comment: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteHostConfigureRequest {
    pub listen: String,
    pub advertise_url: String,
    pub tls_certificate: String,
    pub tls_private_key: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemotePairRequest {
    pub output: String,
    pub expires_minutes: u64,
    pub actions: Vec<String>,
    pub run_ids: Vec<String>,
    pub workspace_ids: Vec<String>,
    pub max_artifact_bytes: u64,
    /// First-party mobile-companion grants (chat, workflow launch, capture).
    /// Empty for a runner-only controller — see the CLI's `--mobile` flag.
    #[serde(default)]
    pub mobile_capabilities: Vec<String>,
    /// Grants over the device's own hardware (camera, microphone, location, …).
    /// Empty means this runner can ask the device for nothing physical,
    /// whatever the device advertises — see the CLI's `--device` flag.
    #[serde(default)]
    pub device_capabilities: Vec<String>,
}

fn cli_path() -> PathBuf {
    crate::cli_install::bundled_cli_path().unwrap_or_else(|| {
        PathBuf::from(if cfg!(windows) {
            "monkey-cli.exe"
        } else {
            "monkey-cli"
        })
    })
}

fn run_cli(args: Vec<String>) -> Result<String, String> {
    let output = Command::new(cli_path())
        .args(&args)
        .output()
        .map_err(|error| format!("Failed to start bundled monkey-cli: {error}"))?;
    if output.stdout.len() > MAX_CLI_OUTPUT_BYTES || output.stderr.len() > MAX_CLI_OUTPUT_BYTES {
        return Err("Daemon command output exceeded 4 MiB".to_string());
    }
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if detail.is_empty() {
            format!("Daemon command exited with {}", output.status)
        } else {
            detail
        });
    }
    String::from_utf8(output.stdout).map_err(|_| "Daemon output is not valid UTF-8".to_string())
}

pub(crate) async fn command(args: Vec<String>) -> Result<String, String> {
    tokio::task::spawn_blocking(move || run_cli(args))
        .await
        .map_err(|error| error.to_string())?
}

fn parse_json(output: &str) -> Result<Value, String> {
    serde_json::from_str(output.trim())
        .map_err(|error| format!("Invalid daemon JSON output: {error}"))
}

fn validate_token(label: &str, value: &str, max: usize) -> Result<(), String> {
    if value.is_empty()
        || value.len() > max
        || value.chars().any(char::is_control)
        || value.contains('\0')
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!("Invalid {label}"))
    } else {
        Ok(())
    }
}

fn validate_remote_pair_request(request: &RemotePairRequest) -> Result<(), String> {
    if !(1..=1440).contains(&request.expires_minutes) {
        return Err("Pairing expiry must be 1..=1440 minutes".to_string());
    }
    let allowed = [
        "view-runs",
        "view-events",
        "read-artifacts",
        "approve",
        "cancel",
        "kill",
        "control-desktop",
    ];
    if request.actions.is_empty()
        || request
            .actions
            .iter()
            .any(|action| !allowed.contains(&action.as_str()))
    {
        return Err("Pairing requires valid explicit actions".to_string());
    }
    let allowed_mobile = [
        "view-sessions",
        "chat",
        "view-tasks",
        "run-workflows",
        "capture",
    ];
    if request
        .mobile_capabilities
        .iter()
        .any(|capability| !allowed_mobile.contains(&capability.as_str()))
    {
        return Err("Unknown mobile companion capability".to_string());
    }
    // Must stay in step with `protocol::PHYSICAL_DEVICE_CAPABILITIES`. The CLI
    // re-checks it against the enum itself, so an unknown value fails there
    // too; this refuses it before a sidecar process is spawned.
    let allowed_device = [
        "device_info",
        "camera_capture",
        "microphone_capture",
        "location_read",
        "notification_post",
        "screen_capture",
        "audio_playback",
        "voice_stream",
    ];
    if request
        .device_capabilities
        .iter()
        .any(|capability| !allowed_device.contains(&capability.as_str()))
    {
        return Err("Unknown device hardware capability".to_string());
    }
    if request.run_ids.is_empty() && request.workspace_ids.is_empty() {
        return Err("Pairing requires an exact run id or declared workspace id".to_string());
    }
    if request.run_ids.len() > MAX_REMOTE_RUN_SCOPES
        || request.workspace_ids.len() > MAX_REMOTE_WORKSPACE_SCOPES
    {
        return Err("Pairing scope is too large".to_string());
    }
    for run in &request.run_ids {
        validate_id("run id", run)?;
    }
    for workspace in &request.workspace_ids {
        validate_id("workspace id", workspace)?;
    }
    if request.max_artifact_bytes == 0 || request.max_artifact_bytes > MAX_REMOTE_ARTIFACT_BYTES {
        return Err(format!(
            "Pairing artifact budget must be 1..={MAX_REMOTE_ARTIFACT_BYTES} bytes"
        ));
    }
    let has_view_runs = request.actions.iter().any(|action| action == "view-runs");
    if !has_view_runs
        && request
            .actions
            .iter()
            .any(|action| matches!(action.as_str(), "approve" | "read-artifacts"))
    {
        return Err("Approve and artifact scopes also require view-runs".to_string());
    }
    Ok(())
}

fn validate_existing_private_input(label: &str, value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!("{label} must be a real regular file"));
    }
    Ok(())
}

fn validate_output_path(value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err("Output path must be absolute".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "Output path has no parent".to_string())?;
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| format!("Cannot inspect output directory: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("Output directory must be a real directory".to_string());
    }
    Ok(())
}

/// The desktop producer's view of the queue, including K8 backpressure.
///
/// # How the desktop honours the signal
///
/// This poll is the whole mechanism, because the desktop is the only producer
/// with somewhere to *show* the signal before work is committed:
///
/// - **`closed`** is refused by the daemon's own `enqueue`, before it creates a
///   worktree or a snapshot, and the refusal names the reason and a retry delay.
///   [`daemon_desktop_queue`] deliberately does not preflight that here — see its
///   doc comment.
/// - **`slow`** is advisory and reaches the desktop only through this payload. It
///   is the reason the field exists: nothing else can act on it, since the daemon
///   by definition still accepts the work. The UI's job is to surface `detail`
///   and `retry_after_ms` and let the operator decide, which is the correct
///   answer for a producer whose submissions are batch work a human queues.
///
/// Until [`DesktopBackpressure`] was added, this deserializer dropped the whole
/// object silently, so `slow` was unobservable from the desktop and the panel
/// could not distinguish "queue nearly full" from "queue idle".
#[tauri::command]
pub async fn daemon_desktop_status() -> Result<DaemonDesktopStatus, String> {
    let output = command(vec!["daemon".into(), "status".into(), "--json".into()]).await?;
    serde_json::from_str(output.trim()).map_err(|error| error.to_string())
}

/// The scheduling decision log, newest first.
///
/// Its own command and not more fields on [`daemon_desktop_status`] for the same
/// reason it is its own CLI subcommand: status is polled several times a second
/// and a decision log is read when somebody is asking why.
#[tauri::command]
pub async fn daemon_desktop_decisions(limit: u32) -> Result<Vec<DesktopSchedulerDecision>, String> {
    // The CLI clamps the limit to 1..=512 itself, so there is nothing to bound here.
    let output = command(vec![
        "daemon".into(),
        "decisions".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
    ])
    .await?;
    serde_json::from_str(output.trim()).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn daemon_desktop_install(
    request: DaemonInstallRequest,
    app: tauri::AppHandle,
) -> Result<String, String> {
    if !(1..=32).contains(&request.concurrency)
        || !(1..=10_000).contains(&request.max_queue)
        || !(1..=3650).contains(&request.retention_days)
    {
        return Err("Daemon installation limits are outside supported bounds".to_string());
    }
    let mut args = vec![
        "daemon".into(),
        "install".into(),
        "--concurrency".into(),
        request.concurrency.to_string(),
        "--max-queue".into(),
        request.max_queue.to_string(),
        "--retention-days".into(),
        request.retention_days.to_string(),
        "--notifications".into(),
        request.notifications.to_string(),
    ];
    if let Some(port) = request.webhook_port {
        if port == 0 {
            return Err("Webhook port cannot be zero".to_string());
        }
        args.extend(["--webhook-port".into(), port.to_string()]);
    }
    let output = command(args).await?;
    let _ = app.emit(DAEMON_CHANGED_EVENT, "installed");
    Ok(output)
}

/// Bring the resident execution service to a usable state.
///
/// Every desktop chat turn executes on that service (see
/// [`crate::m6a_desktop_bridge`]), which makes it runtime infrastructure rather
/// than a feature: the app installs it, keeps it on the shipped build and
/// starts it, and the user is never asked to go and do that themselves. This is
/// called once at launch by [`ensure_resident_service_at_startup`] and again
/// behind the Repair action chat offers when a turn cannot be routed.
///
/// The whole decision — install, republish, start, or nothing — belongs to
/// `monkey daemon ensure`, which already owns the installed config, the
/// published manifest and the running build. This is the fixed-argument bridge
/// to it and nothing more.
#[tauri::command]
pub async fn daemon_desktop_ensure(app: tauri::AppHandle) -> Result<Value, String> {
    let output = command(vec!["daemon".into(), "ensure".into(), "--json".into()]).await?;
    let value: Value = serde_json::from_str(output.trim()).map_err(|error| error.to_string())?;
    let _ = app.emit(DAEMON_CHANGED_EVENT, "ensured");
    Ok(value)
}

/// Fire-and-forget [`daemon_desktop_ensure`] for the setup hook.
///
/// Failure is not fatal to launching and is not reported here: the person is
/// told where it matters, by the chat surface that could not send, with a
/// Repair action next to the sentence. This print is for a terminal-attached
/// launch, matching every other best-effort startup step.
///
/// Release-only, with its one caller — see the comment there for why a dev
/// build must not claim the machine's service definition.
#[cfg(not(debug_assertions))]
pub fn ensure_resident_service_at_startup(app: tauri::AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(error) = daemon_desktop_ensure(app).await {
            eprintln!("Resident execution service could not be started: {error}");
        }
    });
}

#[tauri::command]
pub async fn daemon_desktop_start(app: tauri::AppHandle) -> Result<String, String> {
    let output = command(vec!["daemon".into(), "start".into()]).await?;
    let _ = app.emit(DAEMON_CHANGED_EVENT, "started");
    Ok(output)
}

#[tauri::command]
pub async fn daemon_desktop_stop(app: tauri::AppHandle) -> Result<String, String> {
    let output = command(vec!["daemon".into(), "stop".into()]).await?;
    let _ = app.emit(DAEMON_CHANGED_EVENT, "stopped");
    Ok(output)
}

#[tauri::command]
pub async fn daemon_desktop_uninstall(
    purge_state: bool,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let mut args = vec!["daemon".into(), "uninstall".into()];
    if purge_state {
        args.push("--purge-state".into());
    }
    let output = command(args).await?;
    let _ = app.emit(DAEMON_CHANGED_EVENT, "uninstalled");
    Ok(output)
}

/// Queues one recipe run as the desktop producer.
///
/// # Why there is no backpressure preflight here
///
/// `closed` is already honoured, authoritatively, one layer down: the daemon's
/// `enqueue` consults the same signal and returns its `detail` plus a retry delay
/// as an error *before* creating a worktree or snapshot, and [`run_cli`] surfaces
/// that stderr verbatim as this command's `Err` — which is already the desktop's
/// error vocabulary, since every command in this module returns
/// `Result<_, String>` for the UI to display.
///
/// Adding a `daemon status` poll before the `daemon run` below would therefore buy
/// no new refusal, spawn a second subprocess per queue, and introduce a race the
/// authoritative check does not have: the queue can fill between the two calls, so
/// the preflight would be advisory anyway while reading as though it were the
/// guard. `slow` is not actionable at this point either — the operator has already
/// committed to queueing, and the daemon still accepts the work. It is surfaced
/// ahead of that decision by [`daemon_desktop_status`] instead.
#[tauri::command]
pub async fn daemon_desktop_queue(request: DaemonQueueRequest) -> Result<Value, String> {
    validate_token("recipe", &request.recipe, 16 * 1024)?;
    if request.max_attempts == 0 || request.max_attempts > 100 || request.max_runtime_seconds == 0 {
        return Err("Daemon retry/runtime limits are outside supported bounds".to_string());
    }
    if request.branch_prefix.is_empty()
        || !request.branch_prefix.ends_with('/')
        || request.branch_prefix.contains("..")
    {
        return Err("Owned branch prefix must be safe and end in '/'".to_string());
    }
    let mut args = vec![
        "daemon".into(),
        "run".into(),
        request.recipe,
        "--priority".into(),
        request.priority.to_string(),
        "--max-attempts".into(),
        request.max_attempts.to_string(),
        "--max-runtime-seconds".into(),
        request.max_runtime_seconds.to_string(),
        "--branch-prefix".into(),
        request.branch_prefix,
        "--allow-commit".into(),
        request.allow_commit.to_string(),
        "--json".into(),
    ];
    if let Some(value) = request.run_key {
        validate_token("run key", &value, 1024)?;
        args.extend(["--run-key".into(), value]);
    }
    if let Some(value) = request.max_memory_mb {
        args.extend(["--max-memory-mb".into(), value.to_string()]);
    }
    if request.owned_worktree {
        args.push("--owned-worktree".into());
    }
    if let Some(value) = request.repository {
        validate_existing_directory("repository", &value)?;
        args.extend(["--repository".into(), value]);
    }
    for remote in request.allowed_remotes {
        validate_id("remote", &remote)?;
        args.extend(["--remote".into(), remote]);
    }
    if request.allow_push {
        args.push("--allow-push".into());
    }
    if request.allow_create_pull_request {
        args.push("--allow-create-pull-request".into());
    }
    if request.allow_review_comment {
        args.push("--allow-review-comment".into());
    }
    parse_json(&command(args).await?)
}

fn validate_existing_directory(label: &str, value: &str) -> Result<(), String> {
    let path = Path::new(value);
    if !path.is_absolute() {
        return Err(format!("{label} path must be absolute"));
    }
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("Cannot inspect {label}: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(format!("{label} must be a real directory"));
    }
    Ok(())
}

async fn run_action(action: &str, run_id: String, extra: Vec<String>) -> Result<String, String> {
    validate_id("run id", &run_id)?;
    let mut args = vec!["daemon".into(), action.into(), run_id];
    args.extend(extra);
    command(args).await
}

#[tauri::command]
pub async fn daemon_desktop_pause(run_id: String) -> Result<String, String> {
    run_action("pause", run_id, vec![]).await
}
#[tauri::command]
pub async fn daemon_desktop_resume(run_id: String) -> Result<String, String> {
    run_action("resume", run_id, vec![]).await
}
#[tauri::command]
pub async fn daemon_desktop_cancel(
    run_id: String,
    reason: Option<String>,
) -> Result<String, String> {
    let mut extra = Vec::new();
    if let Some(value) = reason {
        validate_token("cancellation reason", &value, 1024)?;
        extra.extend(["--reason".into(), value]);
    }
    run_action("cancel", run_id, extra).await
}
#[tauri::command]
pub async fn daemon_desktop_retry(
    run_id: String,
    acknowledge_side_effects: bool,
) -> Result<String, String> {
    let mut extra = Vec::new();
    if acknowledge_side_effects {
        extra.push("--acknowledge-side-effects".into());
    }
    run_action("retry", run_id, extra).await
}

#[tauri::command]
pub async fn daemon_desktop_kill_switch(engaged: bool) -> Result<String, String> {
    command(vec![
        "daemon".into(),
        "kill-switch".into(),
        if engaged { "engage" } else { "release" }.into(),
    ])
    .await
}

#[tauri::command]
pub async fn daemon_desktop_triggers() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "trigger".into(),
            "list".into(),
            "--json".into(),
        ])
        .await?,
    )
}

fn managed_recipe_trigger_id(entry_id: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(entry_id.as_bytes()));
    format!("{MANAGED_RECIPE_TRIGGER_PREFIX}{}", &digest[..40])
}

fn scheduler_now_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| error.to_string())
}

fn visible_recipes(state: &crate::AppState) -> Result<HashMap<String, VisibleRecipe>, String> {
    let global_config_roots = crate::recipes::global_config_roots()?;
    let workspace = crate::workspace::primary_root_canon(state).ok();
    let mut visible = HashMap::new();
    for discovered in crate::recipes::discover_recipes(workspace.as_deref(), &global_config_roots) {
        let Some(recipe) = discovered.recipe else {
            continue;
        };
        let Ok(path) = discovered.path.canonicalize() else {
            continue;
        };
        let Some(path) = path.to_str() else {
            continue;
        };
        let mut required_params = recipe
            .params
            .iter()
            .filter_map(|(name, default)| default.is_none().then_some(name.clone()))
            .collect::<Vec<_>>();
        required_params.sort();
        visible.insert(
            recipe.name,
            VisibleRecipe {
                canonical_path: path.to_string(),
                permission_mode: recipe.permission_mode,
                required_params,
            },
        );
    }
    Ok(visible)
}

fn plan_managed_recipe_triggers(
    request: &DaemonRecipeScheduleSyncRequest,
    visible: &HashMap<String, VisibleRecipe>,
) -> Result<
    (
        Vec<ManagedRecipeTriggerReplacement>,
        Vec<DaemonRecipeScheduleIssue>,
    ),
    String,
> {
    if request.schedules.len() > MAX_RECIPE_SCHEDULES {
        return Err(format!(
            "Recipe schedule snapshot exceeds {MAX_RECIPE_SCHEDULES} entries"
        ));
    }
    let mut seen = BTreeSet::new();
    let mut replacements = Vec::new();
    let mut issues = Vec::new();
    for schedule in &request.schedules {
        if !seen.insert(schedule.entry_id.as_str()) {
            return Err(format!(
                "Recipe schedule '{}' appears more than once",
                schedule.entry_id
            ));
        }
        let issue = |message: String| DaemonRecipeScheduleIssue {
            entry_id: schedule.entry_id.clone(),
            message,
        };
        if schedule.entry_id.is_empty()
            || schedule.entry_id.len() > 512
            || schedule.entry_id.chars().any(char::is_control)
        {
            issues.push(issue("Schedule identifier is invalid".to_string()));
            continue;
        }
        if !schedule.enabled {
            continue;
        }
        let Some(recipe) = visible.get(&schedule.recipe_name) else {
            issues.push(issue(format!(
                "Recipe '{}' is not currently visible; its daemon trigger was disabled",
                schedule.recipe_name
            )));
            continue;
        };
        let Some(requested_path) = schedule.recipe_path.as_deref() else {
            issues.push(issue(
                "Recipe path is unavailable; its daemon trigger was disabled".to_string(),
            ));
            continue;
        };
        let requested_path = match Path::new(requested_path).canonicalize() {
            Ok(path) => path,
            Err(error) => {
                issues.push(issue(format!(
                    "Recipe path cannot be resolved; its daemon trigger was disabled: {error}"
                )));
                continue;
            }
        };
        if requested_path.to_str() != Some(recipe.canonical_path.as_str()) {
            issues.push(issue(
                "Recipe identity changed; refresh recipes before enabling this schedule"
                    .to_string(),
            ));
            continue;
        }
        if let Some(mode) = schedule.permission_mode_override.as_deref() {
            if mode == "bypass" {
                issues.push(issue(
                    "The bypass permission mode is forbidden for unattended daemon schedules"
                        .to_string(),
                ));
                continue;
            }
            if mode != recipe.permission_mode {
                issues.push(issue(format!(
                    "Daemon schedules use the recipe's declared '{}' permission mode; remove the legacy '{}' override",
                    recipe.permission_mode, mode
                )));
                continue;
            }
        }
        if !recipe.required_params.is_empty() {
            issues.push(issue(format!(
                "Recipe requires parameter(s) with no defaults: {}",
                recipe.required_params.join(", ")
            )));
            continue;
        }
        if let Err(error) = crate::automations::validate_cron_impl(&schedule.cron) {
            issues.push(issue(error));
            continue;
        }
        let next = crate::automations::next_occurrences_impl(&schedule.cron, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| "Cron schedule produced no next occurrence".to_string())?;
        let next_fire_at_ms = u64::try_from(next)
            .map_err(|_| "Cron schedule produced a timestamp before the epoch".to_string())?;
        let config_json = serde_json::to_vec(&serde_json::json!({
            "kind": "cron",
            "target": {
                "target_kind": "recipe",
                "recipe": recipe.canonical_path,
                "params": {},
                "payload_param": null
            },
            "workflow": null,
            "schedule": schedule.cron
        }))
        .map_err(|error| error.to_string())?;
        replacements.push(ManagedRecipeTriggerReplacement {
            trigger_id: managed_recipe_trigger_id(&schedule.entry_id),
            config_json,
            next_fire_at_ms,
        });
    }
    replacements.sort_by(|left, right| left.trigger_id.cmp(&right.trigger_id));
    Ok((replacements, issues))
}

#[derive(Clone, Debug)]
struct ExistingManagedTrigger {
    config_json: Vec<u8>,
    enabled: bool,
    next_fire_at_ms: Option<u64>,
    last_delivery_at_ms: Option<u64>,
}

fn read_managed_recipe_triggers(
    connection: &rusqlite::Connection,
) -> Result<HashMap<String, ExistingManagedTrigger>, String> {
    let pattern = format!("{MANAGED_RECIPE_TRIGGER_PREFIX}*");
    let mut statement = connection
        .prepare(
            "SELECT trigger_id,config_json,enabled,next_fire_at_ms,last_delivery_at_ms
             FROM triggers WHERE trigger_id GLOB ?1 ORDER BY trigger_id ASC",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([pattern], |row| {
            let next = row
                .get::<_, Option<i64>>(3)?
                .map(|value| {
                    u64::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(3, value))
                })
                .transpose()?;
            let last = row
                .get::<_, Option<i64>>(4)?
                .map(|value| {
                    u64::try_from(value)
                        .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, value))
                })
                .transpose()?;
            Ok((
                row.get::<_, String>(0)?,
                ExistingManagedTrigger {
                    config_json: row.get(1)?,
                    enabled: row.get::<_, i64>(2)? != 0,
                    next_fire_at_ms: next,
                    last_delivery_at_ms: last,
                },
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<HashMap<_, _>, _>>()
        .map_err(|error| error.to_string())
}

fn last_deliveries_for_entries(
    existing: &HashMap<String, ExistingManagedTrigger>,
    schedules: &[DaemonRecipeSchedule],
) -> BTreeMap<String, u64> {
    schedules
        .iter()
        .filter_map(|schedule| {
            existing
                .get(&managed_recipe_trigger_id(&schedule.entry_id))
                .and_then(|trigger| trigger.last_delivery_at_ms)
                .map(|last| (schedule.entry_id.clone(), last))
        })
        .collect()
}

fn replace_managed_recipe_triggers(
    ledger: &mut crate::run_ledger::RunLedger,
    schedules: &[DaemonRecipeSchedule],
    replacements: &[ManagedRecipeTriggerReplacement],
    now_ms: u64,
) -> Result<(Vec<String>, BTreeMap<String, u64>), String> {
    let transaction = ledger
        .connection_mut()
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let existing = read_managed_recipe_triggers(&transaction)?;
    let desired = replacements
        .iter()
        .map(|replacement| replacement.trigger_id.as_str())
        .collect::<BTreeSet<_>>();
    let mut disabled = existing
        .iter()
        .filter_map(|(trigger_id, trigger)| {
            (trigger.enabled && !desired.contains(trigger_id.as_str()))
                .then_some(trigger_id.clone())
        })
        .collect::<Vec<_>>();
    disabled.sort();
    let now = i64::try_from(now_ms).map_err(|_| "Scheduler timestamp overflow".to_string())?;
    for trigger_id in &disabled {
        transaction
            .execute(
                "UPDATE triggers SET enabled=0,updated_at_ms=?2
                 WHERE trigger_id=?1 AND enabled=1",
                params![trigger_id, now],
            )
            .map_err(|error| error.to_string())?;
    }
    for replacement in replacements {
        let current = existing.get(&replacement.trigger_id);
        if current.is_some_and(|trigger| {
            trigger.enabled && trigger.config_json == replacement.config_json
        }) {
            continue;
        }
        let next = current
            .filter(|trigger| trigger.enabled && trigger.config_json == replacement.config_json)
            .and_then(|trigger| trigger.next_fire_at_ms)
            .unwrap_or(replacement.next_fire_at_ms);
        let next = i64::try_from(next).map_err(|_| "Cron timestamp overflow".to_string())?;
        transaction
            .execute(
                "INSERT INTO triggers (
                    trigger_id,kind,config_json,enabled,created_at_ms,
                    updated_at_ms,next_fire_at_ms,last_delivery_at_ms
                 ) VALUES (?1,'cron',?2,1,?3,?3,?4,NULL)
                 ON CONFLICT(trigger_id) DO UPDATE SET
                    kind='cron',config_json=excluded.config_json,enabled=1,
                    updated_at_ms=excluded.updated_at_ms,
                    next_fire_at_ms=excluded.next_fire_at_ms",
                params![replacement.trigger_id, replacement.config_json, now, next],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok((disabled, last_deliveries_for_entries(&existing, schedules)))
}

#[tauri::command]
pub async fn daemon_desktop_sync_recipe_schedules(
    request: DaemonRecipeScheduleSyncRequest,
    state: tauri::State<'_, crate::AppState>,
) -> Result<DaemonRecipeScheduleSyncResult, String> {
    if request.schedules.len() > MAX_RECIPE_SCHEDULES {
        return Err(format!(
            "Recipe schedule snapshot exceeds {MAX_RECIPE_SCHEDULES} entries"
        ));
    }
    let status = daemon_desktop_status().await?;
    let now = scheduler_now_ms()?;
    let database = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve application data directory".to_string())?
        .join("profile-v1.sqlite3");
    if !status.installed {
        let (disabled_trigger_ids, last_delivery_at_ms) = if database.is_file() {
            let mut ledger =
                crate::run_ledger::RunLedger::open(&database).map_err(|error| error.to_string())?;
            replace_managed_recipe_triggers(&mut ledger, &request.schedules, &[], now)?
        } else {
            (Vec::new(), BTreeMap::new())
        };
        return Ok(DaemonRecipeScheduleSyncResult {
            authority: "in_app".to_string(),
            installed: false,
            service_running: false,
            synchronized_at_ms: now,
            active_trigger_ids: Vec::new(),
            disabled_trigger_ids,
            issues: Vec::new(),
            last_delivery_at_ms,
        });
    }

    let visible = visible_recipes(state.inner())?;
    let (replacements, issues) = plan_managed_recipe_triggers(&request, &visible)?;
    let active_trigger_ids = replacements
        .iter()
        .map(|replacement| replacement.trigger_id.clone())
        .collect::<Vec<_>>();
    let mut ledger =
        crate::run_ledger::RunLedger::open(&database).map_err(|error| error.to_string())?;
    let (disabled_trigger_ids, last_delivery_at_ms) =
        replace_managed_recipe_triggers(&mut ledger, &request.schedules, &replacements, now)?;
    Ok(DaemonRecipeScheduleSyncResult {
        authority: "daemon".to_string(),
        installed: true,
        service_running: status.service_running,
        synchronized_at_ms: now,
        active_trigger_ids,
        disabled_trigger_ids,
        issues,
        last_delivery_at_ms,
    })
}

#[tauri::command]
pub async fn remote_host_status() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "host-status".into(),
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn remote_host_configure(request: RemoteHostConfigureRequest) -> Result<String, String> {
    validate_token("listen address", &request.listen, 512)?;
    let url = url::Url::parse(&request.advertise_url)
        .map_err(|error| format!("Invalid advertised URL: {error}"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Advertised remote URL must be credential-free HTTPS".to_string());
    }
    validate_existing_private_input("TLS certificate", &request.tls_certificate)?;
    validate_existing_private_input("TLS private key", &request.tls_private_key)?;
    command(vec![
        "daemon".into(),
        "remote".into(),
        "host-configure".into(),
        "--listen".into(),
        request.listen,
        "--advertise-url".into(),
        request.advertise_url,
        "--tls-certificate".into(),
        request.tls_certificate,
        "--tls-private-key".into(),
        request.tls_private_key,
    ])
    .await
}

#[tauri::command]
pub async fn remote_host_disable() -> Result<String, String> {
    command(vec![
        "daemon".into(),
        "remote".into(),
        "host-disable".into(),
    ])
    .await
}

/// Creates one invitation and hands the panel everything it needs to show it.
///
/// Returns JSON rather than the CLI's own sentence because the pairing panel
/// has to *render* the compact code as a scannable image, and a webview cannot
/// do that from a line of prose. `--qr --json` is always passed: the code costs
/// nothing to compute, and an operator who has a phone in their hand should not
/// have to re-run the command with another flag to get it — the invitation is
/// one-time, so re-running it would strand the first one.
///
/// The doc comment sits *above* the attribute deliberately: `lib.rs`'s
/// reachability guard scans the text between `#[tauri::command` and `fn`, and
/// anything in that gap makes the command invisible to it.
#[tauri::command]
pub async fn remote_pair_create(request: RemotePairRequest) -> Result<Value, String> {
    validate_output_path(&request.output)?;
    validate_remote_pair_request(&request)?;
    let mut args = vec![
        "daemon".into(),
        "remote".into(),
        "pair-create".into(),
        "--output".into(),
        request.output,
        "--expires-minutes".into(),
        request.expires_minutes.to_string(),
        "--max-artifact-bytes".into(),
        request.max_artifact_bytes.to_string(),
        "--qr".into(),
        "--json".into(),
    ];
    for action in request.actions {
        args.extend(["--action".into(), action]);
    }
    for run in request.run_ids {
        validate_id("run id", &run)?;
        args.extend(["--run".into(), run]);
    }
    for workspace in request.workspace_ids {
        validate_id("workspace id", &workspace)?;
        args.extend(["--workspace".into(), workspace]);
    }
    for capability in request.mobile_capabilities {
        args.extend(["--mobile".into(), capability]);
    }
    for capability in request.device_capabilities {
        args.extend(["--device".into(), capability]);
    }
    parse_json(&command(args).await?)
}

// --- Push, as the operator's own configuration ------------------------------
//
// Little Monkey ships no push project, no key and no relay, so every one of
// these commands acts on state the operator created on this machine. They are
// here because the settings panel is where somebody decides whether their phone
// may be woken at all, and shelling out to a terminal for that decision is how
// a feature ends up switched on by nobody.

#[tauri::command]
pub async fn remote_push_status() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "push-status".into(),
            "--json".into(),
        ])
        .await?,
    )
}

/// Turns push on, either as Web Push (this runner's own VAPID identity, no
/// account anywhere) or against the operator's own Firebase project.
///
/// `include_detail` is passed through rather than defaulted here: it decides
/// whether run specifics reach a lock screen, and the panel states that in the
/// sentence beside the checkbox.
#[tauri::command]
pub async fn remote_push_configure(
    web_push: bool,
    vapid_subject: Option<String>,
    project_id: Option<String>,
    service_account: Option<String>,
    include_detail: bool,
) -> Result<String, String> {
    let mut args = vec!["daemon".into(), "remote".into(), "push-configure".into()];
    if web_push {
        args.push("--web-push".into());
        if let Some(subject) = vapid_subject {
            validate_token("VAPID subject", &subject, 512)?;
            args.extend(["--vapid-subject".into(), subject]);
        }
    } else {
        let project = project_id.ok_or("A Firebase project id is required")?;
        validate_token("project id", &project, 256)?;
        let account = service_account.ok_or("A service account JSON key is required")?;
        validate_existing_private_input("service account key", &account)?;
        args.extend([
            "--project-id".into(),
            project,
            "--service-account".into(),
            account,
        ]);
    }
    if include_detail {
        args.push("--include-detail".into());
    }
    command(args).await
}

#[tauri::command]
pub async fn remote_push_disable() -> Result<String, String> {
    command(vec![
        "daemon".into(),
        "remote".into(),
        "push-disable".into(),
    ])
    .await
}

#[tauri::command]
pub async fn remote_push_test(device_id: String) -> Result<String, String> {
    validate_id("device id", &device_id)?;
    command(vec![
        "daemon".into(),
        "remote".into(),
        "push-test".into(),
        device_id,
    ])
    .await
}

#[tauri::command]
pub async fn remote_pair_list() -> Result<String, String> {
    command(vec!["daemon".into(), "remote".into(), "pair-list".into()]).await
}

#[tauri::command]
pub async fn remote_pair_revoke(device_id: String, reason: String) -> Result<String, String> {
    validate_id("device id", &device_id)?;
    validate_token("revoke reason", &reason, 1024)?;
    command(vec![
        "daemon".into(),
        "remote".into(),
        "pair-revoke".into(),
        device_id,
        "--reason".into(),
        reason,
    ])
    .await
}

#[tauri::command]
pub async fn remote_pair_rotate(device_id: String, output: String) -> Result<String, String> {
    validate_id("device id", &device_id)?;
    validate_output_path(&output)?;
    command(vec![
        "daemon".into(),
        "remote".into(),
        "pair-rotate".into(),
        device_id,
        "--output".into(),
        output,
    ])
    .await
}

// --- Roadmap K17: the placement plane, for the desktop ---------------------
//
// Thin wrappers over the CLI's own JSON, exactly like every command above. The
// node and placement tables live in the daemon's remote database, which this
// library cannot open — `RemoteStore` is a `monkey-cli` type — so the sidecar
// that already owns that state is also the one that answers for it. That is the
// same reasoning `remote_host_status` and `remote_audit` follow, and it keeps
// one implementation of the placement rules rather than a second one here.

#[tauri::command]
pub async fn remote_node_list() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "node-list".into(),
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn remote_placements() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "placements".into(),
            "--json".into(),
        ])
        .await?,
    )
}

/// Re-describes one paired node, or every one when `alias` is absent.
///
/// This is the call that reaches other machines, so it is the slow one; the
/// listing above only reads what a previous refresh stored.
#[tauri::command]
pub async fn remote_node_refresh(alias: Option<String>) -> Result<String, String> {
    let mut args = vec!["daemon".into(), "remote".into(), "node-refresh".into()];
    if let Some(alias) = alias {
        validate_id("node alias", &alias)?;
        args.push(alias);
    }
    command(args).await
}

#[tauri::command]
pub async fn remote_placement_sync() -> Result<String, String> {
    command(vec![
        "daemon".into(),
        "remote".into(),
        "placement-sync".into(),
    ])
    .await
}

/// States what this machine advertises to schedulers allowed to place work on
/// it. Both values are operator statements — nothing infers a machine's
/// jurisdiction — so both are validated here before they reach the sidecar.
#[tauri::command]
pub async fn remote_node_label(
    name: Option<String>,
    residency: Option<String>,
) -> Result<String, String> {
    let mut args = vec!["daemon".into(), "remote".into(), "node-label".into()];
    if let Some(name) = name {
        validate_token("node name", &name, 128)?;
        args.push("--name".into());
        args.push(name);
    }
    if let Some(residency) = residency {
        crate::node_placement::validate_residency(&residency)?;
        args.push("--residency".into());
        args.push(residency);
    }
    if args.len() == 3 {
        return Err("Set a node name, a data-residency label, or both".to_string());
    }
    command(args).await
}

// --- Paired physical devices, for the desktop -----------------------------

#[tauri::command]
pub async fn remote_device_list() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "device-list".into(),
            "--json".into(),
        ])
        .await?,
    )
}

/// Replaces one device's physical grant. The CLI refuses anything that is not
/// a physical capability, so this cannot become a way to widen run access from
/// the desktop.
#[tauri::command]
pub async fn remote_device_grant(
    device_id: String,
    capabilities: Vec<String>,
) -> Result<String, String> {
    validate_id("device id", &device_id)?;
    let mut args = vec![
        "daemon".into(),
        "remote".into(),
        "device-grant".into(),
        device_id,
    ];
    for capability in capabilities {
        validate_token("device capability", &capability, 64)?;
        args.extend(["--capability".into(), capability]);
    }
    command(args).await
}

#[tauri::command]
pub async fn remote_device_commands(device_id: String, limit: u32) -> Result<Value, String> {
    validate_id("device id", &device_id)?;
    if !(1..=200).contains(&limit) {
        return Err("Command limit must be 1..=200".to_string());
    }
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "device-commands".into(),
            device_id,
            "--limit".into(),
            limit.to_string(),
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn remote_device_cancel(command_id: String) -> Result<String, String> {
    validate_id("command id", &command_id)?;
    command(vec![
        "daemon".into(),
        "remote".into(),
        "device-cancel".into(),
        command_id,
    ])
    .await
}

/// The desktop half of the `device_action` agent tool.
///
/// Permission-gated here and then handed to the sidecar, which owns the queue,
/// the capability intersection and the argument bounds — the desktop does not
/// get a second, looser copy of any of them. Same division of labour as every
/// other command in this file, and the reason it is a fixed argument vector
/// rather than a passthrough: the model's arguments arrive as named parameters
/// and are re-emitted as named flags, never as a caller-supplied argv.
// The `allow` precedes the command attribute deliberately: `lib.rs`'s
// `every_tauri_command_is_reachable_from_the_invoke_handler` scans what sits
// *between* `#[tauri::command` and `fn`, and anything it does not recognize
// there makes the command invisible to that guard.
#[allow(clippy::too_many_arguments)]
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_device_action(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    action: String,
    device_id: Option<String>,
    position: Option<String>,
    duration_ms: Option<u64>,
    accuracy: Option<String>,
    title: Option<String>,
    body: Option<String>,
    text: Option<String>,
    run_id: Option<String>,
    artifact_id: Option<String>,
    wait_ms: Option<u64>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<Value, String> {
    validate_token("device action", &action, 64)?;
    let detail = match &device_id {
        Some(device_id) => format!("{action} on {device_id}"),
        None => action.clone(),
    };
    crate::permissions::request_permission(
        &app,
        state.inner(),
        "device_action",
        detail,
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        None,
        None,
    )
    .await?;

    let mut args = vec![
        "daemon".into(),
        "remote".into(),
        "device-action".into(),
        action,
        "--json".into(),
    ];
    if let Some(device_id) = device_id {
        validate_id("device id", &device_id)?;
        args.extend(["--device-id".into(), device_id]);
    }
    for (flag, value) in [
        ("--position", position),
        ("--accuracy", accuracy),
        ("--title", title),
        ("--body", body),
        ("--text", text),
    ] {
        if let Some(value) = value {
            validate_token(flag, &value, 1_024)?;
            args.extend([flag.into(), value]);
        }
    }
    // Ids, not free text: an artifact reference reaches the artifact route, so
    // it goes through the same validation every other id on this bridge does.
    for (flag, value) in [("--run-id", run_id), ("--artifact-id", artifact_id)] {
        if let Some(value) = value {
            validate_id(flag, &value)?;
            args.extend([flag.into(), value]);
        }
    }
    if let Some(duration_ms) = duration_ms {
        args.extend(["--duration-ms".into(), duration_ms.to_string()]);
    }
    if let Some(wait_ms) = wait_ms {
        args.extend(["--wait-ms".into(), wait_ms.to_string()]);
    }
    // The turn and the call inside it: the desktop's durable identity for this
    // invocation, and the only thing that stops a replayed turn from taking a
    // second photograph. Both come from the runtime, never from the model.
    if let (Some(turn_id), Some(tool_call_id)) = (&turn_id, &tool_call_id) {
        validate_id("turn id", turn_id)?;
        validate_id("tool call id", tool_call_id)?;
        args.extend([
            "--invocation-id".into(),
            format!("{turn_id}:{tool_call_id}"),
        ]);
    }
    parse_json(&command(args).await?)
}

#[tauri::command]
pub async fn remote_audit(limit: u32) -> Result<Value, String> {
    if !(1..=10_000).contains(&limit) {
        return Err("Audit limit must be 1..=10000".to_string());
    }
    parse_json(
        &command(vec![
            "daemon".into(),
            "remote".into(),
            "audit".into(),
            "--limit".into(),
            limit.to_string(),
        ])
        .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_bridge_rejects_unsafe_inputs() {
        assert!(validate_id("run", "run-1").is_ok());
        assert!(validate_id("run", "../../escape").is_err());
        assert!(validate_token("recipe", "recipe.json\0--purge", 100).is_err());
    }

    #[test]
    fn remote_pairing_is_bounded_before_cli_dispatch() {
        let valid = RemotePairRequest {
            output: "/tmp/pair.json".to_string(),
            expires_minutes: 15,
            actions: vec!["view-runs".to_string(), "read-artifacts".to_string()],
            run_ids: vec!["run-one".to_string()],
            workspace_ids: Vec::new(),
            max_artifact_bytes: 8 * 1024 * 1024,
            mobile_capabilities: Vec::new(),
            device_capabilities: Vec::new(),
        };
        assert!(validate_remote_pair_request(&valid).is_ok());

        let mut missing_scope = valid.clone();
        missing_scope.run_ids.clear();
        assert!(validate_remote_pair_request(&missing_scope).is_err());

        let mut oversized = valid.clone();
        oversized.max_artifact_bytes = MAX_REMOTE_ARTIFACT_BYTES + 1;
        assert!(validate_remote_pair_request(&oversized).is_err());

        let mut missing_dependency = valid.clone();
        missing_dependency.actions = vec!["approve".to_string()];
        assert!(validate_remote_pair_request(&missing_dependency).is_err());

        // Mobile grants are validated here too, before the CLI is dispatched:
        // an unknown capability never becomes an argv entry.
        let mut unknown_mobile = valid.clone();
        unknown_mobile.mobile_capabilities = vec!["exfiltrate".to_string()];
        assert!(validate_remote_pair_request(&unknown_mobile).is_err());

        let mut granted_mobile = valid;
        granted_mobile.mobile_capabilities = vec!["view-sessions".to_string(), "chat".to_string()];
        assert!(validate_remote_pair_request(&granted_mobile).is_ok());
    }

    /// A verbatim `monkey daemon status --json` payload, as a string.
    ///
    /// A string and not `json!` on purpose: this is the CLI's wire bytes, and the
    /// whole point of the test below is to decode what the CLI actually sends
    /// rather than something re-spelled on this side of the bridge.
    fn cli_status_json(backpressure: &str) -> String {
        format!(
            r#"{{"installed":true,"service_running":true,"heartbeat_fresh":true,"pid":1,
               "kill_switch":false,"queued":0,"active":1,"waiting_approval":0,"paused":0,
               "managed_run_ids":["run-one"],"platform":"macos","backpressure":{backpressure}}}"#
        )
    }

    #[test]
    fn status_deserializes_cli_shape_and_serializes_ui_shape() {
        let status: DaemonDesktopStatus = serde_json::from_str(&cli_status_json(
            r#"{"state":"accepting","accepting":true,"reason":null,"detail":null,
                "retry_after_ms":null,"queue_depth":0,"queue_capacity":128,"queued":0,"held":0}"#,
        ))
        .unwrap();
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["serviceRunning"], true);
        assert_eq!(value["managedRunIds"], serde_json::json!(["run-one"]));
        assert!(value.get("service_running").is_none());
    }

    /// The regression this exists for: `backpressure` decoding as absent.
    ///
    /// Before the field was added, `DaemonDesktopStatus` had no
    /// `deny_unknown_fields`, so the CLI's `backpressure` object was silently
    /// dropped and the desktop could not see the signal at all — nothing failed,
    /// which is exactly why nobody noticed. Spelling the nested struct's
    /// `rename_all` as a plain `camelCase` reintroduces the same class of bug for
    /// `retry_after_ms`/`queue_depth`/`queue_capacity`, so this asserts on the
    /// three multi-word fields specifically and not just on `state`.
    #[test]
    fn status_carries_the_snake_case_backpressure_signal_the_cli_emits() {
        let closed: DaemonDesktopStatus = serde_json::from_str(&cli_status_json(
            r#"{"state":"closed","accepting":false,"reason":"queue_full",
                "detail":"128 of 128 queue slots are in use; wait for a run or cancel one",
                "retry_after_ms":32000,"queue_depth":128,"queue_capacity":128,
                "queued":124,"held":0}"#,
        ))
        .unwrap();

        // The signal arrived populated — not defaulted, not dropped.
        assert_eq!(closed.backpressure.state, DesktopBackpressureState::Closed);
        assert!(!closed.backpressure.accepting);
        assert_eq!(closed.backpressure.reason.as_deref(), Some("queue_full"));
        assert!(closed.backpressure.detail.is_some());
        // The three fields a plain `rename_all = "camelCase"` would have lost.
        assert_eq!(closed.backpressure.retry_after_ms, Some(32_000));
        assert_eq!(closed.backpressure.queue_depth, 128);
        assert_eq!(closed.backpressure.queue_capacity, 128);
        assert_eq!(closed.backpressure.queued, 124);

        // Re-serialization is the UI's camelCase, with nothing lost in between.
        let value = serde_json::to_value(&closed).unwrap();
        assert_eq!(value["backpressure"]["state"], "closed");
        assert_eq!(value["backpressure"]["retryAfterMs"], 32_000);
        assert_eq!(value["backpressure"]["queueDepth"], 128);
        assert_eq!(value["backpressure"]["queueCapacity"], 128);
        assert!(value["backpressure"].get("retry_after_ms").is_none());

        // `slow` is a distinct third state, not a flavour of either other one: a
        // producer that collapsed it into `closed` would refuse work the daemon
        // is still accepting.
        let slow: DaemonDesktopStatus = serde_json::from_str(&cli_status_json(
            r#"{"state":"slow","accepting":true,"reason":"memory_saturated",
                "detail":"all 4 queued runs are waiting on memory; more work will queue but not start",
                "retry_after_ms":1000,"queue_depth":8,"queue_capacity":128,"queued":4,"held":4}"#,
        ))
        .unwrap();
        assert_eq!(slow.backpressure.state, DesktopBackpressureState::Slow);
        assert!(
            slow.backpressure.accepting,
            "`slow` still accepts; only `closed` refuses"
        );
        assert_eq!(slow.backpressure.held, 4);

        // A missing signal is an error, never a permissive default. See the
        // field's doc comment for why this is not an `Option`.
        assert!(serde_json::from_str::<DaemonDesktopStatus>(
            &cli_status_json("null").replace(",\"backpressure\":null", "")
        )
        .is_err());
    }

    /// The casing trap, for the decision log this time.
    ///
    /// A verbatim `monkey daemon decisions --json` row as a string, not a `json!`
    /// macro: re-spelling the payload on this side of the bridge would test the
    /// mirror against itself. `SchedulerDecision` carries a plain
    /// `rename_all = "camelCase"`, so unlike the backpressure block above this is
    /// camelCase on the wire — and getting it wrong here shows up as a panel full
    /// of empty rows rather than as an error, which is why it is asserted field by
    /// field. The CLI end of the same contract is pinned in
    /// `daemon/store.rs::the_decision_log_is_bounded_and_retains_the_newest`.
    #[test]
    fn decisions_deserialize_the_camel_case_shape_the_cli_emits() {
        let decisions: Vec<DesktopSchedulerDecision> = serde_json::from_str(
            r#"[
              {
                "decidedAtMs": 1750000000000,
                "jobId": "job-one",
                "outcome": "admitted",
                "processClass": "interactive",
                "effectiveClass": "batch",
                "workspace": "/Users/x/repo",
                "passedOver": ["job-two", "job-three"],
                "detail": "admitted over 2 jobs; available RAM read 9.2 GiB",
                "measurement": "available_ram_bytes",
                "measuredValue": 9878424780,
                "measuredAtMs": 1749999999000
              }
            ]"#,
        )
        .unwrap();

        let decision = &decisions[0];
        assert_eq!(decision.decided_at_ms, 1_750_000_000_000);
        assert_eq!(decision.job_id, "job-one");
        assert_eq!(decision.outcome, "admitted");
        assert_eq!(decision.process_class, "interactive");
        assert_eq!(decision.effective_class, "batch");
        assert_eq!(decision.workspace.as_deref(), Some("/Users/x/repo"));
        assert_eq!(decision.passed_over, ["job-two", "job-three"]);
        assert!(decision.detail.contains("available RAM"));
        assert_eq!(decision.measurement, "available_ram_bytes");
        assert_eq!(decision.measured_value, Some(9_878_424_780));
        // The observation's own time, never re-stamped as the decision time.
        assert_eq!(decision.measured_at_ms, Some(1_749_999_999_000));

        // Back out to the UI in the same spelling the TS interface declares.
        let value = serde_json::to_value(decision).unwrap();
        assert_eq!(
            value["passedOver"],
            serde_json::json!(["job-two", "job-three"])
        );
        assert_eq!(value["measuredAtMs"], 1_749_999_999_000_u64);
        assert!(value.get("measured_at_ms").is_none());

        // Nullable columns arrive null, and an empty log is an empty array.
        let sparse: Vec<DesktopSchedulerDecision> = serde_json::from_str(
            r#"[{"decidedAtMs":1,"jobId":"j","outcome":"rejected","processClass":"batch",
                 "effectiveClass":"batch","workspace":null,"passedOver":[],"detail":"d",
                 "measurement":"none","measuredValue":null,"measuredAtMs":null}]"#,
        )
        .unwrap();
        assert!(sparse[0].workspace.is_none() && sparse[0].measured_value.is_none());
        assert!(serde_json::from_str::<Vec<DesktopSchedulerDecision>>("[]")
            .unwrap()
            .is_empty());
    }

    /// An absent signal reads as accepting; only an explicit `closed` refuses.
    #[test]
    fn backpressure_signal_is_absent_rather_than_permissive_by_accident() {
        assert!(backpressure_signal(&serde_json::json!({ "installed": true })).is_none());
        let signal = backpressure_signal(&serde_json::json!({
            "backpressure": {
                "state": "slow", "accepting": true, "reason": "queue_deep",
                "detail": "104 of 128 queue slots are in use; slow down",
                "retry_after_ms": 26000, "queue_depth": 104, "queue_capacity": 128,
                "queued": 100, "held": 0
            }
        }))
        .expect("the signal decodes from a raw status value");
        assert_eq!(signal.state, DesktopBackpressureState::Slow);
        assert_eq!(signal.retry_after_ms, Some(26_000));
    }

    fn fixture_schedule(path: &Path, entry_id: &str, cron: &str) -> DaemonRecipeSchedule {
        DaemonRecipeSchedule {
            entry_id: entry_id.to_string(),
            recipe_name: "fixture".to_string(),
            recipe_path: Some(path.to_string_lossy().to_string()),
            cron: cron.to_string(),
            enabled: true,
            permission_mode_override: None,
        }
    }

    #[test]
    fn managed_recipe_ids_are_stable_scoped_and_collision_resistant() {
        let first = managed_recipe_trigger_id("entry-one");
        assert_eq!(first, managed_recipe_trigger_id("entry-one"));
        assert_ne!(first, managed_recipe_trigger_id("entry-two"));
        assert!(first.starts_with(MANAGED_RECIPE_TRIGGER_PREFIX));
        assert!(first
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
        assert!(first.len() < 128);
    }

    #[test]
    fn trigger_plan_fails_closed_for_missing_recipe_params_and_permission_expansion() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-schedule-plan-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("fixture.yml");
        std::fs::write(&path, "fixture").unwrap();
        let canonical = path.canonicalize().unwrap().to_string_lossy().to_string();
        let mut visible = HashMap::new();
        visible.insert(
            "fixture".to_string(),
            VisibleRecipe {
                canonical_path: canonical,
                permission_mode: "acceptEdits".to_string(),
                required_params: vec!["ticket".to_string()],
            },
        );
        let mut schedule = fixture_schedule(&path, "entry-one", "0 3 * * *");
        schedule.permission_mode_override = Some("bypass".to_string());
        let request = DaemonRecipeScheduleSyncRequest {
            schedules: vec![schedule],
        };
        let (replacements, issues) = plan_managed_recipe_triggers(&request, &visible).unwrap();
        assert!(replacements.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].message.contains("bypass"));

        let mut schedule = fixture_schedule(&path, "entry-one", "0 3 * * *");
        schedule.permission_mode_override = Some("plan".to_string());
        let (replacements, issues) = plan_managed_recipe_triggers(
            &DaemonRecipeScheduleSyncRequest {
                schedules: vec![schedule],
            },
            &visible,
        )
        .unwrap();
        assert!(replacements.is_empty());
        assert!(issues[0].message.contains("declared 'acceptEdits'"));

        let (replacements, issues) = plan_managed_recipe_triggers(
            &DaemonRecipeScheduleSyncRequest {
                schedules: vec![fixture_schedule(&path, "entry-one", "0 3 * * *")],
            },
            &visible,
        )
        .unwrap();
        assert!(replacements.is_empty());
        assert!(issues[0].message.contains("ticket"));

        visible.get_mut("fixture").unwrap().required_params.clear();
        let (replacements, issues) = plan_managed_recipe_triggers(
            &DaemonRecipeScheduleSyncRequest {
                schedules: vec![fixture_schedule(&path, "entry-one", "0 3 * * *")],
            },
            &visible,
        )
        .unwrap();
        assert_eq!(replacements.len(), 1);
        assert!(issues.is_empty());
        let config: Value = serde_json::from_slice(&replacements[0].config_json).unwrap();
        assert_eq!(config["kind"], "cron");
        assert_eq!(config["target"]["target_kind"], "recipe");
        assert_eq!(
            config["target"]["recipe"],
            path.canonicalize().unwrap().to_string_lossy().as_ref()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn managed_recipe_batch_atomically_updates_and_disables_without_touching_manual_triggers() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-schedule-ledger-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("fixture.yml");
        std::fs::write(&path, "fixture").unwrap();
        let mut visible = HashMap::new();
        visible.insert(
            "fixture".to_string(),
            VisibleRecipe {
                canonical_path: path.canonicalize().unwrap().to_string_lossy().to_string(),
                permission_mode: "acceptEdits".to_string(),
                required_params: Vec::new(),
            },
        );
        let first_request = DaemonRecipeScheduleSyncRequest {
            schedules: vec![fixture_schedule(&path, "entry-one", "0 3 * * *")],
        };
        let (first, issues) = plan_managed_recipe_triggers(&first_request, &visible).unwrap();
        assert!(issues.is_empty());
        let mut ledger = crate::run_ledger::RunLedger::open_in_memory().unwrap();
        ledger
            .connection_mut()
            .execute(
                "INSERT INTO triggers(trigger_id,kind,config_json,enabled,created_at_ms,updated_at_ms)
                 VALUES('manual-trigger','cron',x'7b7d',1,1,1)",
                [],
            )
            .unwrap();
        let (disabled, _) =
            replace_managed_recipe_triggers(&mut ledger, &first_request.schedules, &first, 1_000)
                .unwrap();
        assert!(disabled.is_empty());
        let trigger_id = managed_recipe_trigger_id("entry-one");
        ledger
            .connection_mut()
            .execute(
                "UPDATE triggers SET last_delivery_at_ms=900 WHERE trigger_id=?1",
                [&trigger_id],
            )
            .unwrap();

        let changed_request = DaemonRecipeScheduleSyncRequest {
            schedules: vec![fixture_schedule(&path, "entry-one", "0 4 * * *")],
        };
        let (changed, issues) = plan_managed_recipe_triggers(&changed_request, &visible).unwrap();
        assert!(issues.is_empty());
        let (disabled, deliveries) = replace_managed_recipe_triggers(
            &mut ledger,
            &changed_request.schedules,
            &changed,
            2_000,
        )
        .unwrap();
        assert!(disabled.is_empty());
        assert_eq!(deliveries.get("entry-one"), Some(&900));
        let config: Vec<u8> = ledger
            .connection()
            .query_row(
                "SELECT config_json FROM triggers WHERE trigger_id=?1 AND enabled=1",
                [&trigger_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&config).unwrap()["schedule"],
            "0 4 * * *"
        );

        let (disabled, deliveries) =
            replace_managed_recipe_triggers(&mut ledger, &changed_request.schedules, &[], 3_000)
                .unwrap();
        assert_eq!(disabled, [trigger_id]);
        assert_eq!(deliveries.get("entry-one"), Some(&900));
        let manual_enabled: i64 = ledger
            .connection()
            .query_row(
                "SELECT enabled FROM triggers WHERE trigger_id='manual-trigger'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(manual_enabled, 1);
        let _ = std::fs::remove_dir_all(root);
    }
}

// --- Messaging channels ---------------------------------------------------
//
// Thin, fixed-argument wrappers over `monkey channels …`. The rules live in the
// CLI (one implementation, two front ends); these exist so the desktop can call
// them without an arbitrary command executor, and so every identifier the UI
// passes is validated before it reaches an argument vector.
//
// `channels_set_credential` is the single place a secret crosses this boundary,
// and it writes straight to the keychain rather than through an argument
// vector: a credential must never be visible in a process listing.

const MAX_CHANNEL_ID: usize = 128;

fn channel_id(label: &str, value: &str) -> Result<String, String> {
    validate_id(label, value)?;
    if value.len() > MAX_CHANNEL_ID || value.starts_with('-') {
        // A value that starts with a dash would be read as a flag by the CLI's
        // own parser even though nothing here goes through a shell.
        return Err(format!("Invalid {label}"));
    }
    Ok(value.to_string())
}

#[tauri::command]
pub async fn channels_list() -> Result<Value, String> {
    parse_json(&command(vec!["channels".into(), "list".into(), "--json".into()]).await?)
}

#[tauri::command]
pub async fn channels_add(
    kind: String,
    label: String,
    config: Option<String>,
) -> Result<Value, String> {
    validate_token("provider", &kind, 32)?;
    validate_token("label", &label, 120)?;
    let mut args = vec!["channels".into(), "add".into(), kind, label];
    if let Some(config) = config {
        // Parsed here as well as in the CLI so a malformed object is refused
        // before it becomes a process argument.
        serde_json::from_str::<Value>(&config)
            .map_err(|error| format!("Provider settings must be a JSON object: {error}"))?;
        args.push("--config".into());
        args.push(config);
    }
    args.push("--json".into());
    parse_json(&command(args).await?)
}

/// Probe an account. The only path that can move an account to `connected`.
#[tauri::command]
pub async fn channels_probe(account_id: String) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "channels".into(),
            "probe".into(),
            account_id,
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn channels_enable(account_id: String, enabled: bool) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["channels".into(), "enable".into(), account_id];
    if !enabled {
        args.push("--off".into());
    }
    command(args).await.map(|_| ())
}

#[tauri::command]
pub async fn channels_set_policy(
    account_id: String,
    direct: Option<String>,
    group: Option<String>,
    activation: Option<String>,
) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["channels".into(), "policy".into(), account_id];
    for (flag, value) in [
        ("--direct", direct),
        ("--group", group),
        ("--activation", activation),
    ] {
        if let Some(value) = value {
            validate_token("policy", &value, 32)?;
            args.push(flag.into());
            args.push(value);
        }
    }
    command(args).await.map(|_| ())
}

#[tauri::command]
pub async fn channels_senders(account_id: String) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "channels".into(),
            "senders".into(),
            account_id,
            "--json".into(),
        ])
        .await?,
    )
}

/// Approve or block a waiting sender.
///
/// Approval is the ability to send messages and nothing else — no tool, device
/// or telephony authority follows from it.
#[tauri::command]
pub async fn channels_decide_sender(
    account_id: String,
    sender_id: String,
    approve: bool,
) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let sender_id = channel_id("sender id", &sender_id)?;
    command(vec![
        "channels".into(),
        if approve {
            "approve".into()
        } else {
            "block".into()
        },
        account_id,
        sender_id,
    ])
    .await
    .map(|_| ())
}

#[tauri::command]
pub async fn channels_routes() -> Result<Value, String> {
    parse_json(&command(vec!["channels".into(), "routes".into(), "--json".into()]).await?)
}

/// The scope and target fields `channels_add_route`/`channels_update_route`
/// share, exactly the CLI's `RouteOptions`.
#[derive(Debug, Default, serde::Deserialize)]
pub struct RouteOptionArgs {
    pub account_id: Option<String>,
    pub conversation_id: Option<String>,
    pub thread_id: Option<String>,
    pub sender_id: Option<String>,
    pub kind: Option<String>,
    pub repository: Option<String>,
    /// Recipe parameters as `name=value` strings.
    #[serde(default)]
    pub params: Vec<String>,
    pub session_scope: Option<String>,
    pub priority: Option<i32>,
    /// Whether runs of this route may answer their conversation. Defaults on.
    pub reply: Option<bool>,
    /// Whether the route is active. Defaults on.
    pub enabled: Option<bool>,
}

/// Turn the shared route fields into CLI arguments.
///
/// Every valued flag is passed as one `--flag=value` token: a provider
/// conversation id may legitimately begin with a dash (a Telegram group id
/// does), and as a separate token the CLI's parser would read it as a flag.
fn route_option_args(options: RouteOptionArgs, args: &mut Vec<String>) -> Result<(), String> {
    for (flag, value) in [
        ("--account", options.account_id),
        ("--conversation", options.conversation_id),
        ("--thread", options.thread_id),
        ("--sender", options.sender_id),
        ("--kind", options.kind),
    ] {
        if let Some(value) = value {
            validate_token("route scope", &value, MAX_CHANNEL_ID)?;
            args.push(format!("{flag}={value}"));
        }
    }
    if let Some(repository) = options.repository {
        validate_token("repository", &repository, 512)?;
        args.push(format!("--repository={repository}"));
    }
    for param in options.params {
        validate_token("route param", &param, 512)?;
        if !param.contains('=') {
            return Err("Route parameters must be name=value".to_string());
        }
        args.push(format!("--param={param}"));
    }
    if let Some(session_scope) = options.session_scope {
        validate_token("session scope", &session_scope, 32)?;
        args.push(format!("--session-scope={session_scope}"));
    }
    if let Some(priority) = options.priority {
        args.push(format!("--priority={priority}"));
    }
    if options.reply == Some(false) {
        args.push("--no-reply".into());
    }
    if options.enabled == Some(false) {
        args.push("--disabled".into());
    }
    Ok(())
}

#[tauri::command]
pub async fn channels_add_route(
    recipe: String,
    options: Option<RouteOptionArgs>,
) -> Result<Value, String> {
    validate_token("recipe", &recipe, 200)?;
    if recipe.starts_with('-') {
        return Err("Invalid recipe".to_string());
    }
    let mut args = vec!["channels".into(), "add-route".into(), recipe];
    route_option_args(options.unwrap_or_default(), &mut args)?;
    args.push("--json".into());
    parse_json(&command(args).await?)
}

#[tauri::command]
pub async fn channels_update_route(
    route_id: String,
    recipe: String,
    options: Option<RouteOptionArgs>,
) -> Result<Value, String> {
    let route_id = channel_id("route id", &route_id)?;
    validate_token("recipe", &recipe, 200)?;
    if recipe.starts_with('-') {
        return Err("Invalid recipe".to_string());
    }
    let mut args = vec!["channels".into(), "update-route".into(), route_id, recipe];
    route_option_args(options.unwrap_or_default(), &mut args)?;
    args.push("--json".into());
    parse_json(&command(args).await?)
}

#[tauri::command]
pub async fn channels_enable_route(route_id: String, enabled: bool) -> Result<(), String> {
    let route_id = channel_id("route id", &route_id)?;
    let mut args = vec!["channels".into(), "enable-route".into(), route_id];
    if !enabled {
        args.push("--off".into());
    }
    command(args).await.map(|_| ())
}

#[tauri::command]
pub async fn channels_remove_route(route_id: String) -> Result<(), String> {
    let route_id = channel_id("route id", &route_id)?;
    command(vec!["channels".into(), "remove-route".into(), route_id])
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn channels_events(account_id: String, limit: u32) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "channels".into(),
            "events".into(),
            account_id,
            "--limit".into(),
            limit.clamp(1, 200).to_string(),
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn channels_remove(account_id: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    command(vec!["channels".into(), "remove".into(), account_id])
        .await
        .map(|_| ())
}

/// Edit an existing account's non-secret settings and label. Secrets cannot
/// travel through here: the CLI subcommand this wraps writes
/// `non_secret_config` and `label` and nothing else.
#[tauri::command]
pub async fn channels_set_config(
    account_id: String,
    config: Option<String>,
    label: Option<String>,
) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["channels".into(), "set-config".into(), account_id];
    if let Some(config) = config {
        // Parsed here as well as in the CLI so a malformed object is refused
        // before it becomes a process argument.
        serde_json::from_str::<Value>(&config)
            .map_err(|error| format!("Provider settings must be a JSON object: {error}"))?;
        args.push("--config".into());
        args.push(config);
    }
    if let Some(label) = label {
        validate_token("label", &label, 120)?;
        args.push(format!("--label={label}"));
    }
    args.push("--json".into());
    parse_json(&command(args).await?)
}

/// The complete callback URL for a webhook account, or `configured: false`
/// with the listener path when no public base URL is set. Composed by the
/// daemon — the one authority on what it is reachable as — never glued
/// together in the frontend.
#[tauri::command]
pub async fn channels_callback_url(account_id: String) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "channels".into(),
            "callback-url".into(),
            account_id,
            "--json".into(),
        ])
        .await?,
    )
}

/// Set or clear the public base URL webhook callbacks are advertised under.
#[tauri::command]
pub async fn channels_set_public_url(url: Option<String>) -> Result<(), String> {
    let mut args = vec!["channels".into(), "set-public-url".into()];
    match url {
        Some(url) => {
            validate_token("public base URL", &url, 512)?;
            // A positional argument, so a value starting with a dash would be
            // read as a flag by the CLI's own parser — `--clear` typed into
            // the URL box must not clear the setting instead of failing.
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err("The public base URL must start with http:// or https://".to_string());
            }
            args.push(url);
        }
        None => args.push("--clear".into()),
    }
    command(args).await.map(|_| ())
}

/// Store an account's credential.
///
/// Writes to the same keychain entry the daemon's adapters read, named by the
/// one definition both sides share, so the desktop and the CLI cannot drift
/// into writing different entries. The value is never echoed back, never
/// logged, and never returned — the account row only ever learns that a
/// credential exists.
#[tauri::command]
pub async fn channels_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    if secret.is_empty() || secret.len() > 8192 {
        return Err("A messaging credential must contain 1-8192 bytes".to_string());
    }
    let reference = crate::channels::credential_ref(&account_id);
    keyring::Entry::new(&crate::channels::KEYCHAIN_SERVICE, &reference)
        .map_err(|error| format!("Failed to open the messaging keychain entry: {error}"))?
        .set_password(&secret)
        .map_err(|error| format!("Failed to save the messaging credential: {error}"))?;
    // The CLI owns the account row; this marks it as having a credential.
    command(vec![
        "channels".into(),
        "mark-credential".into(),
        account_id,
    ])
    .await
    .map(|_| ())
}

// --- Peers -----------------------------------------------------------------
//
// Thin, fixed-argument wrappers over `monkey peers …`, like the messaging
// channel commands above. Two things are deliberately absent: any way to name
// a capability that is not a peer grant (the CLI's parser knows three words),
// and any way for React to see a pairing token — an invitation is written to a
// file the operator chooses and moved out of band, exactly as a controller
// pairing already is.

/// Peer grants an operator may hand out, spelled once.
fn peer_grants(allow: &[String]) -> Result<String, String> {
    let mut tokens = Vec::new();
    for grant in allow {
        match grant.as_str() {
            "message" | "task" | "artifact" => tokens.push(grant.as_str()),
            other => return Err(format!("Unknown peer grant '{other}'")),
        }
    }
    Ok(tokens.join(","))
}

#[tauri::command]
pub async fn peers_list() -> Result<Value, String> {
    parse_json(&command(vec!["peers".into(), "list".into(), "--json".into()]).await?)
}

/// Offer another installation peer standing on this one. The invitation is
/// written to `output`; it is one-time and expires.
#[tauri::command]
pub async fn peers_invite(
    label: String,
    allow: Vec<String>,
    expires_minutes: u64,
    output: String,
) -> Result<Value, String> {
    validate_token("peer label", &label, 120)?;
    validate_output_path(&output)?;
    let grants = peer_grants(&allow)?;
    if grants.is_empty() {
        return Err(
            "A peer invitation must grant at least one of message, task or artifact".to_string(),
        );
    }
    if !(1..=24 * 60).contains(&expires_minutes) {
        return Err("Invitation expiry must be between 1 and 1440 minutes".to_string());
    }
    parse_json(
        &command(vec![
            "peers".into(),
            "invite".into(),
            label,
            "--allow".into(),
            grants,
            "--expires-minutes".into(),
            expires_minutes.to_string(),
            "--output".into(),
            output,
            "--json".into(),
        ])
        .await?,
    )
}

/// Take up another installation's invitation from a file the operator chose.
#[tauri::command]
pub async fn peers_accept(invitation: String, alias: String) -> Result<Value, String> {
    validate_id("peer alias", &alias)?;
    let path = Path::new(&invitation);
    if !path.is_absolute() || !path.is_file() {
        return Err("Choose the invitation file the other installation gave you".to_string());
    }
    parse_json(
        &command(vec![
            "peers".into(),
            "accept".into(),
            invitation,
            alias,
            "--json".into(),
        ])
        .await?,
    )
}

/// Replace what one inbound peer may ask for. An empty list leaves it paired
/// and unable to ask for anything.
#[tauri::command]
pub async fn peers_grant(device_id: String, allow: Vec<String>) -> Result<Value, String> {
    validate_id("device id", &device_id)?;
    let grants = peer_grants(&allow)?;
    parse_json(
        &command(vec![
            "peers".into(),
            "grant".into(),
            device_id,
            "--allow".into(),
            grants,
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn peers_revoke(device_id: String, reason: String) -> Result<(), String> {
    validate_id("device id", &device_id)?;
    validate_token("revoke reason", &reason, 1024)?;
    command(vec![
        "peers".into(),
        "revoke".into(),
        device_id,
        "--reason".into(),
        reason,
    ])
    .await
    .map(|_| ())
}

/// Threads inbound peers opened here, with their recent traffic.
#[tauri::command]
pub async fn peers_threads(peer: Option<String>, limit: u32) -> Result<Value, String> {
    let mut args = vec!["peers".into(), "threads".into()];
    if let Some(peer) = peer {
        validate_id("device id", &peer)?;
        args.push("--peer".into());
        args.push(peer);
    }
    args.push("--limit".into());
    args.push(limit.clamp(1, 200).to_string());
    args.push("--json".into());
    parse_json(&command(args).await?)
}

// --- Conversation ingress -------------------------------------------------

/// Turns that arrived from outside, across every origin, with the run each one
/// became.
///
/// One command rather than one per subsystem: the desktop asks the same
/// question about a Telegram message, an inbound call and a peer handover, and
/// the answer has the same shape. The listing carries identifiers, state and
/// failure reasons — never message text, and never a credential.
#[tauri::command]
pub async fn ingress_turns(source: Option<String>, limit: u32) -> Result<Value, String> {
    let mut args = vec!["ingress".into(), "list".into()];
    if let Some(source) = source {
        // Parsed against the enum rather than pattern-checked, so the only
        // strings that can become a process argument are the six the durable
        // contract defines.
        let source = crate::channels::ingress::ConversationSource::parse(&source)
            .ok_or_else(|| format!("Unknown conversation source '{source}'"))?;
        args.push("--source".into());
        args.push(source.as_str().to_string());
    }
    args.push("--limit".into());
    args.push(limit.clamp(1, 200).to_string());
    args.push("--json".into());
    parse_json(&command(args).await?)
}

/// The three arguments that name one turn, validated once.
///
/// The origin is parsed against the enum rather than pattern-checked, so the only
/// strings that reach an argument vector are the six the durable contract
/// defines; the account and event are the identity a surface submitted under, and
/// they are already constrained to that shape by the bridge that accepted them.
fn ingress_turn_args(
    action: &str,
    source: &str,
    account: &str,
    event: &str,
) -> Result<Vec<String>, String> {
    let source = crate::channels::ingress::ConversationSource::parse(source)
        .ok_or_else(|| format!("Unknown conversation source '{source}'"))?;
    validate_id("ingress account", account)?;
    validate_id("ingress event id", event)?;
    Ok(vec![
        "ingress".into(),
        action.into(),
        "--source".into(),
        source.as_str().to_string(),
        "--account".into(),
        account.to_string(),
        "--event".into(),
        event.to_string(),
        "--json".into(),
    ])
}

/// One turn and every continuation it produced.
///
/// What a desktop surface asks while it is watching a turn it submitted: an
/// unmet workspace-mutation contract is answered by a *continuation's* run, and
/// this is how the UI finds that run without ever executing anything itself.
#[tauri::command]
pub async fn ingress_turn_show(
    source: String,
    account: String,
    event: String,
) -> Result<Value, String> {
    let args = ingress_turn_args("show", &source, &account, &event)?;
    parse_json(&command(args).await?)
}

/// Continue an accepted turn that was frozen at a tool boundary.
///
/// The daemon inherits the accepted turn's frozen execution context verbatim, so
/// nothing about the machine's current configuration can change what the resumed
/// turn runs. A turn that was never accepted, or was accepted without a frozen
/// context, is refused there rather than reconstructed here.
///
/// `request_id` is the caller's identity for the Resume action itself, and this
/// command is a pure conduit for it: no id is minted here, so a caller that
/// retries after a lost response — which is what a timed-out `invoke` is —
/// reaches the same continuation instead of starting a second one.
#[tauri::command]
pub async fn ingress_turn_resume(
    source: String,
    account: String,
    event: String,
    request_id: String,
) -> Result<Value, String> {
    validate_id("ingress resume request id", &request_id)?;
    let mut args = ingress_turn_args("resume", &source, &account, &event)?;
    args.push("--request-id".into());
    args.push(request_id);
    parse_json(&command(args).await?)
}
// --- Telephony ------------------------------------------------------------
//
// The same arrangement as the messaging channel commands above: fixed-argument
// wrappers over `monkey telecom …`, so the rules live in one place and the
// desktop never gets an arbitrary command executor.
//
// `telecom_set_credential` is the only path a carrier secret takes across this
// boundary, and it goes straight to the keychain rather than through an
// argument vector.

#[tauri::command]
pub async fn telecom_list() -> Result<Value, String> {
    parse_json(&command(vec!["telecom".into(), "list".into(), "--json".into()]).await?)
}

#[tauri::command]
pub async fn telecom_add(
    kind: String,
    label: String,
    carrier_account_id: String,
    from_number: String,
    public_url: Option<String>,
    config: Option<String>,
) -> Result<Value, String> {
    validate_token("carrier", &kind, 32)?;
    validate_token("label", &label, 120)?;
    validate_token("carrier account id", &carrier_account_id, 128)?;
    validate_token("number", &from_number, 20)?;
    let mut args = vec![
        "telecom".into(),
        "add".into(),
        kind,
        label,
        carrier_account_id,
        from_number,
    ];
    if let Some(url) = public_url {
        validate_token("public URL", &url, 512)?;
        args.push("--public-url".into());
        args.push(url);
    }
    if let Some(config) = config {
        serde_json::from_str::<Value>(&config)
            .map_err(|error| format!("Carrier settings must be a JSON object: {error}"))?;
        args.push("--config".into());
        args.push(config);
    }
    args.push("--json".into());
    parse_json(&command(args).await?)
}

/// Probe a carrier account. The only path that can move one to `connected`.
#[tauri::command]
pub async fn telecom_probe(account_id: String) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "telecom".into(),
            "probe".into(),
            account_id,
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn telecom_enable(account_id: String, enabled: bool) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["telecom".into(), "enable".into(), account_id];
    if !enabled {
        args.push("--off".into());
    }
    command(args).await.map(|_| ())
}

#[tauri::command]
pub async fn telecom_set_policy(
    account_id: String,
    inbound: Option<String>,
    outbound: Option<String>,
) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["telecom".into(), "policy".into(), account_id];
    if let Some(value) = inbound {
        validate_token("inbound policy", &value, 16)?;
        args.push("--inbound".into());
        args.push(value);
    }
    if let Some(value) = outbound {
        validate_token("outbound approval", &value, 16)?;
        args.push("--outbound".into());
        args.push(value);
    }
    command(args).await.map(|_| ())
}

#[tauri::command]
pub async fn telecom_set_limits(
    account_id: String,
    max_concurrent: Option<u32>,
    ring_timeout_s: Option<u32>,
    max_duration_s: Option<u32>,
    recording: Option<bool>,
) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["telecom".into(), "limits".into(), account_id];
    for (flag, value) in [
        ("--max-concurrent", max_concurrent),
        ("--ring-timeout-s", ring_timeout_s),
        ("--max-duration-s", max_duration_s),
    ] {
        if let Some(value) = value {
            args.push(flag.into());
            args.push(value.to_string());
        }
    }
    if let Some(recording) = recording {
        args.push("--recording".into());
        args.push(recording.to_string());
    }
    command(args).await.map(|_| ())
}

/// What the number says when an answered call connects.
#[tauri::command]
pub async fn telecom_set_greeting(account_id: String, text: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    if text.chars().count() > 600 {
        return Err("That greeting is too long to say on a phone call".to_string());
    }
    command(vec!["telecom".into(), "greeting".into(), account_id, text])
        .await
        .map(|_| ())
}

#[tauri::command]
pub async fn telecom_calls(account_id: String, limit: u32) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "telecom".into(),
            "calls".into(),
            account_id,
            "--limit".into(),
            limit.clamp(1, 200).to_string(),
            "--json".into(),
        ])
        .await?,
    )
}

/// The URL the operator pastes into their carrier's console.
#[tauri::command]
pub async fn telecom_callback_url(account_id: String) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "telecom".into(),
            "callback-url".into(),
            account_id,
            "--json".into(),
        ])
        .await?,
    )
}

#[tauri::command]
pub async fn telecom_remove(account_id: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    command(vec!["telecom".into(), "remove".into(), account_id])
        .await
        .map(|_| ())
}

/// Store a carrier credential.
///
/// Writes the same keychain entry the daemon reads, named by the one definition
/// both sides share. The value is never echoed back, never logged and never
/// returned; the account row only ever learns that a credential exists.
#[tauri::command]
pub async fn telecom_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    if secret.is_empty() || secret.len() > 8192 {
        return Err("A carrier credential must contain 1-8192 bytes".to_string());
    }
    let reference = crate::channels::telecom_credential_ref(&account_id);
    keyring::Entry::new(&crate::channels::KEYCHAIN_SERVICE, &reference)
        .map_err(|error| format!("Failed to open the telephony keychain entry: {error}"))?
        .set_password(&secret)
        .map_err(|error| format!("Failed to save the carrier credential: {error}"))?;
    command(vec!["telecom".into(), "mark-credential".into(), account_id])
        .await
        .map(|_| ())
}

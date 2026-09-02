//! Fixed-argument desktop bridge for the bundled resident daemon.
//!
//! This module deliberately does not expose an arbitrary CLI executor. Every
//! command has a typed argument set, invokes the app-owned sidecar without a
//! shell, bounds output, and leaves the daemon as the single authoritative
//! engine/ledger owner.

use rusqlite::{params, TransactionBehavior};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;
use tauri::Emitter;

const MAX_CLI_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_REMOTE_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;
const MAX_REMOTE_RUN_SCOPES: usize = 1_024;
const MAX_REMOTE_WORKSPACE_SCOPES: usize = 128;
const MAX_RECIPE_SCHEDULES: usize = 1_024;
const MANAGED_RECIPE_TRIGGER_PREFIX: &str = "lm-managed-recipe-v1-";
const DAEMON_CHANGED_EVENT: &str = "daemon://changed";

fn execution_target_registry_path() -> Result<PathBuf, String> {
    crate::app_paths::data_dir()
        .map(|path| path.join("execution-targets.json"))
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())
}

#[tauri::command]
pub async fn execution_targets_list() -> Result<Value, String> {
    let registry =
        crate::execution_target::TargetRegistry::load(&execution_target_registry_path()?)
            .map_err(|error| error.to_string())?;
    serde_json::to_value(registry.targets).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_target_probe(id: String) -> Result<Value, String> {
    let path = execution_target_registry_path()?;
    let mut registry =
        crate::execution_target::TargetRegistry::load(&path).map_err(|error| error.to_string())?;
    let previous = registry
        .get(&id)
        .map_err(|error| error.to_string())?
        .identity()
        .clone();
    let target = registry
        .get(&id)
        .map_err(|error| error.to_string())?
        .target()
        .map_err(|error| error.to_string())?;
    let snapshot = target.probe().map_err(|error| error.to_string())?;
    if previous
        .verified_identity
        .as_ref()
        .zip(snapshot.identity.verified_identity.as_ref())
        .is_some_and(|(before, after)| before != after)
    {
        if let Some(config) = registry.targets.get_mut(&id) {
            match config {
                crate::execution_target::TargetConfig::Local { identity }
                | crate::execution_target::TargetConfig::Docker { identity, .. }
                | crate::execution_target::TargetConfig::RemoteNode { identity }
                | crate::execution_target::TargetConfig::SshRunner { identity, .. } => {
                    identity.trust_state = crate::execution_target::TargetTrustState::Changed;
                }
            }
        }
        registry.save(&path).map_err(|error| error.to_string())?;
        return Err(
            crate::execution_target::TargetError::TargetIdentityChanged(format!(
                "target '{id}' identity changed during probe"
            ))
            .to_string(),
        );
    }
    if let Some(config) = registry.targets.get_mut(&id) {
        match config {
            crate::execution_target::TargetConfig::Local { identity }
            | crate::execution_target::TargetConfig::Docker { identity, .. }
            | crate::execution_target::TargetConfig::RemoteNode { identity }
            | crate::execution_target::TargetConfig::SshRunner { identity, .. } => {
                *identity = snapshot.identity.clone()
            }
        }
    }
    registry.save(&path).map_err(|error| error.to_string())?;
    serde_json::to_value(snapshot).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_target_remove(id: String) -> Result<(), String> {
    let path = execution_target_registry_path()?;
    let mut registry =
        crate::execution_target::TargetRegistry::load(&path).map_err(|error| error.to_string())?;
    registry.remove(&id).map_err(|error| error.to_string())?;
    registry.save(&path).map_err(|error| error.to_string())
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionTargetAddRequest {
    pub id: String,
    pub kind: String,
    pub name: Option<String>,
    pub image: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub known_hosts: Option<String>,
    pub key_file: Option<String>,
    pub runner_data: Option<String>,
}

#[tauri::command]
pub async fn execution_target_add(request: ExecutionTargetAddRequest) -> Result<(), String> {
    validate_id("target id", &request.id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let runner_data = request
        .runner_data
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("execution-runner"));
    let display_name = request.name.unwrap_or_else(|| request.id.clone());
    let identity = |kind| crate::execution_target::TargetIdentity {
        stable_id: request.id.clone(),
        display_name: display_name.clone(),
        kind,
        endpoint: request.host.clone(),
        verified_identity: None,
        platform: "unknown".into(),
        runner_version: "unknown".into(),
        protocol_version: crate::execution_target::EXECUTION_PROTOCOL_VERSION,
        capabilities: match kind {
            crate::execution_target::ExecutionTargetKind::Docker => {
                crate::execution_target::TargetCapabilities::docker()
            }
            _ => crate::execution_target::TargetCapabilities::default(),
        },
        last_successful_probe_ms: None,
        trust_state: crate::execution_target::TargetTrustState::Unverified,
    };
    let config = match request.kind.replace('-', "_").as_str() {
        "docker" => crate::execution_target::TargetConfig::Docker {
            identity: identity(crate::execution_target::ExecutionTargetKind::Docker),
            image: request.image.ok_or("Docker target image is required")?,
            runner_data,
        },
        "ssh_runner" | "ssh" => crate::execution_target::TargetConfig::SshRunner {
            identity: identity(crate::execution_target::ExecutionTargetKind::SshRunner),
            config: crate::execution_target::SshRunnerConfig {
                host: request.host.ok_or("SSH target host is required")?,
                user: request.user,
                port: request.port,
                key_file: request.key_file.map(PathBuf::from),
                known_hosts: PathBuf::from(
                    request.known_hosts.ok_or("SSH known_hosts is required")?,
                ),
                jump_host: None,
                runner_binary: "monkey".into(),
            },
            runner_data,
        },
        other => return Err(format!("unsupported execution target kind '{other}'")),
    };
    let path = data_dir.join("execution-targets.json");
    let mut registry =
        crate::execution_target::TargetRegistry::load(&path).map_err(|error| error.to_string())?;
    registry.add(config).map_err(|error| error.to_string())?;
    registry.save(&path).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_workspace_push(
    workspace: String,
    workspace_id: Option<String>,
) -> Result<Value, String> {
    let path = PathBuf::from(workspace)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let id = workspace_id.unwrap_or_else(|| {
        format!(
            "workspace-{}",
            &format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()))[..24]
        )
    });
    let transfer = crate::execution_target::WorkspaceTransfer::from_workspace(&path, &id)
        .map_err(|error| error.to_string())?;
    serde_json::to_value(transfer).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_result_review(result_id: String) -> Result<Value, String> {
    validate_id("result id", &result_id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let result = crate::execution_target::load_execution_result(&data_dir, &result_id)
        .map_err(|error| error.to_string())?;
    serde_json::to_value(result).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_result_apply(result_id: String, workspace: String) -> Result<(), String> {
    validate_id("result id", &result_id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let workspace = PathBuf::from(workspace)
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let result = crate::execution_target::load_execution_result(&data_dir, &result_id)
        .map_err(|error| error.to_string())?;
    crate::execution_target::apply_execution_result(&workspace, &result)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_result_export(result_id: String, output: String) -> Result<(), String> {
    validate_id("result id", &result_id)?;
    validate_output_path(&output)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let result = crate::execution_target::load_execution_result(&data_dir, &result_id)
        .map_err(|error| error.to_string())?;
    let bytes = serde_json::to_vec_pretty(&result).map_err(|error| error.to_string())?;
    std::fs::write(output, bytes).map_err(|error| error.to_string())
}

#[tauri::command]
pub async fn execution_result_discard(result_id: String) -> Result<(), String> {
    validate_id("result id", &result_id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    crate::execution_target::discard_workspace_result(&data_dir, &result_id)
        .map_err(|error| error.to_string())
}

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
    finish_cli_output(output)
}

fn finish_cli_output(output: std::process::Output) -> Result<String, String> {
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

fn run_cli_with_secret(args: Vec<String>, secret: String) -> Result<String, String> {
    let output = Command::new(cli_path())
        .args(&args)
        .env("LM_EXTENSION_WEBHOOK_SECRET", secret)
        .output()
        .map_err(|error| format!("Failed to start bundled monkey-cli: {error}"))?;
    finish_cli_output(output)
}

/// Run the sidecar with a secret on its stdin.
///
/// The writer's identity is the point, not just the transport. macOS admits a
/// keychain item to the executable that created it and asks a human about
/// anybody else, so a credential the desktop wrote in its own process is one
/// the installed daemon can only read behind a confirmation dialog — which,
/// for a background LaunchAgent, means a read that never returns. The bundled
/// sidecar *is* the daemon's executable, so writing through it makes the writer
/// and the reader one identity and the daemon's read unattended. It stays off
/// the argument vector for the original reason: argv is world-readable.
/// Taking the program as a parameter keeps the pipe plumbing testable
/// without the bundled binary.
fn run_cli_with_stdin(
    program: PathBuf,
    args: Vec<String>,
    secret: String,
) -> Result<String, String> {
    use std::io::Write;
    let mut child = Command::new(program)
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start bundled monkey-cli: {error}"))?;
    // Taken and dropped here, so the child sees end-of-input even if it reads
    // past the newline.
    child
        .stdin
        .take()
        .ok_or_else(|| "Bundled monkey-cli did not accept a credential".to_string())?
        .write_all(format!("{secret}\n").as_bytes())
        .map_err(|error| format!("Failed to hand the credential to monkey-cli: {error}"))?;
    let output = child
        .wait_with_output()
        .map_err(|error| format!("Failed to run bundled monkey-cli: {error}"))?;
    finish_cli_output(output)
}

pub(crate) async fn command_with_stdin(
    args: Vec<String>,
    secret: String,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || run_cli_with_stdin(cli_path(), args, secret))
        .await
        .map_err(|error| error.to_string())?
}

/// The same bounds the sidecar's own store enforces, checked before a secret
/// leaves the app so the failure names the field the user is looking at.
///
/// No line-break rule: the sidecar reads its stdin to EOF, and a Google Chat
/// account's credential is a pasted service-account key file.
fn bounded_secret(label: &str, secret: &str) -> Result<(), String> {
    if secret.is_empty() || secret.len() > 8192 {
        return Err(format!("A {label} must contain 1-8192 bytes"));
    }
    Ok(())
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

fn consume_autonomous_placement_boundary(
    run_spec: &mut crate::run_protocol::RunSpec,
    placement_kind: &str,
) -> Result<(), String> {
    let nodes = run_spec
        .autonomous_task
        .as_mut()
        .and_then(|value| value.get_mut("task_snapshot"))
        .and_then(|value| value.get_mut("plan"))
        .and_then(|value| value.get_mut("nodes"))
        .and_then(Value::as_array_mut)
        .ok_or_else(|| "autonomous placement spec omitted its frozen node plan".to_string())?;
    let mut consumed = false;
    for node in &mut *nodes {
        let Some(object) = node.as_object_mut() else {
            return Err("autonomous placement spec contains a non-object node".to_string());
        };
        let current = object
            .get("executionPlacement")
            .or_else(|| object.get("execution_placement"))
            .cloned();
        let Some(current) = current else { continue };
        let kind = current
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if kind != placement_kind {
            continue;
        }
        if object.get("placementFulfilled").and_then(Value::as_bool) == Some(true)
            || current.get("placementFulfilled").and_then(Value::as_bool) == Some(true)
        {
            return Err(
                "autonomous placement was already fulfilled; refusing a second placement hop"
                    .to_string(),
            );
        }
        let node_id = current
            .get("nodeId")
            .or_else(|| current.get("node_id"))
            .and_then(Value::as_str)
            .or_else(|| object.get("nodeId").and_then(Value::as_str))
            .unwrap_or("placed-node")
            .to_string();
        let isolation = if placement_kind == "docker" {
            object.insert("isolation".to_string(), Value::String("shared".to_string()));
            "shared".to_string()
        } else {
            object
                .get("isolation")
                .and_then(Value::as_str)
                .unwrap_or("shared")
                .to_string()
        };
        {
            let requirements = if object.contains_key("executionRequirements") {
                object.get_mut("executionRequirements")
            } else {
                object.get_mut("execution_requirements")
            };
            if let Some(requirements) = requirements.and_then(Value::as_object_mut) {
                requirements.insert("isolation".to_string(), Value::String(isolation.clone()));
            }
        }
        object.insert("requestedExecutionPlacement".to_string(), current);
        object.insert("placementFulfilled".to_string(), Value::Bool(true));
        object.insert(
            "executionPlacement".to_string(),
            serde_json::json!({
                "kind": "local",
                "targetId": "local",
                "nodeId": node_id,
                "reason": format!("already fulfilled by {placement_kind} placement executor"),
                "placementFulfilled": true
            }),
        );
        consumed = true;
    }
    if consumed {
        return Ok(());
    }
    let already_consumed = nodes.iter().all(|node| {
        let Some(object) = node.as_object() else {
            return false;
        };
        if object.get("placementFulfilled").and_then(Value::as_bool) != Some(true)
            && object
                .get("executionPlacement")
                .and_then(|placement| placement.get("placementFulfilled"))
                .and_then(Value::as_bool)
                != Some(true)
        {
            return false;
        }
        object
            .get("requestedExecutionPlacement")
            .or_else(|| object.get("requested_placement"))
            .or_else(|| {
                object
                    .get("executionPlacement")
                    .and_then(|placement| placement.get("requestedPlacement"))
            })
            .and_then(|placement| placement.get("kind"))
            .and_then(Value::as_str)
            == Some(placement_kind)
    });
    if already_consumed {
        Ok(())
    } else {
        Err(format!(
            "autonomous placement spec contains no {placement_kind} node"
        ))
    }
}

fn autonomous_execution_target_lost(error: impl std::fmt::Display) -> String {
    format!("EXECUTION_TARGET_LOST: {error}")
}

fn normalize_autonomous_placement_result(mut result: Value) -> Value {
    if let Some(object) = result.as_object_mut() {
        let target_lost = object
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status == "execution_target_lost")
            || object
                .get("failureCode")
                .and_then(Value::as_str)
                .is_some_and(|code| code == "EXECUTION_TARGET_LOST")
            || object
                .get("failureKind")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "EXECUTION_TARGET_LOST")
            || object
                .get("final_message")
                .or_else(|| object.get("finalMessage"))
                .and_then(Value::as_str)
                .is_some_and(|message| message.trim_start().starts_with("EXECUTION_TARGET_LOST:"));
        if target_lost {
            object.insert(
                "failureCode".to_string(),
                Value::String("EXECUTION_TARGET_LOST".to_string()),
            );
            object.insert(
                "failureKind".to_string(),
                Value::String("EXECUTION_TARGET_LOST".to_string()),
            );
            if !object.contains_key("summary") {
                if let Some(message) = object
                    .get("final_message")
                    .or_else(|| object.get("finalMessage"))
                    .cloned()
                {
                    object.insert("summary".to_string(), message);
                }
            }
        }
        let ok = object
            .get("ok")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| object.get("status").and_then(Value::as_str) == Some("ok"));
        object.insert("ok".to_string(), Value::Bool(ok));
    }
    result
}

fn parse_typed_json<T: DeserializeOwned>(output: &str) -> Result<T, String> {
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

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutonomousTaskPlacementRequest {
    /// Deprecated compatibility field. Routing is always resolved from the
    /// stable target id in the registry; callers cannot select an executor by
    /// spelling a backend kind.
    #[serde(default)]
    pub kind: Option<String>,
    pub target_id: String,
    pub run_spec: crate::run_protocol::RunSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutonomousPlacementRecord {
    run_id: String,
    target_id: String,
    handle: crate::execution_target::TargetRunHandle,
    workspace: crate::execution_target::WorkspaceHandle,
    created_at_ms: u64,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    paused: bool,
}

fn autonomous_placement_record_path(data_dir: &Path, run_id: &str) -> Result<PathBuf, String> {
    validate_id("placement run id", run_id)?;
    Ok(data_dir
        .join("execution-placements")
        .join(format!("{run_id}.json")))
}

fn save_autonomous_placement_record(
    data_dir: &Path,
    record: &AutonomousPlacementRecord,
) -> Result<(), String> {
    let path = autonomous_placement_record_path(data_dir, &record.run_id)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(record).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    std::fs::rename(temporary, path).map_err(|error| error.to_string())
}

fn load_autonomous_placement_record(
    data_dir: &Path,
    run_id: &str,
) -> Result<Option<AutonomousPlacementRecord>, String> {
    let path = autonomous_placement_record_path(data_dir, run_id)?;
    if !path.exists() {
        return Ok(None);
    }
    serde_json::from_slice(&std::fs::read(path).map_err(|error| error.to_string())?)
        .map(Some)
        .map_err(|error| error.to_string())
}

fn delete_autonomous_placement_record(data_dir: &Path, run_id: &str) -> Result<(), String> {
    let path = autonomous_placement_record_path(data_dir, run_id)?;
    if path.exists() {
        std::fs::remove_file(path).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn resolve_execution_target_kind(
    target_id: &str,
    requested_kind: Option<&str>,
) -> Result<String, String> {
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let registry =
        crate::execution_target::TargetRegistry::load(&data_dir.join("execution-targets.json"))
            .map_err(|error| error.to_string())?;
    if let Ok(config) = registry.get(target_id) {
        return Ok(match config.identity().kind {
            crate::execution_target::ExecutionTargetKind::Local => "local",
            crate::execution_target::ExecutionTargetKind::Docker => "docker",
            crate::execution_target::ExecutionTargetKind::RemoteNode => "remote_node",
            crate::execution_target::ExecutionTargetKind::SshRunner => "ssh_runner",
        }
        .to_string());
    }
    // K17 aliases are owned by the paired-node registry. Keep the legacy
    // alias as a lookup hint only; execution still happens in the target
    // adapter below, never in the frontend.
    match requested_kind.map(|kind| kind.replace('-', "_")).as_deref() {
        Some("remote_node") => Ok("remote_node".to_string()),
        Some("docker") => Ok("docker".to_string()),
        Some("ssh_runner") => Ok("ssh_runner".to_string()),
        Some("local") | None => Err(format!("unknown execution target '{target_id}'")),
        Some(other) => Err(format!("unknown execution target kind '{other}'")),
    }
}

#[tauri::command]
pub async fn autonomous_task_recover_node(
    target_id: String,
    run_id: String,
) -> Result<Value, String> {
    validate_id("placement target", &target_id)?;
    validate_id("placement run id", &run_id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let Some(record) = load_autonomous_placement_record(&data_dir, &run_id)? else {
        return Ok(serde_json::json!({"known": false}));
    };
    if record.target_id != target_id {
        return Err("placement record target does not match the requested target".to_string());
    }
    if record.cancelled {
        delete_autonomous_placement_record(&data_dir, &run_id)?;
        return Ok(serde_json::json!({
            "known": true,
            "pending": false,
            "ok": false,
            "failureCode": "RUN_CANCELLED",
            "failureKind": "RUN_CANCELLED",
            "summary": "Recovered remote placement was cancelled by the operator"
        }));
    }
    let registry =
        crate::execution_target::TargetRegistry::load(&data_dir.join("execution-targets.json"))
            .map_err(|error| error.to_string())?;
    let target = registry
        .get(&target_id)
        .map_err(|error| error.to_string())?
        .target()
        .map_err(|error| error.to_string())?;
    let status = target
        .status(&record.handle)
        .map_err(|error| error.to_string())?;
    if matches!(
        status,
        crate::execution_target::TargetRunStatus::Queued
            | crate::execution_target::TargetRunStatus::Running
    ) {
        return Ok(serde_json::json!({
            "known": true,
            "pending": true,
            "status": status,
            "remoteRunId": record.handle.remote_id
        }));
    }
    if status != crate::execution_target::TargetRunStatus::Succeeded {
        let (failure_code, failure_kind) = match status {
            crate::execution_target::TargetRunStatus::Failed => ("RUNNER_FAILED", "RUNNER_FAILED"),
            crate::execution_target::TargetRunStatus::Cancelled => {
                ("RUN_CANCELLED", "RUN_CANCELLED")
            }
            _ => ("RUNNER_LOST", "RUNNER_LOST"),
        };
        return Ok(serde_json::json!({
            "known": true,
            "pending": false,
            "ok": false,
            "failureCode": failure_code,
            "failureKind": failure_kind,
            "summary": format!("Recovered remote placement ended with status {status:?}")
        }));
    }
    let result = target
        .workspace_result(&record.handle)
        .map_err(|error| error.to_string())?;
    let result_id = crate::execution_target::persist_workspace_result(&data_dir, &result)
        .map_err(|error| error.to_string())?;
    target
        .cleanup(&record.workspace)
        .map_err(|error| error.to_string())?;
    delete_autonomous_placement_record(&data_dir, &run_id)?;
    Ok(serde_json::json!({
        "known": true,
        "pending": false,
        "ok": true,
        "reviewRequired": true,
        "summary": "Recovered remote placement; review the persisted workspace result before applying it",
        "resultId": result_id,
        "changedFiles": result.new_files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
        "deletedFiles": result.deleted_files,
    }))
}

#[tauri::command]
pub async fn autonomous_task_control_node(
    target_id: String,
    run_id: String,
    action: String,
) -> Result<Value, String> {
    validate_id("placement target", &target_id)?;
    validate_id("placement run id", &run_id)?;
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let Some(mut record) = load_autonomous_placement_record(&data_dir, &run_id)? else {
        return Ok(serde_json::json!({"known": false}));
    };
    if record.target_id != target_id {
        return Err("placement record target does not match the requested target".to_string());
    }
    let registry =
        crate::execution_target::TargetRegistry::load(&data_dir.join("execution-targets.json"))
            .map_err(|error| error.to_string())?;
    let target = registry
        .get(&target_id)
        .map_err(|error| error.to_string())?
        .target()
        .map_err(|error| error.to_string())?;
    match action.as_str() {
        "cancel" => {
            target
                .cancel(&record.handle)
                .map_err(|error| error.to_string())?;
            record.cancelled = true;
            record.paused = false;
        }
        "pause" => {
            target
                .pause(&record.handle)
                .map_err(|error| error.to_string())?;
            record.paused = true;
        }
        "resume" => {
            target
                .resume(&record.handle)
                .map_err(|error| error.to_string())?;
            record.paused = false;
        }
        _ => return Err("placement control action must be cancel, pause, or resume".to_string()),
    }
    save_autonomous_placement_record(&data_dir, &record)?;
    Ok(serde_json::json!({
        "known": true,
        "action": action,
        "remoteRunId": record.handle.remote_id,
        "cancelled": record.cancelled,
        "paused": record.paused
    }))
}

fn configured_docker_target(
    data_dir: &Path,
    target_id: &str,
) -> Result<(String, Box<dyn crate::execution_target::ExecutionTarget>), String> {
    let registry =
        crate::execution_target::TargetRegistry::load(&data_dir.join("execution-targets.json"))
            .map_err(|error| error.to_string())?;
    if let Ok(config) = registry.get(target_id) {
        let image = match config {
            crate::execution_target::TargetConfig::Docker { image, .. } => image.clone(),
            _ => return Err(format!("execution target '{target_id}' is not Docker")),
        };
        return config
            .target()
            .map(|target| (image, target))
            .map_err(|error| error.to_string());
    }
    // Keep direct legacy callers working, but configured target IDs always
    // resolve through the registry and therefore cannot be mistaken for an
    // image name.
    let target = crate::execution_target::DockerExecutionTarget::new(
        format!(
            "docker-{}",
            &format!("{:x}", Sha256::digest(target_id.as_bytes()))[..24]
        ),
        format!("Docker {target_id}"),
        target_id.to_string(),
        data_dir.join("execution-runner"),
    )
    .map_err(|error| error.to_string())?;
    Ok((target_id.to_string(), Box::new(target)))
}

fn autonomous_runner_result_from_events(
    events: &[crate::execution_target::TargetEvent],
) -> Option<Value> {
    events.iter().rev().find_map(|event| {
        event
            .message
            .lines()
            .rev()
            .chain(std::iter::once(event.message.as_str()))
            .find_map(|line| {
                serde_json::from_str::<Value>(line.trim())
                    .ok()
                    .filter(Value::is_object)
            })
    })
}

async fn execute_registered_target_placement(
    request: AutonomousTaskPlacementRequest,
) -> Result<Value, String> {
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let registry_path = data_dir.join("execution-targets.json");
    let mut registry = crate::execution_target::TargetRegistry::load(&registry_path)
        .map_err(|error| error.to_string())?;
    let configured = registry.targets.get(&request.target_id).cloned();
    let (target, previous_identity) = if let Some(config) = configured.as_ref() {
        (
            config.target().map_err(|error| error.to_string())?,
            Some(config.identity().clone()),
        )
    } else if request
        .kind
        .as_deref()
        .is_some_and(|kind| matches!(kind.replace('-', "_").as_str(), "docker"))
    {
        let (_, target) = configured_docker_target(&data_dir, &request.target_id)?;
        (target, None)
    } else if request
        .kind
        .as_deref()
        .is_some_and(|kind| matches!(kind.replace('-', "_").as_str(), "remote_node"))
    {
        validate_id("remote node alias", &request.target_id)?;
        let identity = crate::execution_target::TargetIdentity {
            stable_id: request.target_id.clone(),
            display_name: request.target_id.clone(),
            kind: crate::execution_target::ExecutionTargetKind::RemoteNode,
            endpoint: None,
            verified_identity: None,
            platform: "remote-node".to_string(),
            runner_version: "k17".to_string(),
            protocol_version: crate::execution_target::EXECUTION_PROTOCOL_VERSION,
            capabilities: crate::execution_target::TargetCapabilities::default(),
            last_successful_probe_ms: None,
            trust_state: crate::execution_target::TargetTrustState::Unverified,
        };
        let snapshot = crate::execution_target::ExecutionTargetSnapshot::freeze(
            identity,
            crate::execution_target::execution_now_ms(),
        )
        .map_err(|error| error.to_string())?;
        let target = crate::execution_target::RemoteNodeTarget::from_snapshot(snapshot)
            .map_err(|error| error.to_string())?;
        let target: Box<dyn crate::execution_target::ExecutionTarget> = Box::new(target);
        (target, None)
    } else {
        return Err(format!("unknown execution target '{}'", request.target_id));
    };

    if previous_identity.as_ref().is_some_and(|identity| {
        matches!(
            identity.trust_state,
            crate::execution_target::TargetTrustState::Changed
                | crate::execution_target::TargetTrustState::Revoked
        )
    }) {
        return Err(
            crate::execution_target::TargetError::TargetIdentityChanged(format!(
                "execution target '{}' is not trusted",
                request.target_id
            ))
            .to_string(),
        );
    }
    let snapshot = target.probe().map_err(|error| error.to_string())?;
    if let Some(previous) = previous_identity.as_ref() {
        if previous
            .verified_identity
            .as_ref()
            .zip(snapshot.identity.verified_identity.as_ref())
            .is_some_and(|(before, after)| before != after)
        {
            if let Some(config) = registry.targets.get_mut(&request.target_id) {
                config.identity_mut().trust_state =
                    crate::execution_target::TargetTrustState::Changed;
            }
            registry
                .save(&registry_path)
                .map_err(|error| error.to_string())?;
            return Err(
                crate::execution_target::TargetError::TargetIdentityChanged(format!(
                    "execution target '{}' identity changed",
                    request.target_id
                ))
                .to_string(),
            );
        }
    }
    let placement_kind = match snapshot.identity.kind {
        crate::execution_target::ExecutionTargetKind::Docker => "docker",
        crate::execution_target::ExecutionTargetKind::RemoteNode => "remote_node",
        crate::execution_target::ExecutionTargetKind::SshRunner => "ssh_runner",
        _ => {
            return Err(format!(
                "execution target '{}' is not a supported autonomous runner",
                request.target_id
            ))
        }
    };
    if let Some(config) = registry.targets.get_mut(&request.target_id) {
        *config.identity_mut() = snapshot.identity.clone();
        registry
            .save(&registry_path)
            .map_err(|error| error.to_string())?;
    } else if snapshot.identity.kind == crate::execution_target::ExecutionTargetKind::RemoteNode {
        registry
            .add(crate::execution_target::TargetConfig::RemoteNode {
                identity: snapshot.identity.clone(),
            })
            .map_err(|error| error.to_string())?;
        registry
            .save(&registry_path)
            .map_err(|error| error.to_string())?;
    }

    let mut spec = request.run_spec;
    consume_autonomous_placement_boundary(&mut spec, placement_kind)?;
    let (workspace_id, host_workspace) = {
        let workspace = spec.workspace.as_ref().ok_or_else(|| {
            format!("{placement_kind} autonomous placement requires exactly one workspace root")
        })?;
        crate::agent_worktrees::validate_docker_workspace_root_count(workspace.roots.len())
            .map_err(|error| error.to_string())?;
        (
            workspace.workspace_id.clone(),
            PathBuf::from(workspace.roots[0].canonical_path.clone()),
        )
    };
    let mut transfer = spec.workspace_transfer.clone().unwrap_or(
        crate::execution_target::WorkspaceTransfer::from_workspace(&host_workspace, &workspace_id)
            .map_err(|error| format!("{placement_kind} workspace transfer failed: {error}"))?,
    );
    // All nodes of one autonomous task share a task-scoped executor workspace.
    // This lets verification/review observe changes from earlier remote nodes
    // without ever applying those changes to the user's checkout.
    let task_scope = spec
        .autonomous_task
        .as_ref()
        .and_then(|value| value.get("task_snapshot"))
        .and_then(|value| value.get("taskId"))
        .and_then(Value::as_str)
        .unwrap_or(&spec.run_id);
    let task_scope_digest = format!("{:x}", Sha256::digest(task_scope.as_bytes()));
    transfer.snapshot_id = format!("task-{}", &task_scope_digest[..24]);
    transfer.policy = crate::execution_target::WorkspacePolicy::Persistent;
    let requires_git = !matches!(
        transfer.kind,
        crate::execution_target::WorkspaceTransferKind::ContentSnapshot
    );
    spec.workspace_transfer = Some(transfer.clone());
    spec.execution_target = Some(snapshot.clone());
    snapshot
        .require(&crate::execution_target::RequiredCapabilities {
            shell: true,
            git: requires_git,
            disposable_workspace: true,
            ..Default::default()
        })
        .map_err(|error| error.to_string())?;
    spec.workspace
        .as_mut()
        .expect("workspace validated above")
        .roots[0]
        .canonical_path = "/workspace".to_string();
    let recipe =
        crate::recipes::placed_recipe_from_spec(&spec, format!("placed-{placement_kind}-node"))?;
    let recipe_bytes = serde_json::to_vec(&recipe)
        .map_err(|error| format!("Could not serialize placement recipe: {error}"))?;
    let execution_spec_digest = format!("{:x}", Sha256::digest(spec.run_id.as_bytes()));
    let execution_spec_path = format!(
        ".little-monkey/execution-spec-{}.json",
        &execution_spec_digest[..24]
    );
    let input_file = crate::execution_target::WorkspaceResultFile {
        path: execution_spec_path.clone(),
        sha256: format!("{:x}", Sha256::digest(&recipe_bytes)),
        bytes: recipe_bytes,
        executable: false,
        file_type: "file".to_string(),
        symlink_target: None,
    };
    let workspace_handle = target
        .prepare_workspace(
            &transfer,
            crate::execution_target::WorkspacePolicy::Persistent,
        )
        .map_err(|error| error.to_string())?;
    let run_request = crate::execution_target::RunRequest {
        run_id: spec.run_id.clone(),
        target: snapshot,
        required_capabilities: crate::execution_target::RequiredCapabilities {
            shell: true,
            git: requires_git,
            disposable_workspace: true,
            ..Default::default()
        },
        workspace: workspace_handle.clone(),
        command: vec![
            "task".to_string(),
            "run".to_string(),
            execution_spec_path,
            "--json".to_string(),
        ],
        environment: BTreeMap::new(),
        wall_time_ms: spec.budgets.wall_time_ms,
        max_artifact_bytes: spec.budgets.max_artifact_bytes,
        workspace_transfer: Some(transfer),
        input_files: vec![input_file],
        run_spec: Some(spec.clone()),
    };
    let handle = target
        .submit_run(run_request)
        .map_err(|error| error.to_string())?;
    save_autonomous_placement_record(
        &data_dir,
        &AutonomousPlacementRecord {
            run_id: spec.run_id.clone(),
            target_id: request.target_id.clone(),
            handle: handle.clone(),
            workspace: workspace_handle.clone(),
            created_at_ms: crate::execution_target::execution_now_ms(),
            cancelled: false,
            paused: false,
        },
    )?;
    let mut deadline =
        Instant::now() + std::time::Duration::from_millis(spec.budgets.wall_time_ms.max(1));
    let status = loop {
        if let Some(control) = load_autonomous_placement_record(&data_dir, &spec.run_id)? {
            if control.cancelled {
                let _ = target.cleanup(&workspace_handle);
                delete_autonomous_placement_record(&data_dir, &spec.run_id)?;
                return Err(
                    "RUN_CANCELLED: autonomous placement was cancelled by the operator".into(),
                );
            }
            if control.paused {
                // A user pause suspends wall-time accounting as well as the
                // executor process so a long pause cannot become a timeout.
                deadline += std::time::Duration::from_millis(500);
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                continue;
            }
        }
        let status = target.status(&handle).map_err(|error| error.to_string())?;
        if matches!(
            status,
            crate::execution_target::TargetRunStatus::Succeeded
                | crate::execution_target::TargetRunStatus::Failed
                | crate::execution_target::TargetRunStatus::Cancelled
                | crate::execution_target::TargetRunStatus::Lost
        ) {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = target.cancel(&handle);
            let _ = target.cleanup(&workspace_handle);
            delete_autonomous_placement_record(&data_dir, &spec.run_id)?;
            return Err("RUNNER_LOST: autonomous placement exceeded its wall-time budget".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    };
    if status != crate::execution_target::TargetRunStatus::Succeeded {
        let detail = target
            .events(&handle, 0)
            .ok()
            .and_then(|events| events.into_iter().last())
            .map(|event| event.message.trim().chars().take(2_048).collect::<String>())
            .filter(|message| !message.is_empty());
        let _ = target.cleanup(&workspace_handle);
        delete_autonomous_placement_record(&data_dir, &spec.run_id)?;
        let code = match status {
            crate::execution_target::TargetRunStatus::Failed => "RUNNER_FAILED",
            crate::execution_target::TargetRunStatus::Cancelled => "RUN_CANCELLED",
            _ => "RUNNER_LOST",
        };
        return Err(format!(
            "{code}: {placement_kind} runner finished with status {status:?}{}",
            detail
                .map(|message| format!("; output: {message}"))
                .unwrap_or_default()
        ));
    }
    let runner_events = target.events(&handle, 0).ok();
    let runner_result = runner_events
        .as_deref()
        .and_then(autonomous_runner_result_from_events);
    let result = target
        .workspace_result(&handle)
        .map_err(|error| error.to_string())?;
    let result_id = crate::execution_target::persist_workspace_result(&data_dir, &result)
        .map_err(|error| error.to_string())?;
    target
        .cleanup(&workspace_handle)
        .map_err(|error| error.to_string())?;
    delete_autonomous_placement_record(&data_dir, &spec.run_id)?;
    let mut response = serde_json::json!({
        "ok": true,
        "reviewRequired": true,
        "summary": format!("{placement_kind} runner completed; review the persisted workspace result before applying it"),
        "resultId": result_id,
        "changedFiles": result.new_files.iter().map(|file| file.path.clone()).collect::<Vec<_>>(),
        "deletedFiles": result.deleted_files,
    });
    if let Some(runner_result) = runner_result {
        if let Some(object) = response.as_object_mut() {
            for field in ["evidence", "review"] {
                if let Some(value) = runner_result.get(field) {
                    object.insert(field.to_string(), value.clone());
                }
            }
        }
    }
    Ok(response)
}

/// Execute a frozen autonomous node through the selected execution-target contract.
/// Backend-specific transport and workspace behavior belongs to each
/// `ExecutionTarget`; autonomous orchestration stays target-neutral.
#[tauri::command]
pub async fn autonomous_task_place_node(
    request: AutonomousTaskPlacementRequest,
) -> Result<Value, String> {
    request
        .run_spec
        .validate()
        .map_err(|error| error.to_string())?;
    validate_token("placement target", &request.target_id, 512)?;
    if request.target_id.starts_with('-') {
        return Err("Placement target cannot start with '-'".to_string());
    }
    resolve_execution_target_kind(&request.target_id, request.kind.as_deref())?;
    execute_registered_target_placement(request).await
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

/// Fixed, pre-authorized device bridge for the Wasm permission broker. The
/// extension host has already intersected the exact manifest grant with the
/// invocation's artifact set; this function retains the daemon's authoritative
/// paired-device capability intersection and argument normalization.
pub(crate) async fn extension_device_action(
    device_id: &str,
    action: &str,
    request: &Value,
    invocation_id: &str,
) -> Result<Value, String> {
    validate_id("device id", device_id)?;
    validate_id("extension invocation id", invocation_id)?;
    if !matches!(
        action,
        "device_info"
            | "camera_capture"
            | "microphone_capture"
            | "location_read"
            | "notification_post"
            | "screen_capture"
            | "audio_playback"
    ) {
        return Err("Unsupported device capability".to_string());
    }
    let object = request
        .as_object()
        .ok_or_else(|| "Device request must be a JSON object".to_string())?;
    let allowed = [
        "position",
        "duration_ms",
        "accuracy",
        "title",
        "body",
        "text",
        "artifact_id",
        "wait_ms",
    ];
    if let Some(field) = object
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(format!("Unknown device request field '{field}'"));
    }

    let mut args = vec![
        "daemon".into(),
        "remote".into(),
        "device-action".into(),
        action.into(),
        "--device-id".into(),
        device_id.into(),
        "--run-id".into(),
        invocation_id.into(),
        "--json".into(),
    ];
    for (field, flag, max) in [
        ("position", "--position", 16usize),
        ("accuracy", "--accuracy", 16usize),
        ("title", "--title", 128usize),
        ("body", "--body", 512usize),
        ("text", "--text", 4_096usize),
    ] {
        if let Some(value) = object.get(field).and_then(Value::as_str) {
            validate_token(field, value, max)?;
            args.extend([flag.into(), value.into()]);
        }
    }
    if let Some(value) = object.get("artifact_id").and_then(Value::as_str) {
        validate_id("artifact id", value)?;
        args.extend(["--artifact-id".into(), value.into()]);
    }
    if let Some(value) = object.get("duration_ms").and_then(Value::as_u64) {
        if value == 0 || value > 300_000 {
            return Err("Device duration_ms is out of range".to_string());
        }
        args.extend(["--duration-ms".into(), value.to_string()]);
    }
    let wait_ms = object
        .get("wait_ms")
        .and_then(Value::as_u64)
        .unwrap_or(10_000);
    if !(1_000..=20_000).contains(&wait_ms) {
        return Err("Device wait_ms must be 1000..=20000".to_string());
    }
    args.extend(["--wait-ms".into(), wait_ms.to_string()]);
    parse_json(&command(args).await?)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(crate) struct ExtensionWebhookStatus {
    pub trigger_id: String,
    pub handler_id: String,
    pub version: String,
    pub enabled: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn extension_webhook_register(
    trigger_id: &str,
    extension_id: &str,
    handler_id: &str,
    version: &str,
    manifest_sha256: &str,
    secret: String,
    max_skew_ms: u64,
) -> Result<(), String> {
    validate_id("trigger id", trigger_id)?;
    validate_id("extension id", extension_id)?;
    validate_id("handler id", handler_id)?;
    validate_id("extension version", version)?;
    validate_id("extension manifest digest", manifest_sha256)?;
    if secret.is_empty() || secret.len() > 64 * 1024 {
        return Err("Webhook secret must contain 1-65536 bytes".to_string());
    }
    if !(1_000..=60 * 60 * 1_000).contains(&max_skew_ms) {
        return Err("Webhook signature skew must be 1000..=3600000 ms".to_string());
    }
    let args = vec![
        "daemon".into(),
        "trigger".into(),
        "add-webhook".into(),
        trigger_id.into(),
        "--extension-id".into(),
        extension_id.into(),
        "--extension-handler-id".into(),
        handler_id.into(),
        "--extension-version".into(),
        version.into(),
        "--extension-manifest-sha256".into(),
        manifest_sha256.into(),
        "--secret-env".into(),
        "LM_EXTENSION_WEBHOOK_SECRET".into(),
        "--max-skew-ms".into(),
        max_skew_ms.to_string(),
    ];
    tokio::task::spawn_blocking(move || run_cli_with_secret(args, secret))
        .await
        .map_err(|error| error.to_string())??;
    Ok(())
}

pub(crate) async fn extension_webhook_remove(
    trigger_id: &str,
    extension_id: &str,
) -> Result<(), String> {
    validate_id("trigger id", trigger_id)?;
    validate_id("extension id", extension_id)?;
    command(vec![
        "daemon".into(),
        "trigger".into(),
        "remove".into(),
        trigger_id.into(),
        "--extension-id".into(),
        extension_id.into(),
    ])
    .await?;
    Ok(())
}

pub(crate) async fn extension_webhooks(
    extension_id: &str,
) -> Result<Vec<ExtensionWebhookStatus>, String> {
    validate_id("extension id", extension_id)?;
    let value = parse_json(
        &command(vec![
            "daemon".into(),
            "trigger".into(),
            "list".into(),
            "--json".into(),
        ])
        .await?,
    )?;
    let rows = value
        .as_array()
        .ok_or_else(|| "Daemon trigger list is not an array".to_string())?;
    let mut result = Vec::new();
    for row in rows {
        let Some(config) = row.get("config") else {
            continue;
        };
        let Some(target) = config.get("target") else {
            continue;
        };
        if target.get("target_kind").and_then(Value::as_str) != Some("extension")
            || target.get("extension_id").and_then(Value::as_str) != Some(extension_id)
        {
            continue;
        }
        let trigger_id = row
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Extension trigger id is missing".to_string())?;
        let handler_id = target
            .get("handler_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Extension trigger handler is missing".to_string())?;
        let version = target
            .get("version")
            .and_then(Value::as_str)
            .ok_or_else(|| "Extension trigger version is missing".to_string())?;
        result.push(ExtensionWebhookStatus {
            trigger_id: trigger_id.to_string(),
            handler_id: handler_id.to_string(),
            version: version.to_string(),
            enabled: row.get("enabled").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    result.sort_by(|left, right| left.trigger_id.cmp(&right.trigger_id));
    Ok(result)
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

    /// A credential reaches the sidecar on stdin and nowhere else, and a
    /// sidecar that refuses it fails the save instead of reporting success.
    #[cfg(unix)]
    #[test]
    fn a_secret_travels_on_stdin_and_a_failing_sidecar_is_an_error() {
        let echoed = run_cli_with_stdin(
            PathBuf::from("/bin/cat"),
            Vec::new(),
            "s3cret-token".to_string(),
        )
        .expect("the child should receive the secret on stdin");
        assert_eq!(echoed, "s3cret-token\n");

        let refused = run_cli_with_stdin(
            PathBuf::from("/bin/sh"),
            vec![
                "-c".to_string(),
                "cat >/dev/null; echo 'no such account' >&2; exit 1".to_string(),
            ],
            "s3cret-token".to_string(),
        )
        .expect_err("a non-zero sidecar exit must not read as a saved credential");
        assert_eq!(refused, "no such account");
    }

    #[test]
    fn fixed_bridge_rejects_unsafe_inputs() {
        assert!(validate_id("run", "run-1").is_ok());
        assert!(validate_id("run", "../../escape").is_err());
        assert!(validate_token("recipe", "recipe.json\0--purge", 100).is_err());
    }

    /// React names a provider from a closed set and nothing else. There is no
    /// argument on any exposure command that could become a program to run.
    #[test]
    fn the_exposure_bridge_takes_no_command_from_the_frontend() {
        let refused = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(channels_exposure_set_tunnel(
                "/bin/sh".to_string(),
                "monkey.example.com".to_string(),
                "/usr/local/bin/cloudflared".to_string(),
                None,
            ));
        assert!(refused.is_err(), "an arbitrary program is not a provider");

        // And a value that would be read as a flag by the CLI's own parser is
        // refused before it can become one.
        let dashed = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime")
            .block_on(channels_exposure_set_tunnel(
                "cloudflared".to_string(),
                "--clear".to_string(),
                "/usr/local/bin/cloudflared".to_string(),
                None,
            ));
        assert!(dashed.is_err());
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

    #[test]
    fn autonomous_placement_normalizes_target_loss_before_backend_specific_processing() {
        let result = normalize_autonomous_placement_result(serde_json::json!({
            "status": "execution_target_lost",
            "final_message": "EXECUTION_TARGET_LOST: Docker daemon unavailable"
        }));
        assert_eq!(result["ok"], false);
        assert_eq!(result["failureCode"], "EXECUTION_TARGET_LOST");
        assert_eq!(result["failureKind"], "EXECUTION_TARGET_LOST");
        assert_eq!(result["summary"], result["final_message"]);

        let result = normalize_autonomous_placement_result(serde_json::json!({
            "status": "ok"
        }));
        assert_eq!(result["ok"], true);
        assert!(result.get("failureCode").is_none());
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

    #[tokio::test]
    async fn docker_placement_round_trips_a_patch_artifact_to_the_host() {
        let required = std::env::var("LITTLE_MONKEY_REQUIRE_DOCKER_E2E").as_deref() == Ok("1");
        let Some(image) = std::env::var_os("LITTLE_MONKEY_DOCKER_E2E_IMAGE") else {
            assert!(!required, "Docker E2E image is required but not configured");
            eprintln!("SKIPPED: LITTLE_MONKEY_DOCKER_E2E_IMAGE is not configured");
            return;
        };
        let docker_ready = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !docker_ready {
            assert!(!required, "Docker daemon is required but unavailable");
            eprintln!("SKIPPED: Docker daemon is unavailable");
            return;
        }

        let workspace = std::env::temp_dir().join(format!(
            "little-monkey-docker-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let verifier = workspace.with_file_name(format!(
            "little-monkey-docker-e2e-verifier-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let git = |root: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git should start");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let init = |root: &Path| {
            std::fs::create_dir_all(root).unwrap();
            git(root, &["init", "-q"]);
            git(root, &["config", "user.email", "e2e@example.test"]);
            git(root, &["config", "user.name", "Docker E2E"]);
            std::fs::write(root.join("a.txt"), "baseline\n").unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "baseline"]);
        };
        let clone_baseline = |source: &Path, target: &Path| {
            let output = Command::new("git")
                .args(["clone", "-q"])
                .arg(source)
                .arg(target)
                .output()
                .expect("git clone should start");
            assert!(
                output.status.success(),
                "git clone: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            git(target, &["remote", "remove", "origin"]);
        };
        init(&workspace);

        let run_id = format!("docker-e2e-{}", uuid::Uuid::new_v4().simple());
        let capability = serde_json::json!({
            "state": "unsupported",
            "evidence": "Docker E2E"
        });
        let run_spec: crate::run_protocol::RunSpec = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "run_id": run_id.clone(),
            "idempotency_key": run_id.clone(),
            "created_at_ms": 1784000000000u64,
            "kind": "autonomous_task",
            "submitted_by": {
                "client_id": "docker-e2e-test",
                "instance_id": "docker-e2e-test",
                "kind": "test",
                "version": "1"
            },
            "task": "Run the Docker E2E mutation",
            "instructions": null,
            "input_artifact_ids": [],
            "target": {
                "kind": "provider",
                "target_id": "docker-e2e-target",
                "label": "Docker E2E target",
                "provider_id": "test-provider",
                "endpoint": "https://example.test/v1",
                "model": "test-model",
                "credential_ref_id": "docker-e2e-credential",
                "capabilities": {
                    "tool_calling": capability,
                    "vision": capability,
                    "embeddings": capability,
                    "structured_output": capability,
                    "image_generation": capability,
                    "audio": capability,
                    "runtime_lifecycle": capability,
                    "fim": capability,
                    "code_completion": capability,
                    "inline_edit": capability,
                    "fim_metadata": null
                }
            },
            "workspace": {
                "workspace_id": "docker-e2e-workspace",
                "primary_root_id": "docker-e2e-root",
                "roots": [{
                    "root_id": "docker-e2e-root",
                    "canonical_path": workspace.canonicalize().unwrap().to_string_lossy(),
                    "access": "read_write",
                    "allow_symlinks_within_root": false
                }],
                "repository_policy": null
            },
            "permission_policy": {
                "mode": "auto",
                "unattended": true,
                "approval_timeout_ms": 1000,
                "default_tool_decision": "allow",
                "tool_rules": [],
                "allow_network": false,
                "allow_external_mutations": false
            },
            "budgets": {
                "wall_time_ms": 120000,
                "max_iterations": 1,
                "max_model_calls": 1,
                "max_tool_calls": 1,
                "max_input_tokens": 1,
                "max_output_tokens": 1,
                "max_cost_micros": null,
                "max_artifact_bytes": 1048576,
                "max_event_count": 32
            },
            "autonomous_task": {
                "schema_version": 1,
                "task_id": run_id.clone(),
                "objective": "Run the Docker E2E mutation",
                "source": "test",
                "relevant_files": ["docker-e2e.txt"],
                "current_workspace_revision": "docker-e2e-baseline",
                "max_repair_rounds": 0,
                "max_workers": 1,
                "guidance": [],
                "delivery_intent": "leave_worktree",
                "execution_owner": {
                    "kind": "remote",
                    "instance_id": "docker-e2e-test",
                    "lease_epoch": 1,
                    "lease_expires_at_ms": 1784000120000u64
                },
                "previous_execution_owner": null,
                "completed_nodes": [],
                "next_node_id": "docker-e2e-node",
                "task_snapshot": {
                    "taskId": run_id.clone(),
                    "objective": "Run the Docker E2E mutation",
                    "source": "test",
                    "workspaceRevision": "docker-e2e-baseline",
                    "plan": {
                        "planId": format!("docker-{run_id}"),
                        "strategy": "PLAN",
                        "revision": 1,
                        "nodes": [{
                            "nodeId": "docker-e2e-node",
                            "taskClass": "implementation",
                            "objective": "Write the Docker E2E file",
                            "dependencies": [],
                            "mutationScope": ["docker-e2e.txt"],
                            "isolation": "shared",
                            "relevantFiles": ["docker-e2e.txt"],
                            "capabilities": ["read", "mutate"],
                            "executionPlacement": {
                                "kind": "docker",
                                "targetId": image.to_string_lossy(),
                                "nodeId": "docker-e2e-node"
                            }
                        }],
                        "outcome": "RUNNING"
                    }
                }
            }
        }))
        .expect("Docker E2E RunSpec should decode");
        let result = autonomous_task_place_node(AutonomousTaskPlacementRequest {
            kind: Some("docker".to_string()),
            target_id: image.to_string_lossy().into_owned(),
            run_spec,
        })
        .await
        .expect("Docker placement should succeed");
        assert_eq!(result.get("ok").and_then(Value::as_bool), Some(true));
        let data_dir = crate::app_paths::data_dir().expect("app data dir");
        assert!(
            !workspace.join("docker-e2e.txt").exists(),
            "remote results must not mutate the host workspace"
        );
        let result_id = result["resultId"]
            .as_str()
            .expect("Docker placement should persist a result");
        let remote_result = crate::execution_target::load_workspace_result(&data_dir, result_id)
            .expect("persisted Docker result should be readable");
        clone_baseline(&workspace, &verifier);
        crate::execution_target::apply_workspace_result(
            &verifier,
            &remote_result.base_snapshot_digest,
            &remote_result,
        )
        .expect("explicit result apply should succeed on a clean verifier");
        assert_eq!(
            std::fs::read_to_string(verifier.join("docker-e2e.txt")).unwrap(),
            "written by the Docker autonomous runner\n"
        );
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&verifier);
    }

    #[tokio::test]
    async fn docker_placement_runs_the_real_monkey_cli_autonomous_executor() {
        let required = std::env::var("LITTLE_MONKEY_REQUIRE_REAL_DOCKER_E2E").as_deref() == Ok("1");
        let Some(image) = std::env::var_os("LITTLE_MONKEY_REAL_DOCKER_E2E_IMAGE") else {
            assert!(
                !required,
                "real Docker E2E image is required but not configured"
            );
            eprintln!("SKIPPED: LITTLE_MONKEY_REAL_DOCKER_E2E_IMAGE is not configured");
            return;
        };
        let docker_ready = Command::new("docker")
            .args(["info", "--format", "{{.ServerVersion}}"])
            .output()
            .is_ok_and(|output| output.status.success());
        if !docker_ready {
            assert!(!required, "Docker daemon is required but unavailable");
            eprintln!("SKIPPED: Docker daemon is unavailable");
            return;
        }

        let workspace = std::env::temp_dir().join(format!(
            "little-monkey-docker-real-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let verifier = workspace.with_file_name(format!(
            "little-monkey-docker-real-e2e-verifier-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let git = |root: &Path, args: &[&str]| {
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .expect("git should start");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let init = |root: &Path| {
            std::fs::create_dir_all(root).unwrap();
            git(root, &["init", "-q"]);
            git(root, &["config", "user.email", "e2e@example.test"]);
            git(root, &["config", "user.name", "Docker real E2E"]);
            std::fs::write(root.join("baseline.txt"), "baseline\n").unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", "baseline"]);
        };
        let clone_baseline = |source: &Path, target: &Path| {
            let output = Command::new("git")
                .args(["clone", "-q"])
                .arg(source)
                .arg(target)
                .output()
                .expect("git clone should start");
            assert!(
                output.status.success(),
                "git clone: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            git(target, &["remote", "remove", "origin"]);
        };
        init(&workspace);

        let image = image.to_string_lossy().into_owned();
        let run_id = format!("docker-real-e2e-{}", uuid::Uuid::new_v4().simple());
        let capability = serde_json::json!({
            "state": "supported",
            "evidence": "deterministic model fixture inside the container"
        });
        let run_spec: crate::run_protocol::RunSpec = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "run_id": run_id,
            "idempotency_key": "docker-real-e2e-idempotency",
            "created_at_ms": 1785000000000u64,
            "kind": "autonomous_task",
            "submitted_by": {
                "client_id": "docker-real-e2e-test",
                "instance_id": "docker-real-e2e-test",
                "kind": "test",
                "version": "1"
            },
            "task": "Run the real Docker autonomous executor",
            "instructions": null,
            "input_artifact_ids": [],
            "target": {
                "kind": "provider",
                "target_id": "docker-real-e2e-target",
                "label": "Docker real E2E target",
                "provider_id": "local-openai-compatible",
                "endpoint": "http://127.0.0.1:18080",
                "model": "docker-real-fixture",
                "credential_ref_id": "credential:none",
                "capabilities": {
                    "tool_calling": capability,
                    "vision": capability,
                    "embeddings": capability,
                    "structured_output": capability,
                    "image_generation": capability,
                    "audio": capability,
                    "runtime_lifecycle": capability,
                    "fim": capability,
                    "code_completion": capability,
                    "inline_edit": capability,
                    "fim_metadata": null
                }
            },
            "workspace": {
                "workspace_id": "docker-real-e2e-workspace",
                "primary_root_id": "docker-real-e2e-root",
                "roots": [{
                    "root_id": "docker-real-e2e-root",
                    "canonical_path": workspace.canonicalize().unwrap().to_string_lossy(),
                    "access": "read_write",
                    "allow_symlinks_within_root": false
                }],
                "repository_policy": null
            },
            "permission_policy": {
                "mode": "auto",
                "unattended": true,
                "approval_timeout_ms": 60000,
                "default_tool_decision": "allow",
                "tool_rules": [{"tool": "write_file", "decision": "allow"}],
                "allow_network": true,
                "allow_external_mutations": false
            },
            "budgets": {
                "wall_time_ms": 120000,
                "max_iterations": 8,
                "max_model_calls": 16,
                "max_tool_calls": 16,
                "max_input_tokens": 100000,
                "max_output_tokens": 100000,
                "max_cost_micros": null,
                "max_artifact_bytes": 1048576,
                "max_event_count": 256
            },
            "autonomous_task": {
                "schema_version": 1,
                "task_id": run_id,
                "objective": "Run the real Docker autonomous executor",
                "source": "test",
                "relevant_files": ["docker-e2e.txt"],
                "current_workspace_revision": "docker-real-e2e-baseline",
                "max_repair_rounds": 0,
                "max_workers": 1,
                "guidance": [],
                "delivery_intent": "leave_worktree",
                "execution_owner": {
                    "kind": "remote",
                    "instance_id": "docker-real-e2e",
                    "lease_epoch": 1,
                    "lease_expires_at_ms": 4000000000000u64
                },
                "previous_execution_owner": null,
                "completed_nodes": [],
                "next_node_id": "docker-real-implement",
                "task_snapshot": {
                    "taskId": run_id,
                    "objective": "Run the real Docker autonomous executor",
                    "source": "test",
                    "workspaceRevision": "docker-real-e2e-baseline",
                    "plan": {
                        "planId": "docker-real-e2e-plan",
                        "strategy": "PLAN",
                        "revision": 1,
                        "nodes": [
                            {
                                "nodeId": "docker-real-implement",
                                "taskClass": "implementation",
                                "objective": "Write docker-e2e.txt using the file tool",
                                "dependencies": [],
                                "mutationScope": ["docker-e2e.txt"],
                                "isolation": "shared",
                                "relevantFiles": ["docker-e2e.txt"],
                                "capabilities": ["read", "mutate"],
                                "executionPlacement": {
                                    "kind": "docker",
                                    "targetId": image.clone(),
                                    "nodeId": "docker-real-implement"
                                }
                            },
                            {
                                "nodeId": "docker-real-verify",
                                "taskClass": "verification",
                                "objective": "Run the configured Docker verification command",
                                "dependencies": ["docker-real-implement"],
                                "mutationScope": [],
                                "isolation": "shared",
                                "relevantFiles": ["docker-e2e.txt"],
                                "capabilities": ["read", "verify"]
                            },
                            {
                                "nodeId": "docker-real-review",
                                "taskClass": "review",
                                "objective": "Review the Docker mutation and return structured evidence",
                                "dependencies": ["docker-real-verify"],
                                "mutationScope": [],
                                "isolation": "shared",
                                "relevantFiles": ["docker-e2e.txt"],
                                "capabilities": ["read", "verify"]
                            }
                        ]
                    },
                    "outcome": "RUNNING"
                }
            }
        }))
        .expect("real Docker E2E RunSpec should decode");
        let result = autonomous_task_place_node(AutonomousTaskPlacementRequest {
            kind: Some("docker".to_string()),
            target_id: image,
            run_spec,
        })
        .await
        .expect("real Docker placement should succeed");
        assert_eq!(
            result.get("ok").and_then(Value::as_bool),
            Some(true),
            "real Docker child result: {result}"
        );
        assert!(result["changedFiles"]
            .as_array()
            .is_some_and(|files| files.iter().any(|file| file == "docker-e2e.txt")));
        assert!(result["evidence"]
            .as_array()
            .is_some_and(|items| !items.is_empty()));
        assert_eq!(result["review"]["verdict"], "pass");
        let data_dir = crate::app_paths::data_dir().expect("app data dir");
        assert!(result["resultId"].is_string());
        assert!(
            !workspace.join("docker-e2e.txt").exists(),
            "remote results must not mutate the host workspace"
        );
        let remote_result = crate::execution_target::load_workspace_result(
            &data_dir,
            result["resultId"].as_str().unwrap(),
        )
        .expect("real Docker result should be persisted");
        clone_baseline(&workspace, &verifier);
        crate::execution_target::apply_workspace_result(
            &verifier,
            &remote_result.base_snapshot_digest,
            &remote_result,
        )
        .expect("explicit result apply should replay on a clean host repo");
        assert_eq!(
            std::fs::read_to_string(verifier.join("docker-e2e.txt")).unwrap(),
            "written by the real monkey-cli\n"
        );
        let _ = std::fs::remove_dir_all(&workspace);
        let _ = std::fs::remove_dir_all(&verifier);
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

    /// The peer bridge, held against a real `monkey peers list --json` payload
    /// rather than a round trip of these structs' own output.
    ///
    /// A round trip would pass with any spelling, including one no CLI has ever
    /// emitted and no `peersClient.ts` has ever read. What matters is that the
    /// exact bytes the CLI prints decode here with every field populated, and
    /// that what goes back out carries the same names the panel indexes by —
    /// because a silent rename does not fail anywhere, it just renders blank.
    #[test]
    fn the_peer_bridge_decodes_the_cli_and_re_emits_what_the_panel_reads() {
        let payload = r#"{
            "inbound": [{
                "device_id": "device-1",
                "label": "Studio desktop",
                "grants": ["message"],
                "advertised_grants": ["message", "task", "artifact"],
                "requested_grants": ["task"],
                "state": "active",
                "peer_only": true,
                "last_sequence": 4,
                "last_seen_at_ms": 1700000000000,
                "presence": "online",
                "secret_generation": 2
            }],
            "outbound": [{
                "alias": "studio",
                "peer_id": "runner-two",
                "peer_url": "https://studio.invalid",
                "grants": ["message", "task"],
                "advertised_grants": ["message", "task", "artifact"],
                "requested_grants": [],
                "certificate_sha256": "ab",
                "last_seen_at_ms": null,
                "presence": "unknown",
                "secret_generation": 1
            }]
        }"#;

        let listed: PeerListResponse = parse_typed_json(payload).expect("decode the CLI payload");
        let inbound = &listed.inbound[0];
        assert_eq!(inbound.grants, vec![PeerGrantView::Message]);
        // What it asked for is decoded, and is not what it holds.
        assert_eq!(inbound.requested_grants, vec![PeerGrantView::Task]);
        assert_eq!(inbound.presence, PeerPresenceView::Online);
        assert_eq!(inbound.state, PeerPairingStateView::Active);
        assert_eq!(inbound.last_seen_at_ms, Some(1_700_000_000_000));
        assert_eq!(inbound.secret_generation, 2);
        let outbound = &listed.outbound[0];
        assert_eq!(outbound.presence, PeerPresenceView::Unknown);
        assert_eq!(outbound.last_seen_at_ms, None);

        // Back out under the names `peersClient.ts` declares. These views are
        // snake_case in *both* directions, unlike the camelCase responses
        // elsewhere in this file — see the comment above `PeerGrantView`.
        let value = serde_json::to_value(&listed).unwrap();
        for field in [
            "device_id",
            "advertised_grants",
            "requested_grants",
            "peer_only",
            "last_seen_at_ms",
            "secret_generation",
            "presence",
        ] {
            assert!(
                value["inbound"][0].get(field).is_some(),
                "the panel reads `{field}` and the bridge did not emit it"
            );
        }
        assert!(value["inbound"][0].get("deviceId").is_none());
        assert_eq!(value["inbound"][0]["presence"], "online");
        assert_eq!(value["inbound"][0]["requested_grants"][0], "task");

        // An older CLI that has not learned the newer fields still decodes, so
        // a mid-upgrade desktop shows a peer rather than an error.
        let older: PeerListResponse = parse_typed_json(
            r#"{"inbound":[{"device_id":"d","label":"l","grants":[],"state":"revoked",
                 "peer_only":true,"last_sequence":0,"last_seen_at_ms":null,
                 "presence":"unknown","secret_generation":1}],"outbound":[]}"#,
        )
        .expect("decode without the advertisement fields");
        assert!(older.inbound[0].advertised_grants.is_empty());
        assert_eq!(older.inbound[0].state, PeerPairingStateView::Revoked);
    }

    /// Every other peer command's response, same rule.
    #[test]
    fn each_peer_command_decodes_the_shape_its_cli_prints() {
        let threads: PeerThreadsResponse = parse_typed_json(
            r#"{"threads":[{"thread_id":"thread-1","peer_device_id":"device-1",
                 "peer_instance_id":"instance-remote","session_key":"peer:device-1:thread-1",
                 "created_at_ms":1,"last_activity_at_ms":2,"message_count":1,
                 "recent":[{"message_id":"msg-1","direction":"inbound","kind":"task_request",
                 "disposition":"rejected","rejection":"missing_capability","job_id":null,
                 "correlation_id":"corr-1","created_at_ms":1}]}],"recipe":"peer-task"}"#,
        )
        .expect("threads");
        assert_eq!(threads.recipe, "peer-task");
        let message = &threads.threads[0].recent[0];
        assert_eq!(message.disposition, PeerMessageDispositionView::Rejected);
        assert_eq!(message.direction, PeerMessageDirectionView::Inbound);
        assert_eq!(message.correlation_id.as_deref(), Some("corr-1"));

        let rotated: PeerRotationResponse = parse_typed_json(
            r#"{"device_id":"device-1","secret_generation":3,"output":"/tmp/rot.json"}"#,
        )
        .expect("rotation");
        assert_eq!(rotated.secret_generation, 3);

        let accepted: PeerRotationAcceptedResponse = parse_typed_json(
            r#"{"alias":"studio","secret_generation":3,"certificate_sha256":"ab"}"#,
        )
        .expect("rotation accepted");
        assert_eq!(accepted.alias, "studio");

        let cleared: PeerClearResponse = parse_typed_json(
            r#"{"device_id":"device-1","threads_removed":2,"grants_cleared":true}"#,
        )
        .expect("clear");
        assert_eq!(cleared.threads_removed, 2);
        assert!(cleared.grants_cleared);

        // `peers status --json` carries more than the desktop reads. Extra
        // fields must not break the decode, or adding one to the CLI would
        // break the app.
        let status: PeerStatusResponse = parse_typed_json(
            r#"{"alias":"studio","peer_id":"runner-two","last_seen_at_ms":5,
                "presence":"online","advertised_grants":["message"],"granted":["message"]}"#,
        )
        .expect("status");
        assert_eq!(status.presence, PeerPresenceView::Online);
        assert_eq!(status.last_seen_at_ms, Some(5));

        let invited: PeerInvitationResponse = parse_typed_json(
            r#"{"pairing_id":"pair-1","expires_at_ms":9,"grants":["message"],
                "output":"/tmp/invite.json"}"#,
        )
        .expect("invitation");
        assert_eq!(invited.grants, vec![PeerGrantView::Message]);

        let paired: PeerAcceptedResponse = parse_typed_json(
            r#"{"alias":"studio","peer_id":"runner-two","peer_url":"https://studio.invalid",
                "grants":["message","task"],"certificate_sha256":"ab"}"#,
        )
        .expect("accepted");
        assert_eq!(paired.grants.len(), 2);

        let granted: PeerGrantResponse =
            parse_typed_json(r#"{"device_id":"device-1","grants":["message","artifact"]}"#)
                .expect("grant");
        assert_eq!(
            granted.grants,
            vec![PeerGrantView::Message, PeerGrantView::Artifact]
        );

        // What this installation sent, as `peers outbound --json` and
        // `peers remote-thread --json` both print it.
        let outbound: PeerOutboundResponse = parse_typed_json(
            r#"{"messages":[{"alias":"studio","message_id":"pmsg-1","thread_id":"thread-1",
                "correlation_id":"corr-1","kind":"task_request","state":"succeeded",
                "result_text":"the build is red because of a bad migration",
                "sent_at_ms":1,"checked_at_ms":9}]}"#,
        )
        .expect("outbound");
        assert_eq!(outbound.messages[0].state, "succeeded");
        assert_eq!(
            outbound.messages[0].correlation_id.as_deref(),
            Some("corr-1")
        );
        assert_eq!(outbound.messages[0].checked_at_ms, Some(9));

        // A task nobody has polled for yet: no result, no check time.
        let pending: PeerOutboundResponse = parse_typed_json(
            r#"{"messages":[{"alias":"studio","message_id":"pmsg-2","thread_id":"thread-1",
                "correlation_id":null,"kind":"message","state":"queued","result_text":null,
                "sent_at_ms":2,"checked_at_ms":null}]}"#,
        )
        .expect("pending outbound");
        assert_eq!(pending.messages[0].result_text, None);
        assert_eq!(pending.messages[0].checked_at_ms, None);
    }

    /// The remote poll takes an alias and a thread id, and nothing else can be
    /// smuggled through either of them.
    #[test]
    fn the_remote_thread_bridge_refuses_anything_that_is_not_an_identifier() {
        for forged in [
            "../../v1/remote/runs",
            "thread-1/../node",
            "https://elsewhere.invalid",
            "thread 1",
            "",
        ] {
            assert!(
                validate_id("thread id", forged).is_err(),
                "'{forged}' must not reach a peer route"
            );
        }
        assert!(validate_id("thread id", "thread-9f2c4a").is_ok());
    }

    /// The ids the daemon hands the desktop have to come back through the
    /// bridge unchanged, or the conversation they name can never be opened.
    #[test]
    fn a_providers_own_identifier_reaches_the_cli_and_a_forged_one_does_not() {
        for real in [
            // What `conversations list` actually returns for a channel.
            "channel:telegram:chan-d9d972559ad540bdb51f8e9055b38068:931819457",
            // A Matrix sender, and an SMS conversation.
            "@someone:server.org",
            "+15555550123",
            "chan-d9d972559ad540bdb51f8e9055b38068",
        ] {
            assert!(
                channel_id("conversation id", real).is_ok(),
                "'{real}' is an id this app itself minted"
            );
        }
        for forged in [
            // Read as a flag by the CLI's parser, not as a value.
            "--limit=9999",
            "../../escape",
            "id with spaces",
            "",
        ] {
            assert!(
                channel_id("conversation id", forged).is_err(),
                "'{forged}' must not reach an argument vector"
            );
        }
        assert!(channel_id("conversation id", &"a".repeat(257)).is_err());
    }

    /// A grant the peer surface must never hand out is refused at the bridge,
    /// before an argument is built.
    #[test]
    fn the_peer_bridge_refuses_a_grant_that_is_not_a_peer_grant() {
        assert!(peer_grants(&["message".into(), "task".into()]).is_ok());
        for forbidden in ["admin", "place_runs", "view_runs", "control_desktop", ""] {
            assert!(
                peer_grants(&[forbidden.to_string()]).is_err(),
                "'{forbidden}' must not be spellable as a peer grant"
            );
        }
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
// and it crosses on the sidecar's stdin rather than through an argument vector:
// a credential must never be visible in a process listing, and the keychain
// entry has to be created by the executable the daemon later reads it from.

const MAX_CHANNEL_ID: usize = 256;

/// One identifier the daemon itself minted, on its way back into an argument
/// vector.
///
/// Deliberately wider than [`validate_id`]: these are *providers'* ids, not
/// this app's, and the shapes they really take carry punctuation — a
/// conversation is addressed as `channel:<provider>:<account>:<conversation>`,
/// a Matrix sender is `@someone:server.org`, an SMS conversation is an E.164
/// number. Rejecting those is not a safety property, it is a permanently
/// unopenable conversation.
///
/// What is actually load-bearing stays: nothing empty or unbounded, no `..`
/// path segment, and no leading dash — a value starting with one would be read
/// as a flag by the CLI's own parser even though nothing here goes through a
/// shell.
fn channel_id(label: &str, value: &str) -> Result<String, String> {
    let allowed = |ch: char| {
        ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':' | '@' | '+')
    };
    if value.is_empty()
        || value.len() > MAX_CHANNEL_ID
        || value.contains("..")
        || value.starts_with('-')
        || !value.chars().all(allowed)
    {
        return Err(format!("Invalid {label}"));
    }
    Ok(value.to_string())
}

/// Conversations that live outside the desktop app — a paired phone's chat, a
/// messaging conversation the agent is answering — so the session list can
/// show them next to this machine's own sessions.
///
/// `environment` is optional and validated as a token rather than an id: it is
/// a fixed vocabulary (`remote_control`, `channel`, `channel:<provider>`) the
/// CLI itself refuses anything outside of.
#[tauri::command]
pub async fn conversations_list(environment: Option<String>, limit: u32) -> Result<Value, String> {
    let mut args = vec!["conversations".into(), "list".into()];
    if let Some(environment) = environment {
        validate_token("environment", &environment, 64)?;
        if environment.starts_with('-') {
            return Err("Invalid environment".to_string());
        }
        args.push("--environment".into());
        args.push(environment);
    }
    args.push("--limit".into());
    args.push(limit.clamp(1, 500).to_string());
    args.push("--json".into());
    parse_json(&command(args).await?)
}

/// One outside conversation's transcript, oldest first.
#[tauri::command]
pub async fn conversations_show(
    environment: String,
    id: String,
    limit: u32,
) -> Result<Value, String> {
    validate_token("environment", &environment, 64)?;
    let id = channel_id("conversation id", &id)?;
    if environment.starts_with('-') {
        return Err("Invalid environment".to_string());
    }
    parse_json(
        &command(vec![
            "conversations".into(),
            "show".into(),
            "--environment".into(),
            environment,
            "--id".into(),
            id,
            "--limit".into(),
            limit.clamp(1, 2_000).to_string(),
            "--json".into(),
        ])
        .await?,
    )
}

/// Erase one outside conversation from this machine. The CLI refuses — with
/// a reason the sidebar shows — while a turn or a reply for it is in flight.
#[tauri::command]
pub async fn conversations_delete(environment: String, id: String) -> Result<(), String> {
    validate_token("environment", &environment, 64)?;
    let id = channel_id("conversation id", &id)?;
    if environment.starts_with('-') {
        return Err("Invalid environment".to_string());
    }
    command(vec![
        "conversations".into(),
        "delete".into(),
        "--environment".into(),
        environment,
        "--id".into(),
        id,
    ])
    .await
    .map(|_| ())
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

/// Approve or block a sender — a waiting one, or an approved one whose access
/// the operator is taking back.
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

/// Forget a sender: their approval or block, their model pick, and that they
/// were greeted. Their next message meets the pairing challenge afresh.
#[tauri::command]
pub async fn channels_forget_sender(account_id: String, sender_id: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let sender_id = channel_id("sender id", &sender_id)?;
    command(vec![
        "channels".into(),
        "forget".into(),
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
/// Handed to the sidecar's own `set-token` on stdin rather than written here:
/// that is the one command that writes this keychain entry, and it runs in the
/// executable the daemon later reads it from — see [`run_cli_with_stdin`] for
/// why the writing process is what decides whether the daemon's read prompts.
/// The value is never echoed back, never logged, and never returned — the
/// account row only ever learns that a credential exists.
#[tauri::command]
pub async fn channels_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    bounded_secret("messaging credential", &secret)?;
    command_with_stdin(
        vec!["channels".into(), "set-token".into(), account_id],
        secret,
    )
    .await
    .map(|_| ())
}

// --- Public callback exposure ------------------------------------------------
//
// How this machine is reached from the internet, as five fixed-argument
// commands. Note what React cannot express through any of them: it names a
// tunnel provider from a closed set, a hostname, and a path — never an argv,
// never a flag, never a command to run. The daemon builds what it executes from
// its own template, and the credential goes from here straight to the keychain
// without passing through a CLI argument at all.

/// What the exposure is configured as and what it is doing. Never a credential:
/// the status type has no field for one.
#[tauri::command]
pub async fn channels_exposure_status() -> Result<Value, String> {
    parse_json(
        &command(vec![
            "channels".into(),
            "exposure".into(),
            "status".into(),
            "--json".into(),
        ])
        .await?,
    )
}

/// Go back to publishing the URL yourself.
#[tauri::command]
pub async fn channels_exposure_manual() -> Result<(), String> {
    command(vec!["channels".into(), "exposure".into(), "manual".into()])
        .await
        .map(|_| ())
}

/// Configure the daemon to run the operator's own tunnel client.
///
/// Every value is validated here as well as in the CLI, because this is the
/// boundary a browser reaches: a hostname that is really a URL, or a path that
/// is really a flag, must be refused before it becomes an argument.
#[tauri::command]
pub async fn channels_exposure_set_tunnel(
    provider: String,
    hostname: String,
    executable: String,
    metrics_port: Option<u16>,
) -> Result<(), String> {
    // A closed set, checked here rather than passed through. React naming an
    // arbitrary program is the thing this whole shape exists to prevent.
    if provider != "cloudflared" {
        return Err(format!(
            "'{provider}' is not a tunnel provider this build knows how to run."
        ));
    }
    validate_token("tunnel hostname", &hostname, 253)?;
    validate_token("tunnel client path", &executable, 4096)?;
    // Both are positional, so a leading dash would be read as a flag by the
    // CLI's own parser — the same trap `set-public-url` guards against.
    if hostname.starts_with('-') || executable.starts_with('-') {
        return Err("A hostname or path may not begin with '-'.".to_string());
    }
    let mut args = vec![
        "channels".into(),
        "exposure".into(),
        "tunnel".into(),
        provider,
        "--hostname".into(),
        hostname,
        "--executable".into(),
        executable,
    ];
    if let Some(port) = metrics_port {
        args.push("--metrics-port".into());
        args.push(port.to_string());
    }
    command(args).await.map(|_| ())
}

/// Store the tunnel credential.
///
/// On the sidecar's stdin, never in an argument: an argument is visible in a
/// process listing. The daemon's tunnel supervisor is what reads this entry
/// back, so the sidecar is also the process that has to have written it.
#[tauri::command]
pub async fn channels_exposure_set_token(token: String) -> Result<(), String> {
    bounded_secret("tunnel credential", &token)?;
    command_with_stdin(
        vec![
            "channels".into(),
            "exposure".into(),
            "set-token".into(),
        ],
        token,
    )
    .await
    .map(|_| ())
}

/// Forget the tunnel credential.
#[tauri::command]
pub async fn channels_exposure_clear_token() -> Result<(), String> {
    command(vec![
        "channels".into(),
        "exposure".into(),
        "clear-token".into(),
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

// --- Peers -----------------------------------------------------------------
//
// Every view below is **snake_case in both directions**, unlike the camelCase
// responses elsewhere in this file. That is deliberate and load-bearing: these
// types decode `monkey peers … --json`, which emits snake_case, and they are
// also what `peersClient.ts` reads. Making the two spellings the same means the
// typed bridge is a validating pass-through — a field the CLI renames fails to
// decode here rather than reaching the UI as `undefined`.

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerGrantView {
    Message,
    Task,
    Artifact,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerPresenceView {
    Online,
    Offline,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerPairingStateView {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct InboundPeerView {
    pub device_id: String,
    pub label: String,
    pub grants: Vec<PeerGrantView>,
    #[serde(default)]
    pub advertised_grants: Vec<PeerGrantView>,
    #[serde(default)]
    pub requested_grants: Vec<PeerGrantView>,
    pub state: PeerPairingStateView,
    pub peer_only: bool,
    pub last_sequence: u64,
    pub last_seen_at_ms: Option<u64>,
    pub presence: PeerPresenceView,
    pub secret_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct OutboundPeerView {
    pub alias: String,
    pub peer_id: String,
    pub peer_url: String,
    pub grants: Vec<PeerGrantView>,
    #[serde(default)]
    pub advertised_grants: Vec<PeerGrantView>,
    #[serde(default)]
    pub requested_grants: Vec<PeerGrantView>,
    pub certificate_sha256: String,
    pub last_seen_at_ms: Option<u64>,
    pub presence: PeerPresenceView,
    pub secret_generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerListResponse {
    pub inbound: Vec<InboundPeerView>,
    pub outbound: Vec<OutboundPeerView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerInvitationResponse {
    pub pairing_id: String,
    pub expires_at_ms: u64,
    pub grants: Vec<PeerGrantView>,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerAcceptedResponse {
    pub alias: String,
    pub peer_id: String,
    pub peer_url: String,
    pub grants: Vec<PeerGrantView>,
    pub certificate_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerGrantResponse {
    pub device_id: String,
    pub grants: Vec<PeerGrantView>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerMessageDirectionView {
    Inbound,
    Outbound,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerMessageDispositionView {
    Accepted,
    Rejected,
    Delivered,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerThreadMessageView {
    pub message_id: String,
    pub direction: PeerMessageDirectionView,
    pub kind: String,
    pub disposition: PeerMessageDispositionView,
    pub rejection: Option<String>,
    pub job_id: Option<String>,
    pub correlation_id: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerThreadView {
    pub thread_id: String,
    pub peer_device_id: String,
    pub peer_instance_id: String,
    pub session_key: String,
    pub created_at_ms: i64,
    pub last_activity_at_ms: i64,
    pub message_count: usize,
    pub recent: Vec<PeerThreadMessageView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerThreadsResponse {
    pub threads: Vec<PeerThreadView>,
    pub recipe: String,
}

/// One thing this installation sent to a peer, and the last thing it heard
/// back.
///
/// The counterpart of [`PeerThreadView`], which is the inbound side. Both are
/// needed: an operator who sends a task to another installation cannot see
/// anything about it on the receiving-side listing, because it is not their
/// thread — it is one they opened over there.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerOutboundMessageView {
    pub alias: String,
    pub message_id: String,
    pub thread_id: String,
    pub correlation_id: Option<String>,
    pub kind: String,
    /// What the send returned, or what a later poll of that peer's thread
    /// reported: `queued`, `accepted`, `duplicate`, `rejected`, `succeeded`,
    /// `failed` or `cancelled`.
    pub state: String,
    pub result_text: Option<String>,
    pub sent_at_ms: i64,
    pub checked_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerOutboundResponse {
    pub messages: Vec<PeerOutboundMessageView>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerRotationResponse {
    pub device_id: String,
    pub secret_generation: u64,
    pub output: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerRotationAcceptedResponse {
    pub alias: String,
    pub secret_generation: u64,
    pub certificate_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerClearResponse {
    pub device_id: String,
    pub threads_removed: u32,
    pub grants_cleared: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct PeerStatusResponse {
    pub alias: String,
    pub peer_id: String,
    pub last_seen_at_ms: Option<u64>,
    pub presence: PeerPresenceView,
}

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
pub async fn peers_list() -> Result<PeerListResponse, String> {
    parse_typed_json(&command(vec!["peers".into(), "list".into(), "--json".into()]).await?)
}

/// Offer another installation peer standing on this one. The invitation is
/// written to `output`; it is one-time and expires.
#[tauri::command]
pub async fn peers_invite(
    label: String,
    allow: Vec<String>,
    expires_minutes: u64,
    output: String,
) -> Result<PeerInvitationResponse, String> {
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
    parse_typed_json(
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
pub async fn peers_accept(
    invitation: String,
    alias: String,
) -> Result<PeerAcceptedResponse, String> {
    validate_id("peer alias", &alias)?;
    let path = Path::new(&invitation);
    if !path.is_absolute() || !path.is_file() {
        return Err("Choose the invitation file the other installation gave you".to_string());
    }
    parse_typed_json(
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
pub async fn peers_grant(
    device_id: String,
    allow: Vec<String>,
) -> Result<PeerGrantResponse, String> {
    validate_id("device id", &device_id)?;
    let grants = peer_grants(&allow)?;
    parse_typed_json(
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

/// Rotate an inbound peer's HMAC credential. The old key stops working before
/// the bundle is returned; the bundle contains the replacement once and is
/// written only to the operator-selected private path.
#[tauri::command]
pub async fn peers_rotate(
    device_id: String,
    output: String,
) -> Result<PeerRotationResponse, String> {
    validate_id("device id", &device_id)?;
    validate_output_path(&output)?;
    parse_typed_json(
        &command(vec![
            "peers".into(),
            "rotate".into(),
            device_id,
            "--output".into(),
            output,
            "--json".into(),
        ])
        .await?,
    )
}

/// Import the bundle produced by the peer that rotated this outbound
/// pairing. The remote store verifies identity and refuses scope expansion.
#[tauri::command]
pub async fn peers_accept_rotation(
    bundle: String,
    alias: String,
) -> Result<PeerRotationAcceptedResponse, String> {
    validate_id("peer alias", &alias)?;
    validate_existing_private_input("rotation bundle", &bundle)?;
    parse_typed_json(
        &command(vec![
            "peers".into(),
            "accept-rotation".into(),
            bundle,
            alias,
            "--json".into(),
        ])
        .await?,
    )
}

/// Clear one peer's operational traffic. For a revoked pairing this also
/// clears the retained peer grants so it leaves the management list; the
/// immutable remote audit trail remains intact.
#[tauri::command]
pub async fn peers_clear(device_id: String) -> Result<PeerClearResponse, String> {
    validate_id("device id", &device_id)?;
    parse_typed_json(
        &command(vec![
            "peers".into(),
            "clear".into(),
            device_id,
            "--json".into(),
        ])
        .await?,
    )
}

/// Forget an outbound peer profile and remove its current keychain secret.
#[tauri::command]
pub async fn peers_forget(alias: String) -> Result<(), String> {
    validate_id("peer alias", &alias)?;
    command(vec!["peers".into(), "forget".into(), alias])
        .await
        .map(|_| ())
}

/// Perform a signed, certificate-pinned liveness check against one outbound
/// peer. The response contains identity and time only.
#[tauri::command]
pub async fn peers_status(alias: String) -> Result<PeerStatusResponse, String> {
    validate_id("peer alias", &alias)?;
    parse_typed_json(
        &command(vec![
            "peers".into(),
            "status".into(),
            alias,
            "--json".into(),
        ])
        .await?,
    )
}

/// Threads inbound peers opened here, with their recent traffic.
#[tauri::command]
pub async fn peers_threads(
    peer: Option<String>,
    limit: u32,
) -> Result<PeerThreadsResponse, String> {
    let mut args = vec!["peers".into(), "threads".into()];
    if let Some(peer) = peer {
        validate_id("device id", &peer)?;
        args.push("--peer".into());
        args.push(peer);
    }
    args.push("--limit".into());
    args.push(limit.clamp(1, 200).to_string());
    args.push("--json".into());
    parse_typed_json(&command(args).await?)
}

/// What this installation has sent to peers, newest first.
///
/// Local state only: reading it contacts nobody. The remote half is
/// [`peers_remote_thread`], which the operator asks for explicitly.
#[tauri::command]
pub async fn peers_outbound(
    alias: Option<String>,
    limit: u32,
) -> Result<PeerOutboundResponse, String> {
    let mut args = vec!["peers".into(), "outbound".into()];
    if let Some(alias) = alias {
        validate_id("peer alias", &alias)?;
        args.push("--alias".into());
        args.push(alias);
    }
    args.push("--limit".into());
    args.push(limit.clamp(1, 200).to_string());
    args.push("--json".into());
    parse_typed_json(&command(args).await?)
}

/// Ask one peer about one thread this installation opened.
///
/// Two fixed arguments, both validated as identifiers: there is no URL, no
/// route and no host here, so nothing React sends can reach anywhere other than
/// a peer's own thread endpoint through the signed, certificate-pinned call the
/// CLI already makes. The thread must be one this installation has a record of
/// sending to, which is why there is no "list that peer's threads" command
/// anywhere — enumerating another node's conversations is not something a peer
/// should be able to do.
#[tauri::command]
pub async fn peers_remote_thread(
    alias: String,
    thread_id: String,
) -> Result<PeerOutboundResponse, String> {
    validate_id("peer alias", &alias)?;
    validate_id("thread id", &thread_id)?;
    parse_typed_json(
        &command(vec![
            "peers".into(),
            "remote-thread".into(),
            alias,
            thread_id,
            "--json".into(),
        ])
        .await?,
    )
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
// boundary, and like a messaging credential it goes in on the sidecar's stdin
// rather than through an argument vector.

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

/// Point an account's carrier somewhere else, or update its non-secret
/// settings. The public URL is what every signature check is rebuilt from, so
/// an operator whose tunnel moved fixes it here rather than by re-adding the
/// number and losing its history.
#[tauri::command]
pub async fn telecom_set_public_url(
    account_id: String,
    url: Option<String>,
    config: Option<String>,
) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    let mut args = vec!["telecom".into(), "set-url".into(), account_id];
    match url {
        Some(url) => {
            validate_token("public URL", &url, 512)?;
            args.push("--url".into());
            args.push(url);
        }
        None if config.is_none() => args.push("--clear".into()),
        None => {}
    }
    if let Some(config) = config {
        serde_json::from_str::<Value>(&config)
            .map_err(|error| format!("Carrier settings must be a JSON object: {error}"))?;
        args.push("--config".into());
        args.push(config);
    }
    command(args).await.map(|_| ())
}

/// Recent texts on a number, both directions, with their delivery state.
#[tauri::command]
pub async fn telecom_messages(account_id: String, limit: u32) -> Result<Value, String> {
    let account_id = channel_id("account id", &account_id)?;
    parse_json(
        &command(vec![
            "telecom".into(),
            "messages".into(),
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
/// Same arrangement as a messaging credential: the sidecar's `set-token` reads
/// it from stdin and writes the keychain entry, so the executable that created
/// the entry is the one the daemon reads it back from. The value is never
/// echoed back, never logged and never returned; the account row only ever
/// learns that a credential exists.
#[tauri::command]
pub async fn telecom_set_credential(account_id: String, secret: String) -> Result<(), String> {
    let account_id = channel_id("account id", &account_id)?;
    bounded_secret("carrier credential", &secret)?;
    command_with_stdin(
        vec!["telecom".into(), "set-token".into(), account_id],
        secret,
    )
    .await
    .map(|_| ())
}

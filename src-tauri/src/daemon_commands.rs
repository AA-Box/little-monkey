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
    ];
    if request.actions.is_empty()
        || request
            .actions
            .iter()
            .any(|action| !allowed.contains(&action.as_str()))
    {
        return Err("Pairing requires valid explicit actions".to_string());
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

#[tauri::command]
pub async fn daemon_desktop_status() -> Result<DaemonDesktopStatus, String> {
    let output = command(vec!["daemon".into(), "status".into(), "--json".into()]).await?;
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
    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve application data directory".to_string())?;
    let workspace = crate::workspace::primary_root_canon(state).ok();
    let mut visible = HashMap::new();
    for discovered in crate::recipes::discover_recipes(workspace.as_deref(), &app_data) {
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

#[tauri::command]
pub async fn remote_pair_create(request: RemotePairRequest) -> Result<String, String> {
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
    command(args).await
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
        };
        assert!(validate_remote_pair_request(&valid).is_ok());

        let mut missing_scope = valid.clone();
        missing_scope.run_ids.clear();
        assert!(validate_remote_pair_request(&missing_scope).is_err());

        let mut oversized = valid.clone();
        oversized.max_artifact_bytes = MAX_REMOTE_ARTIFACT_BYTES + 1;
        assert!(validate_remote_pair_request(&oversized).is_err());

        let mut missing_dependency = valid;
        missing_dependency.actions = vec!["approve".to_string()];
        assert!(validate_remote_pair_request(&missing_dependency).is_err());
    }

    #[test]
    fn status_deserializes_cli_shape_and_serializes_ui_shape() {
        let status: DaemonDesktopStatus=serde_json::from_value(serde_json::json!({"installed":true,"service_running":true,"heartbeat_fresh":true,"pid":1,"kill_switch":false,"queued":0,"active":1,"waiting_approval":0,"paused":0,"managed_run_ids":["run-one"],"platform":"macos"})).unwrap();
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["serviceRunning"], true);
        assert_eq!(value["managedRunIds"], serde_json::json!(["run-one"]));
        assert!(value.get("service_running").is_none());
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

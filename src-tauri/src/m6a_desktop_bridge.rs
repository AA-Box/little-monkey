//! Secure M6A desktop-to-daemon submission bridge.
//!
//! The webview supplies a typed, secret-free desktop snapshot. Rust validates
//! it as a normal recipe, publishes it to an app-private temporary file, then
//! invokes only the fixed `daemon run` command. The resident daemon copies
//! that file into its immutable snapshot store before acknowledging the run;
//! no model/tool loop exists in this module.

use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(unix)]
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::recipes::{self, Recipe};

const MAX_SUBMISSION_BYTES: usize = 48 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesktopTurnSubmitRequest {
    pub turn_id: String,
    pub recipe: Recipe,
    /// Which of the operator's own surfaces this turn came from: `desktop`
    /// (the chat composer) or `voice` (a finalized hands-free utterance).
    ///
    /// Both are the operator speaking, and both take the same durable path;
    /// the distinction is what a listing shows and how the turn deduplicates.
    /// Absent means desktop, so an older webview keeps working.
    #[serde(default)]
    pub source: Option<String>,
}

/// The origins this bridge may claim.
///
/// A closed list rather than a passthrough: the webview must not be able to
/// have its turn recorded as a paired peer or an authenticated phone, which
/// would make the ingress listing lie about who asked for something.
const BRIDGE_SOURCES: [&str; 2] = ["desktop", "voice"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DesktopTurnSubmitResponse {
    pub job_id: String,
    pub run_id: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutonomousTaskSubmitRequest {
    pub task_id: String,
    pub recipe: Recipe,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AutonomousTaskOwnerFenceRequest {
    pub task_id: String,
    pub owner: recipes::AutonomousTaskOwnerSnapshot,
}

fn validate_id(value: &str) -> Result<(), String> {
    named_id(value, "turn")
}

/// Path- and argv-safe identifier check.
///
/// The session id needs it for the same reason the turn id does: both become
/// part of the durable ingress identity and one of them becomes a filename.
fn named_id(value: &str, what: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err(format!("Desktop {what} id is invalid"))
    } else {
        Ok(())
    }
}

/// The conversation origin this request claims, defaulting to the composer.
fn request_source(request: &DesktopTurnSubmitRequest) -> Result<&str, String> {
    let source = request.source.as_deref().unwrap_or("desktop");
    if !BRIDGE_SOURCES.contains(&source) {
        return Err(format!(
            "A desktop submission may only originate from {}, not '{source}'",
            BRIDGE_SOURCES.join(" or ")
        ));
    }
    Ok(source)
}

fn validate_request(request: &DesktopTurnSubmitRequest) -> Result<Vec<u8>, String> {
    validate_id(&request.turn_id)?;
    request_source(request)?;
    recipes::validate_recipe(&request.recipe)?;
    let snapshot =
        request.recipe.desktop_turn.as_ref().ok_or_else(|| {
            "Desktop daemon submission requires a desktop_turn snapshot".to_string()
        })?;
    if snapshot.turn_id != request.turn_id {
        return Err("Desktop turn id differs from its immutable recipe snapshot".to_string());
    }
    named_id(&snapshot.session_id, "session")?;
    let bytes = serde_json::to_vec_pretty(&request.recipe).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_SUBMISSION_BYTES {
        return Err("Desktop daemon submission exceeds the 48 MiB transport limit".to_string());
    }
    Ok(bytes)
}

/// What publishing a submission did, so the caller knows whether the file is
/// its own to clean up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Published {
    /// This call created the file.
    Created,
    /// An in-flight submission of the identical turn already published it. The
    /// retry rides on that one; deleting it here would pull the snapshot out
    /// from under a `daemon run` that is still reading it.
    AlreadyPublishedIdentically,
}

fn publish_private_snapshot(path: &Path, bytes: &[u8]) -> Result<Published, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Desktop submission path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create desktop submission directory: {error}"))?;
    let parent_metadata = std::fs::symlink_metadata(parent)
        .map_err(|error| format!("Failed to inspect desktop submission directory: {error}"))?;
    if !parent_metadata.file_type().is_dir() || parent_metadata.file_type().is_symlink() {
        return Err("Desktop submission directory is not a real directory".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Failed to protect desktop submission directory: {error}"))?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            // A retry of the same send after a timed-out response must not be
            // an error: the whole point of a stable turn id is that the second
            // attempt collapses onto the first. It only collapses when it is
            // genuinely the same submission, byte for byte — a *different*
            // turn claiming an existing id is still refused, and so is anything
            // that is not the regular file this function writes.
            if metadata.file_type().is_file()
                && std::fs::read(path).is_ok_and(|existing| existing == bytes)
            {
                return Ok(Published::AlreadyPublishedIdentically);
            }
            return Err(format!(
                "Desktop submission '{}' already exists; refusing to overwrite it",
                path.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "Failed to inspect desktop submission '{}': {error}",
                path.display()
            ));
        }
    }

    let temporary = parent.join(format!(
        ".desktop-submission-{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        format!("Failed to exclusively create desktop submission temp file: {error}")
    })?;
    let write_result = (|| -> std::io::Result<()> {
        if !file.metadata()?.file_type().is_file() {
            return Err(std::io::Error::other(
                "desktop submission temp is not a regular file",
            ));
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = write_result {
        drop(file);
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Failed to write desktop submission: {error}"));
    }
    drop(file);

    // Publishing with a hard link is an atomic create-new operation: an
    // attacker-created symlink or a concurrent same-turn submission makes it
    // fail with AlreadyExists and is never followed or overwritten.
    if let Err(error) = std::fs::hard_link(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("Failed to publish desktop submission: {error}"));
    }
    if let Err(error) = std::fs::remove_file(&temporary) {
        let _ = std::fs::remove_file(path);
        return Err(format!(
            "Failed to finalize desktop submission temp cleanup: {error}"
        ));
    }
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Failed to sync desktop submission directory: {error}"))?;
    #[cfg(not(unix))]
    let _ = parent;
    Ok(Published::Created)
}

fn daemon_ready(value: &Value) -> Result<(), String> {
    if value.get("installed").and_then(Value::as_bool) != Some(true) {
        return Err("The M6A resident runner is not installed for this profile".to_string());
    }
    if value.get("service_running").and_then(Value::as_bool) != Some(true)
        || value.get("heartbeat_fresh").and_then(Value::as_bool) != Some(true)
    {
        return Err(
            "The installed M6A resident runner is not healthy; start it before sending this turn"
                .to_string(),
        );
    }
    // Kept as its own check even though `backpressure.reason == "kill_switch"` now
    // says the same thing: the signal is allowed to be absent (an older sidecar,
    // per `backpressure_signal`) and losing the kill-switch refusal in that case
    // would be a real regression, while this boolean is a required status field.
    if value.get("kill_switch").and_then(Value::as_bool) == Some(true) {
        return Err("The M6A global kill switch is engaged".to_string());
    }
    // This is an *interactive* producer, so the two backpressure states are not
    // symmetric: `closed` refuses, but `slow` deliberately proceeds — a person is
    // waiting on this turn and has nothing to defer it to, so deferring would be a
    // refusal they never asked for. (Batch producers make the opposite call; see
    // `m5_delivery::reviewer::patch_backpressure`.) Refusing `closed` here only
    // buys a better message and saves writing the snapshot: the daemon's own
    // `enqueue` is the guard and would refuse this run anyway.
    if let Some(signal) = crate::daemon_commands::backpressure_signal(value) {
        if signal.state == crate::daemon_commands::DesktopBackpressureState::Closed {
            return Err(signal
                .detail
                .unwrap_or_else(|| "The M6A resident runner is not accepting work".to_string()));
        }
    }
    Ok(())
}

#[tauri::command]
pub async fn m6a_desktop_turn_submit(
    request: DesktopTurnSubmitRequest,
) -> Result<DesktopTurnSubmitResponse, String> {
    let bytes = validate_request(&request)?;
    let status_text = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ])
    .await?;
    let status: Value = serde_json::from_str(status_text.trim())
        .map_err(|error| format!("Invalid daemon status JSON: {error}"))?;
    daemon_ready(&status)?;

    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve app data directory".to_string())?;
    let path: PathBuf = app_data
        .join("daemon")
        .join("desktop-submissions")
        .join(format!("{}.json", request.turn_id));
    let published = publish_private_snapshot(&path, &bytes)?;

    // Submitted as a conversation turn rather than straight onto the queue: the
    // operator pressing Send is an origin like any other, and the turn is
    // durably accepted — under the turn id the webview generated and keeps
    // across retries — before the agent starts.
    let session_id = request
        .recipe
        .desktop_turn
        .as_ref()
        .map(|snapshot| snapshot.session_id.clone())
        .unwrap_or_default();
    let source = request_source(&request)?;
    let output = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "run".to_string(),
        path.to_string_lossy().into_owned(),
        "--ingress-source".to_string(),
        source.to_string(),
        "--ingress-account".to_string(),
        session_id.clone(),
        "--ingress-event".to_string(),
        request.turn_id.clone(),
        "--ingress-session".to_string(),
        format!("{source}:{session_id}"),
        "--priority".to_string(),
        "100".to_string(),
        "--max-attempts".to_string(),
        "1".to_string(),
        "--max-runtime-seconds".to_string(),
        request
            .recipe
            .timeout_seconds
            .unwrap_or(30 * 60)
            .to_string(),
        "--branch-prefix".to_string(),
        "desktop/".to_string(),
        "--allow-commit".to_string(),
        "false".to_string(),
        "--json".to_string(),
    ])
    .await;
    // The daemon has copied the source into its own protected snapshot before
    // it prints a successful queue response, so this transport copy is never
    // needed after the fixed command returns. A retry that found the file
    // already there leaves it to whoever created it.
    if published == Published::Created {
        let _ = std::fs::remove_file(&path);
    }
    let value: Value = serde_json::from_str(output?.trim())
        .map_err(|error| format!("Invalid desktop queue JSON: {error}"))?;
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid desktop queue response: {error}"))
}

/// Moves a running desktop autonomous task to the same resident daemon queue
/// used by the CLI. The recipe contains the frozen coordinator snapshot; the
/// daemon never has to infer autonomy from a name or re-read desktop state.
#[tauri::command]
pub async fn autonomous_task_submit(
    request: AutonomousTaskSubmitRequest,
) -> Result<DesktopTurnSubmitResponse, String> {
    named_id(&request.task_id, "autonomous task")?;
    recipes::validate_recipe(&request.recipe)?;
    let snapshot = request.recipe.autonomous_task.as_ref().ok_or_else(|| {
        "Autonomous task submission requires an autonomous_task snapshot".to_string()
    })?;
    if snapshot.task_id != request.task_id {
        return Err("Autonomous task id differs from its immutable recipe snapshot".to_string());
    }
    let owner = snapshot.execution_owner.as_ref().ok_or_else(|| {
        "Autonomous task submission requires an execution owner lease".to_string()
    })?;
    let status_text = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "status".to_string(),
        "--json".to_string(),
    ])
    .await?;
    let status: Value = serde_json::from_str(status_text.trim())
        .map_err(|error| format!("Invalid daemon status JSON: {error}"))?;
    daemon_ready(&status)?;

    let app_data = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey data directory".to_string())?;
    let path = app_data
        .join("daemon")
        .join("autonomous-submissions")
        .join(format!("{}.json", request.task_id));
    let bytes = serde_json::to_vec_pretty(&request.recipe).map_err(|error| error.to_string())?;
    let published = publish_private_snapshot(&path, &bytes)?;
    let output = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "run".to_string(),
        path.to_string_lossy().into_owned(),
        "--run-key".to_string(),
        format!("autonomous-task:{}", request.task_id),
        "--priority".to_string(),
        "100".to_string(),
        "--max-attempts".to_string(),
        "1".to_string(),
        "--max-runtime-seconds".to_string(),
        request
            .recipe
            .timeout_seconds
            .unwrap_or(30 * 60)
            .to_string(),
        "--initially-paused".to_string(),
        "--json".to_string(),
    ])
    .await;
    if published == Published::Created {
        let _ = std::fs::remove_file(&path);
    }
    let value: Value = serde_json::from_str(output?.trim())
        .map_err(|error| format!("Invalid autonomous queue JSON: {error}"))?;
    let mut response: DesktopTurnSubmitResponse = serde_json::from_value(value)
        .map_err(|error| format!("Invalid autonomous queue response: {error}"))?;
    // Queueing is deliberately complete before ownership changes. The queued
    // job is parked at the daemon's queue-only boundary; after this CAS there
    // is no fallible activation step left that could strand a daemon-owned job
    // while the desktop resumes it.
    if let Some(previous) = snapshot.previous_execution_owner.as_ref() {
        recipes::transfer_autonomous_task_owner(&snapshot.task_id, previous, owner)?;
    } else {
        recipes::claim_autonomous_task_owner(&snapshot.task_id, owner)?;
    }
    let activation = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "resume".to_string(),
        response.run_id.clone(),
    ])
    .await;
    if let Err(error) = activation {
        let rollback = recipes::AutonomousTaskOwnerSnapshot {
            kind: "desktop".to_string(),
            instance_id: snapshot
                .previous_execution_owner
                .as_ref()
                .map(|previous| previous.instance_id.clone())
                .unwrap_or_else(|| format!("desktop-{}", snapshot.task_id)),
            lease_epoch: owner.lease_epoch.saturating_add(1),
            lease_expires_at_ms: owner.lease_expires_at_ms,
        };
        let _ = recipes::transfer_autonomous_task_owner(&snapshot.task_id, owner, &rollback);
        return Err(format!(
            "Could not activate parked autonomous daemon job: {error}"
        ));
    }
    response.state = "queued".to_string();
    Ok(response)
}

/// Fences every desktop-side mutation against the durable owner identity and
/// epoch. A lease renewal may change only the expiry; a different owner or
/// epoch is a hard stop, including for a stale in-flight tool call.
#[tauri::command]
pub fn autonomous_task_owner_fence(request: AutonomousTaskOwnerFenceRequest) -> Result<(), String> {
    named_id(&request.task_id, "autonomous task")?;
    if request.owner.kind != "desktop" || request.owner.lease_epoch == 0 {
        return Err("Desktop side effects require a valid desktop execution owner".to_string());
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| format!("Could not read wall clock: {error}"))?
        .as_millis() as u64;
    if recipes::autonomous_task_owner_epoch_matches(&request.task_id, &request.owner)?.is_none() {
        if request.owner.lease_epoch != 1 {
            return Err("Autonomous task owner fence lost its durable owner epoch".to_string());
        }
        recipes::claim_autonomous_task_owner(&request.task_id, &request.owner)?;
    }
    let _ = recipes::renew_autonomous_task_owner(
        &request.task_id,
        &request.owner,
        now.saturating_add(60_000),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-m6a-publish-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn daemon_readiness_fails_closed() {
        assert!(daemon_ready(&serde_json::json!({
            "installed": true,
            "service_running": true,
            "heartbeat_fresh": true,
            "kill_switch": false
        }))
        .is_ok());
        assert!(daemon_ready(&serde_json::json!({
            "installed": true,
            "service_running": false,
            "heartbeat_fresh": false,
            "kill_switch": false
        }))
        .is_err());
        assert!(daemon_ready(&serde_json::json!({
            "installed": true,
            "service_running": true,
            "heartbeat_fresh": true,
            "kill_switch": true
        }))
        .is_err());
    }

    /// The interactive asymmetry: `slow` proceeds, `closed` refuses.
    ///
    /// The `slow` half is the one worth a test. Treating it as a refusal is the
    /// easy mistake, and it would deny a turn the daemon is still accepting to a
    /// person sitting in front of the app with nothing to defer to.
    #[test]
    fn an_interactive_turn_proceeds_on_slow_and_refuses_on_closed() {
        let status = |backpressure: &str| -> Value {
            serde_json::from_str(&format!(
                r#"{{"installed":true,"service_running":true,"heartbeat_fresh":true,
                     "kill_switch":false,"backpressure":{backpressure}}}"#
            ))
            .unwrap()
        };

        // Verbatim CLI spelling — snake_case inside this object.
        assert!(daemon_ready(&status(
            r#"{"state":"slow","accepting":true,"reason":"queue_deep",
                "detail":"104 of 128 queue slots are in use; slow down",
                "retry_after_ms":26000,"queue_depth":104,"queue_capacity":128,
                "queued":100,"held":0}"#
        ))
        .is_ok());

        let refusal = daemon_ready(&status(
            r#"{"state":"closed","accepting":false,"reason":"queue_full",
                "detail":"128 of 128 queue slots are in use; wait for a run or cancel one",
                "retry_after_ms":32000,"queue_depth":128,"queue_capacity":128,
                "queued":124,"held":0}"#,
        ))
        .unwrap_err();
        // The daemon's own sentence, not one invented here.
        assert!(refusal.contains("128 of 128 queue slots"));

        // A signal that goes absent must never block the app.
        assert!(daemon_ready(&status("null")).is_ok());
    }

    #[test]
    fn desktop_ids_are_path_safe() {
        assert!(validate_id("turn-1234").is_ok());
        assert!(validate_id("../../recipe").is_err());
        assert!(validate_id("turn/recipe").is_err());
    }

    #[test]
    fn private_publish_refuses_destination_collisions() {
        let directory = test_directory();
        let destination = directory.join("submissions").join("turn.json");
        assert_eq!(
            publish_private_snapshot(&destination, b"first").unwrap(),
            Published::Created
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        let error = publish_private_snapshot(&destination, b"second").unwrap_err();
        assert!(error.contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        std::fs::remove_dir_all(directory).unwrap();
    }

    /// The retry case the stable turn id exists for.
    ///
    /// A bridge call can time out after the daemon already took the turn, so
    /// the webview submits the identical request again under the same id. That
    /// second attempt has to get as far as the daemon — which answers with the
    /// job it already has — instead of dying on a file that the first attempt
    /// is still using.
    #[test]
    fn resubmitting_the_identical_turn_rides_on_the_attempt_already_in_flight() {
        let directory = test_directory();
        let destination = directory.join("submissions").join("turn.json");
        assert_eq!(
            publish_private_snapshot(&destination, b"same bytes").unwrap(),
            Published::Created
        );
        assert_eq!(
            publish_private_snapshot(&destination, b"same bytes").unwrap(),
            Published::AlreadyPublishedIdentically
        );
        // A different turn claiming that id is still refused: collapsing on the
        // id alone would let one send overwrite another's snapshot.
        assert!(publish_private_snapshot(&destination, b"other bytes")
            .unwrap_err()
            .contains("refusing to overwrite"));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_bridge_may_only_claim_the_operators_own_surfaces() {
        let request = |source: Option<&str>| DesktopTurnSubmitRequest {
            turn_id: "turn-1".into(),
            recipe: serde_json::from_value(serde_json::json!({
                "version": 1,
                "name": "desktop-turn-1",
                "target": {"ollama": "qwen2.5:7b"},
                "permission_mode": "readonly",
                "prompt": "hello",
            }))
            .expect("recipe"),
            source: source.map(str::to_string),
        };

        assert_eq!(request_source(&request(None)).unwrap(), "desktop");
        assert_eq!(request_source(&request(Some("voice"))).unwrap(), "voice");
        // A webview must not be able to have its turn recorded as an
        // authenticated phone or a paired peer.
        for forged in ["peer", "mobile", "telephone", "messaging_channel"] {
            assert!(request_source(&request(Some(forged)))
                .unwrap_err()
                .contains(forged));
        }
    }

    #[cfg(unix)]
    #[test]
    fn private_publish_never_follows_a_destination_symlink() {
        use std::os::unix::fs::symlink;
        let directory = test_directory();
        let submissions = directory.join("submissions");
        std::fs::create_dir_all(&submissions).unwrap();
        let victim = directory.join("victim.txt");
        std::fs::write(&victim, b"untouched").unwrap();
        let destination = submissions.join("turn.json");
        symlink(&victim, &destination).unwrap();
        let error = publish_private_snapshot(&destination, b"attacker-data").unwrap_err();
        assert!(error.contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");
        std::fs::remove_dir_all(directory).unwrap();
    }
}

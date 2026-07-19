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
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub struct DesktopTurnSubmitResponse {
    pub job_id: String,
    pub run_id: String,
    pub state: String,
}

fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.contains("..")
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        Err("Desktop turn id is invalid".to_string())
    } else {
        Ok(())
    }
}

fn validate_request(request: &DesktopTurnSubmitRequest) -> Result<Vec<u8>, String> {
    validate_id(&request.turn_id)?;
    recipes::validate_recipe(&request.recipe)?;
    let snapshot =
        request.recipe.desktop_turn.as_ref().ok_or_else(|| {
            "Desktop daemon submission requires a desktop_turn snapshot".to_string()
        })?;
    if snapshot.turn_id != request.turn_id {
        return Err("Desktop turn id differs from its immutable recipe snapshot".to_string());
    }
    let bytes = serde_json::to_vec_pretty(&request.recipe).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_SUBMISSION_BYTES {
        return Err("Desktop daemon submission exceeds the 48 MiB transport limit".to_string());
    }
    Ok(bytes)
}

fn publish_private_snapshot(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
        Ok(_) => {
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
    Ok(())
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
    if value.get("kill_switch").and_then(Value::as_bool) == Some(true) {
        return Err("The M6A global kill switch is engaged".to_string());
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
    publish_private_snapshot(&path, &bytes)?;

    let output = crate::daemon_commands::command(vec![
        "daemon".to_string(),
        "run".to_string(),
        path.to_string_lossy().into_owned(),
        "--run-key".to_string(),
        format!("desktop-turn:{}", request.turn_id),
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
        "codex/desktop/".to_string(),
        "--allow-commit".to_string(),
        "false".to_string(),
        "--json".to_string(),
    ])
    .await;
    // The daemon has copied the source into its own protected snapshot before
    // it prints a successful queue response, so this transport copy is never
    // needed after the fixed command returns.
    let _ = std::fs::remove_file(&path);
    let value: Value = serde_json::from_str(output?.trim())
        .map_err(|error| format!("Invalid desktop queue JSON: {error}"))?;
    serde_json::from_value(value)
        .map_err(|error| format!("Invalid desktop queue response: {error}"))
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
        publish_private_snapshot(&destination, b"first").unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        let error = publish_private_snapshot(&destination, b"second").unwrap_err();
        assert!(error.contains("refusing to overwrite"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"first");
        std::fs::remove_dir_all(directory).unwrap();
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

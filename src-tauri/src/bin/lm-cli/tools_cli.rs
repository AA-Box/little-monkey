//! Agent file/shell tools for the CLI — ported 1:1 from
//! `src-tauri/src/tools.rs`'s logic, but reusing `little_monkey_lib::workspace`
//! directly (the actual sandboxing: canonicalization, `..`-traversal and
//! symlink-escape rejection) rather than re-implementing it. The only real
//! difference from the GUI's tools is how permission is asked for — see
//! `permission.rs` — since there's no window here to emit a
//! `permission://request` event to.

use std::process::Stdio;
use std::time::Duration;

use little_monkey_lib::{workspace, AppState};
use regex::Regex;
use walkdir::WalkDir;

use crate::permission::TerminalPermissions;

const GREP_SKIP_DIRS: [&str; 4] = [".git", "node_modules", "target", "dist"];
const GREP_MAX_MATCHES: usize = 200;
const SHELL_TIMEOUT: Duration = Duration::from_secs(120);

pub fn read_file(state: &AppState, path: &str) -> Result<String, String> {
    let (resolved, _) = workspace::resolve_path_and_root(state, path)?;
    if !resolved.is_file() {
        return Err(format!("'{path}' is not a file"));
    }
    std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{path}': {e}"))
}

pub fn list_dir(state: &AppState, path: &str) -> Result<Vec<serde_json::Value>, String> {
    let (resolved, _) = workspace::resolve_path_and_root(state, path)?;
    if !resolved.is_dir() {
        return Err(format!("'{path}' is not a directory"));
    }

    let read_dir = std::fs::read_dir(&resolved).map_err(|e| format!("Failed to list '{path}': {e}"))?;
    let mut entries = Vec::new();
    for entry in read_dir {
        let entry = entry.map_err(|e| format!("Failed to read entry in '{path}': {e}"))?;
        let metadata = entry
            .metadata()
            .map_err(|e| format!("Failed to stat entry in '{path}': {e}"))?;
        entries.push(serde_json::json!({
            "name": entry.file_name().to_string_lossy().to_string(),
            "is_dir": metadata.is_dir(),
            "size": metadata.len(),
        }));
    }
    entries.sort_by(|a, b| a["name"].as_str().unwrap_or_default().cmp(b["name"].as_str().unwrap_or_default()));
    Ok(entries)
}

pub fn grep(state: &AppState, pattern: &str, path: Option<&str>) -> Result<Vec<serde_json::Value>, String> {
    let regex = Regex::new(pattern).map_err(|e| format!("Invalid regex '{pattern}': {e}"))?;
    let (search_root, _display_root) = workspace::resolve_path_and_root(state, path.unwrap_or("."))?;

    let mut matches = Vec::new();
    let walker = WalkDir::new(&search_root).into_iter().filter_entry(|entry| {
        if entry.file_type().is_dir() {
            if let Some(name) = entry.file_name().to_str() {
                return !GREP_SKIP_DIRS.contains(&name);
            }
        }
        true
    });

    'outer: for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(content) => content,
            Err(_) => continue, // binary or unreadable — skip silently
        };
        let display_path = entry
            .path()
            .strip_prefix(&search_root)
            .unwrap_or_else(|_| entry.path())
            .to_string_lossy()
            .to_string();

        for (idx, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(serde_json::json!({ "file": display_path, "line": idx + 1, "text": line }));
                if matches.len() >= GREP_MAX_MATCHES {
                    break 'outer;
                }
            }
        }
    }
    Ok(matches)
}

pub async fn write_file(
    state: &AppState,
    perms: &mut TerminalPermissions,
    path: &str,
    content: &str,
) -> Result<String, String> {
    let detail = format!("Write {} bytes to {}", content.len(), path);
    perms.request("write_file", &detail).await?;

    let (resolved, _) = workspace::resolve_path_and_root(state, path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create parent directories for '{path}': {e}"))?;
    }
    std::fs::write(&resolved, content).map_err(|e| format!("Failed to write '{path}': {e}"))?;
    Ok(format!("Wrote {} bytes to {}", content.len(), path))
}

/// Same short diff-style preview as `tools.rs::build_diff_preview`, shown in
/// the permission prompt before an edit is applied.
fn build_diff_preview(old_string: &str, new_string: &str) -> String {
    const MAX_PREVIEW_LINES: usize = 6;
    const MAX_LINE_CHARS: usize = 120;

    fn truncate(line: &str) -> String {
        if line.chars().count() > MAX_LINE_CHARS {
            let mut truncated: String = line.chars().take(MAX_LINE_CHARS).collect();
            truncated.push('…');
            truncated
        } else {
            line.to_string()
        }
    }

    let mut preview = Vec::new();

    let old_lines: Vec<&str> = old_string.lines().collect();
    for line in old_lines.iter().take(MAX_PREVIEW_LINES) {
        preview.push(format!("- {}", truncate(line)));
    }
    if old_lines.len() > MAX_PREVIEW_LINES {
        preview.push(format!("  … ({} more removed lines)", old_lines.len() - MAX_PREVIEW_LINES));
    }

    let new_lines: Vec<&str> = new_string.lines().collect();
    for line in new_lines.iter().take(MAX_PREVIEW_LINES) {
        preview.push(format!("+ {}", truncate(line)));
    }
    if new_lines.len() > MAX_PREVIEW_LINES {
        preview.push(format!("  … ({} more added lines)", new_lines.len() - MAX_PREVIEW_LINES));
    }

    preview.join("\n")
}

pub async fn edit_file(
    state: &AppState,
    perms: &mut TerminalPermissions,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> Result<String, String> {
    if old_string.is_empty() {
        return Err("old_string must not be empty".to_string());
    }

    let (resolved, _) = workspace::resolve_path_and_root(state, path)?;
    if !resolved.is_file() {
        return Err(format!("'{path}' is not a file"));
    }

    let current = std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read '{path}': {e}"))?;
    let occurrences = current.matches(old_string).count();
    if occurrences == 0 {
        return Err(format!("old_string not found in '{path}'"));
    }
    if occurrences > 1 {
        return Err(format!(
            "old_string appears {occurrences} times in '{path}'; it must be unique. Include more surrounding context."
        ));
    }

    let preview = build_diff_preview(old_string, new_string);
    let detail = format!("Edit {path}\n{preview}");
    perms.request("edit_file", &detail).await?;

    let updated = current.replacen(old_string, new_string, 1);
    std::fs::write(&resolved, &updated).map_err(|e| format!("Failed to write '{path}': {e}"))?;
    Ok(format!("Edited {path}"))
}

pub async fn run_shell(
    state: &AppState,
    perms: &mut TerminalPermissions,
    command: &str,
    cwd: Option<&str>,
) -> Result<serde_json::Value, String> {
    perms.request("run_shell", command).await?;

    let cwd_path = match cwd {
        Some(c) => workspace::resolve_path_and_root(state, c)?.0,
        None => workspace::primary_root_canon(state)?,
    };

    // `sh` does not exist on Windows — use the platform's own command
    // interpreter there. Same rule as the GUI's tool_run_shell.
    #[cfg(target_os = "windows")]
    let (shell, shell_flag) = ("cmd", "/C");
    #[cfg(not(target_os = "windows"))]
    let (shell, shell_flag) = ("sh", "-c");

    let mut command_builder = tokio::process::Command::new(shell);
    command_builder
        .arg(shell_flag)
        .arg(command)
        .current_dir(&cwd_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The timeout below works by DROPPING the in-flight
        // `wait_with_output` future (and the child with it) — without this,
        // the spawned process would keep running orphaned after a timeout.
        .kill_on_drop(true);

    let child = command_builder.spawn().map_err(|e| format!("Failed to spawn shell: {e}"))?;

    let output = match tokio::time::timeout(SHELL_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => return Err(format!("Failed to run command: {e}")),
        Err(_) => return Err(format!("Command timed out after {} seconds", SHELL_TIMEOUT.as_secs())),
    };

    Ok(serde_json::json!({
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
        "code": output.status.code(),
    }))
}

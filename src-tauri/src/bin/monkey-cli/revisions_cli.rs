//! `monkey revisions` — the cross-entity view of the configuration history
//! (K24), reusing `little_monkey_lib::config_revisions::changes` directly.
//!
//! The CLI writes this history too (workflow definitions through
//! `WorkflowService`, MCP servers through `mcp_cli`), so the read belongs here
//! and not only in the desktop's history panel: a change made from a terminal
//! that can only be inspected from a WebView is half a record.
//!
//! Reads the active profile's data dir, the same one the desktop resolves —
//! `app_paths::data_dir()` is the profile chokepoint, so `monkey --profile x
//! revisions` shows x's history and not the default profile's.

use little_monkey_lib::config_revisions::{self, ChangeSet};

fn root() -> Result<std::path::PathBuf, String> {
    let dir = little_monkey_lib::app_paths::data_dir()
        .ok_or("Could not resolve the app data directory")?;
    Ok(config_revisions::revision_root(&dir))
}

fn when(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "unknown time".to_string())
}

/// Prints recent changes, or one change when `change` is given.
pub fn list(change: Option<&str>, limit: usize) -> Result<(), String> {
    let sets = config_revisions::changes(&root()?, change, limit).map_err(|e| e.to_string())?;
    if sets.is_empty() {
        println!(
            "{}",
            match change {
                Some(id) => format!("No change recorded with id {id}."),
                None => "No configuration revisions recorded yet.".to_string(),
            }
        );
        return Ok(());
    }
    for set in &sets {
        print_set(set);
    }
    Ok(())
}

fn print_set(set: &ChangeSet) {
    match &set.change_id {
        Some(id) => println!(
            "{}  change {}  ({} entit{})",
            when(set.created_at),
            id,
            set.entries.len(),
            if set.entries.len() == 1 { "y" } else { "ies" }
        ),
        // Not "one change that touched one thing" — a revision written before
        // change ids existed, whose siblings (if it had any) are unknowable.
        // Saying so is the honest answer; grouping it by timestamp would be a
        // guess printed as a fact.
        None => println!(
            "{}  not correlated (recorded before change ids)",
            when(set.created_at)
        ),
    }
    for entry in &set.entries {
        println!(
            "    {:<14} {:<32} r{:<4} {}",
            entry.kind, entry.entity_id, entry.revision.sequence, entry.revision.label
        );
    }
}

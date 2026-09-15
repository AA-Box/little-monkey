//! `monkey memory` subcommand parity for Memory Studio's lifecycle actions.
//! Reuses the library's `AppHandle`-free `*_impl` entry points directly —
//! same reasoning as `stacks_cli.rs`/`checkpoints_cli.rs` — against the very
//! same `<app-data>/memories.json` the desktop app writes, so a memory pinned
//! here is pinned there and vice versa.
//!
//! No `add`/`edit`/`delete` here: `tool_remember` already writes from the CLI
//! agent, and this subcommand exists for the lifecycle verbs Memory Studio
//! grew (pin, expire, merge) that had no headless equivalent at all.
//!
//! Path note: [`memories_path`] resolves `app_paths::data_dir()`, while
//! `main.rs`'s `compose_system_prompt_impl` is handed
//! `agent_config_roots().legacy`. Both are
//! `profiles::profile_root(base_data_dir, selected_id(registry))` — the same
//! directory, reached by two spellings — so `monkey memory` really does act
//! on the file the CLI's own prompt is composed from.

use std::path::{Path, PathBuf};

use clap::Subcommand;
use little_monkey_lib::memory::{self, MemoryEntry};

/// Resolves (creating the app-data dir if necessary) `<app-data>/memories.json`
/// — the same file `memory.rs::memories_file_path` resolves via an
/// `AppHandle`. Copied from `tools_cli.rs`'s helper of the same name.
fn memories_path() -> Option<PathBuf> {
    let dir = little_monkey_lib::app_paths::data_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.join("memories.json"))
}

#[derive(Subcommand, Debug)]
pub enum MemoryCmd {
    /// List every stored memory: id, scope, lifecycle flags, dates, text.
    List {
        /// Include memories retired by a merge (hidden by default — they
        /// exist only so `unmerge` can restore them).
        #[arg(long)]
        all: bool,
    },
    /// Pin a memory: folded into the prompt first, exempt from expiry and
    /// from the per-scope fact cap (20 pins per scope).
    Pin { id: String },
    /// Unpin a memory.
    Unpin { id: String },
    /// Set or clear a memory's expiry. An expired memory stops reaching
    /// prompts immediately but stays on disk until `monkey memory purge`.
    Expire {
        id: String,
        /// `YYYY-MM-DD` (expires at the end of that day) or a full RFC 3339
        /// UTC timestamp ending in `Z`.
        #[arg(long)]
        at: Option<String>,
        /// Clear the expiry instead of setting one.
        #[arg(long)]
        clear: bool,
    },
    /// Combine two or more memories from one scope into a single memory that
    /// keeps their ids. The originals are retired, not deleted.
    Merge {
        /// Two or more memory ids, all from the same scope.
        ids: Vec<String>,
        /// The combined text. Omitted, the originals' texts are joined.
        #[arg(long)]
        text: Option<String>,
    },
    /// Undo a merge: restore the originals and drop the merged memory.
    Unmerge { id: String },
    /// Permanently delete every expired memory, in every scope.
    Purge,
}

/// The lifecycle flags shown in `list`'s third column, in the order they
/// matter to the reader: whether the memory reaches a prompt at all, then
/// why not. Pure so it can be tested without a store.
fn flags(entry: &MemoryEntry, now: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    if entry.pinned {
        out.push("pinned");
    }
    if !entry.enabled {
        out.push("disabled");
    }
    if entry.retired_at.is_some() {
        out.push("retired");
    }
    if !entry.pinned && entry.expires_at.as_deref().is_some_and(|e| e <= now) {
        out.push("expired");
    }
    if !entry.merged_from.is_empty() {
        out.push("merged");
    }
    if out.is_empty() {
        "-".to_string()
    } else {
        out.join(",")
    }
}

/// Collapses every run of whitespace to one space. Memory text is
/// model-authored and only end-trimmed on the way in, so an embedded newline
/// or tab would otherwise break the tab-separated output — or let one memory
/// forge extra rows in it.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn path_or_err() -> Result<PathBuf, String> {
    memories_path().ok_or_else(|| "Could not resolve the app data directory".to_string())
}

/// Resolves which storage scope holds `id`, so a CLI user never has to type a
/// canonical project root.
fn scope_of(path: &Path, id: &str) -> Result<String, String> {
    memory::scope_of_impl(path, id)?.ok_or_else(|| format!("No memory with id '{id}'."))
}

/// The one scope every id in `ids` lives in. Resolving them all (rather than
/// trusting the first) is what makes a cross-scope merge report the refusal
/// `merge_impl` actually enforces, instead of "no memory with id …" from
/// looking the second id up under the first one's scope.
fn one_scope(path: &Path, ids: &[String]) -> Result<String, String> {
    let mut scope: Option<String> = None;
    for id in ids {
        let found = scope_of(path, id)?;
        match &scope {
            Some(first) if first != &found => {
                return Err(
                    "Memories can only be merged within one scope — every id must be in the same project, or all global."
                        .to_string(),
                );
            }
            Some(_) => {}
            None => scope = Some(found),
        }
    }
    scope.ok_or_else(|| "Merging needs at least two memories.".to_string())
}

/// The lines `list` prints, given a listing. Pure so the one branch that is
/// not just a `*_impl` call — hiding merge-retired rows unless `--all` — is
/// testable without `app_paths::data_dir()`.
fn list_rows(entries: &[MemoryEntry], all: bool, now: &str) -> Vec<String> {
    let rows: Vec<String> = entries
        .iter()
        .filter(|entry| all || entry.retired_at.is_none())
        .map(|entry| {
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}",
                entry.id,
                entry.project_root.as_deref().unwrap_or("global"),
                flags(entry, now),
                entry.created_at,
                entry.last_used_at.as_deref().unwrap_or("never"),
                one_line(&entry.text),
            )
        })
        .collect();
    if rows.is_empty() {
        return vec!["No memories stored yet.".to_string()];
    }
    rows
}

pub fn list(all: bool) -> Result<(), String> {
    let path = path_or_err()?;
    let entries = memory::list_all_impl(&path)?;
    for row in list_rows(&entries, all, &memory::now_rfc3339()) {
        println!("{row}");
    }
    Ok(())
}

pub fn set_pinned(id: &str, pinned: bool) -> Result<(), String> {
    let path = path_or_err()?;
    let scope = scope_of(&path, id)?;
    let fact = memory::set_pinned_impl(&path, &scope, id, pinned)?;
    println!(
        "{} {}",
        if fact.pinned { "Pinned" } else { "Unpinned" },
        fact.id
    );
    Ok(())
}

/// Which expiry `expire` should write: `Some(stamp)` to set one, `None` to
/// clear. Pure so the argument rules are testable without a store. `--at`
/// together with `--clear` is refused rather than letting one win silently —
/// the two ask for opposite end states.
fn expiry_arg<'a>(at: Option<&'a str>, clear: bool) -> Result<Option<&'a str>, String> {
    match (at, clear) {
        (Some(_), true) => {
            Err("Pass either --at <YYYY-MM-DD|RFC3339> or --clear, not both.".to_string())
        }
        (Some(value), false) => Ok(Some(value)),
        (None, true) => Ok(None),
        (None, false) => Err(
            "Pass --at <YYYY-MM-DD|RFC3339> to set an expiry, or --clear to remove one."
                .to_string(),
        ),
    }
}

pub fn expire(id: &str, at: Option<&str>, clear: bool) -> Result<(), String> {
    let value = expiry_arg(at, clear)?;
    let path = path_or_err()?;
    let scope = scope_of(&path, id)?;
    let fact = memory::set_expiry_impl(&path, &scope, id, value)?;
    match fact.expires_at {
        // A pinned fact is exempt from expiry (`reaches_prompt`), so saying
        // only "expires <when>" would state an end state the store will not
        // honour — while the date is still stored, and applies again on
        // unpin. `MemoryStudioPanel`'s `expiryPinnedRetainedHint` says the
        // same thing on the desktop side.
        Some(when) if fact.pinned => println!(
            "{} expires {} — but it is pinned, so it stays in the prompt until you `monkey memory unpin {}`.",
            fact.id, when, fact.id
        ),
        Some(when) => println!("{} expires {}", fact.id, when),
        None => println!("{} no longer expires", fact.id),
    }
    Ok(())
}

pub fn merge(ids: &[String], text: Option<&str>) -> Result<(), String> {
    let path = path_or_err()?;
    let scope = one_scope(&path, ids)?;
    let fact = memory::merge_impl(&path, &scope, ids, text)?;
    println!(
        "Merged {} memories into {} (the originals are retired, not deleted — `monkey memory unmerge {}` restores them)",
        fact.merged_from.len(),
        fact.id,
        fact.id
    );
    Ok(())
}

pub fn unmerge(id: &str) -> Result<(), String> {
    let path = path_or_err()?;
    let scope = scope_of(&path, id)?;
    let restored = memory::unmerge_impl(&path, &scope, id)?;
    println!("Restored {restored} original memories.");
    Ok(())
}

pub fn purge() -> Result<(), String> {
    let path = path_or_err()?;
    let removed = memory::purge_expired_impl(&path)?;
    println!("Purged {removed} expired memories.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> MemoryEntry {
        MemoryEntry {
            id: "id-1".to_string(),
            text: "plain".to_string(),
            source: "agent".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            enabled: true,
            source_turn_id: None,
            pinned: false,
            expires_at: None,
            last_used_at: None,
            merged_from: Vec::new(),
            merged_into: None,
            retired_at: None,
            scope: "global".to_string(),
            project_root: None,
        }
    }

    const NOW: &str = "2026-06-01T00:00:00.000Z";

    #[test]
    fn flags_names_every_lifecycle_state_and_nothing_for_a_plain_memory() {
        assert_eq!(flags(&entry(), NOW), "-");

        let mut pinned = entry();
        pinned.pinned = true;
        assert_eq!(flags(&pinned, NOW), "pinned");

        let mut disabled = entry();
        disabled.enabled = false;
        assert_eq!(flags(&disabled, NOW), "disabled");

        let mut expired = entry();
        expired.expires_at = Some("2000-01-01T00:00:00.000Z".to_string());
        assert_eq!(flags(&expired, NOW), "expired");

        let mut not_yet = entry();
        not_yet.expires_at = Some("2999-01-01T00:00:00.000Z".to_string());
        assert_eq!(flags(&not_yet, NOW), "-");

        // A pinned memory is exempt from expiry, so it is never "expired".
        let mut pinned_stale = entry();
        pinned_stale.pinned = true;
        pinned_stale.expires_at = Some("2000-01-01T00:00:00.000Z".to_string());
        assert_eq!(flags(&pinned_stale, NOW), "pinned");

        let mut retired = entry();
        retired.retired_at = Some(NOW.to_string());
        retired.merged_into = Some("other".to_string());
        assert_eq!(flags(&retired, NOW), "retired");

        let mut merged = entry();
        merged.merged_from = vec!["a".to_string(), "b".to_string()];
        assert_eq!(flags(&merged, NOW), "merged");
    }

    /// A store of our own, so the id->scope resolution can be exercised
    /// without `app_paths::data_dir()` (which the verbs resolve internally).
    fn temp_store() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_memory_cli_test_{}_{}.json",
            std::process::id(),
            nanos
        ))
    }

    #[test]
    fn expiry_arg_refuses_at_and_clear_together_and_neither_alone() {
        assert_eq!(
            expiry_arg(Some("2026-12-31"), false),
            Ok(Some("2026-12-31"))
        );
        assert_eq!(expiry_arg(None, true), Ok(None));
        // `--at` must not be silently discarded by `--clear`.
        assert!(expiry_arg(Some("2026-12-31"), true)
            .unwrap_err()
            .contains("not both"));
        assert!(expiry_arg(None, false).unwrap_err().contains("--clear"));
    }

    #[test]
    fn one_scope_resolves_a_single_scope_and_names_a_cross_scope_merge() {
        let path = temp_store();
        let global = memory::add_fact_impl(&path, "__global__", "a global fact", "user", None)
            .expect("global fact");
        let second =
            memory::add_fact_impl(&path, "__global__", "another global fact", "user", None)
                .expect("second global fact");
        let project = memory::add_fact_impl(&path, "/tmp/project", "a project fact", "user", None)
            .expect("project fact");

        assert_eq!(
            one_scope(&path, &[global.id.clone(), second.id.clone()]),
            Ok("__global__".to_string())
        );
        // The refusal names the real reason instead of "no memory with id".
        let err = one_scope(&path, &[global.id.clone(), project.id.clone()]).unwrap_err();
        assert!(err.contains("within one scope"), "unexpected error: {err}");
        assert!(one_scope(&path, &[])
            .unwrap_err()
            .contains("at least two memories"));
        assert!(one_scope(&path, &["nope".to_string()])
            .unwrap_err()
            .contains("No memory with id"));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn list_rows_hides_a_merge_retired_memory_unless_all_is_passed() {
        let mut plain = entry();
        plain.id = "keeper".to_string();
        let mut retired = entry();
        retired.id = "retired-one".to_string();
        retired.retired_at = Some(NOW.to_string());
        retired.merged_into = Some("keeper".to_string());
        let entries = vec![plain, retired];

        let default = list_rows(&entries, false, NOW);
        assert_eq!(default.len(), 1);
        assert!(default[0].starts_with("keeper\tglobal\t-\t"));

        let all = list_rows(&entries, true, NOW);
        assert_eq!(all.len(), 2);
        assert!(all[1].contains("\tretired\t"));

        // A store holding nothing but retired rows still says so out loud
        // rather than printing an empty listing that reads like "no store".
        assert_eq!(
            list_rows(&entries[1..], false, NOW),
            vec!["No memories stored yet.".to_string()]
        );
    }

    #[test]
    fn one_line_collapses_text_that_could_forge_extra_rows() {
        assert_eq!(one_line("plain text"), "plain text");
        assert_eq!(one_line("a\tb\nid\tscope\tflags"), "a b id scope flags");
        assert_eq!(one_line("  padded \n\n lines  "), "padded lines");
    }
}

//! Read-only access to `MONKEY.md` project-instruction files: a global file
//! at `<app_data>/MONKEY.md` (applies to every workspace) plus one optional
//! file at the top of each attached workspace root (primary and every
//! secondary), mirroring how CLAUDE.md works for this app's multi-root
//! workspace model.
//!
//! Every file is plain, user-owned markdown with no schema. A missing file
//! is simply absent from the result — never an error, since a workspace with
//! no rules configured yet (the common case) must not fail every turn's
//! prompt build. Content is capped at [`MAX_RULE_CHARS`] characters so a
//! pathological file can't blow out the model's context window; a truncated
//! file gets a visible marker appended and `truncated: true` so the UI/model
//! both know it was cut.
//!
//! Follows the `checkpoints.rs`/`sessions.rs` AppHandle-free `*_impl` split:
//! [`read_rules_impl`] takes plain paths so it's directly unit-testable and
//! reusable from `lm-cli` (slice 5), while [`rules_read`] is the thin
//! `#[tauri::command]` wrapper that resolves the global path and the
//! attached roots.

use std::path::{Path, PathBuf};

use tauri::Manager;

use crate::{workspace, AppState};

/// Filename looked for at the global app-data dir and at the top of every
/// attached workspace root.
const RULE_FILE_NAME: &str = "MONKEY.md";

/// Per-file character cap enforced on read — see module docs.
const MAX_RULE_CHARS: usize = 16_000;

/// Appended to a file's content when it's cut at [`MAX_RULE_CHARS`], so both
/// the model and anyone reading the raw prompt can see it was truncated.
const TRUNCATION_MARKER: &str = "\n\n[... truncated: file exceeds the 16,000 character limit ...]";

/// One MONKEY.md file found on disk, ready to inject into the system prompt.
#[derive(serde::Serialize, Debug, Clone, PartialEq, Eq)]
pub struct RuleFile {
    /// `"global"` for the single app-data file, `"project"` for a workspace
    /// root's file.
    pub scope: String,
    /// Display label: `"global"` for the global file, otherwise the owning
    /// workspace root's label (primary or secondary).
    pub label: String,
    /// Absolute path the content was read from.
    pub path: String,
    pub content: String,
    pub truncated: bool,
}

/// Reads and caps a single MONKEY.md file. Returns `None` if it doesn't
/// exist or can't be read for any other reason — a broken rules file must
/// never turn into a hard error for the turn that needed the prompt built.
fn read_rule_file(path: &Path) -> Option<(String, bool)> {
    let raw = std::fs::read_to_string(path).ok()?;
    if raw.chars().count() > MAX_RULE_CHARS {
        let truncated: String = raw.chars().take(MAX_RULE_CHARS).collect();
        Some((format!("{truncated}{TRUNCATION_MARKER}"), true))
    } else {
        Some((raw, false))
    }
}

/// Core logic behind [`rules_read`], parameterized by plain paths so it
/// needs no `AppHandle`/`State` and is directly unit-testable (and reusable
/// from `lm-cli`). `global_path` is the full path to the global MONKEY.md
/// (not just its directory); `roots` mirrors [`workspace::all_roots`]'s
/// `(canonical_path, label, is_primary)` triples, primary first. Order of
/// the returned list is global first, then roots in the order given (which
/// `all_roots` already returns primary-first).
pub fn read_rules_impl(global_path: &Path, roots: &[(PathBuf, String, bool)]) -> Vec<RuleFile> {
    let mut files = Vec::new();

    if let Some((content, truncated)) = read_rule_file(global_path) {
        files.push(RuleFile {
            scope: "global".to_string(),
            label: "global".to_string(),
            path: global_path.to_string_lossy().to_string(),
            content,
            truncated,
        });
    }

    for (root, label, _is_primary) in roots {
        let path = root.join(RULE_FILE_NAME);
        if let Some((content, truncated)) = read_rule_file(&path) {
            files.push(RuleFile {
                scope: "project".to_string(),
                label: label.clone(),
                path: path.to_string_lossy().to_string(),
                content,
                truncated,
            });
        }
    }

    files
}

/// Every MONKEY.md file currently in effect: the global app-data file plus
/// one per attached workspace root, absent entries skipped. Never fails just
/// because no workspace is open yet — that just means no project-scope
/// entries (the global file, if any, still applies).
#[tauri::command]
pub fn rules_read(app: tauri::AppHandle, state: tauri::State<'_, AppState>) -> Result<Vec<RuleFile>, String> {
    let global_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    let global_path = global_dir.join(RULE_FILE_NAME);
    let roots = workspace::all_roots(state.inner()).unwrap_or_default();
    Ok(read_rules_impl(&global_path, &roots))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_rules_test_{}_{}_{}",
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn missing_global_file_is_absent_not_an_error() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md"); // never written
        let files = read_rules_impl(&global_path, &[]);
        assert!(files.is_empty(), "missing global file must not produce any entry, got {files:?}");
    }

    #[test]
    fn present_global_file_is_read_untruncated() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md");
        std::fs::write(&global_path, "Always write tests.").unwrap();

        let files = read_rules_impl(&global_path, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].scope, "global");
        assert_eq!(files[0].label, "global");
        assert_eq!(files[0].content, "Always write tests.");
        assert!(!files[0].truncated);
    }

    #[test]
    fn oversized_file_is_truncated_with_visible_marker() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md");
        let huge = "a".repeat(MAX_RULE_CHARS + 500);
        std::fs::write(&global_path, &huge).unwrap();

        let files = read_rules_impl(&global_path, &[]);
        assert_eq!(files.len(), 1);
        assert!(files[0].truncated);
        assert!(
            files[0].content.contains("truncated"),
            "truncated content must contain a visible marker"
        );
        // The un-marked prefix must be capped at exactly MAX_RULE_CHARS chars.
        let prefix_len = files[0].content.len() - TRUNCATION_MARKER.len();
        assert_eq!(prefix_len, MAX_RULE_CHARS);
    }

    #[test]
    fn multiple_secondary_roots_each_produce_a_labeled_entry() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md"); // absent — irrelevant here

        let primary = TempDir::new();
        std::fs::write(primary.path.join("MONKEY.md"), "Primary rules.").unwrap();
        let secondary_a = TempDir::new();
        std::fs::write(secondary_a.path.join("MONKEY.md"), "Secondary A rules.").unwrap();
        let secondary_b = TempDir::new();
        // secondary_b intentionally has no MONKEY.md — must be silently absent.

        let roots = vec![
            (primary.path.clone(), "project".to_string(), true),
            (secondary_a.path.clone(), "libs".to_string(), false),
            (secondary_b.path.clone(), "docs".to_string(), false),
        ];

        let files = read_rules_impl(&global_path, &roots);
        assert_eq!(files.len(), 2, "only roots with a MONKEY.md present should produce an entry: {files:?}");
        assert_eq!(files[0].scope, "project");
        assert_eq!(files[0].label, "project");
        assert_eq!(files[0].content, "Primary rules.");
        assert_eq!(files[1].label, "libs");
        assert_eq!(files[1].content, "Secondary A rules.");
    }
}

//! Read-only access to `MONKEY.md` project-instruction files: a global file
//! at `<app_data>/MONKEY.md` (applies to every workspace) plus one optional
//! file at the top of each attached workspace root (primary and every
//! secondary), mirroring how CLAUDE.md works for this app's multi-root
//! workspace model.
//!
//! Each scope also falls back to `AGENTS.md` when no `MONKEY.md` is present
//! there, so third-party tooling that ships instructions as an `AGENTS.md`
//! (e.g. the ponytail agent-rules plugin) is picked up without the user
//! having to rename or duplicate the file. `MONKEY.md` always wins when both
//! exist in the same scope. Writes (`rules_write`) are unaffected — the
//! Settings editor always writes `MONKEY.md`.
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
//! reusable from `monkey-cli` (slice 5), while [`rules_read`] is the thin
//! `#[tauri::command]` wrapper that resolves the global path and the
//! attached roots.

use std::path::{Path, PathBuf};

use crate::config_revisions::{self, RecordRequest};
use crate::profiles::ProfileScopedPaths;
use crate::{workspace, AppState};

/// Revision kind for a rules/memory file (roadmap K24).
///
/// Its own kind rather than sharing `prompt-library`'s, because "restore this"
/// means writing a different file for each and the history panel is keyed on the
/// pair: a shared kind would put a workspace's MONKEY.md into the same list as a
/// persona.
pub const RULES_REVISION_KIND: &str = "rules";

/// The revision entity a rules file is filed under.
///
/// The *path* rather than a label, and that is load-bearing: two attached roots
/// can both be called `src`, and a label-keyed history would merge their files
/// into one log where a restore would put the wrong content in the wrong repo.
/// `config_revisions::slug` already hashes the id, so a path with separators in
/// it is a safe single filename.
pub fn rules_entity_id(scope: &str, target: &Path) -> String {
    match scope {
        // One global file per profile, and its path already carries the profile
        // data root — so two profiles keep separate histories for free, which is
        // the same boundary every other profile-scoped store draws.
        "global" => format!("global:{}", target.display()),
        _ => format!("project:{}", target.display()),
    }
}

/// Filename looked for at the global app-data dir and at the top of every
/// attached workspace root.
const RULE_FILE_NAME: &str = "MONKEY.md";

/// Fallback filename checked in the same scope when [`RULE_FILE_NAME`] is
/// absent — the de facto standard instructions filename used by other agent
/// tooling (Codex, Cursor's AGENTS.md support, the ponytail plugin, etc).
const AGENTS_FILE_NAME: &str = "AGENTS.md";

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

/// Reads `primary_path` (a `MONKEY.md` path); if absent, tries the sibling
/// `AGENTS.md` in the same directory instead. Returns the path actually
/// read alongside its (possibly truncated) content, so callers can report
/// where the content came from.
fn read_rule_file_with_fallback(primary_path: &Path) -> Option<(PathBuf, String, bool)> {
    if let Some((content, truncated)) = read_rule_file(primary_path) {
        return Some((primary_path.to_path_buf(), content, truncated));
    }
    let fallback_path = primary_path.with_file_name(AGENTS_FILE_NAME);
    read_rule_file(&fallback_path).map(|(content, truncated)| (fallback_path, content, truncated))
}

/// Core logic behind [`rules_read`], parameterized by plain paths so it
/// needs no `AppHandle`/`State` and is directly unit-testable (and reusable
/// from `monkey-cli`). `global_path` is the full path to the global MONKEY.md
/// (not just its directory); `roots` mirrors [`workspace::all_roots`]'s
/// `(canonical_path, label, is_primary)` triples, primary first. Order of
/// the returned list is global first, then roots in the order given (which
/// `all_roots` already returns primary-first). Each scope falls back to a
/// sibling `AGENTS.md` when its `MONKEY.md` is absent — see module docs.
pub fn read_rules_impl(global_path: &Path, roots: &[(PathBuf, String, bool)]) -> Vec<RuleFile> {
    let mut files = Vec::new();

    if let Some((path, content, truncated)) = read_rule_file_with_fallback(global_path) {
        files.push(RuleFile {
            scope: "global".to_string(),
            label: "global".to_string(),
            path: path.to_string_lossy().to_string(),
            content,
            truncated,
        });
    }

    for (root, label, _is_primary) in roots {
        let path = root.join(RULE_FILE_NAME);
        if let Some((resolved_path, content, truncated)) = read_rule_file_with_fallback(&path) {
            files.push(RuleFile {
                scope: "project".to_string(),
                label: label.clone(),
                path: resolved_path.to_string_lossy().to_string(),
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
pub fn rules_read(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<RuleFile>, String> {
    let global_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    let global_path = global_dir.join(RULE_FILE_NAME);
    let roots = workspace::all_roots(state.inner()).unwrap_or_default();
    Ok(read_rules_impl(&global_path, &roots))
}

/// Core logic behind [`rules_write`], parameterized by plain `AppState` +
/// global path (no `AppHandle`) so it's directly unit-testable, mirroring
/// the `*_impl` split used throughout `checkpoints.rs`/`workspace.rs`.
///
/// `scope: "global"` writes `global_path` (`<app_data>/MONKEY.md`),
/// ignoring `root_path`. `scope: "project"` requires `root_path` — the same
/// resolvable path a tool call would use (e.g. `"MONKEY.md"` for the
/// primary root, or `"<label>/MONKEY.md"` for an attached secondary root's
/// label) — and resolves it through `workspace::resolve_path_and_root` so a
/// caller can never write outside an attached root.
fn write_rules_impl(
    state: &AppState,
    revision_root: &Path,
    global_path: &Path,
    scope: &str,
    root_path: Option<&str>,
    content: &str,
    base_revision_id: Option<String>,
) -> Result<String, String> {
    let target = match scope {
        "global" => global_path.to_path_buf(),
        "project" => {
            let root_path = root_path
                .ok_or_else(|| "root_path is required when scope is \"project\"".to_string())?;
            let (resolved, _root_canon) = workspace::resolve_path_and_root(state, root_path)?;
            // `resolve_path_and_root` only guarantees the resolved path stays
            // inside an attached workspace root — it imposes no constraint on
            // the filename, so without this check `rules_write` would be an
            // ungated arbitrary-file-overwrite primitive for any file under
            // any attached root (this command is deliberately NOT routed
            // through `permissions::request_permission` — see the doc comment
            // on `rules_write` below). The Settings editor only ever passes a
            // path resolving to `MONKEY.md`; enforce that here too so the
            // backend contract doesn't silently rely on that being the only
            // caller.
            if resolved.file_name().and_then(|n| n.to_str()) != Some(RULE_FILE_NAME) {
                return Err(format!(
                    "Refusing to write '{}': project-scope rules_write may only write a '{}' file",
                    resolved.display(),
                    RULE_FILE_NAME
                ));
            }
            resolved
        }
        other => {
            return Err(format!(
                "Unknown rules scope '{}' (expected \"global\" or \"project\")",
                other
            ))
        }
    };

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            format!(
                "Failed to create parent directories for '{}': {}",
                target.display(),
                e
            )
        })?;
    }
    // Revision first, exactly as `prompts::prompts_save` orders it, and for the
    // same reason: recording is the only step that can *reject* the write, and
    // rejecting after the file is already overwritten would defeat the point of
    // detecting the conflict at all.
    //
    // A rules file was the last-write-wins case this item named. Two windows —
    // or the desktop and the CLI — editing the same MONKEY.md silently kept
    // whichever saved second, with no record that the other edit had existed.
    let revision = config_revisions::record(
        revision_root,
        RULES_REVISION_KIND,
        &rules_entity_id(scope, &target),
        RecordRequest {
            branch: None,
            base_revision_id,
            label: "Saved".to_string(),
            content: content.to_string(),
        },
    )
    .map_err(|e| e.to_string())?;
    std::fs::write(&target, content)
        .map_err(|e| format!("Failed to write '{}': {}", target.display(), e))?;
    Ok(revision.revision_id)
}

/// The rules file a scope resolves to, without writing anything.
///
/// Shared by [`rules_write`] and [`rules_current_revision`] so a caller reading
/// its base revision and a caller saving against that base agree on which entity
/// they are talking about — deriving the id twice from two different resolutions
/// is exactly how a conflict check ends up silently checking the wrong log.
fn rules_target(
    state: &AppState,
    global_path: &Path,
    scope: &str,
    root_path: Option<&str>,
) -> Result<PathBuf, String> {
    match scope {
        "global" => Ok(global_path.to_path_buf()),
        "project" => {
            let root_path = root_path
                .ok_or_else(|| "root_path is required when scope is \"project\"".to_string())?;
            let (resolved, _root_canon) = workspace::resolve_path_and_root(state, root_path)?;
            Ok(resolved)
        }
        other => Err(format!(
            "Unknown rules scope '{}' (expected \"global\" or \"project\")",
            other
        )),
    }
}

/// Save a MONKEY.md file from the Settings "Rules" editor. A direct,
/// human-initiated UI action — like `git_commit`/`checkpoint_revert`,
/// intentionally NOT routed through `permissions::request_permission`; the
/// user is editing their own instructions file by hand, not asking the agent
/// to write it.
/// `base_revision_id` is the revision the caller last saw. Supplying it opts into
/// the concurrent-edit check: if another window (or the CLI) saved this same
/// rules file since, the write is REFUSED with a `conflict:`-prefixed error and
/// the file on disk is left untouched, instead of the last writer silently
/// winning. `None` keeps the old unconditional behaviour, which is what a first
/// save, an import, and a restore want.
///
/// Returns the new revision id, which the caller holds as its base for the next
/// save.
#[tauri::command]
pub fn rules_write(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    scope: String,
    root_path: Option<String>,
    content: String,
    base_revision_id: Option<String>,
) -> Result<String, String> {
    let global_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    let global_path = global_dir.join(RULE_FILE_NAME);
    let revision_root = config_revisions::revision_root(&global_dir);
    write_rules_impl(
        state.inner(),
        &revision_root,
        &global_path,
        &scope,
        root_path.as_deref(),
        &content,
        base_revision_id,
    )
}

/// The current revision id of one rules file, so a window that just hydrated
/// from disk knows what base to save against.
///
/// `None` when nothing has been recorded yet — a file edited before this existed,
/// or one that has never been saved through the app.
#[tauri::command]
pub fn rules_current_revision(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    scope: String,
    root_path: Option<String>,
) -> Result<Option<String>, String> {
    let global_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    let global_path = global_dir.join(RULE_FILE_NAME);
    let target = rules_target(state.inner(), &global_path, &scope, root_path.as_deref())?;
    let root = config_revisions::revision_root(&global_dir);
    config_revisions::head(
        &root,
        RULES_REVISION_KIND,
        &rules_entity_id(&scope, &target),
        None,
    )
    .map(|head| head.map(|revision| revision.revision_id))
    .map_err(|e| e.to_string())
}

/// The revision entity id for one rules file, so the history panel can ask for
/// the right log without re-deriving the path rule in TypeScript.
#[tauri::command]
pub fn rules_revision_entity(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    scope: String,
    root_path: Option<String>,
) -> Result<String, String> {
    let global_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    let global_path = global_dir.join(RULE_FILE_NAME);
    let target = rules_target(state.inner(), &global_path, &scope, root_path.as_deref())?;
    Ok(rules_entity_id(&scope, &target))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// [`write_rules_impl`] with a throwaway revision root and no base revision.
    ///
    /// Every case below is about *where the file lands*, and threading a
    /// revision root plus a `None` base through each of them would bury the
    /// cases that are genuinely about versioning — which call the real function.
    fn write_rules(
        state: &AppState,
        global_path: &Path,
        scope: &str,
        root_path: Option<&str>,
        content: &str,
    ) -> Result<String, String> {
        let revision_root = global_path
            .parent()
            .unwrap_or(Path::new("."))
            .join("config-revisions-test");
        write_rules_impl(
            state,
            &revision_root,
            global_path,
            scope,
            root_path,
            content,
            None,
        )
    }

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
        assert!(
            files.is_empty(),
            "missing global file must not produce any entry, got {files:?}"
        );
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
        assert_eq!(
            files.len(),
            2,
            "only roots with a MONKEY.md present should produce an entry: {files:?}"
        );
        assert_eq!(files[0].scope, "project");
        assert_eq!(files[0].label, "project");
        assert_eq!(files[0].content, "Primary rules.");
        assert_eq!(files[1].label, "libs");
        assert_eq!(files[1].content, "Secondary A rules.");
    }

    /// Builds an `AppState` with `root` attached as the primary workspace
    /// root (and any `secondaries` attached after it), without going through
    /// `workspace::set_primary_workspace_root_impl` (private to that
    /// module) — `WorkspaceRoot`'s fields are `pub`, so constructing entries
    /// directly is simplest here.
    fn state_with_roots(root: &Path, secondaries: &[(&Path, &str)]) -> AppState {
        let state = AppState::default();
        {
            let mut roots = state.workspace_roots.lock().unwrap();
            roots.push(workspace::WorkspaceRoot {
                id: root.to_string_lossy().to_string(),
                path: root.to_path_buf(),
                label: "project".to_string(),
            });
            for (path, label) in secondaries {
                roots.push(workspace::WorkspaceRoot {
                    id: path.to_string_lossy().to_string(),
                    path: path.to_path_buf(),
                    label: label.to_string(),
                });
            }
        }
        state
    }

    #[test]
    fn write_global_scope_writes_the_global_path_and_creates_parent_dirs() {
        let dir = TempDir::new();
        // Nested, not-yet-existing app-data dir, like a fresh install.
        let global_path = dir.path.join("nested").join("MONKEY.md");
        let state = AppState::default();

        write_rules(&state, &global_path, "global", None, "Global rules.").unwrap();

        assert_eq!(
            std::fs::read_to_string(&global_path).unwrap(),
            "Global rules."
        );
    }

    #[test]
    fn write_project_scope_writes_the_primary_root_file() {
        let global_path = TempDir::new().path.join("MONKEY.md"); // unused by this branch
        let ws = TempDir::new();
        let state = state_with_roots(&ws.path, &[]);

        write_rules(
            &state,
            &global_path,
            "project",
            Some(RULE_FILE_NAME),
            "Primary project rules.",
        )
        .unwrap();

        let written = ws.path.canonicalize().unwrap().join(RULE_FILE_NAME);
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "Primary project rules."
        );
    }

    #[test]
    fn write_project_scope_writes_a_secondary_root_file_via_its_label() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let primary = TempDir::new();
        let secondary = TempDir::new();
        let state = state_with_roots(&primary.path, &[(&secondary.path, "libs")]);

        let root_path = format!("libs/{}", RULE_FILE_NAME);
        write_rules(
            &state,
            &global_path,
            "project",
            Some(&root_path),
            "Secondary rules.",
        )
        .unwrap();

        let written = secondary.path.canonicalize().unwrap().join(RULE_FILE_NAME);
        assert_eq!(
            std::fs::read_to_string(&written).unwrap(),
            "Secondary rules."
        );
    }

    #[test]
    fn write_project_scope_without_root_path_errors() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let ws = TempDir::new();
        let state = state_with_roots(&ws.path, &[]);

        let err = write_rules(&state, &global_path, "project", None, "content").unwrap_err();
        assert!(
            err.contains("root_path"),
            "expected a root_path error, got: {err}"
        );
    }

    #[test]
    fn write_project_scope_cannot_escape_the_workspace_root() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let ws = TempDir::new();
        let state = state_with_roots(&ws.path, &[]);

        let err = write_rules(
            &state,
            &global_path,
            "project",
            Some("../escape/MONKEY.md"),
            "evil",
        )
        .unwrap_err();
        assert!(
            err.contains("escapes"),
            "expected an escape error, got: {err}"
        );
    }

    #[test]
    fn write_project_scope_refuses_a_non_monkey_md_target() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let ws = TempDir::new();
        let victim = ws.path.join("package.json");
        std::fs::write(&victim, "{\"original\": true}").unwrap();
        let state = state_with_roots(&ws.path, &[]);

        let err =
            write_rules(&state, &global_path, "project", Some("package.json"), "{}").unwrap_err();
        assert!(
            err.contains("Refusing to write") && err.contains("MONKEY.md"),
            "expected a refusal mentioning MONKEY.md, got: {err}"
        );
        // The arbitrary file must be left untouched.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "{\"original\": true}"
        );
    }

    #[test]
    fn write_project_scope_refuses_a_non_monkey_md_target_via_secondary_root_label() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let primary = TempDir::new();
        let secondary = TempDir::new();
        let state = state_with_roots(&primary.path, &[(&secondary.path, "libs")]);

        let err = write_rules(
            &state,
            &global_path,
            "project",
            Some("libs/pre-commit"),
            "evil",
        )
        .unwrap_err();
        assert!(
            err.contains("Refusing to write"),
            "expected a refusal, got: {err}"
        );
    }

    #[test]
    fn global_scope_falls_back_to_agents_md_when_monkey_md_absent() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md"); // never written
        std::fs::write(dir.path.join("AGENTS.md"), "Global agents rules.").unwrap();

        let files = read_rules_impl(&global_path, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].scope, "global");
        assert_eq!(files[0].content, "Global agents rules.");
        assert_eq!(files[0].path, dir.path.join("AGENTS.md").to_string_lossy());
    }

    #[test]
    fn monkey_md_takes_precedence_over_agents_md_in_the_same_scope() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md");
        std::fs::write(&global_path, "Monkey rules.").unwrap();
        std::fs::write(dir.path.join("AGENTS.md"), "Agents rules.").unwrap();

        let files = read_rules_impl(&global_path, &[]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "Monkey rules.");
        assert_eq!(files[0].path, global_path.to_string_lossy());
    }

    #[test]
    fn project_root_falls_back_to_agents_md_when_monkey_md_absent() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md"); // absent — irrelevant here

        let root = TempDir::new();
        std::fs::write(root.path.join("AGENTS.md"), "Project agents rules.").unwrap();

        let roots = vec![(root.path.clone(), "project".to_string(), true)];
        let files = read_rules_impl(&global_path, &roots);

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].scope, "project");
        assert_eq!(files[0].content, "Project agents rules.");
        assert_eq!(files[0].path, root.path.join("AGENTS.md").to_string_lossy());
    }

    #[test]
    fn write_unknown_scope_errors() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let state = AppState::default();

        let err = write_rules(&state, &global_path, "bogus", None, "content").unwrap_err();
        assert!(
            err.contains("Unknown rules scope"),
            "expected an unknown-scope error, got: {err}"
        );
    }

    /// The gap K24 named: rules files saved last-write-wins. They now go through
    /// the same append-only log every persona and workflow does.
    #[test]
    fn a_rules_save_records_a_revision_and_dedupes_an_unchanged_one() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md");
        let revisions = dir.path.join("config-revisions");
        let state = AppState::default();

        let first = write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "Always write tests.",
            None,
        )
        .unwrap();

        let entity = rules_entity_id("global", &global_path);
        let history =
            crate::config_revisions::history(&revisions, RULES_REVISION_KIND, &entity, None)
                .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].revision_id, first);

        // An unchanged save is not a revision. An editor that saves on a
        // debounce must not fill the history with copies of the same text.
        let same = write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "Always write tests.",
            Some(first.clone()),
        )
        .unwrap();
        assert_eq!(same, first, "an identical save returns the existing head");
        assert_eq!(
            crate::config_revisions::history(&revisions, RULES_REVISION_KIND, &entity, None)
                .unwrap()
                .len(),
            1
        );

        let second = write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "Always write tests, and run them.",
            Some(first.clone()),
        )
        .unwrap();
        assert_ne!(second, first);
        assert_eq!(
            crate::config_revisions::history(&revisions, RULES_REVISION_KIND, &entity, None)
                .unwrap()
                .len(),
            2
        );
        // The snapshot is restorable: it is the content, not a diff.
        let restored =
            crate::config_revisions::get(&revisions, RULES_REVISION_KIND, &entity, &first).unwrap();
        assert_eq!(restored.content, "Always write tests.");
    }

    /// A stale base is refused, **and the file on disk is left untouched** —
    /// which is the half that makes the refusal worth anything. Rejecting after
    /// the write would report a conflict for an edit that had already landed.
    #[test]
    fn a_save_against_a_stale_base_is_refused_and_changes_nothing_on_disk() {
        let dir = TempDir::new();
        let global_path = dir.path.join("MONKEY.md");
        let revisions = dir.path.join("config-revisions");
        let state = AppState::default();

        let first = write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "one",
            None,
        )
        .unwrap();
        // Another window saves.
        write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "two",
            Some(first.clone()),
        )
        .unwrap();

        // This one still believes `first` is current.
        let error = write_rules_impl(
            &state,
            &revisions,
            &global_path,
            "global",
            None,
            "three",
            Some(first),
        )
        .unwrap_err();
        assert!(
            error.to_lowercase().contains("conflict:"),
            "the UI matches on this prefix to tell a concurrent edit from a disk \
             error, got {error:?}"
        );
        assert_eq!(
            std::fs::read_to_string(&global_path).unwrap(),
            "two",
            "a refused save must not have already overwritten the file"
        );
    }

    /// Two roots can both be labelled `src`. Keying the history on the label
    /// would merge their files into one log, where a restore would put one
    /// repo's instructions into the other.
    #[test]
    fn two_workspace_roots_keep_separate_histories() {
        let global_path = TempDir::new().path.join("MONKEY.md");
        let primary = TempDir::new();
        let secondary = TempDir::new();
        let revisions = TempDir::new();
        let state = state_with_roots(&primary.path, &[(&secondary.path, "libs")]);

        write_rules_impl(
            &state,
            &revisions.path,
            &global_path,
            "project",
            Some(RULE_FILE_NAME),
            "primary rules",
            None,
        )
        .unwrap();
        write_rules_impl(
            &state,
            &revisions.path,
            &global_path,
            "project",
            Some(&format!("libs/{RULE_FILE_NAME}")),
            "secondary rules",
            None,
        )
        .unwrap();

        let primary_entity = rules_entity_id(
            "project",
            &primary.path.canonicalize().unwrap().join(RULE_FILE_NAME),
        );
        let secondary_entity = rules_entity_id(
            "project",
            &secondary.path.canonicalize().unwrap().join(RULE_FILE_NAME),
        );
        assert_ne!(primary_entity, secondary_entity);

        for (entity, expected) in [
            (&primary_entity, "primary rules"),
            (&secondary_entity, "secondary rules"),
        ] {
            let history = crate::config_revisions::history(
                &revisions.path,
                RULES_REVISION_KIND,
                entity,
                None,
            )
            .unwrap();
            assert_eq!(history.len(), 1, "each root gets exactly its own log");
            let head =
                crate::config_revisions::head(&revisions.path, RULES_REVISION_KIND, entity, None)
                    .unwrap()
                    .unwrap();
            assert_eq!(head.content, expected);
        }
    }
}

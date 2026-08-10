//! Tauri boundary for migration-controlled profile storage and global search.

use crate::profile_store::{
    GlobalSearchHit, GlobalSearchRequest, ProfileMigrationResult, ProfileMigrationStatus,
    ProfileSaveResult,
};
use crate::AppState;

pub(crate) fn current_migration_status(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<ProfileMigrationStatus, String> {
    let source = crate::sessions::sessions_file_path(app)?;
    crate::run_commands::with_profile_ledger(app, state, |ledger| {
        crate::profile_store::migration_status(ledger, &source)
    })
}

pub(crate) fn migrate_current_profile(
    app: &tauri::AppHandle,
    state: &AppState,
) -> Result<ProfileMigrationResult, String> {
    let source = crate::sessions::sessions_file_path(app)?;
    let artifacts = crate::artifact_commands::store_for(app, state)?;
    crate::run_commands::with_profile_ledger(app, state, |ledger| {
        crate::profile_store::migrate_legacy_file(ledger, &artifacts, &source)
    })
}

pub(crate) fn sync_profile_payload(
    app: &tauri::AppHandle,
    state: &AppState,
    payload: &str,
) -> Result<ProfileSaveResult, String> {
    let artifacts = crate::artifact_commands::store_for(app, state)?;
    crate::run_commands::with_profile_ledger(app, state, |ledger| {
        crate::profile_store::save_payload(ledger, &artifacts, payload)
    })
}

/// Canonicalizes an optional profile-search workspace filter and proves that
/// it names one of the roots currently attached to this app instance.
///
/// `profile_search_documents` retains historical sessions, including work
/// from roots that are no longer attached. The search UI is allowed to query
/// that history globally, but an explicit workspace filter must never become
/// an oracle for an arbitrary path a renderer supplies. Requiring the exact
/// attached root also keeps the SQL equality filter aligned with the
/// canonical paths stored in the profile index.
fn scope_search_workspace(
    state: &AppState,
    request: &mut GlobalSearchRequest,
) -> Result<(), String> {
    let Some(workspace_path) = request.workspace_path.as_deref() else {
        return Ok(());
    };
    let (resolved, root) = crate::workspace::resolve_path_and_root(state, workspace_path)?;
    if resolved != root {
        return Err(
            "Global search workspace filter must be an exact attached workspace root".to_string(),
        );
    }
    request.workspace_path = Some(root.to_string_lossy().to_string());
    Ok(())
}

/// Returns the canonical workspace allowlist from native state. This is kept
/// separate from the renderer-supplied search filter: a caller may refine
/// within these roots, but it can never expand the authorization set.
fn attached_search_workspace_paths(state: &AppState) -> Result<Vec<String>, String> {
    let roots = state
        .workspace_roots
        .lock()
        .map_err(|_| "Workspace roots lock poisoned".to_string())?;
    let mut paths = roots
        .iter()
        .map(|root| {
            root.path
                .canonicalize()
                .map(|path| path.to_string_lossy().to_string())
                .map_err(|error| {
                    format!(
                        "Workspace root '{}' is no longer valid: {error}",
                        root.path.display(),
                    )
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[tauri::command]
pub fn profile_migration_status(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<ProfileMigrationStatus, String> {
    current_migration_status(&app, state.inner())
}

#[tauri::command]
pub fn profile_migrate(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<ProfileMigrationResult, String> {
    migrate_current_profile(&app, state.inner())
}

#[tauri::command]
pub fn profile_global_search(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    mut request: GlobalSearchRequest,
) -> Result<Vec<GlobalSearchHit>, String> {
    scope_search_workspace(state.inner(), &mut request)?;
    let allowed_workspace_paths = attached_search_workspace_paths(state.inner())?;
    let artifacts = crate::artifact_commands::store_for(&app, state.inner())?;
    crate::run_commands::with_profile_ledger(&app, state.inner(), |ledger| {
        crate::profile_store::global_search_with_artifacts_scoped(
            ledger,
            &artifacts,
            &request,
            &allowed_workspace_paths,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "little_monkey_profile_command_{label}_{}_{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn request(workspace_path: Option<String>) -> GlobalSearchRequest {
        GlobalSearchRequest {
            query: "needle".to_string(),
            workspace_path,
            ..GlobalSearchRequest::default()
        }
    }

    #[test]
    fn explicit_search_workspace_must_be_an_exact_attached_root() {
        let attached = temp_dir("attached");
        let nested = attached.join("nested");
        let outside = temp_dir("outside");
        fs::create_dir_all(&nested).unwrap();
        let canonical = attached.canonicalize().unwrap();

        let state = AppState::default();
        state
            .workspace_roots
            .lock()
            .unwrap()
            .push(crate::workspace::WorkspaceRoot {
                id: canonical.to_string_lossy().to_string(),
                path: canonical.clone(),
                label: "attached".to_string(),
            });
        assert_eq!(
            attached_search_workspace_paths(&state).unwrap(),
            vec![canonical.to_string_lossy().to_string()],
        );

        let mut exact = request(Some(attached.to_string_lossy().to_string()));
        scope_search_workspace(&state, &mut exact).unwrap();
        assert_eq!(
            exact.workspace_path.as_deref(),
            Some(canonical.to_string_lossy().as_ref()),
        );

        let mut descendant = request(Some(nested.to_string_lossy().to_string()));
        assert!(scope_search_workspace(&state, &mut descendant).is_err());

        let mut detached = request(Some(outside.to_string_lossy().to_string()));
        assert!(scope_search_workspace(&state, &mut detached).is_err());

        let _ = fs::remove_dir_all(attached);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn unfiltered_search_remains_available_for_profile_history() {
        let state = AppState::default();
        let mut request = request(None);
        scope_search_workspace(&state, &mut request).unwrap();
        assert!(request.workspace_path.is_none());
        assert!(attached_search_workspace_paths(&state).unwrap().is_empty());
    }
}

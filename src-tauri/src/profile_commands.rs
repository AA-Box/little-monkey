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
    request: GlobalSearchRequest,
) -> Result<Vec<GlobalSearchHit>, String> {
    let artifacts = crate::artifact_commands::store_for(&app, state.inner())?;
    crate::run_commands::with_profile_ledger(&app, state.inner(), |ledger| {
        crate::profile_store::global_search_with_artifacts(ledger, &artifacts, &request)
    })
}

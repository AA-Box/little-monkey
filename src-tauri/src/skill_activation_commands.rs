use std::path::Path;
use std::sync::Arc;

use crate::skill_activation::{
    SkillActivationEntry, SkillActivationError, SkillActivationPolicy, SkillActivationStore,
};

use tauri::Emitter;

pub const SKILL_ACTIVATION_CHANGED_EVENT: &str = "skill-activation://changed";

pub struct SkillActivationCommandState {
    pub store: Arc<SkillActivationStore>,
}

async fn run_activation_blocking<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, SkillActivationError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|error| format!("Skill activation worker failed: {error}"))?
        .map_err(|error| error.to_string())
}

impl SkillActivationCommandState {
    pub fn production(profile_data_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            store: Arc::new(
                SkillActivationStore::new(profile_data_dir).map_err(|error| error.to_string())?,
            ),
        })
    }
}

#[tauri::command]
pub async fn skill_activation_list(
    state: tauri::State<'_, SkillActivationCommandState>,
) -> Result<Vec<SkillActivationEntry>, String> {
    let store = state.store.clone();
    run_activation_blocking(move || store.list()).await
}

#[tauri::command]
pub async fn skill_activation_get(
    state: tauri::State<'_, SkillActivationCommandState>,
    key: String,
) -> Result<Option<SkillActivationEntry>, String> {
    let store = state.store.clone();
    run_activation_blocking(move || store.get(&key)).await
}

#[tauri::command]
pub async fn skill_activation_set(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, SkillActivationCommandState>,
    key: String,
    policy: SkillActivationPolicy,
    pinned: bool,
) -> Result<SkillActivationEntry, String> {
    if window.label() != "main" {
        return Err(
            "Skill activation settings are only available from the main window".to_string(),
        );
    }
    let store = state.store.clone();
    let entry = run_activation_blocking(move || store.set(&key, policy, pinned)).await?;
    // The file is the authority; this event only tells already-open renderers
    // to refresh their cache. CLI mutations converge at the next turn/focus.
    let _ = app.emit(SKILL_ACTIVATION_CHANGED_EVENT, window.label());
    Ok(entry)
}

#[tauri::command]
pub async fn skill_activation_migrate(
    state: tauri::State<'_, SkillActivationCommandState>,
    entries: Vec<SkillActivationEntry>,
) -> Result<Vec<SkillActivationEntry>, String> {
    let store = state.store.clone();
    run_activation_blocking(move || store.migrate_once(entries)).await
}

use std::path::Path;
use std::sync::Arc;

use crate::native_skill_commands::run_blocking;
use crate::skill_activation::{SkillActivationEntry, SkillActivationPolicy, SkillActivationStore};

pub struct SkillActivationCommandState {
    pub store: Arc<SkillActivationStore>,
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
    run_blocking(move || store.list()).await
}

#[tauri::command]
pub async fn skill_activation_get(
    state: tauri::State<'_, SkillActivationCommandState>,
    key: String,
) -> Result<Option<SkillActivationEntry>, String> {
    let store = state.store.clone();
    run_blocking(move || store.get(&key)).await
}

#[tauri::command]
pub async fn skill_activation_set(
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
    run_blocking(move || store.set(&key, policy, pinned)).await
}

#[tauri::command]
pub async fn skill_activation_migrate(
    state: tauri::State<'_, SkillActivationCommandState>,
    entries: Vec<SkillActivationEntry>,
) -> Result<Vec<SkillActivationEntry>, String> {
    let store = state.store.clone();
    run_blocking(move || store.migrate_once(entries)).await
}

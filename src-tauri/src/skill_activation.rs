//! Profile-owned activation preferences for installed skills.
//!
//! This store is deliberately Tauri-free so the desktop bridge and the CLI
//! share the same file format and mutation rules.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SKILL_ACTIVATION_SCHEMA_VERSION: u32 = 1;
const STATE_FILE: &str = "skill-activation-v1.json";
const MAX_ENTRIES: usize = 4096;
const MAX_KEY_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SkillActivationPolicy {
    Automatic,
    #[default]
    Ask,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillActivationPreference {
    pub policy: SkillActivationPolicy,
    pub pinned: bool,
    pub updated_at_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillActivationEntry {
    pub key: String,
    #[serde(flatten)]
    pub preference: SkillActivationPreference,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SkillActivationState {
    schema_version: u32,
    #[serde(default)]
    migration_completed: bool,
    #[serde(default)]
    entries: BTreeMap<String, SkillActivationPreference>,
}

#[derive(Debug)]
pub enum SkillActivationError {
    Io(String),
    Json(String),
    InvalidKey(String),
    TooManyEntries,
}

impl std::fmt::Display for SkillActivationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Json(error) => write!(formatter, "{error}"),
            Self::InvalidKey(error) => write!(formatter, "{error}"),
            Self::TooManyEntries => write!(formatter, "too many skill activation preferences"),
        }
    }
}

impl std::error::Error for SkillActivationError {}

#[derive(Debug)]
pub struct SkillActivationStore {
    path: PathBuf,
    mutation: Mutex<()>,
}

impl SkillActivationStore {
    pub fn new(profile_data_dir: impl AsRef<Path>) -> Result<Self, SkillActivationError> {
        let root = profile_data_dir.as_ref();
        fs::create_dir_all(root).map_err(|error| SkillActivationError::Io(error.to_string()))?;
        Ok(Self {
            path: root.join(STATE_FILE),
            mutation: Mutex::new(()),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, ()>, SkillActivationError> {
        self.mutation
            .lock()
            .map_err(|_| SkillActivationError::Io("skill activation lock poisoned".to_string()))
    }

    fn load(&self) -> Result<SkillActivationState, SkillActivationError> {
        if !self.path.exists() {
            return Ok(SkillActivationState {
                schema_version: SKILL_ACTIVATION_SCHEMA_VERSION,
                ..SkillActivationState::default()
            });
        }
        let bytes =
            fs::read(&self.path).map_err(|error| SkillActivationError::Io(error.to_string()))?;
        let state: SkillActivationState = serde_json::from_slice(&bytes)
            .map_err(|error| SkillActivationError::Json(error.to_string()))?;
        if state.schema_version != SKILL_ACTIVATION_SCHEMA_VERSION {
            return Err(SkillActivationError::Json(format!(
                "unsupported skill activation schema {}",
                state.schema_version
            )));
        }
        Ok(state)
    }

    fn save(&self, state: &SkillActivationState) -> Result<(), SkillActivationError> {
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|error| SkillActivationError::Json(error.to_string()))?;
        let temporary = self
            .path
            .with_file_name(format!("{STATE_FILE}.tmp-{}", Uuid::new_v4().simple()));
        let result = (|| {
            let mut file = fs::File::create(&temporary)
                .map_err(|error| SkillActivationError::Io(error.to_string()))?;
            file.write_all(&bytes)
                .map_err(|error| SkillActivationError::Io(error.to_string()))?;
            file.sync_all()
                .map_err(|error| SkillActivationError::Io(error.to_string()))?;
            fs::rename(&temporary, &self.path)
                .map_err(|error| SkillActivationError::Io(error.to_string()))
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn validate_key(key: &str) -> Result<(), SkillActivationError> {
        if key.trim().is_empty() || key.len() > MAX_KEY_BYTES || key.contains('\n') {
            return Err(SkillActivationError::InvalidKey(
                "skill activation key is empty or invalid".to_string(),
            ));
        }
        Ok(())
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn list(&self) -> Result<Vec<SkillActivationEntry>, SkillActivationError> {
        let _guard = self.lock()?;
        Ok(self
            .load()?
            .entries
            .into_iter()
            .map(|(key, preference)| SkillActivationEntry { key, preference })
            .collect())
    }

    pub fn get(&self, key: &str) -> Result<Option<SkillActivationEntry>, SkillActivationError> {
        Self::validate_key(key)?;
        let _guard = self.lock()?;
        Ok(self
            .load()?
            .entries
            .get(key)
            .cloned()
            .map(|preference| SkillActivationEntry {
                key: key.to_string(),
                preference,
            }))
    }

    pub fn set(
        &self,
        key: &str,
        policy: SkillActivationPolicy,
        pinned: bool,
    ) -> Result<SkillActivationEntry, SkillActivationError> {
        Self::validate_key(key)?;
        let _guard = self.lock()?;
        let mut state = self.load()?;
        if !state.entries.contains_key(key) && state.entries.len() >= MAX_ENTRIES {
            return Err(SkillActivationError::TooManyEntries);
        }
        let preference = SkillActivationPreference {
            policy,
            pinned,
            updated_at_unix_ms: Self::now(),
        };
        state.entries.insert(key.to_string(), preference.clone());
        state.schema_version = SKILL_ACTIVATION_SCHEMA_VERSION;
        self.save(&state)?;
        Ok(SkillActivationEntry {
            key: key.to_string(),
            preference,
        })
    }

    /// Imports the old frontend snapshot once. Existing backend entries win,
    /// so a retry can never overwrite a newer desktop/CLI decision.
    pub fn migrate_once(
        &self,
        entries: Vec<SkillActivationEntry>,
    ) -> Result<Vec<SkillActivationEntry>, SkillActivationError> {
        let _guard = self.lock()?;
        let mut state = self.load()?;
        if !state.migration_completed {
            for entry in entries.into_iter().take(MAX_ENTRIES) {
                Self::validate_key(&entry.key)?;
                if state.entries.len() >= MAX_ENTRIES && !state.entries.contains_key(&entry.key) {
                    break;
                }
                state.entries.entry(entry.key).or_insert(entry.preference);
            }
            state.migration_completed = true;
            state.schema_version = SKILL_ACTIVATION_SCHEMA_VERSION;
            self.save(&state)?;
        }
        Ok(state
            .entries
            .into_iter()
            .map(|(key, preference)| SkillActivationEntry { key, preference })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "little-monkey-skill-activation-{name}-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&directory).expect("test directory");
        directory
    }

    #[test]
    fn missing_preferences_fail_closed_to_ask_at_the_boundary() {
        let directory = test_directory("missing");
        let store = SkillActivationStore::new(&directory).expect("store");
        assert!(store.get("native:global:test").expect("get").is_none());
        let entry = store
            .set("native:global:test", SkillActivationPolicy::Automatic, true)
            .expect("set");
        assert_eq!(entry.preference.policy, SkillActivationPolicy::Automatic);
        assert!(entry.preference.pinned);
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn migration_is_one_time_and_backend_entries_win() {
        let directory = test_directory("migration");
        let store = SkillActivationStore::new(&directory).expect("store");
        store
            .set("local:existing", SkillActivationPolicy::Manual, false)
            .expect("set");
        let migrated = store
            .migrate_once(vec![
                SkillActivationEntry {
                    key: "local:existing".to_string(),
                    preference: SkillActivationPreference {
                        policy: SkillActivationPolicy::Automatic,
                        pinned: true,
                        updated_at_unix_ms: 1,
                    },
                },
                SkillActivationEntry {
                    key: "local:new".to_string(),
                    preference: SkillActivationPreference {
                        policy: SkillActivationPolicy::Manual,
                        pinned: false,
                        updated_at_unix_ms: 1,
                    },
                },
            ])
            .expect("migration");
        assert_eq!(migrated.len(), 2);
        assert_eq!(
            store
                .get("local:existing")
                .expect("get")
                .unwrap()
                .preference
                .policy,
            SkillActivationPolicy::Manual
        );
        store
            .migrate_once(vec![SkillActivationEntry {
                key: "local:new".to_string(),
                preference: SkillActivationPreference {
                    policy: SkillActivationPolicy::Automatic,
                    pinned: true,
                    updated_at_unix_ms: 2,
                },
            }])
            .expect("second migration");
        assert_eq!(
            store
                .get("local:new")
                .expect("get")
                .unwrap()
                .preference
                .policy,
            SkillActivationPolicy::Manual
        );
        fs::remove_dir_all(directory).expect("cleanup");
    }
}

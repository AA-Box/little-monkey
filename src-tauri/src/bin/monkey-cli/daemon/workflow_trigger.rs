//! File-backed M4 -> M6 persistent-trigger adapter.
//!
//! M4 publishes immutable, digest-bound workflow trigger batches under the
//! daemon directory. The resident daemon validates the whole directory before
//! atomically replacing each workflow's owned trigger rows. It never accepts
//! ad-hoc SQLite rows from another subsystem.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use little_monkey_lib::m4_runtime::workflow_trigger_registration_id;
use little_monkey_lib::m4_services::M4_TRIGGER_ADAPTER_CONTRACT_VERSION;
use little_monkey_lib::workflow_core::WorkflowTrigger;
use serde::{Deserialize, Serialize};

use super::ledger::{SharedLedger, StoredTrigger, TriggerReplacement};
use super::trigger::{
    next_cron_ms, sha256_hex, TriggerConfig, TriggerTarget, WorkflowTriggerBinding,
};

const BATCH_DIRECTORY: &str = "workflow-triggers-v1";
const MAX_BATCH_FILES: usize = 1_024;
const MAX_BATCH_BYTES: u64 = 1024 * 1024;
const MAX_TRIGGERS_PER_WORKFLOW: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct WorkflowTriggerBatchFile {
    contract_version: u32,
    workflow_id: String,
    workflow_version: u32,
    definition_sha256: String,
    triggers: Vec<WorkflowTrigger>,
    updated_unix_ms: u64,
    enabled: bool,
}

#[derive(Default)]
pub struct WorkflowBatchSynchronizer {
    applied_signature: Option<String>,
    rejected_signature: Option<String>,
    rejected_retry_at_ms: u64,
}

impl WorkflowBatchSynchronizer {
    pub fn sync_if_changed(
        &mut self,
        daemon_root: &Path,
        shared: &mut SharedLedger,
        now_ms: u64,
    ) -> Result<usize, String> {
        let directory = daemon_root.join(BATCH_DIRECTORY);
        let (signature, paths) = batch_directory_signature(&directory)?;
        if self.applied_signature.as_deref() == Some(signature.as_str()) {
            return Ok(0);
        }
        if self.rejected_signature.as_deref() == Some(signature.as_str())
            && now_ms < self.rejected_retry_at_ms
        {
            return Ok(0);
        }
        match sync_paths(shared, &paths, now_ms) {
            Ok(changed) => {
                self.applied_signature = Some(signature);
                self.rejected_signature = None;
                self.rejected_retry_at_ms = 0;
                Ok(changed)
            }
            Err(error) => {
                self.rejected_signature = Some(signature);
                self.rejected_retry_at_ms = now_ms.saturating_add(5_000);
                Err(error)
            }
        }
    }
}

fn batch_directory_signature(directory: &Path) -> Result<(String, Vec<PathBuf>), String> {
    if !directory.exists() {
        return Ok((sha256_hex(b"missing"), Vec::new()));
    }
    let directory_metadata = fs::symlink_metadata(directory).map_err(|error| {
        format!(
            "Cannot inspect workflow trigger directory '{}': {error}",
            directory.display()
        )
    })?;
    if directory_metadata.file_type().is_symlink() || !directory_metadata.is_dir() {
        return Err(format!(
            "Workflow trigger directory '{}' must be a real directory",
            directory.display()
        ));
    }
    let mut records = Vec::new();
    let mut paths = Vec::new();
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Cannot read '{}': {error}", directory.display()))?
    {
        let entry = entry.map_err(|error| format!("Cannot read trigger batch entry: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        if paths.len() == MAX_BATCH_FILES {
            return Err(format!(
                "Workflow trigger directory exceeds {MAX_BATCH_FILES} batch files"
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("Cannot inspect '{}': {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "Workflow trigger batch '{}' must be a regular file",
                path.display()
            ));
        }
        if metadata.len() > MAX_BATCH_BYTES {
            return Err(format!(
                "Workflow trigger batch '{}' exceeds {MAX_BATCH_BYTES} bytes",
                path.display()
            ));
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "Workflow trigger batch filename must be UTF-8".to_string())?;
        validate_batch_filename(name)?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        records.push(format!("{name}:{}:{modified}", metadata.len()));
        paths.push(path);
    }
    paths.sort();
    records.sort();
    Ok((sha256_hex(records.join("\n").as_bytes()), paths))
}

fn validate_batch_filename(name: &str) -> Result<(), String> {
    let Some(stem) = name.strip_suffix(".json") else {
        return Err("Workflow trigger batch must use a .json extension".to_string());
    };
    validate_digest(stem, "workflow trigger batch filename")
}

fn sync_paths(shared: &mut SharedLedger, paths: &[PathBuf], now_ms: u64) -> Result<usize, String> {
    let mut batches = Vec::with_capacity(paths.len());
    let mut workflow_ids = BTreeSet::new();
    for path in paths {
        let bytes = fs::read(path)
            .map_err(|error| format!("Cannot read trigger batch '{}': {error}", path.display()))?;
        if bytes.len() as u64 > MAX_BATCH_BYTES {
            return Err(format!(
                "Workflow trigger batch '{}' exceeds {MAX_BATCH_BYTES} bytes",
                path.display()
            ));
        }
        let batch: WorkflowTriggerBatchFile = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid trigger batch '{}': {error}", path.display()))?;
        validate_batch(&batch, path)?;
        if !workflow_ids.insert(batch.workflow_id.clone()) {
            return Err(format!(
                "Duplicate trigger batch for workflow '{}'",
                batch.workflow_id
            ));
        }
        batches.push(batch);
    }

    let existing = managed_triggers(shared.list_triggers()?)?;
    let mut plans = Vec::with_capacity(batches.len());
    for batch in &batches {
        plans.push(build_plan(batch, existing.get(&batch.workflow_id), now_ms)?);
    }
    // Only after every file and replacement has validated do we mutate rows.
    let mut changed = 0usize;
    for plan in plans {
        shared.replace_trigger_batch(&plan.previous_ids, &plan.replacements, now_ms)?;
        changed = changed.saturating_add(plan.replacements.len());
    }
    // Atomic M4 unregister removes the batch file. Disable only rows explicitly
    // marked as M4-batch-managed; manual workflow targets remain untouched.
    for (workflow_id, triggers) in &existing {
        if workflow_ids.contains(workflow_id) {
            continue;
        }
        let previous_ids = triggers
            .iter()
            .map(|(stored, _)| stored.trigger_id.clone())
            .collect::<Vec<_>>();
        if !previous_ids.is_empty() {
            shared.replace_trigger_batch(&previous_ids, &[], now_ms)?;
        }
    }
    Ok(changed)
}

fn validate_batch(batch: &WorkflowTriggerBatchFile, path: &Path) -> Result<(), String> {
    if batch.contract_version != M4_TRIGGER_ADAPTER_CONTRACT_VERSION {
        return Err(format!(
            "Unsupported M4 trigger adapter contract {}",
            batch.contract_version
        ));
    }
    validate_identifier(&batch.workflow_id, "workflow id")?;
    if batch.workflow_version == 0 {
        return Err("Workflow trigger batch version must be positive".to_string());
    }
    validate_digest(&batch.definition_sha256, "workflow definition digest")?;
    if batch.updated_unix_ms == 0 {
        return Err("Workflow trigger batch update timestamp must be positive".to_string());
    }
    if batch.triggers.len() > MAX_TRIGGERS_PER_WORKFLOW {
        return Err(format!(
            "Workflow trigger batch exceeds {MAX_TRIGGERS_PER_WORKFLOW} triggers"
        ));
    }
    let expected_name = format!("{}.json", sha256_hex(batch.workflow_id.as_bytes()));
    if path.file_name().and_then(|value| value.to_str()) != Some(expected_name.as_str()) {
        return Err(format!(
            "Workflow trigger batch filename does not match workflow '{}'",
            batch.workflow_id
        ));
    }
    Ok(())
}

type ManagedTriggerMap = BTreeMap<String, Vec<(StoredTrigger, TriggerConfig)>>;

fn managed_triggers(stored: Vec<StoredTrigger>) -> Result<ManagedTriggerMap, String> {
    let mut managed = ManagedTriggerMap::new();
    for row in stored {
        let config: TriggerConfig = serde_json::from_slice(&row.config_json)
            .map_err(|error| format!("Invalid trigger '{}': {error}", row.trigger_id))?;
        let Some((workflow_id, _, binding)) = config.workflow_target() else {
            continue;
        };
        if binding.managed_by_batch {
            managed
                .entry(workflow_id.to_string())
                .or_default()
                .push((row, config));
        }
    }
    Ok(managed)
}

struct ReplacementPlan {
    previous_ids: Vec<String>,
    replacements: Vec<TriggerReplacement>,
}

fn build_plan(
    batch: &WorkflowTriggerBatchFile,
    existing: Option<&Vec<(StoredTrigger, TriggerConfig)>>,
    now_ms: u64,
) -> Result<ReplacementPlan, String> {
    let existing = existing.map(Vec::as_slice).unwrap_or_default();
    let highest_version = existing
        .iter()
        .filter_map(|(_, config)| config.workflow_binding())
        .map(|binding| binding.workflow_version)
        .max()
        .unwrap_or(0);
    if batch.workflow_version < highest_version {
        return Err(format!(
            "Refusing workflow trigger rollback for '{}': {} < {}",
            batch.workflow_id, batch.workflow_version, highest_version
        ));
    }
    if batch.workflow_version == highest_version && highest_version != 0 {
        for (_, config) in existing {
            let Some((_, digest, binding)) = config.workflow_target() else {
                continue;
            };
            if binding.workflow_version == highest_version && digest != batch.definition_sha256 {
                return Err(format!(
                    "Refusing digest replacement at workflow version {} for '{}'",
                    batch.workflow_version, batch.workflow_id
                ));
            }
        }
    }

    let previous_ids = existing
        .iter()
        .map(|(stored, _)| stored.trigger_id.clone())
        .collect::<Vec<_>>();
    if !batch.enabled {
        return Ok(ReplacementPlan {
            previous_ids,
            replacements: Vec::new(),
        });
    }

    let mut encoded = batch
        .triggers
        .iter()
        .map(|trigger| {
            serde_json::to_vec(trigger)
                .map(|bytes| (bytes, trigger.clone()))
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    encoded.sort_by(|left, right| left.0.cmp(&right.0));
    for pair in encoded.windows(2) {
        if pair[0].0 == pair[1].0 {
            return Err(format!(
                "Workflow '{}' declares a duplicate persistent trigger",
                batch.workflow_id
            ));
        }
    }

    let existing_by_id = existing
        .iter()
        .map(|(stored, config)| (stored.trigger_id.as_str(), (stored, config)))
        .collect::<BTreeMap<_, _>>();
    let mut replacements = Vec::with_capacity(encoded.len());
    for (_canonical, declared_trigger) in encoded {
        let trigger_id = workflow_trigger_registration_id(
            &batch.workflow_id,
            batch.workflow_version,
            &declared_trigger,
        )?;
        let (mut config, mut next_fire_at_ms) = compile_trigger(batch, declared_trigger, now_ms)?;
        if let Some((stored, current)) = existing_by_id.get(trigger_id.as_str()) {
            preserve_runtime_state(&mut config, current);
            if same_trigger_definition(&config, current) {
                next_fire_at_ms = stored.next_fire_at_ms;
            }
        }
        config.validate()?;
        replacements.push(TriggerReplacement {
            trigger_id,
            kind: config.kind_token().to_string(),
            config_json: serde_json::to_vec(&config).map_err(|error| error.to_string())?,
            next_fire_at_ms,
        });
    }
    Ok(ReplacementPlan {
        previous_ids,
        replacements,
    })
}

fn compile_trigger(
    batch: &WorkflowTriggerBatchFile,
    trigger: WorkflowTrigger,
    now_ms: u64,
) -> Result<(TriggerConfig, Option<u64>), String> {
    let target = TriggerTarget::Workflow {
        workflow_id: batch.workflow_id.clone(),
        definition_sha256: batch.definition_sha256.clone(),
    };
    let binding = WorkflowTriggerBinding {
        workflow_version: batch.workflow_version,
        managed_by_batch: true,
        trigger: trigger.clone(),
    };
    match trigger {
        WorkflowTrigger::PersistentCron { expression } => {
            let next = next_cron_ms(&expression, now_ms)?;
            Ok((
                TriggerConfig::Cron {
                    target,
                    workflow: Some(binding),
                    schedule: expression,
                },
                Some(next),
            ))
        }
        WorkflowTrigger::Filesystem {
            canonical_root,
            pattern,
        } => Ok((
            TriggerConfig::Filesystem {
                target,
                workflow: Some(binding),
                path: canonical_root,
                recursive: true,
                pattern: Some(pattern),
                last_fingerprint: None,
            },
            None,
        )),
        WorkflowTrigger::SignedWebhook {
            secret_reference,
            replay_window_ms,
            ..
        } => Ok((
            TriggerConfig::SignedWebhook {
                target,
                workflow: Some(binding),
                secret_reference: Some(secret_reference),
                max_skew_ms: replay_window_ms,
            },
            None,
        )),
        WorkflowTrigger::Manual | WorkflowTrigger::InAppCron { .. } => Err(format!(
            "Workflow '{}' batch contains a non-persistent trigger",
            batch.workflow_id
        )),
        WorkflowTrigger::EventIngestion { .. } => Err(format!(
            "Workflow '{}' event-ingestion trigger has no configured M6 event source",
            batch.workflow_id
        )),
    }
}

fn preserve_runtime_state(desired: &mut TriggerConfig, current: &TriggerConfig) {
    if let (
        TriggerConfig::Filesystem {
            last_fingerprint: desired_fingerprint,
            ..
        },
        TriggerConfig::Filesystem {
            last_fingerprint: current_fingerprint,
            ..
        },
    ) = (desired, current)
    {
        *desired_fingerprint = current_fingerprint.clone();
    }
}

fn same_trigger_definition(left: &TriggerConfig, right: &TriggerConfig) -> bool {
    let mut left = left.clone();
    let mut right = right.clone();
    if let TriggerConfig::Filesystem {
        last_fingerprint, ..
    } = &mut left
    {
        *last_fingerprint = None;
    }
    if let TriggerConfig::Filesystem {
        last_fingerprint, ..
    } = &mut right
    {
        *last_fingerprint = None;
    }
    left == right
}

fn validate_identifier(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err(format!("{label} must be a bounded ASCII identifier"))
    } else {
        Ok(())
    }
}

fn validate_digest(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err(format!("{label} must be a 64-character SHA-256 digest"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::m4_runtime::FilesystemWorkflowTriggerRegistrar;
    use little_monkey_lib::m4_services::{
        PersistentWorkflowTriggerRegistrar, WorkflowTriggerBatch,
    };

    fn batch(root: &Path, version: u32, digest: &str, triggers: Vec<WorkflowTrigger>) {
        fs::create_dir_all(root.join(BATCH_DIRECTORY)).unwrap();
        let workflow_id = "workflow.one";
        let path = root
            .join(BATCH_DIRECTORY)
            .join(format!("{}.json", sha256_hex(workflow_id.as_bytes())));
        let bytes = serde_json::to_vec(&WorkflowTriggerBatchFile {
            contract_version: M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
            workflow_id: workflow_id.into(),
            workflow_version: version,
            definition_sha256: digest.into(),
            triggers,
            updated_unix_ms: 1,
            enabled: true,
        })
        .unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!("lm-m4-trigger-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn registration_id_matches_contract() {
        let trigger = WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".into(),
        };
        let canonical = serde_json::to_vec(&trigger).unwrap();
        let id = workflow_trigger_registration_id("workflow.one", 7, &trigger).unwrap();
        assert_eq!(
            id,
            format!(
                "m4w-{}-v7-{}",
                &sha256_hex(b"workflow.one")[..16],
                &sha256_hex(&canonical)[..16]
            )
        );
    }

    #[test]
    fn consumes_the_production_m4_registrar_envelope_without_schema_translation() {
        let app_data = root();
        let registrar = FilesystemWorkflowTriggerRegistrar::new(&app_data).unwrap();
        let trigger = WorkflowTrigger::PersistentCron {
            expression: "*/10 * * * *".into(),
        };
        let expected_ids = registrar
            .replace_batch(&WorkflowTriggerBatch {
                contract_version: M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
                workflow_id: "workflow.production".into(),
                workflow_version: 4,
                definition_sha256: "d".repeat(64),
                triggers: vec![trigger],
            })
            .unwrap();
        let mut shared = SharedLedger::open(&app_data.join("profile.sqlite3")).unwrap();
        let mut sync = WorkflowBatchSynchronizer::default();
        assert_eq!(
            sync.sync_if_changed(&app_data.join("daemon"), &mut shared, 10_000)
                .unwrap(),
            1
        );
        let rows = shared.list_triggers().unwrap();
        assert_eq!(rows.iter().filter(|row| row.enabled).count(), 1);
        assert_eq!(rows[0].trigger_id, expected_ids[0]);
        let _ = fs::remove_dir_all(app_data);
    }

    #[test]
    fn batch_replacement_is_atomic_and_removal_disables_owned_rows() {
        let root = root();
        let db = root.join("ledger.sqlite3");
        let mut shared = SharedLedger::open(&db).unwrap();
        batch(
            &root,
            1,
            &"a".repeat(64),
            vec![WorkflowTrigger::PersistentCron {
                expression: "*/5 * * * *".into(),
            }],
        );
        let mut sync = WorkflowBatchSynchronizer::default();
        assert_eq!(sync.sync_if_changed(&root, &mut shared, 10_000).unwrap(), 1);
        let rows = shared.list_triggers().unwrap();
        assert_eq!(rows.iter().filter(|row| row.enabled).count(), 1);

        fs::remove_dir_all(root.join(BATCH_DIRECTORY)).unwrap();
        sync.sync_if_changed(&root, &mut shared, 10_001).unwrap();
        assert!(shared
            .list_triggers()
            .unwrap()
            .iter()
            .all(|row| !row.enabled));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_version_and_digest_rollback_without_changing_active_batch() {
        let root = root();
        let db = root.join("ledger.sqlite3");
        let mut shared = SharedLedger::open(&db).unwrap();
        let cron = vec![WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".into(),
        }];
        batch(&root, 2, &"b".repeat(64), cron.clone());
        let mut first = WorkflowBatchSynchronizer::default();
        first.sync_if_changed(&root, &mut shared, 10_000).unwrap();

        batch(&root, 1, &"a".repeat(64), cron.clone());
        let mut rollback = WorkflowBatchSynchronizer::default();
        assert!(rollback
            .sync_if_changed(&root, &mut shared, 10_001)
            .unwrap_err()
            .contains("rollback"));
        assert_eq!(
            shared
                .list_triggers()
                .unwrap()
                .iter()
                .filter(|r| r.enabled)
                .count(),
            1
        );

        batch(&root, 2, &"c".repeat(64), cron);
        let mut replacement = WorkflowBatchSynchronizer::default();
        assert!(replacement
            .sync_if_changed(&root, &mut shared, 10_002)
            .unwrap_err()
            .contains("digest replacement"));
        assert_eq!(
            shared
                .list_triggers()
                .unwrap()
                .iter()
                .filter(|r| r.enabled)
                .count(),
            1
        );
        let _ = fs::remove_dir_all(root);
    }
}

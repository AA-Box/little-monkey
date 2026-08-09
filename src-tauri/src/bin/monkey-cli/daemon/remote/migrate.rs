//! Building a frozen process image on one node and landing it on another
//! (roadmap K18).
//!
//! The types and the admission decision live in `little_monkey_lib::migration`,
//! because both nodes must agree on them and neither owns the other. What is
//! here is the two things that only ever happen on one side: reading an image
//! off the origin's disk, and writing one onto the target's.
//!
//! # Landing is aimed at the *desktop's* K13 re-entry, not at the daemon
//!
//! `frozenTurn.ts` already knows how to re-enter a frozen turn: it finds the
//! checkpoint whose `resume.process_id` matches a suspended `chat_turn` row,
//! asks `checkpoint_restorability`, and continues the conversation. That is the
//! thing K18's acceptance means by "resumes there". So landing writes into
//! exactly the three places that path reads — the checkpoints directory, the
//! sessions file, and `agent_processes` — and adds no second resume path. The
//! move is complete when the target's own desktop can press Resume.

use std::path::{Path, PathBuf};

use little_monkey_lib::checkpoints::CheckpointManifest;
use little_monkey_lib::migration::{
    collect_tree, MigrationHeader, MigrationImage, MigrationPayload, MIGRATION_PROTOCOL_VERSION,
};
use little_monkey_lib::process_table::{AdmitProcess, ProcessKind, ProcessState};
use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::RunSpec;

use crate::daemon::store::DaemonPaths;

const MANIFEST_FILE: &str = "manifest.json";
const SESSIONS_FILE: &str = "chat_sessions.json";

/// Where a landed image ended up, for the receipt the origin gets back.
pub struct LandedMigration {
    pub process_id: String,
    pub workspace_root: PathBuf,
}

pub fn checkpoints_dir(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("checkpoints")
}

/// Reads a frozen checkpoint and its workspace into a portable image.
///
/// The workspace comes from the freeze image's own `resume.workspace` rather
/// than from anything ambient: that path is what K13 recorded as the namespace
/// the process was working in, and capturing a different one would ship a
/// workspace the conversation never saw.
pub fn build_image(
    app_data_dir: &Path,
    origin_node_id: &str,
    checkpoint_id: &str,
    spec: &RunSpec,
    origin_last_sequence: u64,
    origin_last_event_hash: &str,
    required_residency: Option<String>,
) -> Result<MigrationImage, String> {
    little_monkey_lib::checkpoints::validate_checkpoint_id(checkpoint_id)?;
    let checkpoint_dir = checkpoints_dir(app_data_dir).join(checkpoint_id);
    let manifest = read_manifest(&checkpoint_dir)?;
    let resume = manifest
        .resume
        .as_ref()
        .ok_or_else(|| {
            format!("Checkpoint '{checkpoint_id}' is a turn snapshot, not a frozen process")
        })?
        .clone();

    let workspace_files = match resume.workspace.as_deref() {
        Some(root) => collect_tree(Path::new(root))?,
        // A model-only turn with no filesystem. The image is still a valid
        // process image; there is simply nothing to carry, and inventing an
        // empty workspace on the target would be inventing a namespace the
        // origin never had.
        None => Vec::new(),
    };
    let checkpoint_files = collect_tree(&checkpoint_dir)?;
    let session = read_session(app_data_dir, &manifest.session_id)?;

    let payload = MigrationPayload {
        checkpoint_files,
        workspace_files,
        session,
    };
    payload.validate()?;
    let header = MigrationHeader {
        protocol_version: MIGRATION_PROTOCOL_VERSION,
        origin_node_id: origin_node_id.to_string(),
        run_id: spec.run_id.clone(),
        process_id: resume.process_id.clone(),
        checkpoint_id: checkpoint_id.to_string(),
        manifest,
        required_residency,
        payload_bytes: payload.decoded_bytes(),
        payload_sha256: payload.digest()?,
    };
    let image = MigrationImage {
        header,
        spec: spec.clone(),
        origin_workspace_root: resume.workspace.clone(),
        origin_last_sequence,
        origin_last_event_hash: origin_last_event_hash.to_string(),
        payload,
    };
    image.validate()?;
    Ok(image)
}

/// Writes an admitted image into this node, leaving a suspended process row its
/// desktop half can resume.
///
/// Order matters and is the reverse of what reads it: files, then the session,
/// then the process row, then the manifest that points at all three. The row is
/// created before the manifest is rewritten because the manifest has to name the
/// *local* process id, and that id does not exist until the row does.
pub fn land_migration(
    app_data_dir: &Path,
    paths: &DaemonPaths,
    image: &MigrationImage,
    now_ms: u64,
) -> Result<LandedMigration, String> {
    let workspace_root = paths
        .root
        .join("migrations")
        .join(&image.header.run_id)
        .join("workspace");
    std::fs::create_dir_all(&workspace_root)
        .map_err(|error| format!("Could not create the landing workspace: {error}"))?;
    for file in &image.payload.workspace_files {
        file.write(&workspace_root)?;
    }

    let checkpoint_dir = checkpoints_dir(app_data_dir).join(&image.header.checkpoint_id);
    std::fs::create_dir_all(&checkpoint_dir)
        .map_err(|error| format!("Could not create the landing checkpoint: {error}"))?;
    for file in &image.payload.checkpoint_files {
        // The origin's own `manifest.json` is skipped: the manifest written
        // below is that file with its paths rewritten, and letting the raw copy
        // land first would leave a window in which the checkpoint on disk names
        // the origin's directories.
        if file.path == MANIFEST_FILE {
            continue;
        }
        file.write(&checkpoint_dir)?;
    }

    if let Some(session) = image.payload.session.as_ref() {
        merge_session(app_data_dir, session)?;
    }

    let ledger = RunLedger::open(&paths.ledger_db).map_err(|error| error.to_string())?;
    let table = ledger.process_table();
    let now = i64::try_from(now_ms).map_err(|_| "Migration timestamp overflow".to_string())?;
    // `external_id` is the origin's process id, which is the only durable link
    // back to the row this image was frozen from. The local `process_id` is
    // minted here rather than copied: two machines' `agent_processes` are two
    // tables, and one of them adopting the other's ids would make a later audit
    // that read both unable to say which row it was looking at.
    let record = table
        .admit(
            &AdmitProcess::new(ProcessKind::ChatTurn, image.header.process_id.clone())
                .with_run(image.header.run_id.clone())
                .with_workspace(Some(workspace_root.to_string_lossy().to_string())),
            now,
        )
        .map_err(|error| error.to_string())?;
    // Admitted → Running → Suspended: the transition table has no direct
    // admitted→suspended edge, and rightly so — a process that never ran cannot
    // be resumable. This one *did* run, on the origin, and the two steps are the
    // honest way to say so on a table that only records local history.
    table
        .transition(&record.process_id, ProcessState::Running, None, now)
        .map_err(|error| error.to_string())?;
    table
        .transition(&record.process_id, ProcessState::Suspended, None, now)
        .map_err(|error| error.to_string())?;

    let manifest = localise_manifest(
        &image.header.manifest,
        &record.process_id,
        image.origin_workspace_root.as_deref(),
        &workspace_root,
    )?;
    write_manifest(&checkpoint_dir, &manifest)?;

    Ok(LandedMigration {
        process_id: record.process_id,
        workspace_root,
    })
}

/// Rewrites the frozen manifest so every path in it names this machine.
///
/// A manifest carried across a move still points at the origin's directories,
/// and a revert run against it would either fail or — worse, on a machine that
/// happens to have the same paths — restore backups over an unrelated file. So
/// the rewrite is not cosmetic: a manifest whose entries cannot be re-rooted is
/// refused rather than landed half-translated.
fn localise_manifest(
    manifest: &CheckpointManifest,
    process_id: &str,
    origin_root: Option<&str>,
    local_root: &Path,
) -> Result<CheckpointManifest, String> {
    let mut manifest = manifest.clone();
    if let Some(resume) = manifest.resume.as_mut() {
        resume.process_id = process_id.to_string();
        if resume.workspace.is_some() {
            resume.workspace = Some(local_root.to_string_lossy().to_string());
        }
    }
    let Some(origin_root) = origin_root else {
        return Ok(manifest);
    };
    for entry in &mut manifest.entries {
        let relative = Path::new(&entry.path)
            .strip_prefix(origin_root)
            .map_err(|_| {
                format!(
                    "'{}' is outside the migrated workspace, so this image cannot be re-rooted here",
                    entry.path
                )
            })?;
        entry.path = local_root.join(relative).to_string_lossy().to_string();
    }
    Ok(manifest)
}

fn read_manifest(dir: &Path) -> Result<CheckpointManifest, String> {
    let raw = std::fs::read_to_string(dir.join(MANIFEST_FILE))
        .map_err(|error| format!("Could not read '{}': {error}", dir.display()))?;
    serde_json::from_str(&raw).map_err(|error| format!("Checkpoint manifest is invalid: {error}"))
}

fn write_manifest(dir: &Path, manifest: &CheckpointManifest) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| format!("Could not serialize the landed manifest: {error}"))?;
    let temporary = dir.join("manifest.json.tmp");
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("Could not write the landed manifest: {error}"))?;
    std::fs::rename(&temporary, dir.join(MANIFEST_FILE))
        .map_err(|error| format!("Could not publish the landed manifest: {error}"))
}

/// Extracts one session object from the origin's sessions blob.
fn read_session(
    app_data_dir: &Path,
    session_id: &str,
) -> Result<Option<serde_json::Value>, String> {
    let path = app_data_dir.join(SESSIONS_FILE);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let blob: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|error| format!("The sessions file is invalid: {error}"))?;
    Ok(blob
        .get("sessions")
        .and_then(serde_json::Value::as_array)
        .and_then(|sessions| {
            sessions.iter().find(|session| {
                session.get("id").and_then(serde_json::Value::as_str) == Some(session_id)
            })
        })
        .cloned())
}

/// Adds the migrated conversation to this node's sessions blob.
///
/// Appends rather than replaces, and skips an id that is already present: the
/// target's sessions file is the local user's, and a migration is not a licence
/// to overwrite a conversation they are having. A duplicate id means the same
/// session already landed, which is the replay case the transport already makes
/// safe.
fn merge_session(app_data_dir: &Path, session: &serde_json::Value) -> Result<(), String> {
    let Some(session_id) = session.get("id").and_then(serde_json::Value::as_str) else {
        return Err("The migrated session has no id".to_string());
    };
    let path = app_data_dir.join(SESSIONS_FILE);
    let mut blob: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| format!("The sessions file is invalid: {error}"))?,
        Err(_) => serde_json::json!({ "sessions": [], "activeSessionId": null }),
    };
    let sessions = blob
        .get_mut("sessions")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| "The sessions file has no session list".to_string())?;
    if sessions
        .iter()
        .any(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(session_id))
    {
        return Ok(());
    }
    sessions.push(session.clone());
    let bytes = serde_json::to_vec(&blob)
        .map_err(|error| format!("Could not serialize the sessions file: {error}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("Could not write the sessions file: {error}"))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("Could not publish the sessions file: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::checkpoints::{CheckpointEntry, ResumeState};

    fn manifest(workspace: Option<&str>, entry_path: &str) -> CheckpointManifest {
        CheckpointManifest {
            version: 3,
            created_at_ms: 1,
            session_id: "session-01".to_string(),
            anchor_index: 0,
            label: "a turn".to_string(),
            shell_ran: false,
            external_effects: vec![],
            committed_effects: None,
            reverted: false,
            prev_id: None,
            entries: vec![CheckpointEntry {
                path: entry_path.to_string(),
                backup: Some("0.bak".to_string()),
                redo: None,
                after: None,
            }],
            remembered_facts: vec![],
            staged_task_suggestions: vec![],
            resume: Some(ResumeState {
                process_id: "turn-origin".to_string(),
                frozen_at_ms: 5,
                model: Some("qwen3-8b".to_string()),
                runtime_id: None,
                workspace: workspace.map(str::to_string),
                pending_approvals: vec![],
            }),
        }
    }

    #[test]
    fn landing_re_roots_every_path_at_the_local_workspace() {
        let local = PathBuf::from("/target/work");
        let landed = localise_manifest(
            &manifest(Some("/origin/work"), "/origin/work/src/main.rs"),
            "turn-local",
            Some("/origin/work"),
            &local,
        )
        .expect("re-rooting succeeds");
        assert_eq!(landed.entries[0].path, "/target/work/src/main.rs");
        let resume = landed.resume.expect("the image is still a freeze");
        // The local row's id, not the origin's — otherwise the desktop's resume
        // path would look for a process this machine does not have.
        assert_eq!(resume.process_id, "turn-local");
        assert_eq!(resume.workspace.as_deref(), Some("/target/work"));
    }

    #[test]
    fn a_path_outside_the_migrated_workspace_refuses_rather_than_landing_half_translated() {
        let error = localise_manifest(
            &manifest(Some("/origin/work"), "/somewhere/else/notes.md"),
            "turn-local",
            Some("/origin/work"),
            Path::new("/target/work"),
        )
        .expect_err("an unrelated path cannot be re-rooted");
        assert!(error.contains("outside the migrated workspace"), "{error}");
    }

    #[test]
    fn a_model_only_turn_lands_with_no_workspace_to_re_root() {
        let landed = localise_manifest(
            &manifest(None, "/origin/work/src/main.rs"),
            "turn-local",
            None,
            Path::new("/target/work"),
        )
        .expect("a workspace-less image still lands");
        assert!(landed.resume.expect("still a freeze").workspace.is_none());
        // Untouched: with no origin root there is nothing to rewrite against,
        // and guessing one would be inventing a mapping.
        assert_eq!(landed.entries[0].path, "/origin/work/src/main.rs");
    }

    #[test]
    fn merging_a_session_never_overwrites_one_the_local_user_already_has() {
        let dir = std::env::temp_dir().join(format!(
            "little-monkey-migration-session-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join(SESSIONS_FILE);
        std::fs::write(
            &path,
            serde_json::json!({
                "sessions": [{ "id": "s-1", "messages": ["local"] }],
                "activeSessionId": "s-1",
            })
            .to_string(),
        )
        .expect("seed sessions");

        merge_session(
            &dir,
            &serde_json::json!({ "id": "s-1", "messages": ["migrated"] }),
        )
        .expect("merge is a no-op for a session already here");
        merge_session(
            &dir,
            &serde_json::json!({ "id": "s-2", "messages": ["migrated"] }),
        )
        .expect("a new session is added");

        let blob: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read back"))
                .expect("valid json");
        let sessions = blob["sessions"].as_array().expect("session list");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["messages"][0], "local");
        assert_eq!(sessions[1]["id"], "s-2");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Persistent VDN jobs: submit, recover after a process crash, retry and cancel.
//! The journal is distinct from native sd-server requests because VDN's Python
//! environment, upstream code acknowledgment and territory grant are different.
//! It shares the native video GPU gate and publishes into the same Gallery.
use crate::profiles::ProfileScopedPaths;
use serde::{Deserialize, Serialize};
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::{fs, path::{Path, PathBuf}, sync::{Mutex, OnceLock}, collections::HashSet};
use tauri::AppHandle;

const MAX_ATTEMPTS: u32 = 3;

/// A native CUDA out-of-memory condition can be transient (GPU contention)
/// even after the worker has exhausted its progressive placement tiers.
fn recoverable_native_oom(error: &str) -> bool {
    let message = error.to_ascii_lowercase();
    message.contains("cuda out of memory")
        || message.contains("all native cuda placement tiers exhausted")
        || message.contains("cublas_status_alloc_failed")
        || message.contains("cuda error: out of memory")
}

const MAX_RECORD_BYTES: u64 = 64 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum VdnJobStatus { Queued, Running, Interrupted, Completed, Failed, Cancelled }
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all="camelCase")]
pub struct VdnJob {
    pub id: String,
    pub request: crate::studio_vdn::VdnRunRequest,
    pub status: VdnJobStatus,
    pub attempts: u32,
    pub result_entry_id: Option<String>,
    #[serde(default)]
    pub source_artifact_id: Option<String>,
    #[serde(default)]
    pub input_hashes: BTreeMap<String, String>,
    pub error: Option<String>,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default)]
    pub updated_at_ms: u64,
}
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64).unwrap_or(0)
}
fn local_inputs(request: &crate::studio_vdn::VdnRunRequest) -> BTreeMap<String, String> {
    let mut inputs = BTreeMap::new();
    if let Some(path) = &request.first_image_path { inputs.insert("first".into(), path.clone()); }
    if let Some(path) = &request.last_image_path { inputs.insert("last".into(), path.clone()); }
    if let Some(path) = &request.reference_video_path { inputs.insert("video".into(), path.clone()); }
    if let Some(path) = &request.reference_audio_path { inputs.insert("audio".into(), path.clone()); }
    for (index, path) in request.reference_image_paths.iter().enumerate() {
        inputs.insert(format!("image:{index}"), path.clone());
    }
    for (index, path) in request.lora_paths.iter().enumerate() {
        inputs.insert(format!("lora:{index}"), path.clone());
    }
    inputs
}

fn hash_local_file(path: &str) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|error|
        format!("VDN source file is unavailable: {error}"))?;
    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > 8 * 1024 * 1024 * 1024 {
        return Err("Native VDN input must be a bounded regular file".into());
    }
    let mut reader = std::io::BufReader::new(file);
    let mut digest = Sha256::new();
    let mut buf = [0_u8; 1024 * 1024];
    loop {
        let len = reader.read(&mut buf).map_err(|error| error.to_string())?;
        if len == 0 { break; }
        digest.update(&buf[..len]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn pin_local_inputs(request: &crate::studio_vdn::VdnRunRequest)
    -> Result<BTreeMap<String, String>, String> {
    local_inputs(request).into_iter()
        .map(|(label, path)| Ok((label, hash_local_file(&path)?))).collect()
}

fn verify_local_inputs(job: &VdnJob) -> Result<(), String> {
    let inputs = local_inputs(&job.request);
    if inputs.len() != job.input_hashes.len() {
        return Err("Native VDN local input set changed after submission".into());
    }
    for (label, path) in inputs {
        let current_hash = hash_local_file(&path)?;
        if job.input_hashes.get(&label).map(String::as_str) != Some(current_hash.as_str()) {
            return Err(format!("Native VDN {label} changed after job submission"));
        }
    }
    Ok(())
}

fn lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
fn active() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}
/// Remove only native generated scratch windows, never the published MP4,
/// original film identity, or content-addressed AV latent tail.
fn remove_segment_cache(app: &AppHandle, id: &str) -> Result<(), String> {
    uuid::Uuid::parse_str(id).map_err(|_| "Invalid native recovery cache job ID")?;
    let root = app.profile_data_dir().map_err(|e| e.to_string())?
        .join("studio-vdn-h3").join("temporal-segments");
    let root_meta = match fs::symlink_metadata(&root) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        return Err("Unsafe native recovery cache root".into());
    }
    let folder = root.join(id);
    let folder_meta = match fs::symlink_metadata(&folder) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if folder_meta.file_type().is_symlink() || !folder_meta.is_dir() {
        return Err("Unsafe native recovery cache directory".into());
    }
    fs::remove_dir_all(folder).map_err(|e| e.to_string())
}

/// Startup/lifecycle GC, never touches jobs that can be resumed or running.
/// For orphan directories without a journal we retain seven days for manual
/// recovery, then remove them if they are not registered in this process.
fn prune_native_cache(app: &AppHandle, jobs: &[VdnJob]) {
    let keep: HashSet<&str> = jobs.iter()
        .filter(|job| matches!(job.status, VdnJobStatus::Queued
            | VdnJobStatus::Running | VdnJobStatus::Interrupted)
            || (matches!(job.status, VdnJobStatus::Failed)
                && job.attempts < MAX_ATTEMPTS
                && now_ms().saturating_sub(job.updated_at_ms) < 7 * 86_400_000))
        .map(|job| job.id.as_str()).collect();
    let root = match app.profile_data_dir() {
        Ok(folder) => folder.join("studio-vdn-h3").join("temporal-segments"),
        Err(_) => return,
    };
    if !fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir() && !m.file_type().is_symlink()) {
        return;
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if uuid::Uuid::parse_str(&name).is_err() || keep.contains(name.as_str()) {
                continue;
            }
            if active().lock().is_ok_and(|live| live.contains(&name)) {
                continue;
            }
            let found = jobs.iter().find(|j| j.id == name);
            let expired_orphan = found.is_none()
                && entry.metadata().ok()
                    .and_then(|m| m.modified().ok())
                    .is_some_and(|when| std::time::SystemTime::now()
                        .duration_since(when).is_ok_and(|age|
                            age.as_secs() >= 7 * 24 * 3600));
            if found.is_some() || expired_orphan {
                let _ = remove_segment_cache(app, &name);
            }
        }
    }
}

fn directory(app: &AppHandle) -> Result<PathBuf, String> {
    let p = app.profile_data_dir().map_err(|e| e.to_string())?
        .join("studio-v1").join("vdn-jobs");
    fs::create_dir_all(&p).map_err(|e| e.to_string())?;
    let meta = fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err("Unsafe VDN journal directory".into());
    }
    Ok(p)
}
fn path(dir: &Path, id: &str) -> Result<PathBuf, String> {
    let parsed = uuid::Uuid::parse_str(id).map_err(|_| "Invalid VDN job ID")?;
    if parsed.to_string() != id { return Err("Noncanonical VDN job ID".into()); }
    Ok(dir.join(format!("{id}.json")))
}
fn load(dir: &Path, id: &str) -> Result<VdnJob, String> {
    let p = path(dir, id)?;
    let metadata = fs::symlink_metadata(&p).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink()
        || metadata.len() > MAX_RECORD_BYTES {
        return Err("Unsafe VDN journal record".into());
    }
    let j: VdnJob = serde_json::from_slice(&fs::read(p).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    if j.id != id || j.request.job_id != id { return Err("VDN journal ID mismatch".into()); }
    Ok(j)
}
fn store(dir: &Path, j: &VdnJob) -> Result<(), String> {
    let dest = path(dir, &j.id)?;
    let bytes = serde_json::to_vec(j).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_RECORD_BYTES { return Err("VDN journal too large".into()); }
    let temp = dir.join(format!(".{}.{}.tmp", j.id, uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true)
        .open(&temp).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    drop(file);
    fs::rename(&temp, dest).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    std::fs::File::open(dir).and_then(|handle| handle.sync_all())
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn modify(app: &AppHandle, id: &str, f: impl FnOnce(&mut VdnJob) -> Result<(), String>)
    -> Result<VdnJob, String> {
    let _guard = lock().lock().map_err(|e| e.to_string())?;
    let dir = directory(app)?;
    let mut j = load(&dir, id)?;
    f(&mut j)?;
    j.updated_at_ms = now_ms();
    store(&dir, &j)?;
    Ok(j)
}
fn list(app: &AppHandle) -> Result<Vec<VdnJob>, String> {
    let _guard = lock().lock().map_err(|e| e.to_string())?;
    let dir = directory(app)?;
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") { continue; }
        let id = entry.file_name().to_string_lossy().trim_end_matches(".json").to_string();
        found.push(load(&dir, &id)?);
    }
    found.sort_by(|a,b| a.id.cmp(&b.id));
    Ok(found)
}
async fn prepare_continuation(app: &AppHandle, job: &VdnJob)
    -> Result<(crate::studio_vdn::VdnRunRequest, Option<PathBuf>), String> {
    let mut request = job.request.clone();
    // The dependency has already been resolved to a pinned Gallery artifact.
    // Clear the orchestration-only predecessor ID before validating the
    // executable render request with its extracted keyframe or media.
    request.source_vdn_job_id = None;
    let Some(source_id) = request.source_gallery_entry_id.take() else {
        return Ok((request, None));
    };
    let entry = crate::generation_commands::generation_gallery(app.clone())?
        .into_iter().find(|entry| entry.entry_id == source_id)
        .ok_or("VDN continuation source no longer exists in this profile")?;
    if !matches!(entry.media_type.as_str(), "video/mp4" | "video/webm") {
        return Err("VDN continuation requires a video gallery entry".into());
    }
    if job.source_artifact_id.as_deref() != Some(entry.artifact_id.as_str()) {
        return Err("VDN continuation artifact changed since submission".into());
    }
    let root = app.profile_data_dir().map_err(|e| e.to_string())?;
    let store = crate::artifact_store::ArtifactStore::with_max_blob_size(
        root.join("content-v1"), 256 * 1024 * 1024
    ).map_err(|e| e.to_string())?;
    let bytes = store.read(&entry.artifact_id).map_err(|e| e.to_string())?;
    if request.continuation_mode == "latent_context" {
        let original_job_id = entry.video_job_id.as_deref()
            .ok_or("AV latent continuation requires a native VDN Gallery job")?;
        let parent = {
            let _guard = lock().lock().map_err(|e| e.to_string())?;
            load(&directory(app)?, original_job_id)?
        };
        if !matches!(parent.status, VdnJobStatus::Completed)
            || parent.request.backend != crate::studio_vdn::VdnBackend::Diffusers
            || parent.result_entry_id.as_deref() != Some(entry.entry_id.as_str()) {
            return Err("AV latent source must be a completed native Diffusers Gallery job".into());
        }
        let source_root = root.join("studio-vdn-h3").join("temporal-contexts");
        let root_meta = fs::symlink_metadata(&source_root)
            .map_err(|_| "Previous video has no saved AV latent context")?;
        if !root_meta.is_dir() || root_meta.file_type().is_symlink() {
            return Err("Unsafe native AV latent context directory".into());
        }
        let saved = source_root.join(format!("{original_job_id}.safetensors"));
        let meta = fs::symlink_metadata(&saved)
            .map_err(|_| "Previous video predates native AV context support; regenerate it")?;
        if !meta.is_file() || meta.file_type().is_symlink()
            || !(128..=64 * 1024 * 1024).contains(&meta.len()) {
            return Err("Unsafe or oversized saved native AV latent context".into());
        }
        // The native H3 FL2VA branch needs the original *first* picture in
        // Qwen3-VL vision conditioning, not the most recent predecessor frame.
        // Its per-job identity PNG has been copied along the original lineage,
        // SHA256-bound into the Safetensors snapshot and independently checked
        // again inside the native worker. Legacy generations must be rebuilt.
        let identity = source_root.join(format!("{original_job_id}.identity.png"));
        let identity_meta = fs::symlink_metadata(&identity)
            .map_err(|_| "Original film identity is missing; regenerate the native source")?;
        if !identity_meta.is_file() || identity_meta.file_type().is_symlink()
            || !(64..=8 * 1024 * 1024).contains(&identity_meta.len()) {
            return Err("Unsafe or oversized native original-picture identity".into());
        }
        request.resolved_temporal_identity_sha256 = Some(hash_local_file(
            identity.to_string_lossy().as_ref())?);
        request.resolved_temporal_identity_path =
            Some(identity.to_string_lossy().into_owned());
        // A later Gallery job must retain the *original* Ref2VA people/voice
        // references when source latents were generated by transformer_ref.
        // Walk the durable ancestry, not a user-controlled filename. Every
        // reused local image/video/audio is rechecked against its original
        // job's content hashes; missing/changed sources fail closed.
        if request.reference_image_paths.is_empty()
            && request.reference_video_path.is_none()
            && request.reference_audio_path.is_none()
        {
            let mut ancestor = parent.clone();
            let mut visited = HashSet::new();
            for _ in 0..8 {
                if !visited.insert(ancestor.id.clone()) {
                    return Err("Cycle in native VDN reference ancestry".into());
                }
                if !ancestor.request.reference_image_paths.is_empty()
                    || ancestor.request.reference_video_path.is_some()
                    || ancestor.request.reference_audio_path.is_some()
                {
                    verify_local_inputs(&ancestor)?;
                    request.reference_image_paths = ancestor.request.reference_image_paths.clone();
                    request.reference_video_path = ancestor.request.reference_video_path.clone();
                    request.reference_audio_path = ancestor.request.reference_audio_path.clone();
                    break;
                }
                let next_id = if let Some(prior) = &ancestor.request.source_vdn_job_id {
                    Some(prior.clone())
                } else if let Some(gallery) = &ancestor.request.source_gallery_entry_id {
                    crate::generation_commands::generation_gallery(app.clone())?
                        .into_iter()
                        .find(|item| &item.entry_id == gallery)
                        .and_then(|item| item.video_job_id)
                } else {
                    None
                };
                let Some(next_id) = next_id else { break; };
                let next = {
                    let _guard = lock().lock().map_err(|e| e.to_string())?;
                    load(&directory(app)?, &next_id)?
                };
                if !matches!(next.status, VdnJobStatus::Completed) {
                    return Err("Native AV reference lineage contains unfinished source".into());
                }
                ancestor = next;
            }
        }
        request.resolved_temporal_context_path = Some(saved.to_string_lossy().into_owned());
        request.resolved_temporal_source_sha256 =
            Some(format!("{:x}", Sha256::digest(&bytes)));
        request.resolved_temporal_source_job_id = Some(original_job_id.into());
        // The previous Gallery clip contains the entire published chain.
        // A profile-owned scratch copy is handed to the native re-muxer;
        // the pinned SHA is checked again by the worker before joining.
        if entry.media_type != "video/mp4" {
            return Err("Native AV continuation requires an MP4 Gallery source".into());
        }
        let scratch = directory(app)?.join(format!("{}.temporal-parent.mp4", job.id));
        fs::write(&scratch, &bytes).map_err(|e| e.to_string())?;
        request.resolved_temporal_parent_path = Some(scratch.to_string_lossy().into_owned());
        return Ok((request, Some(scratch)));
    }
    if request.continuation_mode == "video_reference" {
        // Retain temporal motion *and* the original soundtrack by supplying the
        // pinned clip directly to native MiniMax-H3 Ref2VA reference decoding.
        let suffix = if entry.media_type == "video/webm" { "webm" } else { "mp4" };
        let temp = directory(app)?.join(format!("{}.source.{suffix}", job.id));
        fs::write(&temp, &bytes).map_err(|e| e.to_string())?;
        request.reference_video_path = Some(temp.to_string_lossy().into_owned());
        return Ok((request, Some(temp)));
    }
    // The retained AV prefix starts 22 frames before the previous ending.
    // An extracted *final* frame is therefore NOT the first frame of this
    // window: injecting it as FL2VA would conflict with the fixed prefix.
    // Use the actual latent context alone, with optional caller Ref2VA refs.
    if request.continuation_mode == "latent_context" {
        return Ok((request, None));
    }
    let png_base64 = crate::studio_video_jobs::extract_final_frame(
        app, &bytes, &entry.media_type
    ).await?;
    let png = base64::engine::general_purpose::STANDARD.decode(png_base64)
        .map_err(|e| e.to_string())?;
    let dest = directory(app)?.join(format!("{}.first.png", job.id));
    fs::write(&dest, png).map_err(|e| e.to_string())?;
    request.first_image_path = Some(dest.to_string_lossy().into_owned());
    Ok((request, Some(dest)))
}

/// The direct renderer checks this immediately after acquiring the shared
/// engine gate, closing the race between a queued cancellation and subprocess spawn.
pub(crate) fn is_cancelled(app: &AppHandle, id: &str) -> Result<bool, String> {
    let _guard = lock().lock().map_err(|e| e.to_string())?;
    let dir = directory(app)?;
    if !path(&dir, id)?.exists() { return Ok(false); }
    Ok(matches!(load(&dir, id)?.status, VdnJobStatus::Cancelled))
}

/// A native preview is usable only if the prior job is complete in this
/// profile, has a published Gallery artifact, and its own request generated
/// a real checkpoint using the native Diffusers backend.
pub(crate) fn completed_with_preview(app: &AppHandle, id: &str) -> Result<bool, String> {
    let _guard = lock().lock().map_err(|error| error.to_string())?;
    let dir = directory(app)?;
    let job = load(&dir, id)?;
    if !matches!(job.status, VdnJobStatus::Completed)
        || !job.request.native_two_pass
        || job.result_entry_id.is_none()
    { return Ok(false); }
    let checkpoint_root = app.profile_data_dir().map_err(|error| error.to_string())?
        .join("studio-vdn-h3").join("latent-checkpoints");
    let dir = fs::symlink_metadata(&checkpoint_root).map_err(|error| error.to_string())?;
    if dir.file_type().is_symlink() || !dir.is_dir() {
        return Err("Native VDN checkpoint directory is unsafe".into());
    }
    let preview = checkpoint_root.join(format!("{id}.safetensors"));
    let clips: Vec<PathBuf> = if job.request.segments > 1 {
        (0..job.request.segments)
            .map(|part| PathBuf::from(format!("{}.segment{part}.safetensors",
                preview.display())))
            .collect()
    } else {
        vec![preview]
    };
    Ok(clips.iter().all(|path| {
        fs::symlink_metadata(path).is_ok_and(|m|
            m.is_file() && !m.file_type().is_symlink()
            && m.len() > 128 && m.len() <= 64 * 1024 * 1024)
    }))
}

fn published_entry(app: &AppHandle, id: &str) -> Result<Option<String>, String> {
    Ok(crate::generation_commands::generation_gallery(app.clone())?
        .into_iter()
        .find(|entry| entry.video_job_id.as_deref() == Some(id))
        .map(|entry| entry.entry_id))
}

/// A child is dispatched only after its parent has published an actual
/// content-addressed video. No placeholder source or successful status alone
/// is sufficient to unlock the dependency.
fn predecessor(app: &AppHandle, child: &VdnJob)
    -> Result<Option<(String, String)>, String> {
    let Some(parent_id) = child.request.source_vdn_job_id.as_deref() else {
        return Ok(Some((String::new(), String::new())));
    };
    let jobs = list(app)?;
    let parent = jobs.into_iter().find(|job| job.id == parent_id)
        .ok_or("Native VDN predecessor not found in this profile")?;
    match parent.status {
        VdnJobStatus::Completed => {
            let entry_id = parent.result_entry_id.ok_or(
                "VDN predecessor is completed without a Gallery entry")?;
            let entry = crate::generation_commands::generation_gallery(app.clone())?
                .into_iter().find(|item| item.entry_id == entry_id
                    && item.video_job_id.as_deref() == Some(parent_id))
                .ok_or("VDN predecessor Gallery artifact disappeared")?;
            Ok(Some((entry_id, entry.artifact_id)))
        },
        VdnJobStatus::Failed | VdnJobStatus::Cancelled | VdnJobStatus::Interrupted =>
            Err("Native VDN predecessor failed or was cancelled".into()),
        VdnJobStatus::Running | VdnJobStatus::Queued => Ok(None),
    }
}

fn dispatch_waiting(app: &AppHandle) {
    if let Ok(jobs) = list(app) {
        for job in jobs {
            if matches!(job.status, VdnJobStatus::Queued) {
                dispatch(app.clone(), job.id);
            }
        }
    }
}

fn dispatch(app: AppHandle, id: String) {
    let already = active().lock().map(|mut running| !running.insert(id.clone())).unwrap_or(true);
    if already { return; }
    tauri::async_runtime::spawn(async move {
        let dependency = {
            let pending = list(&app).ok().and_then(|jobs| jobs.into_iter()
                .find(|job| job.id == id));
            match pending {
                Some(ref child) => predecessor(&app, child),
                None => Err("Queued VDN job no longer exists".into()),
            }
        };
        if dependency.as_ref().ok().is_some_and(|ready| ready.is_none()) {
            if let Ok(mut jobs) = active().lock() { jobs.remove(&id); }
            return;
        }
        if let Err(error) = &dependency {
            let _ = modify(&app, &id, |j| {
                if matches!(j.status, VdnJobStatus::Queued) {
                    j.status = VdnJobStatus::Failed;
                    j.error = Some(error.clone());
                }
                Ok(())
            });
            if let Ok(mut jobs) = active().lock() { jobs.remove(&id); }
            dispatch_waiting(&app);
            return;
        }
        let job = modify(&app, &id, |j| {
            if !matches!(j.status, VdnJobStatus::Queued) { return Err("VDN job not queued".into()); }
            if let Some((entry_id, artifact_id)) = dependency.as_ref().ok().and_then(|v| v.as_ref()) {
                if j.request.source_vdn_job_id.is_some() {
                    j.request.source_gallery_entry_id = Some(entry_id.clone());
                    j.source_artifact_id = Some(artifact_id.clone());
                }
            }
            j.status = VdnJobStatus::Running;
            j.attempts += 1;
            Ok(())
        });
        let mut requeue_delay = None;
        if let Ok(j) = job {
            // Gallery publication is the durable commit point. Recover a crash
            // between writing the gallery and updating this journal idempotently.
            if let Ok(Some(entry_id)) = published_entry(&app, &id) {
                let _ = modify(&app, &id, |record| {
                    record.status = VdnJobStatus::Completed;
                    record.result_entry_id = Some(entry_id);
                    record.error = None;
                    Ok(())
                });
                if let Ok(mut active) = active().lock() { active.remove(&id); }
                return;
            }
            let verified = verify_local_inputs(&j);
            let result = match verified {
                Err(error) => Err(error),
                Ok(()) => match prepare_continuation(&app, &j).await {
                Ok((request, scratch)) => {
                    let result = crate::studio_vdn::generation_vdn_run(app.clone(), request).await;
                    if let Some(path) = scratch { let _ = fs::remove_file(path); }
                    result
                }
                Err(error) => Err(error),
                },
            };
            let updated = modify(&app, &id, |j| {
                if matches!(j.status, VdnJobStatus::Cancelled) && result.is_err() {
                    return Ok(());
                }
                match &result {
                    Ok(entry) => {
                        j.result_entry_id = Some(entry.entry_id.clone());
                        j.status = VdnJobStatus::Completed;
                        j.error = None;
                    }
                    Err(error) => {
                        // Bounded autonomous recovery after every native
                        // placement tier has OOMed; retain finished segments
                        // and sampler checkpoints. Other failures stay failed.
                        j.status = if j.attempts < MAX_ATTEMPTS
                            && j.request.backend == crate::studio_vdn::VdnBackend::Diffusers
                            && recoverable_native_oom(error) {
                            VdnJobStatus::Queued
                        } else {
                            VdnJobStatus::Failed
                        };
                        j.error = Some(error.chars().take(500).collect());
                    }
                }
                Ok(())
            });
            if result.is_ok() {
                crate::studio_video_timeline::notify_video_published(&app);
            }
            if let Ok(ref retry) = updated {
                if matches!(retry.status, VdnJobStatus::Queued) {
                    requeue_delay = Some((retry.attempts as u64).saturating_mul(10).min(30));
                }
            }
            if let Ok(jobs) = list(&app) {
                if let Some(finished) = jobs.iter().find(|row| row.id == id) {
                    if matches!(finished.status, VdnJobStatus::Completed | VdnJobStatus::Cancelled)
                        || (matches!(finished.status, VdnJobStatus::Failed)
                            && finished.attempts >= MAX_ATTEMPTS) {
                        let _ = remove_segment_cache(&app, &id);
                    }
                }
            }
        }
        if let Ok(mut running) = active().lock() { running.remove(&id); }
        if let Some(delay) = requeue_delay {
            // Another process may release VRAM before the next tiered attempt.
            // The persisted journal remains resumable if the app closes here.
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }
        dispatch_waiting(&app);
    });
}
#[tauri::command(rename_all="camelCase")]
pub fn generation_vdn_submit(app: AppHandle, request: crate::studio_vdn::VdnRunRequest)
    -> Result<VdnJob, String> {
    crate::generation_commands::ensure_vdn_usage(&app)?;
    crate::studio_vdn::validate_for_durable_submit(&request)?;
    let id = request.job_id.clone();
    if let Some(parent_id) = &request.source_vdn_job_id {
        let records = list(&app)?;
        if !records.iter().any(|row| row.id == *parent_id) {
            return Err("Native VDN predecessor must already be a persisted job".into());
        }
    }

    let source_artifact_id = if let Some(source_id) = request.source_gallery_entry_id.as_deref() {
        let entry = crate::generation_commands::generation_gallery(app.clone())?
            .into_iter().find(|entry| entry.entry_id == source_id)
            .ok_or("Video continuation source does not exist")?;
        if !matches!(entry.media_type.as_str(), "video/mp4" | "video/webm") {
            return Err("VDN source must be an MP4 or WebM gallery clip".into());
        }
        Some(entry.artifact_id)
    } else { None };
    let input_hashes = pin_local_inputs(&request)?;
    let timestamp = now_ms();
    let job = VdnJob { id: id.clone(), request, status: VdnJobStatus::Queued,
        attempts: 0, result_entry_id: None, source_artifact_id, input_hashes, error: None,
        created_at_ms: timestamp, updated_at_ms: timestamp };
    {
        let _guard = lock().lock().map_err(|e| e.to_string())?;
        let dir = directory(&app)?;
        if path(&dir, &id)?.exists() { return Err("VDN job ID already exists".into()); }
        store(&dir, &job)?;
    }
    dispatch(app, id);
    Ok(job)
}
#[tauri::command]
pub fn generation_vdn_jobs(app: AppHandle) -> Result<Vec<VdnJob>, String> { list(&app) }
#[tauri::command(rename_all="camelCase")]
pub fn generation_vdn_resume(app: AppHandle, job_id: String) -> Result<VdnJob, String> {
    crate::generation_commands::ensure_vdn_usage(&app)?;
    let job = modify(&app, &job_id, |j| {
        if !matches!(j.status, VdnJobStatus::Interrupted | VdnJobStatus::Failed) {
            return Err("Only interrupted/failed VDN jobs may restart".into());
        }
        if j.attempts >= MAX_ATTEMPTS { return Err("VDN retry limit reached".into()); }
        j.status = VdnJobStatus::Queued;
        j.error = None;
        Ok(())
    })?;
    dispatch(app, job_id);
    Ok(job)
}
#[tauri::command(rename_all="camelCase")]
pub fn generation_vdn_cancel_job(app: AppHandle, job_id: String) -> Result<VdnJob, String> {
    let j = modify(&app, &job_id, |j| {
        if !matches!(j.status, VdnJobStatus::Queued | VdnJobStatus::Running | VdnJobStatus::Interrupted) {
            return Err("VDN job is already terminal".into());
        }
        j.status = VdnJobStatus::Cancelled;
        Ok(())
    })?;
    let _ = crate::studio_vdn::generation_vdn_cancel(job_id);
    dispatch_waiting(&app);
    Ok(j)
}
/// Called at app startup; an interrupted CUDA sampler restarts from the
/// persisted seed, never pretends to resume unavailable model internals.
pub fn start_background_recovery(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let Ok(jobs) = list(&app) else { return; };
        prune_native_cache(&app, &jobs);
        for j in jobs {
            if let Ok(Some(entry_id)) = published_entry(&app, &j.id) {
                let _ = modify(&app, &j.id, |record| {
                    record.status = VdnJobStatus::Completed;
                    record.result_entry_id = Some(entry_id);
                    record.error = None;
                    Ok(())
                });
                continue;
            }
            match j.status {
                VdnJobStatus::Running => {
                    if let Ok(updated) = modify(&app, &j.id, |j| {
                        j.status = if j.attempts < MAX_ATTEMPTS {
                            VdnJobStatus::Queued
                        } else { VdnJobStatus::Interrupted };
                        j.error = Some("Recovered after app exit; resuming verified native segment/checkpoint".into());
                        Ok(())
                    }) {
                        if matches!(updated.status, VdnJobStatus::Queued) { dispatch(app.clone(), j.id); }
                    }
                }
                VdnJobStatus::Queued => dispatch(app.clone(), j.id),
                _ => {}
            }
        }
        // Parent publication may have been recovered after an earlier child
        // was scanned. Retry dependency dispatch once the journal is reconciled.
        dispatch_waiting(&app);
    });
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journal_reloads_exact_seed_and_source_identity() {
        let root = std::env::temp_dir().join(format!("vdn-durable-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let request: crate::studio_vdn::VdnRunRequest = serde_json::from_value(
            serde_json::json!({
                "jobId": id, "pythonExecutable": "/not/executed/python",
                "prompt": "fixed seed", "steps": 8, "frames": 141,
                "seed": 712, "firstImagePath": null, "lastImagePath": null,
                "sourceGalleryEntryId": "published-parent",
                "fp8": false, "offloadDit": true,
                "backend": "diffusers", "acknowledgeThirdPartyCode": true
            })
        ).unwrap();
        let record = VdnJob {
            id: id.clone(),
            request,
            status: VdnJobStatus::Interrupted,
            attempts: 1,
            result_entry_id: None,
            source_artifact_id: Some("deadbeef".repeat(8)),
            input_hashes: BTreeMap::new(),
            error: Some("app crashed".into()),
            created_at_ms: 10,
            updated_at_ms: 11,
        };
        store(&root, &record).unwrap();
        let loaded = load(&root, &id).unwrap();
        assert_eq!(loaded.request.seed, 712);
        assert_eq!(loaded.request.source_gallery_entry_id.as_deref(), Some("published-parent"));
        assert_eq!(loaded.source_artifact_id, record.source_artifact_id);
        assert_eq!(loaded.attempts, 1);
        assert_eq!(loaded.created_at_ms, 10);
        // Atomic replacement must work repeatedly on Windows as well as Unix.
        let mut updated = loaded;
        updated.status = VdnJobStatus::Completed;
        updated.result_entry_id = Some("published-clip".into());
        updated.updated_at_ms = 42;
        store(&root, &updated).unwrap();
        store(&root, &updated).unwrap();
        let persisted = load(&root, &id).unwrap();
        assert!(matches!(persisted.status, VdnJobStatus::Completed));
        assert_eq!(persisted.result_entry_id.as_deref(), Some("published-clip"));
        assert_eq!(persisted.updated_at_ms, 42);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sha256_source_pin_detects_changed_bytes() {
        let root = std::env::temp_dir().join(format!("vdn-hash-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let asset = root.join("original.png");
        fs::write(&asset, b"first version").unwrap();
        let first = hash_local_file(asset.to_str().unwrap()).unwrap();
        fs::write(&asset, b"second version").unwrap();
        let second = hash_local_file(asset.to_str().unwrap()).unwrap();
        assert_ne!(first, second);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_rejects_traversal() {
        assert!(path(Path::new("/tmp"), "../secret").is_err());
        assert!(path(Path::new("/tmp"), "not-a-uuid").is_err());
    }
}

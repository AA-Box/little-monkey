//! Durable, profile-scoped Studio video job journal.
//!
//! A completed gallery entry is the publication checkpoint. Neither sd-server
//! nor the packaged MLX video service exports sampler/frame checkpoints, so
//! interrupted sampling is *not* represented as resumable mid-frame state.
//! Resuming explicitly starts a new render of the persisted, fixed-seed request.

use crate::generation::{GenerationRequest, GenerationTask};
use base64::Engine as _;
use crate::generation_commands::{self, GenerationEntry};
use crate::profiles::ProfileScopedPaths;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const MAX_JOBS: usize = 128;
const MAX_JOB_BYTES: u64 = 48 * 1024 * 1024;
const MAX_DEPENDENCIES: usize = 16;
const MAX_ATTEMPTS: u32 = 3;
const MAX_HANDOFF_VIDEO_BYTES: usize = 256 * 1024 * 1024;
const MAX_HANDOFF_PNG_BYTES: u64 = 22 * 1024 * 1024;
const FRAME_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoJobStatus {
    Queued,
    Running,
    Interrupted,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoJobStage {
    WaitingForDependencies,
    PreparingInput,
    Rendering,
    Published,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct VideoJob {
    id: String,
    idempotency_key: String,
    depends_on: Vec<String>,
    #[serde(default)]
    source_job_id: Option<String>,
    /// Published Studio clip/timeline: use its last frame with the local I2V engine.
    #[serde(default)]
    source_gallery_entry_id: Option<String>,
    #[serde(default)]
    source_artifact_id: Option<String>,
    request: GenerationRequest,
    status: VideoJobStatus,
    stage: VideoJobStage,
    attempts: u32,
    result_entry_ids: Vec<String>,
    error: Option<String>,
    created_at_ms: u64,
    updated_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoJobView {
    pub(crate) id: String,
    pub(crate) depends_on: Vec<String>,
    pub(crate) source_job_id: Option<String>,
    pub(crate) source_gallery_entry_id: Option<String>,
    pub(crate) source_artifact_id: Option<String>,
    pub(crate) model_id: String,
    pub(crate) prompt: String,
    pub(crate) seed: i64,
    pub(crate) status: VideoJobStatus,
    pub(crate) stage: VideoJobStage,
    pub(crate) attempts: u32,
    pub(crate) result_entry_ids: Vec<String>,
    pub(crate) error: Option<String>,
    pub(crate) created_at_ms: u64,
    pub(crate) updated_at_ms: u64,
}

impl From<&VideoJob> for VideoJobView {
    fn from(job: &VideoJob) -> Self {
        Self {
            id: job.id.clone(),
            depends_on: job.depends_on.clone(),
            source_job_id: job.source_job_id.clone(),
            source_gallery_entry_id: job.source_gallery_entry_id.clone(),
            source_artifact_id: job.source_artifact_id.clone(),
            model_id: job.request.model_id.clone(),
            prompt: job.request.prompt.chars().take(240).collect(),
            seed: job.request.seed,
            status: job.status,
            stage: job.stage,
            attempts: job.attempts,
            result_entry_ids: job.result_entry_ids.clone(),
            error: job.error.clone(),
            created_at_ms: job.created_at_ms,
            updated_at_ms: job.updated_at_ms,
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|time| time.as_millis() as u64)
        .unwrap_or(0)
}

fn journal_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn active() -> &'static Mutex<HashSet<String>> {
    static RUNNING: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn engine_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn directory(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.profile_data_dir()
        .map_err(|error| error.to_string())?
        .join("studio-v1").join("video-jobs");
    fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    // Check the leaf *before* chmod: chmod follows symlinks and could modify
    // an outside directory if a local attacker swapped the journal path.
    let meta = fs::symlink_metadata(&dir).map_err(|error| error.to_string())?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err("Studio video journal must be an app-owned directory".to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    Ok(dir)
}

fn path_for(dir: &Path, id: &str) -> Result<PathBuf, String> {
    // Never use caller-controlled path segments without validation.
    let parsed = Uuid::parse_str(id).map_err(|_| "Invalid video job id".to_string())?;
    if parsed.to_string() != id {
        return Err("Invalid video job id".to_string());
    }
    Ok(dir.join(format!("{id}.json")))
}

fn read_file(path: &Path) -> Result<VideoJob, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > MAX_JOB_BYTES {
        return Err("Unsafe or oversized Studio video job record".to_string());
    }
    let job: VideoJob = serde_json::from_slice(
        &fs::read(path).map_err(|error| error.to_string())?
    ).map_err(|error| format!("Invalid Studio video job record: {error}"))?;
    if path.file_name().and_then(|name| name.to_str()) != Some(format!("{}.json", job.id).as_str()) {
        return Err("Studio video journal filename/id mismatch".to_string());
    }
    Ok(job)
}

fn write_file(dir: &Path, job: &VideoJob) -> Result<(), String> {
    let path = path_for(dir, &job.id)?;
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err("Unsafe Studio video job destination".to_string());
        }
    }
    let bytes = serde_json::to_vec(job).map_err(|error| error.to_string())?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err("Video request exceeds the durable journal limit".to_string());
    }
    let temporary = dir.join(format!(".{}.{}.tmp", job.id, Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<(), String> {
        let mut file = options.open(&temporary).map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, &path).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        {
            fs::File::open(dir).and_then(|folder| folder.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_all(dir: &Path) -> Result<Vec<VideoJob>, String> {
    let mut jobs = Vec::new();
    for entry in fs::read_dir(dir).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        if entry.path().extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        if jobs.len() >= MAX_JOBS {
            return Err("Studio video journal exceeds its job limit".to_string());
        }
        jobs.push(read_file(&entry.path())?);
    }
    jobs.sort_by_key(|job| job.created_at_ms);
    Ok(jobs)
}

fn saved_results(app: &AppHandle, id: &str) -> Result<Vec<String>, String> {
    let entries = generation_commands::generation_gallery(app.clone())?;
    Ok(entries.into_iter()
        .filter(|entry| entry.video_job_id.as_deref() == Some(id))
        .map(|entry| entry.entry_id).collect())
}

fn reconcile_one(job: &mut VideoJob, is_active: bool, results: Vec<String>) -> bool {
    if !matches!(job.status, VideoJobStatus::Running | VideoJobStatus::Interrupted | VideoJobStatus::Failed) {
        return false;
    }
    if !results.is_empty() {
        job.status = VideoJobStatus::Completed;
        job.stage = VideoJobStage::Published;
        job.result_entry_ids = results;
        job.error = None;
    } else if job.status == VideoJobStatus::Running && !is_active {
        job.status = VideoJobStatus::Interrupted;
        job.error = Some("The app exited while rendering. The engine provides no mid-frame checkpoint; resume starts the saved request again.".to_string());
    } else {
        return false;
    }
    job.updated_at_ms = now_ms();
    true
}

fn reconcile(app: &AppHandle, dir: &Path, jobs: &mut [VideoJob]) -> Result<(), String> {
    let active_ids = active().lock().map_err(|error| error.to_string())?.clone();
    for job in jobs.iter_mut() {
        if matches!(job.status, VideoJobStatus::Running | VideoJobStatus::Interrupted | VideoJobStatus::Failed) {
            let results = saved_results(app, &job.id)?;
            if reconcile_one(job, active_ids.contains(&job.id), results) {
                write_file(dir, job)?;
            }
        }
    }
    Ok(())
}

/// Pin one random seed at submission. A persisted random request would have
/// no reproducible replay, and duplicate idempotency submissions must not
/// draw another random seed.
fn pin_seed(mut request: GenerationRequest) -> GenerationRequest {
    if request.seed < 0 {
        request.seed = (Uuid::new_v4().as_u128() as i64) & i64::MAX;
    }
    request
}

fn same_effective_request(saved: &GenerationRequest, incoming: &GenerationRequest) -> bool {
    let mut normalized = incoming.clone();
    if normalized.seed < 0 {
        normalized.seed = saved.seed;
    }
    serde_json::to_value(saved).ok() == serde_json::to_value(normalized).ok()
}

/// Boot-time job reconciliation and dispatch, independent of Studio frontend.
pub(crate) fn start_background_recovery(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        if let Err(error) = dispatch(&app) {
            eprintln!("Studio video startup recovery failed: {error}");
        }
    });
}

/// A single predecessor is automatically the media source. Multiple parents
/// need an explicit selector to avoid arbitrary output choices.
fn continuation_source(dependencies: &[String], explicit: Option<&str>) -> Result<Option<String>, String> {
    match explicit {
        Some(id) if dependencies.iter().any(|parent| parent == id) => Ok(Some(id.into())),
        Some(_) => Err("Video source must be one of the declared dependencies".into()),
        None if dependencies.len() == 1 => Ok(Some(dependencies[0].clone())),
        None if dependencies.len() > 1 => Err("Choose sourceJobId when a video has multiple dependencies".into()),
        None => Ok(None),
    }
}

fn parent_artifact(parent: &VideoJob, entries: &[GenerationEntry]) -> Result<GenerationEntry, String> {
    if parent.status != VideoJobStatus::Completed {
        return Err("Source video is not complete".into());
    }
    let entry = entries.iter().find(|entry|
        entry.video_job_id.as_deref() == Some(parent.id.as_str())
            && parent.result_entry_ids.contains(&entry.entry_id)
            && entry.task.is_video()
    ).ok_or("The source job has no published video artifact")?;
    if entry.media_type != "video/mp4" && entry.media_type != "video/webm" {
        return Err(format!("Unsupported continuation video format: {}", entry.media_type));
    }
    Ok(entry.clone())
}

/// A gallery ID is resolved against published media in the current profile.
/// Never accept an arbitrary caller-controlled path or artifact hash.
fn gallery_source_artifact(app: &AppHandle, entry_id: &str) -> Result<GenerationEntry, String> {
    let gallery = generation_commands::generation_gallery(app.clone())?;
    let entry = gallery.into_iter().find(|entry| entry.entry_id == entry_id)
        .ok_or("Source gallery clip no longer exists in this profile")?;
    if !matches!(entry.media_type.as_str(), "video/mp4" | "video/webm") {
        return Err("Source gallery entry must be an MP4 or WebM video".into());
    }
    Ok(entry)
}

/// Persisted gallery inputs are pinned to an artifact when submitted.
/// Rejecting a replacement is essential for deterministic retries: a reused
/// gallery ID must not silently switch source media between executions.
fn verify_source_artifact(job: &VideoJob, entry: &GenerationEntry) -> Result<(), String> {
    if let Some(expected) = job.source_artifact_id.as_deref() {
        if expected != entry.artifact_id {
            return Err("Source video changed since the job was submitted; refusing to render different media on retry".into());
        }
    }
    Ok(())
}

/// FFmpeg's image2 -update 1 overwrites one PNG for every decoded input frame.
/// The final PNG is therefore the decoded last frame rather than a thumbnail.
pub(crate) async fn extract_final_frame(app: &AppHandle, media: &[u8], mime: &str) -> Result<String, String> {
    let root = app.profile_data_dir().map_err(|error| error.to_string())?
        .join("studio-v1").join("video-handoff");
    extract_final_frame_in(&root, media, mime).await
}

async fn extract_final_frame_in(root: &Path, media: &[u8], mime: &str) -> Result<String, String> {
    if media.is_empty() || media.len() > MAX_HANDOFF_VIDEO_BYTES {
        return Err("Source video is empty or too large".into());
    }
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    if fs::symlink_metadata(&root).map_err(|error| error.to_string())?.file_type().is_symlink() {
        return Err("Unsafe video handoff directory".into());
    }
    let scratch = root.join(Uuid::new_v4().to_string());
    fs::create_dir(&scratch).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))
            .map_err(|error| error.to_string())?;
    }
    let input = scratch.join(if mime == "video/webm" { "input.webm" } else { "input.mp4" });
    let output = scratch.join("last.png");
    let result = async {
        fs::write(&input, media).map_err(|error| error.to_string())?;
        // A Finder-launched macOS app does not inherit Homebrew's shell PATH.
        // Prefer the normal installation locations before falling back to PATH.
        #[cfg(target_os = "macos")]
        let ffmpeg_path = ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg"]
            .iter().map(Path::new).find(|path| path.is_file())
            .map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("ffmpeg"));
        #[cfg(not(target_os = "macos"))]
        let ffmpeg_path = PathBuf::from("ffmpeg");
        let mut cmd = Command::new(ffmpeg_path);
        cmd.args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&input)
            .args(["-map", "0:v:0", "-an", "-sn", "-dn", "-vsync", "0",
                   "-f", "image2", "-update", "1", "-y"])
            .arg(&output)
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .kill_on_drop(true);
        let status = tokio::time::timeout(FRAME_EXTRACTION_TIMEOUT, cmd.status())
            .await.map_err(|_| "Final-frame extraction exceeded two minutes".to_string())?
            .map_err(|error| format!("FFmpeg unavailable for video continuation: {error}. Install FFmpeg and retry."))?;
        if !status.success() {
            return Err("FFmpeg could not decode the source video's final frame".into());
        }
        let meta = fs::symlink_metadata(&output).map_err(|error| error.to_string())?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_HANDOFF_PNG_BYTES {
            return Err("Decoded final frame is unsafe or too large".into());
        }
        let bytes = fs::read(&output).map_err(|error| error.to_string())?;
        if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.len() < 24 {
            return Err("Final-frame decoder returned no PNG".into());
        }
        let w = u32::from_be_bytes(bytes[16..20].try_into().unwrap_or([0; 4]));
        let h = u32::from_be_bytes(bytes[20..24].try_into().unwrap_or([0; 4]));
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return Err("Decoded frame dimensions exceed engine limits".into());
        }
        Ok(base64::engine::general_purpose::STANDARD.encode(&bytes))
    }.await;
    let _ = fs::remove_dir_all(&scratch);
    result
}

/// Resolve a real video blob from the verified artifact store and bind its
/// decoded last frame to the child's image-to-video request.
async fn prepared_request(app: &AppHandle, job: &VideoJob) -> Result<GenerationRequest, String> {
    if job.source_job_id.is_none() && job.source_gallery_entry_id.is_none() {
        return Ok(job.request.clone());
    }
    let entry = if let Some(source_id) = &job.source_job_id {
        let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
        let dir = directory(app)?;
        let parent = read_file(&path_for(&dir, source_id)?)?;
        let gallery = generation_commands::generation_gallery(app.clone())?;
        parent_artifact(&parent, &gallery)?
    } else if let Some(entry_id) = &job.source_gallery_entry_id {
        gallery_source_artifact(app, entry_id)?
    } else {
        return Err("No video source selected".into());
    };
    verify_source_artifact(job, &entry)?;
    let base = app.profile_data_dir().map_err(|error| error.to_string())?;
    let store = crate::artifact_store::ArtifactStore::with_max_blob_size(
        base.join("content-v1"), MAX_HANDOFF_VIDEO_BYTES as u64
    ).map_err(|error| error.to_string())?;
    // Integrity check happens in the content store before FFmpeg decodes.
    let bytes = store.read(&entry.artifact_id).map_err(|error| error.to_string())?;
    let png = extract_final_frame(app, &bytes, &entry.media_type).await?;
    transition(app, &job.id, |record| {
        record.source_artifact_id = Some(entry.artifact_id.clone());
        Ok(())
    })?;
    let mut request = job.request.clone();
    request.task = GenerationTask::ImageToVideo;
    request.init_image_base64 = Some(png);
    Ok(request)
}

fn dependencies_complete(job: &VideoJob, jobs: &[VideoJob]) -> bool {
    job.depends_on.iter().all(|id| jobs.iter().any(|candidate| {
        candidate.id == *id && candidate.status == VideoJobStatus::Completed
    }))
}

fn ready_jobs(jobs: &[VideoJob]) -> Vec<String> {
    jobs.iter().filter(|job|
        job.status == VideoJobStatus::Queued && dependencies_complete(job, jobs)
    ).map(|job| job.id.clone()).collect()
}

fn dispatch(app: &AppHandle) -> Result<(), String> {
    let ready = {
        let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
        let dir = directory(app)?;
        let mut jobs = read_all(&dir)?;
        reconcile(app, &dir, &mut jobs)?;
        ready_jobs(&jobs)
    };
    for id in ready {
        let inserted = active().lock().map_err(|error| error.to_string())?.insert(id.clone());
        if !inserted {
            continue;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            // The managed engine is shared: never swap models while another
            // video job is rendering, including while waiting on MLX ownership.
            let _gate = engine_gate().lock().await;
            let _ = execute(&app, &id).await;
            if let Ok(mut active) = active().lock() {
                active.remove(&id);
            }
            let _ = dispatch(&app);
        });
    }
    Ok(())
}

fn transition(
    app: &AppHandle,
    id: &str,
    update: impl FnOnce(&mut VideoJob) -> Result<(), String>,
) -> Result<VideoJob, String> {
    let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
    let dir = directory(app)?;
    let mut job = read_file(&path_for(&dir, id)?)?;
    update(&mut job)?;
    job.updated_at_ms = now_ms();
    write_file(&dir, &job)?;
    Ok(job)
}

async fn execute(app: &AppHandle, id: &str) -> Result<(), String> {
    let job = transition(app, id, |job| {
        if job.status != VideoJobStatus::Queued {
            return Err("Video job is no longer queued".to_string());
        }
        job.status = VideoJobStatus::Running;
        job.stage = if job.source_job_id.is_some() || job.source_gallery_entry_id.is_some() { VideoJobStage::PreparingInput } else { VideoJobStage::Rendering };
        job.attempts += 1;
        job.error = None;
        Ok(())
    })?;
    let result: Result<Vec<GenerationEntry>, String> = async {
        let request = prepared_request(app, &job).await?;
        transition(app, id, |record| {
            if record.status != VideoJobStatus::Running {
                return Err("Video job cancelled during media preparation".into());
            }
            record.stage = VideoJobStage::Rendering;
            Ok(())
        })?;
        generation_commands::generation_run_with_job(
            app.clone(),
            app.state::<crate::AppState>(),
            app.state::<crate::m3_commands::M3CommandState>(),
            request,
            id,
        ).await
    }.await;
    let _ = transition(app, id, |job| {
        // Cancellation wins over late engine errors, but published artifacts
        // are never discarded because a cancel raced with publication.
        match &result {
            Ok(entries) => {
                job.status = VideoJobStatus::Completed;
                job.stage = VideoJobStage::Published;
                job.result_entry_ids = entries.iter().map(|entry| entry.entry_id.clone()).collect();
                job.error = None;
            }
            Err(error) if job.status != VideoJobStatus::Cancelled => {
                job.status = VideoJobStatus::Failed;
                job.error = Some(error.chars().take(500).collect());
            }
            Err(_) => {}
        }
        Ok(())
    });
    if result.is_ok() {
        crate::studio_video_timeline::notify_video_published(app);
    }
    result.map(|_| ())
}

/// Durable video work is always assigned to a locally installed model.
/// A remote model ID must never be accepted as an accidental fallback to an
/// external service. Check this in Rust, not only in the frontend picker.
fn validate_local_video_model(app: &AppHandle, request: &GenerationRequest) -> Result<(), String> {
    if crate::generation::parse_remote_model_id(&request.model_id).is_some() {
        return Err("Durable native video jobs cannot invoke a remote video backend".into());
    }
    let models = generation_commands::generation_models(app.clone())?;
    let model = models.iter().find(|model| model.spec.id == request.model_id)
        .ok_or("Selected native video model does not exist")?;
    if !model.spec.supports(request.task) {
        return Err("Selected local model does not support this video task".into());
    }
    if !model.installed {
        return Err("Selected native video model must be installed before submitting work".into());
    }
    if !model.license_accepted {
        return Err("The selected video model's license must be accepted before use".into());
    }
    if !model.fits_in_memory {
        return Err("Selected video model requires more RAM than is available on this machine".into());
    }
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_submit(
    app: AppHandle,
    request: GenerationRequest,
    idempotency_key: String,
    depends_on: Vec<String>,
    source_job_id: Option<String>,
    source_gallery_entry_id: Option<String>,
) -> Result<VideoJobView, String> {
    if !request.task.is_video() {
        return Err("Durable Studio jobs accept only video tasks".to_string());
    }
    validate_local_video_model(&app, &request)?;
    if idempotency_key.is_empty() || idempotency_key.len() > 128
        || !idempotency_key.bytes().all(|ch| ch.is_ascii_alphanumeric() || b"_-.:".contains(&ch)) {
        return Err("Video idempotency key must be 1–128 safe ASCII characters".to_string());
    }
    if depends_on.len() > MAX_DEPENDENCIES || depends_on.iter().collect::<BTreeSet<_>>().len() != depends_on.len() {
        return Err("Too many or duplicate video dependencies".to_string());
    }
    let source = continuation_source(&depends_on, source_job_id.as_deref())?;
    if source.is_some() && request.init_image_base64.is_some() {
        return Err("A chained video cannot also supply another starting frame".into());
    }
    let pinned_gallery_artifact = if let Some(entry_id) = source_gallery_entry_id.as_deref() {
        if source.is_some() || !depends_on.is_empty() || request.init_image_base64.is_some() {
            return Err("A gallery continuation cannot mix job dependencies or another starting frame".into());
        }
        if request.task != GenerationTask::ImageToVideo {
            return Err("Gallery continuation requires an image-to-video model".into());
        }
        // Resolve the immutable content hash before journaling, not only when
        // a queued worker finally starts. This pins the input across restarts.
        Some(gallery_source_artifact(&app, entry_id)?.artifact_id)
    } else {
        None
    };
    let result = {
        let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
        let dir = directory(&app)?;
        let mut jobs = read_all(&dir)?;
        reconcile(&app, &dir, &mut jobs)?;
        if let Some(old) = jobs.iter().find(|job| job.idempotency_key == idempotency_key) {
            if !same_effective_request(&old.request, &request)
                || old.depends_on != depends_on || old.source_job_id != source
                || old.source_gallery_entry_id != source_gallery_entry_id
                || (pinned_gallery_artifact.is_some() && old.source_artifact_id != pinned_gallery_artifact) {
                return Err("Video idempotency key already belongs to a different request".to_string());
            }
            VideoJobView::from(old)
        } else {
            if jobs.len() >= MAX_JOBS {
                return Err("Studio video job history is full (128 jobs)".to_string());
            }
            for dependency in &depends_on {
                if !jobs.iter().any(|job| &job.id == dependency) {
                    return Err(format!("Unknown video dependency {dependency}"));
                }
            }
            let time = now_ms();
            let job = VideoJob {
                id: Uuid::new_v4().to_string(),
                idempotency_key,
                depends_on,
                source_job_id: source,
                source_gallery_entry_id: source_gallery_entry_id,
                source_artifact_id: pinned_gallery_artifact,
                request: pin_seed(request),
                status: VideoJobStatus::Queued,
                stage: VideoJobStage::WaitingForDependencies,
                attempts: 0,
                result_entry_ids: Vec::new(),
                error: None,
                created_at_ms: time,
                updated_at_ms: time,
            };
            write_file(&dir, &job)?;
            VideoJobView::from(&job)
        }
    };
    dispatch(&app)?;
    Ok(result)
}

/// Create a new scene using the parent's published video as the source.
#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_continue(
    app: AppHandle,
    parent_job_id: String,
    prompt: String,
    idempotency_key: String,
) -> Result<VideoJobView, String> {
    let mut request = {
        let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
        let parent = read_file(&path_for(&directory(&app)?, &parent_job_id)?)?;
        parent.request
    };
    if prompt.trim().is_empty() {
        return Err("A continuation needs a new scene prompt".into());
    }
    request.prompt = prompt;
    request.task = GenerationTask::ImageToVideo;
    request.init_image_base64 = None;
    request.mask_image_base64 = None;
    generation_video_submit(app, request, idempotency_key, vec![parent_job_id.clone()], Some(parent_job_id), None)
}

#[tauri::command]
pub fn generation_video_jobs(app: AppHandle) -> Result<Vec<VideoJobView>, String> {
    let views = {
        let _guard = journal_lock().lock().map_err(|error| error.to_string())?;
        let dir = directory(&app)?;
        let mut jobs = read_all(&dir)?;
        reconcile(&app, &dir, &mut jobs)?;
        jobs.iter().rev().map(VideoJobView::from).collect::<Vec<_>>()
    };
    dispatch(&app)?;
    Ok(views)
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_resume(app: AppHandle, job_id: String) -> Result<VideoJobView, String> {
    let job = transition(&app, &job_id, |job| {
        if !matches!(job.status, VideoJobStatus::Interrupted | VideoJobStatus::Failed) {
            return Err("Only interrupted or failed video jobs can be resumed".to_string());
        }
        if job.request.seed < 0 {
            return Err("A random-seed job cannot be replayed deterministically".to_string());
        }
        if job.attempts >= MAX_ATTEMPTS {
            return Err("Video render retry limit reached".to_string());
        }
        job.status = VideoJobStatus::Queued;
        job.stage = VideoJobStage::WaitingForDependencies;
        job.error = None;
        Ok(())
    })?;
    dispatch(&app)?;
    Ok(VideoJobView::from(&job))
}

#[tauri::command(rename_all = "camelCase")]
pub fn generation_video_cancel(app: AppHandle, job_id: String) -> Result<VideoJobView, String> {
    let mut was_rendering = false;
    let job = transition(&app, &job_id, |job| {
        if !matches!(job.status, VideoJobStatus::Queued | VideoJobStatus::Running | VideoJobStatus::Interrupted) {
            return Err("Video job is already terminal".to_string());
        }
        was_rendering = job.status == VideoJobStatus::Running;
        job.status = VideoJobStatus::Cancelled;
        job.error = None;
        Ok(())
    })?;
    if was_rendering
        && active().lock().map_err(|error| error.to_string())?.contains(&job_id) {
        // Stop only when the job actually owns the native generation process.
        // A queued job waiting for the gate must not stop somebody else's run.
        let _ = app.state::<crate::AppState>().generation_engine.stop();
    }
    Ok(VideoJobView::from(&job))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example(id: &str, status: VideoJobStatus, depends_on: Vec<String>) -> VideoJob {
        VideoJob {
            id: id.into(), idempotency_key: id.into(), depends_on,
            source_job_id: None, source_gallery_entry_id: None, source_artifact_id: None,
            request: serde_json::from_value(serde_json::json!({
                "modelId":"model", "task":"text_to_video","prompt":"test",
                "width":512,"height":512,"steps":10,"cfgScale":4,"seed":1
            })).unwrap(),
            status, stage: VideoJobStage::WaitingForDependencies, attempts: 0,
            result_entry_ids: vec![], error: None, created_at_ms: 1, updated_at_ms: 1,
        }
    }

    #[test]
    fn dependencies_require_all_successful_predecessors() {
        let root = example("a", VideoJobStatus::Completed, vec![]);
        let waiting = example("b", VideoJobStatus::Interrupted, vec![]);
        let child = example("c", VideoJobStatus::Queued, vec!["a".into(), "b".into()]);
        assert!(!dependencies_complete(&child, &[root.clone(), waiting]));
        assert!(dependencies_complete(&child, &[root, example("b", VideoJobStatus::Completed, vec![])]));
    }

    #[test]
    fn journal_round_trips_request_without_losing_seed_or_dependency_ids() {
        let dir = std::env::temp_dir().join(format!("lm-video-jobs-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let mut job = example(&Uuid::new_v4().to_string(), VideoJobStatus::Queued, vec!["parent".into()]);
        job.request.seed = 42;
        write_file(&dir, &job).unwrap();
        let restored = read_all(&dir).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].request.seed, 42);
        assert_eq!(restored[0].depends_on, vec!["parent"]);
        assert_eq!(restored[0].status, VideoJobStatus::Queued);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_reconciles_published_and_interrupted_jobs_without_replaying_them() {
        let mut published = example("parent", VideoJobStatus::Running, vec![]);
        assert!(reconcile_one(&mut published, false, vec!["gallery-1".into()]));
        assert_eq!(published.status, VideoJobStatus::Completed);
        assert_eq!(published.stage, VideoJobStage::Published);
        let mut orphan = example("orphan", VideoJobStatus::Running, vec![]);
        assert!(reconcile_one(&mut orphan, false, vec![]));
        assert_eq!(orphan.status, VideoJobStatus::Interrupted);
        assert!(!reconcile_one(&mut orphan, false, vec![]));
        let child = example("child", VideoJobStatus::Queued, vec!["parent".into()]);
        let ready = ready_jobs(&[published, orphan, child]);
        assert_eq!(ready, vec!["child"]);
        // No replay of a crashed native sampler without user approval.
        assert!(!ready.contains(&"orphan".to_string()));
    }

    #[test]
    fn startup_keeps_legitimately_active_renderers_running() {
        let mut running = example("active", VideoJobStatus::Running, vec![]);
        assert!(!reconcile_one(&mut running, true, vec![]));
        assert_eq!(running.status, VideoJobStatus::Running);
    }

    #[test]
    fn random_seeds_are_pinned_once_and_deduplicate_retries() {
        let mut incoming = example("input", VideoJobStatus::Queued, vec![]).request;
        incoming.seed = -1;
        let fixed = pin_seed(incoming.clone());
        assert!(fixed.seed >= 0);
        assert!(same_effective_request(&fixed, &incoming));
        let mut changed = incoming;
        changed.prompt = "different request".into();
        assert!(!same_effective_request(&fixed, &changed));
    }

    #[test]
    fn single_dependency_is_an_automatic_video_source() {
        assert_eq!(continuation_source(&["a".into()], None).unwrap(), Some("a".into()));
        assert!(continuation_source(&["a".into(), "b".into()], None).is_err());
        assert_eq!(continuation_source(&["a".into(), "b".into()], Some("b")).unwrap(), Some("b".into()));
        assert!(continuation_source(&["a".into()], Some("other")).is_err());
    }

    #[test]
    fn gallery_source_is_backward_compatible_with_existing_video_journals() {
        let mut value = serde_json::to_value(example("job", VideoJobStatus::Queued, vec![])).unwrap();
        value.as_object_mut().unwrap().remove("sourceGalleryEntryId");
        let restored: VideoJob = serde_json::from_value(value).unwrap();
        assert!(restored.source_gallery_entry_id.is_none());
        let mut sourced = restored;
        sourced.source_gallery_entry_id = Some("gallery-clip".into());
        assert_eq!(serde_json::to_value(&sourced).unwrap()["sourceGalleryEntryId"], "gallery-clip");
    }

    #[test]
    fn pinned_gallery_source_refuses_replacement_on_retry() {
        let mut job = example("job", VideoJobStatus::Queued, vec![]);
        job.source_gallery_entry_id = Some("source".into());
        job.source_artifact_id = Some("a".repeat(64));
        let mut source = GenerationEntry {
            entry_id: "source".into(),
            artifact_id: "a".repeat(64),
            video_job_id: None,
            model_id: "test".into(),
            task: GenerationTask::TextToVideo,
            prompt: String::new(),
            negative_prompt: String::new(),
            media_type: "video/mp4".into(),
            size_bytes: 10,
            width: 64,
            height: 64,
            steps: 1,
            cfg_scale: 1.0,
            seed: 1,
            frame_count: 1,
            fps: 1,
            duration_ms: 1000,
            created_at_ms: 1,
        };
        assert!(verify_source_artifact(&job, &source).is_ok());
        source.artifact_id = "b".repeat(64);
        assert!(verify_source_artifact(&job, &source).is_err());
        // Old journals without a pinned digest remain readable.
        job.source_artifact_id = None;
        assert!(verify_source_artifact(&job, &source).is_ok());
    }

    #[test]
    fn parent_must_be_completed_and_published() {
        let job = example("parent", VideoJobStatus::Completed, vec![]);
        assert!(parent_artifact(&job, &[]).is_err());
        let mut busy = job.clone();
        busy.status = VideoJobStatus::Running;
        assert!(parent_artifact(&busy, &[]).is_err());
    }

    /// Runs FFmpeg on a generated four-frame MP4 and proves that the result is
    /// the *fourth* frame, not merely a video thumbnail/first frame. CI installs
    /// FFmpeg for its Ubuntu Rust job; other platform runners may lack it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_mp4_decoder_hands_off_the_true_final_frame() {
        use base64::Engine as _;
        let root = std::env::temp_dir().join(format!("lm-handoff-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let input = root.join("source.mp4");
        let expected = root.join("expected.png");
        let made = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error",
                   "-f", "lavfi", "-i", "testsrc2=size=64x64:rate=2:duration=2",
                   "-c:v", "mpeg4", "-q:v", "3", "-y"])
            .arg(&input).status().expect("Linux CI must install ffmpeg");
        assert!(made.success(), "FFmpeg fixture must render a 4-frame video");
        let actual = super::extract_final_frame_in(
            &root, &fs::read(&input).unwrap(), "video/mp4",
        ).await.unwrap();
        let expected_status = std::process::Command::new("ffmpeg")
            .args(["-nostdin", "-hide_banner", "-loglevel", "error", "-i"])
            .arg(&input)
            .args(["-vf", "select=eq(n\\,3)", "-frames:v", "1", "-y"])
            .arg(&expected).status().unwrap();
        assert!(expected_status.success());
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(actual).unwrap(),
            fs::read(&expected).unwrap(),
            "Continuation must use the *final* decoded frame"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn untrusted_ids_cannot_traverse_job_directory() {
        let dir = Path::new("/tmp");
        assert!(path_for(dir, "../outside").is_err());
        assert!(path_for(dir, "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx").is_err());
    }
}

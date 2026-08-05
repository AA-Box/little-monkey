//! Desktop command surface for Studio — the image and video generation
//! section.
//!
//! Everything model-shaped lives in [`crate::generation`]; this module only
//! binds it to Tauri: resolving app-owned paths, reusing the existing Hugging
//! Face downloader for weight components, publishing finished media into the
//! shared artifact store, and keeping a small persisted gallery.
//!
//! Weights are never mirrored by the app. A component download is a direct,
//! user-initiated transfer from the model's own Hugging Face repo, gated on the
//! license acceptance recorded here — which matters for models whose terms
//! exclude whole territories (see [`crate::generation::LicenseGate`]).

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::artifact_store::ArtifactStore;
use crate::generation::{self, GenerationModelSpec, GenerationRequest, JobProgress};
use crate::managed_runtime::{self, STABLE_DIFFUSION};
use crate::AppState;

const GALLERY_FILE: &str = "studio-gallery.json";
/// The user's own model list. There is no built-in catalogue — this file is
/// the whole registry, and it starts empty.
const MODELS_FILE: &str = "studio-models.json";
const ACCEPTED_LICENSES_FILE: &str = "studio-accepted-licenses.json";
/// Keeps a corrupt or hand-edited gallery from being read without bound.
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
/// A long clip on a constrained machine can legitimately sample for a long
/// time; this exists so a wedged engine cannot poll forever.
const JOB_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// A curated model plus everything the picker needs to decide what to show.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationModelView {
    #[serde(flatten)]
    pub spec: GenerationModelSpec,
    pub installed: bool,
    /// What the model weighs on this machine, measured. A component the user
    /// supplied from their own disk carries no declared size, so a card built
    /// from the spec alone reports 0 GB for a model that is plainly there.
    pub total_bytes: u64,
    /// Bytes still to fetch, so the UI can promise the real download size
    /// rather than the model's full weight.
    pub missing_bytes: u64,
    pub license_accepted: bool,
    /// False when the host has too little memory for this model. The picker
    /// shows it disabled with the reason rather than letting a run stall.
    pub fits_in_memory: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationEntry {
    pub entry_id: String,
    pub artifact_id: String,
    pub model_id: String,
    pub task: generation::GenerationTask,
    pub prompt: String,
    pub negative_prompt: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    pub seed: i64,
    pub frame_count: u32,
    pub fps: u32,
    pub duration_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationEngineStatus {
    /// False on hosts where stable-diffusion.cpp publishes no binary; Studio
    /// hides itself rather than offering a surface that cannot run.
    pub supported: bool,
    pub engine_installed: bool,
    pub loaded_model_id: Option<String>,
    pub total_ram_bytes: u64,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationProgressEvent {
    job_id: String,
    phase: String,
    queue_position: u32,
    /// Completion of the sampling pass, 0–100. `None` while the engine is
    /// still loading weights, which is the only stretch it does not count.
    percent: Option<u32>,
    /// `(step, total)` behind `percent`, so the UI can say "7 / 25" rather
    /// than only a bar.
    step: Option<u32>,
    total_steps: Option<u32>,
}

impl GenerationProgressEvent {
    fn new(job_id: &str, phase: &str) -> Self {
        Self {
            job_id: job_id.to_string(),
            phase: phase.to_string(),
            queue_position: 0,
            percent: None,
            step: None,
            total_steps: None,
        }
    }

    fn with_progress(mut self, progress: Option<(u32, u32)>) -> Self {
        if let Some((step, total)) = progress {
            self.percent = Some((step * 100 / total).min(100));
            self.step = Some(step);
            self.total_steps = Some(total);
        }
        self
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_millis() as u64)
        .unwrap_or(0)
}

fn studio_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?
        .join("studio-v1");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
    Ok(dir)
}

/// Where a model's component files live: one flat directory per model id.
fn model_root(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = crate::models::models_dir(app)?.join("generation");
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("Failed to create {}: {error}", dir.display()))?;
    Ok(dir)
}

fn artifacts(app: &AppHandle) -> Result<ArtifactStore, String> {
    let base = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    ArtifactStore::with_max_blob_size(base.join("content-v1"), 256 * 1024 * 1024)
        .map_err(|error| error.to_string())
}

fn read_state<T: serde::de::DeserializeOwned + Default>(
    app: &AppHandle,
    file: &str,
) -> Result<T, String> {
    let path = studio_dir(app)?.join(file);
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Ok(T::default());
    };
    if metadata.len() > MAX_STATE_BYTES {
        return Err(format!("{file} exceeds its size limit"));
    }
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| format!("Invalid {file}: {error}"))
}

fn write_state<T: Serialize>(app: &AppHandle, file: &str, value: &T) -> Result<(), String> {
    let directory = studio_dir(app)?;
    let temporary = directory.join(format!(".{file}.{}.tmp", Uuid::new_v4()));
    std::fs::write(
        &temporary,
        serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, directory.join(file)).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        error.to_string()
    })
}

/// Physical memory, via the same `system-memory` probe the runtime hub uses
/// for its fit decisions. Returns 0 when the probe fails; callers treat that
/// as "unknown" and let the model through rather than blocking on a bad read.
fn total_ram_bytes() -> u64 {
    std::panic::catch_unwind(system_memory::total).unwrap_or(0)
}

/// The verified app-owned `sd-server`, materializing it from bundled resources
/// on first use exactly as the llama runtime does.
fn engine_binary(app: &AppHandle) -> Result<PathBuf, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    let resource_dir = app.path().resource_dir().ok();
    if let Ok(Some(path)) =
        managed_runtime::materialize_bundled_runtime_for(&STABLE_DIFFUSION, resource_dir.as_deref(), &app_data)
    {
        return Ok(path);
    }
    managed_runtime::find_managed_sd_server(Some(&app_data)).ok_or_else(|| {
        "The generation engine is not installed in this build; run `pnpm stage:runtime:sd` and rebuild"
            .to_string()
    })
}

#[tauri::command]
pub fn generation_engine_status(app: AppHandle) -> Result<GenerationEngineStatus, String> {
    let supported = managed_runtime::runtime_supported_here(&STABLE_DIFFUSION);
    let app_data = app.path().app_data_dir().ok();
    Ok(GenerationEngineStatus {
        supported,
        engine_installed: supported
            && managed_runtime::find_managed_sd_server(app_data.as_deref()).is_some(),
        loaded_model_id: app.state::<AppState>().generation_engine.loaded_model(),
        total_ram_bytes: total_ram_bytes(),
    })
}

/// Every model the user has added.
fn registry(app: &AppHandle) -> Result<Vec<GenerationModelSpec>, String> {
    read_state(app, MODELS_FILE)
}

fn find_registered(app: &AppHandle, id: &str) -> Result<GenerationModelSpec, String> {
    registry(app)?
        .into_iter()
        .find(|spec| spec.id == id)
        .ok_or_else(|| "That model is not in your library".to_string())
}

/// Adds or replaces a model. Slots are assigned by the caller — the app never
/// guesses which file fills which slot, because guessing wrong produces an
/// engine error that reads like a broken download rather than a wrong choice.
#[tauri::command]
pub fn generation_add_model(
    app: AppHandle,
    spec: GenerationModelSpec,
) -> Result<Vec<GenerationModelSpec>, String> {
    generation::validate_model_spec(&spec)?;
    let mut models = registry(&app)?;
    match models.iter_mut().find(|entry| entry.id == spec.id) {
        Some(existing) => *existing = spec,
        None => models.push(spec),
    }
    models.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    write_state(&app, MODELS_FILE, &models)?;
    Ok(models)
}

/// Forgets a model. Files the app downloaded for it are removed; files the
/// user pointed at on their own disk are left exactly where they are.
#[tauri::command]
pub fn generation_remove_model(app: AppHandle, model_id: String) -> Result<(), String> {
    let spec = find_registered(&app, &model_id)?;
    let mut models = registry(&app)?;
    models.retain(|entry| entry.id != model_id);
    write_state(&app, MODELS_FILE, &models)?;

    let owned = model_root(&app)?.join(&spec.id);
    if owned.is_dir() {
        std::fs::remove_dir_all(&owned)
            .map_err(|error| format!("Failed to remove {}: {error}", owned.display()))?;
    }
    Ok(())
}

#[tauri::command]
pub fn generation_models(app: AppHandle) -> Result<Vec<GenerationModelView>, String> {
    let root = model_root(&app)?;
    let accepted: BTreeSet<String> = read_state(&app, ACCEPTED_LICENSES_FILE)?;
    let ram = total_ram_bytes();
    Ok(registry(&app)?
        .into_iter()
        .map(|spec| {
            let missing = spec.missing_components(&root);
            let missing_bytes = missing.iter().map(|entry| entry.size_bytes).sum();
            GenerationModelView {
                installed: missing.is_empty(),
                total_bytes: spec.size_on_disk(&root),
                missing_bytes,
                license_accepted: !spec.license.acceptance_required
                    || accepted.contains(&spec.license.id),
                // A zero reading means the probe failed, not that the machine
                // has no memory — do not block the model on a failed probe.
                fits_in_memory: ram == 0 || ram >= spec.min_ram_bytes,
                spec,
            }
        })
        .collect())
}

/// Records that the user accepted a model's terms. Called from the license
/// sheet the UI shows before any weight is fetched.
#[tauri::command]
pub fn generation_accept_license(app: AppHandle, license_id: String) -> Result<(), String> {
    if license_id.is_empty() || license_id.len() > 128 {
        return Err("Invalid license id".to_string());
    }
    if !registry(&app)?
        .iter()
        .any(|spec| spec.license.id == license_id)
    {
        return Err("Unknown license".to_string());
    }
    let mut accepted: BTreeSet<String> = read_state(&app, ACCEPTED_LICENSES_FILE)?;
    accepted.insert(license_id);
    write_state(&app, ACCEPTED_LICENSES_FILE, &accepted)
}

/// Downloads whatever components of `model_id` are missing, straight from each
/// component's own Hugging Face repo. Progress rides the existing
/// `models://download-progress` event so Studio and the model manager show the
/// same transfer.
#[tauri::command]
pub async fn generation_download_model(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    model_id: String,
) -> Result<(), String> {
    let spec = find_registered(&app, &model_id)?;
    if spec.license.acceptance_required {
        let accepted: BTreeSet<String> = read_state(&app, ACCEPTED_LICENSES_FILE)?;
        if !accepted.contains(&spec.license.id) {
            return Err(format!(
                "{} must be accepted before these weights can be downloaded",
                spec.license.name
            ));
        }
    }

    let root = model_root(&app)?.join(&spec.id);
    std::fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create {}: {error}", root.display()))?;

    let cancel = Arc::new(CancellationToken::new());
    {
        let mut downloads = state.model_downloads.lock().map_err(|e| e.to_string())?;
        if downloads.contains_key(&spec.id) {
            return Err("This model is already downloading".to_string());
        }
        downloads.insert(spec.id.clone(), cancel.clone());
    }

    let result = async {
        for component in &spec.components {
            // A component the user supplied from their own disk is present or
            // it is not; there is nothing to fetch and nothing to own.
            let crate::generation::ComponentSource::HuggingFace { repo, file } = &component.source
            else {
                continue;
            };
            let destination = root.join(component.file_name());
            if destination.is_file() {
                continue;
            }
            // Download beside the destination and rename, so an interrupted
            // transfer never leaves a half file that looks installed.
            let temporary = root.join(format!(".{}.{}.part", component.file_name(), Uuid::new_v4()));
            let outcome =
                crate::models::download_to_file(&app, repo, file, &temporary, &cancel).await;
            if let Err(error) = outcome {
                let _ = std::fs::remove_file(&temporary);
                return Err(error);
            }
            std::fs::rename(&temporary, &destination).map_err(|error| {
                let _ = std::fs::remove_file(&temporary);
                format!("Failed to install {}: {error}", component.file_name())
            })?;
        }
        Ok(())
    }
    .await;

    state
        .model_downloads
        .lock()
        .map_err(|e| e.to_string())?
        .remove(&spec.id);
    result
}

#[tauri::command]
pub fn generation_cancel_download(
    state: tauri::State<'_, AppState>,
    model_id: String,
) -> Result<bool, String> {
    let downloads = state.model_downloads.lock().map_err(|e| e.to_string())?;
    Ok(match downloads.get(&model_id) {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    })
}

/// Bounds a synthesized utterance, which is a wav on disk rather than a
/// response body and so is not covered by the HTTP client's own limits.
const MAX_SPEECH_BYTES: u64 = 64 * 1024 * 1024;
/// A long utterance on a cold CPU still finishes well inside this.
const SPEECH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Synthesizes one utterance with the managed `llama-tts`.
///
/// Speech does not go through `sd-server` at all: `llama-tts` is a one-shot
/// process that loads its weights, writes a single wav and exits. There is no
/// server to keep warm, no job id and no queue, so this whole path is one
/// await rather than the submit-and-poll loop the diffusion engine needs.
async fn run_speech(
    app: &AppHandle,
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<generation::GeneratedMedia, String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    // Speech has its own verified tree, pinned ahead of the chat engine's.
    if let Ok(Some(path)) = managed_runtime::materialize_bundled_runtime_for(
        &managed_runtime::LLAMA_TTS,
        app.path().resource_dir().ok().as_deref(),
        &app_data,
    ) {
        return run_speech_with(app, &path, spec, request).await;
    }
    let binary = managed_runtime::find_managed_llama_tts(Some(&app_data))
        .ok_or("The speech engine is not installed in this build; run `pnpm stage:runtime:tts` and rebuild")?;
    run_speech_with(app, &binary, spec, request).await
}

/// The body of [`run_speech`], once the verified binary is known.
async fn run_speech_with(
    app: &AppHandle,
    binary: &std::path::Path,
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<generation::GeneratedMedia, String> {

    let output_path = studio_dir(app)?.join(format!("speech-{}.wav", Uuid::new_v4()));
    let args = generation::speech_args(spec, &model_root(app)?, request, &output_path)?;
    let run = tokio::process::Command::new(binary)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .output();
    let finished = tokio::time::timeout(SPEECH_TIMEOUT, run)
        .await
        .map_err(|_| "Speech generation exceeded its time limit".to_string())?
        .map_err(|error| format!("Failed to start the speech engine: {error}"))?;

    let read = (|| {
        if !finished.status.success() {
            // The engine's own diagnosis is the only actionable part of a
            // failure here — a missing vocoder and a wrong quantization both
            // exit non-zero and say so only on stderr.
            let (detail, _) = crate::output_cap::cap_tail(
                String::from_utf8_lossy(&finished.stderr).trim().to_string(),
                2_000,
            );
            return Err(if detail.is_empty() {
                format!("Speech engine exited ({})", finished.status)
            } else {
                format!("Speech engine exited ({}):\n{detail}", finished.status)
            });
        }
        let size = std::fs::metadata(&output_path)
            .map_err(|_| "The speech engine wrote no audio".to_string())?
            .len();
        if size == 0 {
            return Err("The speech engine wrote an empty file".to_string());
        }
        if size > MAX_SPEECH_BYTES {
            return Err("Generated audio exceeds its size limit".to_string());
        }
        std::fs::read(&output_path).map_err(|error| error.to_string())
    })();
    let _ = std::fs::remove_file(&output_path);

    Ok(generation::GeneratedMedia {
        bytes: read?,
        media_type: "audio/wav".to_string(),
        frame_count: 1,
        fps: 1,
    })
}

/// Runs one generation end to end: ensure the engine is serving the requested
/// model, submit, poll to a terminal state, then publish the media as an
/// artifact and record it in the gallery.
#[tauri::command]
pub async fn generation_run(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    request: GenerationRequest,
) -> Result<GenerationEntry, String> {
    let spec = find_registered(&app, &request.model_id)?;
    let request = generation::validate_request(&spec, &request)?;

    let media = if request.task.is_speech() {
        let _ = app.emit(
            "studio://progress",
            GenerationProgressEvent::new("speech", "running"),
        );
        run_speech(&app, &spec, &request).await?
    } else {
        run_diffusion(&app, &state, &spec, &request).await?
    };

    let blob = artifacts(&app)?
        .put(&media.bytes)
        .map_err(|error| error.to_string())?;
    let entry = GenerationEntry {
        entry_id: format!("studio-{}", Uuid::new_v4()),
        artifact_id: blob.id,
        model_id: spec.id.clone(),
        task: request.task,
        prompt: request.prompt.clone(),
        negative_prompt: request.negative_prompt.clone(),
        media_type: media.media_type,
        size_bytes: blob.size,
        width: request.width,
        height: request.height,
        steps: request.steps,
        cfg_scale: request.cfg_scale,
        seed: request.seed,
        frame_count: media.frame_count,
        fps: media.fps,
        duration_ms: if media.fps > 0 {
            u64::from(media.frame_count) * 1000 / u64::from(media.fps)
        } else {
            0
        },
        created_at_ms: now_ms(),
    };

    let mut gallery: Vec<GenerationEntry> = read_state(&app, GALLERY_FILE)?;
    gallery.push(entry.clone());
    gallery.sort_by_key(|item| item.created_at_ms);
    write_state(&app, GALLERY_FILE, &gallery)?;

    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new(&entry.entry_id, "completed"),
    );
    Ok(entry)
}

/// The `sd-server` half of [`generation_run`]: ensure the engine is serving
/// this model, submit, and poll to a terminal state.
///
/// The job API has no step counter, so the percentage in each progress event
/// is scraped from the engine's own output by
/// [`generation::GenerationEngineState`] rather than read from the poll body.
async fn run_diffusion(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
    spec: &GenerationModelSpec,
    request: &GenerationRequest,
) -> Result<generation::GeneratedMedia, String> {
    let root = model_root(app)?;
    let binary = engine_binary(app)?;
    let engine = &state.generation_engine;

    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new("", "loading"),
    );
    let base_url = engine.ensure_ready(&binary, spec, &root).await?;
    engine.clear_progress();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let job_id = generation::submit_job(&client, &base_url, spec, request).await?;
    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new(&job_id, "submitted"),
    );

    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            generation::cancel_job(&client, &base_url, &job_id).await;
            return Err("Generation exceeded its time limit".to_string());
        }
        match generation::poll_job(&client, &base_url, &job_id).await? {
            JobProgress::Running { queue_position } => {
                let mut event = GenerationProgressEvent::new(&job_id, "running")
                    .with_progress(engine.progress());
                event.queue_position = queue_position;
                let _ = app.emit("studio://progress", event);
            }
            JobProgress::Completed(media) => return Ok(*media),
            JobProgress::Failed(error) => return Err(error),
            JobProgress::Cancelled => return Err("Generation cancelled".to_string()),
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    }
}

#[tauri::command]
pub async fn generation_cancel(
    state: tauri::State<'_, AppState>,
    job_id: String,
) -> Result<bool, String> {
    if job_id.is_empty() || job_id.len() > 128 {
        return Err("Invalid job id".to_string());
    }
    // The engine is launched on a fresh port each time, so its address comes
    // from the running instance rather than a constant.
    let Some(base_url) = state.generation_engine.base_url() else {
        return Ok(false);
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    Ok(generation::cancel_job(&client, &base_url, &job_id).await)
}

#[tauri::command]
pub fn generation_gallery(app: AppHandle) -> Result<Vec<GenerationEntry>, String> {
    read_state(&app, GALLERY_FILE)
}

/// Base64 data URL for a gallery entry, used by both `<img>` and `<video>`.
/// The engine holds tens of gigabytes of weights, so callers preview from the
/// artifact store rather than regenerating.
#[tauri::command]
pub fn generation_media_data_url(
    app: AppHandle,
    artifact_id: String,
) -> Result<String, String> {
    let gallery: Vec<GenerationEntry> = read_state(&app, GALLERY_FILE)?;
    let entry = gallery
        .into_iter()
        .find(|entry| entry.artifact_id == artifact_id)
        .ok_or("That media is not present in the Studio gallery")?;
    let bytes = artifacts(&app)?
        .read(&entry.artifact_id)
        .map_err(|error| error.to_string())?;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    Ok(format!(
        "data:{};base64,{}",
        entry.media_type,
        STANDARD.encode(bytes)
    ))
}

/// Releases the engine's memory. The loaded weight set is tens of gigabytes,
/// so Studio never keeps it warm once the user is done.
#[tauri::command]
pub fn generation_unload_engine(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.generation_engine.stop()
}

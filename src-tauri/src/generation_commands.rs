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
use crate::generation::{
    self, GenerationModelSpec, GenerationRequest, JobProgress, GENERATION_PORT,
};
use crate::managed_runtime::{self, STABLE_DIFFUSION};
use crate::AppState;

const GALLERY_FILE: &str = "studio-gallery.json";
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

#[tauri::command]
pub fn generation_models(app: AppHandle) -> Result<Vec<GenerationModelView>, String> {
    let root = model_root(&app)?;
    let accepted: BTreeSet<String> = read_state(&app, ACCEPTED_LICENSES_FILE)?;
    let ram = total_ram_bytes();
    Ok(generation::curated_models()
        .into_iter()
        .map(|spec| {
            let missing = spec.missing_components(&root);
            let missing_bytes = missing.iter().map(|entry| entry.size_bytes).sum();
            GenerationModelView {
                installed: missing.is_empty(),
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
    if !generation::curated_models()
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
    let spec = generation::find_model(&model_id).ok_or("Unknown generation model")?;
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
            let destination = root.join(component.file_name());
            if destination.is_file() {
                continue;
            }
            // Download beside the destination and rename, so an interrupted
            // transfer never leaves a half file that looks installed.
            let temporary = root.join(format!(".{}.{}.part", component.file_name(), Uuid::new_v4()));
            let outcome = crate::models::download_to_file(
                &app,
                &component.repo,
                &component.file,
                &temporary,
                &cancel,
            )
            .await;
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

/// Runs one generation end to end: ensure the engine is serving the requested
/// model, submit, poll to a terminal state, then publish the media as an
/// artifact and record it in the gallery.
#[tauri::command]
pub async fn generation_run(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    request: GenerationRequest,
) -> Result<GenerationEntry, String> {
    let spec = generation::find_model(&request.model_id).ok_or("Unknown generation model")?;
    let request = generation::validate_request(&spec, &request)?;
    let root = model_root(&app)?;
    let binary = engine_binary(&app)?;

    let base_url = state
        .generation_engine
        .ensure_ready(&binary, &spec, &root, GENERATION_PORT)
        .await?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(120))
        .build()
        .map_err(|error| error.to_string())?;
    let job_id = generation::submit_job(&client, &base_url, &spec, &request).await?;
    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent {
            job_id: job_id.clone(),
            phase: "submitted".to_string(),
            queue_position: 0,
        },
    );

    let deadline = tokio::time::Instant::now() + JOB_TIMEOUT;
    let media = loop {
        if tokio::time::Instant::now() >= deadline {
            generation::cancel_job(&client, &base_url, &job_id).await;
            return Err("Generation exceeded its time limit".to_string());
        }
        match generation::poll_job(&client, &base_url, &job_id).await? {
            JobProgress::Running { queue_position } => {
                let _ = app.emit(
                    "studio://progress",
                    GenerationProgressEvent {
                        job_id: job_id.clone(),
                        phase: "running".to_string(),
                        queue_position,
                    },
                );
            }
            JobProgress::Completed(media) => break *media,
            JobProgress::Failed(error) => return Err(error),
            JobProgress::Cancelled => return Err("Generation cancelled".to_string()),
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
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
        GenerationProgressEvent {
            job_id,
            phase: "completed".to_string(),
            queue_position: 0,
        },
    );
    Ok(entry)
}

#[tauri::command]
pub async fn generation_cancel(job_id: String) -> Result<bool, String> {
    if job_id.is_empty() || job_id.len() > 128 {
        return Err("Invalid job id".to_string());
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    Ok(generation::cancel_job(
        &client,
        &format!("http://127.0.0.1:{GENERATION_PORT}"),
        &job_id,
    )
    .await)
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

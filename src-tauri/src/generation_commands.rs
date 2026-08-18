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
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::artifact_store::ArtifactStore;
use crate::generation::{
    self, EngineCommand, GenerationEngineKind, GenerationModelSpec, GenerationRequest, JobProgress,
};
use crate::managed_runtime::{self, STABLE_DIFFUSION};
use crate::profiles::ProfileScopedPaths;
use crate::studio_tools;
use crate::AppState;

const GALLERY_FILE: &str = "studio-gallery.json";
/// The user's own model list. There is no built-in catalogue — this file is
/// the whole registry, and it starts empty.
const MODELS_FILE: &str = "studio-models.json";
const ACCEPTED_LICENSES_FILE: &str = "studio-accepted-licenses.json";
/// The user's LoRA library. Separate from the model registry because a LoRA is
/// not a model: it fills no slot, launches no engine, and is chosen per run.
const LORAS_FILE: &str = "studio-loras.json";
/// The user's loose weight files — CLIPs, text encoders, VAEs. Separate from
/// the model registry because a model entry must be a whole model, and these
/// are shared between them.
const PARTS_FILE: &str = "studio-parts.json";
/// Remote generation endpoints — a ComfyUI the user runs, or a hosted
/// OpenAI-compatible image API. Separate from the model registry because a
/// backend has no weight files: nothing to download, verify, or launch.
const BACKENDS_FILE: &str = "studio-backends.json";
/// A ceiling on the backend list. Nobody has fifty ComfyUI servers; this only
/// stops a scripted caller from growing the file without bound.
const MAX_BACKENDS: usize = 32;
/// Keeps a corrupt or hand-edited gallery from being read without bound.
const MAX_STATE_BYTES: u64 = 8 * 1024 * 1024;
/// How long a running job may show *no sign of movement at all* before it is
/// called wedged and cancelled.
///
/// Not a budget for the whole run. A clip's cost is the user's own choice —
/// frames times resolution times steps, and then a VAE decode that dominates
/// everything else — so any wall-clock ceiling is really a hidden cap on the
/// settings the picker offers. This measures stillness instead: sampling step,
/// queue position, and the engine's own output are all watched, and a job that
/// changes none of them for this long has stopped rather than slowed. The
/// window is wide because the decode phase is genuinely quiet on some
/// architectures.
const JOB_STALL_TIMEOUT: Duration = Duration::from_secs(60 * 60);
/// Matches Runtime Hub's default timed keep-alive. Studio runs do not need a
/// second manual unload step, but reloading a video model after every prompt
/// would make the UI needlessly slow.
const STUDIO_ENGINE_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// One tool run, end to end. Unlike a generation this is a single synchronous
/// request — there is no job to poll — so the deadline covers the operation
/// itself and not just a round trip.
const TOOL_RUN_TIMEOUT: Duration = Duration::from_secs(300);

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
        .profile_data_dir()
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
        .profile_data_dir()
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

/// What to spawn for `spec`: the verified app-owned `sd-server`, or the video
/// service inside the user's installed MLX package.
///
/// Resolved per model rather than once per app because the two engines read
/// disjoint file formats — an MLX conversion is unreadable to
/// stable-diffusion.cpp and a GGUF is unreadable to MLX — so which program to
/// start is a property of the weights, not of the host.
fn engine_command(app: &AppHandle, spec: &GenerationModelSpec) -> Result<EngineCommand, String> {
    // K22: refuse to hand out an executable path at all while the startup
    // integrity check reports a tampered component.
    crate::self_integrity::ensure_loadable()?;
    let app_data = app
        .profile_data_dir()
        .map_err(|error| format!("Failed to resolve app data directory: {error}"))?;
    match spec.engine {
        GenerationEngineKind::MlxVideo => mlx_video_command(&app_data),
        GenerationEngineKind::StableDiffusionCpp => {
            let resource_dir = app.path().resource_dir().ok();
            if let Ok(Some(path)) = managed_runtime::materialize_bundled_runtime_for(
                &STABLE_DIFFUSION,
                resource_dir.as_deref(),
                &app_data,
            ) {
                return Ok(EngineCommand::binary(path));
            }
            managed_runtime::find_managed_sd_server(Some(&app_data))
                .map(EngineCommand::binary)
                .ok_or_else(|| {
                    "The generation engine is not installed in this build; run `pnpm stage:runtime:sd` and rebuild"
                        .to_string()
                })
        }
    }
}

/// The MLX package's own interpreter running the video service inside it.
///
/// Every launch re-verifies the whole installed tree — `verify_active` re-hashes
/// each file the signed manifest declares and refuses any file it does not — so
/// the script this returns is as verified as the interpreter that runs it. It is
/// resolved from the manifest's install root rather than named directly, because
/// a path assembled here that the installer never published would run unverified
/// code.
#[cfg(target_os = "macos")]
fn mlx_video_command(app_data: &Path) -> Result<EngineCommand, String> {
    let installer = crate::m3_production::production_mlx_installer(&app_data.join("m3"))
        .map_err(|error| error.to_string())?;
    let install = installer.verify_active().map_err(|error| {
        format!(
            "The MLX video engine is not ready: {error}. Install the MLX package from Settings \
             → Runtime Hub → Components."
        )
    })?;
    let service = install.version_directory.join(MLX_VIDEO_SERVICE_ENTRY);
    if !service.is_file() {
        return Err(format!(
            "The installed MLX package has no video service ({}). Install a package built after \
             this feature shipped.",
            MLX_VIDEO_SERVICE_ENTRY
        ));
    }
    Ok(EngineCommand {
        program: install.python_executable,
        prefix_args: vec![service.to_string_lossy().to_string()],
    })
}

/// The same, everywhere the MLX package cannot be installed at all.
#[cfg(not(target_os = "macos"))]
fn mlx_video_command(_app_data: &Path) -> Result<EngineCommand, String> {
    Err("The MLX video engine runs on Apple silicon only.".to_string())
}

/// The video service's path inside an installed MLX package, mirroring
/// `serviceEntry` for the text one. Kept in step with
/// `scripts/build-mlx-package.mjs`, which copies it there.
#[cfg(target_os = "macos")]
const MLX_VIDEO_SERVICE_ENTRY: &str = "service/mlx_video_server.py";

/// Whether the engine binary can actually start on this host.
///
/// `runtime_supported_here` only answers whether upstream publishes an sd
/// build for this target. It cannot answer whether that build runs here: the
/// Linux archive is compiled on Ubuntu 24.04 and needs glibc 2.38 /
/// GLIBCXX 3.4.32 plus a Vulkan loader, so on an older distribution the tree
/// stages and verifies perfectly and then dies at exec. Asking the binary to
/// print its version settles every one of those cases at once — loader
/// failures included — for the price of one short process.
///
/// Cached on the binary's identity — its path and modification time — because
/// this is on the path of every Studio status refresh and spawning a process
/// per refresh made switching tabs visibly slow.
///
/// Keyed rather than once-per-process: the answer *can* change while the app
/// runs. Re-staging the runtime after a failed extraction, or pointing
/// `LITTLE_MONKEY_SD_RUNTIME` at a build that works, writes a different file,
/// and a process-wide answer would keep reporting the old one's verdict until
/// the app was restarted — telling the user their fix did nothing.
fn engine_starts(binary: &Path) -> bool {
    /// A binary's identity for the probe cache: where it is, and the version
    /// of it that was probed. `None` is a file that could not be stat'd, which
    /// re-probes rather than pretending the answer is still good.
    type ProbeKey = (PathBuf, Option<SystemTime>);
    static STARTS: OnceLock<Mutex<HashMap<ProbeKey, bool>>> = OnceLock::new();
    let key: ProbeKey = (
        binary.to_path_buf(),
        std::fs::metadata(binary)
            .and_then(|meta| meta.modified())
            .ok(),
    );
    if let Ok(cache) = STARTS.get_or_init(Default::default).lock() {
        if let Some(&cached) = cache.get(&key) {
            return cached;
        }
    }
    // Probed outside the lock: it spawns a process, and holding the mutex
    // across that would serialize every concurrent status refresh behind it.
    // Two refreshes racing here both probe and both write the same answer.
    let starts = binary_starts(binary);
    if let Ok(mut cache) = STARTS.get_or_init(Default::default).lock() {
        cache.insert(key, starts);
    }
    starts
}

fn binary_starts(binary: &Path) -> bool {
    Command::new(binary)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[tauri::command]
pub fn generation_engine_status(app: AppHandle) -> Result<GenerationEngineStatus, String> {
    let app_data = app.profile_data_dir().ok();
    // Presence, not verification: this runs on every Studio refresh, and
    // hashing the whole runtime tree here made switching tabs take seconds.
    // The launch path still verifies before spawning anything.
    let target_supported = managed_runtime::runtime_supported_here(&STABLE_DIFFUSION);
    let present = target_supported
        .then(|| {
            managed_runtime::managed_server_path_unverified(&STABLE_DIFFUSION, app_data.as_deref())
        })
        .flatten();
    // An engine that is not there yet cannot be probed, and must not be
    // called unsupported for it: a fresh install has nothing under app data
    // until Studio first materializes the bundle.
    let supported = target_supported && present.as_deref().is_none_or(engine_starts);
    Ok(GenerationEngineStatus {
        supported,
        engine_installed: supported && present.is_some(),
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
            let temporary = root.join(format!(
                ".{}.{}.part",
                component.file_name(),
                Uuid::new_v4()
            ));
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

/// The user's LoRA library.
#[tauri::command]
pub fn generation_loras(app: AppHandle) -> Result<Vec<generation::LoraAsset>, String> {
    read_state(&app, LORAS_FILE)
}

/// Adds a LoRA to the library, keyed on its path so re-adding the same file
/// renames it rather than listing it twice.
#[tauri::command]
pub fn generation_add_lora(
    app: AppHandle,
    asset: generation::LoraAsset,
) -> Result<Vec<generation::LoraAsset>, String> {
    generation::validate_lora_asset(&asset)?;
    // Caught here rather than in the validator, which has no filesystem. A
    // LoRA that is not there fails several minutes into a load otherwise, with
    // an engine message that does not name it.
    if !PathBuf::from(&asset.path).is_file() {
        return Err(format!("{} is not a file on this machine", asset.path));
    }
    let mut loras: Vec<generation::LoraAsset> = read_state(&app, LORAS_FILE)?;
    match loras.iter_mut().find(|entry| entry.path == asset.path) {
        Some(existing) => *existing = asset,
        None => loras.push(asset),
    }
    loras.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    write_state(&app, LORAS_FILE, &loras)?;
    Ok(loras)
}

/// The user's loose parts: CLIPs, text encoders, VAEs.
#[tauri::command]
pub fn generation_parts(app: AppHandle) -> Result<Vec<generation::PartAsset>, String> {
    read_state(&app, PARTS_FILE)
}

/// Adds a part, keyed on its path so re-adding the same file corrects it
/// rather than listing it twice.
#[tauri::command]
pub fn generation_add_part(
    app: AppHandle,
    asset: generation::PartAsset,
) -> Result<Vec<generation::PartAsset>, String> {
    generation::validate_part_asset(&asset)?;
    if !PathBuf::from(&asset.path).is_file() {
        return Err(format!("{} is not a file on this machine", asset.path));
    }
    let mut parts: Vec<generation::PartAsset> = read_state(&app, PARTS_FILE)?;
    match parts.iter_mut().find(|entry| entry.path == asset.path) {
        Some(existing) => *existing = asset,
        None => parts.push(asset),
    }
    parts.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    write_state(&app, PARTS_FILE, &parts)?;
    Ok(parts)
}

/// Forgets a part. The file itself is the user's and stays where it is.
#[tauri::command]
pub fn generation_remove_part(
    app: AppHandle,
    path: String,
) -> Result<Vec<generation::PartAsset>, String> {
    let mut parts: Vec<generation::PartAsset> = read_state(&app, PARTS_FILE)?;
    parts.retain(|entry| entry.path != path);
    write_state(&app, PARTS_FILE, &parts)?;
    Ok(parts)
}

#[tauri::command]
pub fn generation_backends(app: AppHandle) -> Result<Vec<generation::RemoteBackend>, String> {
    read_state(&app, BACKENDS_FILE)
}

/// Registers a remote backend, keyed on its id so re-adding one edits it.
///
/// Nothing is contacted here. A backend that is switched off, still starting,
/// or on a laptop that is currently elsewhere is a normal thing to have saved,
/// so reachability is proven by the first generation rather than made a
/// precondition of writing the entry down.
#[tauri::command]
pub fn generation_add_backend(
    app: AppHandle,
    backend: generation::RemoteBackend,
) -> Result<Vec<generation::RemoteBackend>, String> {
    generation::validate_remote_backend(&backend)?;
    let mut backends: Vec<generation::RemoteBackend> = read_state(&app, BACKENDS_FILE)?;
    if backends.len() >= MAX_BACKENDS && !backends.iter().any(|entry| entry.id == backend.id) {
        return Err(format!("At most {MAX_BACKENDS} backends can be registered"));
    }
    match backends.iter_mut().find(|entry| entry.id == backend.id) {
        Some(existing) => *existing = backend,
        None => backends.push(backend),
    }
    backends.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    write_state(&app, BACKENDS_FILE, &backends)?;
    Ok(backends)
}

/// Forgets a backend. Whatever it pointed at keeps running; the app only ever
/// held its address.
#[tauri::command]
pub fn generation_remove_backend(
    app: AppHandle,
    backend_id: String,
) -> Result<Vec<generation::RemoteBackend>, String> {
    let mut backends: Vec<generation::RemoteBackend> = read_state(&app, BACKENDS_FILE)?;
    backends.retain(|entry| entry.id != backend_id);
    write_state(&app, BACKENDS_FILE, &backends)?;
    Ok(backends)
}

/// Forgets a LoRA. The file itself is the user's and stays where it is.
#[tauri::command]
pub fn generation_remove_lora(
    app: AppHandle,
    path: String,
) -> Result<Vec<generation::LoraAsset>, String> {
    let mut loras: Vec<generation::LoraAsset> = read_state(&app, LORAS_FILE)?;
    loras.retain(|entry| entry.path != path);
    write_state(&app, LORAS_FILE, &loras)?;
    Ok(loras)
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
    // K22: same refusal as the diffusion engine — speech is a second native
    // binary and gets the same gate.
    crate::self_integrity::ensure_loadable()?;
    let app_data = app
        .profile_data_dir()
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
///
/// Returns every artifact the run produced, newest-first. A batch of images is
/// one job and one set of settings, so it is one call — but it is several
/// gallery entries, because each image is separately keepable and separately
/// deletable.
#[tauri::command]
pub async fn generation_run(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    m3: tauri::State<'_, crate::m3_commands::M3CommandState>,
    request: GenerationRequest,
) -> Result<Vec<GenerationEntry>, String> {
    // A remote backend has no library entry to look up, no components to
    // override and no engine to launch, so it forks before all three rather
    // than threading an "is it local" flag through them.
    let (model_id, request, batch) =
        if let Some((backend_id, model)) = generation::parse_remote_model_id(&request.model_id) {
            let backend = find_backend(&app, backend_id)?;
            let validated = generation::validate_remote_request(&backend, &request)?;
            let media = run_remote(&app, &state, &backend, model, &validated).await?;
            (request.model_id.clone(), validated, vec![media])
        } else {
            let spec = find_registered(&app, &request.model_id)?;
            let validated = generation::validate_request(&spec, &request)?;
            // A swapped VAE or text encoder changes what the engine loads, so it
            // is resolved before anything launches. The engine keys reuse on its
            // launch arguments, so switching one mid-session relaunches on its own.
            let spec = generation::apply_component_overrides(
                &spec,
                &read_state::<Vec<generation::PartAsset>>(&app, PARTS_FILE)?,
                &validated.component_overrides,
            )?;
            // After the overrides, because a ControlNet or IP-Adapter can be
            // chosen for this run rather than belonging to the model entry.
            generation::validate_conditioning(&spec, &validated)?;
            let _mlx_owner = if spec.engine == GenerationEngineKind::MlxVideo {
                Some(m3.mlx_ownership.acquire().await)
            } else {
                None
            };
            let media = if validated.task.is_speech() {
                let _ = app.emit(
                    "studio://progress",
                    GenerationProgressEvent::new("speech", "running"),
                );
                vec![run_speech(&app, &spec, &validated).await?]
            } else {
                if spec.engine == GenerationEngineKind::MlxVideo {
                    crate::m3_commands::unload_mlx_for_studio_locked(&m3).await?;
                }
                run_diffusion(&app, &state, &spec, &validated).await?
            };
            (spec.id.clone(), validated, media)
        };

    let store = artifacts(&app)?;
    // One timestamp for the whole batch would make the gallery's sort — and so
    // the order the images appear in — arbitrary between siblings. Offsetting
    // by index keeps them in the order the engine sampled them.
    let created_at_ms = now_ms();
    let mut entries = Vec::with_capacity(batch.len());
    for (index, media) in batch.into_iter().enumerate() {
        let blob = store.put(&media.bytes).map_err(|error| error.to_string())?;
        entries.push(GenerationEntry {
            entry_id: format!("studio-{}", Uuid::new_v4()),
            artifact_id: blob.id,
            model_id: model_id.clone(),
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
            created_at_ms: created_at_ms + index as u64,
        });
    }

    let mut gallery: Vec<GenerationEntry> = read_state(&app, GALLERY_FILE)?;
    gallery.extend(entries.iter().cloned());
    gallery.sort_by_key(|item| item.created_at_ms);
    write_state(&app, GALLERY_FILE, &gallery)?;

    if let Some(last) = entries.last() {
        let _ = app.emit(
            "studio://progress",
            GenerationProgressEvent::new(&last.entry_id, "completed"),
        );
    }
    Ok(entries)
}

fn find_backend(app: &AppHandle, backend_id: &str) -> Result<generation::RemoteBackend, String> {
    read_state::<Vec<generation::RemoteBackend>>(app, BACKENDS_FILE)?
        .into_iter()
        .find(|entry| entry.id == backend_id)
        .ok_or_else(|| format!("No backend named '{backend_id}' is registered"))
}

/// In-flight remote jobs, so [`generation_cancel`] can reach one.
///
/// Local runs are cancelled through the engine's own job API, which needs no
/// bookkeeping here. A remote run has no such handle — cancelling it means
/// dropping the HTTP wait (and telling ComfyUI to interrupt), which only the
/// token that run is holding can do.
fn remote_jobs() -> &'static std::sync::Mutex<std::collections::HashMap<String, CancellationToken>>
{
    static JOBS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, CancellationToken>>> =
        OnceLock::new();
    JOBS.get_or_init(Default::default)
}

/// The remote half of [`generation_run`].
///
/// There is no queue to report and no sampling counter to scrape — a remote
/// backend exposes neither — so the UI gets "running" once and then the result.
async fn run_remote(
    app: &AppHandle,
    state: &tauri::State<'_, AppState>,
    backend: &generation::RemoteBackend,
    model: &str,
    request: &GenerationRequest,
) -> Result<generation::GeneratedMedia, String> {
    // The managed engine holds a model in VRAM for reuse. A remote run about to
    // compete with it for the same GPU — the common case, since ComfyUI usually
    // runs on this machine — should not have to. It reloads on the next local
    // run, which costs one warm-up rather than an out-of-memory failure.
    let _ = state.generation_engine.stop();

    let job_id = format!("remote-{}", Uuid::new_v4());
    let cancellation = CancellationToken::new();
    if let Ok(mut jobs) = remote_jobs().lock() {
        jobs.insert(job_id.clone(), cancellation.clone());
    }
    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new(&job_id, "running"),
    );

    let result =
        crate::generation_remote::run(backend, model, request, &job_id, &cancellation).await;

    if let Ok(mut jobs) = remote_jobs().lock() {
        jobs.remove(&job_id);
    }
    result
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
) -> Result<Vec<generation::GeneratedMedia>, String> {
    let root = model_root(app)?;
    let command = engine_command(app, spec)?;
    let engine = &state.generation_engine;

    // The engine only resolves a LoRA by its name inside `--lora-model-dir`,
    // so the absolute paths the request carries are linked in and replaced
    // with those names. Only a run that selected one pays for the clone.
    let staged;
    let request = if request.loras.is_empty() {
        request
    } else {
        let mut owned = request.clone();
        generation::stage_loras(&root, &mut owned)?;
        staged = owned;
        &staged
    };

    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new("", "loading"),
    );
    let base_url = engine.ensure_ready(&command, spec, &root).await?;
    engine.clear_progress();

    let result = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .map_err(|error| error.to_string())?;
        let job_id = generation::submit_job(&client, &base_url, spec, request).await?;
        let _ = app.emit(
            "studio://progress",
            GenerationProgressEvent::new(&job_id, "submitted"),
        );

        let mut moving = (engine.output_mark(), u32::MAX);
        let mut moved_at = tokio::time::Instant::now();
        loop {
            match generation::poll_job(&client, &base_url, &job_id).await? {
                JobProgress::Running { queue_position } => {
                    let mut event = GenerationProgressEvent::new(&job_id, "running")
                        .with_progress(engine.progress());
                    event.queue_position = queue_position;
                    let _ = app.emit("studio://progress", event);
                    let now = (engine.output_mark(), queue_position);
                    if now != moving {
                        moving = now;
                        moved_at = tokio::time::Instant::now();
                    } else if moved_at.elapsed() >= JOB_STALL_TIMEOUT {
                        generation::cancel_job(&client, &base_url, &job_id).await;
                        return Err(format!(
                            "Generation stopped making progress for {} minutes and was cancelled",
                            JOB_STALL_TIMEOUT.as_secs() / 60
                        ));
                    }
                }
                JobProgress::Completed(media) => return Ok(media),
                JobProgress::Failed(error) => return Err(error),
                JobProgress::Cancelled => return Err("Generation cancelled".to_string()),
            }
            tokio::time::sleep(Duration::from_millis(750)).await;
        }
    }
    .await;
    // Once ensure_ready succeeded, this run may have loaded tens of GB even
    // when submit, polling, cancellation, or generation itself failed. Every
    // such exit must arm the same idle cleanup used by successful runs.
    engine.schedule_idle_stop(STUDIO_ENGINE_IDLE_TIMEOUT);
    result
}

#[tauri::command]
pub async fn generation_cancel(
    state: tauri::State<'_, AppState>,
    job_id: String,
) -> Result<bool, String> {
    if job_id.is_empty() || job_id.len() > 128 {
        return Err("Invalid job id".to_string());
    }
    // A remote job is cancelled by dropping its own wait, not by calling an
    // engine that is not running it.
    if let Ok(jobs) = remote_jobs().lock() {
        if let Some(token) = jobs.get(&job_id) {
            token.cancel();
            return Ok(true);
        }
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

/// Removes one generation, bytes and all.
///
/// The artifact store is content addressed, so two runs that produced
/// identical bytes share one blob and deleting it for either would blank the
/// other. The blob therefore goes only once nothing else in the gallery points
/// at it.
///
/// ponytail: gallery-scoped refcount. The store is shared with the rest of the
/// app, so a blob referenced from outside Studio — which needs byte-identical
/// content, i.e. the same image saved somewhere else too — would still be
/// removed. A real refcount in `ArtifactStore` is the upgrade if another
/// feature ever starts deleting as well.
#[tauri::command]
pub fn generation_delete_entry(app: AppHandle, entry_id: String) -> Result<(), String> {
    let mut gallery: Vec<GenerationEntry> = read_state(&app, GALLERY_FILE)?;
    let Some(at) = gallery.iter().position(|entry| entry.entry_id == entry_id) else {
        return Err("That generation is not in the gallery".to_string());
    };
    let removed = gallery.remove(at);
    let still_referenced = gallery
        .iter()
        .any(|entry| entry.artifact_id == removed.artifact_id);
    // The index is written first: an entry with no bytes is a broken thumbnail,
    // but bytes with no entry are invisible and unreclaimable.
    write_state(&app, GALLERY_FILE, &gallery)?;
    if !still_referenced {
        artifacts(&app)?
            .delete(&removed.artifact_id)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// Base64 data URL for a gallery entry, used by both `<img>` and `<video>`.
/// The engine holds tens of gigabytes of weights, so callers preview from the
/// artifact store rather than regenerating.
#[tauri::command]
pub fn generation_media_data_url(app: AppHandle, artifact_id: String) -> Result<String, String> {
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

/// What the engine running right now says it supports.
///
/// `None` when nothing is running, which is the normal state before the first
/// generation: the pickers fall back to their compiled-in lists rather than
/// standing empty until a model is loaded.
#[tauri::command]
pub async fn generation_capabilities(
    state: tauri::State<'_, AppState>,
) -> Result<Option<generation::EngineCapabilities>, String> {
    // Launched on a fresh port each time, so the address comes from the running
    // instance rather than a constant — and only once that instance has
    // answered, because a short-deadline probe against an engine still loading
    // weights burns one of its worker threads and helps nobody.
    let Some(base_url) = state.generation_engine.ready_base_url() else {
        return Ok(None);
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    generation::fetch_capabilities(&client, &base_url)
        .await
        .map(Some)
}

/// Releases the engine's memory. The loaded weight set is tens of gigabytes,
/// so Studio never keeps it warm once the user is done.
#[tauri::command]
pub fn generation_unload_engine(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.generation_engine.stop()
}

// -------------------------------------------------------------------------
// Studio tools — the sidecar tier
// -------------------------------------------------------------------------

/// Tools the user has added. Like the model registry, there is no built-in
/// catalogue: this file is the whole library and it starts empty.
const TOOLS_FILE: &str = "studio-tools.json";
/// Nobody has a hundred tools; this only stops a scripted caller growing the
/// file without bound, as `MAX_BACKENDS` does for remote endpoints.
const MAX_TOOLS: usize = 64;

#[tauri::command]
pub fn studio_tools(app: AppHandle) -> Result<Vec<studio_tools::StudioTool>, String> {
    read_state(&app, TOOLS_FILE)
}

/// Adds a tool to the library, or replaces the entry with the same id.
///
/// Replacing rather than refusing is what makes an upgrade work: installing a
/// newer version of a managed tool through the component hub yields the same
/// `componentId` at a new artifact path, and the caller adds it again.
#[tauri::command]
pub fn studio_tool_add(
    app: AppHandle,
    tool: studio_tools::StudioTool,
) -> Result<Vec<studio_tools::StudioTool>, String> {
    studio_tools::validate_tool(&tool)?;
    // The pure validator deliberately does not touch the filesystem, so the
    // "you picked a folder" and "you picked something that is gone" cases are
    // caught here, where there is a real path to report.
    if !Path::new(&tool.path).is_file() {
        return Err(format!("There is no file at {}", tool.path));
    }
    let mut tools: Vec<studio_tools::StudioTool> = read_state(&app, TOOLS_FILE)?;
    if let Some(existing) = tools.iter_mut().find(|entry| entry.id == tool.id) {
        *existing = tool;
    } else {
        if tools.len() >= MAX_TOOLS {
            return Err(format!("At most {MAX_TOOLS} tools can be added"));
        }
        tools.push(tool);
    }
    write_state(&app, TOOLS_FILE, &tools)?;
    Ok(tools)
}

#[tauri::command]
pub fn studio_tool_remove(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    tool_id: String,
) -> Result<Vec<studio_tools::StudioTool>, String> {
    let mut tools: Vec<studio_tools::StudioTool> = read_state(&app, TOOLS_FILE)?;
    tools.retain(|tool| tool.id != tool_id);
    write_state(&app, TOOLS_FILE, &tools)?;
    // A removed tool must not keep running: its process outlives the library
    // entry otherwise, holding its model in memory with no way to reach it.
    state.studio_tool.stop(&tool_id)?;
    Ok(tools)
}

fn find_tool(app: &AppHandle, tool_id: &str) -> Result<studio_tools::StudioTool, String> {
    read_state::<Vec<studio_tools::StudioTool>>(app, TOOLS_FILE)?
        .into_iter()
        .find(|tool| tool.id == tool_id)
        .ok_or_else(|| format!("No tool named '{tool_id}' has been added"))
}

/// A client for the loopback tool sidecar. Two deadlines because the two calls
/// are nothing alike: the manifest is a page of JSON, and a run is the whole
/// operation plus its result.
fn tool_client(timeout: Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|error| error.to_string())
}

/// Starts a tool if it is not already running and returns what it declares.
///
/// This is what makes a tool's UI appear: Studio draws its form from the
/// manifest, so a tool ships its controls as data rather than as code.
#[tauri::command]
pub async fn studio_tool_manifest(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    tool_id: String,
) -> Result<studio_tools::ToolManifest, String> {
    let tool = find_tool(&app, &tool_id)?;
    let client = tool_client(Duration::from_secs(30))?;
    let (_, manifest) = state.studio_tool.ensure_ready(&tool, &client).await?;
    Ok(manifest)
}

/// Runs one tool operation and files its result in the gallery.
///
/// The inputs are checked against the running tool's own manifest rather than
/// against whatever the form last drew, so a tool that changed underneath the
/// UI rejects the stale field instead of being handed it.
#[tauri::command]
pub async fn studio_tool_run(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    tool_id: String,
    inputs: std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<Vec<GenerationEntry>, String> {
    let tool = find_tool(&app, &tool_id)?;
    let client = tool_client(Duration::from_secs(30))?;
    let (base_url, manifest) = state.studio_tool.ensure_ready(&tool, &client).await?;
    let body = studio_tools::validate_inputs(&manifest, &inputs)?;

    let _ = app.emit(
        "studio://progress",
        GenerationProgressEvent::new(&tool_id, "running"),
    );
    let run_client = tool_client(TOOL_RUN_TIMEOUT)?;
    let response = state.studio_tool.run(&base_url, &run_client, &body).await?;

    let store = artifacts(&app)?;
    let summary = studio_tools::run_summary(&manifest, &body);
    // A tool run has no sampler, no seed and no dimensions, so the gallery
    // entry carries zeros there rather than inventing values. The task records
    // whether an image went in, which is the one thing about the run the
    // gallery can meaningfully say.
    let task = if manifest.inputs.iter().any(|input| {
        input.kind == studio_tools::ToolInputKind::Image && body.contains_key(&input.key)
    }) {
        generation::GenerationTask::ImageToImage
    } else {
        generation::GenerationTask::TextToImage
    };
    let created_at_ms = now_ms();
    let mut entries = Vec::with_capacity(response.media.len());
    for (index, media) in response.media.iter().enumerate() {
        let bytes = studio_tools::decode_media(media)?;
        let blob = store.put(&bytes).map_err(|error| error.to_string())?;
        entries.push(GenerationEntry {
            entry_id: format!("studio-{}", Uuid::new_v4()),
            artifact_id: blob.id,
            model_id: format!("tool:{tool_id}"),
            task,
            prompt: summary.clone(),
            negative_prompt: String::new(),
            media_type: media.media_type.clone(),
            size_bytes: blob.size,
            width: 0,
            height: 0,
            steps: 0,
            cfg_scale: 0.0,
            seed: 0,
            frame_count: 1,
            fps: 1,
            duration_ms: 0,
            created_at_ms: created_at_ms + index as u64,
        });
    }

    let mut gallery: Vec<GenerationEntry> = read_state(&app, GALLERY_FILE)?;
    gallery.extend(entries.iter().cloned());
    gallery.sort_by_key(|item| item.created_at_ms);
    write_state(&app, GALLERY_FILE, &gallery)?;

    if let Some(last) = entries.last() {
        let _ = app.emit(
            "studio://progress",
            GenerationProgressEvent::new(&last.entry_id, "completed"),
        );
    }
    Ok(entries)
}

/// Imports a published tool catalog into the component registry.
///
/// The one-click Install beside each Available tool has always worked; what was
/// missing was any way to get entries in front of it, because the registry file
/// starts empty and there is no catalog server to poll. A catalog is a small
/// JSON array a publisher hands out, so importing one is the whole distribution
/// story short of running a CDN — and every entry still goes through the hub's
/// digest-checked download when the user actually installs it.
///
/// **Only `studio_tool` entries are taken.** The registry this writes into also
/// feeds llama.cpp, MLX and accelerator components, so a file titled "tool
/// catalog" must not be able to add or replace an inference runtime — that
/// would turn importing a tool list into repointing the engine. Entries of any
/// other kind are dropped rather than rejected, so one stray line does not
/// discard a catalog the user meant to import.
#[tauri::command]
pub fn studio_tool_import_catalog(
    m3: tauri::State<'_, crate::m3_commands::M3CommandState>,
    path: String,
) -> Result<Vec<crate::m3_runtime_hub::M3ComponentCatalogEntry>, String> {
    use crate::m3_runtime_hub::{M3ComponentCatalogEntry, M3ComponentKind};

    let metadata = std::fs::metadata(&path).map_err(|error| format!("{path}: {error}"))?;
    if !metadata.is_file() || metadata.len() > MAX_STATE_BYTES {
        return Err("A tool catalog must be a bounded JSON file".to_string());
    }
    let bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
    // Accepts either a bare array or `{ "entries": [...] }`, because both
    // spellings are what people actually publish.
    let imported: Vec<M3ComponentCatalogEntry> = serde_json::from_slice(&bytes)
        .or_else(|_| {
            serde_json::from_slice::<serde_json::Value>(&bytes).and_then(|value| {
                serde_json::from_value(value.get("entries").cloned().unwrap_or(value))
            })
        })
        .map_err(|error| format!("This is not a tool catalog this app can read: {error}"))?;

    let held = crate::m3_production::component_registry_entries(m3.component_hub.root())
        .map_err(|error| error.to_string())?;
    let merged = merge_tool_catalog(held, imported)?;
    let stored =
        crate::m3_production::replace_component_registry_entries(&m3.component_hub, merged)
            .map_err(|error| error.to_string())?;
    Ok(stored
        .into_iter()
        .filter(|entry| entry.kind == M3ComponentKind::StudioTool)
        .collect())
}

/// Folds an imported catalog into what the registry already holds.
///
/// Pure so the two rules that matter are testable without a hub: nothing but a
/// `studio_tool` survives the import, and existing entries are merged rather
/// than replaced.
fn merge_tool_catalog(
    mut held: Vec<crate::m3_runtime_hub::M3ComponentCatalogEntry>,
    imported: Vec<crate::m3_runtime_hub::M3ComponentCatalogEntry>,
) -> Result<Vec<crate::m3_runtime_hub::M3ComponentCatalogEntry>, String> {
    use crate::m3_runtime_hub::M3ComponentKind;

    let tools: Vec<_> = imported
        .into_iter()
        .filter(|entry| entry.kind == M3ComponentKind::StudioTool)
        .collect();
    if tools.is_empty() {
        return Err("That catalog contains no Studio tools".to_string());
    }
    // Keyed on id *and* version so a re-import updates in place rather than
    // duplicating every entry, and so two versions of one tool coexist.
    for entry in tools {
        match held
            .iter_mut()
            .find(|held| held.component_id == entry.component_id && held.version == entry.version)
        {
            Some(held) => *held = entry,
            None => held.push(entry),
        }
    }
    Ok(held)
}

/// Which tools are resident, so the UI can offer to release only those.
#[tauri::command]
pub fn studio_tools_running(state: tauri::State<'_, AppState>) -> Result<Vec<String>, String> {
    Ok(state.studio_tool.running_tools())
}

/// Releases tool memory, as `generation_unload_engine` does the engine's.
///
/// `tool_id` releases one; omitting it releases every resident tool, which is
/// what the user means by "release memory" when several are warm.
#[tauri::command]
pub fn studio_tool_stop(
    state: tauri::State<'_, AppState>,
    tool_id: Option<String>,
) -> Result<(), String> {
    match tool_id {
        Some(tool_id) => state.studio_tool.stop(&tool_id),
        None => state.studio_tool.stop_all(),
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "macos")]
    use super::MLX_VIDEO_SERVICE_ENTRY;
    use super::{binary_starts, merge_tool_catalog, Duration, Path, SystemTime, Uuid};
    use crate::m3_runtime_hub::{M3ComponentCatalogEntry, M3ComponentChannel, M3ComponentKind};

    fn entry(id: &str, version: &str, kind: M3ComponentKind) -> M3ComponentCatalogEntry {
        M3ComponentCatalogEntry {
            schema_version: 1,
            source_id: "local".to_string(),
            component_id: id.to_string(),
            kind,
            display_name: id.to_string(),
            accelerator: None,
            version: version.to_string(),
            channel: M3ComponentChannel::Stable,
            download_url: "https://example.com/tool.bin".to_string(),
            sha256: "a".repeat(64),
            size_bytes: 1024,
            published_at_ms: 1_700_000_000_000,
            compatibility_note: None,
            metadata: Default::default(),
        }
    }

    /// The registry this writes into also feeds llama.cpp, MLX and accelerator
    /// components. A file titled "tool catalog" that could add or replace one of
    /// those would turn importing a tool list into repointing the engine.
    #[test]
    fn importing_a_catalog_takes_only_studio_tools() {
        let held = vec![entry(
            "llama-cpp-server",
            "1.0.0",
            M3ComponentKind::LlamaCppServer,
        )];
        let imported = vec![
            entry("face-swap", "1.0.0", M3ComponentKind::StudioTool),
            // The smuggled one: same id as the installed runtime, so a blind
            // merge would overwrite where llama.cpp is fetched from.
            entry("llama-cpp-server", "1.0.0", M3ComponentKind::LlamaCppServer),
        ];
        let merged = merge_tool_catalog(held, imported).unwrap();
        assert_eq!(merged.len(), 2);
        assert_eq!(
            merged
                .iter()
                .filter(|held| held.kind == M3ComponentKind::LlamaCppServer)
                .count(),
            1,
            "the runtime entry must be the one already held"
        );
        assert!(merged.iter().any(|held| held.component_id == "face-swap"));
    }

    #[test]
    fn a_catalog_with_no_tools_is_refused_rather_than_silently_doing_nothing() {
        let imported = vec![entry("cuda", "1.0.0", M3ComponentKind::CudaSupport)];
        assert!(merge_tool_catalog(Vec::new(), imported)
            .unwrap_err()
            .contains("no Studio tools"));
    }

    /// Importing a second publisher's catalog must not drop the first's.
    #[test]
    fn importing_merges_rather_than_replaces_and_updates_in_place() {
        let held = vec![entry("face-swap", "1.0.0", M3ComponentKind::StudioTool)];
        let merged = merge_tool_catalog(
            held,
            vec![
                // Same id and version: an update, not a duplicate.
                entry("face-swap", "1.0.0", M3ComponentKind::StudioTool),
                // Same id, new version: both are keepable.
                entry("face-swap", "2.0.0", M3ComponentKind::StudioTool),
                entry("upscaler", "1.0.0", M3ComponentKind::StudioTool),
            ],
        )
        .unwrap();
        assert_eq!(merged.len(), 3);
        assert_eq!(
            merged
                .iter()
                .filter(|e| e.component_id == "face-swap")
                .count(),
            2
        );
    }

    /// The whole point of the probe: a file that is present but cannot be
    /// executed here reads as "does not start", which is what an old-glibc
    /// Linux host produces from a perfectly staged, perfectly verified tree.
    #[test]
    fn a_binary_that_cannot_be_executed_does_not_start() {
        let directory =
            std::env::temp_dir().join(format!("lm-sd-probe-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let fake = directory.join("sd-server");
        std::fs::write(&fake, b"not really a binary").unwrap();
        assert!(!binary_starts(&fake));
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// Restaging a broken engine has to be noticed without restarting the app.
    ///
    /// The probe is cached because it runs on every Studio status refresh, but
    /// a cache keyed on nothing answers for the file that *used* to be there:
    /// the user replaces a binary that would not exec, Studio keeps saying
    /// unsupported, and the fix appears to have done nothing.
    #[cfg(unix)]
    #[test]
    fn the_probe_notices_a_binary_that_changed_underneath_it() {
        use std::os::unix::fs::PermissionsExt;

        let directory =
            std::env::temp_dir().join(format!("lm-sd-cache-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("sd-server");

        std::fs::write(&path, b"not really a binary").unwrap();
        assert!(!super::engine_starts(&path), "a non-binary does not start");

        // The replacement a user staging a working runtime would leave behind.
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Set rather than slept for: two writes in the same tick of a coarse
        // filesystem clock would hand the new file the old one's cache key,
        // and a test that sleeps to avoid that is a test that flakes.
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000))
            .unwrap();

        assert!(
            super::engine_starts(&path),
            "the replaced binary is probed again rather than answered from the old verdict"
        );
        std::fs::remove_dir_all(&directory).unwrap();
    }

    /// The path Studio launches and the path the packaging script writes are
    /// two hand-maintained strings that must be the same one.
    ///
    /// Nothing else catches a drift between them: a package built with the
    /// service under another name still installs and still verifies — its
    /// manifest is over whatever files it contains — and the failure surfaces
    /// only as a video model that cannot start, on a machine that has already
    /// downloaded 300 MB.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_video_service_is_where_the_packaging_script_puts_it() {
        let script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/build-mlx-package.mjs"),
        )
        .unwrap();
        assert!(
            script.contains(&format!("\"{MLX_VIDEO_SERVICE_ENTRY}\"")),
            "build-mlx-package.mjs does not copy the service to {MLX_VIDEO_SERVICE_ENTRY}"
        );
    }
}

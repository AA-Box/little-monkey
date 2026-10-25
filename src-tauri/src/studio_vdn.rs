//! Standalone OpenVDN VDN-H3 generator (NOT FreeVideo/ComfyUI).
//!
//! A bounded, cancelable subprocess runs the published OpenVDN Diffusers
//! ModularPipeline through a Little Monkey-owned Python worker, then stores
//! the real MP4 in Studio's content-addressed Gallery. The license policy is
//! checked before any Hugging Face model code or weights can load, at runtime,
//! and before publication. Without a valid territorial grant it fails closed.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::sync::CancellationToken;
use crate::profiles::ProfileScopedPaths;

const MAX_PROMPT: usize = 8192;
const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_VIDEO_BYTES: u64 = 256 * 1024 * 1024;
const MAX_LOG_BYTES: u64 = 256 * 1024;
const MAX_DURATION: Duration = Duration::from_secs(4 * 60 * 60);
const WORKER: &str = include_str!("../../scripts/studio-vdn-h3-worker.py");
const SAMPLER_SOURCE: &str = include_str!("../../scripts/studio_vdn_sampler.py");
const TEMPORAL_SOURCE: &str = include_str!("../../scripts/studio_vdn_temporal.py");
const FAST_WORKER: &str = include_str!("../../scripts/studio-vdn-fast-worker.py");
const ADAPTIVE_WORKER: &str = include_str!("../../scripts/studio-vdn-freevideo-worker.py");

/// An independent real rendering runtime. Each shares licensing, cancellation
/// and gallery storage; none is implemented by relabeling the Diffusers mode.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VdnBackend {
    #[default]
    Diffusers,
    OptimizedSingle,
    OptimizedUlysses,
    Sglang,
    FreeVideoAdaptive,
}

fn worker_source(backend: VdnBackend) -> &'static str {
    match backend {
        VdnBackend::Diffusers => WORKER,
        VdnBackend::FreeVideoAdaptive => ADAPTIVE_WORKER,
        _ => FAST_WORKER,
    }
}

fn staged_worker_source(backend: VdnBackend) -> String {
    if backend == VdnBackend::Diffusers {
        // A self-contained, uniquely named worker script makes the sampler
        // callback part of the same Python process and works after a restart.
        format!("{SAMPLER_SOURCE}\n{TEMPORAL_SOURCE}\n{WORKER}")
    } else {
        worker_source(backend).to_owned()
    }
}

fn worker_args(
    backend: VdnBackend, repo_root: Option<&str>, checkpoint_root: Option<&str>,
    gpu_class: Option<&str>, server_port: Option<u16>,
) -> Result<Vec<String>, String> {
    if backend == VdnBackend::Diffusers { return Ok(Vec::new()); }
    if backend == VdnBackend::FreeVideoAdaptive {
        let cache = checkpoint_root.ok_or(
            "Select the already prepared FreeVideo model cache")?;
        let cache = Path::new(cache).canonicalize().map_err(|_|
            "Prepared FreeVideo model cache cannot be found")?;
        return Ok(vec!["--cache".into(), cache.to_string_lossy().into_owned()]);
    }
    let mut args = vec!["--backend".into(), match backend {
        VdnBackend::OptimizedSingle => "optimized_single",
        VdnBackend::OptimizedUlysses => "optimized_ulysses",
        VdnBackend::Sglang => "sglang",
        VdnBackend::Diffusers | VdnBackend::FreeVideoAdaptive => unreachable!(),
    }.into()];
    if backend == VdnBackend::Sglang {
        let port = server_port.unwrap_or(30010);
        if port == 0 { return Err("SGLang must have a nonzero loopback port".into()); }
        args.extend(["--port".into(), port.to_string()]);
    } else {
        if !cfg!(target_os = "linux") {
            return Err("Native tuned OpenVDN inference currently requires Linux CUDA; use Diffusers or Wan on this host".into());
        }
        let root = repo_root.ok_or("Select the original OpenVDN source checkout")?;
        let ckpt = checkpoint_root.ok_or("Select the official OpenVDN weights directory")?;
        let root = Path::new(root).canonicalize()
            .map_err(|_| "OpenVDN source checkout does not exist")?;
        let ckpt = Path::new(ckpt).canonicalize()
            .map_err(|_| "OpenVDN checkpoint directory does not exist")?;
        if !root.is_dir() || !ckpt.is_dir() {
            return Err("OpenVDN source and checkpoint roots must both exist".into());
        }
        args.extend(["--repo-root".into(), root.to_string_lossy().into_owned(),
                     "--checkpoint-root".into(), ckpt.to_string_lossy().into_owned()]);
        if backend == VdnBackend::OptimizedUlysses {
            let gpu = gpu_class.unwrap_or("h200");
            if gpu != "h200" && gpu != "b200" {
                return Err("Native Ulysses has only H200 and B200 validated profiles".into());
            }
            args.extend(["--gpus".into(), "8".into(), "--gpu-class".into(), gpu.into()]);
        }
    }
    Ok(args)
}


fn default_native_memory_mode() -> String { "auto".into() }
fn default_native_refine_steps() -> u32 { 3 }
fn default_native_segments() -> u32 { 1 }
fn default_continuation_mode() -> String { "last_frame".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VdnRunRequest {
    pub job_id: String,
    pub python_executable: String,
    pub prompt: String,
    pub steps: u32,
    pub frames: u32,
    #[serde(default = "default_native_segments")]
    pub segments: u32,
    pub seed: i64,
    pub first_image_path: Option<String>,
    pub last_image_path: Option<String>,
    #[serde(default)]
    pub source_gallery_entry_id: Option<String>,
    #[serde(default)]
    pub source_vdn_job_id: Option<String>,
    #[serde(default = "default_continuation_mode")]
    pub continuation_mode: String,
    #[serde(default)]
    pub reference_image_paths: Vec<String>,
    #[serde(default)]
    pub reference_video_path: Option<String>,
    #[serde(default)]
    pub lora_paths: Vec<String>,
    #[serde(default)]
    pub reference_audio_path: Option<String>,
    pub fp8: bool,
    pub offload_dit: bool,
    #[serde(default = "default_native_memory_mode")]
    pub memory_mode: String,
    #[serde(default)]
    pub native_two_pass: bool,
    #[serde(default = "default_native_refine_steps")]
    pub refine_steps: u32,
    #[serde(default)]
    pub refine_from_job_id: Option<String>,
    #[serde(default)]
    pub backend: VdnBackend,
    #[serde(default)]
    pub repo_root: Option<String>,
    #[serde(default)]
    pub checkpoint_root: Option<String>,
    #[serde(default)]
    pub gpu_class: Option<String>,
    #[serde(default)]
    pub server_port: Option<u16>,
    /// The official modular VDN component executes code downloaded from HF.
    /// Do not do that without explicit per-invocation user confirmation.
    pub acknowledge_third_party_code: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct VdnProgress<'a> {
    job_id: &'a str,
    phase: &'a str,
    detail: &'a str,
}

fn running() -> &'static Mutex<HashMap<String, CancellationToken>> {
    static RUNNING: OnceLock<Mutex<HashMap<String, CancellationToken>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashMap::new()))
}

struct RegisteredJob(String);
impl Drop for RegisteredJob {
    fn drop(&mut self) {
        if let Ok(mut active) = running().lock() {
            active.remove(&self.0);
        }
    }
}

fn register(id: &str) -> Result<(CancellationToken, RegisteredJob), String> {
    let mut jobs = running().lock().map_err(|error| error.to_string())?;
    if !jobs.is_empty() {
        return Err("Another VDN-H3 generation is already running on this computer".into());
    }
    let cancellation = CancellationToken::new();
    jobs.insert(id.to_string(), cancellation.clone());
    Ok((cancellation, RegisteredJob(id.into())))
}

fn validate_image(path: &Option<String>) -> Result<Option<PathBuf>, String> {
    let Some(path) = path.as_deref() else { return Ok(None); };
    let resolved = Path::new(path).canonicalize()
        .map_err(|_| "Keyframe does not exist or cannot be read")?;
    let info = std::fs::metadata(&resolved).map_err(|e| e.to_string())?;
    if !info.is_file() || info.len() < 8 || info.len() > MAX_IMAGE_BYTES {
        return Err("A first/last-frame image must be a readable PNG/JPEG of at most 32 MB".into());
    }
    let mut header = [0u8; 8];
    use std::io::Read as _;
    std::fs::File::open(&resolved).map_err(|e| e.to_string())?
        .read_exact(&mut header).map_err(|e| e.to_string())?;
    let png = header == [137,80,78,71,13,10,26,10];
    let jpeg = header[0..3] == [255,216,255];
    if !png && !jpeg {
        return Err("VDN first/last frame must contain PNG or JPEG bytes".into());
    }
    Ok(Some(resolved))
}

fn validate_request(request: &VdnRunRequest) -> Result<(PathBuf, Option<PathBuf>, Option<PathBuf>), String> {
    uuid::Uuid::parse_str(&request.job_id)
        .map_err(|_| "VDN job ID must be a UUID")?;
    if request.prompt.trim().is_empty() || request.prompt.len() > MAX_PROMPT
        || request.prompt.chars().any(|c| c == '\0') {
        return Err("A nonempty VDN prompt of at most 8192 bytes is required".into());
    }
    if !request.acknowledge_third_party_code {
        return Err("Confirm execution of the official upstream OpenVDN model component code".into());
    }
    if !(1..=50).contains(&request.steps)
        || !(120..=360).contains(&request.frames)
        || (request.frames - 5) % 17 != 0 {
        return Err("VDN requires 1-50 inference steps and 17n+5 frames in 120..360".into());
    }
    if !(1..=8).contains(&request.segments) {
        return Err("Native VDN supports 1..8 temporal segments".into());
    }
    if request.segments != 1 && request.backend != VdnBackend::Diffusers {
        return Err("Native temporal continuity requires the Little Monkey Diffusers backend".into());
    }
    if request.segments > 1 && request.native_two_pass {
        return Err("Multi-segment native continuation cannot reuse a single two-pass checkpoint".into());
    }
    if request.segments > 1 && request.last_image_path.is_some() {
        return Err("Multi-segment native continuation cannot use a fixed terminal keyframe".into());
    }
    if !matches!(request.backend, VdnBackend::Diffusers | VdnBackend::FreeVideoAdaptive) && request.steps != 8 {
        return Err("Optimized OpenVDN 8-step checkpoint must use exactly eight model evaluations".into());
    }
    if !matches!(request.memory_mode.as_str(), "auto" | "resident" | "stream_cpu" | "stream_disk") {
        return Err("Invalid Little Monkey native memory policy".into());
    }
    if !matches!(request.continuation_mode.as_str(), "last_frame" | "video_reference") {
        return Err("Unsupported native continuation mode".into());
    }
    if let Some(source_id) = &request.source_vdn_job_id {
        uuid::Uuid::parse_str(source_id).map_err(|_| "Invalid VDN predecessor job ID")?;
        if source_id == &request.job_id { return Err("VDN job cannot depend on itself".into()); }
        if request.source_gallery_entry_id.is_some() {
            return Err("Choose a Gallery source or predecessor job, not both".into());
        }
        if request.first_image_path.is_some() {
            return Err("VDN predecessor supplies its own first frame".into());
        }
    }
    if request.continuation_mode == "video_reference" {
        let linked = request.source_gallery_entry_id.is_some()
            || request.source_vdn_job_id.is_some();
        if !linked && request.reference_video_path.is_none() {
            return Err("Video reference continuation needs a Gallery clip, predecessor job or resolved local video".into());
        }
        if request.backend != VdnBackend::Diffusers {
            return Err("Motion/audio reference continuation requires native Diffusers Ref2VA".into());
        }
        if (linked && request.reference_video_path.is_some())
            || request.first_image_path.is_some() || request.last_image_path.is_some() {
            return Err("Cannot mix pending video continuation with another video/keyframe".into());
        }
    }
    if request.native_two_pass && request.backend != VdnBackend::Diffusers {
        return Err("Little Monkey native latent refinement currently requires Diffusers".into());
    }
    if request.refine_from_job_id.is_some() && !request.native_two_pass {
        return Err("Refine-from requires native two-pass".into());
    }
    if !(1..=12).contains(&request.refine_steps) {
        return Err("Native refinement requires 1..12 steps".into());
    }
    if let Some(prior) = &request.refine_from_job_id {
        uuid::Uuid::parse_str(prior).map_err(|_| "Invalid native preview job ID")?;
        if prior == &request.job_id { return Err("Cannot refine from own job ID".into()); }
    }
    if request.seed < 0 {
        return Err("VDN seed must be nonnegative".into());
    }
    let python = Path::new(&request.python_executable).canonicalize()
        .map_err(|_| "Choose the Python executable from the OpenVDN-compatible environment")?;
    if !python.is_file() {
        return Err("Python executable is not a file".into());
    }
    if request.lora_paths.len() > 4 { return Err("VDN supports at most four LoRAs".into()); }
    if !request.lora_paths.is_empty() &&
        !matches!(request.backend, VdnBackend::Diffusers | VdnBackend::OptimizedSingle | VdnBackend::OptimizedUlysses | VdnBackend::FreeVideoAdaptive) {
        return Err("External LoRA requires native optimized OpenVDN".into());
    }
    for lora in &request.lora_paths {
        let path = Path::new(lora).canonicalize().map_err(|_| "LoRA file missing")?;
        let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() < 128 || metadata.len() > 8 * 1024 * 1024 * 1024 ||
            path.extension().and_then(|x| x.to_str()) != Some("safetensors") {
            return Err("VDN LoRA must be a local .safetensors file up to 8 GiB".into());
        }
    }
    if let Some(audio) = &request.reference_audio_path {
        if !matches!(request.backend, VdnBackend::Diffusers | VdnBackend::FreeVideoAdaptive) {
            return Err("Audio-conditioned references require the native Diffusers Ref2VA backend".into());
        }
        let path = Path::new(audio).canonicalize().map_err(|_| "Reference audio not found")?;
        let info = std::fs::metadata(&path).map_err(|e| e.to_string())?;
        if !info.is_file() || info.len() < 32 || info.len() > 64 * 1024 * 1024 {
            return Err("Audio reference must be a local file of at most 64 MiB".into());
        }
    }
    if request.reference_video_path.is_some() &&
        !matches!(request.backend, VdnBackend::Diffusers | VdnBackend::OptimizedSingle | VdnBackend::OptimizedUlysses) {
        return Err("Video references require native OpenVDN or Diffusers".into());
    }
    if let Some(path) = &request.reference_video_path {
        let video = Path::new(path).canonicalize().map_err(|_| "Reference video not found")?;
        let metadata = std::fs::metadata(&video).map_err(|e| e.to_string())?;
        if !metadata.is_file() || metadata.len() > MAX_VIDEO_BYTES || metadata.len() < 32 {
            return Err("Reference video must be a local file of 32 bytes to 256 MB".into());
        }
    }
    if request.reference_image_paths.len() > 8 {
        return Err("VDN supports at most eight reference images".into());
    }
    if !request.reference_image_paths.is_empty() &&
        !matches!(request.backend, VdnBackend::Diffusers | VdnBackend::OptimizedSingle | VdnBackend::OptimizedUlysses) {
        return Err("Ref2VA requires native OpenVDN or Diffusers".into());
    }
    for reference in &request.reference_image_paths {
        validate_image(&Some(reference.clone()))?;
    }
    if request.backend == VdnBackend::Diffusers
        && (request.reference_audio_path.is_some()
            || request.reference_video_path.is_some()
            || !request.reference_image_paths.is_empty())
        && (request.first_image_path.is_some() || request.last_image_path.is_some()) {
        return Err("Ref2VA reference conditioning cannot be combined with keyframes".into());
    }
    if request.backend == VdnBackend::Diffusers
        && request.reference_audio_path.is_some()
        && request.reference_video_path.is_none() && request.reference_image_paths.is_empty()
        && !(request.continuation_mode == "video_reference"
            && (request.source_gallery_entry_id.is_some()
                || request.source_vdn_job_id.is_some())) {
        return Err("Native Ref2VA audio reference requires image, video or a pinned video continuation".into());
    }
    if request.source_gallery_entry_id.is_some() && request.first_image_path.is_some() {
        return Err("Cannot combine continuation with first image".into());
    }
    let first = validate_image(&request.first_image_path)?;
    let last = validate_image(&request.last_image_path)?;
    Ok((python, first, last))
}

pub(crate) fn validate_for_durable_submit(request: &VdnRunRequest) -> Result<(), String> {
    validate_request(request).map(|_| ())
}

fn valid_mp4(bytes: &[u8]) -> bool {
    bytes.len() >= 32 && bytes.get(4..8) == Some(b"ftyp".as_slice())
}

fn read_last_phase(log: &Path) -> Option<(String, String)> {
    let bytes = std::fs::read(log).ok()?;
    if bytes.len() as u64 > MAX_LOG_BYTES {
        return None;
    }
    let s = String::from_utf8(bytes).ok()?;
    s.lines().rev().find_map(|line| {
        let row: serde_json::Value = serde_json::from_str(line).ok()?;
        let phase = row.get("phase")?.as_str()?.to_owned();
        let detail = match (
            row.get("stage").and_then(|v| v.as_str()),
            row.get("completedSteps").and_then(|v| v.as_u64()),
            row.get("totalSteps").and_then(|v| v.as_u64()),
        ) {
            (Some(stage), Some(done), Some(total)) if done <= total && total <= 100 => {
                format!("{stage}: completed {done}/{total} sampler steps")
            }
            _ => String::new(),
        };
        Some((phase, detail))
    })
}

fn brief_error(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let compact = text.lines().rev().find(|line| line.contains("reason"))
        .unwrap_or_else(|| text.lines().last().unwrap_or("VDN worker failed"));
    compact.chars().take(400).collect()
}

/// Terminate worker children (torchrun process groups) before killing the wrapper.
async fn stop_vdn_worker(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        // Python worker forwards SIGTERM to its CUDA/torchrun process group.
        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
        if tokio::time::timeout(Duration::from_secs(12), child.wait()).await.is_ok() {
            return;
        }
    }
    let _ = child.kill().await;
}

#[tauri::command]
pub async fn generation_vdn_run(
    app: AppHandle,
    request: VdnRunRequest,
) -> Result<crate::generation_commands::GenerationEntry, String> {
    // The check precedes ALL subprocess execution and Hugging Face downloads.
    crate::generation_commands::ensure_vdn_usage(&app)?;
    let (python, first, last) = validate_request(&request)?;
    let fast_args = worker_args(
        request.backend, request.repo_root.as_deref(),
        request.checkpoint_root.as_deref(),
        request.gpu_class.as_deref(), request.server_port,
    )?;
    let _video_engine_guard = crate::studio_video_jobs::engine_gate().lock().await;
    if crate::studio_vdn_jobs::is_cancelled(&app, &request.job_id)? {
        return Err("VDN job cancelled before GPU allocation".into());
    }
    if request.source_gallery_entry_id.is_some()
        || request.source_vdn_job_id.is_some() {
        return Err("Native VDN continuation requires a resolved durable job; direct rendering cannot bypass source handoff".into());
    }
    crate::generation_commands::ensure_vdn_usage(&app)?;
    let (cancel, _registration) = register(&request.job_id)?;
    let directory = app.profile_data_dir().map_err(|error| error.to_string())?
        .join("studio-vdn-h3");
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    // A unique staged worker path avoids Windows rename-over-existing-file
    // errors and cross-job mutation of a script already executing.
    let source = directory.join(format!("studio-vdn-h3-worker-{}.py", request.job_id));
    let staging = directory.join(format!(".worker-{}.tmp", request.job_id));
    std::fs::write(&staging, staged_worker_source(request.backend))
        .map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
    }
    std::fs::rename(&staging, &source).map_err(|error| error.to_string())?;
    let output = directory.join(format!("{}.mp4", request.job_id));
    let stdout_path = directory.join(format!("{}.stdout", request.job_id));
    let stderr_path = directory.join(format!("{}.stderr", request.job_id));
    // Clear leftovers from an interrupted run, but never touch other jobs.
    for path in [&output, &stdout_path, &stderr_path] {
        let _ = std::fs::remove_file(path);
    }
    let stdout = std::fs::File::create(&stdout_path).map_err(|e| e.to_string())?;
    let stderr = std::fs::File::create(&stderr_path).map_err(|e| e.to_string())?;
    let mut process = tokio::process::Command::new(python);
    process.arg("-u").arg(&source)
        .arg("--prompt").arg(&request.prompt)
        .arg("--output").arg(&output)
        .arg("--steps").arg(request.steps.to_string())
        .arg("--frames").arg(request.frames.to_string())
        .arg("--segments").arg(request.segments.to_string())
        .arg("--seed").arg(request.seed.to_string())
        .arg("--device").arg("cuda")
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .kill_on_drop(true);
    if request.backend == VdnBackend::Diffusers {
        process.arg("--memory-mode").arg(&request.memory_mode);
        let checkpoint_home = directory.join("step-checkpoints");
        std::fs::create_dir_all(&checkpoint_home).map_err(|e| e.to_string())?;
        let root_metadata = std::fs::symlink_metadata(&checkpoint_home)
            .map_err(|e| e.to_string())?;
        if root_metadata.file_type().is_symlink() || !root_metadata.is_dir() {
            return Err("Unsafe native sampler checkpoint root".into());
        }
        let job_checkpoints = checkpoint_home.join(&request.job_id);
        std::fs::create_dir_all(&job_checkpoints).map_err(|e| e.to_string())?;
        let job_metadata = std::fs::symlink_metadata(&job_checkpoints)
            .map_err(|e| e.to_string())?;
        if job_metadata.file_type().is_symlink() || !job_metadata.is_dir() {
            return Err("Unsafe job-specific sampler checkpoint directory".into());
        }
        process.arg("--step-checkpoint-root").arg(job_checkpoints);
        if request.native_two_pass {
            let checkpoints = directory.join("latent-checkpoints");
            std::fs::create_dir_all(&checkpoints).map_err(|e| e.to_string())?;
            process.arg("--two-pass").arg("--refine-steps").arg(request.refine_steps.to_string());
            process.arg("--checkpoint").arg(checkpoints.join(format!("{}.safetensors", request.job_id)));
            if let Some(prior) = &request.refine_from_job_id {
                if !crate::studio_vdn_jobs::completed_with_preview(&app, prior)? {
                    return Err("Refine-from requires a completed native two-pass source job".into());
                }
                process.arg("--refine-from").arg(checkpoints.join(format!("{prior}.safetensors")));
            }
        }
    }
    if request.backend == VdnBackend::Diffusers && request.offload_dit {
        process.arg("--offload-dit");
    }
    process.args(&fast_args);
    if request.fp8 { process.arg("--fp8"); }
    if let Some(ref image) = first { process.arg("--first").arg(image); }
    if let Some(ref image) = last { process.arg("--last").arg(image); }
    for lora in &request.lora_paths { process.arg("--lora").arg(lora); }
    if let Some(audio) = &request.reference_audio_path {
        process.arg("--reference-audio").arg(audio);
    }
    if let Some(reference_video) = &request.reference_video_path {
        process.arg("--reference-video").arg(reference_video);
    }
    if !request.reference_image_paths.is_empty() {
        process.arg("--refs");
        for reference in &request.reference_image_paths { process.arg(reference); }
    }
    let mut child = process.spawn().map_err(|error|
        format!("Could not start the installed Python VDN environment: {error}"))?;
    let _ = app.emit("studio://vdn-progress", VdnProgress {
        job_id: &request.job_id, phase: "running", detail: "OpenVDN model loading",
    });

    enum Outcome { Completed(std::process::ExitStatus), Cancelled, TimedOut, RightsRevoked(String) }
    let mut health = tokio::time::interval(Duration::from_secs(3));
    let expiry = tokio::time::sleep(MAX_DURATION);
    tokio::pin!(expiry);
    let outcome = loop {
        let event = tokio::select! {
            finished = child.wait() => Outcome::Completed(
                finished.map_err(|error| format!("VDN process wait failed: {error}"))?
            ),
            _ = cancel.cancelled() => Outcome::Cancelled,
            _ = &mut expiry => Outcome::TimedOut,
            _ = health.tick() => {
                match crate::generation_commands::ensure_vdn_usage(&app) {
                    Ok(()) => {
                        if let Some((phase, detail)) = read_last_phase(&stdout_path) {
                            let _ = app.emit("studio://vdn-progress", VdnProgress {
                                job_id: &request.job_id, phase: &phase, detail: &detail,
                            });
                        }
                        continue;
                    },
                    Err(error) => Outcome::RightsRevoked(error),
                }
            },
        };
        break event;
    };
    let result = match outcome {
        Outcome::Completed(status) if status.success() => (|| -> Result<
            crate::generation_commands::GenerationEntry, String
        > {
            // Recheck immediately before publication; region and entitlement
            // may have changed during a long-running GPU operation.
            crate::generation_commands::ensure_vdn_usage(&app)?;
            let info = std::fs::metadata(&output)
                .map_err(|_| "OpenVDN did not produce an MP4".to_string())?;
            if info.len() < 32 || info.len() > MAX_VIDEO_BYTES {
                Err("OpenVDN output must be a nonempty MP4 smaller than 256 MB".into())
            } else {
                let bytes = std::fs::read(&output).map_err(|error| error.to_string())?;
                if !valid_mp4(&bytes) {
                    Err("VDN worker output is not a valid MP4 header".into())
                } else {
                    crate::generation_commands::publish_vdn_video(
                        &app, &request.job_id,
                        match request.backend {
                            VdnBackend::Diffusers
                                if !request.reference_image_paths.is_empty()
                                   || request.reference_video_path.is_some()
                                   || request.reference_audio_path.is_some() =>
                                "minimax-h3-ref2va-diffusers",
                            VdnBackend::Diffusers => "vdn-h3-diffusers",
                            VdnBackend::OptimizedSingle => "vdn-h3-optimized",
                            VdnBackend::OptimizedUlysses => "vdn-h3-ulysses",
                            VdnBackend::Sglang => "vdn-h3-sglang",
                            VdnBackend::FreeVideoAdaptive => "vdn-h3-freevideo-adaptive",
                        },
                        &request.prompt, request.steps, request.seed,
                        request.frames, first.is_some() || !request.reference_image_paths.is_empty()
                            || request.reference_video_path.is_some(), &bytes
                    )
                }
            }
        })(),
        Outcome::Completed(_status) => Err(format!(
            "VDN worker failed: {}", brief_error(&stderr_path)
        )),
        Outcome::Cancelled => {
            stop_vdn_worker(&mut child).await;
            Err("VDN generation cancelled".into())
        },
        Outcome::TimedOut => {
            stop_vdn_worker(&mut child).await;
            Err("VDN inference exceeded the four-hour limit".into())
        },
        Outcome::RightsRevoked(reason) => {
            stop_vdn_worker(&mut child).await;
            Err(format!("VDN license no longer permits inference: {reason}"))
        },
    };
    for path in [&stdout_path, &stderr_path, &output, &source] {
        let _ = std::fs::remove_file(path);
    }
    let _ = app.emit("studio://vdn-progress", VdnProgress {
        job_id: &request.job_id,
        phase: if result.is_ok() { "completed" } else { "failed" },
        detail: "",
    });
    result
}

/// Real preflight of the chosen Python/CUDA environment, without downloading
/// the restricted weights or invoking any Hugging Face remote model code.
#[tauri::command(rename_all = "camelCase")]
pub async fn generation_vdn_probe(
    app: AppHandle,
    python_executable: String,
    backend: VdnBackend,
    repo_root: Option<String>,
    checkpoint_root: Option<String>,
    gpu_class: Option<String>,
    server_port: Option<u16>,
) -> Result<serde_json::Value, String> {
    let python = Path::new(&python_executable).canonicalize()
        .map_err(|_| "Choose a real Python executable in the OpenVDN environment")?;
    if !python.is_file() {
        return Err("VDN Python executable is not a file".into());
    }
    let extra = worker_args(backend, repo_root.as_deref(),
        checkpoint_root.as_deref(), gpu_class.as_deref(), server_port)?;
    let dir = app.profile_data_dir().map_err(|error| error.to_string())?
        .join("studio-vdn-h3");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let script = dir.join(format!(".vdn-probe-{}.py", uuid::Uuid::new_v4()));
    std::fs::write(&script, staged_worker_source(backend)).map_err(|error| error.to_string())?;
    let outcome = async {
        let mut cmd = tokio::process::Command::new(python);
        cmd.arg("-u").arg(&script).arg("--probe").args(&extra)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(90), cmd.output())
            .await.map_err(|_| "VDN environment probe timed out".to_string())?
            .map_err(|error| format!("Cannot probe the Python environment: {error}"))?;
        if !output.status.success() {
            let details = String::from_utf8_lossy(&output.stderr);
            let last = details.lines().last().unwrap_or("The OpenVDN runtime was not available");
            return Err(last.chars().take(350).collect());
        }
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .map_err(|_| "VDN environment returned no valid capability report".to_string())
    }.await;
    let _ = std::fs::remove_file(&script);
    outcome
}

#[tauri::command]
pub fn generation_vdn_cancel(job_id: String) -> Result<bool, String> {
    let jobs = running().lock().map_err(|error| error.to_string())?;
    let Some(token) = jobs.get(&job_id) else { return Ok(false); };
    token.cancel();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_engine_selector_never_falls_through_to_unlicensed_diffusers() {
        assert_eq!(worker_source(VdnBackend::Diffusers), WORKER);
        assert_eq!(worker_source(VdnBackend::OptimizedSingle), FAST_WORKER);
        assert_eq!(worker_source(VdnBackend::OptimizedUlysses), FAST_WORKER);
        assert_eq!(worker_source(VdnBackend::Sglang), FAST_WORKER);
        assert_eq!(worker_source(VdnBackend::FreeVideoAdaptive), ADAPTIVE_WORKER);
        let sglang = worker_args(VdnBackend::Sglang, None, None, None, Some(30010)).unwrap();
        assert!(sglang.windows(2).any(|pair| pair == ["--backend", "sglang"]));
        assert!(sglang.windows(2).any(|pair| pair == ["--port", "30010"]));
        assert!(worker_args(VdnBackend::Sglang, None, None, None, Some(0)).is_err());
        assert!(worker_args(VdnBackend::OptimizedSingle, None, None, None, None).is_err());
        assert!(worker_args(VdnBackend::OptimizedUlysses, None, None, Some("invalid"), None).is_err());
    }

    #[test]
    fn vdn_backend_serde_rejects_unknown_instead_of_running_a_default_engine() {
        let parsed: VdnBackend = serde_json::from_str("\"optimized_ulysses\"").unwrap();
        assert_eq!(parsed, VdnBackend::OptimizedUlysses);
        assert!(serde_json::from_str::<VdnBackend>("\"arbitrary_remote\"").is_err());
    }

    #[test]
    fn only_actual_mp4_brand_is_published() {
        assert!(!valid_mp4(b"not a video"));
        let mut video = vec![0u8; 32];
        video[4..8].copy_from_slice(b"ftyp");
        assert!(valid_mp4(&video));
        video[4..8].copy_from_slice(b"free");
        assert!(!valid_mp4(&video));
    }

    #[test]
    fn worker_declares_original_vdn_hybrid_pipeline_not_comfyui() {
        assert!(WORKER.contains("ModularPipeline.from_pretrained"));
        assert!(WORKER.contains("OpenVDN/vdn-minimax-h3"));
        assert!(WORKER.contains("num_inference_steps=args.steps + 1"));
        assert!(WORKER.contains("apply_group_offloading"));
        assert!(SAMPLER_SOURCE.contains("native_sampler_recovery"));
        assert!(staged_worker_source(VdnBackend::Diffusers).contains("NativeStepStore"));
        assert!(staged_worker_source(VdnBackend::Diffusers).contains("NativeTemporalPrefix"));
        assert!(!WORKER.contains("import comfyui"));
    }

    #[test]
    fn diffusers_native_ref2va_images_and_audio_validate_before_launch() {
        let dir = std::env::temp_dir().join(format!("vdn-ref2va-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let image = dir.join("portrait.png");
        let audio = dir.join("voice.wav");
        std::fs::write(&image, [137, 80, 78, 71, 13, 10, 26, 10]).unwrap();
        std::fs::write(&audio, vec![1u8; 64]).unwrap();
        let python = std::env::current_exe().unwrap().to_string_lossy().into_owned();
        let request: VdnRunRequest = serde_json::from_value(serde_json::json!({
            "jobId": uuid::Uuid::new_v4().to_string(),
            "pythonExecutable": python, "prompt": "A subject moves gently",
            "steps": 8, "frames": 141, "seed": 31,
            "firstImagePath": null, "lastImagePath": null,
            "referenceImagePaths": [image.to_string_lossy().into_owned()],
            "referenceAudioPath": audio.to_string_lossy().into_owned(),
            "fp8": false, "offloadDit": true, "backend": "diffusers",
            "acknowledgeThirdPartyCode": true
        })).unwrap();
        assert!(validate_request(&request).is_ok(), "Native Ref2VA should reach the worker");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolved_native_motion_reference_does_not_need_gone_gallery_pointer() {
        let dir = std::env::temp_dir().join(format!("vdn-motion-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let clip = dir.join("prior.mp4");
        std::fs::write(&clip, vec![1u8; 128]).unwrap();
        let mut request: VdnRunRequest = serde_json::from_value(serde_json::json!({
            "jobId": uuid::Uuid::new_v4().to_string(),
            "pythonExecutable": std::env::current_exe().unwrap().to_string_lossy().into_owned(),
            "prompt": "Continue the motion", "steps": 8, "frames": 141, "seed": 31,
            "firstImagePath": null, "lastImagePath": null,
            "continuationMode": "video_reference",
            "referenceVideoPath": clip.to_string_lossy().into_owned(),
            "fp8": false, "offloadDit": true, "backend": "diffusers",
            "acknowledgeThirdPartyCode": true
        })).unwrap();
        assert!(validate_request(&request).is_ok(), "Resolved artifact must reach Ref2VA worker");
        request.reference_video_path = None;
        assert!(validate_request(&request).is_err(), "Unresolved source must fail closed");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn malformed_requests_are_rejected_before_launch() {
        let invalid = VdnRunRequest {
            job_id: uuid::Uuid::new_v4().to_string(),
            python_executable: "missing-python".into(),
            prompt: "sample".into(),
            steps: 8,
            frames: 345,
            segments: 1,
            seed: 0,
            first_image_path: None,
            last_image_path: None,
            source_gallery_entry_id: None,
            source_vdn_job_id: None,
            continuation_mode: "last_frame".into(),
            reference_image_paths: vec![],
            reference_video_path: None,
            lora_paths: vec![],
            reference_audio_path: None,
            fp8: false,
            offload_dit: true,
            memory_mode: "auto".into(),
            native_two_pass: false,
            refine_steps: 3,
            refine_from_job_id: None,
            backend: VdnBackend::Diffusers,
            repo_root: None,
            checkpoint_root: None,
            gpu_class: None,
            server_port: None,
            acknowledge_third_party_code: false,
        };
        assert!(validate_request(&invalid).is_err());
    }
}

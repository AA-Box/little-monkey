//! Portable Character Studio training for Windows/Linux NVIDIA hosts.
//!
//! Apple Silicon keeps the existing verified MFLUX path in `studio_parity`.
//! This module fills the other desktop path without importing Python into the
//! signed Tauri process: a pinned Musubi Tuner checkout and its venv live under
//! the active profile, every long-running child is isolated in its own process
//! group/tree, cancellation kills that tree, and the only output promoted into
//! Studio is the final `.safetensors` LoRA.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime};
use tauri::AppHandle;
use uuid::Uuid;

use crate::profiles::ProfileScopedPaths;

const MUSUBI_VERSION: &str = "v0.3.4";
const MUSUBI_COMMIT: &str = "30c658c4f4b0bf05038b3346eff9670259b10fc7";
const MUSUBI_REPOSITORY: &str = "https://github.com/kohya-ss/musubi-tuner.git";
const MAX_TRAINING_IMAGES: usize = 128;
const MIN_TRAINING_IMAGES: usize = 4;
const MAX_LOG_BYTES: usize = 32 * 1024;
const SETUP_MARKER: &str = "portable-trainer.json";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterTrainerCapabilities {
    pub backend: String,
    pub supported: bool,
    pub setup_ready: bool,
    pub python_version: Option<String>,
    pub gpu_name: Option<String>,
    pub vram_mib: Option<u64>,
    pub compute_capability: Option<String>,
    pub minimum_vram_mib: Option<u64>,
    pub source_version: String,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableTrainingStatus {
    pub status: String,
    pub phase: String,
    pub log_tail: String,
    pub lora_path: Option<String>,
    pub lora_name: Option<String>,
    pub current_pid: Option<u32>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableTrainingRequest {
    pub name: String,
    pub source_dir: String,
    pub dit_path: String,
    pub vae_path: String,
    pub text_encoder_path: String,
    pub trigger_word: String,
    pub steps: u32,
    pub resolution: u32,
}

#[derive(Clone, Debug)]
struct NvidiaGpu {
    name: String,
    vram_mib: u64,
    major: u32,
    minor: u32,
}

#[derive(Clone, Debug)]
struct PythonCommand {
    executable: String,
    prefix: Vec<String>,
    version: String,
}

#[derive(Debug)]
struct TrainerRunState {
    status: String,
    phase: String,
    log_path: Option<PathBuf>,
    current_pid: Option<u32>,
    cancel_requested: bool,
    lora_path: Option<String>,
    lora_name: Option<String>,
}

impl Default for TrainerRunState {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            phase: String::new(),
            log_path: None,
            current_pid: None,
            cancel_requested: false,
            lora_path: None,
            lora_name: None,
        }
    }
}

static TRAINER_RUN: OnceLock<Mutex<TrainerRunState>> = OnceLock::new();

fn state() -> &'static Mutex<TrainerRunState> {
    TRAINER_RUN.get_or_init(|| Mutex::new(TrainerRunState::default()))
}

fn root(app: &AppHandle) -> Result<PathBuf, String> {
    let path = app
        .profile_data_dir()
        .map_err(|error| error.to_string())?
        .join("studio-v1")
        .join("character-trainer")
        .join("musubi");
    fs::create_dir_all(&path).map_err(|error| error.to_string())?;
    Ok(path)
}

fn repo_dir(root: &Path) -> PathBuf {
    root.join("musubi-tuner")
}

fn venv_python(root: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        root.join("venv").join("Scripts").join("python.exe")
    }
    #[cfg(not(target_os = "windows"))]
    {
        root.join("venv").join("bin").join("python")
    }
}

fn marker_path(root: &Path) -> PathBuf {
    root.join(SETUP_MARKER)
}

fn marker_matches(root: &Path) -> bool {
    fs::read_to_string(marker_path(root))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("commit").and_then(|v| v.as_str()).map(str::to_string))
        .is_some_and(|commit| commit == MUSUBI_COMMIT)
}

fn setup_ready(root: &Path) -> bool {
    marker_matches(root)
        && venv_python(root).is_file()
        && repo_dir(root)
            .join("src")
            .join("musubi_tuner")
            .join("zimage_train_network.py")
            .is_file()
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn detect_nvidia_gpu() -> Option<NvidiaGpu> {
    let output = command_output(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total,compute_cap",
            "--format=csv,noheader,nounits",
        ],
    )?;
    let line = output.lines().next()?.trim();
    let mut parts = line.split(',').map(str::trim);
    let name = parts.next()?.to_string();
    let vram_mib = parts.next()?.parse::<u64>().ok()?;
    let mut cap = parts.next()?.split('.');
    let major = cap.next()?.parse::<u32>().ok()?;
    let minor = cap.next().unwrap_or("0").parse::<u32>().ok()?;
    Some(NvidiaGpu {
        name,
        vram_mib,
        major,
        minor,
    })
}

fn python_probe(executable: &str, prefix: &[&str]) -> Option<PythonCommand> {
    let mut command = Command::new(executable);
    command.args(prefix).args([
        "-c",
        "import struct,sys; print(f'{sys.version_info.major}.{sys.version_info.minor} {struct.calcsize(\"P\")*8}')",
    ]);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut values = text.split_whitespace();
    let version = values.next()?.to_string();
    let bits = values.next()?.parse::<u32>().ok()?;
    let mut pieces = version.split('.');
    let major = pieces.next()?.parse::<u32>().ok()?;
    let minor = pieces.next()?.parse::<u32>().ok()?;
    if major != 3 || !(10..=12).contains(&minor) || bits != 64 {
        return None;
    }
    Some(PythonCommand {
        executable: executable.to_string(),
        prefix: prefix.iter().map(|value| (*value).to_string()).collect(),
        version,
    })
}

fn detect_python() -> Option<PythonCommand> {
    #[cfg(target_os = "windows")]
    {
        for selector in ["-3.12", "-3.11", "-3.10"] {
            if let Some(python) = python_probe("py", &[selector]) {
                return Some(python);
            }
        }
        python_probe("python", &[])
    }
    #[cfg(not(target_os = "windows"))]
    {
        for binary in ["python3.12", "python3.11", "python3.10", "python3", "python"] {
            if let Some(python) = python_probe(binary, &[]) {
                return Some(python);
            }
        }
        None
    }
}

fn minimum_vram_mib(gpu: &NvidiaGpu) -> u64 {
    if supports_fp8(gpu) {
        12 * 1024
    } else {
        16 * 1024
    }
}

fn supports_fp8(gpu: &NvidiaGpu) -> bool {
    gpu.major > 8 || (gpu.major == 8 && gpu.minor >= 9)
}

fn capability_reason(gpu: Option<&NvidiaGpu>, python: Option<&PythonCommand>) -> Option<String> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        let _ = (gpu, python);
        return None;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = (gpu, python);
        return Some("Portable Character Studio is not supported on Intel macOS. Apple Silicon uses the MFLUX trainer.".into());
    }
    #[cfg(not(target_os = "macos"))]
    {
        let Some(gpu) = gpu else {
            return Some("Local Character Studio training on Windows/Linux currently requires an NVIDIA GPU visible to nvidia-smi.".into());
        };
        if gpu.major < 8 {
            return Some(format!(
                "{} reports CUDA compute capability {}.{}. Z-Image training requires capability 8.0 or newer because its text-encoder path requires bfloat16.",
                gpu.name, gpu.major, gpu.minor
            ));
        }
        let floor = minimum_vram_mib(gpu);
        if gpu.vram_mib < floor {
            return Some(format!(
                "{} has {:.1} GiB VRAM; this training recipe requires at least {:.0} GiB.",
                gpu.name,
                gpu.vram_mib as f64 / 1024.0,
                floor as f64 / 1024.0
            ));
        }
        if python.is_none() {
            return Some("Install a 64-bit Python 3.10, 3.11, or 3.12 interpreter before setting up the local trainer.".into());
        }
        None
    }
}

#[tauri::command]
pub fn character_trainer_capabilities(app: AppHandle) -> Result<CharacterTrainerCapabilities, String> {
    let trainer_root = root(&app)?;
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Ok(CharacterTrainerCapabilities {
            backend: "mflux".into(),
            supported: true,
            setup_ready: true,
            python_version: None,
            gpu_name: Some("Apple Silicon / Metal".into()),
            vram_mib: None,
            compute_capability: None,
            minimum_vram_mib: None,
            source_version: "managed-mflux".into(),
            reason: None,
        });
    }
    #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
    {
        let gpu = detect_nvidia_gpu();
        let python = detect_python();
        let reason = capability_reason(gpu.as_ref(), python.as_ref());
        Ok(CharacterTrainerCapabilities {
            backend: "musubi_zimage".into(),
            supported: reason.is_none(),
            setup_ready: reason.is_none() && setup_ready(&trainer_root),
            python_version: python.as_ref().map(|value| value.version.clone()),
            gpu_name: gpu.as_ref().map(|value| value.name.clone()),
            vram_mib: gpu.as_ref().map(|value| value.vram_mib),
            compute_capability: gpu
                .as_ref()
                .map(|value| format!("{}.{}", value.major, value.minor)),
            minimum_vram_mib: gpu.as_ref().map(minimum_vram_mib),
            source_version: format!("musubi-tuner {MUSUBI_VERSION} ({})", &MUSUBI_COMMIT[..8]),
            reason,
        })
    }
}

fn safe_leaf(value: &str) -> String {
    let safe: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = safe.trim_matches(['.', '_']);
    if trimmed.is_empty() {
        "character".into()
    } else {
        trimmed.chars().take(80).collect()
    }
}

fn write_marker(root: &Path, torch_index: &str, python_version: &str) -> Result<(), String> {
    let value = serde_json::json!({
        "schemaVersion": 1,
        "source": MUSUBI_REPOSITORY,
        "version": MUSUBI_VERSION,
        "commit": MUSUBI_COMMIT,
        "torchIndex": torch_index,
        "pythonVersion": python_version,
        "installedAtMs": now_ms(),
    });
    fs::write(
        marker_path(root),
        serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn reset_run(status: &str, phase: &str, log_path: PathBuf) -> Result<(), String> {
    let mut run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
    if matches!(run.status.as_str(), "running" | "setting_up") {
        return Err("Character Studio already has an active operation".into());
    }
    run.status = status.into();
    run.phase = phase.into();
    run.log_path = Some(log_path);
    run.current_pid = None;
    run.cancel_requested = false;
    run.lora_path = None;
    run.lora_name = None;
    Ok(())
}

fn set_phase(phase: &str) {
    if let Ok(mut run) = state().lock() {
        run.phase = phase.to_string();
    }
}

fn set_pid(pid: Option<u32>) {
    if let Ok(mut run) = state().lock() {
        run.current_pid = pid;
    }
}

fn cancelled() -> bool {
    state()
        .lock()
        .map(|run| run.cancel_requested)
        .unwrap_or(true)
}

fn finish(status: &str, phase: &str, lora: Option<(&Path, &str)>) {
    if let Ok(mut run) = state().lock() {
        run.status = status.into();
        run.phase = phase.into();
        run.current_pid = None;
        if let Some((path, name)) = lora {
            run.lora_path = Some(path.to_string_lossy().into_owned());
            run.lora_name = Some(name.to_string());
        }
    }
}

fn tail_text(path: &Path, max: usize) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn append_log(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

fn configure_child(command: &mut Command) {
    command
        .env("PYTHONUNBUFFERED", "1")
        .env("PIP_DISABLE_PIP_VERSION_CHECK", "1")
        .env("GIT_TERMINAL_PROMPT", "0");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
}

fn terminate_tree(pid: u32) {
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(unix)]
    {
        let group = format!("-{pid}");
        let _ = Command::new("kill").args(["-TERM", &group]).output();
        thread::sleep(Duration::from_millis(600));
        let _ = Command::new("kill").args(["-KILL", &group]).output();
    }
}

fn run_logged(mut command: Command, label: &str, log_path: &Path) -> Result<(), String> {
    append_log(log_path, &format!("\n== {label} =="));
    configure_child(&mut command);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(|error| error.to_string())?;
    let err = log.try_clone().map_err(|error| error.to_string())?;
    command.stdout(Stdio::from(log)).stderr(Stdio::from(err));
    let mut child = command
        .spawn()
        .map_err(|error| format!("Failed to start {label}: {error}"))?;
    let pid = child.id();
    set_pid(Some(pid));
    loop {
        if cancelled() {
            terminate_tree(pid);
            let _ = child.wait();
            set_pid(None);
            return Err("cancelled".into());
        }
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) => {
                set_pid(None);
                if status.success() {
                    return Ok(());
                }
                return Err(format!(
                    "{label} failed ({}). Last output:\n{}",
                    status,
                    tail_text(log_path, MAX_LOG_BYTES)
                ));
            }
            None => thread::sleep(Duration::from_millis(250)),
        }
    }
}

fn python_command(python: &PythonCommand) -> Command {
    let mut command = Command::new(&python.executable);
    command.args(&python.prefix);
    command
}

fn venv_command(root: &Path) -> Command {
    Command::new(venv_python(root))
}

fn git_checkout(root: &Path, log_path: &Path) -> Result<(), String> {
    let repo = repo_dir(root);
    if repo.exists() && !marker_matches(root) {
        fs::remove_dir_all(&repo).map_err(|error| error.to_string())?;
    }
    if !repo.join(".git").is_dir() {
        fs::create_dir_all(&repo).map_err(|error| error.to_string())?;
        let mut init = Command::new("git");
        init.arg("init").arg(&repo);
        run_logged(init, "initialize pinned trainer source", log_path)?;
        let mut remote = Command::new("git");
        remote
            .current_dir(&repo)
            .args(["remote", "add", "origin", MUSUBI_REPOSITORY]);
        run_logged(remote, "configure trainer source", log_path)?;
    }
    let mut fetch = Command::new("git");
    fetch
        .current_dir(&repo)
        .args(["fetch", "--depth=1", "origin", MUSUBI_COMMIT]);
    run_logged(fetch, "download pinned Musubi Tuner source", log_path)?;
    let mut checkout = Command::new("git");
    checkout
        .current_dir(&repo)
        .args(["checkout", "--force", "--detach", "FETCH_HEAD"]);
    run_logged(checkout, "activate pinned Musubi Tuner source", log_path)?;
    let head = command_output_in(&repo, "git", &["rev-parse", "HEAD"])
        .ok_or("Could not verify the trainer source revision")?;
    if head.trim() != MUSUBI_COMMIT {
        return Err(format!(
            "Trainer source verification failed: expected {MUSUBI_COMMIT}, got {}",
            head.trim()
        ));
    }
    Ok(())
}

fn command_output_in(directory: &Path, program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .current_dir(directory)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn setup_impl(app: &AppHandle) -> Result<CharacterTrainerCapabilities, String> {
    let trainer_root = root(app)?;
    let gpu = detect_nvidia_gpu().ok_or(
        "Local Character Studio setup requires an NVIDIA GPU visible to nvidia-smi",
    )?;
    let python = detect_python().ok_or(
        "Local Character Studio setup requires 64-bit Python 3.10, 3.11, or 3.12",
    )?;
    if let Some(reason) = capability_reason(Some(&gpu), Some(&python)) {
        return Err(reason);
    }
    let log_path = trainer_root.join("setup.log");
    let _ = File::create(&log_path).map_err(|error| error.to_string())?;
    reset_run("setting_up", "Preparing trainer source", log_path.clone())?;

    let result = (|| {
        set_phase("Downloading pinned Musubi Tuner source");
        git_checkout(&trainer_root, &log_path)?;

        if !venv_python(&trainer_root).is_file() {
            set_phase("Creating isolated Python environment");
            let mut venv = python_command(&python);
            venv.args(["-m", "venv"]).arg(trainer_root.join("venv"));
            run_logged(venv, "create trainer venv", &log_path)?;
        }

        set_phase("Updating packaging tools");
        let mut pip = venv_command(&trainer_root);
        pip.args([
            "-m",
            "pip",
            "install",
            "--no-input",
            "--progress-bar",
            "off",
            "--upgrade",
            "pip",
            "setuptools",
            "wheel",
        ]);
        run_logged(pip, "update packaging tools", &log_path)?;

        let (torch, vision, index) = if gpu.major >= 10 {
            (
                "torch==2.7.1",
                "torchvision==0.22.1",
                "https://download.pytorch.org/whl/cu128",
            )
        } else {
            (
                "torch==2.5.1",
                "torchvision==0.20.1",
                "https://download.pytorch.org/whl/cu124",
            )
        };
        set_phase("Installing CUDA PyTorch");
        let mut torch_install = venv_command(&trainer_root);
        torch_install.args([
            "-m",
            "pip",
            "install",
            "--no-input",
            "--progress-bar",
            "off",
            torch,
            vision,
            "--index-url",
            index,
        ]);
        run_logged(torch_install, "install CUDA PyTorch", &log_path)?;

        set_phase("Installing Musubi Tuner dependencies");
        let mut package = venv_command(&trainer_root);
        package
            .current_dir(repo_dir(&trainer_root))
            .args([
                "-m",
                "pip",
                "install",
                "--no-input",
                "--progress-bar",
                "off",
                "-e",
                ".",
            ]);
        run_logged(package, "install Musubi Tuner", &log_path)?;

        set_phase("Checking CUDA training environment");
        let mut probe = venv_command(&trainer_root);
        probe.args([
            "-c",
            "import torch,accelerate; from musubi_tuner import zimage_train_network; assert torch.cuda.is_available(), 'CUDA is unavailable'; p=torch.cuda.get_device_properties(0); print(torch.__version__, p.name, p.total_memory)",
        ]);
        run_logged(probe, "verify trainer environment", &log_path)?;
        write_marker(&trainer_root, index, &python.version)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            finish("idle", "Trainer ready", None);
            character_trainer_capabilities(app.clone())
        }
        Err(error) if error == "cancelled" => {
            finish("cancelled", "Setup cancelled", None);
            Err(error)
        }
        Err(error) => {
            finish("error", "Trainer setup failed", None);
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn character_trainer_setup(app: AppHandle) -> Result<CharacterTrainerCapabilities, String> {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        tauri::async_runtime::spawn_blocking(move || setup_impl(&app))
            .await
            .map_err(|error| error.to_string())?
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = app;
        Err("The portable Musubi trainer is only used on Windows and Linux".into())
    }
}

fn canonical_model_file(value: &str, label: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value)
        .canonicalize()
        .map_err(|error| format!("Invalid {label}: {error}"))?;
    if !path.is_file() || path.extension().and_then(|value| value.to_str()) != Some("safetensors") {
        return Err(format!("{label} must be a local .safetensors file"));
    }
    Ok(path)
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "/")
        .replace('"', "\\\"")
}

fn prepare_dataset(
    request: &PortableTrainingRequest,
    run_root: &Path,
) -> Result<(PathBuf, usize), String> {
    let source = PathBuf::from(&request.source_dir)
        .canonicalize()
        .map_err(|error| format!("Invalid photo folder: {error}"))?;
    if !source.is_dir() {
        return Err("Photo source must be a directory".into());
    }
    let images = run_root.join("images");
    let cache = run_root.join("cache");
    fs::create_dir_all(&images).map_err(|error| error.to_string())?;
    fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
    let mut count = 0usize;
    for entry in fs::read_dir(source).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "webp") {
            continue;
        }
        if count >= MAX_TRAINING_IMAGES {
            return Err("Character Studio accepts at most 128 photos per run".into());
        }
        let stem = format!("image-{:04}", count + 1);
        fs::copy(&path, images.join(format!("{stem}.{extension}")))
            .map_err(|error| error.to_string())?;
        let caption_source = path.with_extension("txt");
        let caption = if caption_source.is_file() {
            fs::read_to_string(caption_source).unwrap_or_else(|_| request.trigger_word.clone())
        } else {
            format!("{}, portrait photograph of the same person", request.trigger_word.trim())
        };
        fs::write(images.join(format!("{stem}.txt")), caption)
            .map_err(|error| error.to_string())?;
        count += 1;
    }
    if count < MIN_TRAINING_IMAGES {
        return Err("Portable Character Studio needs at least 4 photos".into());
    }
    let repeats = ((request.steps as usize / count / 8).clamp(2, 40)).max(2);
    let dataset = run_root.join("dataset.toml");
    let config = format!(
        "[general]\nresolution = [{0}, {0}]\ncaption_extension = \".txt\"\nbatch_size = 1\nenable_bucket = true\nbucket_no_upscale = false\n\n[[datasets]]\nimage_directory = \"{1}\"\ncache_directory = \"{2}\"\nnum_repeats = {3}\n",
        request.resolution,
        toml_path(&images),
        toml_path(&cache),
        repeats,
    );
    fs::write(&dataset, config).map_err(|error| error.to_string())?;
    Ok((dataset, count))
}

fn training_impl(app: AppHandle, request: PortableTrainingRequest) -> Result<(PathBuf, String), String> {
    let trainer_root = root(&app)?;
    if !setup_ready(&trainer_root) {
        return Err("Set up the portable Character Studio trainer first".into());
    }
    if request.name.trim().is_empty() || request.name.len() > 80 {
        return Err("Character name must be 1–80 characters".into());
    }
    if request.trigger_word.trim().is_empty() || request.trigger_word.len() > 80 {
        return Err("Trigger word must be 1–80 characters".into());
    }
    if !(50..=10_000).contains(&request.steps) {
        return Err("Training steps must be between 50 and 10,000".into());
    }
    if !(512..=1024).contains(&request.resolution) || request.resolution % 64 != 0 {
        return Err("Training resolution must be 512–1024 and divisible by 64".into());
    }
    let gpu = detect_nvidia_gpu().ok_or("NVIDIA GPU disappeared before training")?;
    if let Some(reason) = capability_reason(Some(&gpu), detect_python().as_ref()) {
        return Err(reason);
    }
    let dit = canonical_model_file(&request.dit_path, "DiT model")?;
    let vae = canonical_model_file(&request.vae_path, "VAE model")?;
    let text_encoder = canonical_model_file(&request.text_encoder_path, "text encoder")?;

    let run_root = trainer_root
        .join("runs")
        .join(format!("{}-{}", safe_leaf(&request.name), Uuid::new_v4()));
    fs::create_dir_all(&run_root).map_err(|error| error.to_string())?;
    let output = run_root.join("output");
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    let (dataset, _image_count) = prepare_dataset(&request, &run_root)?;
    let log_path = run_root.join("training.log");
    let _ = File::create(&log_path).map_err(|error| error.to_string())?;
    reset_run("running", "Caching image latents", log_path.clone())?;

    let repo = repo_dir(&trainer_root);
    let python = venv_python(&trainer_root);
    let dataset_s = dataset.to_string_lossy().to_string();
    let vae_s = vae.to_string_lossy().to_string();
    let text_s = text_encoder.to_string_lossy().to_string();
    let dit_s = dit.to_string_lossy().to_string();

    set_phase("Step 1/4 · Caching image latents");
    let mut latents = Command::new(&python);
    latents.current_dir(&repo).args([
        "src/musubi_tuner/zimage_cache_latents.py",
        "--dataset_config",
        &dataset_s,
        "--vae",
        &vae_s,
    ]);
    latents.env("CUDA_VISIBLE_DEVICES", "0");
    run_logged(latents, "cache image latents", &log_path)?;

    set_phase("Step 2/4 · Caching text encoder outputs");
    let mut text_cache = Command::new(&python);
    text_cache.current_dir(&repo).args([
        "src/musubi_tuner/zimage_cache_text_encoder_outputs.py",
        "--dataset_config",
        &dataset_s,
        "--text_encoder",
        &text_s,
        "--batch_size",
        "8",
        "--fp8_llm",
    ]);
    text_cache.env("CUDA_VISIBLE_DEVICES", "0");
    run_logged(text_cache, "cache text encoder outputs", &log_path)?;

    set_phase("Step 3/4 · Training character LoRA");
    let output_name = format!("char_{}_zimage", safe_leaf(&request.name));
    let steps = request.steps.to_string();
    let output_s = output.to_string_lossy().to_string();
    let mut train = Command::new(&python);
    train
        .current_dir(&repo)
        .env("CUDA_VISIBLE_DEVICES", "0")
        .env("OMP_NUM_THREADS", "1")
        .args([
            "src/musubi_tuner/zimage_train_network.py",
            "--dit",
            &dit_s,
            "--vae",
            &vae_s,
            "--text_encoder",
            &text_s,
            "--dataset_config",
            &dataset_s,
            "--sdpa",
            "--mixed_precision",
            "bf16",
            "--blocks_to_swap",
            "16",
            "--timestep_sampling",
            "shift",
            "--weighting_scheme",
            "none",
            "--discrete_flow_shift",
            "2.0",
            "--optimizer_type",
            "adamw8bit",
            "--learning_rate",
            "1e-4",
            "--gradient_checkpointing",
            "--max_data_loader_n_workers",
            "2",
            "--persistent_data_loader_workers",
            "--network_module",
            "networks.lora_zimage",
            "--network_dim",
            "32",
            "--max_train_steps",
            &steps,
            "--save_precision",
            "bf16",
            "--seed",
            "42",
            "--output_dir",
            &output_s,
            "--output_name",
            &output_name,
        ]);
    if supports_fp8(&gpu) {
        train.args(["--fp8_base", "--fp8_scaled"]);
    }
    run_logged(train, "train character LoRA", &log_path)?;

    let trained = output.join(format!("{output_name}.safetensors"));
    if !trained.is_file() {
        return Err("Training completed but did not produce the expected .safetensors LoRA".into());
    }

    set_phase("Step 4/4 · Converting LoRA for Studio");
    let library = trainer_root
        .parent()
        .unwrap_or(&trainer_root)
        .join("trained-loras");
    fs::create_dir_all(&library).map_err(|error| error.to_string())?;
    let final_path = library.join(format!("{}.safetensors", safe_leaf(&request.name)));
    let trained_s = trained.to_string_lossy().to_string();
    let final_s = final_path.to_string_lossy().to_string();
    let mut convert = Command::new(&python);
    convert.current_dir(&repo).args([
        "src/musubi_tuner/networks/convert_lora.py",
        "--input",
        &trained_s,
        "--output",
        &final_s,
        "--target",
        "other",
    ]);
    run_logged(convert, "convert LoRA", &log_path)?;
    if !final_path.is_file() {
        return Err("LoRA conversion completed without writing its output".into());
    }
    Ok((final_path, request.name))
}

#[tauri::command]
pub fn character_portable_training_start(
    app: AppHandle,
    request: PortableTrainingRequest,
) -> Result<PortableTrainingStatus, String> {
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        {
            let run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
            if matches!(run.status.as_str(), "running" | "setting_up") {
                return Err("Character Studio already has an active operation".into());
            }
        }
        thread::spawn(move || match training_impl(app, request) {
            Ok((path, name)) => finish("complete", "Character LoRA ready", Some((&path, &name))),
            Err(error) if error == "cancelled" => finish("cancelled", "Training cancelled", None),
            Err(error) => {
                if let Ok(run) = state().lock() {
                    if let Some(log) = &run.log_path {
                        append_log(log, &format!("\nERROR: {error}"));
                    }
                }
                finish("error", &error, None);
            }
        });
        character_training_status()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (app, request);
        Err("The portable Musubi trainer is only used on Windows and Linux".into())
    }
}

#[tauri::command]
pub fn character_training_status() -> Result<PortableTrainingStatus, String> {
    let run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
    Ok(PortableTrainingStatus {
        status: run.status.clone(),
        phase: run.phase.clone(),
        log_tail: run
            .log_path
            .as_deref()
            .map(|path| tail_text(path, MAX_LOG_BYTES))
            .unwrap_or_default(),
        lora_path: run.lora_path.clone(),
        lora_name: run.lora_name.clone(),
        current_pid: run.current_pid,
    })
}

#[tauri::command]
pub fn character_training_cancel() -> Result<(), String> {
    let pid = {
        let mut run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
        if !matches!(run.status.as_str(), "running" | "setting_up") {
            return Ok(());
        }
        run.cancel_requested = true;
        run.phase = "Cancelling…".into();
        run.current_pid
    };
    if let Some(pid) = pid {
        terminate_tree(pid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_leaf_never_creates_a_path() {
        assert_eq!(safe_leaf("../Jane / Doe"), "Jane___Doe");
        assert_eq!(safe_leaf("..."), "character");
    }

    #[test]
    fn fp8_floor_tracks_gpu_generation() {
        let ampere = NvidiaGpu {
            name: "A".into(),
            vram_mib: 24 * 1024,
            major: 8,
            minor: 6,
        };
        let ada = NvidiaGpu {
            name: "B".into(),
            vram_mib: 24 * 1024,
            major: 8,
            minor: 9,
        };
        assert!(!supports_fp8(&ampere));
        assert_eq!(minimum_vram_mib(&ampere), 16 * 1024);
        assert!(supports_fp8(&ada));
        assert_eq!(minimum_vram_mib(&ada), 12 * 1024);
    }
}

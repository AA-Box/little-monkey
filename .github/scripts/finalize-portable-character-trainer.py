from pathlib import Path


def patch(path: str, old: str, new: str) -> None:
    target = Path(path)
    text = target.read_text()
    if new in text:
        return
    if old not in text:
        raise SystemExit(f"anchor not found in {path}: {old[:140]!r}")
    target.write_text(text.replace(old, new, 1))


# Wire the module and commands into the Tauri surface.
patch(
    "src-tauri/src/lib.rs",
    "pub mod studio_parity;\npub mod studio_tools;",
    "pub mod studio_parity;\npub mod character_trainer;\npub mod studio_tools;",
)
patch(
    "src-tauri/src/lib.rs",
    "            studio_parity::character_training_start,\n            studio_parity::studio_workflow_run,",
    "            studio_parity::character_training_start,\n            character_trainer::character_trainer_capabilities,\n            character_trainer::character_trainer_setup,\n            character_trainer::character_portable_training_start,\n            character_trainer::character_training_status,\n            character_trainer::character_training_cancel,\n            studio_parity::studio_workflow_run,",
)

# Portable trainer correctness/hardening fixes found during end-to-end review.
patch(
    "src-tauri/src/character_trainer.rs",
    '''#[cfg(target_os = "macos")]
    {
        let _ = (gpu, python);
        return Some("Portable Character Studio is not supported on Intel macOS. Apple Silicon uses the MFLUX trainer.".into());
    }''',
    '''#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
    {
        let _ = (gpu, python);
        return Some("Portable Character Studio is not supported on Intel macOS. Apple Silicon uses the MFLUX trainer.".into());
    }''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''fn capability_reason(gpu: Option<&NvidiaGpu>, python: Option<&PythonCommand>) -> Option<String> {''',
    '''fn portable_gpu_reason(gpu: Option<&NvidiaGpu>) -> Option<String> {
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
    None
}

fn git_available() -> bool {
    command_output("git", &["--version"]).is_some()
}

fn capability_reason(gpu: Option<&NvidiaGpu>, python: Option<&PythonCommand>) -> Option<String> {''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    #[cfg(not(target_os = "macos"))]
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
    }''',
    '''    #[cfg(not(target_os = "macos"))]
    {
        if let Some(reason) = portable_gpu_reason(gpu) {
            return Some(reason);
        }
        if python.is_none() {
            return Some("Install a 64-bit Python 3.10, 3.11, or 3.12 interpreter before setting up the local trainer.".into());
        }
        if !git_available() {
            return Some("Install Git before setting up the local Character Studio trainer; the pinned Musubi source is verified by commit before use.".into());
        }
        None
    }''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    let trimmed = safe.trim_matches(['.', '_']);''',
    '''    let trimmed = safe.trim_matches(|character| character == '.' || character == '_');''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''fn set_phase(phase: &str) {
    if let Ok(mut run) = state().lock() {
        run.phase = phase.to_string();
    }
}''',
    '''fn set_phase(phase: &str) {
    if let Ok(mut run) = state().lock() {
        run.phase = phase.to_string();
    }
}

fn attach_training_log(path: PathBuf) {
    if let Ok(mut run) = state().lock() {
        run.log_path = Some(path);
    }
}''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    fetch
        .current_dir(&repo)
        .args(["fetch", "--depth=1", "origin", MUSUBI_COMMIT]);''',
    '''    fetch
        .current_dir(&repo)
        .args(["fetch", "--depth=1", "origin", "tag", MUSUBI_VERSION]);''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    let python = detect_python().ok_or(
        "Local Character Studio setup requires 64-bit Python 3.10, 3.11, or 3.12",
    )?;
    if let Some(reason) = capability_reason(Some(&gpu), Some(&python)) {''',
    '''    let python = detect_python().ok_or(
        "Local Character Studio setup requires 64-bit Python 3.10, 3.11, or 3.12",
    )?;
    if !git_available() {
        return Err("Local Character Studio setup requires Git so the pinned Musubi release can be verified before use".into());
    }
    if let Some(reason) = capability_reason(Some(&gpu), Some(&python)) {''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    if let Some(reason) = capability_reason(Some(&gpu), detect_python().as_ref()) {
        return Err(reason);
    }''',
    '''    if let Some(reason) = portable_gpu_reason(Some(&gpu)) {
        return Err(reason);
    }''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    let _ = File::create(&log_path).map_err(|error| error.to_string())?;
    reset_run("running", "Caching image latents", log_path.clone())?;

    let repo = repo_dir(&trainer_root);''',
    '''    let _ = File::create(&log_path).map_err(|error| error.to_string())?;
    attach_training_log(log_path.clone());
    set_phase("Caching image latents");

    let repo = repo_dir(&trainer_root);''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''    let library = trainer_root
        .parent()
        .unwrap_or(&trainer_root)
        .join("trained-loras");''',
    '''    let library = trainer_root
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&trainer_root)
        .join("trained-loras");''',
)
patch(
    "src-tauri/src/character_trainer.rs",
    '''        {
            let run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
            if matches!(run.status.as_str(), "running" | "setting_up") {
                return Err("Character Studio already has an active operation".into());
            }
        }
        thread::spawn(move || match training_impl(app, request) {''',
    '''        {
            let mut run = state().lock().map_err(|_| "Character trainer state is poisoned")?;
            if matches!(run.status.as_str(), "running" | "setting_up") {
                return Err("Character Studio already has an active operation".into());
            }
            run.status = "running".into();
            run.phase = "Preparing training dataset".into();
            run.log_path = None;
            run.current_pid = None;
            run.cancel_requested = false;
            run.lora_path = None;
            run.lora_name = None;
        }
        thread::spawn(move || match training_impl(app, request) {''',
)

# Keep docs truthful about the two local trainer backends and the remaining gap.
patch(
    "docs/limitations.md",
    "Character LoRA training currently uses the verified MFLUX runtime on Apple silicon and requires a local Krea-2-Raw model directory.",
    "Character LoRA training uses the verified MFLUX runtime with a local Krea-2-Raw model directory on Apple silicon; Windows/Linux NVIDIA hosts use a profile-scoped, pinned Musubi Tuner v0.3.4 Z-Image environment with local DiT/VAE/Qwen3 weights. The portable recipe requires CUDA compute capability 8.0+, 12 GiB VRAM on the fp8 path or 16 GiB otherwise, plus 64-bit Python 3.10–3.12 and Git for first-time setup. AMD/ROCm Character Studio training is not shipped yet.",
)

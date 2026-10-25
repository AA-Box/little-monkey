#!/usr/bin/env python3
"""Little Monkey's standalone VDN-H3 local inference worker.

No FreeVideo or ComfyUI dependency. Uses the official OpenVDN published
ModularPipeline components and its trained hybrid-attention weights.
These weights have MiniMax H3 geographic license restrictions; the Rust
host checks authorization *before* spawning this worker or allowing Hub
downloads. The worker itself is not an authorization mechanism.

Python environment requirements: CUDA PyTorch, accelerate, av, numpy,
pillow, and the OpenVDN-compatible patched diffusers ModularPipeline.
The selected Hugging Face repository publishes remote Python model code
and loading it requires trust_remote_code=True. A user must explicitly
consent to executing the upstream third-party components.
"""
import argparse
import json
import os
from pathlib import Path
import sys
import shutil
import tempfile
import subprocess
import signal
import importlib.util
import hashlib

# The Rust host embeds this module into its self-contained, job-specific
# staged worker. Running the checked-in Python script directly imports the
# same Little Monkey-owned implementation from the adjacent source file.
if "native_sampler_recovery" not in globals():
    # unittest loads this file by spec_from_file_location and does not add
    # scripts/ to sys.path. Resolve only the sibling app-owned source module.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from studio_vdn_sampler import (
        native_sampler_recovery, clear_native_sampler_stage,
    )

MODEL_REPOSITORY = "OpenVDN/vdn-minimax-h3"
# Ref2VA is a distinct original H3 checkpoint partition, not VDN hybrid LoRA.
REF_MODEL_REPOSITORY = "MiniMaxAI/MiniMax-H3"
FPS = 24


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--probe", action="store_true")
    parser.add_argument("--prompt", default="")
    parser.add_argument("--output", default="")
    parser.add_argument("--first", default="")
    parser.add_argument("--last", default="")
    parser.add_argument("--refs", nargs="+", default=[])
    parser.add_argument("--reference-video", default="")
    parser.add_argument("--reference-audio", default="")
    parser.add_argument("--lora", action="append", default=[])
    parser.add_argument("--steps", type=int, default=8)
    parser.add_argument("--frames", type=int, default=141)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--fp8", action="store_true")
    parser.add_argument("--offload-dit", action="store_true")
    parser.add_argument("--memory-mode", choices=("auto", "resident", "stream_cpu", "stream_disk"), default="auto")
    parser.add_argument("--two-pass", action="store_true")
    parser.add_argument("--refine-from", default="")
    parser.add_argument("--checkpoint", default="")
    parser.add_argument("--step-checkpoint-root", default="")
    parser.add_argument("--refine-steps", type=int, default=3)
    parser.add_argument("--upscaler-cache-dir", default="")
    parser.add_argument("--internal-attempt", choices=("resident", "resident_fp8", "stream_cpu", "stream_disk", "fp8_disk", "int8_cpu", "fp8_cpu"), default="")
    parser.add_argument("--device", default="cuda")
    args = parser.parse_args(argv)
    if args.self_test:
        print(json.dumps({
            "status": "ready_for_dependency_probe",
            "model": MODEL_REPOSITORY,
            "fps": FPS,
            "backend": "openvdn-diffusers",
        }), flush=True)
        return args
    if args.probe:
        # This is a real installed dependency check, not a simulated GPU.
        # Never loads model remote code or downloads checkpoint bytes.
        import torch
        from diffusers import ModularPipeline
        from diffusers.hooks import apply_group_offloading
        from accelerate import cpu_offload_with_hook
        from diffusers.utils.export_utils import encode_video
        import av
        if not torch.cuda.is_available():
            raise RuntimeError("The selected Python environment has no CUDA GPU.")
        cap = torch.cuda.get_device_capability()
        report = {
            "status": "ready",
            "model": MODEL_REPOSITORY,
            "backend": "openvdn-diffusers",
            "device": torch.cuda.get_device_name(),
            "computeCapability": f"{cap[0]}.{cap[1]}",
            "vramBytes": torch.cuda.get_device_properties(0).total_memory,
            "torch": torch.__version__,
            "fp8Supported": cap[0] >= 9,
        }
        free_vram, _ = torch.cuda.mem_get_info()
        host_ram = available_host_ram()
        disk_free = shutil.disk_usage(Path.cwd()).free
        try:
            report["nativeMemoryPlans"] = choose_memory_plans(
                free_vram, host_ram, disk_free, capability=cap[0],
                torchao_available=importlib.util.find_spec("torchao") is not None,
            )
            report["resourcesReady"] = True
        except ValueError as exc:
            report["nativeMemoryPlans"] = []
            report["resourcesReady"] = False
            report["resourceWarning"] = str(exc)
        report["freeVramBytes"] = free_vram
        report["hostRamAvailableBytes"] = host_ram
        report["diskFreeBytes"] = disk_free
        if not hasattr(ModularPipeline, "load_components"):
            raise RuntimeError("Installed Diffusers is missing ModularPipeline.load_components")
        print(json.dumps(report), flush=True)
        return args
    if not args.prompt.strip() or len(args.prompt.encode("utf-8")) > 8192:
        parser.error("The prompt must be non-empty and no longer than 8192 bytes")
    if not args.output or not Path(args.output).is_absolute():
        parser.error("A controlled absolute output path is required")
    if not 1 <= args.steps <= 50:
        parser.error("Step count must be from 1 to 50")
    if not 120 <= args.frames <= 360 or (args.frames - 5) % 17:
        parser.error("Frame count must be a supported 17n+5 value (120..360)")
    if args.step_checkpoint_root and not Path(args.step_checkpoint_root).is_absolute():
        parser.error("Native step checkpoint location must be absolute and app-owned")
    if args.refine_from and not args.two_pass:
        parser.error("Refine-from needs native two-pass mode")
    if args.two_pass and (not args.checkpoint or not Path(args.checkpoint).is_absolute()):
        parser.error("Native two-pass needs an app-owned absolute checkpoint path")
    if not 1 <= args.refine_steps <= 12:
        parser.error("Native refinement steps must be 1..12")
    if args.two_pass and (args.steps != 8 or args.refine_steps != 3):
        parser.error("Trained H3 two-pass requires eight preview and three refinement steps")
    if args.two_pass and (not args.upscaler_cache_dir or
                          not Path(args.upscaler_cache_dir).is_absolute()):
        parser.error("Native two-pass needs an absolute app-owned learned upscaler cache")
    if args.device != "cuda":
        parser.error("This worker currently supports CUDA only; use native Wan for macOS")
    if (args.first or args.last) and (args.refs or args.reference_video or args.reference_audio):
        parser.error("Keyframe and Ref2VA conditioning cannot be combined in one inference")
    if args.reference_audio and not (args.refs or args.reference_video):
        parser.error("Ref2VA audio requires an image or video reference")
    if len(args.refs) > 9 or len(args.lora) > 4:
        parser.error("Ref2VA supports at most nine image references and four LoRA adapters")
    for name, path, maximum in (
        ("first keyframe", args.first, 32 * 1024**2),
        ("last keyframe", args.last, 32 * 1024**2),
        *[("reference image", path, 32 * 1024**2) for path in args.refs],
        ("reference video", args.reference_video, 256 * 1024**2),
        ("reference audio", args.reference_audio, 64 * 1024**2),
        *[("LoRA", path, 8 * 1024**3) for path in args.lora],
    ):
        if not path:
            continue
        candidate = Path(path)
        if not candidate.is_file() or candidate.is_symlink():
            parser.error(f"{name} must be a local regular file")
        size = candidate.stat().st_size
        if not 8 <= size <= maximum:
            parser.error(f"{name} size must be 8 bytes to {maximum} bytes")
    return args


GiB = 1024 ** 3


def available_host_ram():
    """Linux MemAvailable or cross-platform psutil if installed; unknown is None."""
    try:
        import psutil
        return int(psutil.virtual_memory().available)
    except ImportError:
        pass
    try:
        with open("/proc/meminfo", encoding="ascii") as handle:
            for line in handle:
                if line.startswith("MemAvailable:"):
                    return int(line.split()[1]) * 1024
    except (OSError, IndexError, ValueError):
        pass
    return None


def is_cuda_oom(error):
    """Only retry allocation failures, not bad weights, bad prompts or permissions."""
    reason = str(error).lower()
    return any(marker in reason for marker in (
        "cuda out of memory", "cublas_status_alloc_failed", "cuda error: out of memory",
        "torch.outofmemoryerror", "cuda error: memory allocation",
    ))


def choose_memory_plans(free_vram, host_ram, disk_free, *,
                        capability, offload=False, fp8=False, mode="auto",
                        torchao_available=True):
    """Ordered real execution tiers, with conservative memory safety margins.

    A tier is not advertised as supported unless prerequisites exist.
    Failed tiers are retried by a *new Python process*, not on a poisoned GPU.
    """
    if free_vram < 0 or disk_free < 0 or (host_ram is not None and host_ram < 0):
        raise ValueError("Negative memory capacity")
    if mode not in ("auto", "resident", "stream_cpu", "stream_disk"):
        raise ValueError("Unsupported memory policy")
    if fp8 and capability < 9:
        raise ValueError("FP8 requires compute capability 9.0+")
    # Official H3 BF16 weights are ~61.7 GB (denoiser) and ~62.1 GB
    # (Qwen3-VL). An offload switch does not make those host requirements
    # disappear. The INT8 documented recipe needs about 75 GB host memory.
    cpu_ok = host_ram is not None and host_ram >= 136 * GiB
    quant_cpu_ok = host_ram is not None and host_ram >= 80 * GiB
    disk_ok = (host_ram is not None and host_ram >= 40 * GiB
               and disk_free >= 160 * GiB)
    resident_ok = (host_ram is not None and host_ram >= 68 * GiB
                   and free_vram >= (48 if fp8 else 78) * GiB)
    plans = []
    def add(tier):
        if tier not in plans:
            plans.append(tier)
    if mode == "resident":
        if not resident_ok:
            raise ValueError("Resident H3 requires approximately 78 GiB VRAM (48 GiB FP8) and 68 GiB host RAM")
        add("resident_fp8" if fp8 else "resident")
    elif mode == "stream_cpu":
        if not (quant_cpu_ok if fp8 else cpu_ok):
            raise ValueError("CPU streaming needs 136 GiB available host RAM (80 GiB FP8)")
        add("fp8_cpu" if fp8 else "stream_cpu")
    elif mode == "stream_disk":
        if not disk_ok:
            raise ValueError("Disk streaming needs 160 GiB free storage and 40 GiB available host RAM")
        add("fp8_disk" if fp8 else "stream_disk")
    else:
        if not offload and resident_ok:
            add("resident_fp8" if fp8 else "resident")
        if fp8 and quant_cpu_ok and capability >= 9:
            add("fp8_cpu")
        if not fp8 and cpu_ok:
            add("stream_cpu")
        if not fp8 and torchao_available and quant_cpu_ok:
            add("int8_cpu")
        if disk_ok:
            add("fp8_disk" if fp8 else "stream_disk")
    if not plans:
        raise ValueError("Native H3 requires substantial host RAM or disk-backed model streaming; available resources are insufficient")
    return plans


def _forward_signal(signum, _frame):
    child = getattr(_forward_signal, "child", None)
    if child is not None and child.poll() is None:
        try:
            if os.name == "posix":
                os.killpg(child.pid, signal.SIGTERM)
            else:
                child.terminate()
        except ProcessLookupError:
            pass
    raise SystemExit(128 + signum)


def run(args):
    import torch
    if not torch.cuda.is_available():
        raise RuntimeError("VDN-H3 requires CUDA; no CPU-only inference claim")
    major, _ = torch.cuda.get_device_capability()
    if args.internal_attempt:
        args.offload_dit = not args.internal_attempt.startswith("resident")
        args.disk_offload = args.internal_attempt.endswith("_disk") or args.internal_attempt == "stream_disk"
        args.int8 = args.internal_attempt == "int8_cpu"
        args.fp8 = args.internal_attempt.startswith("fp8") or args.internal_attempt == "resident_fp8"
        with tempfile.TemporaryDirectory(prefix="vdn-spill-", dir=Path(args.output).parent) as cache:
            args.disk_offload_path = cache if args.disk_offload else None
            return _run_once(args)

    free, total = torch.cuda.mem_get_info()
    host = available_host_ram()
    disk = shutil.disk_usage(Path(args.output).parent).free
    plans = choose_memory_plans(
        free, host, disk, capability=major, offload=args.offload_dit,
        fp8=args.fp8, mode=args.memory_mode,
        torchao_available=importlib.util.find_spec("torchao") is not None,
    )
    print(json.dumps({"phase": "memory_plan", "engine": "little_monkey_native",
                      "tiers": plans, "freeVramBytes": free,
                      "totalVramBytes": total, "freeHostRamBytes": host,
                      "freeDiskBytes": disk, "prefetch": "diffusers_stream_hooks"}),
          flush=True)
    original = [str(Path(__file__).resolve()), "--prompt", args.prompt,
                "--output", args.output, "--frames", str(args.frames),
                "--steps", str(args.steps), "--seed", str(args.seed),
                "--device", args.device]
    for flag, value in (("--first", args.first), ("--last", args.last),
                        ("--reference-video", args.reference_video),
                        ("--reference-audio", args.reference_audio),
                        ("--checkpoint", args.checkpoint),
                        ("--step-checkpoint-root", args.step_checkpoint_root),
                        ("--refine-from", args.refine_from),
                        ("--upscaler-cache-dir", args.upscaler_cache_dir)):
        if value:
            original.extend([flag, value])
    if args.refs:
        original.extend(["--refs", *args.refs])
    for lora in args.lora:
        original.extend(["--lora", lora])
    if args.two_pass:
        original.extend(["--two-pass", "--refine-steps", str(args.refine_steps)])
    for tier in plans:
        argv = [sys.executable, "-u", *original, "--internal-attempt", tier]
        if tier.startswith("fp8") or tier == "resident_fp8":
            argv.append("--fp8")
        print(json.dumps({"phase": "attempt", "tier": tier}), flush=True)
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                 text=True, bufsize=1,
                                 start_new_session=(os.name == "posix"))
        _forward_signal.child = child
        errors = []
        try:
            for line in child.stdout:
                print(line, end="", flush=True)
                errors.append(line)
                if len(errors) > 100:
                    errors.pop(0)
            code = child.wait()
        finally:
            _forward_signal.child = None
        if code == 0:
            return
        reason = "".join(errors)[-8000:]
        if not is_cuda_oom(reason):
            raise RuntimeError("VDN native inference failed outside memory recovery: " + reason[-1400:])
        print(json.dumps({"phase": "oom_recovery", "tier": tier,
                          "next": "fresh_process", "exitCode": code}), flush=True)
    raise RuntimeError("All native CUDA placement tiers exhausted their memory budget")


for _sig in (signal.SIGTERM, signal.SIGINT):
    signal.signal(_sig, _forward_signal)


def checkpoint_identity(args):
    """Content-address the original request, never scratch-file job paths."""
    digest = hashlib.sha256()
    for value in (args.prompt, args.frames, args.seed, args.steps):
        digest.update(str(value).encode("utf-8"))
        digest.update(b"\\0")
    media = [
        ("first", args.first), ("last", args.last),
        ("reference_video", args.reference_video),
        ("reference_audio", args.reference_audio),
        *[("ref", path) for path in args.refs],
        *[("lora", path) for path in args.lora],
    ]
    for kind, file in media:
        digest.update(kind.encode("ascii"))
        if not file:
            digest.update(b"none")
            continue
        with open(file, "rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                digest.update(chunk)
        digest.update(b"\\0")
    return digest.hexdigest()


def validate_reference_media(args):
    """Bound decoded media size/duration before allocating video/audio tensors."""
    if args.reference_video or args.reference_audio:
        import av
        for media in (args.reference_video, args.reference_audio):
            if not media:
                continue
            with av.open(media) as container:
                seconds = (container.duration / av.time_base
                           if container.duration is not None else None)
                if seconds is None or seconds <= 0 or seconds > 30:
                    raise RuntimeError("Reference media duration must be known and at most 30 seconds")
                if media == args.reference_video:
                    stream = next((v for v in container.streams if v.type == "video"), None)
                    if stream is None:
                        raise RuntimeError("Reference video does not contain video frames")
                    if stream.width * stream.height > 1920 * 1080:
                        raise RuntimeError("Reference video exceeds 1080p safety bound")
                    if stream.frames and stream.frames > 900:
                        raise RuntimeError("Reference video exceeds 900 frames")
    if args.refs:
        from PIL import Image
        for source in args.refs:
            with Image.open(source) as photo:
                if photo.width * photo.height > 16_777_216:
                    raise RuntimeError("Reference image exceeds 16 megapixels")


def validate_preview_layout(video_shape, audio_shape):
    """Prevent malformed/corrupt checkpoints from allocating arbitrary GPU shapes."""
    if len(video_shape) != 5 or video_shape[0] != 1 or video_shape[1] != 24:
        raise ValueError("Native preview must hold 1 x 24-channel video latents")
    if not 1 <= video_shape[2] <= 102 or tuple(video_shape[3:]) != (24, 42):
        raise ValueError("Native preview geometry must be half-canvas video latents")
    if len(audio_shape) != 3 or audio_shape[0:2] != (2, 32):
        raise ValueError("Native preview must hold stereo-channel H3 audio latents")
    if not 1 <= audio_shape[2] <= 500:
        raise ValueError("Native preview audio latent length invalid")


def native_h3_refine_schedule(pipe, torch):
    """Temporarily install the genuine three-step H3 video/audio sigma tail.

    The ModularPipeline normally replaces both schedules at run start.
    Bound wrappers intercept that reset, then restore the original methods
    even if denoising is cancelled or a CUDA error interrupts the process.
    """
    import contextlib
    import types
    video = pipe.scheduler
    audio = pipe.audio_scheduler
    original_video = video.set_timesteps
    original_audio = audio.set_timesteps
    sigmas = [0.9035, 0.6316, 0.3158, 0.0]
    base = [sigma / (12.0 + (1.0 - 12.0) * sigma) for sigma in sigmas]
    audio_sigmas = [3.0 * sigma / (1.0 + 2.0 * sigma) for sigma in base]

    def reset_video(*args, **kwargs):
        return original_video(sigmas=sigmas, device=kwargs.get("device"))

    def reset_audio(*args, **kwargs):
        return original_audio(sigmas=audio_sigmas, device=kwargs.get("device"))

    @contextlib.contextmanager
    def active():
        video.set_timesteps = reset_video
        audio.set_timesteps = reset_audio
        try:
            yield
        finally:
            video.set_timesteps = original_video
            audio.set_timesteps = original_audio
    return active()


def native_two_pass(pipe, args, torch, device, keyframes, references):
    """Native latent-space two-pass using Diffusers' actual MiniMax-H3 state I/O.

    Pass one samples at half the target height/width. The separate Little
    Monkey-owned 3D learned model lifts normalized H3 video latents to the
    target canvas. Three true H3 diffusion steps then refine those latents.
    The publisher's pinned model weights must be present and verified.
    """
    from contextlib import contextmanager
    from safetensors import safe_open
    from safetensors.torch import save_file

    fingerprint = checkpoint_identity(args)
    source = args.refine_from or args.checkpoint
    def read(path):
        if os.path.islink(path) or not os.path.isfile(path) or os.path.getsize(path) > 64 * 1024**2:
            raise RuntimeError("Native preview checkpoint is absent or too large")
        with safe_open(path, framework="pt", device="cpu") as handle:
            meta = handle.metadata() or {}
            if meta.get("schema") != "little-monkey-vdn-latents-v1":
                raise RuntimeError("Unsupported native preview checkpoint")
            if meta.get("identity") != fingerprint:
                raise RuntimeError("Preview inputs changed; rejecting latent mismatch")
            return handle.get_tensor("video"), handle.get_tensor("audio")

    if args.refine_from or (os.path.isfile(args.checkpoint) and
                             os.path.getsize(args.checkpoint) > 0):
        latents, audio_latents = read(source)
        print(json.dumps({"phase": "resuming_preview_latents", "path": source}), flush=True)
    else:
        print(json.dumps({"phase": "native_preview_sampling",
                          "canvas": [384, 672]}), flush=True)
        with native_sampler_recovery(args, "preview", pipe, model_identity=(
                REF_MODEL_REPOSITORY if references else MODEL_REPOSITORY)):
            preview = pipe(
            prompt=args.prompt,
            num_frames=args.frames,
            num_inference_steps=args.steps + 1,
            height=384, width=672,
            generator=torch.Generator(device).manual_seed(args.seed),
            output=["latents", "audio_latents"],
            **({"references": references} if references else keyframes),
            )
        latents, audio_latents = preview["latents"], preview["audio_latents"]
        validate_preview_layout(tuple(latents.shape), tuple(audio_latents.shape))
        from pathlib import Path
        destination = Path(args.checkpoint)
        destination.parent.mkdir(parents=True, exist_ok=True)
        temp = str(destination) + ".partial"
        save_file({"video": latents.detach().float().cpu().contiguous(),
                   "audio": audio_latents.detach().float().cpu().contiguous()},
                  temp, metadata={"schema": "little-monkey-vdn-latents-v1",
                                  "identity": fingerprint})
        with open(temp, "rb") as written:
            os.fsync(written.fileno())
        os.replace(temp, destination)
        clear_native_sampler_stage(args, "preview")
        if os.name == "posix":
            folder = os.open(str(destination.parent), os.O_RDONLY)
            try:
                os.fsync(folder)
            finally:
                os.close(folder)
        print(json.dumps({"phase": "preview_checkpointed", "path": str(destination)}),
              flush=True)

    validate_preview_layout(tuple(latents.shape), tuple(audio_latents.shape))
    latents = latents.to(device=device, dtype=torch.float32)
    audio_latents = audio_latents.to(device=device, dtype=torch.float32)
    if latents.shape[2] != (args.frames - 5) // 17 * 5 + 2:
        raise RuntimeError("Preview latent time axis does not match requested frames")
    print(json.dumps({"phase": "native_learned_upscaler",
                      "architecture": "h3-trained-3d-24channel",
                      "checkpoint": "pinned-sha256"}), flush=True)
    upscaled = native_learned_upscale(latents, args.upscaler_cache_dir, (48, 84))
    # Start an actual 3-step H3 tail schedule with calibrated noise, rather
    # than restarting a full sigma=1 schedule or adding arbitrary 10% noise.
    # Audio has its own H3 sigma shift and is initialized consistently.
    video_sigma = 0.9035
    raw_sigma = video_sigma / (12.0 + (1.0 - 12.0) * video_sigma)
    audio_sigma = 3.0 * raw_sigma / (1.0 + 2.0 * raw_sigma)
    generator = torch.Generator(device).manual_seed(args.seed + 1)
    upscaled = ((1.0 - video_sigma) * upscaled +
                video_sigma * torch.randn(upscaled.shape, generator=generator,
                                           device=device, dtype=upscaled.dtype))
    audio_latents = ((1.0 - audio_sigma) * audio_latents +
                     audio_sigma * torch.randn(audio_latents.shape,
                                               generator=generator, device=device,
                                               dtype=audio_latents.dtype))
    print(json.dumps({"phase": "native_latent_refinement",
                      "steps": 3, "latentReuse": True, "learnedUpscale": True,
                      "schedule": "h3-calibrated-three-step"}), flush=True)
    with native_h3_refine_schedule(pipe, torch):
        with native_sampler_recovery(args, "refine", pipe, model_identity=(
                REF_MODEL_REPOSITORY if references else MODEL_REPOSITORY)):
            return pipe(
            prompt=args.prompt, num_frames=args.frames, height=768, width=1344,
            num_inference_steps=4,
            generator=torch.Generator(device).manual_seed(args.seed + 2),
            latents=upscaled, audio_latents=audio_latents,
            output=["videos", "audio", "sampling_rate"],
            **({"references": references} if references else keyframes),
            )


def _run_once(args):
    # Import only AFTER the host has decided the license permits execution.
    # Do not import or initialize CUDA in '--self-test' mode.
    import numpy as np
    import torch
    from accelerate import cpu_offload_with_hook
    from diffusers import ModularPipeline
    from diffusers.hooks import apply_group_offloading
    from diffusers.utils.export_utils import encode_video

    if not torch.cuda.is_available():
        raise RuntimeError("VDN-H3 requires a CUDA GPU. Choose Wan on this machine.")
    keyframes = {}
    references = []
    if args.refs or args.reference_video or args.reference_audio:
        validate_reference_media(args)
        from diffusers.modular_pipelines.minimax_h3.references import (
            MiniMaxH3ImageReference, MiniMaxH3VideoReference, MiniMaxH3AudioReference,
        )
        for path in args.refs:
            references.append(MiniMaxH3ImageReference.from_file(path))
        if args.reference_video:
            references.append(MiniMaxH3VideoReference.from_file(args.reference_video))
        if args.reference_audio:
            references.append(MiniMaxH3AudioReference.from_file(args.reference_audio))
    if args.first or args.last:
        from diffusers.utils import load_image
        if args.first:
            keyframes["image"] = load_image(args.first)
        if args.last:
            keyframes["last_image"] = load_image(args.last)

    mode = "ref2va" if references else "fl2va" if keyframes else "t2va"
    # Native Ref2VA conditioning requires transformer_ref weights from the
    # original MiniMax H3 release; OpenVDN's hybrid checkpoint is t2va/fl2va.
    # Never silently treat original reference weights as a hybrid VDN model.
    checkpoint = REF_MODEL_REPOSITORY if references else MODEL_REPOSITORY
    print(json.dumps({"phase": "loading", "model": checkpoint}), flush=True)
    print(json.dumps({"phase": "conditioning", "workflow": mode,
                      "images": len(args.refs), "videos": bool(args.reference_video),
                      "audio": bool(args.reference_audio),
                      "hybridVDN": not references}), flush=True)
    pipe = ModularPipeline.from_pretrained(checkpoint, workflow=mode)
    denoiser_partition = "transformer_ref" if references else "transformer"
    if args.int8:
        # The documented H3 consumer-GPU recipe quantizes BOTH large models
        # during loading. Quantizing just the transformer after loading all
        # BF16 weights still requires well over 120 GB of host memory.
        from diffusers import MiniMaxH3Transformer3DModel, TorchAoConfig
        from transformers import Qwen3VLForConditionalGeneration
        from transformers import TorchAoConfig as TransformersTorchAoConfig
        from torchao.quantization import Int8WeightOnlyConfig
        pipe.update_components(**{
            denoiser_partition: MiniMaxH3Transformer3DModel.from_pretrained(
                checkpoint, subfolder=denoiser_partition, dtype=torch.bfloat16,
                quantization_config=TorchAoConfig(
                    Int8WeightOnlyConfig(version=2),
                    modules_to_not_convert=[
                        "proj_in", "audio_proj_in", "context_embedder",
                        "time_embedder", "time_proj", "token_refiner",
                        "norm_out", "proj_out", "audio_proj_out",
                    ],
                ),
                low_cpu_mem_usage=False,
            ),
            "text_encoder": Qwen3VLForConditionalGeneration.from_pretrained(
                checkpoint, subfolder="text_encoder", dtype=torch.bfloat16,
                quantization_config=TransformersTorchAoConfig(
                    Int8WeightOnlyConfig(version=2),
                    modules_to_not_convert=[
                        "model.visual", "model.language_model.embed_tokens",
                        "model.language_model.norm", "lm_head",
                    ],
                ),
            ),
        })
    kwargs = {"trust_remote_code": True, "dtype": torch.bfloat16}
    if args.fp8:
        kwargs["fp8"] = {denoiser_partition: True}
    pipe.load_components(**kwargs)
    denoiser = getattr(pipe, denoiser_partition, None)
    if denoiser is None:
        raise RuntimeError("Selected VDN weights do not contain the required "
                           "Ref2VA transformer partition; refusing unconditioned fallback")
    for lora in args.lora:
        if not lora.endswith(".safetensors"):
            raise RuntimeError("LoRA must be a safetensors file")
        if not hasattr(pipe, "load_lora_weights"):
            raise RuntimeError("Installed Diffusers lacks native H3 LoRA loading")
        pipe.load_lora_weights(lora)
    if args.int8:
        denoiser.requires_grad_(False)
        pipe.text_encoder.requires_grad_(False)
        print(json.dumps({"phase": "quantization", "precision": "int8_weight_only",
                          "models": [denoiser_partition, "text_encoder"],
                          "loadTimeQuantization": True}), flush=True)
    if any(getattr(pipe, name, None) is None for name in
           ("text_encoder", "vae", "audio_vae")):
        raise RuntimeError("An OpenVDN component did not load successfully.")

    device = torch.device(args.device)
    text_stream_kwargs = {
        "onload_device": device, "offload_device": "cpu",
        "offload_type": "leaf_level", "use_stream": True,
    }
    if args.disk_offload:
        text_path = Path(args.disk_offload_path) / "text_encoder"
        text_path.mkdir(parents=True, exist_ok=True)
        text_stream_kwargs["offload_to_disk_path"] = str(text_path)
    # Stream the language-model layers; Qwen's visual encoder is invoked by
    # the same real Diffusers text-encoding block.
    apply_group_offloading(pipe.text_encoder.model, **text_stream_kwargs)
    _, vae = cpu_offload_with_hook(pipe.vae, execution_device=device)
    cpu_offload_with_hook(pipe.audio_vae, execution_device=device,
                          prev_module_hook=vae)

    def decoder_back(module, inputs):
        vae.offload()
    denoiser.register_forward_pre_hook(decoder_back)
    if args.offload_dit:
        stream_kwargs = {
            "onload_device": device, "offload_device": "cpu",
            "offload_type": "block_level",
            "num_blocks_per_group": 1, "use_stream": True,
        }
        if args.disk_offload_path:
            dit_path = Path(args.disk_offload_path) / "denoiser"
            dit_path.mkdir(parents=True, exist_ok=True)
            stream_kwargs["offload_to_disk_path"] = str(dit_path)
        apply_group_offloading(denoiser, **stream_kwargs)
    else:
        denoiser.to(device)

    print(json.dumps({"phase": "sampling", "steps": args.steps,
                      "frames": args.frames}), flush=True)
    if args.two_pass:
        sample = native_two_pass(pipe, args, torch, device, keyframes, references)
    else:
        with native_sampler_recovery(args, "single", pipe, model_identity=checkpoint):
            sample = pipe(
            prompt=args.prompt, num_frames=args.frames,
            num_inference_steps=args.steps + 1,
            generator=torch.Generator(device).manual_seed(args.seed),
            output=["videos", "audio", "sampling_rate"],
            **({"references": references} if references else keyframes),
            )
    videos, audio, sample_rate = (sample["videos"], sample["audio"],
                                 sample["sampling_rate"])

    frames = torch.from_numpy(np.stack([np.asarray(frame) for frame in videos[0]]))
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_suffix(".partial.mp4")
    if temporary.exists():
        temporary.unlink()
    print(json.dumps({"phase": "encoding"}), flush=True)
    encode_video(frames, fps=FPS, output_path=str(temporary),
                 audio=audio[0].float().cpu(), audio_sample_rate=sample_rate)
    if not temporary.is_file() or temporary.stat().st_size < 32:
        raise RuntimeError("OpenVDN produced no video bytes")
    os.replace(temporary, output)
    clear_native_sampler_stage(args, "refine" if args.two_pass else "single")
    print(json.dumps({"phase": "completed", "path": str(output),
                      "frames": len(videos[0]), "fps": FPS}), flush=True)


if __name__ == "__main__":
    args = parse_args()
    if not args.self_test and not args.probe:
        try:
            run(args)
        except Exception as error:
            print(json.dumps({"phase": "failed", "reason": str(error)}),
                  file=sys.stderr, flush=True)
            raise

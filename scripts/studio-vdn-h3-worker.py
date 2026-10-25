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

MODEL_REPOSITORY = "OpenVDN/vdn-minimax-h3"
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
    parser.add_argument("--internal-attempt", choices=("resident", "stream_cpu", "stream_disk", "int8_cpu", "fp8_cpu"), default="")
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
    if args.device != "cuda":
        parser.error("This worker currently supports CUDA only; use native Wan for macOS")
    if (args.first or args.last) and (args.refs or args.reference_video or args.reference_audio):
        parser.error("Keyframe and Ref2VA conditioning cannot be combined in one inference")
    if args.reference_audio and not (args.refs or args.reference_video):
        parser.error("Ref2VA audio requires an image or video reference")
    if len(args.refs) > 9 or len(args.lora) > 4:
        parser.error("Ref2VA supports at most nine image references and four LoRA adapters")
    for path in (args.first, args.last, *args.refs, args.reference_video, args.reference_audio, *args.lora):
        if path and (not Path(path).is_file() or Path(path).stat().st_size > (256 * 1024 * 1024 if path == args.reference_video else 64 * 1024 * 1024)):
            parser.error("Reference/keyframe must be an existing bounded local file")
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
    disk_ok = disk_free >= 96 * GiB
    cpu_ok = host_ram is None or host_ram >= 16 * GiB
    plans = []
    def add(tier):
        if tier not in plans:
            plans.append(tier)
    if mode == "resident":
        add("resident")
    elif mode == "stream_cpu":
        add("stream_cpu")
    elif mode == "stream_disk":
        if not disk_ok:
            raise ValueError("Disk streaming needs at least 96 GiB of available space")
        add("stream_disk")
    else:
        if not offload and free_vram >= 44 * GiB:
            add("resident")
        if cpu_ok:
            add("stream_cpu")
        if torchao_available and host_ram is not None and host_ram >= 48 * GiB:
            add("int8_cpu")
        if capability >= 9:
            add("fp8_cpu")
        if disk_ok:
            add("stream_disk")
    if not plans:
        raise ValueError("Insufficient host RAM/disk for selected VDN memory modes")
    if fp8 and mode == "auto":
        plans = ["fp8_cpu"] + [tier for tier in plans if tier != "fp8_cpu"]
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
        args.offload_dit = args.internal_attempt != "resident"
        args.disk_offload = args.internal_attempt == "stream_disk"
        args.int8 = args.internal_attempt == "int8_cpu"
        args.fp8 = args.internal_attempt == "fp8_cpu"
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
                        ("--reference-audio", args.reference_audio)):
        if value:
            original.extend([flag, value])
    if args.refs:
        original.extend(["--refs", *args.refs])
    for lora in args.lora:
        original.extend(["--lora", lora])
    for tier in plans:
        argv = [sys.executable, "-u", *original, "--internal-attempt", tier]
        if tier == "fp8_cpu":
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

    print(json.dumps({"phase": "loading", "model": MODEL_REPOSITORY}), flush=True)
    mode = "ref2va" if references else "fl2va" if keyframes else "t2va"
    print(json.dumps({"phase": "conditioning", "workflow": mode,
                      "images": len(args.refs), "videos": bool(args.reference_video),
                      "audio": bool(args.reference_audio)}), flush=True)
    pipe = ModularPipeline.from_pretrained(MODEL_REPOSITORY, workflow=mode)
    kwargs = {"trust_remote_code": True, "torch_dtype": torch.bfloat16}
    if args.fp8:
        kwargs["fp8"] = {"transformer_ref" if references else "transformer": True}
    pipe.load_components(**kwargs)
    denoiser = getattr(pipe, "transformer_ref" if references else "transformer", None)
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
        try:
            from torchao.quantization import Int8WeightOnlyConfig, quantize_
        except ImportError as error:
            raise RuntimeError(
                "Consumer CUDA INT8 requires torchao in the selected VDN environment"
            ) from error
        print(json.dumps({"phase": "quantizing", "precision": "int8_weight_only"}),
              flush=True)
        quantize_(denoiser, Int8WeightOnlyConfig())
    if any(getattr(pipe, name, None) is None for name in
           ("text_encoder", "vae", "audio_vae")):
        raise RuntimeError("An OpenVDN component did not load successfully.")

    device = torch.device(args.device)
    apply_group_offloading(pipe.text_encoder, onload_device=device,
                           offload_device="cpu", offload_type="leaf_level",
                           use_stream=True)
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
            stream_kwargs["offload_to_disk_path"] = args.disk_offload_path
        apply_group_offloading(denoiser, **stream_kwargs)
    else:
        denoiser.to(device)

    print(json.dumps({"phase": "sampling", "steps": args.steps,
                      "frames": args.frames}), flush=True)
    videos, audio, sample_rate = pipe(
        prompt=args.prompt,
        num_frames=args.frames,
        num_inference_steps=args.steps + 1,
        generator=torch.Generator(device).manual_seed(args.seed),
        output=["videos", "audio", "sampling_rate"],
        **({"references": references} if references else keyframes),
    ).values()

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

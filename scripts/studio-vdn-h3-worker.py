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
    parser.add_argument("--steps", type=int, default=8)
    parser.add_argument("--frames", type=int, default=141)
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--fp8", action="store_true")
    parser.add_argument("--offload-dit", action="store_true")
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
    for path in (args.first, args.last):
        if path and (not Path(path).is_file() or Path(path).stat().st_size > 32 * 1024 * 1024):
            parser.error("Keyframe must exist and be smaller than 32 MB")
    return args


def choose_memory_policy(free_bytes, *, fp8, offload, fp8_supported):
    """Choose an actual fallback without requiring a GPU in unit tests.

    Offloading is preferable to an optimistic resident load below 40 GiB.
    FP8 is only considered on devices whose capability was checked at runtime.
    """
    if free_bytes < 0:
        raise ValueError("CUDA free memory must be nonnegative")
    return {
        "offload": bool(offload or free_bytes < 40 * 1024 ** 3),
        "fp8": bool(fp8 and fp8_supported),
    }


def is_cuda_oom(error):
    """Restrict retry to CUDA allocation failures, not arbitrary model errors."""
    return "out of memory" in str(error).lower() and (
        "cuda" in str(error).lower() or "cublas" in str(error).lower()
    )


def run(args):
    import gc
    import torch

    if not torch.cuda.is_available():
        raise RuntimeError("VDN-H3 requires a CUDA GPU")
    free_bytes, _ = torch.cuda.mem_get_info()
    major, _ = torch.cuda.get_device_capability()
    if args.fp8 and major < 9:
        raise RuntimeError("VDN FP8 requires a CUDA device with compute capability 9.0+")
    policy = choose_memory_policy(
        free_bytes, fp8=args.fp8, offload=args.offload_dit,
        fp8_supported=major >= 9,
    )
    args.offload_dit = policy["offload"]
    print(json.dumps({"phase": "memory_plan", "freeVramBytes": free_bytes,
                      "offloadDit": args.offload_dit, "fp8": args.fp8}),
          flush=True)
    try:
        return _run_once(args)
    except RuntimeError as error:
        if not is_cuda_oom(error):
            raise
        # Rebuild all model components instead of trying to reuse a model
        # after a partially completed CUDA operation.
        if not args.offload_dit:
            args.offload_dit = True
            fallback = "cpu_block_offload"
        elif not args.fp8 and major >= 9:
            args.fp8 = True
            fallback = "fp8_with_cpu_block_offload"
        else:
            raise RuntimeError(
                "VDN exhausted CUDA memory with the available offload/precision "
                "modes; reduce the clip size or use a larger GPU"
            ) from error
        gc.collect()
        torch.cuda.empty_cache()
        print(json.dumps({"phase": "oom_retry", "strategy": fallback}),
              flush=True)
        return _run_once(args)


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
    if args.first or args.last:
        from diffusers.utils import load_image
        if args.first:
            keyframes["image"] = load_image(args.first)
        if args.last:
            keyframes["last_image"] = load_image(args.last)

    print(json.dumps({"phase": "loading", "model": MODEL_REPOSITORY}), flush=True)
    pipe = ModularPipeline.from_pretrained(
        MODEL_REPOSITORY, workflow="fl2va" if keyframes else "t2va")
    kwargs = {"trust_remote_code": True, "torch_dtype": torch.bfloat16}
    if args.fp8:
        kwargs["fp8"] = {"transformer": True}
    pipe.load_components(**kwargs)
    if any(getattr(pipe, name, None) is None for name in
           ("text_encoder", "vae", "audio_vae", "transformer")):
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
    pipe.transformer.register_forward_pre_hook(decoder_back)
    if args.offload_dit:
        apply_group_offloading(pipe.transformer, onload_device=device,
                               offload_device="cpu", offload_type="block_level",
                               num_blocks_per_group=1, use_stream=True)
    else:
        pipe.transformer.to(device)

    print(json.dumps({"phase": "sampling", "steps": args.steps,
                      "frames": args.frames}), flush=True)
    videos, audio, sample_rate = pipe(
        prompt=args.prompt,
        num_frames=args.frames,
        num_inference_steps=args.steps + 1,
        generator=torch.Generator(device).manual_seed(args.seed),
        output=["videos", "audio", "sampling_rate"],
        **keyframes,
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

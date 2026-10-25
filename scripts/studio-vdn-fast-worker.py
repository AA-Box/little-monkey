#!/usr/bin/env python3
"""Authorized high-performance VDN-H3 native and SGLang local backends.

No model weights, app tokens or license grants are bundled. Little Monkey's
Rust host MUST authorize MiniMax model use before launching this executable.
Modes:
  optimized_single: official OpenVDN infer.py (tuned FP8/BF16)
  optimized_ulysses: official OpenVDN infer_ulysses.py via torchrun (8 H200/B200)
  sglang: OpenVDN-weight SGLang server on loopback /v1/videos

No FreeVideo or ComfyUI process, no shell=True, no user-supplied network URL.
"""
import argparse
import json
import os
import shutil
import tempfile
from pathlib import Path
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

MODEL = "OpenVDN/vdn-minimax-h3"
MAX_OUTPUT = 256 * 1024 * 1024
CHILD = None
REMOTE_JOB = None
REMOTE_PORT = None


def emit(phase, **data):
    print(json.dumps({"phase": phase, **data}), flush=True)


def stop_children():
    global CHILD
    if CHILD is not None and CHILD.poll() is None:
        if os.name == "posix":
            os.killpg(CHILD.pid, signal.SIGTERM)
        else:
            CHILD.terminate()
        try:
            CHILD.wait(timeout=10)
        except subprocess.TimeoutExpired:
            if os.name == "posix":
                os.killpg(CHILD.pid, signal.SIGKILL)
            else:
                CHILD.kill()
            CHILD.wait()


def shutdown(sig, frame):
    stop_children()
    if REMOTE_JOB is not None and REMOTE_PORT is not None:
        try:
            request_json("DELETE", f"/v1/videos/{REMOTE_JOB}", port=REMOTE_PORT)
        except (OSError, ValueError):
            pass
    raise SystemExit(128 + sig)


if hasattr(signal, "SIGTERM"):
    signal.signal(signal.SIGTERM, shutdown)


def arguments(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--probe", action="store_true")
    p.add_argument("--plan", action="store_true", help="Print the real commands without executing")
    p.add_argument("--backend", choices=("optimized_single", "optimized_ulysses", "sglang"),
                   default="optimized_single")
    p.add_argument("--repo-root", default="")
    p.add_argument("--checkpoint-root", default="")
    p.add_argument("--port", type=int, default=30010)
    p.add_argument("--gpu-class", choices=("h200", "b200"), default="h200")
    p.add_argument("--gpus", type=int, default=8)
    p.add_argument("--prompt", default="")
    p.add_argument("--first", default="")
    p.add_argument("--last", default="")
    p.add_argument("--refs", nargs="+", default=[])
    p.add_argument("--reference-video", default="")
    p.add_argument("--output", default="")
    p.add_argument("--steps", type=int, default=8)
    p.add_argument("--frames", type=int, default=141)
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--fp8", action="store_true")
    p.add_argument("--device", default="cuda")
    args = p.parse_args(argv)
    if args.self_test:
        emit("ready", backend=args.backend, model=MODEL)
        return args
    if not 1 <= args.port <= 65535:
        p.error("SGLang loopback port must be 1..65535")
    if not args.probe:
        if (not args.prompt.strip() or len(args.prompt.encode("utf-8")) > 8192
                or not args.output or not Path(args.output).is_absolute()):
            p.error("A nonempty prompt <=8192 bytes and an absolute app-owned output are required")
        if args.steps != 8 or not 120 <= args.frames <= 360 or (args.frames - 5) % 17:
            p.error("Optimized VDN-H3 requires 8 evaluations and 17n+5 frames, 120..360")
        if args.seed < 0:
            p.error("Seed must be nonnegative")
        if args.device != "cuda":
            p.error("Optimized VDN-H3 requires CUDA")
        if args.reference_video:
            pth = Path(args.reference_video)
            if not pth.is_file() or pth.stat().st_size > MAX_OUTPUT:
                p.error("Reference video must exist and be at most 256 MB")
        if len(args.refs) > 8:
            p.error("At most eight reference images are supported")
        if args.backend == "sglang" and (args.refs or args.reference_video):
            p.error("This SGLang VDN endpoint does not accept Ref2VA references")
        for path in (args.first, args.last, *args.refs):
            if path and (not Path(path).is_file() or Path(path).stat().st_size > 32 * 1024 * 1024):
                p.error("Keyframe must be an existing file smaller than 32MB")
    if args.backend == "optimized_ulysses" and args.gpus != 8:
        p.error("Native Ulysses shipped profiles are for exactly 8 H200/B200 GPUs")
    return args


def source_tree(args):
    root = Path(args.repo_root).expanduser().resolve(strict=True)
    if not root.is_dir():
        raise ValueError("OpenVDN repository root is not a directory")
    files = ["src/inference/encode_prompt.py", "src/inference/encode_keyframes.py",
             "src/inference/infer.py", "src/inference/infer_ulysses.py",
             "configs/inference/8nfe_tuned.yaml",
             "configs/inference/8nfe_tuned_fp8.yaml",
             "configs/inference/8nfe_tuned_fp8_ulysses_h200.yaml",
             "configs/inference/8nfe_tuned_fp8_ulysses_b200.yaml"]
    for relative in files:
        entry = (root / relative).resolve(strict=True)
        if not entry.is_file() or not entry.is_relative_to(root):
            raise ValueError("Missing or unsafe official OpenVDN file: " + relative)
    ckpts = Path(args.checkpoint_root).expanduser().resolve(strict=True)
    if not ckpts.is_dir() or not (ckpts / "stage-dmd-step-250" / "model_spec.json").is_file():
        raise ValueError("VDN 8-step checkpoint is not installed in checkpoint root")
    return root, ckpts


def frames_from_reference_video(source, temporary_dir, max_frames=4):
    """Decode bounded real frames and pass them through native Ref2VA encoder.

    Video-to-reference here is visual frame conditioning; OpenVDN does not
    expose arbitrary video-motion latent conditioning.
    """
    import av
    from PIL import Image
    path = Path(source)
    if path.stat().st_size > MAX_OUTPUT:
        raise ValueError("Reference video exceeds 256 MB")
    with av.open(str(path)) as container:
        stream = next((v for v in container.streams if v.type == "video"), None)
        if stream is None:
            raise ValueError("Reference video contains no video stream")
        duration = float(stream.duration * stream.time_base) if stream.duration else 0
        if duration <= 0 or duration > 90:
            raise ValueError("Reference video duration must be 0-90 seconds")
        target_times = [duration * i / max_frames for i in range(max_frames)]
        selected = []
        target = 0
        for n, frame in enumerate(container.decode(video=stream.index)):
            if n > 3000:
                raise ValueError("Reference video decoding exceeds 3000 frames")
            seconds = float(frame.pts * frame.time_base) if frame.pts is not None else n / 24
            if target < len(target_times) and seconds >= target_times[target]:
                output = Path(temporary_dir) / f"ref-video-{target:02d}.png"
                Image.fromarray(frame.to_ndarray(format="rgb24")).save(output)
                selected.append(str(output))
                target += 1
                if target == len(target_times):
                    break
    if not selected:
        raise ValueError("Reference video decoded no frames")
    return selected


def native_commands(args, python_executable=sys.executable):
    """Exact official encoder then optimized renderer command vectors. No shell."""
    root, ckpts = source_tree(args)
    out = Path(args.output)
    cache = out.with_suffix(".prompt.pt")
    encoder = "src/inference/encode_keyframes.py" if args.first or args.last or args.refs else "src/inference/encode_prompt.py"
    prepare = [python_executable, str(root / encoder),
               "--prompt", args.prompt, "--out", str(cache)]
    if args.first:
        prepare.extend(["--first", args.first])
    if args.last:
        prepare.extend(["--last", args.last])
    if args.refs:
        prepare.extend(["--refs", *args.refs])
    if args.first or args.last or args.refs:
        prepare.extend(["--vae_root", str(ckpts / "h3-base")])
    if args.backend == "optimized_ulysses":
        config = f"8nfe_tuned_fp8_ulysses_{args.gpu_class}.yaml"
        render = [python_executable, "-m", "torch.distributed.run",
                  "--standalone", "--nproc_per_node=8",
                  str(root / "src/inference/infer_ulysses.py")]
    else:
        config = "8nfe_tuned_fp8.yaml" if args.fp8 else "8nfe_tuned.yaml"
        render = [python_executable, str(root / "src/inference/infer.py")]
    render.extend([
        "--config", str(root / "configs/inference" / config),
        "checkpoint=" + str(ckpts / "stage-dmd-step-250"),
        "base_source=" + str(ckpts / "h3-base"),
        "vae_source=" + str(ckpts / "h3-base"),
        "render.prompt_file=" + str(cache),
        "render.out=" + str(out),
        "render.num_steps=8",
        "render.num_frames=" + str(args.frames),
        "render.seed=" + str(args.seed),
        "render.record=true",
    ])
    return root, cache, prepare, render


def run_command(argv, directory):
    global CHILD
    CHILD = subprocess.Popen(argv, cwd=str(directory), stdin=subprocess.DEVNULL,
                             start_new_session=(os.name == "posix"))
    try:
        code = CHILD.wait()
    finally:
        CHILD = None
    if code != 0:
        raise RuntimeError(f"Official OpenVDN stage failed with exit code {code}")


def inspect_cuda(args):
    import torch
    if not torch.cuda.is_available():
        raise RuntimeError("CUDA is unavailable in the selected Python environment")
    count = torch.cuda.device_count()
    if args.backend == "optimized_ulysses":
        if count < 8:
            raise RuntimeError("Native Ulysses requires 8 visible CUDA GPUs")
        for idx in range(8):
            name = torch.cuda.get_device_name(idx).lower()
            if args.gpu_class not in name:
                raise RuntimeError(f"GPU {idx} is {name}, not the selected {args.gpu_class.upper()} class")
    else:
        count = min(1, count)
    return {"device": torch.cuda.get_device_name(0),
            "computeCapability": ".".join(map(str, torch.cuda.get_device_capability(0))),
            "vramBytes": torch.cuda.get_device_properties(0).total_memory,
            "visibleGpuCount": torch.cuda.device_count(),
            "fp8Supported": torch.cuda.get_device_capability(0)[0] >= 9,
            "torch": torch.__version__}


def request_json(method, path, body=None, port=30010):
    """Fixed loopback origin, no redirects or arbitrary network access."""
    url = f"http://127.0.0.1:{port}{path}"
    payload = json.dumps(body).encode("utf-8") if body is not None else None
    req = urllib.request.Request(url, data=payload, method=method)
    req.add_header("Content-Type", "application/json")
    with urllib.request.build_opener(NoRedirect()).open(req, timeout=30) as resp:
        data = resp.read(512 * 1024 + 1)
        if len(data) > 512 * 1024:
            raise ValueError("SGLang response exceeds 512KB")
    return json.loads(data)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        raise ValueError("Unexpected SGLang redirect")


def serving_model(port):
    data = request_json("GET", "/v1/models", port=port)
    if not isinstance(data, dict):
        raise ValueError("SGLang did not return a model list")
    choices = [row.get("id") for row in data.get("data", []) if isinstance(row, dict)]
    if MODEL not in choices:
        raise ValueError("The local SGLang server is not serving OpenVDN/vdn-minimax-h3")
    return MODEL


def run_sglang(args):
    global REMOTE_JOB, REMOTE_PORT
    serving_model(args.port)
    seconds = args.frames / 24
    if not 5 <= seconds <= 15:
        raise ValueError("VDN SGLang target duration must be 5-15 seconds")
    conditions = []
    for frame_index, path in ((0, args.first), (-1, args.last)):
        if path:
            resolved = Path(path).resolve(strict=True)
            conditions.append({"type": "image", "uri": resolved.as_uri(),
                               "role": "keyframe", "frame_index": frame_index})
    body = {"model": MODEL, "task": "fl2va" if conditions else "t2va",
            "prompt": args.prompt, "seconds": seconds,
            "conditions": conditions,
            "target": {"short_edge": 768, "aspect_ratio": "auto" if conditions else "16:9",
                       "duration_seconds": seconds},
            "num_outputs_per_prompt": 1, "num_inference_steps": 8,
            "flow_shift": 12.0, "audio_flow_shift": 3.0, "seed": args.seed}
    info = request_json("POST", "/v1/videos", body, port=args.port)
    job = info.get("id")
    if not isinstance(job, str) or not (1 <= len(job) <= 128) or any(c not in "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-" for c in job):
        raise ValueError("SGLang returned an unsafe or empty video job identifier")
    REMOTE_JOB, REMOTE_PORT = job, args.port
    emit("sampling", backend="sglang", jobId=job)
    for _ in range(4 * 60 * 60 // 2):
        state = request_json("GET", "/v1/videos/" + job, port=args.port)
        status = state.get("status")
        if status == "completed":
            break
        if status in ("failed", "cancelled", "expired"):
            raise RuntimeError("SGLang video job ended: " + str(status))
        if status not in ("queued", "in_progress", "processing"):
            raise RuntimeError("Unexpected SGLang job status: " + str(status))
        time.sleep(2)
    else:
        raise TimeoutError("SGLang exceeded four-hour video rendering limit")
    req = urllib.request.Request(f"http://127.0.0.1:{args.port}/v1/videos/{job}/content")
    with urllib.request.build_opener(NoRedirect()).open(req, timeout=180) as response:
        target = Path(args.output)
        tmp = target.with_suffix(".partial.mp4")
        with tmp.open("wb") as stream:
            size = 0
            while True:
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                size += len(chunk)
                if size > MAX_OUTPUT:
                    raise ValueError("SGLang output exceeded 256 MB")
                stream.write(chunk)
        if size < 32:
            raise ValueError("SGLang returned an empty video")
        tmp.replace(target)
    REMOTE_JOB, REMOTE_PORT = None, None


def main(args):
    if args.backend == "sglang":
        if args.probe:
            serving_model(args.port)
            emit("ready", backend="sglang", model=MODEL, port=args.port)
            return
        run_sglang(args)
    else:
        root, ckpts = source_tree(args)
        if args.plan:
            _, _, enc, render = native_commands(args)
            emit("plan", backend=args.backend, stages=[enc, render])
            return
        if args.probe:
            caps = inspect_cuda(args)
            emit("ready", backend=args.backend, model=MODEL, **caps)
            return
        caps = inspect_cuda(args)
        if args.fp8 and not caps["fp8Supported"]:
            raise RuntimeError("FP8 tuned runner requires Hopper or newer GPU")
        reference_dir = None
        if args.reference_video:
            reference_dir = tempfile.mkdtemp(prefix="vdn-video-refs-", dir=Path(args.output).parent)
            try:
                args.refs.extend(frames_from_reference_video(args.reference_video, reference_dir))
            except Exception:
                shutil.rmtree(reference_dir, ignore_errors=True)
                raise
        if len(args.refs) > 8:
            if reference_dir: shutil.rmtree(reference_dir, ignore_errors=True)
            raise ValueError("Combined image/video references exceed eight frames")
        _, cache, encoder, renderer = native_commands(args)
        emit("encoding", backend=args.backend)
        try:
            run_command(encoder, root)
            emit("sampling", backend=args.backend, gpuCount=args.gpus if args.backend == "optimized_ulysses" else 1)
            run_command(renderer, root)
        finally:
            cache.unlink(missing_ok=True)
            if reference_dir:
                shutil.rmtree(reference_dir, ignore_errors=True)
    output = Path(args.output)
    if not output.is_file() or output.stat().st_size < 32:
        raise RuntimeError("VDN optimized backend did not produce a nonempty MP4")
    with output.open("rb") as stream:
        if stream.read(8)[4:8] != b"ftyp":
            raise RuntimeError("VDN optimized backend produced invalid MP4 bytes")
    emit("completed", backend=args.backend, bytes=output.stat().st_size)


if __name__ == "__main__":
    args = arguments()
    if not args.self_test:
        try:
            main(args)
        except Exception as exc:
            print(json.dumps({"phase": "failed", "reason": str(exc)}), file=sys.stderr, flush=True)
            raise

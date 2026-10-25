#!/usr/bin/env python3
"""Optional installed FreeVideo adaptive runtime for genuine low-memory VDN.

This adapter never downloads or embeds FreeVideo. The user selects a Python
environment with the Apache-licensed FreeVideo runtime installed and a
prepared model cache. MiniMax H3 rights are checked by the Little Monkey Rust
host before spawn, throughout execution and before Gallery publication.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import sys

CHILD = None


def args_for_worker(argv=None):
    p = argparse.ArgumentParser()
    p.add_argument("--self-test", action="store_true")
    p.add_argument("--probe", action="store_true")
    p.add_argument("--plan", action="store_true")
    p.add_argument("--cache", default="")
    p.add_argument("--prompt", default="")
    p.add_argument("--output", default="")
    p.add_argument("--steps", type=int, default=8)
    p.add_argument("--frames", type=int, default=141)
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--first", default="")
    p.add_argument("--last", default="")
    p.add_argument("--refs", nargs="+", default=[])
    p.add_argument("--reference-video", default="")
    p.add_argument("--reference-audio", default="")
    p.add_argument("--lora", action="append", default=[])
    p.add_argument("--fp8", action="store_true")
    p.add_argument("--offload-dit", action="store_true")
    p.add_argument("--device", default="cuda")
    a = p.parse_args(argv)
    if a.self_test:
        return a
    if not a.cache or not Path(a.cache).exists():
        p.error("Select an existing, prepared FreeVideo model cache")
    if a.probe:
        return a
    if not a.output or not Path(a.output).is_absolute():
        p.error("An app-owned absolute output path is required")
    if not a.prompt.strip() or len(a.prompt.encode("utf-8")) > 8192:
        p.error("Prompt must contain 1-8192 UTF-8 bytes")
    if not 1 <= a.steps <= 32 or a.seed < 0:
        p.error("Invalid FreeVideo steps or seed")
    if not 120 <= a.frames <= 360 or (a.frames - 5) % 17:
        p.error("FreeVideo clip must use a 17n+5 frame grid")
    if len(a.refs) > 8 or len(a.lora) > 4:
        p.error("Too many reference images or LoRA assets")
    if (a.first or a.last) and (a.refs or a.reference_video or a.reference_audio):
        p.error("FreeVideo cannot mix keyframes with reference conditioning")
    for item in [a.first, a.last, *a.refs, a.reference_video, a.reference_audio, *a.lora]:
        if item and (not Path(item).is_file() or Path(item).stat().st_size <= 0):
            p.error("All media and LoRA paths must be local nonempty files")
    return a


def media_request(args):
    value = {"version": 1, "references": [], "loras": []}
    if args.first:
        value["first"] = args.first
    if args.last:
        value["last"] = args.last
    for image in args.refs:
        value["references"].append({"path": image, "kind": "image"})
    if args.reference_video:
        value["references"].append({"path": args.reference_video, "kind": "video"})
    if args.reference_audio:
        value["references"].append({"path": args.reference_audio, "kind": "audio"})
    for lora in args.lora:
        if Path(lora).suffix.lower() != ".safetensors":
            raise ValueError("FreeVideo LoRA requires a safetensors file")
        value["loras"].append({"path": lora, "strength": 1.0})
    return value


def command(args, prompt_path, media_path):
    return [
        sys.executable, "-m", "freevideo_engine", "generate",
        "--prompt-file", str(prompt_path),
        "--cache", str(Path(args.cache).resolve()),
        "--media", str(media_path),
        "--out", args.output,
        "--frames", str(args.frames),
        "--seed", str(args.seed),
        "--base-steps", str(args.steps),
        "--resource-retries", "2",
        "--two-pass",
    ]


def stop(signum, frame):
    global CHILD
    if CHILD is not None and CHILD.poll() is None:
        try:
            if os.name == "posix":
                os.killpg(CHILD.pid, signal.SIGTERM)
            else:
                CHILD.terminate()
        except ProcessLookupError:
            pass
    raise SystemExit(128 + signum)


if hasattr(signal, "SIGTERM"):
    signal.signal(signal.SIGTERM, stop)


def run(args):
    global CHILD
    if args.self_test:
        print(json.dumps({"status": "ready_for_dependency_probe",
                          "backend": "freevideo_adaptive"}), flush=True)
        return
    if importlib.util.find_spec("freevideo_engine") is None:
        raise RuntimeError("Install the optional FreeVideo package into the selected Python environment")
    if args.probe:
        print(json.dumps({"status": "ready", "backend": "freevideo_adaptive",
                          "model": "OpenVDN/vdn-minimax-h3",
                          "adaptive": True, "requiresLicensedWeights": True}), flush=True)
        return
    out = Path(args.output)
    out.parent.mkdir(parents=True, exist_ok=True)
    prompt = out.with_suffix(".vdn-prompt.txt")
    media = out.with_suffix(".vdn-media.json")
    prompt.write_text(args.prompt, encoding="utf-8")
    media.write_text(json.dumps(media_request(args)), encoding="utf-8")
    argv = command(args, prompt, media)
    if args.plan:
        print(json.dumps({"phase": "plan", "argv": argv}), flush=True)
        prompt.unlink(missing_ok=True)
        media.unlink(missing_ok=True)
        return
    print(json.dumps({"phase": "loading", "backend": "freevideo_adaptive"}), flush=True)
    try:
        CHILD = subprocess.Popen(argv, stdin=subprocess.DEVNULL,
                                 start_new_session=os.name == "posix")
        code = CHILD.wait()
        CHILD = None
        if code != 0:
            raise RuntimeError(f"FreeVideo adaptive worker exited with status {code}")
        if not out.is_file() or not 32 <= out.stat().st_size <= 256 * 1024 * 1024:
            raise RuntimeError("FreeVideo produced no valid-sized MP4 file")
        with out.open("rb") as inp:
            if inp.read(8)[4:8] != b"ftyp":
                raise RuntimeError("FreeVideo output lacks MP4 ftyp")
        print(json.dumps({"phase": "completed", "backend": "freevideo_adaptive"}), flush=True)
    finally:
        prompt.unlink(missing_ok=True)
        media.unlink(missing_ok=True)


if __name__ == "__main__":
    try:
        run(args_for_worker())
    except Exception as exc:
        print(json.dumps({"phase": "failed", "reason": str(exc)}),
              file=sys.stderr, flush=True)
        raise

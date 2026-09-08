#!/usr/bin/env python3
"""Signed MLX runtime entrypoint selecting the safest available engine.

The product always launches this entrypoint. On an M5+ / macOS 26+ host with
the exact Qwen3.6 MLX affine-Q4 layout and a packaged Lily binary, it execs the
managed Lily adapter. Every other case execs the normal MLX service unchanged.
No user PATH executable, custom provider, or manually started process is used.
"""

from __future__ import annotations

import argparse
import json
import os
import platform
import re
import subprocess
import sys
from pathlib import Path

MAX_CONFIG_BYTES = 4 * 1024 * 1024


def _model_is_lily_candidate(model_path: Path) -> bool:
    try:
        with (model_path / "config.json").open("rb") as handle:
            config = json.loads(handle.read(MAX_CONFIG_BYTES))
    except (OSError, ValueError):
        return False
    quant = config.get("quantization") or config.get("quantization_config") or {}
    bits = quant.get("bits")
    group = quant.get("group_size")
    mode = str(quant.get("mode") or "affine").lower()
    return (
        str(config.get("model_type")) == "qwen3_5_moe"
        and int(bits or 0) == 4
        and int(group or 0) == 64
        and mode == "affine"
        and isinstance(config.get("vision_config"), dict)
    )


def _macos_26_or_newer() -> bool:
    if sys.platform != "darwin" or platform.machine().lower() not in {"arm64", "aarch64"}:
        return False
    try:
        major = int((platform.mac_ver()[0] or "0").split(".", 1)[0])
    except ValueError:
        return False
    return major >= 26


def _apple_m_generation() -> int | None:
    """Conservatively identify Apple M-series generation without guessing GPU family."""
    try:
        output = subprocess.check_output(
            ["/usr/sbin/system_profiler", "SPHardwareDataType", "-json"],
            stderr=subprocess.DEVNULL,
            timeout=5,
            text=True,
        )
        data = json.loads(output)
        rows = data.get("SPHardwareDataType") or []
        chip = str(rows[0].get("chip_type") or rows[0].get("machine_name") or "") if rows else ""
    except (OSError, subprocess.SubprocessError, ValueError, TypeError, AttributeError):
        return None
    match = re.search(r"\bApple\s+M(\d+)\b", chip, flags=re.IGNORECASE)
    return int(match.group(1)) if match else None


def _lily_eligible(model_path: Path, lily_binary: Path) -> bool:
    if os.environ.get("LITTLE_MONKEY_DISABLE_LILY") == "1":
        return False
    generation = _apple_m_generation()
    return (
        lily_binary.is_file()
        and os.access(lily_binary, os.X_OK)
        and _macos_26_or_newer()
        and generation is not None
        and generation >= 5
        and _model_is_lily_candidate(model_path)
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Little Monkey managed MLX runtime router")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--model", required=True)
    args = parser.parse_args(argv)
    if args.host != "127.0.0.1":
        parser.error("--host must be 127.0.0.1")

    service_dir = Path(__file__).resolve().parent
    root = service_dir.parent
    python = root / "runtime/bin/python3"
    normal = service_dir / "mlx_server.py"
    managed_lily = service_dir / "lily_managed.py"
    lily = root / "bin/lily"
    model = Path(args.model).resolve()

    if _lily_eligible(model, lily):
        sys.stderr.write("mlx-runtime engine=lily eligibility=hardware+model\n")
        sys.stderr.flush()
        command = [
            str(python),
            str(managed_lily),
            "--host",
            args.host,
            "--port",
            str(args.port),
            "--model",
            str(model),
            "--lily-binary",
            str(lily),
            "--fallback-service",
            str(normal),
            "--python",
            str(python),
        ]
    else:
        sys.stderr.write("mlx-runtime engine=mlx eligibility=lily-unavailable\n")
        sys.stderr.flush()
        command = [
            str(python),
            str(normal),
            "--host",
            args.host,
            "--port",
            str(args.port),
            "--model",
            str(model),
        ]
    os.execv(command[0], command)
    return 127


if __name__ == "__main__":
    raise SystemExit(main())

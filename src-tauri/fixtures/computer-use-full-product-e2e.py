#!/usr/bin/env python3
"""Runs the real desktop app's frontend -> Tauri -> OS Computer Use golden.

The app itself writes the report only after its frontend dispatcher has called
the production Tauri commands. This wrapper starts the real fixture, waits for
that report, extracts the returned screenshot, and rejects incomplete evidence.
"""

import argparse
import base64
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def terminate(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], check=False, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    else:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--fixture", required=True, type=Path)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--timeout", type=int, default=900)
    args = parser.parse_args()
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.unlink(missing_ok=True)
    app_log_path = args.report.with_suffix(".app.log")
    app_log_path.unlink(missing_ok=True)
    frontend_log_path = args.report.with_suffix(".frontend.log")
    frontend_log_path.unlink(missing_ok=True)

    python = os.environ.get("COMPUTER_USE_FIXTURE_PYTHON", sys.executable)
    fixture = subprocess.Popen([python, str(args.fixture)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    repo = Path(__file__).resolve().parents[2]
    pnpm = shutil.which("pnpm") or shutil.which("pnpm.cmd") or "pnpm"
    app_command = os.environ.get("COMPUTER_USE_FULL_PRODUCT_COMMAND")
    frontend_command = os.environ.get("COMPUTER_USE_FULL_PRODUCT_FRONTEND_COMMAND")
    node = shutil.which("node") or shutil.which("node.exe") or "node"
    frontend = frontend_command.split() if frontend_command else [
        node,
        str(repo / "node_modules/vite/bin/vite.js"),
        "--host",
        "127.0.0.1",
        "--port",
        "1420",
        "--strictPort",
    ]
    config_path: Path | None = None
    if app_command is None:
        config_fd, config_name = tempfile.mkstemp(
            dir=repo / "src-tauri",
            prefix="little-monkey-full-product-",
            suffix=".json",
        )
        os.close(config_fd)
        config_path = Path(config_name)
        tauri_config = json.loads((repo / "src-tauri" / "tauri.conf.json").read_text(encoding="utf-8"))
        tauri_config["build"]["beforeDevCommand"] = ""
        tauri_config["build"]["devUrl"] = "http://127.0.0.1:1420"
        config_path.write_text(json.dumps(tauri_config), encoding="utf-8")
    command = app_command.split() if app_command else [
        pnpm,
        "tauri",
        "dev",
        "--no-watch",
        "--config",
        str(config_path),
    ]
    environment = os.environ.copy()
    environment.update({
        "COMPUTER_USE_FULL_PRODUCT_E2E": "1",
        "COMPUTER_USE_FULL_PRODUCT_REPORT": str(args.report),
        "VITE_COMPUTER_USE_FULL_PRODUCT_E2E": "1",
        "VITE_COMPUTER_USE_FIXTURE_PID": str(fixture.pid),
    })
    if os.name != "nt":
        environment.setdefault("GDK_BACKEND", "x11")
    frontend_log = frontend_log_path.open("w", encoding="utf-8")
    frontend_process = subprocess.Popen(
        frontend,
        cwd=repo,
        env=environment,
        stdout=frontend_log,
        stderr=subprocess.STDOUT,
        start_new_session=(os.name != "nt"),
    )
    frontend_deadline = time.monotonic() + 120
    while time.monotonic() < frontend_deadline:
        if frontend_process.poll() is not None:
            break
        try:
            with urllib.request.urlopen("http://127.0.0.1:1420/", timeout=2):
                break
        except (urllib.error.URLError, TimeoutError):
            time.sleep(1)
    else:
        frontend_log.flush()
        tail = frontend_log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
        print("frontend dev server output:", file=sys.stderr)
        print("\n".join(tail), file=sys.stderr)
        print("frontend dev server did not become ready", file=sys.stderr)
        terminate(frontend_process)
        frontend_log.close()
        terminate(fixture)
        if config_path:
            config_path.unlink(missing_ok=True)
        return 1
    if frontend_process.poll() is not None:
        frontend_log.flush()
        tail = frontend_log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
        print("frontend dev server output:", file=sys.stderr)
        print("\n".join(tail), file=sys.stderr)
        print(f"frontend dev server exited (exit={frontend_process.poll()})", file=sys.stderr)
        frontend_log.close()
        terminate(fixture)
        if config_path:
            config_path.unlink(missing_ok=True)
        return 1

    app_log = app_log_path.open("w", encoding="utf-8")
    app = subprocess.Popen(
        command,
        cwd=repo,
        env=environment,
        stdout=app_log,
        stderr=subprocess.STDOUT,
        start_new_session=(os.name != "nt"),
    )
    try:
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline and not args.report.is_file():
            if app.poll() is not None and not args.report.is_file():
                break
            time.sleep(1)
        if not args.report.is_file():
            app_log.flush()
            tail = app_log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
            if tail:
                print("full product app output:", file=sys.stderr)
                print("\n".join(tail), file=sys.stderr)
            print(f"full product app did not produce {args.report} (exit={app.poll()})", file=sys.stderr)
            return 1
        report = json.loads(args.report.read_text(encoding="utf-8"))
        required = {
            "status": "completed",
            "real_frontend_dispatcher": True,
            "task_coordinator": True,
            "real_tauri_ipc": True,
            "desktop_control_state": "production",
            "real_desktop_actions_executed": True,
            "state_verified": True,
            "screenshot_received_by_frontend": True,
        }
        for key, expected in required.items():
            if report.get(key) != expected:
                print(f"full product evidence failed {key}: {report.get(key)!r}", file=sys.stderr)
                return 1
        model_loop = report.get("model_loop", {})
        if model_loop.get("kind") != "deterministic-frontend-model-tool-loop" or model_loop.get("completed") is not True:
            print("full product evidence did not complete the frontend model loop", file=sys.stderr)
            return 1
        if report.get("native_provider") in {None, "unknown", "unsupported"}:
            print("full product evidence did not identify a production native provider", file=sys.stderr)
            return 1
        image = report.get("screenshot_base64", "")
        if not isinstance(image, str) or len(image) < 100:
            print("full product evidence omitted the returned screenshot bytes", file=sys.stderr)
            return 1
        args.report.with_suffix(".png").write_bytes(base64.b64decode(image, validate=True))
        trace = args.report.with_suffix(".trace.jsonl")
        trace.write_text("\n".join(json.dumps(event) for event in report.get("tool_calls", [])) + "\n", encoding="utf-8")
        return 0
    finally:
        terminate(app)
        app_log.close()
        terminate(frontend_process)
        frontend_log.close()
        terminate(fixture)
        if config_path:
            config_path.unlink(missing_ok=True)


if __name__ == "__main__":
    raise SystemExit(main())

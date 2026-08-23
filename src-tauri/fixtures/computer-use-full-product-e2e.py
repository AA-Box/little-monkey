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
    frontend_is_server = frontend_command is not None
    frontend = frontend_command.split() if frontend_is_server else [pnpm, "build"]
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
        # Tauri merges this override with tauri.conf.json. Omitting devUrl
        # leaves the base development URL active, so explicitly clear it.
        tauri_config["build"]["devUrl"] = None
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
    if frontend_is_server:
        frontend_deadline = time.monotonic() + 120
        while time.monotonic() < frontend_deadline:
            if frontend_process.poll() is not None:
                break
            time.sleep(1)
        if frontend_process.poll() is None:
            pass
        else:
            frontend_log.flush()
            tail = frontend_log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
            print("frontend command output:", file=sys.stderr)
            print("\n".join(tail), file=sys.stderr)
            print(f"frontend command exited (exit={frontend_process.poll()})", file=sys.stderr)
            frontend_log.close()
            terminate(fixture)
            if config_path:
                config_path.unlink(missing_ok=True)
            return 1
    else:
        try:
            frontend_process.wait(timeout=300)
        except subprocess.TimeoutExpired:
            terminate(frontend_process)
        if frontend_process.returncode != 0:
            frontend_log.flush()
            tail = frontend_log_path.read_text(encoding="utf-8", errors="replace").splitlines()[-80:]
            print("frontend build output:", file=sys.stderr)
            print("\n".join(tail), file=sys.stderr)
            print(f"frontend build exited (exit={frontend_process.returncode})", file=sys.stderr)
            frontend_log.close()
            terminate(fixture)
            if config_path:
                config_path.unlink(missing_ok=True)
            return 1

    capability_path = repo / "src-tauri" / "capabilities" / "computer-use-full-product-e2e.json"
    capability = json.loads((repo / "src-tauri" / "capabilities" / "default.json").read_text(encoding="utf-8"))
    capability["identifier"] = "computer-use-full-product-e2e"
    capability["description"] = "Temporary capability for the real frontend/native acceptance window"
    capability["windows"] = ["main"]
    capability.pop("remote", None)
    capability["permissions"].extend([
        "allow-computer-use-full-product-report",
        "allow-desktop-control-start-session",
        "allow-desktop-control-stop-session",
        "allow-desktop-control-provider-info",
        "allow-tool-computer-list-targets",
        "allow-tool-computer-inspect",
        "allow-tool-computer-click",
        "allow-tool-computer-set-value",
        "allow-tool-computer-screenshot",
    ])
    capability_path.unlink(missing_ok=True)
    capability_path.write_text(json.dumps(capability), encoding="utf-8")
    # Declare the capability before Tauri creates the local webview so its ACL
    # is present during initial IPC authority construction.
    tauri_config["app"]["windows"] = [{
        "label": "main",
        "title": "Little Monkey",
        "width": 1440,
        "height": 800,
        "minWidth": 1400,
        "minHeight": 600,
        "center": True,
        "titleBarStyle": "Overlay",
        "hiddenTitle": True,
    }]
    tauri_config["app"]["security"]["capabilities"] = ["computer-use-full-product-e2e"]
    config_path.write_text(json.dumps(tauri_config), encoding="utf-8")
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
        # The Tauri CLI compiles the full Windows app inside this subprocess.
        # Its final cargo line can reach a redirected log a few seconds after
        # the nominal build window, so leave a bounded grace period before
        # treating a missing launch marker as a build failure. The workflow's
        # job timeout remains the outer bound for a genuinely stuck build.
        # Hosted Windows runners can spend more than twenty minutes fetching
        # and compiling the first Tauri dependency graph. Keep this guard
        # below the workflow timeout, but do not reject a valid late launch.
        build_deadline = time.monotonic() + max(args.timeout + 300, 1800)
        acceptance_deadline: float | None = None
        while not args.report.is_file():
            app_log.flush()
            app_output = app_log_path.read_text(encoding="utf-8", errors="replace")
            if acceptance_deadline is None and "little-monkey.exe" in app_output:
                acceptance_deadline = time.monotonic() + args.timeout
            if acceptance_deadline is None:
                if app.poll() is not None or time.monotonic() >= build_deadline:
                    break
            elif time.monotonic() >= acceptance_deadline:
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
        capability_path.unlink(missing_ok=True)


if __name__ == "__main__":
    raise SystemExit(main())

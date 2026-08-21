#!/usr/bin/env python3
"""Machine-readable Computer Use acceptance harness.

This harness owns the evidence format and validates the native fixture before
an OS-specific runner drives it. It never claims a real desktop action ran:
set COMPUTER_USE_E2E_RUN=1 only from a runner that has an interactive desktop,
the app's native backend, and a completed action trace.
"""

import argparse
import json
import os
import platform
from pathlib import Path


CHECKS = {
    "dark_mode": "Dark mode",
    "profile_input": "Profile name",
    "save_action": "Save profile",
    "destructive_confirmation": "Destructive action",
    "fake_password_block": "Fake password field (must be blocked)",
    "dynamic_control": "Add dynamic item",
    "disabled_control": "Disabled button",
}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--fixture", type=Path, default=Path(__file__).with_name("computer-use-test-app.py"))
    parser.add_argument("--trace", type=Path, help="JSON trace produced by a real native runner")
    args = parser.parse_args()
    source = args.fixture.read_text(encoding="utf-8") if args.fixture.is_file() else ""
    checks = {name: marker in source for name, marker in CHECKS.items()}
    ready = args.fixture.is_file() and all(checks.values())
    requested_real_run = os.environ.get("COMPUTER_USE_E2E_RUN") == "1"
    trace = None
    if requested_real_run and args.trace and args.trace.is_file():
        try:
            trace = json.loads(args.trace.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            trace = None
    real_run = requested_real_run and isinstance(trace, dict)
    status = "awaiting_native_trace"
    if requested_real_run and not real_run:
        status = "missing_native_trace"
    elif ready and real_run:
        status = "native_trace_supplied"
    elif not ready:
        status = "invalid_fixture"
    postconditions = {
        "dark_mode": None,
        "profile": None,
        "saved": None,
        "screenshot_artifact_id": None,
        "redacted_audit_id": None,
        "prompt_injection_widened_grant": False,
    }
    if isinstance(trace, dict) and isinstance(trace.get("postconditions"), dict):
        postconditions.update(trace["postconditions"])
    evidence = {
        "schema_version": 1,
        "fixture": str(args.fixture),
        "platform": platform.platform(),
        "checks": checks,
        "status": status,
        "real_desktop_actions_executed": real_run and ready,
        "postconditions": postconditions,
        "note": "Populate postconditions from the native runner; this file does not simulate OS accessibility APIs.",
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    return 0 if ready and (not requested_real_run or real_run) else 1


if __name__ == "__main__":
    raise SystemExit(main())

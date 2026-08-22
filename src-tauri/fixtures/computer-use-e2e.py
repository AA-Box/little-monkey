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
import subprocess
import sys
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
    parser.add_argument("--driver", type=Path, default=Path(__file__).with_name("computer-use-native-driver.py"))
    args = parser.parse_args()
    source = args.fixture.read_text(encoding="utf-8") if args.fixture.is_file() else ""
    checks = {name: marker in source for name, marker in CHECKS.items()}
    ready = args.fixture.is_file() and all(checks.values())
    requested_real_run = os.environ.get("COMPUTER_USE_E2E_RUN") == "1"
    trace = None
    driver_error = None
    if requested_real_run and ready:
        trace_path = args.report.with_suffix(".native-trace.json")
        completed = subprocess.run(
            [sys.executable, str(args.driver), "--fixture", str(args.fixture), "--trace", str(trace_path)],
            check=False,
        )
        if completed.returncode == 0:
            try:
                trace = json.loads(trace_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError) as error:
                driver_error = str(error)
        else:
            driver_error = f"native driver exited with status {completed.returncode}"
    real_run = (
        requested_real_run
        and isinstance(trace, dict)
        and trace.get("native_desktop_actions_executed") is True
        and isinstance(trace.get("driver"), dict)
        and trace["driver"].get("kind") == "little-monkey-production-backend"
        and trace["driver"].get("pid", 0) > 0
        and trace["driver"].get("window_id")
        and trace["driver"].get("provider") in {"Accessibility", "UIAutomation", "AT-SPI"}
    )
    status = "awaiting_native_trace"
    if requested_real_run and not real_run:
        status = "missing_native_trace"
    elif ready and real_run:
        status = "native_driver_completed"
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
    required_actions = {
        "list_targets", "inspect", "semantic_toggle", "semantic_set_value",
        "semantic_invoke_save", "screenshot", "restart", "persisted_state",
    }
    actions = set(trace.get("actions", [])) if isinstance(trace, dict) else set()
    if real_run and not required_actions.issubset(actions):
        real_run = False
        status = "invalid_native_evidence"
        driver_error = "native driver omitted one or more required semantic actions"
    if real_run:
        negatives = trace.get("negative_cases", {}) if isinstance(trace, dict) else {}
        post = trace.get("postconditions", {}) if isinstance(trace, dict) else {}
        if not all(post.get(key) for key in ("dark_mode", "profile", "saved", "screenshot_artifact_id")):
            real_run = False
            status = "invalid_native_evidence"
            driver_error = "native driver did not prove all persistence and screenshot postconditions"
        elif (
            not negatives.get("secure_field_detected_and_not_typed")
            or not negatives.get("disabled_control_not_mutated")
            or not negatives.get("second_same_app_window_rejected")
            or negatives.get("prompt_injection_widened_grant")
        ):
            real_run = False
            status = "invalid_native_evidence"
            driver_error = "native driver did not prove the negative security cases"
        else:
            grant = trace.get("grant", {}) if isinstance(trace, dict) else {}
            if grant.get("window_scoped") is not True or grant.get("approval") != "test-approved-through-real-gate":
                real_run = False
                status = "invalid_native_evidence"
                driver_error = "native driver did not prove the scoped grant and real approval gate"
    evidence = {
        "schema_version": 1,
        "fixture": str(args.fixture),
        "platform": platform.platform(),
        "checks": checks,
        "status": status,
        "real_desktop_actions_executed": real_run and ready,
        "postconditions": postconditions,
        "driver": trace.get("driver") if isinstance(trace, dict) else None,
        "actions": trace.get("actions", []) if isinstance(trace, dict) else [],
        "negative_cases": trace.get("negative_cases", {}) if isinstance(trace, dict) else {},
        "driver_error": driver_error,
        "note": "Evidence is accepted only from the executable OS accessibility driver; fabricated trace dictionaries are not accepted.",
    }
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(evidence, indent=2) + "\n", encoding="utf-8")
    return 0 if ready and (not requested_real_run or real_run) else 1


if __name__ == "__main__":
    raise SystemExit(main())

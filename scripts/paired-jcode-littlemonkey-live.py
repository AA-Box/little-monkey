#!/usr/bin/env python3
"""Operator-controlled physical JCode/Little Monkey same-fixture comparison.

REQUIRES two genuine already-running native agent processes and real model
access. No mocked model results are ever accepted. Uses one independent
fixture/marker for every wave, and verifies tool-generated assistant evidence
rather than taking the user's prompt as proof. This does not start, install,
authenticate, self-update or kill either service.

Only Linux PSS samples are comparable in definition; external provider/model
servers outside daemon trees are excluded on both sides. No fairness claim
unless exact model IDs match and all workloads independently pass.
"""
from __future__ import annotations
import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import uuid

MAX_EVENT_BYTES = 2 * 1024 * 1024


def command(argv: list[str], *, timeout: float = 90) -> str:
    result = subprocess.run(argv, text=True, capture_output=True, timeout=timeout, check=False)
    if result.returncode:
        raise RuntimeError(f"Native command {argv[0]} {argv[1:3]} failed ({result.returncode})")
    if len(result.stdout) > MAX_EVENT_BYTES:
        raise RuntimeError("Native command produced unbounded output")
    return result.stdout.strip()


def parsed_command(argv: list[str], *, timeout: float = 90):
    return json.loads(command(argv, timeout=timeout))


def jcode_debug(path: str, operation: str, session_id: str | None = None,
                timeout: float = 180) -> str:
    request = {"type": "debug_command", "id": 1, "command": operation}
    if session_id:
        request["session_id"] = session_id
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
        stream.settimeout(timeout)
        stream.connect(path)
        stream.sendall((json.dumps(request) + "\n").encode())
        result = b""
        while len(result) < MAX_EVENT_BYTES:
            next_chunk = stream.recv(4096)
            if not next_chunk:
                break
            result += next_chunk
            while b"\n" in result:
                line, result = result.split(b"\n", 1)
                value = json.loads(line)
                if value.get("type") in (None, "debug_response") or "ok" in value:
                    if not value.get("ok", False):
                        raise RuntimeError("JCode debug request rejected")
                    return str(value.get("output", ""))
    raise RuntimeError("JCode did not return a verified debug response")


def live_pss_kib(root_pid: int) -> int | None:
    table = command(["ps", "-axo", "pid=,ppid="], timeout=15)
    rows = {}
    for line in table.splitlines():
        fields = line.split()
        if len(fields) == 2 and all(s.isdigit() for s in fields):
            rows[int(fields[0])] = int(fields[1])
    if root_pid not in rows:
        return None
    ids = {root_pid}
    changed = True
    while changed:
        before = len(ids)
        ids.update(pid for pid, parent in rows.items() if parent in ids)
        changed = len(ids) != before
    total = 0
    for pid in ids:
        try:
            rollup = Path(f"/proc/{pid}/smaps_rollup").read_text()
        except (OSError, UnicodeError):
            return None
        entries = [line.split() for line in rollup.splitlines() if line.startswith("Pss:")]
        if len(entries) != 1 or len(entries[0]) < 3 or entries[0][2] != "kB":
            return None
        try:
            total += int(entries[0][1])
        except ValueError:
            return None
    return total


def jcode_one(socket_path: str, sid: str, prompt: str, marker: str, timeout: int):
    started = time.monotonic()
    jcode_debug(socket_path, "message:" + prompt, sid, timeout)
    answer = jcode_debug(socket_path, "last_response", sid, timeout)
    return {"completed": marker in answer, "seconds": round(time.monotonic() - started, 3)}


def monkey_one(cli: str, target: str, cwd: str, prompt: str, marker: str, timeout: int):
    started = time.monotonic()
    created = parsed_command(
        [cli, "task", "start", prompt, "--target", target, "--workspace", cwd, "--json"],
        timeout=90,
    )
    run_id = created.get("run_id")
    if not isinstance(run_id, str) or not run_id.startswith("task-"):
        raise RuntimeError("No durable Little Monkey task identity")
    events = command([cli, "task", "attach", run_id, "--follow", "--json"], timeout=timeout)
    status = parsed_command([cli, "task", "status", "--run-id", run_id, "--json"])
    if not isinstance(status, list) or len(status) != 1 or status[0].get("run_id") != run_id:
        raise RuntimeError("Little Monkey durable status mismatch")
    assistant = ""
    for line in events.splitlines():
        ev = json.loads(line)
        if ev.get("run_id") != run_id:
            raise RuntimeError("Foreign run event")
        payload = ev.get("event", {})
        if (payload.get("type") == "model_delta"
                and payload.get("payload", {}).get("channel") == "assistant"):
            assistant += payload.get("payload", {}).get("text", "")
    return {"completed": status[0].get("status") == "succeeded" and marker in assistant,
            "seconds": round(time.monotonic() - started, 3)}


def wave(samples: int, roots: dict[str, int], label: str, jobs, timeout: int):
    outcomes = []
    memory = []
    with ThreadPoolExecutor(max_workers=samples) as pool:
        futures = [pool.submit(job) for job in jobs]
        while any(not future.done() for future in futures):
            measured = live_pss_kib(roots[label])
            memory.append(measured)
            time.sleep(.25)
        for future in futures:
            try:
                outcomes.append(future.result(timeout=1))
            except Exception:
                outcomes.append({"completed": False, "seconds": None, "error": "Native workload failed"})
    measured = live_pss_kib(roots[label])
    memory.append(measured)
    complete = len(memory) > 0 and all(m is not None for m in memory)
    times = sorted(v["seconds"] for v in outcomes if v.get("completed") and
                   isinstance(v.get("seconds"), (int, float)))
    return {"requested": samples, "completed": len(times), "failures": samples - len(times),
            "p50Seconds": times[(len(times) - 1) // 2] if times else None,
            "maxSeconds": max(times) if times else None,
            "completePssSamples": len([m for m in memory if m is not None]),
            "totalMemorySamples": len(memory),
            "peakPssMiB": round(max(memory) / 1024, 2) if complete else None,
            "source": "real native processes, Linux /proc/PID/smaps_rollup"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode-debug-socket", required=True)
    parser.add_argument("--jcode-pid", type=int, required=True)
    parser.add_argument("--jcode-model", required=True)
    parser.add_argument("--monkey-cli", required=True)
    parser.add_argument("--little-monkey-target", required=True)
    parser.add_argument("--levels", default="1,2,5")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    parser.add_argument("--confirm-live-model-costs", action="store_true")
    args = parser.parse_args()
    if not args.confirm_live_model_costs:
        parser.error("Explicit --confirm-live-model-costs required")
    if os.name != "posix" or not Path("/proc").is_dir():
        parser.error("Physical controlled PSS comparison requires Linux /proc")
    if args.timeout_seconds < 30 or args.timeout_seconds > 900:
        parser.error("Choose 30..900 seconds per real task")
    levels = [int(v) for v in args.levels.split(",")]
    if not levels or len(levels) > 5 or any(v < 1 or v > 20 for v in levels):
        parser.error("Choose 1..5 levels, each 1..20 sessions")
    if args.jcode_pid <= 0 or live_pss_kib(args.jcode_pid) is None:
        parser.error("JCode PID cannot be independently sampled")
    state = parsed_command([args.monkey_cli, "daemon", "status", "--json"])
    if not state.get("installed") or not state.get("service_running") or not state.get("heartbeat_fresh"):
        parser.error("Little Monkey native daemon is not a healthy installed service")
    lm_pid = state.get("pid")
    if not isinstance(lm_pid, int) or live_pss_kib(lm_pid) is None:
        parser.error("Little Monkey daemon PID cannot be independently sampled")
    roots = {"jcode": args.jcode_pid, "little_monkey": lm_pid}
    results = []
    with tempfile.TemporaryDirectory(prefix="lm-jcode-paired-") as folder:
        for sessions in levels:
            fixture = Path(folder, f"case-{sessions}")
            fixture.mkdir()
            marker = "SAME_REAL_FIXTURE_" + uuid.uuid4().hex
            (fixture / "fixture.txt").write_text("Independent marker: " + marker + "\n")
            prompt = ("Use a tool to read fixture.txt in the current workspace. "
                      "Return its exact independent marker in your answer. "
                      "Do not modify any files or execute network requests.")
            ids = []
            for _ in range(sessions):
                output = jcode_debug(args.jcode_debug_socket,
                                     "create_session:" + str(fixture))
                sid = json.loads(output).get("session_id")
                if not isinstance(sid, str):
                    raise RuntimeError("No authentic JCode session identity")
                jcode_debug(args.jcode_debug_socket, "set_model:" + args.jcode_model, sid)
                ids.append(sid)
            jcode_jobs = [
                lambda sid=sid: jcode_one(args.jcode_debug_socket, sid, prompt, marker,
                                         args.timeout_seconds) for sid in ids
            ]
            lm_jobs = [
                lambda: monkey_one(args.monkey_cli, args.little_monkey_target,
                                   str(fixture), prompt, marker, args.timeout_seconds)
                for _ in range(sessions)
            ]
            # Alternate order to avoid always advantaging one warm-cache path.
            if sessions % 2:
                first, second = ("jcode", jcode_jobs), ("little_monkey", lm_jobs)
            else:
                first, second = ("little_monkey", lm_jobs), ("jcode", jcode_jobs)
            rows = {}
            for key, jobs in (first, second):
                rows[key] = wave(sessions, roots, key, jobs, args.timeout_seconds)
            results.append({"sessions": sessions, **rows})
    models_equal = args.jcode_model == args.little_monkey_target
    passed = all(entry[who]["failures"] == 0 for entry in results
                 for who in ("jcode", "little_monkey"))
    result = {"kind": "physical-paired-headless-agent-benchmark",
              "verifiedLiveRuns": passed, "sameFixturePerWave": True,
              "exactModelIdentifiersMatched": models_equal,
              "jcodeModel": args.jcode_model, "littleMonkeyTarget": args.little_monkey_target,
              "scope": "sequential waves; daemon-only process tree excluding external model servers",
              "pssComparability": "only with matching model, process scope, cache conditions",
              "results": results}
    print(json.dumps(result, indent=2))
    if not passed or not models_equal or any(
        row[who]["peakPssMiB"] is None for row in results
        for who in ("jcode", "little_monkey")
    ):
        raise SystemExit(1)


if __name__ == "__main__":
    main()

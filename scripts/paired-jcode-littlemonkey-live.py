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
import hashlib
from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import os
from pathlib import Path
import select
import signal
import socket
import stat
import subprocess
import tempfile
import time
import uuid

MAX_EVENT_BYTES = 2 * 1024 * 1024


def command(argv: list[str], *, timeout: float = 90) -> str:
    # A timed-out CLI may have spawned descendants. Give every invocation its
    # own process group and reap the whole group on timeout or output overflow.
    child = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             start_new_session=True)
    deadline = time.monotonic() + timeout
    chunks = []
    size = 0
    try:
        while True:
            ready, _, _ = select.select([child.stdout], [], [], max(0, deadline - time.monotonic()))
            if not ready:
                raise TimeoutError("Native command timed out")
            chunk = os.read(child.stdout.fileno(), 65536)
            if not chunk:
                break
            size += len(chunk)
            if size > MAX_EVENT_BYTES:
                raise RuntimeError("Native command produced unbounded output")
            chunks.append(chunk)
        remaining = max(0, deadline - time.monotonic())
        if child.wait(timeout=remaining):
            raise RuntimeError("Native command failed")
        return b"".join(chunks).decode("utf-8").strip()
    except (TimeoutError, RuntimeError, UnicodeError, subprocess.TimeoutExpired):
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait()
        raise
    finally:
        child.stdout.close()


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
        received = 0
        while received < MAX_EVENT_BYTES:
            next_chunk = stream.recv(4096)
            if not next_chunk:
                break
            received += len(next_chunk)
            result += next_chunk
            while b"\n" in result:
                line, result = result.split(b"\n", 1)
                value = json.loads(line)
                if value.get("type") in (None, "debug_response") or "ok" in value:
                    if not value.get("ok", False):
                        raise RuntimeError("JCode debug request rejected")
                    return str(value.get("output", ""))
    raise RuntimeError("JCode did not return a verified debug response")


def read_process_pss_kib(pid: int) -> int | None:
    try:
        rollup = Path(f"/proc/{pid}/smaps_rollup").read_text()
    except (OSError, UnicodeError):
        return None
    entries = [line.split() for line in rollup.splitlines() if line.startswith("Pss:")]
    if len(entries) != 1 or len(entries[0]) < 3 or entries[0][2] != "kB":
        return None
    try:
        value = int(entries[0][1])
        return value if value >= 0 else None
    except ValueError:
        return None


def live_pss_kib(root_pid: int) -> int | None:
    # Reading `ps` here created a short-lived child that routinely vanished
    # before its smaps could be read. The kernel's /proc table avoids that race.
    for _ in range(3):
        rows = {}
        for entry in Path('/proc').iterdir():
            if not entry.name.isdigit():
                continue
            try:
                parent = next(line.split()[1] for line in
                              (entry / 'status').read_text().splitlines()
                              if line.startswith('PPid:'))
                rows[int(entry.name)] = int(parent)
            except (OSError, StopIteration, ValueError):
                continue  # Process exited during the table snapshot.
        if root_pid not in rows:
            return None
        ids = {root_pid}
        changed = True
        while changed:
            before = len(ids)
            ids.update(pid for pid, parent in rows.items() if parent in ids)
            changed = len(ids) != before
        samples = [read_process_pss_kib(pid) for pid in ids]
        if all(sample is not None for sample in samples):
            return sum(samples)
        time.sleep(.01)
    return None  # A stable, complete tree could not be sampled.


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


def fixture_digest(folder: Path) -> str:
    """Strict whole-directory digest: no unnoticed model mutations between waves."""
    state = hashlib.sha256()
    for item in sorted(folder.rglob("*")):
        # Native agents need a committed Git repository, but Git may update
        # internal index/status bookkeeping during READ-ONLY operations.
        # Score the checkout, not .git internals, and still reject every
        # added/changed/deleted workspace file including hidden files.
        if ".git" in item.relative_to(folder).parts:
            continue
        if item.is_symlink():
            raise RuntimeError("Agent introduced a symlink in disposable fixture")
        state.update(str(item.relative_to(folder)).encode())
        state.update(str(stat.S_IMODE(item.stat().st_mode)).encode())
        if item.is_file():
            state.update(b"file")
            state.update(hashlib.sha256(item.read_bytes()).digest())
        elif item.is_dir():
            state.update(b"directory")
        else:
            raise RuntimeError("Agent introduced a special file in disposable fixture")
    return state.hexdigest()


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


def emit_report(result: dict, output: str | None) -> None:
    payload = json.dumps(result, indent=2) + "\n"
    if output:
        destination = Path(output)
        with tempfile.NamedTemporaryFile(mode="w", dir=destination.parent,
                                         prefix=destination.name + ".", delete=False) as temp:
            temp.write(payload)
            staged = Path(temp.name)
        staged.replace(destination)
    print(payload, end="")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--jcode-debug-socket", required=True)
    parser.add_argument("--jcode-pid", type=int, required=True)
    parser.add_argument("--jcode-model", required=True)
    parser.add_argument("--jcode-backend", required=True)
    parser.add_argument("--monkey-cli", required=True)
    parser.add_argument("--little-monkey-target", required=True)
    parser.add_argument("--little-monkey-backend", required=True)
    parser.add_argument("--levels", default="1,2,5")
    parser.add_argument("--timeout-seconds", type=int, default=300)
    parser.add_argument("--output", help="Write the JSON result atomically to this file")
    parser.add_argument("--simulation", action="store_true",
                        help="Exercise local process contracts; never claim live model verification")
    parser.add_argument("--confirm-live-model-costs", action="store_true")
    args = parser.parse_args()
    if not args.simulation and not args.confirm_live_model_costs:
        parser.error("Explicit --confirm-live-model-costs required")
    if os.name != "posix" or not Path("/proc").is_dir():
        parser.error("Physical controlled PSS comparison requires Linux /proc")
    if args.timeout_seconds < 30 or args.timeout_seconds > 900:
        parser.error("Choose 30..900 seconds per real task")
    levels = [int(v) for v in args.levels.split(",")]
    if not levels or len(levels) > 5 or any(v < 1 or v > 20 for v in levels):
        parser.error("Choose 1..5 levels, each 1..20 sessions")
    if args.jcode_model != args.little_monkey_target:
        parser.error("Exact model identifiers must match before comparison")
    if args.jcode_backend != args.little_monkey_backend:
        parser.error("Exact backend identifiers must match before comparison")
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
    try:
        with tempfile.TemporaryDirectory(prefix="lm-jcode-paired-") as folder:
            for sessions in levels:
                fixture = Path(folder, f"case-{sessions}")
                fixture.mkdir()
                marker = "SAME_REAL_FIXTURE_" + uuid.uuid4().hex
                (fixture / "fixture.txt").write_text("Independent marker: " + marker + "\n")
                # monkey task start validates a durable Git workspace revision.
                # A plain temp directory would be rejected BEFORE any real model
                # execution, falsely suggesting the comparative harness worked.
                command(["git", "init", "--quiet", str(fixture)])
                command(["git", "-C", str(fixture), "add", "--", "fixture.txt"])
                command(["git", "-C", str(fixture),
                         "-c", "user.name=Little Monkey Benchmark",
                         "-c", "user.email=benchmark@localhost",
                         "commit", "--quiet", "-m", "Disposable read-only fixture"])
                pristine = fixture_digest(fixture)
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
                    if fixture_digest(fixture) != pristine:
                        raise RuntimeError(key + " mutated a read-only test fixture")
                results.append({"sessions": sessions, **rows})
    except Exception as error:
        emit_report({"kind": "paired-headless-agent-benchmark", "status": "FAILED",
                     "simulation": args.simulation, "verifiedLiveRuns": False,
                     "failureType": type(error).__name__, "results": results}, args.output)
        return 1
    passed = all(entry[who]["failures"] == 0 for entry in results
                 for who in ("jcode", "little_monkey"))
    status = "PASS" if passed and all(
                  row[who]["peakPssMiB"] is not None for row in results
                  for who in ("jcode", "little_monkey")) else "FAILED"
    result = {"kind": "physical-paired-headless-agent-benchmark",
              "status": status,
              "simulation": args.simulation,
              "contractPassed": passed,
              "verifiedLiveRuns": status == "PASS" and not args.simulation,
              "sameFixturePerWave": True,
              "exactModelIdentifiersMatched": True,
              "exactBackendIdentifiersMatched": True,
              "jcodeModel": args.jcode_model, "littleMonkeyTarget": args.little_monkey_target,
              "backend": args.jcode_backend,
              "scope": "sequential waves; daemon-only process tree excluding external model servers",
              "pssComparability": "only with matching model, process scope, cache conditions",
              "results": results}
    emit_report(result, args.output)
    return 0 if result["status"] == "PASS" else 1


if __name__ == "__main__":
    raise SystemExit(main())

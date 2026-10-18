#!/usr/bin/env python3
"""Live Linux PSS acceptance: persistent Little Monkey CLI vs JCode daemon.

This script performs real model turns. No simulated responses or estimated PSS
are accepted. Requires both installed binaries, configured access to the SAME
remote model/provider, and access to /proc/*/smaps_rollup.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import socket
import subprocess
import tempfile
import threading
import time

TURNS = (
    "Use a tool to list the files in the current directory, then reply with just the number of entries.",
    "Read the first 20 lines of README.md with a tool and reply with its top-level title only.",
    "Search the repository for the string 'fn main' with a tool and reply with how many files matched.",
    "In one sentence, summarize what you have learned about this repository so far.",
    "Reply with exactly the word: done",
)
MARKER = "LITTLE_MONKEY_BENCH_EVENT="


def require_binary(name: str) -> str:
    binary = shutil.which(name)
    if binary is None:
        raise RuntimeError("Required installed executable not found: " + name)
    return binary


def process_tree_pss_kb(roots: list[int]) -> int:
    """Count Linux PSS once per PID including daemon/process trees and helpers."""
    result = subprocess.run(
        ["ps", "-e", "-o", "pid=,ppid=,pgid="],
        check=True, capture_output=True, text=True, timeout=15,
    )
    rows = []
    for raw in result.stdout.splitlines():
        fields = raw.split()
        if len(fields) == 3 and all(value.isdigit() for value in fields):
            rows.append(tuple(int(value) for value in fields))
    groups = {group for pid, _, group in rows if pid in roots}
    included = set(roots)
    while True:
        expanded = included | {
            pid for pid, ppid, pgid in rows
            if ppid in included or pgid in groups
        }
        if expanded == included:
            break
        included = expanded
    if not included:
        raise RuntimeError("No live processes found during PSS measurement")
    total = 0
    for pid in included:
        rollup = Path(f"/proc/{pid}/smaps_rollup")
        try:
            raw = rollup.read_text()
        except FileNotFoundError:
            # Exited after process listing: do not count it.
            continue
        except PermissionError as error:
            raise RuntimeError(f"PSS not readable for PID {pid}") from error
        pss = next((line for line in raw.splitlines() if line.startswith("Pss:")), None)
        if pss is None:
            raise RuntimeError(f"No Pss field for live PID {pid}")
        total += int(pss.split()[1])
    if total <= 0:
        raise RuntimeError("PSS measurement returned zero")
    return total


def end_process(process: subprocess.Popen) -> None:
    if process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)


class Monkey:
    """One persistent, five-turn headless REPL; completion markers from Rust."""
    def __init__(self, binary: str, model: str, provider: str, cwd: str, logdir: Path):
        environment = dict(os.environ, LITTLE_MONKEY_BENCH_EVENTS="1")
        command = [
            binary, "--workspace", cwd, "--provider", provider,
            "--permission-mode", "plan", "--no-mcp", "--no-rules", model,
        ]
        self.proc = subprocess.Popen(
            command, cwd=cwd, env=environment, text=True, bufsize=1,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            start_new_session=True,
        )
        self.events: queue.Queue = queue.Queue()
        self.output = (logdir / f"monkey-{self.proc.pid}.stdout.log").open("w")
        self.errors = (logdir / f"monkey-{self.proc.pid}.stderr.log").open("w")
        def drain_stdout():
            for line in self.proc.stdout:
                self.output.write(line)
                self.output.flush()
        def drain_stderr():
            for line in self.proc.stderr:
                self.errors.write(line)
                self.errors.flush()
                if line.startswith(MARKER):
                    try:
                        self.events.put(json.loads(line[len(MARKER):]))
                    except ValueError:
                        self.events.put({"ok": False, "parse_error": True})
        threading.Thread(target=drain_stdout, daemon=True).start()
        threading.Thread(target=drain_stderr, daemon=True).start()

    def turn(self, prompt: str, timeout: int) -> dict:
        if self.proc.poll() is not None:
            raise RuntimeError("Little Monkey CLI exited before the turn")
        assert self.proc.stdin is not None
        self.proc.stdin.write(prompt + "\n")
        self.proc.stdin.flush()
        try:
            result = self.events.get(timeout=timeout)
        except queue.Empty as error:
            raise RuntimeError("Little Monkey CLI turn did not complete") from error
        if result.get("ok") is not True:
            raise RuntimeError("Little Monkey CLI reported a failed model turn")
        return result

    def close(self):
        if self.proc.poll() is None and self.proc.stdin is not None:
            try:
                self.proc.stdin.write("/bye\n")
                self.proc.stdin.flush()
            except (BrokenPipeError, OSError):
                pass
        end_process(self.proc)
        self.output.close()
        self.errors.close()


class JCode:
    """One isolated JCode daemon with N five-turn sessions via debug control."""
    def __init__(self, binary: str, cwd: str, logdir: Path):
        self.home = logdir / "jcode-home"
        self.runtime = logdir / "jcode-run"
        self.home.mkdir()
        self.runtime.mkdir()
        existing = Path.home() / ".jcode"
        for name in ("auth.json", "anthropic-auth.json", "openai-auth.json", "config.toml", "models"):
            path = existing / name
            if path.exists():
                (self.home / name).symlink_to(path)
        env = dict(os.environ, JCODE_HOME=str(self.home), JCODE_RUNTIME_DIR=str(self.runtime),
                   JCODE_TEMP_SERVER="1", JCODE_SERVER_OWNER_PID=str(os.getpid()),
                   JCODE_NO_TELEMETRY="1", JCODE_DEBUG_CONTROL="1", JCODE_MEMORY_ENABLED="0")
        self.sock = self.runtime / "bench-debug.sock"
        self.log = (logdir / "jcode.stderr.log").open("w")
        self.proc = subprocess.Popen(
            [binary, "--no-update", "--no-selfdev", "serve", "--socket", str(self.runtime / "bench.sock")],
            cwd=cwd, env=env, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=self.log, start_new_session=True,
        )
        deadline = time.monotonic() + 45
        while not self.sock.exists() and time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError("JCode daemon exited before debug socket")
            time.sleep(.1)
        if not self.sock.exists():
            raise RuntimeError("JCode debug socket unavailable; ensure JCode supports debug control")

    def command(self, command: str, session_id: str | None = None, timeout: int = 600) -> str:
        request = {"type": "debug_command", "id": 1, "command": command}
        if session_id:
            request["session_id"] = session_id
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(timeout)
            client.connect(str(self.sock))
            client.sendall((json.dumps(request) + "\n").encode())
            buf = b""
            while True:
                chunk = client.recv(1024 * 1024)
                if not chunk:
                    raise RuntimeError("JCode debug channel disconnected")
                buf += chunk
                if len(buf) > 8 * 1024 * 1024:
                    raise RuntimeError("Oversized JCode debug response")
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    if not line:
                        continue
                    item = json.loads(line)
                    if item.get("type") in (None, "debug_response") or "ok" in item:
                        if not item.get("ok", False):
                            raise RuntimeError("JCode turn failed: " + str(item.get("error", ""))[:120])
                        return str(item.get("output", ""))

    def close(self):
        end_process(self.proc)
        self.log.close()


def measure(monkey: str, jcode: str, model: str, provider: str, cwd: str,
            sessions: int, timeout: int, base: Path) -> dict:
    out = {"sessions": sessions, "models": {"monkey": model, "jcode": model},
           "provider": provider, "samples": []}
    for name in ("monkey", "jcode"):
        log = base / (name + "-" + str(sessions))
        log.mkdir()
        agents = []
        server = None
        try:
            if name == "monkey":
                agents = [Monkey(monkey, model, provider, cwd, log) for _ in range(sessions)]
                roots = [agent.proc.pid for agent in agents]
            else:
                server = JCode(jcode, cwd, log)
                roots = [server.proc.pid]
                agent_ids = []
                for _ in range(sessions):
                    ident = json.loads(server.command("create_session:" + cwd, timeout=120))["session_id"]
                    server.command("set_model:" + model, ident, timeout=60)
                    agent_ids.append(ident)
            time.sleep(.5)
            samples = [{"turn": 0, "pss_kb": process_tree_pss_kb(roots)}]
            errors = []
            for index, prompt in enumerate(TURNS, start=1):
                outcomes = [None] * sessions
                def one(i: int):
                    try:
                        outcomes[i] = (agents[i].turn(prompt, timeout) if name == "monkey"
                                       else server.command("message:" + prompt, agent_ids[i], timeout))
                    except Exception as error:
                        errors.append(str(error))
                threads = [threading.Thread(target=one, args=(i,)) for i in range(sessions)]
                for thread in threads:
                    thread.start()
                for thread in threads:
                    thread.join(timeout + 10)
                if errors or any(value is None for value in outcomes) or any(thread.is_alive() for thread in threads):
                    raise RuntimeError(f"{name} turn {index} failed: " + "; ".join(errors[:3]))
                time.sleep(.5)
                row = {"turn": index, "pss_kb": process_tree_pss_kb(roots)}
                if name == "monkey":
                    row["tool_results"] = sum(result.get("tool_results", 0) for result in outcomes)
                    row["history_messages"] = min(result.get("history_messages", 0) for result in outcomes)
                samples.append(row)
            if name == "monkey" and samples[-1]["tool_results"] == 0:
                raise RuntimeError("Little Monkey emitted no successful tool-result records; benchmark invalid")
            if name == "jcode":
                for ident in agent_ids:
                    session = json.loads(server.command("history", ident, timeout=30))
                    if not isinstance(session, list) or len(session) < 5:
                        raise RuntimeError("JCode did not preserve a real five-turn history")
            out[name] = {"samples": samples,
                         "peak_pss_kb": max(row["pss_kb"] for row in samples)}
        finally:
            for agent in agents:
                agent.close()
            if server is not None:
                server.close()
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--workspace", required=True)
    parser.add_argument("--model", required=True, help="Exact model identifier used in both tools")
    parser.add_argument("--provider", required=True, help="Provider in Little Monkey; configure matching JCode provider")
    parser.add_argument("--monkey", default="monkey-cli")
    parser.add_argument("--jcode", default="jcode")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    if not Path("/proc/self/smaps_rollup").exists():
        raise SystemExit("Linux /proc smaps_rollup required; no RSS estimates accepted")
    workspace = str(Path(args.workspace).resolve(strict=True))
    if not (Path(workspace) / "README.md").is_file():
        raise SystemExit("Workspace must have README.md for identical turn-2 task")
    if args.timeout < 30:
        raise SystemExit("Turn timeout must be at least 30 seconds")
    monkey = require_binary(args.monkey)
    jcode = require_binary(args.jcode)
    results = {"schema": 1, "model": args.model, "provider": args.provider,
               "workload": list(TURNS), "hardware": os.uname().machine,
               "linux_kernel": os.uname().release, "runs": []}
    with tempfile.TemporaryDirectory(prefix="little-monkey-jcode-parity-") as temporary:
        directory = Path(temporary)
        for repeat in range(2):
            for concurrency in (1, 5, 10):
                results["runs"].append(measure(
                    monkey, jcode, args.model, args.provider, workspace,
                    concurrency, args.timeout, directory / f"trial-{repeat}-{concurrency}",
                ))
    # No success artifact is written on failure or partial execution.
    destination = Path(args.output)
    destination.parent.mkdir(parents=True, exist_ok=True)
    temp = destination.with_suffix(destination.suffix + ".tmp")
    temp.write_text(json.dumps(results, indent=2) + "\n")
    temp.replace(destination)
    print(f"ACCEPTED: 2 genuine trials x 3 concurrency levels x 2 agents x 5 turns: {destination}")


if __name__ == "__main__":
    main()

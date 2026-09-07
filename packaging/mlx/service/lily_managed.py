#!/usr/bin/env python3
"""Managed Lily acceleration adapter for Little Monkey's private MLX ABI.

This process is never configured as an OpenAI provider. It is launched by the
signed MLX runtime service when the host and model are Lily-capable, owns the
Lily child process, and exposes the exact `/v1/generate` SSE contract consumed
by `ProductionMlxServiceController`.

Lily intentionally supports only greedy, text-only chat without tools or
structured output. Any request outside that surface permanently transitions
this model process to the normal packaged MLX service before forwarding the
request. That keeps Lily an acceleration path, never a capability regression.
"""

from __future__ import annotations

import argparse
import http.client
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

MAX_REQUEST_BYTES = 16 * 1024 * 1024
STARTUP_TIMEOUT_SECONDS = 180.0
SHUTDOWN_TIMEOUT_SECONDS = 8.0
LILY_MODEL_ID = "Qwen3.6-35B-A3B"


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait_http(port: int, path: str, process: subprocess.Popen, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"managed child exited during startup with code {process.returncode}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}{path}", timeout=1.0) as response:
                if 200 <= response.status < 300:
                    return
        except Exception as error:  # noqa: BLE001 - readiness retry records final cause
            last_error = error
        time.sleep(0.1)
    raise RuntimeError(f"managed child did not become ready on port {port}: {last_error}")


def _wait_port(port: int, process: subprocess.Popen, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"fallback MLX service exited during startup with code {process.returncode}")
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return
        except OSError as error:
            last_error = error
        time.sleep(0.1)
    raise RuntimeError(f"fallback MLX service did not become ready on port {port}: {last_error}")


def _terminate(process: subprocess.Popen | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=SHUTDOWN_TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=SHUTDOWN_TIMEOUT_SECONDS)


def _lily_compatible(request: dict) -> bool:
    if request.get("tools"):
        return False
    if request.get("structuredOutputSchema") is not None:
        return False
    temperature = request.get("temperature")
    if temperature is not None and float(temperature) != 0.0:
        return False
    messages = request.get("messages") or []
    if not messages or str(messages[-1].get("role", "")) != "user":
        return False
    for message in messages:
        if message.get("images"):
            return False
        if str(message.get("role", "")) not in {"system", "user", "assistant"}:
            return False
    return True


class _Backend:
    """Owns exactly one heavy inference child at a time."""

    def __init__(self, lily_binary: Path, model: Path, fallback_service: Path, python: Path) -> None:
        self.lily_binary = lily_binary
        self.model = model
        self.fallback_service = fallback_service
        self.python = python
        self._lock = threading.RLock()
        self._mode = "lily"
        self._port = _free_port()
        self._process: subprocess.Popen | None = None
        self._start_lily()

    @property
    def mode(self) -> str:
        with self._lock:
            return self._mode

    @property
    def port(self) -> int:
        with self._lock:
            return self._port

    def _start_lily(self) -> None:
        args = [
            str(self.lily_binary),
            "--model",
            str(self.model),
            "--bind",
            f"127.0.0.1:{self._port}",
            "--max-seq",
            "262144",
        ]
        self._process = subprocess.Popen(args, stdin=subprocess.DEVNULL)
        _wait_http(self._port, "/health", self._process, STARTUP_TIMEOUT_SECONDS)
        with urllib.request.urlopen(f"http://127.0.0.1:{self._port}/v1/models", timeout=2.0) as response:
            models = json.load(response)
        ids = {str(item.get("id")) for item in models.get("data", [])}
        if LILY_MODEL_ID not in ids:
            _terminate(self._process)
            raise RuntimeError(f"Lily did not expose required model {LILY_MODEL_ID}")
        sys.stderr.write("mlx-service engine=lily status=ready\n")
        sys.stderr.flush()

    def ensure_fallback(self) -> int:
        with self._lock:
            if self._mode == "mlx":
                return self._port
            _terminate(self._process)
            self._port = _free_port()
            env = dict(os.environ)
            env["LITTLE_MONKEY_DISABLE_LILY"] = "1"
            self._process = subprocess.Popen(
                [
                    str(self.python),
                    str(self.fallback_service),
                    "--host",
                    "127.0.0.1",
                    "--port",
                    str(self._port),
                    "--model",
                    str(self.model),
                ],
                stdin=subprocess.DEVNULL,
                env=env,
            )
            _wait_port(self._port, self._process, STARTUP_TIMEOUT_SECONDS)
            self._mode = "mlx"
            sys.stderr.write("mlx-service engine=mlx reason=lily_capability_fallback status=ready\n")
            sys.stderr.flush()
            return self._port

    def close(self) -> None:
        with self._lock:
            _terminate(self._process)
            self._process = None

    def lily_generate(self, request: dict) -> dict:
        with self._lock:
            if self._mode != "lily":
                raise RuntimeError("Lily is no longer active")
            port = self._port
        body = {
            "model": LILY_MODEL_ID,
            "messages": [
                {"role": str(message.get("role")), "content": str(message.get("text", ""))}
                for message in request.get("messages", [])
            ],
            "max_tokens": int(request.get("maxTokens") or 512),
            "stream": False,
        }
        if request.get("promptCacheKey"):
            body["prompt_cache_key"] = request["promptCacheKey"]
        encoded = json.dumps(body, separators=(",", ":")).encode("utf-8")
        upstream = urllib.request.Request(
            f"http://127.0.0.1:{port}/v1/chat/completions",
            data=encoded,
            headers={"Content-Type": "application/json"},
            method="POST",
        )
        try:
            with urllib.request.urlopen(upstream, timeout=3600.0) as response:
                return json.load(response)
        except urllib.error.HTTPError as error:
            detail = error.read(4096).decode("utf-8", "replace")
            raise RuntimeError(f"Lily HTTP {error.code}: {detail}") from error


class _Handler(BaseHTTPRequestHandler):
    backend: _Backend | None = None
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        sys.stderr.write("lily-managed %s\n" % (format % args))

    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/health":
            self.send_error(404)
            return
        payload = json.dumps({"ok": True, "engine": self.backend.mode}).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v1/generate":
            self.send_error(404, "unknown endpoint")
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400, "bad content length")
            return
        if length <= 0 or length > MAX_REQUEST_BYTES:
            self.send_error(413, "request too large")
            return
        try:
            request = json.loads(self.rfile.read(length))
        except (OSError, ValueError):
            self.send_error(400, "body is not JSON")
            return

        if not _lily_compatible(request) or self.backend.mode != "lily":
            self._proxy_fallback(request)
            return
        self._serve_lily(request)

    def _begin_sse(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _chunk(self, payload: bytes) -> None:
        self.wfile.write(b"%x\r\n%s\r\n" % (len(payload), payload))
        self.wfile.flush()

    def _event(self, event: dict) -> None:
        self._chunk(("data: %s\n" % json.dumps(event, separators=(",", ":"))).encode("utf-8"))

    def _finish(self) -> None:
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _serve_lily(self, request: dict) -> None:
        self._begin_sse()
        request_id = str(request.get("requestId", ""))
        self._event({"type": "started", "request_id": request_id})
        try:
            response = self.backend.lily_generate(request)
            choices = response.get("choices") or []
            content = str((choices[0].get("message") or {}).get("content") or "") if choices else ""
            if content:
                self._event({"type": "text_delta", "text": content})
            usage = response.get("usage") or {}
            details = usage.get("prompt_tokens_details") or {}
            cached = max(0, int(details.get("cached_tokens") or 0))
            input_tokens = max(0, int(usage.get("prompt_tokens") or 0))
            output_tokens = max(0, int(usage.get("completion_tokens") or 0))
            self._event(
                {
                    "type": "completed",
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cached_input_tokens": cached,
                }
            )
            sys.stderr.write(
                f"mlx-service engine=lily request={request_id} input_tokens={input_tokens} "
                f"output_tokens={output_tokens} cached_input_tokens={cached}\n"
            )
            sys.stderr.flush()
        except Exception as error:  # noqa: BLE001 - protocol must surface terminal failure
            self._event({"type": "error", "code": "generation_failed", "message": str(error)})
            self._event(
                {
                    "type": "completed",
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cached_input_tokens": 0,
                }
            )
        self._finish()

    def _proxy_fallback(self, request: dict) -> None:
        try:
            port = self.backend.ensure_fallback()
            encoded = json.dumps(request, separators=(",", ":")).encode("utf-8")
            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3600.0)
            connection.request("POST", "/v1/generate", body=encoded, headers={"Content-Type": "application/json"})
            response = connection.getresponse()
            if response.status != 200:
                detail = response.read(4096)
                self.send_error(response.status, detail.decode("utf-8", "replace"))
                connection.close()
                return
            self._begin_sse()
            while True:
                chunk = response.read(64 * 1024)
                if not chunk:
                    break
                self._chunk(chunk)
            self._finish()
            connection.close()
        except BrokenPipeError:
            return
        except Exception as error:  # noqa: BLE001
            self.send_error(500, str(error))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Managed Lily acceleration adapter")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--model", required=True)
    parser.add_argument("--lily-binary", required=True)
    parser.add_argument("--fallback-service", required=True)
    parser.add_argument("--python", required=True)
    args = parser.parse_args(argv)
    if args.host != "127.0.0.1":
        parser.error("--host must be 127.0.0.1")

    backend = _Backend(Path(args.lily_binary), Path(args.model), Path(args.fallback_service), Path(args.python))
    _Handler.backend = backend
    server = ThreadingHTTPServer((args.host, args.port), _Handler)

    stopping = threading.Event()

    def stop(_signum=None, _frame=None):
        if stopping.is_set():
            return
        stopping.set()
        threading.Thread(target=server.shutdown, daemon=True).start()

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    sys.stderr.write(f"mlx-service listening on {args.host}:{args.port} engine=lily\n")
    sys.stderr.flush()
    try:
        server.serve_forever()
    finally:
        server.server_close()
        backend.close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Hardware-free contract tests for the managed Lily acceleration path."""

from __future__ import annotations

import importlib.util
import json
import socket
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
SERVICE = HERE / "lily_managed.py"
ROUTER = HERE / "runtime_router.py"


def _load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def _port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _wait(port: int, process: subprocess.Popen) -> None:
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stderr = process.stderr.read() if process.stderr is not None else ""
            raise AssertionError(f"service exited early: {process.returncode}\n{stderr}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=0.2):
                return
        except Exception:
            time.sleep(0.05)
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=5)
    stderr = process.stderr.read() if process.stderr is not None else ""
    raise AssertionError(f"service did not become healthy\n{stderr}")


def _post(port: int, body: dict) -> str:
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/generate",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        return response.read().decode()


def _write_executable(path: Path, source: str) -> None:
    path.write_text(source)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def test_router_model_gate() -> None:
    router = _load(ROUTER, "runtime_router_test")
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        config = {
            "model_type": "qwen3_5_moe",
            "vision_config": {},
            "quantization": {"bits": 4, "group_size": 64, "mode": "affine"},
        }
        (root / "config.json").write_text(json.dumps(config))
        assert router._model_is_lily_candidate(root)
        config["quantization"]["group_size"] = 128
        (root / "config.json").write_text(json.dumps(config))
        assert not router._model_is_lily_candidate(root)


def test_request_capability_gate() -> None:
    managed = _load(SERVICE, "lily_managed_test")
    base = {
        "requestId": "r1",
        "messages": [{"role": "user", "text": "hello", "images": []}],
        "tools": [],
        "maxTokens": 8,
    }
    assert managed._lily_compatible(base)
    assert not managed._lily_compatible({**base, "tools": [{"name": "clock"}]})
    assert not managed._lily_compatible(
        {**base, "messages": [{"role": "user", "text": "look", "images": ["data:image/png;base64,AA=="]}]}
    )
    assert not managed._lily_compatible({**base, "temperature": 0.7})
    assert not managed._lily_compatible({**base, "structuredOutputSchema": {"type": "object"}})


def test_full_managed_lily_then_capability_fallback() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        fake_lily = root / "lily"
        fallback = root / "fallback.py"
        model = root / "model"
        model.mkdir()

        fake_source = textwrap.dedent(
            r'''
            import argparse, json
            from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
            p=argparse.ArgumentParser(); p.add_argument('--model'); p.add_argument('--bind'); p.add_argument('--max-seq'); a=p.parse_args()
            host,port=a.bind.rsplit(':',1)
            class H(BaseHTTPRequestHandler):
              def log_message(self,*a): pass
              def do_GET(self):
                if self.path=='/health': body=b'{"ok":true}'
                elif self.path=='/v1/models': body=b'{"data":[{"id":"Qwen3.6-35B-A3B"}]}'
                else: self.send_error(404); return
                self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
              def do_POST(self):
                n=int(self.headers.get('Content-Length','0')); json.loads(self.rfile.read(n))
                body=json.dumps({'choices':[{'message':{'content':'lily-ok'}}], 'usage':{'prompt_tokens':11,'completion_tokens':2,'prompt_tokens_details':{'cached_tokens':7}}}).encode()
                self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(body))); self.end_headers(); self.wfile.write(body)
            ThreadingHTTPServer((host,int(port)),H).serve_forever()
            '''
        ).lstrip()
        # Use the exact Python interpreter running the test as the shebang.
        # This avoids PATH/env/shebang drift on hosted macOS while still crossing
        # the real executable-child boundary used by the managed adapter.
        _write_executable(fake_lily, f"#!{sys.executable}\n{fake_source}")

        fallback.write_text(
            textwrap.dedent(
                r'''
                import argparse, json
                from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
                p=argparse.ArgumentParser(); p.add_argument('--host'); p.add_argument('--port',type=int); p.add_argument('--model'); a=p.parse_args()
                class H(BaseHTTPRequestHandler):
                  protocol_version='HTTP/1.1'
                  def log_message(self,*a): pass
                  def do_POST(self):
                    n=int(self.headers.get('Content-Length','0')); self.rfile.read(n)
                    events=[
                      {'type':'started','request_id':'r2'},
                      {'type':'text_delta','text':'mlx-fallback'},
                      {'type':'completed','input_tokens':5,'output_tokens':1,'cached_input_tokens':0},
                    ]
                    raw=''.join('data: '+json.dumps(x,separators=(',',':'))+'\n' for x in events).encode()
                    self.send_response(200); self.send_header('Content-Type','text/event-stream'); self.send_header('Content-Length',str(len(raw))); self.end_headers(); self.wfile.write(raw)
                ThreadingHTTPServer((a.host,a.port),H).serve_forever()
                '''
            )
        )

        port = _port()
        process = subprocess.Popen(
            [
                sys.executable,
                str(SERVICE),
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
                "--model",
                str(model),
                "--lily-binary",
                str(fake_lily),
                "--fallback-service",
                str(fallback),
                "--python",
                sys.executable,
                "--startup-timeout-seconds",
                "5",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            _wait(port, process)
            lily = _post(
                port,
                {
                    "requestId": "r1",
                    "messages": [{"role": "user", "text": "hello", "images": []}],
                    "tools": [],
                    "maxTokens": 8,
                    "promptCacheKey": "conversation",
                },
            )
            assert "lily-ok" in lily
            assert '"cached_input_tokens":7' in lily

            fallback_response = _post(
                port,
                {
                    "requestId": "r2",
                    "messages": [{"role": "user", "text": "use a tool", "images": []}],
                    "tools": [{"name": "clock"}],
                    "maxTokens": 8,
                },
            )
            assert "mlx-fallback" in fallback_response
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health") as response:
                assert json.load(response)["engine"] == "mlx"
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


def main() -> None:
    test_router_model_gate()
    test_request_capability_gate()
    test_full_managed_lily_then_capability_fallback()
    print("managed Lily contract checks passed")


if __name__ == "__main__":
    main()

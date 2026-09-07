#!/usr/bin/env python3
"""Hardware-free contract tests for the managed Lily acceleration path."""

from __future__ import annotations

import importlib.util
import json
import socket
import stat
import struct
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


def _raw_http_helpers() -> str:
    return textwrap.dedent(
        r'''
        import socket

        def receive_request(connection):
            data = b''
            while b'\r\n\r\n' not in data:
                chunk = connection.recv(65536)
                if not chunk:
                    raise RuntimeError('client closed before headers')
                data += chunk
            head, body = data.split(b'\r\n\r\n', 1)
            lines = head.decode('iso-8859-1').split('\r\n')
            method, path, _version = lines[0].split(' ', 2)
            headers = {}
            for line in lines[1:]:
                name, value = line.split(':', 1)
                headers[name.lower()] = value.strip()
            length = int(headers.get('content-length', '0'))
            while len(body) < length:
                chunk = connection.recv(65536)
                if not chunk:
                    raise RuntimeError('client closed before body')
                body += chunk
            return method, path, body[:length]

        def respond(connection, body, content_type='application/json', status='200 OK'):
            head = (
                f'HTTP/1.1 {status}\r\n'
                f'Content-Type: {content_type}\r\n'
                f'Content-Length: {len(body)}\r\n'
                'Connection: close\r\n\r\n'
            ).encode('ascii')
            connection.sendall(head + body)

        def serve(host, port, handler):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as server:
                server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                server.bind((host, port))
                server.listen(16)
                while True:
                    connection, _address = server.accept()
                    with connection:
                        method, path, body = receive_request(connection)
                        handler(connection, method, path, body)
        '''
    )


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


def test_full_managed_lily_then_cancel_then_capability_fallback() -> None:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        fake_lily = root / "lily"
        fallback = root / "fallback.py"
        model = root / "model"
        model.mkdir()

        fake_source = _raw_http_helpers() + textwrap.dedent(
            r'''
            import argparse, json, time
            parser=argparse.ArgumentParser()
            parser.add_argument('--model'); parser.add_argument('--bind'); parser.add_argument('--max-seq')
            args=parser.parse_args()
            host,port=args.bind.rsplit(':',1)

            def handler(connection, method, path, body):
                if method == 'GET' and path == '/health':
                    respond(connection, b'{"ok":true}')
                elif method == 'GET' and path == '/v1/models':
                    respond(connection, b'{"data":[{"id":"Qwen3.6-35B-A3B"}]}')
                elif method == 'POST' and path == '/v1/chat/completions':
                    request=json.loads(body)
                    assert request['model'] == 'Qwen3.6-35B-A3B'
                    if any(message.get('content') == 'block' for message in request.get('messages', [])):
                        time.sleep(30)
                    payload=json.dumps({
                        'choices':[{'message':{'content':'lily-ok'}}],
                        'usage':{
                            'prompt_tokens':11,
                            'completion_tokens':2,
                            'prompt_tokens_details':{'cached_tokens':7},
                        },
                    }, separators=(',', ':')).encode()
                    respond(connection, payload)
                else:
                    respond(connection, b'not found', 'text/plain', '404 Not Found')

            serve(host, int(port), handler)
            '''
        )
        _write_executable(fake_lily, f"#!{sys.executable}\n{fake_source}")

        fallback.write_text(
            _raw_http_helpers()
            + textwrap.dedent(
                r'''
                import argparse, json
                parser=argparse.ArgumentParser()
                parser.add_argument('--host'); parser.add_argument('--port',type=int); parser.add_argument('--model')
                args=parser.parse_args()

                def handler(connection, method, path, body):
                    if method != 'POST' or path != '/v1/generate':
                        respond(connection, b'not found', 'text/plain', '404 Not Found')
                        return
                    json.loads(body)
                    events=[
                        {'type':'started','request_id':'r2'},
                        {'type':'text_delta','text':'mlx-fallback'},
                        {'type':'completed','input_tokens':5,'output_tokens':1,'cached_input_tokens':0},
                    ]
                    payload=''.join('data: '+json.dumps(x,separators=(',',':'))+'\n' for x in events).encode()
                    respond(connection, payload, 'text/event-stream')

                serve(args.host, args.port, handler)
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

            # This is the same cancellation signal the Rust controller produces:
            # it drops the streaming HTTP response. Force an RST so the adapter's
            # SSE keepalive observes it deterministically and must kill Lily.
            cancel_body = json.dumps(
                {
                    "requestId": "cancel-me",
                    "messages": [{"role": "user", "text": "block", "images": []}],
                    "tools": [],
                    "maxTokens": 8,
                },
                separators=(",", ":"),
            ).encode()
            cancelled = socket.create_connection(("127.0.0.1", port), timeout=2)
            cancelled.settimeout(3)
            cancelled.sendall(
                (
                    "POST /v1/generate HTTP/1.1\r\n"
                    "Host: 127.0.0.1\r\n"
                    "Content-Type: application/json\r\n"
                    f"Content-Length: {len(cancel_body)}\r\n"
                    "Connection: close\r\n\r\n"
                ).encode()
                + cancel_body
            )
            observed = b""
            while b'"type":"started"' not in observed:
                chunk = cancelled.recv(4096)
                assert chunk, "managed Lily stream closed before started"
                observed += chunk
            cancelled.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            cancelled.close()

            deadline = time.monotonic() + 6
            while time.monotonic() < deadline:
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=0.5) as response:
                    engine = json.load(response)["engine"]
                if engine == "mlx_pending":
                    break
                time.sleep(0.05)
            else:
                raise AssertionError("dropping the stream did not stop Lily compute")

            # A queued/next request must not talk to the killed Lily process. It
            # enters the existing managed MLX service and preserves the original
            # tool request unchanged.
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
    test_full_managed_lily_then_cancel_then_capability_fallback()
    print("managed Lily contract checks passed")


if __name__ == "__main__":
    main()

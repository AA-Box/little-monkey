#!/usr/bin/env python3
"""Hardware-free production-contract tests for managed Lily acceleration."""

from __future__ import annotations

import importlib.util
import json
import os
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


def _stage(message: str) -> None:
    print(f"managed-lily-test: {message}", flush=True)


def _load(path: Path, name: str):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def _port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def _write_executable(path: Path, source: str) -> None:
    path.write_text(source)
    path.chmod(path.stat().st_mode | stat.S_IXUSR)


def _diagnostics(process: subprocess.Popen, stderr_file) -> str:
    stderr_file.flush()
    stderr_file.seek(0)
    text = stderr_file.read()
    return f"parent_rc={process.poll()}\n{text}"


def _wait_health(port: int, process: subprocess.Popen, stderr_file, timeout: float = 12.0) -> dict:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise AssertionError(f"managed parent exited early\n{_diagnostics(process, stderr_file)}")
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=0.4) as response:
                return json.load(response)
        except Exception as error:  # noqa: BLE001 - readiness is intentionally retried
            last_error = error
            time.sleep(0.05)
    raise AssertionError(
        f"managed parent did not become healthy: {last_error!r}\n{_diagnostics(process, stderr_file)}"
    )


def _post(port: int, body: dict, timeout: float = 10.0) -> str:
    request = urllib.request.Request(
        f"http://127.0.0.1:{port}/v1/generate",
        data=json.dumps(body, separators=(",", ":")).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=timeout) as response:
        assert response.status == 200
        return response.read().decode()


def _raw_http_helpers() -> str:
    # Deliberately avoid any \r/\n string escaping in the generated child. The
    # previous fixtures accidentally tested literal backslashes on hosted macOS.
    return textwrap.dedent(
        '''
        import socket
        import sys

        CRLF = bytes((13, 10))
        HEADER_END = CRLF + CRLF
        LF = bytes((10,))

        def receive_request(connection):
            data = b''
            while HEADER_END not in data:
                chunk = connection.recv(65536)
                if not chunk:
                    return None
                data += chunk
                if len(data) > 2 * 1024 * 1024:
                    raise RuntimeError('headers too large')
            head, body = data.split(HEADER_END, 1)
            lines = head.split(CRLF)
            method, path, _version = lines[0].decode('ascii').split(' ', 2)
            headers = {}
            for raw in lines[1:]:
                name, value = raw.decode('iso-8859-1').split(':', 1)
                headers[name.lower()] = value.strip()
            length = int(headers.get('content-length', '0'))
            while len(body) < length:
                chunk = connection.recv(65536)
                if not chunk:
                    raise RuntimeError('client closed before body')
                body += chunk
            return method, path, body[:length]

        def respond(connection, body, content_type='application/json', status='200 OK'):
            lines = [
                'HTTP/1.1 ' + status,
                'Content-Type: ' + content_type,
                'Content-Length: ' + str(len(body)),
                'Connection: close',
            ]
            head = CRLF.join(line.encode('ascii') for line in lines)
            connection.sendall(head + HEADER_END + body)

        def serve(host, port, handler, label):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as server:
                server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                server.bind((host, port))
                server.listen(16)
                print(label + ' listening ' + host + ':' + str(port), file=sys.stderr, flush=True)
                while True:
                    connection, _address = server.accept()
                    with connection:
                        try:
                            request = receive_request(connection)
                            if request is None:
                                continue
                            method, path, body = request
                            handler(connection, method, path, body)
                        except (ConnectionResetError, BrokenPipeError, OSError, RuntimeError):
                            continue
        '''
    )


def _fake_lily_source() -> str:
    return _raw_http_helpers() + textwrap.dedent(
        '''
        import argparse
        import json
        import time

        parser = argparse.ArgumentParser()
        parser.add_argument('--model')
        parser.add_argument('--bind', required=True)
        parser.add_argument('--max-seq')
        args = parser.parse_args()
        host, port_text = args.bind.rsplit(':', 1)
        port = int(port_text)

        def handler(connection, method, path, body):
            if method == 'GET' and path == '/health':
                respond(connection, b'{"ok":true}')
                return
            if method == 'GET' and path == '/v1/models':
                respond(connection, b'{"data":[{"id":"Qwen3.6-35B-A3B"}]}')
                return
            if method == 'POST' and path == '/v1/chat/completions':
                request = json.loads(body)
                assert request['model'] == 'Qwen3.6-35B-A3B'
                if any(message.get('content') == 'block' for message in request.get('messages', [])):
                    time.sleep(15)
                payload = json.dumps(
                    {
                        'choices': [{'message': {'content': 'lily-ok'}}],
                        'usage': {
                            'prompt_tokens': 11,
                            'completion_tokens': 2,
                            'prompt_tokens_details': {'cached_tokens': 7},
                        },
                    },
                    separators=(',', ':'),
                ).encode()
                respond(connection, payload)
                return
            respond(connection, b'not found', 'text/plain', '404 Not Found')

        serve(host, port, handler, 'fake-lily')
        '''
    )


def _fake_fallback_source() -> str:
    return _raw_http_helpers() + textwrap.dedent(
        '''
        import argparse
        import json

        parser = argparse.ArgumentParser()
        parser.add_argument('--host', required=True)
        parser.add_argument('--port', required=True, type=int)
        parser.add_argument('--model', required=True)
        args = parser.parse_args()

        def handler(connection, method, path, body):
            if method != 'POST' or path != '/v1/generate':
                respond(connection, b'not found', 'text/plain', '404 Not Found')
                return
            request = json.loads(body)
            assert request.get('tools') == [{'name': 'clock'}]
            events = [
                {'type': 'started', 'request_id': request['requestId']},
                {'type': 'text_delta', 'text': 'mlx-fallback'},
                {
                    'type': 'completed',
                    'input_tokens': 5,
                    'output_tokens': 1,
                    'cached_input_tokens': 0,
                },
            ]
            payload = b''.join(
                b'data: ' + json.dumps(event, separators=(',', ':')).encode() + LF
                for event in events
            )
            respond(connection, payload, 'text/event-stream')

        serve(args.host, args.port, handler, 'fake-mlx-fallback')
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
        _write_executable(fake_lily, f"#!{sys.executable}\n{_fake_lily_source()}")
        fallback.write_text(_fake_fallback_source())

        port = _port()
        stderr_path = root / "managed.stderr.log"
        with stderr_path.open("w+") as stderr_file:
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
                    "4",
                ],
                stdout=subprocess.DEVNULL,
                stderr=stderr_file,
                text=True,
                env={**os.environ, "PYTHONUNBUFFERED": "1"},
            )
            try:
                _stage("waiting for managed parent")
                health = _wait_health(port, process, stderr_file)
                assert health["engine"] == "lily", _diagnostics(process, stderr_file)

                _stage("verifying Lily inference translation and cache usage")
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

                _stage("starting cancellable Lily request")
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
                _stage("dropping downstream stream")
                cancelled.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
                cancelled.close()

                deadline = time.monotonic() + 6
                while time.monotonic() < deadline:
                    try:
                        with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=0.5) as response:
                            engine = json.load(response)["engine"]
                    except Exception:
                        time.sleep(0.05)
                        continue
                    if engine == "mlx_pending":
                        break
                    time.sleep(0.05)
                else:
                    raise AssertionError(
                        "dropping the stream did not stop Lily compute\n" + _diagnostics(process, stderr_file)
                    )

                _stage("verifying lazy managed-MLX fallback")
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
                with urllib.request.urlopen(f"http://127.0.0.1:{port}/health", timeout=1) as response:
                    assert json.load(response)["engine"] == "mlx"
                _stage("full managed lifecycle passed")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)


def main() -> None:
    _stage("router gate")
    test_router_model_gate()
    _stage("request capability gate")
    test_request_capability_gate()
    test_full_managed_lily_then_cancel_then_capability_fallback()
    print("managed Lily contract checks passed", flush=True)


if __name__ == "__main__":
    main()

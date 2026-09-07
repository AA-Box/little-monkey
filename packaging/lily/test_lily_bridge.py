"""Contract checks for the loopback Lily compatibility bridge.

Run: ``python3 packaging/lily/test_lily_bridge.py``.
No Lily binary, model, Apple GPU, or third-party Python package is required.
"""

from __future__ import annotations

import json
import sys
import threading
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import lily_bridge  # noqa: E402


class _FakeLilyHandler(BaseHTTPRequestHandler):
    requests: list[dict] = []
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        pass

    def _json(self, status: int, body: dict) -> None:
        encoded = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/health":
            self._json(200, {"status": "ok"})
        elif self.path == "/v1/models":
            self._json(
                200,
                {
                    "object": "list",
                    "data": [
                        {
                            "id": lily_bridge.MODEL_ID,
                            "object": "model",
                            "created": 0,
                            "owned_by": "lily",
                        }
                    ],
                },
            )
        else:
            self._json(404, {"error": {"message": "not found"}})

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length))
        type(self).requests.append(body)
        assert self.path == "/v1/chat/completions"
        assert body["model"] == lily_bridge.MODEL_ID
        assert body["stream"] is False
        assert "tools" not in body
        self._json(
            200,
            {
                "id": "chatcmpl-test",
                "object": "chat.completion",
                "created": 1,
                "model": lily_bridge.MODEL_ID,
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "hello from lily"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": 120,
                    "completion_tokens": 4,
                    "total_tokens": 124,
                    "prompt_tokens_details": {"cached_tokens": 96},
                },
            },
        )


def _start_fake_lily():
    _FakeLilyHandler.requests = []
    server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeLilyHandler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def _start_bridge(fake_lily):
    client = lily_bridge.LilyClient(
        f"http://127.0.0.1:{fake_lily.server_address[1]}"
    )
    client.verify()
    handler = type(
        "TestBridgeHandler",
        (lily_bridge.BridgeHandler,),
        {"client": client},
    )
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def _post(server, body):
    connection = HTTPConnection("127.0.0.1", server.server_address[1], timeout=5)
    encoded = json.dumps(body)
    connection.request(
        "POST",
        "/v1/chat/completions",
        body=encoded,
        headers={"Content-Type": "application/json"},
    )
    response = connection.getresponse()
    payload = response.read().decode("utf-8")
    status = response.status
    connection.close()
    return status, payload


def check_loopback_only():
    for forbidden in (
        "https://example.com:8000",
        "http://192.168.1.10:8000",
        "file:///tmp/lily",
    ):
        try:
            lily_bridge.LilyClient(forbidden)
        except ValueError:
            continue
        raise AssertionError(f"accepted non-loopback Lily URL: {forbidden}")
    lily_bridge.LilyClient("http://127.0.0.1:8000")
    lily_bridge.LilyClient("http://localhost:8000")
    print("ok: Lily upstream is loopback-only")


def check_stream_adapter_preserves_usage_and_cache_evidence():
    upstream = _start_fake_lily()
    bridge = _start_bridge(upstream)
    try:
        status, payload = _post(
            bridge,
            {
                "model": lily_bridge.MODEL_ID,
                "messages": [{"role": "user", "content": "hello"}],
                "tools": [],
                "stream": True,
                "prompt_cache_key": "conversation-1",
            },
        )
    finally:
        bridge.shutdown()
        upstream.shutdown()
    assert status == 200, payload
    data_lines = [
        line[len("data:") :].strip()
        for line in payload.splitlines()
        if line.startswith("data:")
    ]
    assert data_lines[-1] == "[DONE]", data_lines
    chunks = [json.loads(line) for line in data_lines[:-1]]
    assert chunks[0]["choices"][0]["delta"]["content"] == "hello from lily"
    assert chunks[1]["choices"][0]["finish_reason"] == "stop"
    assert chunks[2]["usage"]["prompt_tokens_details"]["cached_tokens"] == 96
    assert _FakeLilyHandler.requests[-1]["prompt_cache_key"] == "conversation-1"
    assert _FakeLilyHandler.requests[-1]["stream"] is False
    print("ok: blocking Lily response becomes SSE without losing cache usage")


def check_tools_fail_closed():
    upstream = _start_fake_lily()
    bridge = _start_bridge(upstream)
    try:
        status, payload = _post(
            bridge,
            {
                "model": lily_bridge.MODEL_ID,
                "messages": [{"role": "user", "content": "use a tool"}],
                "tools": [{"type": "function", "function": {"name": "shell_exec"}}],
                "stream": True,
            },
        )
    finally:
        bridge.shutdown()
        upstream.shutdown()
    assert status == 400, payload
    assert "does not support tools" in payload
    assert not _FakeLilyHandler.requests, "unsupported tool request reached Lily"
    print("ok: tool-bearing requests are refused before Lily")


def check_non_text_messages_fail_closed():
    upstream = _start_fake_lily()
    bridge = _start_bridge(upstream)
    try:
        status, payload = _post(
            bridge,
            {
                "model": lily_bridge.MODEL_ID,
                "messages": [
                    {
                        "role": "user",
                        "content": [{"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}],
                    }
                ],
                "stream": True,
            },
        )
    finally:
        bridge.shutdown()
        upstream.shutdown()
    assert status == 400, payload
    assert "text-only" in payload
    assert not _FakeLilyHandler.requests, "multimodal request reached text-only Lily"
    print("ok: multimodal requests are refused rather than stripped")


if __name__ == "__main__":
    check_loopback_only()
    check_stream_adapter_preserves_usage_and_cache_evidence()
    check_tools_fail_closed()
    check_non_text_messages_fail_closed()
    print("all lily bridge contract checks passed")

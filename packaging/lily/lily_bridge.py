"""Loopback-only compatibility bridge from Little Monkey to Perplexity Lily.

Lily intentionally exposes a tiny, blocking subset of OpenAI chat completions:
text-only Qwen3.6-35B-A3B, greedy decoding, ``stream: false``, and no tools.
Little Monkey's generic provider transport expects OpenAI SSE streaming.  This
bridge adapts only that transport mismatch; it does not widen Lily's model or
hardware support and it never discards unsupported capabilities silently.

Run Lily first, then:

    python3 packaging/lily/lily_bridge.py --listen-port 18000

Configure a Little Monkey custom provider at ``http://127.0.0.1:18000/v1``.
The existing custom-provider UI requires a non-empty API-key field; use a local
sentinel such as ``local``.  The bridge never forwards Authorization to Lily.
"""

from __future__ import annotations

import argparse
import json
import sys
from http.client import HTTPConnection, HTTPSConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

MODEL_ID = "Qwen3.6-35B-A3B"
DEFAULT_LILY_BASE_URL = "http://127.0.0.1:8000"
MAX_REQUEST_BYTES = 1 * 1024 * 1024
UPSTREAM_TIMEOUT_SECONDS = 300
_LOOPBACK_HOSTS = {"127.0.0.1", "::1", "localhost"}


class BridgeError(Exception):
    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status
        self.message = message


def _parse_loopback_base_url(raw: str) -> tuple[str, str, int, str]:
    parsed = urlsplit(raw.rstrip("/"))
    if parsed.scheme not in {"http", "https"}:
        raise ValueError("Lily base URL must use http or https")
    if parsed.hostname not in _LOOPBACK_HOSTS:
        raise ValueError("Lily base URL must resolve by an explicit loopback hostname")
    if parsed.username is not None or parsed.password is not None:
        raise ValueError("Lily base URL must not contain credentials")
    if parsed.query or parsed.fragment:
        raise ValueError("Lily base URL must not contain a query or fragment")
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    base_path = parsed.path.rstrip("/")
    return parsed.scheme, parsed.hostname or "", port, base_path


class LilyClient:
    def __init__(self, base_url: str, timeout: int = UPSTREAM_TIMEOUT_SECONDS) -> None:
        self.scheme, self.host, self.port, self.base_path = _parse_loopback_base_url(base_url)
        self.timeout = timeout

    def _connection(self):
        connection_type = HTTPSConnection if self.scheme == "https" else HTTPConnection
        return connection_type(self.host, self.port, timeout=self.timeout)

    def request_json(self, method: str, path: str, body: dict | None = None) -> tuple[int, dict]:
        connection = self._connection()
        encoded = None if body is None else json.dumps(body, separators=(",", ":")).encode("utf-8")
        headers = {"Accept": "application/json"}
        if encoded is not None:
            headers["Content-Type"] = "application/json"
        try:
            connection.request(method, f"{self.base_path}{path}", body=encoded, headers=headers)
            response = connection.getresponse()
            payload = response.read(MAX_REQUEST_BYTES + 1)
            if len(payload) > MAX_REQUEST_BYTES:
                raise BridgeError(502, "Lily returned a response larger than the bridge limit")
            try:
                parsed = json.loads(payload or b"{}")
            except ValueError as error:
                raise BridgeError(502, f"Lily returned invalid JSON: {error}") from error
            if not isinstance(parsed, dict):
                raise BridgeError(502, "Lily returned a non-object JSON response")
            return response.status, parsed
        except OSError as error:
            raise BridgeError(502, f"Could not reach Lily on loopback: {error}") from error
        finally:
            connection.close()

    def verify(self) -> None:
        status, health = self.request_json("GET", "/health")
        if status != 200 or health.get("status") != "ok":
            raise BridgeError(503, "Lily health check failed")
        status, models = self.request_json("GET", "/v1/models")
        exposed = {
            entry.get("id")
            for entry in models.get("data", [])
            if isinstance(entry, dict) and isinstance(entry.get("id"), str)
        }
        if status != 200 or exposed != {MODEL_ID}:
            raise BridgeError(
                503,
                f"Lily must expose exactly {MODEL_ID}; got {sorted(exposed)}",
            )

    def complete(self, request: dict) -> dict:
        tools = request.get("tools") or []
        if tools:
            raise BridgeError(400, "Lily does not support tools; select a tool-free chat mode")
        if request.get("model") != MODEL_ID:
            raise BridgeError(400, f"Lily exposes only {MODEL_ID}")

        messages = request.get("messages")
        if not isinstance(messages, list) or not messages:
            raise BridgeError(400, "messages must be a non-empty array")
        normalized = []
        for message in messages:
            if not isinstance(message, dict):
                raise BridgeError(400, "every message must be an object")
            role = message.get("role")
            content = message.get("content")
            if role not in {"system", "user", "assistant"} or not isinstance(content, str):
                raise BridgeError(400, "Lily accepts text-only system/user/assistant messages")
            normalized.append({"role": role, "content": content})

        upstream = {
            "model": MODEL_ID,
            "messages": normalized,
            "max_tokens": int(request.get("max_tokens") or 1024),
            "stream": False,
        }
        prompt_cache_key = request.get("prompt_cache_key")
        if prompt_cache_key is not None:
            if not isinstance(prompt_cache_key, str) or not prompt_cache_key:
                raise BridgeError(400, "prompt_cache_key must be a non-empty string")
            upstream["prompt_cache_key"] = prompt_cache_key

        status, response = self.request_json("POST", "/v1/chat/completions", upstream)
        if status != 200:
            error = response.get("error")
            detail = error.get("message") if isinstance(error, dict) else None
            raise BridgeError(status, str(detail or "Lily completion failed"))
        return response


class BridgeHandler(BaseHTTPRequestHandler):
    client: LilyClient
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        sys.stderr.write("lily-bridge %s\n" % (format % args))

    def _json(self, status: int, payload: dict) -> None:
        encoded = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(encoded)

    def _error(self, error: BridgeError) -> None:
        self._json(
            error.status,
            {"error": {"message": error.message, "type": "invalid_request_error"}},
        )

    def do_GET(self) -> None:  # noqa: N802
        try:
            if self.path == "/health":
                self.client.verify()
                self._json(200, {"status": "ok", "backend": "lily", "model": MODEL_ID})
                return
            if self.path == "/v1/models":
                self.client.verify()
                self._json(
                    200,
                    {
                        "object": "list",
                        "data": [
                            {
                                "id": MODEL_ID,
                                "object": "model",
                                "created": 0,
                                "owned_by": "lily",
                            }
                        ],
                    },
                )
                return
            self._json(404, {"error": {"message": "not found", "type": "invalid_request_error"}})
        except BridgeError as error:
            self._error(error)

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/v1/chat/completions":
            self._json(404, {"error": {"message": "not found", "type": "invalid_request_error"}})
            return
        try:
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError as error:
                raise BridgeError(400, "invalid Content-Length") from error
            if length <= 0 or length > MAX_REQUEST_BYTES:
                raise BridgeError(413, "request body exceeds the bridge limit")
            try:
                request = json.loads(self.rfile.read(length))
            except ValueError as error:
                raise BridgeError(400, "request body is not valid JSON") from error
            if not isinstance(request, dict):
                raise BridgeError(400, "request body must be a JSON object")

            response = self.client.complete(request)
            if request.get("stream") is False:
                self._json(200, response)
                return
            self._stream_response(response)
        except (BridgeError, ValueError, TypeError) as error:
            if isinstance(error, BridgeError):
                self._error(error)
            else:
                self._error(BridgeError(400, str(error)))

    def _stream_response(self, response: dict) -> None:
        choices = response.get("choices")
        if not isinstance(choices, list) or len(choices) != 1 or not isinstance(choices[0], dict):
            raise BridgeError(502, "Lily returned an invalid choices array")
        choice = choices[0]
        message = choice.get("message")
        if not isinstance(message, dict) or not isinstance(message.get("content"), str):
            raise BridgeError(502, "Lily returned no assistant text")
        text = message["content"]
        finish_reason = str(choice.get("finish_reason") or "stop")
        usage = response.get("usage") if isinstance(response.get("usage"), dict) else None
        completion_id = str(response.get("id") or "chatcmpl-lily-bridge")

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Connection", "close")
        self.end_headers()

        def emit(payload: dict) -> None:
            line = f"data: {json.dumps(payload, separators=(',', ':'))}\n\n".encode("utf-8")
            self.wfile.write(line)
            self.wfile.flush()

        base = {
            "id": completion_id,
            "object": "chat.completion.chunk",
            "created": response.get("created", 0),
            "model": MODEL_ID,
        }
        emit(
            {
                **base,
                "choices": [
                    {"index": 0, "delta": {"role": "assistant", "content": text}, "finish_reason": None}
                ],
            }
        )
        emit(
            {
                **base,
                "choices": [{"index": 0, "delta": {}, "finish_reason": finish_reason}],
            }
        )
        if usage is not None:
            emit({**base, "choices": [], "usage": usage})
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Little Monkey compatibility bridge for Lily")
    parser.add_argument("--listen-host", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, default=18000)
    parser.add_argument("--lily-base-url", default=DEFAULT_LILY_BASE_URL)
    args = parser.parse_args(argv)
    if args.listen_host != "127.0.0.1":
        parser.error("--listen-host must be 127.0.0.1")

    try:
        client = LilyClient(args.lily_base_url)
        client.verify()
    except (ValueError, BridgeError) as error:
        parser.error(str(error))

    handler = type("ConfiguredBridgeHandler", (BridgeHandler,), {"client": client})
    server = ThreadingHTTPServer((args.listen_host, args.listen_port), handler)
    sys.stderr.write(
        f"lily-bridge listening on http://{args.listen_host}:{args.listen_port}; "
        f"upstream={args.lily_base_url}; model={MODEL_ID}\n"
    )
    sys.stderr.flush()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

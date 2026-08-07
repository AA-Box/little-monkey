"""Checks mlx_server against the protocol the Rust supervisor enforces.

Run: `python3 packaging/mlx/service/test_mlx_server.py`

MLX itself is stubbed. What is under test is the wire contract — the event
names, the terminal `completed`, the framing — none of which depends on real
weights, and all of which fails the whole request when it drifts. Requiring an
Apple-silicon machine with a model on disk to catch a renamed JSON key would
mean never catching it.
"""

import json
import sys
import threading
import types
import urllib.request
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

# Installed before importing the server so its deferred `from mlx_lm import ...`
# resolves to these fakes rather than a missing dependency.
_fake = types.ModuleType("mlx_lm")
_fake.load = lambda path: (object(), object())
_fake.stream_generate = lambda *args, **kwargs: iter(())
sys.modules["mlx_lm"] = _fake
sys.modules["mlx_lm.sample_utils"] = types.ModuleType("mlx_lm.sample_utils")
sys.modules["mlx_lm.sample_utils"].make_sampler = lambda temp: ("sampler", temp)

import mlx_server  # noqa: E402


class _Tokenizer:
    chat_template = None

    def encode(self, text):
        return text.split()


def _serve(chunks, capture):
    """Stands the real handler up on a loopback port with a stubbed model."""

    class Handler(mlx_server._Handler):
        model = object()
        tokenizer = _Tokenizer()

        def _generate(self, prompt, max_tokens, temperature):
            capture["prompt"] = prompt
            capture["max_tokens"] = max_tokens
            capture["temperature"] = temperature
            yield from chunks()

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def _generate(server, body):
    """Posts a request and returns the decoded event list, as the Rust side
    parses it: strip `data:`, one JSON object per line."""
    connection = HTTPConnection("127.0.0.1", server.server_address[1])
    connection.request(
        "POST",
        "/v1/generate",
        body=json.dumps(body),
        headers={"Content-Type": "application/json"},
    )
    response = connection.getresponse()
    assert response.status == 200, response.status
    events = []
    for line in response.read().decode("utf-8").splitlines():
        line = line.strip()
        if not line or not line.startswith("data:"):
            continue
        events.append(json.loads(line[len("data:") :].strip()))
    connection.close()
    return events


REQUEST = {
    "requestId": "req-1",
    "modelId": "m",
    "messages": [{"role": "user", "text": "hello there", "images": []}],
    "tools": [],
    "maxTokens": 16,
    "temperature": 0.5,
    "structuredOutputSchema": None,
}

# The event names and field names the Rust enum accepts. Anything else is a
# `deny_unknown_fields` failure that kills the request.
ALLOWED = {
    "started": {"request_id"},
    "text_delta": {"text"},
    "tool_call_start": {"call_id", "name"},
    "tool_call_arguments_delta": {"call_id", "json"},
    "tool_call_end": {"call_id"},
    "completed": {"input_tokens", "output_tokens"},
    "error": {"code", "message"},
}


def check_happy_path():
    capture = {}
    server = _serve(lambda: iter(["Hi", " there"]), capture)
    try:
        events = _generate(server, REQUEST)
    finally:
        server.shutdown()

    for event in events:
        kind = event["type"]
        assert kind in ALLOWED, f"unknown event type {kind}"
        assert set(event) - {"type"} == ALLOWED[kind], f"{kind} carries the wrong fields: {event}"

    assert events[0] == {"type": "started", "request_id": "req-1"}
    assert [e["text"] for e in events if e["type"] == "text_delta"] == ["Hi", " there"]

    terminal = [e for e in events if e["type"] == "completed"]
    assert len(terminal) == 1, "exactly one completed event or the supervisor fails the run"
    assert terminal[0] is events[-1], "completed must be last"
    assert terminal[0]["output_tokens"] == 2
    assert terminal[0]["input_tokens"] == 2, "input tokens come from the rendered prompt"

    assert capture["max_tokens"] == 16
    assert capture["temperature"] == 0.5
    assert "hello there" in capture["prompt"]
    print("ok: happy path emits started, deltas, and exactly one completed")


def check_generation_failure_still_terminates():
    """A model that dies mid-stream must still close the protocol.

    Without the terminal event the supervisor reports "stream ended without a
    completed event" and the real cause — which the error event carries — never
    reaches the user.
    """

    def explode():
        yield "partial"
        raise RuntimeError("out of memory")

    server = _serve(explode, {})
    try:
        events = _generate(server, REQUEST)
    finally:
        server.shutdown()

    kinds = [event["type"] for event in events]
    assert "error" in kinds, kinds
    assert kinds[-1] == "completed", "the stream terminates even after a failure"
    assert "out of memory" in next(e for e in events if e["type"] == "error")["message"]
    print("ok: a failed generation still emits error then completed")


def check_rejects_non_loopback_host():
    """The supervisor always passes loopback; refusing anything else means a
    tampered argument vector cannot expose this on the network."""
    try:
        mlx_server.main(["--host", "0.0.0.0", "--port", "1", "--model", "m"])
    except SystemExit as exit_code:
        assert exit_code.code != 0
        print("ok: a non-loopback --host is refused")
        return
    raise AssertionError("expected --host 0.0.0.0 to be refused")


def check_unknown_endpoint_is_404():
    server = _serve(lambda: iter(()), {})
    try:
        request = urllib.request.Request(
            f"http://127.0.0.1:{server.server_address[1]}/v1/chat", data=b"{}"
        )
        try:
            urllib.request.urlopen(request)
        except urllib.error.HTTPError as error:
            assert error.code == 404, error.code
            print("ok: an unknown endpoint is 404")
            return
        raise AssertionError("expected 404")
    finally:
        server.shutdown()


if __name__ == "__main__":
    check_happy_path()
    check_generation_failure_still_terminates()
    check_rejects_non_loopback_host()
    check_unknown_endpoint_is_404()
    print("all mlx_server contract checks passed")

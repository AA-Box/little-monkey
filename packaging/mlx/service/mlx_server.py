"""The MLX generation service the app supervises.

This file is the `serviceEntry` of a signed MLX package. The app never runs a
Python it did not install: `MlxPackageManifest` names both the interpreter and
this script, both are digest-checked on every launch, and the whole tree is
Ed25519-verified before a byte of it reaches disk. Nothing here reads the
user's PATH, environment Python, or site-packages.

The contract, in full, is what `ProductionMlxServiceController` in
`src-tauri/src/m3_production.rs` sends and parses:

  * launched as `<python> <this file> --host 127.0.0.1 --port N --model PATH`
  * bind loopback only, and keep the port closed until the model is loaded —
    the supervisor treats a connectable port as readiness
  * `POST /v1/generate` with a camelCase `MlxGenerationRequest` body
  * respond with newline-delimited `data: <json>` lines, each a
    `MlxStreamEvent` tagged by a snake_case `type`
  * the stream MUST carry exactly one terminal `completed` event, or the
    supervisor rejects the whole run as a protocol error

`deny_unknown_fields` is set on the Rust side of every one of those structs, so
an extra key here is a hard failure rather than a warning. Emit only what the
schema names.
"""

from __future__ import annotations

import argparse
import json
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Bounded so a malformed or hostile local caller cannot make the service
# allocate without limit. The supervisor caps its own side of the stream at
# 64 MiB; this is the request half of the same idea.
MAX_REQUEST_BYTES = 16 * 1024 * 1024


def _load_model(model_path: str):
    """Loads the weights before the port opens.

    Import is deferred to here so that `--help` and argument errors do not pay
    for importing MLX, and so a broken install reports the missing dependency
    as a clean startup failure on stderr rather than an import traceback at the
    first request — by which time the supervisor has already called the service
    ready and a user is waiting on a reply.
    """
    from mlx_lm import load  # noqa: PLC0415 - deliberate: see docstring

    return load(model_path)


class _Handler(BaseHTTPRequestHandler):
    # Set on the server instance by `main`.
    model = None
    tokenizer = None

    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: object) -> None:
        """Logs to stderr, which the supervisor tails, in one line per event."""
        sys.stderr.write("mlx-service %s\n" % (format % args))

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's spelling
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

        # Headers go out before the first token so the supervisor can start
        # reading; chunked because the length is unknowable up front.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-store")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            self._stream(request)
        except BrokenPipeError:
            # The supervisor cancelled by dropping the connection. Not an error.
            pass

    def _emit(self, event: dict) -> None:
        """Writes one event and flushes it.

        Without the flush the events arrive in buffer-sized clumps and the
        stream stops being a stream — the user waits for the whole reply and
        then sees it appear at once.
        """
        payload = ("data: %s\n" % json.dumps(event, separators=(",", ":"))).encode("utf-8")
        self.wfile.write(b"%x\r\n%s\r\n" % (len(payload), payload))
        self.wfile.flush()

    def _stream(self, request: dict) -> None:
        request_id = str(request.get("requestId", ""))
        self._emit({"type": "started", "request_id": request_id})

        prompt = self._prompt_from(request)
        max_tokens = int(request.get("maxTokens") or 512)
        temperature = request.get("temperature")

        input_tokens = len(self.tokenizer.encode(prompt))
        output_tokens = 0
        try:
            for text in self._generate(prompt, max_tokens, temperature):
                output_tokens += 1
                if text:
                    self._emit({"type": "text_delta", "text": text})
        except Exception as error:  # noqa: BLE001 - any failure must reach the user
            # An error event is part of the protocol; a dropped connection is
            # not, and would surface as an unexplained "stream ended" instead of
            # the reason the model actually stopped.
            self._emit({"type": "error", "code": "generation_failed", "message": str(error)})
        # Exactly one terminal event, on every path including the error one:
        # the supervisor fails the whole request without it.
        self._emit(
            {
                "type": "completed",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
            }
        )
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _generate(self, prompt: str, max_tokens: int, temperature):
        from mlx_lm import stream_generate  # noqa: PLC0415 - see _load_model

        sampler = None
        if temperature is not None:
            from mlx_lm.sample_utils import make_sampler  # noqa: PLC0415

            sampler = make_sampler(temp=float(temperature))
        for response in stream_generate(
            self.model,
            self.tokenizer,
            prompt,
            max_tokens=max_tokens,
            **({"sampler": sampler} if sampler is not None else {}),
        ):
            yield response.text

    def _prompt_from(self, request: dict) -> str:
        """Renders the turns with the model's own chat template.

        Falling back to a plain join rather than failing: a base model with no
        template still generates, and refusing to talk to one would be a
        stricter rule than the runtime it replaces.
        """
        messages = [
            {"role": str(message.get("role", "user")), "content": str(message.get("text", ""))}
            for message in request.get("messages", [])
        ]
        template = getattr(self.tokenizer, "apply_chat_template", None)
        if template is not None and getattr(self.tokenizer, "chat_template", None):
            return template(messages, tokenize=False, add_generation_prompt=True)
        return "\n".join(message["content"] for message in messages)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="MLX generation service")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, required=True)
    parser.add_argument("--model", required=True)
    arguments = parser.parse_args(argv)

    if arguments.host != "127.0.0.1":
        # The supervisor always passes loopback. Refusing anything else means a
        # tampered argument vector cannot turn this into a network service.
        parser.error("--host must be 127.0.0.1")

    _Handler.model, _Handler.tokenizer = _load_model(arguments.model)

    server = ThreadingHTTPServer((arguments.host, arguments.port), _Handler)
    # Only after the weights are resident: the supervisor reads a connectable
    # port as "ready", so binding earlier would advertise a service that then
    # stalls for the minute it takes to page a model in.
    threading.current_thread().name = "mlx-service"
    sys.stderr.write("mlx-service listening on %s:%d\n" % (arguments.host, arguments.port))
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

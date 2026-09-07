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
import os
import queue
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

# Bounded so a malformed or hostile local caller cannot make the service
# allocate without limit. The supervisor caps its own side of the stream at
# 64 MiB; this is the request half of the same idea.
MAX_REQUEST_BYTES = 16 * 1024 * 1024
# `config.json` is small by construction; a larger one is not a model config.
MAX_CONFIG_BYTES = 4 * 1024 * 1024
MAX_PROMPT_CACHE_KEY_BYTES = 256
PROMPT_CACHE_ENTRIES = 2


def _read_config(model_path: str) -> dict:
    """The model's own `config.json`, or `{}` if it has none this can read."""
    try:
        with open(os.path.join(model_path, "config.json"), "rb") as handle:
            return json.loads(handle.read(MAX_CONFIG_BYTES))
    except (OSError, ValueError):
        return {}


def _is_vision_model(config: dict) -> bool:
    """Whether this checkout carries a vision tower.

    `vision_config` is the transformers convention for a sub-config describing
    one, and it is what a multimodal MLX conversion keeps. Read from the model's
    own files rather than from its name or its hub tags: those are metadata a
    repository author writes, and loading a text model with the vision stack
    (or the reverse) fails deep inside the loader with a shape error.
    """
    return isinstance(config.get("vision_config"), dict)


def _load_model(model_path: str):
    """Loads the weights before the port opens, with the stack this model needs.

    Imports are deferred to here so that `--help` and argument errors do not pay
    for importing MLX, and so a broken install reports the missing dependency
    as a clean startup failure on stderr rather than an import traceback at the
    first request — by which time the supervisor has already called the service
    ready and a user is waiting on a reply.
    """
    config = _read_config(model_path)
    if _is_vision_model(config):
        from mlx_vlm import load  # noqa: PLC0415 - deliberate: see docstring

        model, processor = load(model_path)
        return _VisionRuntime(model, processor, config)
    from mlx_lm import load  # noqa: PLC0415 - deliberate: see docstring

    model, tokenizer = load(model_path)
    return _TextRuntime(model, tokenizer)


def _strict_prefix(prefix: list, full: list) -> bool:
    return len(prefix) < len(full) and full[: len(prefix)] == prefix


class _PromptCacheEntry:
    def __init__(self, key, tokens: list, prompt_cache, last_used: int) -> None:
        self.key = key
        self.tokens = list(tokens)
        self.prompt_cache = prompt_cache
        self.last_used = last_used


class _SessionPrefixCache:
    """Fixed two-entry LRU of model decode states.

    The rule intentionally mirrors Lily's safety property: a state is reused
    only when its token sequence is a *strict* prefix of the new prompt. An
    explicit key merely prefers the matching entry; it never bypasses token
    equality. Entries are checked out while a generation owns them, so no MLX
    cache object is shared by two requests.
    """

    def __init__(self, capacity: int = PROMPT_CACHE_ENTRIES) -> None:
        self.capacity = max(0, int(capacity))
        self.entries: list[_PromptCacheEntry] = []
        self.clock = 0
        self.total_hits = 0
        self.total_cached_tokens = 0

    def acquire(self, model, prompt_tokens: list, key):
        candidates = [
            (index, entry)
            for index, entry in enumerate(self.entries)
            if _strict_prefix(entry.tokens, prompt_tokens)
        ]
        if candidates:
            index, entry = max(
                candidates,
                key=lambda item: (
                    key is not None and item[1].key == key,
                    len(item[1].tokens),
                    item[1].last_used,
                ),
            )
            self.entries.pop(index)
            reused = len(entry.tokens)
            self.total_hits += 1
            self.total_cached_tokens += reused
            return entry.prompt_cache, reused

        from mlx_lm.models.cache import make_prompt_cache  # noqa: PLC0415

        return make_prompt_cache(model), 0

    def release(self, key, tokens: list, prompt_cache) -> None:
        if self.capacity == 0 or not tokens:
            return
        self.clock += 1
        self.entries.append(_PromptCacheEntry(key, tokens, prompt_cache, self.clock))
        while len(self.entries) > self.capacity:
            oldest = min(range(len(self.entries)), key=lambda index: self.entries[index].last_used)
            self.entries.pop(oldest)


class _TextRuntime:
    """A text-only model, generated with `mlx_lm`."""

    supports_images = False

    def __init__(self, model, tokenizer) -> None:
        self.model = model
        self.tokenizer = tokenizer
        self.prefix_cache = _SessionPrefixCache()

    def render(self, messages: list[dict], image_count: int) -> str:
        """Renders the turns with the model's own chat template.

        Falling back to a plain join rather than failing: a base model with no
        template still generates, and refusing to talk to one would be a
        stricter rule than the runtime it replaces.
        """
        template = getattr(self.tokenizer, "apply_chat_template", None)
        if template is not None and getattr(self.tokenizer, "chat_template", None):
            return template(messages, tokenize=False, add_generation_prompt=True)
        return "\n".join(message["content"] for message in messages)

    def stream(
        self,
        prompt: str,
        images: list[str],
        max_tokens: int,
        temperature,
        prompt_cache_key=None,
        on_cache_hit=lambda _count: None,
    ):
        from mlx_lm import stream_generate  # noqa: PLC0415 - see _load_model

        prompt_tokens = list(self.tokenizer.encode(prompt))
        prompt_cache, cached_input_tokens = self.prefix_cache.acquire(
            self.model, prompt_tokens, prompt_cache_key
        )
        on_cache_hit(cached_input_tokens)
        suffix = prompt_tokens[cached_input_tokens:]

        sampler = None
        if temperature is not None:
            from mlx_lm.sample_utils import make_sampler  # noqa: PLC0415

            sampler = make_sampler(temp=float(temperature))

        generated_tokens = []
        cacheable = True
        completed = False
        try:
            for response in stream_generate(
                self.model,
                self.tokenizer,
                suffix,
                max_tokens=max_tokens,
                prompt_cache=prompt_cache,
                **({"sampler": sampler} if sampler is not None else {}),
            ):
                token = getattr(response, "token", None)
                if token is None:
                    # A future mlx-lm response shape that stops exposing token
                    # ids can still generate text, but cannot prove that cache
                    # state and our token ledger describe the same prefix.
                    cacheable = False
                else:
                    generated_tokens.append(token)
                yield response.text
            completed = True
        finally:
            # A cancelled/dropped/erroring generation is not cached. Its decode
            # state may have advanced to a token sequence we did not observe all
            # the way to a clean iterator end.
            if completed and cacheable:
                self.prefix_cache.release(
                    prompt_cache_key,
                    prompt_tokens + generated_tokens,
                    prompt_cache,
                )


class _VisionRuntime:
    """A vision-language model, generated with `mlx_vlm`.

    The two stacks are not interchangeable: `mlx_vlm` owns the image
    preprocessing, and its chat template needs to know how many images a turn
    carries so it can place the image tokens the model was trained on.
    """

    supports_images = True

    def __init__(self, model, processor, config: dict) -> None:
        self.model = model
        self.processor = processor
        self.config = config
        # The handler counts prompt tokens with this; a processor exposes the
        # tokenizer it wraps.
        self.tokenizer = getattr(processor, "tokenizer", processor)

    def render(self, messages: list[dict], image_count: int) -> str:
        from mlx_vlm import apply_chat_template  # noqa: PLC0415 - see _load_model

        return apply_chat_template(
            self.processor, self.config, messages, num_images=image_count
        )

    def stream(
        self,
        prompt: str,
        images: list[str],
        max_tokens: int,
        temperature,
        prompt_cache_key=None,
        on_cache_hit=lambda _count: None,
    ):
        from mlx_vlm import stream_generate  # noqa: PLC0415 - see _load_model

        # Image preprocessing and multimodal cache ownership are separate from
        # mlx-lm's text prompt cache. Do not advertise reuse until the VLM stack
        # can prove equivalent token/image state.
        on_cache_hit(0)
        for response in stream_generate(
            self.model,
            self.processor,
            prompt,
            image=images or None,
            max_tokens=max_tokens,
            **({"temperature": float(temperature)} if temperature is not None else {}),
        ):
            yield response.text


class _Job:
    """One generation, run on the worker thread, consumed by a request thread."""

    _DONE = object()

    def __init__(
        self,
        prompt: str,
        images: list[str],
        max_tokens: int,
        temperature,
        prompt_cache_key=None,
    ) -> None:
        self.prompt = prompt
        self.images = images
        self.max_tokens = max_tokens
        self.temperature = temperature
        self.prompt_cache_key = prompt_cache_key
        self.cached_input_tokens = 0
        # Bounded: a client that stops reading must slow the model down rather
        # than let deltas pile up without limit in memory.
        self._output: queue.Queue = queue.Queue(maxsize=256)
        self._cancelled = threading.Event()

    def cancel(self) -> None:
        self._cancelled.set()

    def run(self, runtime) -> None:
        """Runs on the worker thread. Every MLX array stays on that thread."""
        try:
            for text in runtime.stream(
                self.prompt,
                self.images,
                self.max_tokens,
                self.temperature,
                self.prompt_cache_key,
                self._record_cache_hit,
            ):
                if self._cancelled.is_set():
                    break
                if not self._put(text):
                    break
        except Exception as error:  # noqa: BLE001 - any failure must reach the user
            self._put(error)
        self._output.put(self._DONE)

    def _record_cache_hit(self, cached_input_tokens: int) -> None:
        self.cached_input_tokens = max(0, int(cached_input_tokens))

    def _put(self, item) -> bool:
        """Hands one item to the reader, giving up if it has gone away.

        A dropped connection is the supervisor cancelling. Blocking forever on
        a full queue nobody is draining would wedge the worker thread, and with
        it every later request.
        """
        while not self._cancelled.is_set():
            try:
                self._output.put(item, timeout=0.1)
                return True
            except queue.Full:
                continue
        return False

    def deltas(self):
        """Yields text on the request thread until the generation ends."""
        while True:
            item = self._output.get()
            if item is self._DONE:
                return
            if isinstance(item, Exception):
                raise item
            yield item


class _GenerationWorker:
    """Owns the model, and every MLX array made from it.

    MLX binds a stream to the thread that created the arrays. Generating on any
    other thread raises "There is no Stream(gpu, 0) in current thread." (mlx
    0.32.0), and no per-thread `set_default_device` or `set_default_stream`
    recovers it — the weights were loaded elsewhere. A threading HTTP server
    that generates inside the request handler therefore fails *every* request
    while looking perfectly healthy: port connectable, model resident, each
    generation dying inside `stream_generate`.

    So one thread loads the model and runs every generation, and request
    threads hand it work. The server stays threaded, which keeps `/health`,
    unknown routes and a second caller answerable while a generation is in
    flight — the alternative, a single-threaded server, would let one slow
    reader block the supervisor's next connection entirely.
    """

    def __init__(self, model_path: str) -> None:
        self.runtime = None
        self._failure: BaseException | None = None
        self._loaded = threading.Event()
        self._jobs: queue.Queue = queue.Queue()
        self._thread = threading.Thread(
            target=self._run, args=(model_path,), name="mlx-generate", daemon=True
        )
        self._thread.start()

    def wait_until_loaded(self) -> None:
        """Blocks until the weights are resident, re-raising a load failure.

        The supervisor reads a connectable port as ready, so `main` must not
        bind before this returns.
        """
        self._loaded.wait()
        if self._failure is not None:
            raise self._failure

    def submit(self, job: _Job) -> None:
        self._jobs.put(job)

    def _run(self, model_path: str) -> None:
        try:
            self.runtime = _load_model(model_path)
        except BaseException as error:  # noqa: BLE001 - reported by wait_until_loaded
            self._failure = error
            self._loaded.set()
            return
        self._loaded.set()
        while True:
            self._jobs.get().run(self.runtime)


class _Handler(BaseHTTPRequestHandler):
    # Set on the class by `main`.
    worker = None
    runtime = None

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

        try:
            images = self._images_from(request)
            prompt_cache_key = self._prompt_cache_key_from(request)
        except ValueError as error:
            self._emit({"type": "error", "code": "invalid_request", "message": str(error)})
            self._emit({"type": "completed", "input_tokens": 0, "output_tokens": 0, "cached_input_tokens": 0})
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
            return
        prompt = self._prompt_from(request, len(images))
        max_tokens = int(request.get("maxTokens") or 512)
        temperature = request.get("temperature")

        input_tokens = len(self.runtime.tokenizer.encode(prompt))
        output_tokens = 0
        self._cached_input_tokens = 0
        try:
            for text in self._generate(
                prompt,
                images,
                max_tokens,
                temperature,
                prompt_cache_key,
            ):
                output_tokens += 1
                if text:
                    self._emit({"type": "text_delta", "text": text})
        except Exception as error:  # noqa: BLE001 - any failure must reach the user
            # An error event is part of the protocol; a dropped connection is
            # not, and would surface as an unexplained "stream ended" instead of
            # the reason the model actually stopped.
            self._emit({"type": "error", "code": "generation_failed", "message": str(error)})
        # Exactly one terminal event, on every path including the error one.
        # `cached_input_tokens` is measured by the MLX decode cache itself and
        # crosses the supervised ABI so M3 can persist it as canonical usage.
        self._emit(
            {
                "type": "completed",
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cached_input_tokens": self._cached_input_tokens,
            }
        )
        sys.stderr.write(
            "mlx-service request=%s input_tokens=%d output_tokens=%d cached_input_tokens=%d\n"
            % (request_id, input_tokens, output_tokens, self._cached_input_tokens)
        )
        sys.stderr.flush()
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _generate(
        self,
        prompt: str,
        images: list,
        max_tokens: int,
        temperature,
        prompt_cache_key=None,
    ):
        """Hands the work to the model's own thread and yields what comes back.

        Nothing here touches an MLX array; see `_GenerationWorker` for why that
        is the whole point.
        """
        job = _Job(prompt, images, max_tokens, temperature, prompt_cache_key)
        self.worker.submit(job)
        try:
            yield from job.deltas()
        finally:
            self._cached_input_tokens = job.cached_input_tokens
            # Reached on a dropped connection too, which is how the supervisor
            # cancels: the worker stops rather than generating into nothing.
            job.cancel()

    def _prompt_from(self, request: dict, image_count: int) -> str:
        messages = [
            {"role": str(message.get("role", "user")), "content": str(message.get("text", ""))}
            for message in request.get("messages", [])
        ]
        return self.runtime.render(messages, image_count)

    def _prompt_cache_key_from(self, request: dict):
        # `promptCacheKey` is supplied by the production Rust supervisor from a
        # deterministic conversation-prefix digest. The cache still works without
        # one for protocol compatibility, and token equality remains authoritative.
        value = request.get("promptCacheKey")
        if value is None:
            return None
        if not isinstance(value, str) or not value:
            raise ValueError("promptCacheKey must be a non-empty string")
        if len(value.encode("utf-8")) > MAX_PROMPT_CACHE_KEY_BYTES:
            raise ValueError("promptCacheKey is too long")
        return value

    def _images_from(self, request: dict) -> list:
        """The inline images of every turn, in order.

        Two refusals rather than a silent best effort. A text-only model is told
        it cannot see, because dropping the images and answering anyway is the
        failure this service used to have: the reply reads as if the picture was
        considered. And only `data:` URIs are accepted — `mlx_vlm.load_image`
        will happily fetch an `http(s)` URL, which would turn a local model
        server into a fetcher for whatever a request names.
        """
        images = []
        for message in request.get("messages", []):
            for image in message.get("images", []) or []:
                image = str(image)
                if not image.startswith("data:image/"):
                    raise ValueError("images must be inline `data:image/...` URIs")
                images.append(image)
        if images and not self.runtime.supports_images:
            raise ValueError("this model has no vision tower and cannot read images")
        return images


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

    worker = _GenerationWorker(arguments.model)
    worker.wait_until_loaded()
    _Handler.worker = worker
    _Handler.runtime = worker.runtime

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

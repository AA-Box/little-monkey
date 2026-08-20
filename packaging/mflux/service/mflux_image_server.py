"""Small, supervised HTTP service for native image generation.

The desktop process owns this service. It speaks the same job protocol as the
other Studio engines, while keeping the Python model object alive between jobs.
"""

from __future__ import annotations

import argparse
import base64
from contextlib import contextmanager
import io
import json
import random
import sys
import tempfile
import threading
import uuid
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any, Callable


class GenerationCancelled(Exception):
    """Raised by the step callback at the next safe sampling boundary."""


@dataclass
class Job:
    job_id: str
    request: dict[str, Any]
    status: str = "queued"
    step: int = 0
    total_steps: int = 0
    queue_position: int = 0
    images: list[dict[str, str]] = field(default_factory=list)
    error: str | None = None
    cancel_event: threading.Event = field(default_factory=threading.Event)


class ProgressCallback:
    def __init__(self, job: Job, offset: int, total: int, lock: threading.RLock):
        self.job = job
        self.offset = offset
        self.total = total
        self._lock = lock
        self.count = 0

    def call_in_loop(self, _t: int, **_: Any) -> None:
        if self.job.cancel_event.is_set():
            raise GenerationCancelled("Generation cancelled")
        self.count += 1
        step = self.offset + self.count
        with self._lock:
            self.job.step = step
            self.job.total_steps = self.total
        print(f"mflux step {step}/{self.total}", file=sys.stderr, flush=True)


class CancellableVae:
    """Poll cancellation around each VAE operation and decode tile."""

    def __init__(self, vae: Any, check_cancel: Callable[[], None]):
        self._vae = vae
        self._check_cancel = check_cancel

    def encode(self, image: Any) -> Any:
        self._check_cancel()
        encoded = self._vae.encode(image)
        self._check_cancel()
        return encoded

    def decode(self, latent: Any) -> Any:
        self._check_cancel()
        decoded = self._vae.decode(latent)
        self._check_cancel()
        return decoded

    def __getattr__(self, name: str) -> Any:
        return getattr(self._vae, name)


class MfluxRunner:
    """Loads one model key and reuses it for subsequent requests."""

    def __init__(
        self,
        model_path: str,
        base_model: str,
        quantize: int | None,
        model_factory: Callable[..., Any] | None = None,
    ):
        self.model_path = model_path
        self.base_model = base_model or "dev"
        self.quantize = quantize
        self.model_factory = model_factory
        self.model: Any | None = None
        self.model_key: tuple[str, str, int | None] | None = None
        self.lock = threading.RLock()

    def _default_factory(self, *, model_path: str, base_model: str, quantize: int | None) -> Any:
        from mflux.models.common.config import ModelConfig
        from mflux.models.flux.variants.txt2img.flux import Flux1

        config = ModelConfig.from_name(model_name=base_model, base_model=base_model)
        return Flux1(model_config=config, quantize=quantize, model_path=model_path or None)

    def _load(self) -> Any:
        key = (self.model_path, self.base_model, self.quantize)
        with self.lock:
            if self.model is None or self.model_key != key:
                factory = self.model_factory or self._default_factory
                self.model = factory(
                    model_path=self.model_path,
                    base_model=self.base_model,
                    quantize=self.quantize,
                )
                self.model_key = key
            return self.model

    def preload(self) -> None:
        """Load before the HTTP service accepts jobs, so cancellation never
        races an uncancellable model-construction phase.
        """

        self._load()

    @contextmanager
    def _cancellable_vae(self, model: Any, job: Job):
        vae = getattr(model, "vae", None)
        if vae is None:
            yield
            return

        def check_cancel() -> None:
            if job.cancel_event.is_set():
                raise GenerationCancelled("Generation cancelled")

        original_vae = vae
        original_tiling = getattr(model, "tiling_config", None)
        try:
            # The pinned runtime's VAE helper decodes in 512px tiles when a
            # tiling config is present. Wrapping the VAE makes each tile a
            # cancellation boundary, including the post-sampling decode.
            from mflux.models.common.vae.tiling_config import TilingConfig

            model.vae = CancellableVae(vae, check_cancel)
            model.tiling_config = TilingConfig(
                vae_decode_tiles_per_dim=2,
                vae_decode_overlap=8,
                vae_encode_tiled=False,
            )
            yield
        finally:
            model.vae = original_vae
            model.tiling_config = original_tiling

    def generate(self, job: Job, offset: int, total: int, batch_index: int = 0) -> bytes:
        model = self._load()
        steps = int(job.request.get("sample_params", {}).get("sample_steps", 20))
        callback = ProgressCallback(job, offset, total, self.lock)
        # The callback registry is part of the model and is intentionally
        # restored after each request so callbacks never accumulate.
        registry = getattr(model, "callbacks", None)
        original_callbacks = list(getattr(registry, "in_loop", [])) if registry else []
        if registry is not None:
            registry.register(callback)
        try:
            if job.cancel_event.is_set():
                raise GenerationCancelled("Generation cancelled")
            seed = int(job.request.get("seed", -1))
            if seed < 0:
                seed = random.randint(0, 2_147_483_647)
            else:
                seed += batch_index
            sample = job.request.get("sample_params", {})
            kwargs: dict[str, Any] = {
                "seed": seed,
                "prompt": str(job.request.get("prompt", "")),
                "num_inference_steps": steps,
                "width": int(job.request.get("width", 1024)),
                "height": int(job.request.get("height", 1024)),
                "guidance": float(sample.get("guidance", {}).get("txt_cfg", 4.0)),
            }
            init_image = job.request.get("init_image")
            init_strength = job.request.get("strength")
            with self._cancellable_vae(model, job):
                if init_image:
                    with tempfile.NamedTemporaryFile(suffix=".png") as input_file:
                        input_file.write(base64.b64decode(init_image))
                        input_file.flush()
                        kwargs["image_path"] = input_file.name
                        kwargs["image_strength"] = float(init_strength if init_strength is not None else 0.5)
                        image = model.generate_image(**kwargs)
                else:
                    image = model.generate_image(**kwargs)
            if job.cancel_event.is_set():
                raise GenerationCancelled("Generation cancelled")
            output = io.BytesIO()
            getattr(image, "image", image).save(output, format="PNG")
            return output.getvalue()
        finally:
            if registry is not None:
                registry.in_loop[:] = original_callbacks


class ServiceState:
    def __init__(self, runner: MfluxRunner):
        self.runner = runner
        self.jobs: dict[str, Job] = {}
        self.lock = threading.RLock()
        self.executor = ThreadPoolExecutor(max_workers=1, thread_name_prefix="mflux-job")

    def submit(self, request: dict[str, Any]) -> Job:
        job = Job(job_id=uuid.uuid4().hex, request=request)
        with self.lock:
            self.jobs[job.job_id] = job
            queued = [item for item in self.jobs.values() if item.status == "queued"]
            job.queue_position = len(queued)
        self.executor.submit(self._run, job)
        return job

    def _run(self, job: Job) -> None:
        with self.lock:
            if job.cancel_event.is_set():
                job.status = "cancelled"
                return
            job.status = "generating"
            job.queue_position = 0
            steps = int(job.request.get("sample_params", {}).get("sample_steps", 20))
            count = max(1, min(8, int(job.request.get("batch_count", 1))))
            job.total_steps = steps * count
        try:
            count = max(1, min(8, int(job.request.get("batch_count", 1))))
            steps = int(job.request.get("sample_params", {}).get("sample_steps", 20))
            for index in range(count):
                payload = self.runner.generate(job, index * steps, count * steps, batch_index=index)
                encoded = base64.b64encode(payload).decode("ascii")
                with self.lock:
                    job.images.append({"b64_json": encoded, "mime_type": "image/png"})
            with self.lock:
                job.status = "cancelled" if job.cancel_event.is_set() else "completed"
        except GenerationCancelled:
            with self.lock:
                job.status = "cancelled"
        except Exception as error:  # the supervisor owns the process boundary
            with self.lock:
                job.status = "failed"
                job.error = str(error)

    def cancel(self, job_id: str) -> bool:
        with self.lock:
            job = self.jobs.get(job_id)
            if job is None or job.status in {"completed", "failed", "cancelled"}:
                return False
            job.cancel_event.set()
            if job.status == "queued":
                job.status = "cancelled"
            return True

    def snapshot(self, job: Job) -> dict[str, Any]:
        with self.lock:
            result: dict[str, Any] = {
                "id": job.job_id,
                "status": job.status,
                "queue_position": job.queue_position,
                "progress": {"step": job.step, "total": job.total_steps},
            }
            if job.status == "completed":
                result["result"] = {
                    "images": job.images,
                    "output_format": "png",
                }
            if job.status == "failed":
                result["error"] = {"message": job.error or "Generation failed"}
            return result


def capabilities(runner: MfluxRunner) -> dict[str, Any]:
    return {
        "model": {"path": runner.model_path, "base_model": runner.base_model, "quantization": runner.quantize},
        "samplers": ["linear"],
        "schedulers": [],
        "upscalers": [],
        "features": {
            "init_image": True,
            "mask_image": False,
            "control_image": False,
            "ip_adapter_image": False,
            "ref_images": False,
            "lora": False,
            "hires": False,
            "cancel_queued": True,
            "cancel_generating": True,
        },
    }


class Handler(BaseHTTPRequestHandler):
    state: ServiceState

    def _send(self, status: HTTPStatus, body: dict[str, Any]) -> None:
        encoded = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def _json(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length", "0"))
        if length > 16 * 1024 * 1024:
            raise ValueError("Request is too large")
        return json.loads(self.rfile.read(length) or b"{}")

    def do_GET(self) -> None:  # noqa: N802
        if self.path == "/sdcpp/v1/capabilities":
            self._send(HTTPStatus.OK, capabilities(self.state.runner))
            return
        prefix = "/sdcpp/v1/jobs/"
        if self.path.startswith(prefix):
            job_id = self.path[len(prefix) :]
            with self.state.lock:
                job = self.state.jobs.get(job_id)
            if job is None:
                self._send(HTTPStatus.NOT_FOUND, {"error": "job not found"})
            else:
                self._send(HTTPStatus.OK, self.state.snapshot(job))
            return
        self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})

    def do_POST(self) -> None:  # noqa: N802
        if self.path.endswith("/cancel"):
            job_id = self.path.split("/")[-2]
            cancelled = self.state.cancel(job_id)
            self._send(
                HTTPStatus.OK if cancelled else HTTPStatus.NOT_FOUND,
                {"cancelled": cancelled},
            )
            return
        if self.path == "/sdcpp/v1/img_gen":
            try:
                request = self._json()
                if not str(request.get("prompt", "")).strip():
                    raise ValueError("A prompt is required")
                if str(request.get("negative_prompt") or "").strip():
                    raise ValueError("MFLUX does not support negative prompts")
                job = self.state.submit(request)
                self._send(HTTPStatus.ACCEPTED, {"id": job.job_id})
            except (ValueError, json.JSONDecodeError) as error:
                self._send(HTTPStatus.BAD_REQUEST, {"error": str(error)})
            return
        self._send(HTTPStatus.NOT_FOUND, {"error": "not found"})

    def log_message(self, format: str, *args: Any) -> None:
        print(format % args, file=sys.stderr, flush=True)


def create_server(host: str, port: int, runner: MfluxRunner) -> ThreadingHTTPServer:
    state = ServiceState(runner)

    class BoundHandler(Handler):
        pass

    BoundHandler.state = state
    return ThreadingHTTPServer((host, port), BoundHandler)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen-ip", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument("--model-path", required=True)
    parser.add_argument("--base-model", default="dev")
    parser.add_argument("--quantize", type=int, default=None)
    args = parser.parse_args()
    runner = MfluxRunner(args.model_path, args.base_model, args.quantize)
    # Model construction is not safely interruptible from the HTTP worker.
    # Complete it before accepting jobs so cancellation always applies to an
    # active generation rather than racing a one-time load.
    runner.preload()
    server = create_server(args.listen_ip, args.listen_port, runner)
    print(
        f"MFLUX image service listening on {args.listen_ip}:{args.listen_port}",
        file=sys.stderr,
        flush=True,
    )
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()

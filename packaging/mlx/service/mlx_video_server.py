"""The MLX video-generation service Studio supervises.

This is the second service in the signed MLX package. It ships beside
`mlx_server.py` — same tree, same manifest, same digests — but it is launched by
Studio's generation engine rather than by the M3 runtime adapter, and it speaks
a completely different protocol: the one `sd-server` speaks.

That choice is the whole design. `src-tauri/src/generation.rs` already knows how
to launch an engine, wait for `/sdcpp/v1/capabilities`, submit a job, poll it,
scrape a progress bar off stderr and cancel. None of that is worth writing twice,
so this service impersonates that wire contract instead of inventing one:

  * launched as `<python> <this file> --listen-ip 127.0.0.1 --listen-port N
    --lora-model-dir DIR --diffusion-model PATH --t5xxl PATH --vae PATH`,
    the argv `launch_args` builds, with unknown slot flags tolerated
  * `GET /sdcpp/v1/capabilities` answers `{"model": {"path": ...}}` echoing the
    denoiser path it was handed — byte for byte, because `ensure_ready` compares
    it with `==` and spins forever on a mismatch rather than timing out
  * `POST /sdcpp/v1/vid_gen` returns `{"id": ...}`; `GET /sdcpp/v1/jobs/<id>`
    reports `queued`/`generating`/`completed`/`failed`/`cancelled`; a completed
    job carries `result.b64_json`
  * a sampling bar goes to stderr in stable-diffusion.cpp's own shape, because
    the job API has no step counter and that bar is the only thing behind the
    progress percentage a user sees

Generation itself is delegated to mlx-video's CLI in a child process rather than
imported. mlx-video has no published API surface — `generate.py` is a script that
loads the text encoder, frees it, loads the transformer and decodes in one pass —
so the CLI is the only interface it actually keeps stable. A child also gives
cancellation for free: kill it. The cost is that weights reload per job, which is
what mlx-video's own CLI does anyway.
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

# Same bound as the text service: a malformed or hostile local caller must not
# be able to make this process allocate without limit. Init images arrive
# base64-encoded inside the body, so this is larger than that service's cap.
MAX_REQUEST_BYTES = 64 * 1024 * 1024

# The four names mlx-video's loader opens inside `--model-dir`
# (mlx_video/models/wan_2/generate.py). The app stores weights per slot, wherever
# the user put them, so the launch assembles a directory of these names as links.
MODEL_DIR_FILES = {
    "config": "config.json",
    "diffusion": "model.safetensors",
    "t5": "t5_encoder.safetensors",
    "vae": "vae.safetensors",
}

# What the engine tells Studio it can do. Absent flags disable their inputs in
# the UI (`engineSupports` in src/lib/studioClient.ts treats a running engine's
# missing flag as false), which is the honest answer here: mlx-video's Wan path
# takes one init image and nothing else — no mask, no ControlNet, no IP-Adapter,
# no hires pass, and LoRA only as files this service does not stage.
FEATURES = {
    "init_image": True,
    "mask_image": False,
    "control_image": False,
    "ip_adapter_image": False,
    "ref_images": False,
    "lora": False,
    "vae_tiling": True,
    "hires": False,
    "cache": False,
    "cancel_queued": True,
    "cancel_generating": True,
}

# mlx-video exposes one solver knob, `--scheduler`. Studio sends a sampler and a
# scheduler separately, so both lists carry the same three names and whichever
# arrives is passed through.
SCHEDULERS = ["unipc", "euler", "dpm++"]

# tqdm writes `Diffusion:  25%|##  | 1/4 [00:00<00:02,  1.43it/s]`; the engine
# parser wants `| 1/4 - 1.43it/s`. Only the fraction and the rate are reused.
CHILD_PROGRESS = re.compile(r"\|\s*(\d+)/(\d+)\s*\[[^\]]*?,\s*([0-9.]+)(it/s|s/it)")


class GenerationError(RuntimeError):
    """A failure that belongs in the job's error message, not in a traceback."""


def link_into(directory: Path, name: str, target: Path) -> None:
    """Publishes `target` inside `directory` under the name mlx-video expects.

    Symlinked rather than copied: these are 5–11 GB files that the user already
    has on disk, and a launch must not duplicate 19 GB to rename three files.
    Falls back to a copy only for the config, which is a kilobyte.
    """
    link = directory / name
    if link.exists() or link.is_symlink():
        link.unlink()
    try:
        link.symlink_to(target)
    except OSError:
        shutil.copyfile(target, link)


class ModelDirectory:
    """The `--model-dir` mlx-video wants, assembled from the app's slot paths.

    Studio stores one file per component slot and passes them as separate flags.
    mlx-video takes a single directory holding four fixed names. This is the
    adapter between the two, and it is also where the checkpoint's `config.json`
    is found: nothing in Studio's slot vocabulary names a config file, so it is
    read from beside the diffusion model — where every published MLX conversion
    puts it.
    """

    def __init__(self, diffusion: Path, t5: Path, vae: Path, config: Path | None) -> None:
        self.diffusion = diffusion
        config = config or diffusion.parent / MODEL_DIR_FILES["config"]
        if not config.is_file():
            raise GenerationError(
                "This model has no %s. An MLX video checkpoint carries one beside its "
                "weights; put it next to %s, or pass --mlx-video-config <path> in the "
                "model's engine arguments." % (MODEL_DIR_FILES["config"], diffusion.name)
            )
        self.config = config
        self.t5 = t5
        self.vae = vae
        self._root: Path | None = None

    def path(self) -> Path:
        """Materializes the directory once, on first use."""
        if self._root is None:
            root = Path(tempfile.mkdtemp(prefix="mlx-video-model-"))
            link_into(root, MODEL_DIR_FILES["diffusion"], self.diffusion)
            link_into(root, MODEL_DIR_FILES["t5"], self.t5)
            link_into(root, MODEL_DIR_FILES["vae"], self.vae)
            link_into(root, MODEL_DIR_FILES["config"], self.config)
            self._root = root
        return self._root

    def frame_rate(self) -> int:
        """The fps the decoded video actually has.

        mlx-video has no `--fps`: the rate is a property of the checkpoint
        (`sample_fps` in its config), so the request's fps cannot change it and
        reporting the request's value back would mislabel the file.
        """
        try:
            with self.config.open("rb") as handle:
                return int(json.load(handle).get("sample_fps") or 24)
        except (OSError, ValueError):
            return 24


class Job:
    """One generation, and everything the poller is allowed to see of it."""

    def __init__(self, identifier: str, request: dict) -> None:
        self.id = identifier
        self.request = request
        self.status = "queued"
        self.queue_position = 0
        self.error: str | None = None
        self.result: dict | None = None
        self.cancelled = threading.Event()
        self.process: subprocess.Popen | None = None

    def snapshot(self) -> dict:
        body: dict = {"id": self.id, "status": self.status}
        if self.status == "queued":
            body["queue_position"] = self.queue_position
        if self.status == "failed":
            body["error"] = {"message": self.error or "Generation failed"}
        if self.status == "completed" and self.result is not None:
            body["result"] = self.result
        return body


class Runner:
    """Runs one job at a time, because one GPU decodes one video at a time.

    Queued jobs report their position, which is exactly what the engine's
    `queued` status carries and what keeps the app's stall watchdog satisfied
    while a long job is still ahead of this one.
    """

    def __init__(self, model: ModelDirectory) -> None:
        self.model = model
        self.jobs: dict[str, Job] = {}
        self.pending: list[Job] = []
        self.lock = threading.Lock()
        self.worker: threading.Thread | None = None

    def submit(self, request: dict) -> Job:
        job = Job(uuid.uuid4().hex, request)
        with self.lock:
            self.jobs[job.id] = job
            self.pending.append(job)
            job.queue_position = len(self.pending) - 1
            if self.worker is None or not self.worker.is_alive():
                self.worker = threading.Thread(target=self._drain, name="mlx-video", daemon=True)
                self.worker.start()
        return job

    def get(self, identifier: str) -> Job | None:
        with self.lock:
            return self.jobs.get(identifier)

    def cancel(self, identifier: str) -> bool:
        with self.lock:
            job = self.jobs.get(identifier)
            if job is None or job.status in {"completed", "failed", "cancelled"}:
                return False
            job.cancelled.set()
            if job in self.pending:
                self.pending.remove(job)
                job.status = "cancelled"
                return True
            process = job.process
        if process is not None:
            process.kill()
        return True

    def _drain(self) -> None:
        while True:
            with self.lock:
                if not self.pending:
                    return
                job = self.pending.pop(0)
                for position, queued in enumerate(self.pending):
                    queued.queue_position = position
                job.status = "generating"
            try:
                job.result = self._generate(job)
                job.status = "cancelled" if job.cancelled.is_set() else "completed"
            except Exception as error:  # noqa: BLE001 - every failure must reach the user
                if job.cancelled.is_set():
                    job.status = "cancelled"
                else:
                    job.status = "failed"
                    job.error = str(error)

    def _generate(self, job: Job) -> dict:
        request = job.request
        with tempfile.TemporaryDirectory(prefix="mlx-video-job-") as workspace:
            output = Path(workspace) / "out.mp4"
            argv = self._argv(request, Path(workspace), output)
            note("running %s" % " ".join(argv[3:]))
            process = subprocess.Popen(
                argv,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                errors="replace",
                # A child of this service must not inherit a terminal's width or
                # colours; tqdm's plain form is what the progress regex expects.
                env={**os.environ, "TERM": "dumb", "COLUMNS": "80"},
            )
            job.process = process
            tail: list[str] = []
            assert process.stdout is not None
            for line in process.stdout:
                line = line.rstrip()
                if not line:
                    continue
                if not report_progress(line):
                    tail.append(line)
                    del tail[:-40]
            code = process.wait()
            job.process = None
            if job.cancelled.is_set():
                raise GenerationError("cancelled")
            if code != 0:
                raise GenerationError(
                    "mlx-video exited with status %d\n%s" % (code, "\n".join(tail[-12:]))
                )
            if not output.is_file():
                raise GenerationError("mlx-video wrote no video\n%s" % "\n".join(tail[-12:]))
            encoded = base64.b64encode(output.read_bytes()).decode("ascii")
        return {
            "mime_type": "video/mp4",
            "output_format": "mp4",
            "fps": self.model.frame_rate(),
            "frame_count": int(request.get("video_frames") or 1),
            "b64_json": encoded,
        }

    def _argv(self, request: dict, workspace: Path, output: Path) -> list[str]:
        sample = request.get("sample_params") or {}
        guidance = sample.get("guidance") or {}
        argv = [
            sys.executable,
            "-m",
            "mlx_video.models.wan_2.generate",
            "--model-dir",
            str(self.model.path()),
            "--prompt",
            str(request.get("prompt") or ""),
            "--output-path",
            str(output),
        ]
        negative = str(request.get("negative_prompt") or "").strip()
        # The flag and its absence are different requests: with neither, the
        # checkpoint's own (Chinese) negative prompt from config.json applies.
        if negative:
            argv += ["--negative-prompt", negative]
        for flag, value in (
            ("--width", request.get("width")),
            ("--height", request.get("height")),
            ("--num-frames", request.get("video_frames")),
            ("--steps", sample.get("sample_steps")),
            ("--seed", request.get("seed")),
        ):
            if value is not None:
                argv += [flag, str(int(value))]
        if guidance.get("txt_cfg") is not None:
            argv += ["--guide-scale", str(float(guidance["txt_cfg"]))]
        if sample.get("flow_shift") is not None:
            argv += ["--shift", str(float(sample["flow_shift"]))]
        scheduler = str(sample.get("scheduler") or sample.get("sample_method") or "").strip()
        if scheduler in SCHEDULERS:
            argv += ["--scheduler", scheduler]
        initial = request.get("init_image")
        if isinstance(initial, str) and initial:
            image = workspace / "init.png"
            image.write_bytes(base64.b64decode(initial, validate=True))
            argv += ["--image", str(image)]
        return argv


def report_progress(line: str) -> bool:
    """Re-emits a tqdm bar in the shape the app's stderr parser accepts.

    `parse_sampling_progress` (src-tauri/src/generation.rs) takes the text after
    the last `|`, requires an `it/s` or `s/it` suffix, and reads `done/total`
    before the dash. Anything else it sees is treated as tensor loading and
    ignored, so the bar has to be rebuilt rather than forwarded.
    """
    found = CHILD_PROGRESS.search(line)
    if found is None:
        return False
    done, total, rate, unit = found.groups()
    filled = round(20 * int(done) / max(int(total), 1))
    note("  |%s%s| %s/%s - %s%s" % ("=" * filled, " " * (20 - filled), done, total, rate, unit))
    return True


def note(message: str) -> None:
    """Writes one line to stderr, which the supervisor tails.

    Every line here doubles as a liveness heartbeat: the app fails a launch that
    goes 300 s without new output, and cancels a job that goes an hour without
    any.
    """
    sys.stderr.write("mlx-video-service %s\n" % message)
    sys.stderr.flush()


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    runner: Runner
    denoiser_path: str
    #: Body bytes this request has already read. One handler instance serves
    #: every request on a connection, so it is reset per request, not per
    #: instance.
    consumed = 0

    def log_message(self, format: str, *args: object) -> None:
        note(format % args)

    def _drain(self) -> None:
        """Consumes whatever of the request body this request did not read.

        Keep-alive is on, so bytes left unread are parsed as the start of the
        next request on the same connection — a refused POST would otherwise
        turn the following one into a `Bad request version` for no reason the
        caller could see. Counted rather than assumed: reading a body that was
        already consumed blocks until the client gives up.
        """
        try:
            length = int(self.headers.get("Content-Length", "0")) - self.consumed
        except ValueError:
            return
        while length > 0:
            chunk = self.rfile.read(min(length, 64 * 1024))
            if not chunk:
                return
            length -= len(chunk)

    def _send(self, status: int, body: dict) -> None:
        self._drain()
        payload = json.dumps(body).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's spelling
        self.consumed = 0
        if self.path == "/sdcpp/v1/capabilities":
            self._send(
                200,
                {
                    "current_mode": "vid_gen",
                    # Load-bearing: readiness is this string compared with `==`
                    # against the path the app passed. A mismatch does not time
                    # out, it hangs.
                    "model": {"path": self.denoiser_path},
                    "samplers": SCHEDULERS,
                    "schedulers": SCHEDULERS,
                    "upscalers": [],
                    "loras": [],
                    "features": FEATURES,
                },
            )
            return
        if self.path.startswith("/sdcpp/v1/jobs/"):
            job = self.runner.get(self.path.rsplit("/", 1)[-1])
            if job is None:
                self._send(404, {"error": {"message": "unknown job"}})
                return
            self._send(200, job.snapshot())
            return
        self._send(404, {"error": {"message": "unknown endpoint"}})

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's spelling
        self.consumed = 0
        if self.path.startswith("/sdcpp/v1/jobs/") and self.path.endswith("/cancel"):
            identifier = self.path[len("/sdcpp/v1/jobs/") : -len("/cancel")]
            if not self.runner.cancel(identifier):
                self._send(404, {"error": {"message": "unknown job"}})
                return
            self._send(200, {"id": identifier, "status": "cancelled"})
            return
        if self.path != "/sdcpp/v1/vid_gen":
            # Explicitly including /sdcpp/v1/img_gen: this engine makes video,
            # and answering an image request with a video would be worse than
            # refusing it.
            self._send(404, {"error": {"message": "unknown endpoint"}})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self._send(400, {"error": {"message": "bad content length"}})
            return
        if length <= 0 or length > MAX_REQUEST_BYTES:
            self._send(413, {"error": {"message": "request too large"}})
            return
        try:
            raw = self.rfile.read(length)
            self.consumed = len(raw)
            request = json.loads(raw)
        except (OSError, ValueError):
            self._send(400, {"error": {"message": "body is not JSON"}})
            return
        job = self.runner.submit(request)
        self._send(200, {"id": job.id})

    def do_PUT(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's spelling
        self.consumed = 0
        self._send(404, {"error": {"message": "unknown endpoint"}})


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="MLX video generation service")
    parser.add_argument("--listen-ip", default="127.0.0.1")
    parser.add_argument("--listen-port", type=int, required=True)
    parser.add_argument("--lora-model-dir")
    parser.add_argument("--diffusion-model")
    parser.add_argument("--model")
    parser.add_argument("--t5xxl")
    parser.add_argument("--vae")
    parser.add_argument("--mlx-video-config")
    # Studio emits one flag per component slot and appends the model's own extra
    # arguments verbatim. Unknown ones are ignored rather than fatal, so a model
    # carrying a slot this engine has no use for still launches.
    arguments, ignored = parser.parse_known_args(argv)
    if ignored:
        note("ignoring %s" % " ".join(ignored))

    if arguments.listen_ip != "127.0.0.1":
        # The supervisor always passes loopback. Refusing anything else means a
        # tampered argument vector cannot turn this into a network service.
        parser.error("--listen-ip must be 127.0.0.1")

    denoiser = arguments.diffusion_model or arguments.model
    missing = [
        flag
        for flag, value in (
            ("--diffusion-model", denoiser),
            ("--t5xxl", arguments.t5xxl),
            ("--vae", arguments.vae),
        )
        if not value
    ]
    if missing:
        parser.error("MLX video needs %s" % ", ".join(missing))

    try:
        model = ModelDirectory(
            Path(denoiser),
            Path(arguments.t5xxl),
            Path(arguments.vae),
            Path(arguments.mlx_video_config) if arguments.mlx_video_config else None,
        )
        # Materialized before the port opens so a missing or unreadable weight
        # file is a clean startup failure, not a job that fails a minute later.
        model.path()
    except GenerationError as error:
        note(str(error))
        return 1

    _Handler.runner = Runner(model)
    # Echoed verbatim, not normalized: `ensure_ready` compares this with the
    # string it passed, and a resolved or trailing-slash-trimmed variant of the
    # same path would never match.
    _Handler.denoiser_path = denoiser

    server = ThreadingHTTPServer((arguments.listen_ip, arguments.listen_port), _Handler)
    note("listening on %s:%d" % (arguments.listen_ip, arguments.listen_port))
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

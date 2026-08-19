"""Checks mlx_video_server against the protocol Studio's engine enforces.

Run: `python3 packaging/mlx/service/test_mlx_video_server.py`

mlx-video is stubbed with a script that writes a file and prints a tqdm bar.
What is under test is the impersonation — the capabilities body readiness
compares byte for byte, the job lifecycle `decode_job_status` accepts, the
progress shape `parse_sampling_progress` accepts, and the argv the CLI is
handed — none of which needs 19 GB of weights, and every one of which is a
silent hang or an unexplained failure when it drifts.
"""

import base64
import json
import sys
import threading
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import mlx_video_server  # noqa: E402

# stable-diffusion.cpp's own bar, which is what the app's parser was written
# against: `rsplit('|')` then a `done/total - rate` tail.
ENGINE_BAR = "  |==========          | 2/4 - 1.46it/s"


def fake_generator(directory: Path, *, fail: bool = False) -> Path:
    """A stand-in for `python -m mlx_video...`, invoked the same way."""
    script = directory / "fake_generate.py"
    script.write_text(
        "import sys\n"
        "argv = sys.argv[1:]\n"
        "print('Diffusion:  50%|#####     | 2/4 [00:01<00:01,  1.46it/s]')\n"
        "sys.stderr.write('')\n"
        f"if {fail!r}:\n"
        "    print('boom: unsupported dtype')\n"
        "    raise SystemExit(1)\n"
        "out = argv[argv.index('--output-path') + 1]\n"
        "open(out, 'wb').write(b'FAKEMP4')\n"
        # Beside the script, not beside the output: the job's own workspace is a
        # TemporaryDirectory the service deletes as soon as the video is read.
        f"open({str(directory / 'last.argv')!r}, 'w').write('\\n'.join(argv))\n"
    )
    return script


class Service:
    """The real handler on a loopback port, with the child call redirected."""

    def __init__(self, workspace: Path, *, fail: bool = False) -> None:
        model = workspace / "model"
        model.mkdir()
        (model / "config.json").write_text(json.dumps({"sample_fps": 16}))
        for name in ("model.safetensors", "t5_encoder.safetensors", "vae.safetensors"):
            (model / name).write_bytes(b"weights")
        self.denoiser = model / "model.safetensors"
        directory = mlx_video_server.ModelDirectory(
            self.denoiser,
            model / "t5_encoder.safetensors",
            model / "vae.safetensors",
            None,
        )
        script = fake_generator(workspace, fail=fail)
        runner = mlx_video_server.Runner(directory)
        real_argv = runner._argv

        def argv(request, job_workspace, output):
            built = real_argv(request, job_workspace, output)
            # Everything after the module name is what mlx-video would see.
            return [sys.executable, str(script)] + built[3:]

        runner._argv = argv

        class Handler(mlx_video_server._Handler):
            pass

        Handler.runner = runner
        Handler.denoiser_path = str(self.denoiser)
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def request(self, method: str, path: str, body: dict | None = None) -> tuple[int, dict]:
        connection = HTTPConnection("127.0.0.1", self.server.server_address[1])
        connection.request(
            method,
            path,
            body=json.dumps(body) if body is not None else None,
            headers={"Content-Type": "application/json"} if body is not None else {},
        )
        response = connection.getresponse()
        payload = response.read()
        connection.close()
        return response.status, (json.loads(payload) if payload else {})

    def run_to_completion(self, body: dict) -> dict:
        status, submitted = self.request("POST", "/sdcpp/v1/vid_gen", body)
        assert status == 200, status
        assert submitted.get("id"), submitted
        for _ in range(200):
            status, state = self.request("GET", "/sdcpp/v1/jobs/%s" % submitted["id"])
            assert status == 200, status
            if state["status"] in {"completed", "failed", "cancelled"}:
                return state
        raise AssertionError("job never reached a terminal state")


def request_body(**overrides) -> dict:
    body = {
        "prompt": "a boat",
        "negative_prompt": "",
        "width": 448,
        "height": 256,
        "seed": 7,
        "clip_skip": -1,
        "sample_params": {
            "sample_method": "unipc",
            "sample_steps": 4,
            "guidance": {"txt_cfg": 5.0},
            "flow_shift": 3.0,
        },
        "video_frames": 5,
        "fps": 24,
        "output_format": "webm",
    }
    body.update(overrides)
    return body


def check_capabilities_echo_the_denoiser(service: Service) -> None:
    """Readiness is `body.model.path == the path we were launched with`, and a
    mismatch does not time out — `ensure_ready` spins on it forever."""
    status, body = service.request("GET", "/sdcpp/v1/capabilities")
    assert status == 200, status
    assert body["model"]["path"] == str(service.denoiser), body["model"]
    assert body["current_mode"] == "vid_gen", body
    # Read as bare strings and as name-carrying objects respectively.
    assert all(isinstance(entry, str) for entry in body["samplers"]), body["samplers"]
    assert all(isinstance(entry, str) for entry in body["schedulers"]), body["schedulers"]
    assert body["features"]["init_image"] is True, body["features"]
    # A running engine's missing flag reads as false in the UI, so the ones this
    # engine cannot honour must be present and false rather than absent.
    assert body["features"]["hires"] is False, body["features"]


def check_a_job_completes_with_a_decodable_result(service: Service, workspace: Path) -> None:
    state = service.run_to_completion(request_body())
    assert state["status"] == "completed", state
    result = state["result"]
    assert result["mime_type"] == "video/mp4", result
    assert result["frame_count"] == 5, result
    # From the checkpoint's config, not from the request: mlx-video has no fps
    # flag, so echoing the request's 24 would mislabel a 16 fps file.
    assert result["fps"] == 16, result
    assert base64.b64decode(result["b64_json"], validate=True) == b"FAKEMP4", result


def check_the_cli_receives_every_mapped_field(service: Service, workspace: Path) -> None:
    service.run_to_completion(request_body())
    argv = (workspace / "last.argv").read_text().splitlines()
    pairs = dict(zip(argv, argv[1:]))
    assert pairs["--width"] == "448", argv
    assert pairs["--height"] == "256", argv
    assert pairs["--num-frames"] == "5", argv
    assert pairs["--steps"] == "4", argv
    assert pairs["--seed"] == "7", argv
    assert pairs["--guide-scale"] == "5.0", argv
    assert pairs["--shift"] == "3.0", argv
    assert pairs["--scheduler"] == "unipc", argv
    assert pairs["--prompt"] == "a boat", argv
    # An empty negative prompt is not the same request as a blank one: with the
    # flag absent the checkpoint's own default applies.
    assert "--negative-prompt" not in argv, argv
    assert "--image" not in argv, argv


def check_an_init_image_is_written_and_passed(service: Service, workspace: Path) -> None:
    encoded = base64.b64encode(b"\x89PNG\r\n\x1a\n").decode("ascii")
    service.run_to_completion(request_body(init_image=encoded))
    argv = (workspace / "last.argv").read_text().splitlines()
    assert "--image" in argv, argv


def check_a_failing_child_fails_the_job(workspace: Path) -> None:
    service = Service(workspace, fail=True)
    state = service.run_to_completion(request_body())
    assert state["status"] == "failed", state
    # The engine's own words, which is the only place a real cause appears.
    assert "boom: unsupported dtype" in state["error"]["message"], state
    service.server.shutdown()


def check_unknown_endpoints_and_jobs_are_404(service: Service) -> None:
    assert service.request("GET", "/sdcpp/v1/jobs/nope")[0] == 404
    assert service.request("POST", "/sdcpp/v1/img_gen", request_body())[0] == 404
    assert service.request("GET", "/v1/models")[0] == 404
    # A refused POST must leave the connection usable: keep-alive is on, and an
    # undrained body is read as the next request line.
    connection = HTTPConnection("127.0.0.1", service.server.server_address[1])
    for expected in (404, 200):
        if expected == 404:
            connection.request("POST", "/sdcpp/v1/img_gen", body=json.dumps(request_body()))
        else:
            connection.request("GET", "/sdcpp/v1/capabilities")
        response = connection.getresponse()
        response.read()
        assert response.status == expected, (expected, response.status)
    connection.close()


def check_progress_is_reshaped_for_the_engine_parser() -> None:
    """The child's tqdm bar is unreadable to the app; the reshaped one is not."""
    assert mlx_video_server.CHILD_PROGRESS.search(
        "Diffusion:  50%|#####     | 2/4 [00:01<00:01,  1.46it/s]"
    )
    rebuilt = []
    original = mlx_video_server.note
    mlx_video_server.note = rebuilt.append
    try:
        assert mlx_video_server.report_progress(
            "Diffusion:  50%|#####     | 2/4 [00:01<00:01,  1.46it/s]"
        )
        assert not mlx_video_server.report_progress("Loading T5 encoder...")
        # Tensor loading reports MB/s and must not be read as sampling.
        assert not mlx_video_server.report_progress("  |####      | 212/686 - 647.34MB/s")
    finally:
        mlx_video_server.note = original
    assert rebuilt == [ENGINE_BAR], rebuilt


def check_a_missing_config_is_refused_before_the_port_opens(workspace: Path) -> None:
    """A weight set with no config.json cannot generate, and finding that out a
    minute into the first job is the outcome this avoids."""
    bare = workspace / "bare"
    bare.mkdir()
    weights = bare / "model.safetensors"
    weights.write_bytes(b"weights")
    try:
        mlx_video_server.ModelDirectory(weights, weights, weights, None)
    except mlx_video_server.GenerationError as error:
        assert "config.json" in str(error), error
    else:
        raise AssertionError("a checkpoint with no config.json was accepted")


def check_generated_bytecode_cache_is_removed() -> None:
    import tempfile

    with tempfile.TemporaryDirectory() as raw:
        cache = Path(raw) / "imageio_ffmpeg/__pycache__"
        cache.mkdir(parents=True)
        (cache / "_io.cpython-314.pyc").write_bytes(b"cache")
        mlx_video_server.remove_bytecode_caches(Path(raw))
        assert not cache.exists(), "generated bytecode cache was retained"


def main() -> int:
    import tempfile

    with tempfile.TemporaryDirectory() as raw:
        workspace = Path(raw)
        service = Service(workspace)
        check_capabilities_echo_the_denoiser(service)
        check_a_job_completes_with_a_decodable_result(service, workspace)
        check_the_cli_receives_every_mapped_field(service, workspace)
        check_an_init_image_is_written_and_passed(service, workspace)
        check_unknown_endpoints_and_jobs_are_404(service)
        service.server.shutdown()

        with tempfile.TemporaryDirectory() as failing:
            check_a_failing_child_fails_the_job(Path(failing))

        check_progress_is_reshaped_for_the_engine_parser()
        check_a_missing_config_is_refused_before_the_port_opens(workspace)
        check_generated_bytecode_cache_is_removed()
    print("mlx_video_server: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

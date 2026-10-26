#!/usr/bin/env python3
"""GPU-free subprocess and HTTP contracts for the real optimized VDN adapter.

Does not represent a GPU render. Exercises the same CLI and /v1/videos API
contracts invoked when the user's licensed installed GPUs are available.
"""
import http.server
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import unittest

WORKER = Path(__file__).with_name("studio-vdn-fast-worker.py")
MODEL = "OpenVDN/vdn-minimax-h3"


def call(*args):
    return subprocess.run([sys.executable, str(WORKER), *args],
                          capture_output=True, text=True, timeout=20)


def fake_checkout(root):
    src = root / "checkout"
    ckpt = root / "ckpts"
    for name in (
        "src/inference/encode_prompt.py", "src/inference/encode_keyframes.py",
        "src/inference/infer.py", "src/inference/infer_ulysses.py",
        "configs/inference/8nfe_tuned.yaml",
        "configs/inference/8nfe_tuned_fp8.yaml",
        "configs/inference/8nfe_tuned_fp8_ulysses_h200.yaml",
        "configs/inference/8nfe_tuned_fp8_ulysses_b200.yaml",
    ):
        file = src / name
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text("# verified CLI fixture\n")
    model = ckpt / "stage-dmd-step-250" / "model_spec.json"
    model.parent.mkdir(parents=True)
    model.write_text('{"fixture":true}')
    (ckpt / "h3-base").mkdir()
    return src, ckpt


class ApiHandler(http.server.BaseHTTPRequestHandler):
    posts = []
    def log_message(self, *_):
        pass

    def reply(self, status, payload, media="application/json"):
        payload = json.dumps(payload).encode() if isinstance(payload, dict) else payload
        self.send_response(status)
        self.send_header("Content-Type", media)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_GET(self):
        if self.path == "/v1/models":
            self.reply(200, {"data": [{"id": MODEL}]})
        elif self.path == "/v1/videos/vid_123":
            self.reply(200, {"status": "completed", "id": "vid_123"})
        elif self.path == "/v1/videos/vid_123/content":
            self.reply(200, b"\0\0\0\x18ftypisom" + bytes(200), "video/mp4")
        else:
            self.reply(404, {"error": "unrecognized"})

    def do_POST(self):
        if self.path != "/v1/videos":
            self.reply(404, {})
            return
        n = int(self.headers.get("Content-Length", "0"))
        self.__class__.posts.append(json.loads(self.rfile.read(n)))
        self.reply(200, {"id": "vid_123", "status": "queued"})


class OptimizedContract(unittest.TestCase):
    def test_binary_self_test_without_torch(self):
        x = call("--self-test")
        self.assertEqual(x.returncode, 0, x.stderr)
        self.assertEqual(json.loads(x.stdout)["model"], MODEL)

    def test_rejects_unknown_parameters_and_unsupported_gpu_count(self):
        x = call("--backend", "optimized_ulysses", "--gpus", "4",
                 "--prompt", "x", "--output", "/tmp/x.mp4")
        self.assertNotEqual(x.returncode, 0)
        self.assertIn("exactly 8", x.stderr)
        x = call("--backend", "optimized_single", "--steps", "50",
                 "--prompt", "x", "--output", "/tmp/x.mp4")
        self.assertNotEqual(x.returncode, 0)
        self.assertIn("8 evaluations", x.stderr)

    def test_official_single_gpu_encoder_and_fast_config(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, ckpt = fake_checkout(Path(tmp))
            run = call("--plan", "--backend", "optimized_single",
                       "--repo-root", str(root), "--checkpoint-root", str(ckpt),
                       "--prompt", "quiet morning", "--output", str(Path(tmp) / "out.mp4"),
                       "--fp8", "--frames", "141")
            self.assertEqual(run.returncode, 0, run.stderr)
            stages = json.loads(run.stdout)["stages"]
            self.assertIn("encode_prompt.py", " ".join(stages[0]))
            self.assertIn("8nfe_tuned_fp8.yaml", " ".join(stages[1]))
            self.assertIn("render.num_frames=141", stages[1])
            self.assertIn("render.num_steps=8", stages[1])

    def test_keyframed_eight_gpu_ulysses_uses_torchrun(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, ckpt = fake_checkout(Path(tmp))
            first = Path(tmp) / "frame.png"
            first.write_bytes(b"\x89PNG\r\n\x1a\n" + bytes(8))
            run = call("--plan", "--backend", "optimized_ulysses", "--gpus", "8",
                       "--gpu-class", "b200",
                       "--repo-root", str(root), "--checkpoint-root", str(ckpt),
                       "--prompt", "waves", "--first", str(first),
                       "--output", str(Path(tmp) / "out.mp4"))
            self.assertEqual(run.returncode, 0, run.stderr)
            stages = json.loads(run.stdout)["stages"]
            self.assertIn("encode_keyframes.py", " ".join(stages[0]))
            self.assertIn("--first", stages[0])
            self.assertIn("torch.distributed.run", stages[1])
            self.assertIn("--nproc_per_node=8", stages[1])
            self.assertIn("8nfe_tuned_fp8_ulysses_b200.yaml", " ".join(stages[1]))

    def test_rejects_missing_actual_native_checkpoint(self):
        with tempfile.TemporaryDirectory() as tmp:
            root, ckpt = fake_checkout(Path(tmp))
            (ckpt / "stage-dmd-step-250" / "model_spec.json").unlink()
            run = call("--plan", "--backend", "optimized_single",
                       "--repo-root", str(root), "--checkpoint-root", str(ckpt),
                       "--prompt", "test", "--output", str(Path(tmp) / "out.mp4"))
            self.assertNotEqual(run.returncode, 0)
            self.assertIn("checkpoint", run.stderr.lower())

    def test_actual_loopback_video_create_poll_download(self):
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), ApiHandler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            ApiHandler.posts.clear()
            try:
                with tempfile.TemporaryDirectory() as tmp:
                    out = Path(tmp) / "video.mp4"
                    run = call("--backend", "sglang", "--port", str(server.server_port),
                               "--prompt", "rain in a cafe", "--output", str(out),
                               "--frames", "141", "--steps", "8")
                    self.assertEqual(run.returncode, 0, run.stderr)
                    self.assertEqual(out.read_bytes()[4:8], b"ftyp")
                    self.assertEqual(ApiHandler.posts[0]["task"], "t2va")
                    self.assertEqual(ApiHandler.posts[0]["num_inference_steps"], 8)
            finally:
                server.shutdown()
                thread.join(timeout=5)


if __name__ == "__main__":
    unittest.main()

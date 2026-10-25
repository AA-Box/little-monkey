"""CPU-only native learned H3 upscaler tests; no GPU, downloads or H3 weights."""
import hashlib
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
def load(name, relative):
    spec = importlib.util.spec_from_file_location(name, ROOT / relative)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

upscaler = load("h3_upscaler", "scripts/studio_vdn_learned_upscaler.py")
worker = load("h3_native_worker", "scripts/studio-vdn-h3-worker.py")


class NativeLearnedUpscalerTests(unittest.TestCase):
    def test_geometry_limits_and_real_141_frame_latent_length(self):
        self.assertEqual(upscaler.validate_learned_latents((1, 24, 42, 24, 42)), 2.0)
        for shape, target in (
            ((2, 24, 42, 24, 42), (48, 84)),
            ((1, 16, 42, 24, 42), (48, 84)),
            ((1, 24, 103, 24, 42), (48, 84)),
            ((1, 24, 42, 24, 42), (48, 80)),
            ((1, 24, 42, 24, 42), (24, 42)),
            ((1, 24, 42, 24, 42), (120, 210)),
        ):
            with self.subTest(shape=shape, target=target), self.assertRaises(ValueError):
                upscaler.validate_learned_latents(shape, target)

    def test_checkpoint_requires_size_and_exact_sha256(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / "test.safetensors"
            path.write_bytes(b"safe model bytes")
            saved = upscaler.UPSCALER_BYTES, upscaler.UPSCALER_SHA256
            try:
                upscaler.UPSCALER_BYTES = 16
                upscaler.UPSCALER_SHA256 = hashlib.sha256(path.read_bytes()).hexdigest()
                self.assertEqual(upscaler.verify_upscaler_file(path), path)
                upscaler.UPSCALER_SHA256 = "0" * 64
                with self.assertRaisesRegex(ValueError, "SHA256"):
                    upscaler.verify_upscaler_file(path)
                upscaler.UPSCALER_BYTES = 690592672
                with self.assertRaisesRegex(ValueError, "truncated"):
                    upscaler.verify_upscaler_file(path)
            finally:
                upscaler.UPSCALER_BYTES, upscaler.UPSCALER_SHA256 = saved

    def test_refine_sigma_schedule_and_restore_even_on_error(self):
        class Scheduler:
            def __init__(self):
                self.sigmas = None
            def set_timesteps(self, num_inference_steps=None, *, sigmas=None, device=None):
                self.sigmas = sigmas
        class Pipe: pass
        pipe = Pipe()
        pipe.scheduler, pipe.audio_scheduler = Scheduler(), Scheduler()
        video_method = pipe.scheduler.set_timesteps
        audio_method = pipe.audio_scheduler.set_timesteps
        with self.assertRaisesRegex(RuntimeError, "cancel"):
            with worker.native_h3_refine_schedule(pipe, None):
                pipe.scheduler.set_timesteps(4, device="cpu")
                pipe.audio_scheduler.set_timesteps(4, device="cpu")
                self.assertEqual(pipe.scheduler.sigmas, [0.9035, 0.6316, 0.3158, 0.0])
                self.assertEqual(len(pipe.audio_scheduler.sigmas), 4)
                self.assertAlmostEqual(pipe.audio_scheduler.sigmas[-1], 0.0)
                raise RuntimeError("cancel")
        self.assertEqual(pipe.scheduler.set_timesteps, video_method)
        self.assertEqual(pipe.audio_scheduler.set_timesteps, audio_method)

    def test_real_pytorch_3d_network_small_cpu_forward_and_chunk(self):
        try:
            import torch
        except ImportError:
            self.skipTest("PyTorch optional on this runner")
        torch.manual_seed(123)
        model = upscaler.make_h3_resizer(torch, channels=32, in_blocks=1,
                                          out_blocks=1, temporal_every=1)
        with torch.inference_mode():
            source = torch.randn(1, 24, 3, 2, 2)
            lifted = model(source, scale=2.0, target_size=(3, 4, 4))
            self.assertEqual(tuple(lifted.shape), (1, 24, 3, 4, 4))
            self.assertTrue(bool(torch.isfinite(lifted).all()))
            longer = torch.randn(1, 24, 10, 2, 2)
            chunked = model(longer, scale=2.0, target_size=(10, 4, 4),
                            temporal_chunk=4)
            self.assertEqual(tuple(chunked.shape), (1, 24, 10, 4, 4))
            self.assertTrue(bool(torch.isfinite(chunked).all()))
        keys = model.state_dict()
        self.assertIn("in_blocks.0.in_layers.2.weight", keys)
        self.assertIn("in_blocks.1.dwconv.weight", keys)
        self.assertIn("out_blocks.0.out_layers.2.weight", keys)


if __name__ == "__main__":
    unittest.main()

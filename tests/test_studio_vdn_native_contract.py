"""Native VDN creative and checkpoint CLI contracts; no CUDA/model downloads."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

path = Path(__file__).resolve().parents[1] / "scripts" / "studio-vdn-h3-worker.py"
spec = importlib.util.spec_from_file_location("vdn_native", path)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class NativeVdnTests(unittest.TestCase):
    def basic(self, output, *arguments):
        return ["--prompt", "studio character", "--output", str(output),
                "--seed", "1234", "--frames", "141", *arguments]

    def test_native_two_pass_requires_checkpoint(self):
        with tempfile.TemporaryDirectory() as folder:
            with self.assertRaises(SystemExit):
                worker.parse_args(self.basic(Path(folder) / "out.mp4", "--two-pass"))

    def test_native_checkpoint_and_refine_steps_are_bounded(self):
        with tempfile.TemporaryDirectory() as folder:
            out = Path(folder) / "out.mp4"
            args = worker.parse_args(self.basic(out, "--two-pass", "--checkpoint",
                                                str(Path(folder) / "saved.safetensors")))
            self.assertTrue(args.two_pass)
            self.assertEqual(args.refine_steps, 3)
            with self.assertRaises(SystemExit):
                worker.parse_args(self.basic(out, "--two-pass", "--checkpoint",
                                                   str(out) + ".safetensors",
                                                   "--refine-steps", "0"))

    def test_audio_requires_visual_reference(self):
        with tempfile.TemporaryDirectory() as folder:
            out = Path(folder) / "out.mp4"
            audio = Path(folder) / "audio.wav"
            audio.write_bytes(b"local audio data")
            with self.assertRaises(SystemExit):
                worker.parse_args(self.basic(out, "--reference-audio", str(audio)))
            photo = Path(folder) / "ref.png"
            photo.write_bytes(b"local visual reference")
            args = worker.parse_args(self.basic(out, "--refs", str(photo),
                                                "--reference-audio", str(audio)))
            self.assertEqual(args.refs, [str(photo)])
            self.assertEqual(args.reference_audio, str(audio))

    def test_native_preview_layout_is_bounded_before_gpu_allocation(self):
        worker.validate_preview_layout((1, 24, 9, 24, 42), (2, 32, 40))
        for video, audio in (
            ((2, 24, 9, 24, 42), (2, 32, 40)),
            ((1, 24, 9, 48, 84), (2, 32, 40)),
            ((1, 24, 9, 24, 42), (1, 32, 40)),
            ((1, 24, 100000, 24, 42), (2, 32, 40)),
        ):
            with self.assertRaises(ValueError):
                worker.validate_preview_layout(video, audio)

    def test_preview_identity_changes_when_content_changes(self):
        with tempfile.TemporaryDirectory() as folder:
            out = Path(folder) / "out.mp4"
            photo = Path(folder) / "ref.png"
            photo.write_bytes(b"version one")
            args = worker.parse_args(self.basic(out, "--refs", str(photo)))
            first = worker.checkpoint_identity(args)
            photo.write_bytes(b"version two")
            self.assertNotEqual(first, worker.checkpoint_identity(args))


if __name__ == "__main__":
    unittest.main()

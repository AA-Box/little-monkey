"""CPU-only contract tests for optional FreeVideo VDN adaptive adapter."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "scripts" / "studio-vdn-freevideo-worker.py"
spec = importlib.util.spec_from_file_location("vdn_adaptive_worker", SCRIPT)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class AdaptiveContracts(unittest.TestCase):
    def test_real_freevideo_cli_flags(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            file = root / "image.png"
            file.write_bytes(b"valid locally provided reference")
            audio = root / "sample.wav"
            audio.write_bytes(b"valid locally provided audio")
            lora = root / "character.safetensors"
            lora.write_bytes(b"valid locally provided adapter")
            args = worker.args_for_worker([
                "--cache", str(root), "--prompt", "moving portrait",
                "--output", str(root / "output.mp4"),
                "--frames", "141", "--steps", "8",
                "--refs", str(file), "--reference-audio", str(audio),
                "--lora", str(lora),
            ])
            request = worker.media_request(args)
            self.assertEqual([v["kind"] for v in request["references"]], ["image", "audio"])
            self.assertEqual(request["loras"][0]["strength"], 1.0)
            cmd = worker.command(args, root / "prompt.txt", root / "media.json")
            self.assertEqual(cmd[1:4], ["-m", "freevideo_engine", "generate"])
            self.assertIn("--two-pass", cmd)
            self.assertIn("--resource-retries", cmd)
            self.assertIn("--media", cmd)

    def test_does_not_silently_mix_keyframes_and_references(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            file = root / "first.png"
            file.write_bytes(b"local test bytes")
            with self.assertRaises(SystemExit):
                worker.args_for_worker([
                    "--cache", str(root), "--prompt", "video",
                    "--output", str(root / "video.mp4"),
                    "--first", str(file), "--refs", str(file),
                ])

if __name__ == "__main__":
    unittest.main()

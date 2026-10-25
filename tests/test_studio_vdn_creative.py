"""CPU-only native OpenVDN creative argument wiring tests.

These prove the real official encoder/renderer command contracts, not model
quality or correctness of CUDA inference.
"""
import importlib.util
from pathlib import Path
import tempfile
import unittest

WORKER = Path(__file__).resolve().parents[1] / "scripts" / "studio-vdn-fast-worker.py"
spec = importlib.util.spec_from_file_location("studio_vdn_fast_worker", WORKER)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)

class CreativeCommandTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.source = self.root / "OpenVDN"
        self.ckpts = self.root / "weights"
        for filename in (
            "src/inference/encode_prompt.py",
            "src/inference/encode_keyframes.py",
            "src/inference/infer.py",
            "src/inference/infer_ulysses.py",
            "configs/inference/8nfe_tuned.yaml",
            "configs/inference/8nfe_tuned_fp8.yaml",
            "configs/inference/8nfe_tuned_fp8_ulysses_h200.yaml",
            "configs/inference/8nfe_tuned_fp8_ulysses_b200.yaml",
        ):
            p = self.source / filename
            p.parent.mkdir(parents=True, exist_ok=True)
            p.touch()
        marker = self.ckpts / "stage-dmd-step-250" / "model_spec.json"
        marker.parent.mkdir(parents=True)
        marker.write_text("{}", encoding="utf-8")
        self.ref = self.root / "ref.png"
        self.ref.write_bytes(b"\x89PNG\r\n\x1a\n" + b"x" * 128)
        self.lora = self.root / "character.safetensors"
        self.lora.write_bytes(b"z" * 128)

    def args(self, *extras):
        return worker.arguments([
            "--backend", "optimized_single",
            "--repo-root", str(self.source),
            "--checkpoint-root", str(self.ckpts),
            "--prompt", "one moving character",
            "--output", str(self.root / "render.mp4"),
            "--steps", "8", "--frames", "141", *extras,
        ])

    def test_reference_and_external_lora_reach_real_native_commands(self):
        args = self.args("--refs", str(self.ref), "--lora", str(self.lora))
        _, _, encoder, renderer = worker.native_commands(args, "python3")
        self.assertTrue(encoder[1].endswith("encode_keyframes.py"))
        self.assertIn("--refs", encoder)
        self.assertIn(str(self.ref), encoder)
        overrides = [value for value in renderer if value.startswith("external_loras=")]
        self.assertEqual(len(overrides), 1)
        self.assertIn(str(self.lora), overrides[0])

    def test_sglang_ref2va_is_refused(self):
        with self.assertRaises(SystemExit):
            worker.arguments([
                "--backend", "sglang", "--prompt", "video",
                "--output", str(self.root / "render.mp4"),
                "--refs", str(self.ref),
            ])

    def test_lora_must_be_local_safetensors(self):
        with self.assertRaises(SystemExit):
            self.args("--lora", str(self.root / "missing.safetensors"))

if __name__ == "__main__":
    unittest.main()

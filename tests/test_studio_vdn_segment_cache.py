"""Persistent actual CPU A/V window recovery and tamper/restart acceptance."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parents[1] / "scripts"
def load(name):
    spec = importlib.util.spec_from_file_location(name, SCRIPTS / (name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

try:
    import torch
    import numpy as np
    import safetensors
    READY = True
except ImportError:
    READY = False


@unittest.skipUnless(READY, "Requires PyTorch CPU, Safetensors and numpy")
class CrashSafeSegments(unittest.TestCase):
    def test_atomic_segment_record_restore_and_corruption_rejection(self):
        store = load("studio_vdn_segment_cache")
        tail_store = load("studio_vdn_temporal_store")
        with tempfile.TemporaryDirectory() as base:
            root = Path(base) / "stages"
            frames = np.ones((102, 32, 32, 3), dtype=np.uint8)
            audio = np.full((2, 136000), 0.25, dtype=np.float32)
            # The first 124-frame window's tail starts at absolute frame 102.
            high = {"video": torch.ones((1, 24, 7, 48, 84)),
                    "audio": torch.ones((2, 32, 37)),
                    "frame_origin": 102, "audio_origin": tail_store.audio_origin(102)}
            low = tail_store.preview_fallback(high)
            store.save_segment(root, 0, frames, audio, high, low,
                               model="minimax", request_hash="first",
                               predecessor_hash="no-prior", total_frames=124)
            stored = store.load_segment(root, 0, model="minimax",
                                        request_hash="first", predecessor_hash="no-prior",
                                        expected_frames=102, expected_total_frames=124)
            self.assertTrue(np.array_equal(stored["video"], frames))
            self.assertTrue(np.array_equal(stored["audio"], audio))
            self.assertTrue(torch.equal(stored["high"]["video"], high["video"]))
            with self.assertRaises(ValueError):
                store.load_segment(root, 0, model="minimax",
                                   request_hash="different", predecessor_hash="no-prior",
                                   expected_frames=102, expected_total_frames=124)
            with self.assertRaises(ValueError):
                store.load_segment(root, 0, model="minimax",
                                   request_hash="first", predecessor_hash="no-prior",
                                   expected_frames=124, expected_total_frames=124)
            cache_rgb = root / "part-0.rgb"
            with cache_rgb.open("r+b") as f:
                f.seek(100)
                f.write(b"X")
            with self.assertRaises(ValueError):
                store.load_segment(root, 0, model="minimax",
                                   request_hash="first", predecessor_hash="no-prior",
                                   expected_frames=102, expected_total_frames=124)
            store.discard_segment(root, 0)
            self.assertFalse((root / "part-0.safetensors").exists())

    def test_aborted_or_partial_record_never_appears_committed(self):
        store = load("studio_vdn_segment_cache")
        with tempfile.TemporaryDirectory() as base:
            root = Path(base)
            (root / "part-1.rgb").write_bytes(b"partial random data")
            with self.assertRaises(ValueError):
                store.load_segment(root, 1, model="minimax",
                                   request_hash="first", predecessor_hash="no-prior",
                                   expected_frames=102, expected_total_frames=124)
            store.discard_segment(root, 1)
            self.assertFalse((root / "part-1.rgb").exists())


if __name__ == "__main__":
    unittest.main()

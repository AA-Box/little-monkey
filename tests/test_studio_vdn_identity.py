"""Native H3 original-frame provenance, inheritance and tamper acceptance."""
import importlib.util
from fractions import Fraction
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("studio_vdn_identity",
                                              ROOT / "scripts/studio_vdn_identity.py")
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
try:
    import av
    from PIL import Image
    import numpy as np
    READY = True
except ImportError:
    READY = False


@unittest.skipUnless(READY, "PyAV, Pillow and numpy are required")
class IdentityLineage(unittest.TestCase):
    def sample(self, path, value):
        with av.open(str(path), "w", format="mp4") as media:
            stream = media.add_stream("libx264", rate=24)
            stream.width = 32
            stream.height = 32
            stream.pix_fmt = "yuv420p"
            for i in range(3):
                frame = av.VideoFrame.from_ndarray(
                    np.full((32, 32, 3), value, dtype=np.uint8), format="rgb24")
                frame.pts, frame.time_base = i, Fraction(1, 24)
                for packet in stream.encode(frame):
                    media.mux(packet)
            for packet in stream.encode(None):
                media.mux(packet)

    def test_inherits_first_frame_and_rejects_modified_identity(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            origin, later = root / "origin.mp4", root / "later.mp4"
            self.sample(origin, 40)
            self.sample(later, 220)
            first = root / "first.identity.png"
            child = root / "child.identity.png"
            digest = mod.save_first_identity(first, first_video=origin)
            inherited = mod.save_first_identity(
                child, first_video=later, inherited=first, expected_sha=digest)
            self.assertEqual(inherited, digest)
            self.assertEqual(first.read_bytes(), child.read_bytes())
            with Image.open(child) as image:
                self.assertLess(np.array(image).mean(), 100,
                                "Later scenes must not overwrite original picture")
            with self.assertRaises(ValueError):
                mod.save_first_identity(root / "denied.png", inherited=first,
                                        expected_sha="0"*64)
            self.assertFalse((root / "denied.png").exists())
            child.write_bytes(child.read_bytes() + b"changed")
            with self.assertRaises(ValueError):
                mod.save_first_identity(root / "tamper.png", inherited=child,
                                        expected_sha=digest)

    def test_rejects_invalid_or_oversized_image(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            bad = root / "identity.png"
            bad.write_bytes(b"not a PNG" * 50)
            with self.assertRaises(ValueError):
                mod.identity_digest(bad)
            with self.assertRaises(ValueError):
                mod.save_first_identity(root / "child.png", inherited=bad,
                                        expected_sha="0"*64)


if __name__ == "__main__":
    unittest.main()

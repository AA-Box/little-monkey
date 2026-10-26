"""Real CPU PyAV video+AAC joining: exact frames, soundtrack, source pinning."""
import hashlib
import importlib.util
from fractions import Fraction
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("studio_vdn_chain", ROOT / "scripts/studio_vdn_chain.py")
chain = importlib.util.module_from_spec(spec)
spec.loader.exec_module(chain)

try:
    import av
    import numpy as np
    HAS_AV = True
except ImportError:
    HAS_AV = False


@unittest.skipUnless(HAS_AV, "Requires optional CPU PyAV and numpy")
class NativeChainMuxTests(unittest.TestCase):
    @staticmethod
    def sample(path, frames, color):
        from fractions import Fraction
        target_samples = chain.samples_for_frames(frames)
        with av.open(str(path), "w", format="mp4") as container:
            video = container.add_stream("libx264", rate=24)
            video.width, video.height, video.pix_fmt = 32, 32, "yuv420p"
            audio = container.add_stream("aac", rate=32000)
            audio.layout = "stereo"
            for i in range(frames):
                image = np.full((32, 32, 3), color + i % 5, dtype=np.uint8)
                sample = av.VideoFrame.from_ndarray(image, format="rgb24")
                sample.pts, sample.time_base = i, Fraction(1, 24)
                for packet in video.encode(sample):
                    container.mux(packet)
            for packet in video.encode(None):
                container.mux(packet)
            pts = 0
            for begin in range(0, target_samples, 1024):
                n = min(1024, target_samples - begin)
                times = np.arange(begin, begin + n, dtype=np.float32) / 32000
                signal = np.sin(2 * np.pi * 440 * times) * np.float32(0.2)
                stereo = np.ascontiguousarray(np.stack([signal, signal * 0.75]))
                frame = av.AudioFrame.from_ndarray(stereo, format="fltp", layout="stereo")
                frame.sample_rate, frame.pts, frame.time_base = 32000, pts, Fraction(1, 32000)
                for packet in audio.encode(frame):
                    container.mux(packet)
                pts += n
            for packet in audio.encode(None):
                container.mux(packet)

    def test_join_real_mp4_with_audio_and_no_frame_duplication(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            first, second, joined = (root / n for n in ("parent.mp4", "child.mp4", "joined.mp4"))
            self.sample(first, 24, 32)
            self.sample(second, 16, 96)
            parent_sha = hashlib.sha256(first.read_bytes()).hexdigest()
            result = chain.join_native_chain(
                first, second, joined, parent_frames=24, child_frames=16,
                expected_parent_sha256=parent_sha)
            self.assertEqual(result["frames"], 40)
            self.assertEqual(result["audioSamples"], chain.samples_for_frames(40))
            with av.open(str(joined)) as clip:
                frame_count = sum(1 for _ in clip.decode(video=0))
            self.assertEqual(frame_count, 40)
            with av.open(str(joined)) as clip:
                pcm = chain._get_audio(clip, chain.samples_for_frames(40))
            self.assertEqual(pcm.shape, (2, chain.samples_for_frames(40)))
            self.assertGreater(float(np.sqrt(np.mean(pcm * pcm))), .01)
            with self.assertRaises(ValueError):
                chain.join_native_chain(first, second, root / "bad.mp4",
                                        parent_frames=24, child_frames=16,
                                        expected_parent_sha256="a"*64)
            self.assertFalse((root / "bad.mp4").exists())
            with self.assertRaises(ValueError):
                chain.join_native_chain(first, second, root / "bad2.mp4",
                                        parent_frames=25, child_frames=16,
                                        expected_parent_sha256=parent_sha)

    def test_clock_exact_and_seam_length(self):
        self.assertEqual(chain.samples_for_frames(24), 32000)
        self.assertEqual(chain.samples_for_frames(40), 53333)
        parent = np.ones((2, 500), dtype=np.float32)
        child = np.zeros((2, 500), dtype=np.float32)
        one, two = chain._smooth_seam(parent, child)
        self.assertEqual(one.shape, (2, 500))
        self.assertEqual(two.shape, (2, 500))
        self.assertAlmostEqual(float(one[0, -1]), float(two[0, 0]), delta=.01)


if __name__ == "__main__":
    unittest.main()

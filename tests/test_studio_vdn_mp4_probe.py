"""Real PyAV MP4 decode verification; no generated H3 weights required."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

import av
import numpy as np

SRC = Path(__file__).resolve().parents[1] / "scripts/studio_vdn_stream_encode.py"
spec = importlib.util.spec_from_file_location("native_mp4_probe", SRC)
stream = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stream)


def create_playable_clip(path, *, frames=5, with_audio=True):
    with av.open(str(path), mode="w") as container:
        video = container.add_stream("mpeg4", rate=24)
        video.width = 32
        video.height = 32
        video.pix_fmt = "yuv420p"
        audio = None
        if with_audio:
            audio = container.add_stream("aac", rate=32000)
            audio.layout = "stereo"
        for i in range(frames):
            rgb = np.full((32, 32, 3), 20+i*20, dtype=np.uint8)
            frame = av.VideoFrame.from_ndarray(rgb, format="rgb24")
            for packet in video.encode(frame):
                container.mux(packet)
        for packet in video.encode():
            container.mux(packet)
        if with_audio:
            for _ in range(4):
                samples = np.zeros((2, 1024), dtype=np.float32)
                frame = av.AudioFrame.from_ndarray(samples, format="fltp", layout="stereo")
                frame.sample_rate = 32000
                for packet in audio.encode(frame):
                    container.mux(packet)
            for packet in audio.encode():
                container.mux(packet)


class ActualPlayableMp4Tests(unittest.TestCase):
    def test_full_video_and_soundtrack_are_decodable(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "test.mp4"
            create_playable_clip(path)
            report = stream.verify_h3_mp4(
                path, expected_frames=5, expected_fps=24, require_audio=True)
            self.assertEqual(report["decodedFrames"], 5)
            self.assertGreater(report["decodedAudioSamples"], 0)
            self.assertTrue(report["audioPresent"])
            with self.assertRaisesRegex(RuntimeError, "frames"):
                stream.verify_h3_mp4(path, expected_frames=4, expected_fps=24)
            with self.assertRaisesRegex(RuntimeError, "frame rate"):
                stream.verify_h3_mp4(path, expected_frames=5, expected_fps=30)

    def test_audio_is_mandatory_for_h3_output(self):
        with tempfile.TemporaryDirectory() as d:
            path = Path(d) / "silent.mp4"
            create_playable_clip(path, with_audio=False)
            with self.assertRaisesRegex(RuntimeError, "stereo audio"):
                stream.verify_h3_mp4(path, expected_frames=5, expected_fps=24)
            report = stream.verify_h3_mp4(
                path, expected_frames=5, expected_fps=24, require_audio=False)
            self.assertFalse(report["audioPresent"])


if __name__ == "__main__":
    unittest.main()

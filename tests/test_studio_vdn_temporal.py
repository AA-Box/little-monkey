"""Deterministic CPU contracts for Little Monkey's native AV continuation."""
import importlib.util
from pathlib import Path
import unittest

PATH = Path(__file__).resolve().parents[1] / "scripts" / "studio_vdn_temporal.py"
spec = importlib.util.spec_from_file_location("vdn_temporal", PATH)
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)


class NativeTemporalContract(unittest.TestCase):
    def test_aligned_plan_and_no_duplicate_overlap(self):
        for frames in (124, 141, 175, 209, 260, 345):
            self.assertEqual(mod.temporal_frame_plan(frames, 1), frames)
            for count in range(2, 9):
                total = mod.temporal_frame_plan(frames, count)
                self.assertEqual(total, frames + (count - 1) * (frames - 22))
                self.assertEqual(total - mod.temporal_frame_plan(frames, count - 1),
                                 frames - 22)

    def test_reject_invalid_plans(self):
        for args in ((141, 0), (141, 9), (140, 2), (22, 2), (345, -1)):
            with self.assertRaises(ValueError):
                mod.temporal_frame_plan(*args)

    def test_absolute_audio_origin_does_not_accumulate_rounding(self):
        for frames in (124, 141, 209, 260, 345):
            for count in range(2, 9):
                current = mod.temporal_frame_plan(frames, count)
                self.assertEqual(mod.audio_origin(current),
                                 (5 * current + 1) // 3)
                next_start = current - mod.OVERLAP_FRAMES
                self.assertGreater(mod.audio_origin(next_start),
                                   mod.audio_origin(current - frames))
        self.assertEqual(mod.emitted_audio_samples(141), 188000)
        self.assertEqual(mod.emitted_audio_samples(260), 346667)

    def test_source_audio_window_is_bounded(self):
        self.assertEqual(mod.OVERLAP_FRAMES, 22)
        self.assertEqual(mod.VISUAL_CONTEXT_LATENTS, 7)
        self.assertEqual(mod.AUDIO_SAMPLE_RATE, 32000)
        self.assertEqual(mod.SAMPLES_PER_AUDIO_LATENT, 800)


if __name__ == "__main__":
    unittest.main()

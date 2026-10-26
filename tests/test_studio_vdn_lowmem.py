"""Native H3 low-memory planning contracts (runs without Torch or model weights)."""
import importlib.util
from pathlib import Path
import unittest

MODULE = Path(__file__).resolve().parents[1] / "scripts" / "studio_vdn_lowmem.py"
spec = importlib.util.spec_from_file_location("little_monkey_native_lowmem", MODULE)
low = importlib.util.module_from_spec(spec)
spec.loader.exec_module(low)


class NativeAttentionBudgetTests(unittest.TestCase):
    def test_consumable_memory_tightens_head_chunks(self):
        large = low.tile_policy(30000, 56, 128, 16 * 1024 ** 3)
        small = low.tile_policy(30000, 56, 128, 512 * 1024 ** 2)
        self.assertLessEqual(small[1], large[1])
        self.assertTrue(1 <= large[0] <= 2048)
        self.assertTrue(1 <= small[0] <= 2048)

    def test_explicit_caps_cannot_override_safety_budget(self):
        auto = low.tile_policy(80000, 56, 128, 128 * 1024**2)
        forced = low.tile_policy(80000, 56, 128, 128 * 1024**2,
                                 query_override=10000, heads_override=56)
        self.assertEqual(auto, forced)

    def test_streaming_prefetch_respects_real_resource_budget(self):
        gib = 1024**3
        constrained = low.weight_stream_policy(8*gib, 16*gib, disk_spill=True)
        self.assertFalse(constrained["denoiser_async_prefetch"])
        self.assertFalse(constrained["text_async_prefetch"])
        self.assertEqual(constrained["denoiser_blocks_per_group"], 1)
        roomy = low.weight_stream_policy(40*gib, 128*gib, disk_spill=False)
        self.assertTrue(roomy["denoiser_async_prefetch"])
        self.assertTrue(roomy["text_async_prefetch"])
        self.assertEqual(roomy["denoiser_blocks_per_group"], 1)
        sequential = low.weight_stream_policy(40*gib, 16*gib, disk_spill=True)
        self.assertEqual(sequential["denoiser_blocks_per_group"], 2)
        self.assertFalse(sequential["denoiser_async_prefetch"])
        with self.assertRaises(ValueError):
            low.weight_stream_policy(-1, 128*gib)

    def test_limits_and_invalid_geometry(self):
        self.assertEqual(low.tile_policy(100, 6, 8, 1024**3,
                                          query_override=17, heads_override=2), (17, 2))
        for settings in [(0, 8, 16, 1024), (8, 0, 16, 1024),
                         (8, 8, 0, 1024), (8, 8, 16, 0)]:
            with self.assertRaises(ValueError):
                low.tile_policy(*settings)
        with self.assertRaises(ValueError):
            low.tile_policy(100, 8, 16, 1024, query_override=-1)


if __name__ == "__main__":
    unittest.main()

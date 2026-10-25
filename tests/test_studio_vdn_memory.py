"""Native Little Monkey VDN placement tests (no GPU or model downloads)."""
import importlib.util
from pathlib import Path
import unittest

WORKER = Path(__file__).resolve().parents[1] / "scripts" / "studio-vdn-h3-worker.py"
spec = importlib.util.spec_from_file_location("little_monkey_vdn_memory", WORKER)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)
G = 1024 ** 3


class NativePlacementTests(unittest.TestCase):
    def choose(self, vram=20, ram=80, disk=120, **kwargs):
        return worker.choose_memory_plans(vram * G, ram * G, disk * G,
                                          capability=8, **kwargs)

    def test_low_vram_starts_cpu_stream_and_allows_disk_fallback(self):
        tiers = self.choose()
        self.assertEqual(tiers[0], "stream_cpu")
        self.assertIn("stream_disk", tiers)

    def test_abundant_vram_starts_resident_and_can_recover(self):
        self.assertEqual(self.choose(vram=80)[0], "resident")

    def test_forced_cpu_stream_does_not_accidentally_load_gpu_resident(self):
        self.assertEqual(self.choose(mode="stream_cpu"), ["stream_cpu"])

    def test_forced_disk_without_capacity_is_rejected(self):
        with self.assertRaisesRegex(ValueError, "96 GiB"):
            self.choose(disk=10, mode="stream_disk")

    def test_no_auto_int8_without_torchao_or_adequate_host_memory(self):
        self.assertNotIn("int8_cpu", self.choose(torchao_available=False))
        self.assertNotIn("int8_cpu", self.choose(ram=32))

    def test_fp8_is_rejected_on_non_hopper(self):
        with self.assertRaisesRegex(ValueError, "9.0"):
            self.choose(fp8=True)

    def test_new_process_oom_classifier_does_not_hide_other_failures(self):
        self.assertTrue(worker.is_cuda_oom(RuntimeError("CUDA out of memory")))
        self.assertTrue(worker.is_cuda_oom("torch.OutOfMemoryError: CUDA out of memory"))
        self.assertFalse(worker.is_cuda_oom(RuntimeError("file out of memory")))
        self.assertFalse(worker.is_cuda_oom(RuntimeError("CUDA illegal memory access")))

    def test_negative_memory_rejected(self):
        with self.assertRaises(ValueError):
            worker.choose_memory_plans(-1, 90 * G, 90 * G, capability=9)


if __name__ == "__main__":
    unittest.main()

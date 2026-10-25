"""CPU-only policy regression tests for the standalone VDN worker."""
import importlib.util
from pathlib import Path
import unittest

WORKER = Path(__file__).resolve().parents[1] / "scripts" / "studio-vdn-h3-worker.py"
spec = importlib.util.spec_from_file_location("studio_vdn_h3_worker", WORKER)
worker = importlib.util.module_from_spec(spec)
spec.loader.exec_module(worker)


class MemoryPolicyTests(unittest.TestCase):
    def test_low_vram_chooses_offload(self):
        self.assertTrue(worker.choose_memory_policy(20 * 1024 ** 3,
            fp8=False, offload=False, fp8_supported=False)["offload"])

    def test_high_vram_resident_and_explicit_offload(self):
        self.assertFalse(worker.choose_memory_policy(80 * 1024 ** 3,
            fp8=False, offload=False, fp8_supported=True)["offload"])
        self.assertTrue(worker.choose_memory_policy(80 * 1024 ** 3,
            fp8=False, offload=True, fp8_supported=True)["offload"])

    def test_fp8_never_auto_selected_unsupported(self):
        self.assertFalse(worker.choose_memory_policy(80 * 1024 ** 3,
            fp8=True, offload=False, fp8_supported=False)["fp8"])

    def test_retries_only_cuda_oom(self):
        self.assertTrue(worker.is_cuda_oom(RuntimeError("CUDA out of memory")))
        self.assertFalse(worker.is_cuda_oom(RuntimeError("file out of memory")))
        self.assertFalse(worker.is_cuda_oom(RuntimeError("CUDA illegal access")))

    def test_rejects_negative_memory(self):
        with self.assertRaises(ValueError):
            worker.choose_memory_policy(-1, fp8=False, offload=False,
                                        fp8_supported=False)


if __name__ == "__main__":
    unittest.main()

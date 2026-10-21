#!/usr/bin/env python3
"""Non-model contract tests only. These do NOT count as benchmark acceptance."""
import importlib.util
import os
from pathlib import Path
import shutil
import sys
import unittest

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("bench_jcode_parity", HERE / "bench-jcode-parity.py")
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)


class BenchmarkContract(unittest.TestCase):
    def test_exact_five_turns_include_read_only_tools_and_history(self):
        self.assertEqual(len(bench.TURNS), 5)
        self.assertIn("list the files", bench.TURNS[0])
        self.assertIn("README.md", bench.TURNS[1])
        self.assertIn("Search the repository", bench.TURNS[2])
        self.assertIn("learned", bench.TURNS[3])
        self.assertIn("done", bench.TURNS[4])

    def test_missing_executable_fails_closed(self):
        with self.assertRaisesRegex(RuntimeError, "not found"):
            bench.require_binary("little-monkey-benchmark-nonexistent-executable-23012")

    @unittest.skipUnless(Path("/proc/self/smaps_rollup").exists(), "Linux PSS only")
    def test_pss_reads_real_proc_mapping_not_rss(self):
        self.assertGreater(bench.process_tree_pss_kb([os.getpid()]), 0)
        self.assertTrue(shutil.which("ps"))


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Offline contract tests plus actual OS kernel sampler; no fake model pass."""
import os
from pathlib import Path
import runpy
import tempfile
import unittest

MODULE = runpy.run_path(str(Path(__file__).with_name(
    "paired-jcode-littlemonkey-live.py"
)))


class PhysicalComparisonContracts(unittest.TestCase):
    def test_real_linux_self_process_kernel_pss(self):
        if not Path("/proc").is_dir():
            self.skipTest("Linux OS PSS required")
        # The process tree includes a short-lived 'ps' child in its snapshot.
        # Linux may reap that PID before we read /proc: root-only PSS tests
        # the actual kernel path without fabricating a complete tree.
        measured = MODULE["read_process_pss_kib"](os.getpid())
        self.assertIsInstance(measured, int)
        self.assertGreater(measured, 0)

    def test_fixture_digest_rejects_mutation_and_symlinks(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            (root / "fixture.txt").write_text("Original marker\n")
            digest = MODULE["fixture_digest"](root)
            self.assertEqual(MODULE["fixture_digest"](root), digest)
            (root / "fixture.txt").write_text("Changed marker\n")
            self.assertNotEqual(MODULE["fixture_digest"](root), digest)
            (root / "bad-link").symlink_to("fixture.txt")
            with self.assertRaisesRegex(RuntimeError, "symlink"):
                MODULE["fixture_digest"](root)

    def test_incomplete_memory_evidence_never_yields_a_pss_peak(self):
        old = MODULE["live_pss_kib"]
        try:
            MODULE["live_pss_kib"] = lambda _: None
            result = MODULE["wave"](1, {"jcode": 123}, "jcode",
                                   [lambda: {"completed": True, "seconds": .4}], 30)
            self.assertIsNone(result["peakPssMiB"])
        finally:
            MODULE["live_pss_kib"] = old


if __name__ == "__main__":
    unittest.main()

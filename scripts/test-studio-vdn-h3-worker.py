#!/usr/bin/env python3
"""CPU-only real subprocess acceptance tests for Little Monkey's VDN worker.

These tests never download restricted weights or claim to measure CUDA model
performance. They execute the *actual* Python worker under Python on CI.
"""
import json
import subprocess
import sys
import unittest
from pathlib import Path

WORKER = Path(__file__).with_name("studio-vdn-h3-worker.py")


def invoke(*arguments):
    return subprocess.run(
        [sys.executable, str(WORKER), *arguments],
        check=False, capture_output=True, text=True, timeout=15,
    )


class VdnWorkerContract(unittest.TestCase):
    def test_no_cuda_or_weight_download_for_self_test(self):
        result = invoke("--self-test")
        self.assertEqual(result.returncode, 0, result.stderr)
        report = json.loads(result.stdout)
        self.assertEqual(report["backend"], "openvdn-diffusers")
        self.assertEqual(report["model"], "OpenVDN/vdn-minimax-h3")
        self.assertEqual(report["fps"], 24)

    def test_invalid_frames_are_rejected_without_loading_models(self):
        result = invoke("--prompt", "hello", "--output", "/tmp/sample.mp4",
                        "--frames", "126")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("17n+5", result.stderr)

    def test_prompt_required_before_python_model_imports(self):
        result = invoke("--output", "/tmp/sample.mp4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prompt", result.stderr.lower())

    def test_no_arbitrary_relative_output_or_missing_images(self):
        result = invoke("--prompt", "hello", "--output", "relative.mp4")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("absolute output", result.stderr.lower())
        result = invoke("--prompt", "hello", "--output", "/tmp/sample.mp4",
                        "--first", "/unreachable/frame.png")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("keyframe", result.stderr.lower())

    def test_cuda_only_requirement_is_validated_before_model_download(self):
        result = invoke("--prompt", "hello", "--output", "/tmp/sample.mp4",
                        "--device", "cpu")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cuda only", result.stderr.lower())


if __name__ == "__main__":
    unittest.main()

"""CPU-only completed-step crash and dual-scheduler cursor acceptance.

This runs the actual native state-machine helpers; it deliberately does not
pretend to execute CUDA kernels or load proprietary model checkpoints.
"""
import importlib.util
from pathlib import Path
import unittest

SOURCE = Path(__file__).resolve().parents[1] / "scripts" / "studio_vdn_sampler.py"
spec = importlib.util.spec_from_file_location("studio_vdn_sampler", SOURCE)
sampler = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sampler)


class SamplerRecoveryTests(unittest.TestCase):
    def test_interrupted_step_is_repeated_but_committed_steps_are_not(self):
        def compute(start=0, total=7, fail_at=None, video=0, audio=0):
            completed = []
            executed = []
            state = {"video": video, "audio": audio}
            def one(index):
                executed.append(index)
                if index == fail_at:
                    # Simulate the interruption partway through a step.
                    state["video"] += 1000000
                    raise RuntimeError("CUDA worker terminated")
                state["video"] = state["video"] * 3 + (index + 1)
                state["audio"] = state["audio"] * 5 + (index + 7)
            def commit(cursor):
                completed.append((cursor, state["video"], state["audio"]))
            try:
                sampler.run_completed_steps(total, start, one, commit)
            except RuntimeError:
                pass
            return completed, executed
        uninterrupted, _ = compute()
        partial, steps = compute(fail_at=3)
        self.assertEqual([s[0] for s in partial], [1, 2, 3])
        self.assertEqual(steps, [0, 1, 2, 3])
        step, video, audio = partial[-1]
        resumed, resumed_steps = compute(start=step, video=video, audio=audio)
        self.assertEqual(resumed_steps, [3, 4, 5, 6])
        self.assertEqual(resumed[-1], uninterrupted[-1])

    def test_already_complete_cursor_executes_no_steps(self):
        calls = []
        sampler.run_completed_steps(8, 8, lambda i: calls.append(i),
                                    lambda completed: calls.append(completed))
        self.assertEqual(calls, [])

    def test_cursor_validation_and_failed_step_never_commits(self):
        for total, start in [(0, 0), (5, -1), (4, 5)]:
            with self.assertRaises(ValueError):
                sampler.run_completed_steps(total, start, lambda i: None,
                                            lambda i: None)
        commits = []
        def failed(_):
            raise RuntimeError("mid kernel")
        with self.assertRaisesRegex(RuntimeError, "mid kernel"):
            sampler.run_completed_steps(3, 0, failed, commits.append)
        self.assertEqual(commits, [])

    def test_checkpoint_metadata_requires_exact_model_inputs_and_schedule(self):
        store = sampler.NativeStepStore(
            Path("/tmp").resolve() / "native-vdn-cursor-fixture",
            "preview", "sha256-source", 8, "both-sigmas",
            (1536, 24), (128, 32))
        good = store.metadata(3)
        self.assertEqual(sampler.verify_step_metadata(
            good, "sha256-source", "preview", 8,
            "both-sigmas", (1536, 24), (128, 32)), 3)
        for key, value in [
            ("identity", "another-job"), ("stage", "refine"),
            ("schedule", "changed-sigmas"), ("completed", "99"),
            ("audio_shape", "256,32"), ("video_shape", "1024,24"),
        ]:
            bad = {**good, key: value}
            self.assertIsNone(sampler.verify_step_metadata(
                bad, "sha256-source", "preview", 8,
                "both-sigmas", (1536, 24), (128, 32)))
        store.clear()


if __name__ == "__main__":
    unittest.main()

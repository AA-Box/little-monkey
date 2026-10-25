"""Functional first-party original-SafeTensors streaming tests on real CPU torch.

No Hugging Face downloads; fixture creates original indexed shards, streams
individual params directly using native onload hooks, and checks model parity,
memory eviction, adapter math, weight_norm ordering and crash safety.
"""
import importlib.util
from pathlib import Path
import tempfile
import unittest

import torch
from torch import nn
import torch.nn.functional as F
from safetensors.torch import save_file


SCRIPT = Path(__file__).resolve().parents[1] / "scripts/studio_vdn_native_shards.py"
spec = importlib.util.spec_from_file_location("studio_vdn_original_shards", SCRIPT)
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class Projection(nn.Module):
    def __init__(self):
        super().__init__()
        self.q = nn.Linear(8, 12)
        self.k = nn.Linear(12, 8)
        self.register_buffer("fixed_nonpersistent", torch.arange(3), persistent=False)
    def forward(self, x):
        return self.k(torch.relu(self.q(x)))


def make_original_shards(model, folder):
    folder = Path(folder)
    weights = list(model.state_dict().items())
    mid = max(1, len(weights) // 2)
    mapping, files = {}, {}
    for i, pairs in enumerate((weights[:mid], weights[mid:])):
        file = "model-%05d-of-00002.safetensors" % (i+1)
        save_file({key: t.detach().contiguous() for key, t in pairs},
                  str(folder / file), metadata={"format": "pt"})
        mapping.update({key: file for key, _ in pairs})
        files[file] = folder / file
    return native.OriginalShardStore({"weight_map": mapping}, files)


class OriginalShardNativeTests(unittest.TestCase):
    def test_direct_original_weight_execution_eviction_and_no_copy(self):
        torch.manual_seed(3)
        reference = Projection().eval()
        x = torch.randn(4, 8)
        expected = reference(x)
        with tempfile.TemporaryDirectory() as root:
            store = make_original_shards(reference, root)
            # Meta makes *zero* resident parameter allocations.
            with torch.device("meta"):
                streamed = Projection()
            streamed.fixed_nonpersistent = torch.arange(3)
            hooks = native.NativeOriginalShardHooks(
                streamed, store, device="cpu", component="test")
            self.assertGreater(hooks.layers, 0)
            self.assertTrue(all(p.is_meta for p in streamed.parameters()))
            self.assertEqual(set(Path(root).glob("*")), set(store.files.values()))
            torch.testing.assert_close(streamed(x), expected)
            torch.testing.assert_close(streamed(x), expected)
            self.assertTrue(all(p.is_meta for p in streamed.parameters()))
            self.assertFalse(hooks.loaded)
            # No Accelerate .dat, export of any new weight copy or shard rewrite.
            self.assertTrue(all(p.suffix == ".safetensors"
                                for p in Path(root).iterdir()))

    def test_exception_rolls_back_loaded_weights_for_retry(self):
        class Explodes(nn.Linear):
            def forward(self, x):
                raise RuntimeError("interrupted during forward")
        original = Explodes(8, 8)
        x = torch.randn(2, 8)
        with tempfile.TemporaryDirectory() as root:
            store = make_original_shards(original, root)
            with torch.device("meta"):
                model = Explodes(8, 8)
            handles = native.NativeOriginalShardHooks(
                model, store, device="cpu", component="interrupted")
            for _ in range(2):
                with self.assertRaisesRegex(RuntimeError, "interrupted"):
                    model(x)
                self.assertTrue(all(p.is_meta for p in model.parameters()))
                self.assertFalse(handles.loaded)

    def test_native_lora_overlay_matches_manual_reference(self):
        torch.manual_seed(12)
        model = Projection().eval()
        x = torch.randn(2, 8)
        rank = 3
        a = torch.randn(rank, 8)
        b = torch.randn(12, rank)
        manual = model.k(F.relu(model.q(x) +
                                   F.linear(F.linear(x, a), b)))
        with tempfile.TemporaryDirectory() as root:
            store = make_original_shards(model, root)
            adapter = Path(root) / "adapter.safetensors"
            save_file({"q.lora_A.weight": a, "q.lora_B.weight": b, "q.alpha": torch.tensor(float(rank))},
                      str(adapter))
            with torch.device("meta"):
                streamed = Projection()
            streamed.fixed_nonpersistent = torch.arange(3)
            native.NativeOriginalShardHooks(streamed, store, device="cpu",
                                             component="test")
            self.assertEqual(native.attach_native_lora(streamed, [adapter]), 1)
            torch.testing.assert_close(streamed(x), manual, atol=1e-6, rtol=1e-6)
            self.assertTrue(all(p.is_meta for p in streamed.parameters()))
            with self.assertRaisesRegex(ValueError, "Unsupported LoRA state key"):
                empty = Path(root) / "empty.safetensors"
                save_file({"other": torch.zeros(1)}, str(empty))
                native.attach_native_lora(streamed, [empty])

    def test_weight_norm_legacy_hook_runs_after_hydration(self):
        from torch.nn.utils import weight_norm
        class AudioBlock(nn.Module):
            def __init__(self):
                super().__init__()
                self.conv = weight_norm(nn.Conv1d(2, 4, kernel_size=3, padding=1))
            def forward(self, x):
                return self.conv(x)
        torch.manual_seed(8)
        ref = AudioBlock().eval()
        x = torch.randn(1, 2, 13)
        expected = ref(x)
        with tempfile.TemporaryDirectory() as root:
            store = make_original_shards(ref, root)
            with torch.device("meta"):
                model = AudioBlock()
            native.NativeOriginalShardHooks(model, store, device="cpu",
                                             component="audio_vae")
            torch.testing.assert_close(model(x), expected, atol=1e-6, rtol=1e-6)
            self.assertTrue(all(p.is_meta for p in model.parameters()))
            self.assertTrue(model.conv.weight.is_meta)

    def test_fresh_process_after_kill_mid_forward_keeps_original_shards(self):
        import hashlib
        import subprocess
        import sys
        torch.manual_seed(42)
        original = nn.Linear(8, 8).eval()
        inputs = torch.randn(2, 8)
        expected = original(inputs)
        with tempfile.TemporaryDirectory() as root:
            store = make_original_shards(original, root)
            before = {name: hashlib.sha256(path.read_bytes()).digest()
                      for name, path in store.files.items()}
            script = """
import importlib.util, sys, time, torch
from torch import nn
from pathlib import Path
from safetensors import safe_open
script, root = sys.argv[1:]
spec = importlib.util.spec_from_file_location('stream', script)
stream = importlib.util.module_from_spec(spec)
spec.loader.exec_module(stream)
class Slow(nn.Linear):
    def forward(self, x):
        print('hydrated-original-shard', flush=True)
        time.sleep(60)
        return super().forward(x)
with torch.device('meta'):
    model = Slow(8, 8)
files = {p.name: p for p in Path(root).glob('*.safetensors')}
mapping = {'weight': sorted(files)[0], 'bias': sorted(files)[1]}
index = {'weight_map': mapping}
store = stream.OriginalShardStore(index, files)
stream.NativeOriginalShardHooks(model, store, device='cpu', component='killed')
model(torch.ones(2, 8))
"""
            # Use a deliberately single-item shard to avoid relying on
            # dictionary iteration order in the subprocess manifest.
            one = Path(root) / "single.safetensors"
            save_file({k: v.detach().contiguous()
                       for k, v in original.state_dict().items()}, str(one))
            child = script.replace(
                "files = {p.name: p for p in Path(root).glob('*.safetensors')}\nmapping = {'weight': sorted(files)[0], 'bias': sorted(files)[1]}",
                "files = {'single.safetensors': Path(root) / 'single.safetensors'}\nmapping = {'weight': 'single.safetensors', 'bias': 'single.safetensors'}"
            )
            process = subprocess.Popen(
                [sys.executable, "-u", "-c", child, str(SCRIPT), root],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                line = process.stdout.readline().strip()
                self.assertEqual(line, "hydrated-original-shard",
                                 process.stderr.read() if process.poll() is not None else "")
            finally:
                process.kill()
                process.communicate(timeout=15)
            self.assertNotEqual(process.returncode, 0)
            after = {name: hashlib.sha256(path.read_bytes()).digest()
                     for name, path in store.files.items()}
            self.assertEqual(before, after)
            with torch.device("meta"):
                resumed = nn.Linear(8, 8)
            fresh_store = native.OriginalShardStore(
                {"weight_map": {"weight": "single.safetensors",
                                "bias": "single.safetensors"}},
                {"single.safetensors": one})
            native.NativeOriginalShardHooks(
                resumed, fresh_store, device="cpu", component="restarted")
            torch.testing.assert_close(resumed(inputs), expected)

    def test_shard_map_rejects_traversal_and_missing_weights(self):
        with self.assertRaises(ValueError):
            native.validate_weight_map({"weight_map": {"weight": "../x.safetensors"}})
        with tempfile.TemporaryDirectory() as root:
            ref = Projection()
            store = make_original_shards(ref, root)
            with torch.device("meta"):
                model = nn.Linear(8, 8)
            with self.assertRaisesRegex(ValueError, "Missing original weight"):
                native.NativeOriginalShardHooks(model, store, device="cpu", component="bad")


if __name__ == "__main__":
    unittest.main()

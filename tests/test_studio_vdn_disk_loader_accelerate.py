"""Real Accelerate load_checkpoint_and_dispatch SafeTensors disk I/O.

This is a tiny but *functional* shard-backed model forward, not a simulated
disk plan. It runs on CPU so CI needs no licensed H3 model or CUDA.
"""
import json
from pathlib import Path
import tempfile
import unittest

import torch
from torch import nn
from safetensors.torch import save_file
from accelerate import init_empty_weights, load_checkpoint_and_dispatch


def tiny_model():
    return nn.Sequential(nn.Linear(8, 8), nn.ReLU(),
                         nn.Linear(8, 8), nn.Linear(8, 8)).eval()


class ActualShardedAccelerateDiskTests(unittest.TestCase):
    def test_sharded_safetensors_disk_layer_forward_matches_full_reference(self):
        torch.manual_seed(24680)
        original = tiny_model()
        x = torch.randn(3, 8)
        with torch.no_grad():
            expected = original(x)
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            ckpt = root / "checkpoint"
            ckpt.mkdir()
            groups = {
                "model-00001-of-00002.safetensors": {
                    k: v.detach().contiguous()
                    for k, v in original.state_dict().items() if k.startswith(("0.", "3."))},
                "model-00002-of-00002.safetensors": {
                    k: v.detach().contiguous()
                    for k, v in original.state_dict().items() if k.startswith("2.")},
            }
            mapping = {}
            for filename, tensors in groups.items():
                save_file(tensors, str(ckpt / filename))
                mapping.update({name: filename for name in tensors})
            (ckpt / "model.safetensors.index.json").write_text(
                json.dumps({"metadata": {}, "weight_map": mapping}), encoding="utf-8")
            with init_empty_weights():
                model = tiny_model()
            self.assertTrue(all(p.is_meta for p in model.parameters()))
            loaded = load_checkpoint_and_dispatch(
                model, str(ckpt),
                device_map={"0": "cpu", "2": "disk", "3": "cpu"},
                offload_folder=str(root / "disk_offload"),
                offload_state_dict=True,
                dtype=torch.float32, strict=True,
            )
            self.assertEqual(loaded.hf_device_map["2"], "disk")
            self.assertTrue((root / "disk_offload").is_dir())
            with torch.no_grad():
                actual = loaded(x)
                twice = loaded(x)
            torch.testing.assert_close(actual, expected, atol=1e-6, rtol=1e-6)
            torch.testing.assert_close(twice, expected, atol=1e-6, rtol=1e-6)
            # The disk layer is evicted after each forward rather than
            # permanently occupying CPU storage like the eager loader.
            self.assertTrue(loaded[2].weight.is_meta)


if __name__ == "__main__":
    unittest.main()

"""Real PyTorch CPU numerical tests for first-party full-context H3 attention.

Only Diffusers model type and rotary-name are stubbed: no CUDA/hub weights.
The attention, projection, SDPA and FFN operations are real PyTorch.
"""
import importlib.util
from pathlib import Path
import sys
import types
import unittest

import torch
from torch import nn
import torch.nn.functional as F

names = ["diffusers", "diffusers.models", "diffusers.models.transformers",
         "diffusers.models.transformers.transformer_minimax_h3"]
previous = {name: sys.modules.get(name) for name in names}
for name in names:
    sys.modules[name] = types.ModuleType(name)


class H3Attention(nn.Module):
    def __init__(self, fused):
        super().__init__()
        self.heads, self.head_dim, self.fused_projections = 6, 8, fused
        self.norm_q, self.norm_k = nn.LayerNorm(8), nn.LayerNorm(8)
        if fused:
            self.to_qkv = nn.Linear(32, 144)
        else:
            self.to_q, self.to_k, self.to_v = (
                nn.Linear(32, 48), nn.Linear(32, 48), nn.Linear(32, 48))
        self.to_out = nn.ModuleList([nn.Linear(48, 32), nn.Identity()])
        self.processor = None

    def set_processor(self, processor):
        self.processor = processor


sys.modules[names[-1]].MiniMaxH3Attention = H3Attention
sys.modules[names[-1]]._apply_rotary_emb = lambda x, *_: x
path = Path(__file__).resolve().parents[1] / "scripts/studio_vdn_lowmem.py"
spec = importlib.util.spec_from_file_location("little_monkey_h3_torch", path)
low = importlib.util.module_from_spec(spec)
spec.loader.exec_module(low)


def full_context(attn, x):
    if attn.fused_projections:
        q, k, v = attn.to_qkv(x).chunk(3, dim=-1)
    else:
        q, k, v = attn.to_q(x), attn.to_k(x), attn.to_v(x)
    q = attn.norm_q(q.unflatten(-1, (attn.heads, attn.head_dim)))
    k = attn.norm_k(k.unflatten(-1, (attn.heads, attn.head_dim)))
    v = v.unflatten(-1, (attn.heads, attn.head_dim))
    out = F.scaled_dot_product_attention(q.transpose(1, 2), k.transpose(1, 2), v.transpose(1, 2))
    return attn.to_out[1](attn.to_out[0](out.transpose(1, 2).flatten(2, 3)))


class NativeH3Parity(unittest.TestCase):
    def test_query_and_head_chunk_match_full_attention(self):
        torch.manual_seed(21)
        for fused in (False, True):
            attn = H3Attention(fused).eval()
            x = torch.randn(1, 73, 32)
            reference = full_context(attn, x)
            for query, heads in ((1, 1), (7, 2), (20, 4), (73, 6)):
                with self.subTest(fused=fused, q=query, h=heads):
                    actual = low.native_h3_attention(
                        attn, x, query_chunk=query, head_chunk=heads)
                    torch.testing.assert_close(actual, reference, atol=1e-5, rtol=1e-5)

    def test_quantized_style_projection_is_actually_token_streamed(self):
        torch.manual_seed(25)
        class WrappedLinear(nn.Module):
            def __init__(self, layer):
                super().__init__()
                self.inner = layer
                self.largest_input = 0
            def forward(self, x):
                self.largest_input = max(self.largest_input, x.shape[1])
                return self.inner(x)
        attn = H3Attention(True).eval()
        x = torch.randn(1, 51, 32)
        reference = full_context(attn, x)
        attn.to_qkv = WrappedLinear(attn.to_qkv)
        actual = low.native_h3_attention(
            attn, x, query_chunk=5, head_chunk=2)
        torch.testing.assert_close(actual, reference, atol=1e-5, rtol=1e-5)
        self.assertLessEqual(attn.to_qkv.largest_input, 5)

    def test_mask_does_not_get_ignored(self):
        attn = H3Attention(False)
        x = torch.randn(1, 15, 32)
        got = low.native_h3_attention(
            attn, x, attention_mask=torch.ones(15, 15),
            fallback=lambda _attn, states, _rot, _mask: states * 2)
        torch.testing.assert_close(got, x * 2)

    def test_chunked_ffn_is_numerically_equivalent_and_installed_once(self):
        class FeedForward(nn.Module):
            def __init__(self):
                super().__init__()
                self.net = nn.Sequential(nn.Linear(32, 96), nn.GELU(), nn.Linear(96, 32))
            def forward(self, x):
                return self.net(x)
        class TinyModel(nn.Module):
            def __init__(self):
                super().__init__()
                self.attn = H3Attention(False)
                self.ff = FeedForward()
        model = TinyModel()
        x = torch.randn(1, 81, 32)
        expected = model.ff(x)
        self.assertEqual(low.install_native_h3_tiles(model, ff_chunk=9, verbose=False), (1, 1))
        torch.testing.assert_close(model.ff(x), expected, atol=1e-6, rtol=1e-6)
        self.assertEqual(low.install_native_h3_tiles(model, ff_chunk=9, verbose=False), (0, 0))


if __name__ == "__main__":
    unittest.main()

"""Real PyTorch CPU stereo preservation and small-tile native VAE tests."""
import importlib.util
from pathlib import Path
import sys
import types
import unittest
import torch
from torch import nn


packages = ["diffusers", "diffusers.models", "diffusers.models.autoencoders",
            "diffusers.models.autoencoders.autoencoder_kl_minimax_h3",
            "diffusers.models.autoencoders.autoencoder_kl_minimax_h3_audio",
            "diffusers.models.autoencoders.vae"]
for name in packages:
    sys.modules[name] = types.ModuleType(name)


class VideoVAE(nn.Module):
    def enable_tiling(self, **kwargs):
        self.tiling = kwargs
    def enable_slicing(self):
        self.slicing = True


class AudioVAE(nn.Module):
    def __init__(self):
        super().__init__()
        self.decoder = nn.Conv1d(4, 2, kernel_size=5, padding=2).eval()
    def decode(self, latents, return_dict=True):
        signal = self.decoder(latents)
        return types.SimpleNamespace(sample=signal) if return_dict else (signal,)


sys.modules[packages[3]].AutoencoderKLMiniMaxH3 = VideoVAE
sys.modules[packages[4]].AutoencoderKLMiniMaxH3Audio = AudioVAE
sys.modules[packages[5]].DecoderOutput = lambda sample: types.SimpleNamespace(sample=sample)
spec = importlib.util.spec_from_file_location(
    "native_video_audio_decoder",
    Path(__file__).resolve().parents[1] / "scripts/studio_vdn_decoder.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class NativeDecoderTests(unittest.TestCase):
    def test_real_stereo_decode_is_identical_and_chunked_in_batch(self):
        torch.manual_seed(77)
        video = VideoVAE()
        audio = AudioVAE()
        latents = torch.randn(2, 4, 71)
        expected = audio.decode(latents, return_dict=False)[0]
        observed_batch_sizes = []
        def record_batch(_mod, inputs):
            observed_batch_sizes.append(inputs[0].shape[0])
        audio.decoder.register_forward_pre_hook(record_batch)
        native.install_lowmem_decoders(
            video, audio, free_vram=9 * 1024**3, verbose=False)
        actual = audio.decode(latents, return_dict=False)[0]
        torch.testing.assert_close(actual, expected, atol=1e-6, rtol=1e-6)
        self.assertEqual(observed_batch_sizes, [1, 1])
        self.assertEqual(video.tiling["tile_sample_min_height"], 128)
        self.assertEqual(video.tiling["tile_sample_min_overlap_width"], 64)
        self.assertTrue(video.slicing)

    def test_decoder_memory_tiers_and_idempotent_hook(self):
        g = 1024**3
        self.assertEqual(native.decode_policy(8*g)["tile_side"], 128)
        self.assertEqual(native.decode_policy(16*g)["tile_side"], 192)
        self.assertEqual(native.decode_policy(32*g)["tile_side"], 256)
        with self.assertRaises(ValueError):
            native.decode_policy(-5)
        video, audio = VideoVAE(), AudioVAE()
        native.install_lowmem_decoders(video, audio, free_vram=10*g, verbose=False)
        first = audio.decode
        native.install_lowmem_decoders(video, audio, free_vram=10*g, verbose=False)
        self.assertEqual(first, audio.decode)


if __name__ == "__main__":
    unittest.main()

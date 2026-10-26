"""Memory bounded, native MiniMax H3 video/audio decode configuration.

Video VAE retains the official temporal-chunk overlap and spatial blending;
we reduce only spatial tile geometry (not attention temporal context).
Audio VAE uses *FP32* and decodes stereo-as-batch one channel at a time.
Never split audio along time without a proven BigVGAN receptive-field halo.
"""
import functools
import json
import types


def decode_policy(free_vram):
    gib = 1024**3
    if free_vram < 0:
        raise ValueError("Negative decoder VRAM")
    side = 128 if free_vram < 12*gib else 192 if free_vram < 24*gib else 256
    return {"tile_side": side, "spatial_overlap": 64,
            "audio_batch_chunk": 1, "keep_audio_fp32": True}


def install_lowmem_decoders(vae, audio_vae, *, free_vram, verbose=True):
    """Preserve the reference VAE computations and audio sample ordering."""
    from diffusers.models.autoencoders.autoencoder_kl_minimax_h3 import AutoencoderKLMiniMaxH3
    from diffusers.models.autoencoders.autoencoder_kl_minimax_h3_audio import AutoencoderKLMiniMaxH3Audio
    if not isinstance(vae, AutoencoderKLMiniMaxH3):
        raise TypeError("Not the original MiniMax H3 video VAE")
    if not isinstance(audio_vae, AutoencoderKLMiniMaxH3Audio):
        raise TypeError("Not the original MiniMax H3 audio VAE")
    plan = decode_policy(free_vram)
    vae.enable_tiling(
        tile_sample_min_height=plan["tile_side"],
        tile_sample_min_width=plan["tile_side"],
        tile_sample_min_overlap_height=plan["spatial_overlap"],
        tile_sample_min_overlap_width=plan["spatial_overlap"],
    )
    vae.enable_slicing()
    if getattr(audio_vae, "_little_monkey_audio_sliced", False):
        return plan
    original_encode = audio_vae.encode
    @functools.wraps(original_encode)
    def sliced_encode(self, samples, return_dict=True):
        if samples.ndim != 3 or samples.shape[0] <= 1:
            return original_encode(samples, return_dict=return_dict)
        import torch
        from diffusers.models.autoencoders.autoencoder_kl_minimax_h3_audio import (
            MiniMaxH3AudioDiagonalGaussianDistribution, MiniMaxH3AudioEncoderOutput,
        )
        means, logs = [], []
        for mono in samples.split(1, dim=0):
            posterior = original_encode(mono, return_dict=False)[0]
            means.append(posterior.mean)
            logs.append(posterior.logs)
        distribution = MiniMaxH3AudioDiagonalGaussianDistribution(
            torch.cat(means, dim=0), torch.cat(logs, dim=0))
        if not return_dict:
            return (distribution,)
        return MiniMaxH3AudioEncoderOutput(latent_dist=distribution)

    audio_vae.encode = types.MethodType(sliced_encode, audio_vae)
    original_decode = audio_vae.decode

    @functools.wraps(original_decode)
    def sliced_decode(self, latents, return_dict=True):
        if latents.ndim != 3 or latents.shape[0] <= 1:
            return original_decode(latents, return_dict=return_dict)
        import torch
        # Run the actual original BigVGAN decoder per mono channel.
        # Concatenating two independent stereo batch items is mathematically
        # the same operation as a single batch of size 2; no truncation,
        # padding, resampling or artificial crossfades are introduced.
        signals = []
        for mono in latents.split(1, dim=0):
            sound = original_decode(mono, return_dict=False)[0]
            signals.append(sound)
        combined = torch.cat(signals, dim=0)
        if not return_dict:
            return (combined,)
        from diffusers.models.autoencoders.vae import DecoderOutput
        return DecoderOutput(sample=combined)

    audio_vae.decode = types.MethodType(sliced_decode, audio_vae)
    audio_vae._little_monkey_audio_sliced = True
    if verbose:
        print(json.dumps({"phase": "native_vae_decoder_streaming",
                          "videoSpatialTile": plan["tile_side"],
                          "videoSpatialOverlap": plan["spatial_overlap"],
                          "officialTemporalChunking": True,
                          "audioChannelsSequential": True,
                          "audioReferenceEncoderSequential": True,
                          "audioWaveformFp32": True}), flush=True)
    return plan

"""Native incremental H3 Video VAE decode and MP4 write.

The official MiniMax H3 temporal overlap, pad, and blend equations are reused.
Only a single temporal clip (plus its overlap) is on GPU; the final decoded
frames are never accumulated in memory.  The existing Diffusers encode_video
already supports an iterator of uint8 frame *chunks* and interleaves the
original stereo soundtrack via PyAV.
"""
def iter_h3_video_rgb_chunks(vae, latents, pixel_mean, pixel_std, *,
                             expected_frames):
    import torch
    if latents.ndim != 5 or latents.shape[0] != 1:
        raise ValueError("Native H3 video latents must be [1,C,T,H,W]")
    if expected_frames < 1 or expected_frames > 360:
        raise ValueError("Invalid native video frame count")
    t = int(vae.tokens_chunk_size)
    drop = int(vae.config.token_drop)
    temporal = int(vae.temporal_compression_ratio)
    frame_window = t * temporal
    total_tokens = latents.shape[2] + drop
    padding = (-total_tokens) % t
    chunks = (total_tokens + padding) // t - int(drop > 0)
    if chunks <= 0:
        raise RuntimeError("Invalid native VAE temporal geometry")
    # Delay only a single overlap, NOT the full movie. Official _decode_clip
    # includes the original spatial tiling and post_quant_conv.
    mean = torch.as_tensor(pixel_mean, device=latents.device, dtype=torch.float32).view(1, -1, 1, 1, 1)
    std = torch.as_tensor(pixel_std, device=latents.device, dtype=torch.float32).view(1, -1, 1, 1, 1)
    current_dtype = vae.post_quant_conv.weight.dtype
    pending = None
    delivered = 0
    for i in range(chunks):
        start = i * t
        length = t + vae.token_overlap
        # Index the full latent sequence without creating padded copy.
        clip_tokens = latents[:, :, start:min(start+length, latents.shape[2])]
        missing = length - clip_tokens.shape[2]
        if missing:
            clip_tokens = torch.cat((clip_tokens, clip_tokens[:, :, -1:].expand(
                -1, -1, missing, -1, -1)), dim=2)
        clip = vae._decode_clip(clip_tokens.to(current_dtype))
        for j in range(int(drop > 0) + 1):
            frames = clip[:, :, j * frame_window:(j + 1) * frame_window]
            frames = frames[:, :, vae.frame_pre_padding:]
            if j == 0:
                if pending is not None:
                    frames = vae._blend(pending, frames, vae.frame_overlap, dim=-3)
                remaining = expected_frames - delivered
                frames = frames[:, :, :max(0, remaining)]
                if frames.shape[2]:
                    rgb = (frames.float() * std + mean).clamp(0, 1)
                    chunk = (rgb[0].permute(1, 2, 3, 0) * 255).round().to(torch.uint8).cpu()
                    delivered += chunk.shape[0]
                    yield chunk
            else:
                pending = frames
        del clip
    if pending is not None and delivered < expected_frames:
        rgb = (pending[:, :, :expected_frames-delivered].float() * std + mean).clamp(0, 1)
        chunk = (rgb[0].permute(1, 2, 3, 0) * 255).round().to(torch.uint8).cpu()
        delivered += chunk.shape[0]
        yield chunk
    if delivered != expected_frames:
        raise RuntimeError("H3 VAE decoded %d frames instead of requested %d" %
                           (delivered, expected_frames))


def verify_h3_mp4(path, *, expected_frames, expected_fps, require_audio=True):
    """Decode the actual published media to reject silently corrupt outputs.

    Uses PyAV's real decoder rather than trusting model output tensor shapes,
    container header frame counts or file size. No video frames accumulate.
    """
    import av
    if expected_frames < 1 or expected_fps < 1:
        raise ValueError("Invalid expected H3 media geometry")
    with av.open(str(path), mode="r") as container:
        video_streams = [stream for stream in container.streams if stream.type == "video"]
        audio_streams = [stream for stream in container.streams if stream.type == "audio"]
        if len(video_streams) != 1 or (require_audio and len(audio_streams) != 1):
            raise RuntimeError("H3 MP4 must contain exactly one video and a stereo audio stream")
        video_stream = video_streams[0]
        rate = video_stream.average_rate
        if rate is None or abs(float(rate) - float(expected_fps)) > .01:
            raise RuntimeError("H3 MP4 video frame rate does not match request")
        actual_frames = 0
        for decoded in container.decode(video=0):
            if decoded.width < 1 or decoded.height < 1:
                raise RuntimeError("H3 MP4 contains invalid video frame geometry")
            actual_frames += 1
            if actual_frames > expected_frames:
                raise RuntimeError("H3 MP4 contains extra video frames")
        if actual_frames != expected_frames:
            raise RuntimeError("H3 MP4 decoded %d frames; expected %d" %
                               (actual_frames, expected_frames))
    audio_samples = 0
    if require_audio:
        with av.open(str(path), mode="r") as container:
            audio_stream = next(x for x in container.streams if x.type == "audio")
            for decoded in container.decode(audio=0):
                audio_samples += int(decoded.samples)
            if audio_samples == 0:
                raise RuntimeError("H3 MP4 contains no decodable audio")
    return {"decodedFrames": actual_frames, "decodedAudioSamples": audio_samples,
            "fps": expected_fps, "audioPresent": bool(audio_samples)}


def encode_h3_latent_video(pipe, video_latents, audio_latents, *,
                           output_path, frames, fps):
    """Decode original H3 latents and stream original soundtrack + RGB chunks."""
    from pathlib import Path
    from diffusers.utils.export_utils import encode_video
    import torch
    if video_latents.ndim != 5 or audio_latents.ndim != 3:
        raise ValueError("Malformed H3 video/audio latent shapes")
    if audio_latents.shape[0] != 2:
        raise ValueError("Expected two H3 stereo audio latent channels")
    device = video_latents.device
    vae = pipe.vae
    avae = pipe.audio_vae
    # Preserve official channel normalization and mono decoder behaviour.
    with torch.inference_mode():
        avmean = torch.as_tensor(avae.config.latents_mean, device=audio_latents.device).view(1, -1, 1)
        avstd = torch.as_tensor(avae.config.latents_std, device=audio_latents.device).view(1, -1, 1)
        audio = avae.decode(audio_latents * avstd + avmean, return_dict=False)[0]
        audio = audio[:, 0, :].float().cpu()
        vmean = torch.as_tensor(vae.config.latents_mean, device=device).view(1, -1, 1, 1, 1)
        vstd = torch.as_tensor(vae.config.latents_std, device=device).view(1, -1, 1, 1, 1)
        video_latents = video_latents * vstd + vmean
        iterator = iter_h3_video_rgb_chunks(
            vae, video_latents, pipe.pixel_mean, pipe.pixel_std, expected_frames=frames)
        output = Path(output_path)
        output.parent.mkdir(parents=True, exist_ok=True)
        temporary = output.with_suffix(".partial.mp4")
        if temporary.exists():
            temporary.unlink()
        try:
            encode_video(iterator, fps=fps, output_path=str(temporary),
                         audio=audio, audio_sample_rate=pipe.audio_sampling_rate)
            if not temporary.is_file() or temporary.stat().st_size < 32:
                raise RuntimeError("H3 native streaming encoder wrote no video")
            temporary.replace(output)
        except BaseException:
            temporary.unlink(missing_ok=True)
            raise
    return frames

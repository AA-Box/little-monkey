"""Native MiniMax H3 rolling AV-latent continuation for Little Monkey Studio.

The model and scheduler are the installed Diffusers/OpenVDN components. No
FreeVideo runtime, ComfyUI nodes, or model weights are bundled. Continuation
uses fixed 22-frame (seven latent-frame) visual history and absolute 40-Hz
audio origins, then writes one synchronized MP4 rather than concatenating
independently encoded AAC streams.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import tempfile

FPS = 24
AUDIO_LATENTS_PER_SECOND = 40
AUDIO_SAMPLE_RATE = 32000
SAMPLES_PER_AUDIO_LATENT = 800
OVERLAP_FRAMES = 22
VISUAL_CONTEXT_LATENTS = 7
MAX_SEGMENTS = 8

if "save_context" not in globals():
    import sys
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from studio_vdn_temporal_store import (
        load_context, save_context, preview_fallback, preview_identity,
        preview_path, load_preview, save_preview,
    )


def audio_origin(frame):
    """Convert absolute 24-fps frame position to nearest 40-Hz audio tick."""
    if not isinstance(frame, int) or frame < 0:
        raise ValueError("Negative or nonintegral AV frame origin")
    return (5 * frame + 1) // 3


def temporal_frame_plan(frames, segments):
    if (type(frames) is not int or not 120 <= frames <= 360
            or (frames - 5) % 17 or type(segments) is not int
            or not 1 <= segments <= MAX_SEGMENTS):
        raise ValueError("Native temporal video needs 17n+5 frames and 1..8 segments")
    return frames + (segments - 1) * (frames - OVERLAP_FRAMES)


def emitted_audio_samples(frame_count, sample_rate=AUDIO_SAMPLE_RATE):
    if frame_count < 0 or sample_rate <= 0:
        raise ValueError("Invalid audio timeline")
    return (frame_count * sample_rate + FPS // 2) // FPS


def temporal_audio_window(previous, generated, desired_samples):
    """Keep the new region; correct <1 latent tick of H3 rounding at the edge."""
    import numpy as np
    if generated.ndim != 2 or generated.shape[0] != 2 or desired_samples < 0:
        raise ValueError("Invalid decoded H3 stereo audio")
    if abs(generated.shape[1] - desired_samples) > SAMPLES_PER_AUDIO_LATENT:
        raise ValueError("H3 audio and video clocks diverged by over one latent tick")
    if generated.shape[1] < desired_samples:
        if generated.shape[1] == 0:
            raise ValueError("No H3 audio to extend")
        generated = np.pad(generated, ((0, 0), (0, desired_samples - generated.shape[1])),
                           mode="edge")
    else:
        generated = generated[:, :desired_samples]
    result = np.ascontiguousarray(generated, dtype=np.float32)
    if previous is not None and previous.shape[1] and result.shape[1]:
        # A short, length-preserving edge repair avoids adding silence or
        # overlapping two audio timelines. No audio segment is discarded.
        fade = min(160, previous.shape[1], result.shape[1])
        midpoint = (previous[:, -1:] + result[:, :1]) / 2
        left = np.linspace(0, 1, fade + 2, dtype=np.float32)[1:-1][None, :]
        right = 1 - left
        previous[:, -fade:] += (midpoint - previous[:, -1:]) * left
        result[:, :fade] += (midpoint - result[:, :1]) * right
    return result


def capture_context(video, audio, source_start_frame, delivered_frame_count):
    """Freeze last seven visual latents and audio aligned to the next window."""
    if (video.ndim != 5 or video.shape[0:2] != (1, 24)
            or video.shape[2] < VISUAL_CONTEXT_LATENTS or
            audio.ndim != 3 or audio.shape[:2] != (2, 32)):
        raise ValueError("Unsupported H3 AV latent geometry")
    next_start_frame = delivered_frame_count - OVERLAP_FRAMES
    start_tick = audio_origin(next_start_frame) - audio_origin(source_start_frame)
    if start_tick < 0 or start_tick >= audio.shape[2]:
        raise ValueError("Previous H3 audio lacks the requested context")
    result = {
        "video": video[:, :, -VISUAL_CONTEXT_LATENTS:].detach().float().cpu().contiguous().clone(),
        "audio": audio[:, :, start_tick:].detach().float().cpu().contiguous().clone(),
        "frame_origin": next_start_frame,
        "audio_origin": audio_origin(next_start_frame),
    }
    if not 1 <= result["audio"].shape[2] <= 64:
        raise ValueError("H3 audio context is not a bounded overlap")
    return result


class NativeTemporalPrefix:
    """Protect AV prefix rows and their clean clocks inside the actual H3 loop."""

    def __init__(self, transformer, data, device):
        import torch
        from diffusers.modular_pipelines.minimax_h3.before_denoise import patchify_video_latents
        self.video = data["video"].to(device=device, dtype=torch.float32)
        self.audio = data["audio"].to(device=device, dtype=torch.float32)
        self.video_rows = patchify_video_latents(
            self.video, tuple(transformer.config.patch_size))
        self.audio_rows = self.audio.permute(0, 2, 1).reshape(-1, 32).contiguous()
        self.video_start = self.audio_start = self.channel_stride = None
        self.current_video = None
        digest = hashlib.sha256()
        for tensor in (data["video"], data["audio"]):
            digest.update(tensor.detach().float().cpu().contiguous().numpy().tobytes())
        digest.update(str(data["frame_origin"]).encode("ascii"))
        digest.update(str(data["audio_origin"]).encode("ascii"))
        self.digest = digest.hexdigest()

    def bind(self, block, video_start, audio_start):
        if (video_start < 0 or audio_start < 0
                or block.latents.shape[0] <= video_start + self.video_rows.shape[0]
                or (block.audio_latents.shape[0] - audio_start) % 2):
            raise ValueError("Invalid native temporal conditioning row offsets")
        self.video_start, self.audio_start = video_start, audio_start
        self.channel_stride = (block.audio_latents.shape[0] - audio_start) // 2
        if self.channel_stride <= self.audio.shape[2]:
            raise ValueError("H3 audio continuation has no new rows to generate")
        self.restore(block)

    def _audio_positions(self, device):
        import torch
        offsets = torch.arange(self.audio.shape[2], device=device)
        return torch.cat([offsets + self.audio_start,
                          offsets + self.audio_start + self.channel_stride])

    def restore(self, block):
        """Do not allow Euler's in-place update to denoise protected prefixes."""
        if self.video_start is None:
            return
        v = self.video_start
        a = self.audio_start
        t = self.audio.shape[2]
        block.latents[v:v + self.video_rows.shape[0]] = self.video_rows.to(
            device=block.latents.device, dtype=block.latents.dtype)
        values = self.audio_rows.to(device=block.audio_latents.device,
                                    dtype=block.audio_latents.dtype)
        block.audio_latents[a:a+t] = values[:t]
        block.audio_latents[a+self.channel_stride:a+self.channel_stride+t] = values[t:]
        self.restore_model_rows()

    def before(self, model, args, kwargs):
        import torch
        video, audio = kwargs["hidden_states"][0], kwargs["audio_hidden_states"][0]
        if self.video_start is None:
            raise ValueError("Temporal context was not attached to H3 before sampling")
        end = self.video_start + self.video_rows.shape[0]
        if video.shape[0] <= end or audio.shape[0] < self.audio_start + 2 * self.channel_stride:
            raise ValueError("H3 layout is incompatible with native temporal overlap")
        self.current_video = video
        self.restore_model_rows()
        audio_positions = self._audio_positions(audio.device)
        audio[audio_positions] = self.audio_rows.to(device=audio.device, dtype=audio.dtype)
        row_times = kwargs["timestep"][kwargs["timestep_indices"]].clone()
        row_times[kwargs["video_indices"][self.video_start:end]] = 1.0
        row_times[kwargs["audio_indices"][audio_positions]] = 1.0
        kwargs["timestep"], kwargs["timestep_indices"] = torch.unique(
            row_times, sorted=True, return_inverse=True)
        return args, kwargs

    def restore_model_rows(self):
        if self.current_video is not None:
            v = self.video_start
            self.current_video[v:v + self.video_rows.shape[0]] = self.video_rows.to(
                device=self.current_video.device, dtype=self.current_video.dtype)

    def finish(self, video_latents, audio_latents):
        t = self.audio.shape[2]
        video_latents[:, :, :VISUAL_CONTEXT_LATENTS] = self.video.to(
            device=video_latents.device, dtype=video_latents.dtype)
        audio_latents[:, :, :t] = self.audio.to(
            device=audio_latents.device, dtype=audio_latents.dtype)


def render_temporal_video(pipe, args, torch, device, keyframes, references,
                          checkpoint, denoiser, encode_video, native_sampler_recovery,
                          clear_native_sampler_stage, request_identity):
    """Sample bounded rolling windows with fixed AV context and one final mux."""
    import numpy as np
    stored = (load_context(args.temporal_context_in,
                           args.temporal_source_video_sha256, checkpoint,
                           args.temporal_source_job_id)
              if args.temporal_context_in else None)
    source_frames = stored["total_frames"] if stored else 0
    frames_total = temporal_frame_plan(args.frames, args.segments)
    if stored is not None:
        frames_total -= OVERLAP_FRAMES
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    cursor = 0
    audio_cursor = 0
    sample_rate = None
    video_file = None
    video_memory = None
    context = stored["high"] if stored else None
    low_context = stored["low"] if stored else None
    continuation_anchor = keyframes.get("image") if stored else None
    audio_chunks = []
    with tempfile.TemporaryDirectory(prefix=".vdn-temporal-", dir=output.parent) as work:
        workdir = Path(work)
        for part in range(args.segments):
            source_frame = 0 if context is None else source_frames + cursor - OVERLAP_FRAMES
            prefix = NativeTemporalPrefix(denoiser, context, device) if context else None
            stage = "temporal_%d" % part
            was_continuing = context is not None
            print(json.dumps({"phase": "temporal_sampling", "part": part + 1,
                              "parts": args.segments, "overlapFrames": 22 if was_continuing else 0}),
                  flush=True)
            # FL2VA still requires its explicit first-image input on every
            # call. For subsequent windows it must agree with the protected
            # latent prefix, never reset to the original opening image.
            window_keyframes = (keyframes if not was_continuing else
                                {"image": continuation_anchor} if keyframes and continuation_anchor is not None else {})
            if args.two_pass:
                import torch.nn.functional as functional
                low_prefix = (NativeTemporalPrefix(denoiser, low_context, device)
                              if low_context else None)
                preview_fingerprint = preview_identity(
                    request_identity, part,
                    low_prefix.digest if low_prefix else None)
                source_path = preview_path(
                    args.refine_from or args.checkpoint, part)
                resumed = False
                if Path(source_path).is_file() or Path(source_path).is_symlink():
                    try:
                        preview_latents, preview_audio = load_preview(
                            source_path, preview_fingerprint)
                        resumed = True
                        print(json.dumps({"phase": "temporal_preview_reused",
                                          "segment": part + 1}), flush=True)
                    except (OSError, ValueError):
                        if args.refine_from:
                            raise
                        # Completed earlier segments may be regenerated on restart.
                        # Their new tensors invalidate a later local preview,
                        # which must be sampled again instead of aborting the job.
                        Path(source_path).unlink(missing_ok=True)
                if args.refine_from and not resumed:
                    raise RuntimeError("Missing compatible saved temporal preview segment")
                if not resumed:
                    with native_sampler_recovery(
                            args, stage + "_preview", pipe, model_identity=checkpoint,
                            temporal_context=low_prefix,
                            temporal_transformer=denoiser):
                        preview = pipe(
                            prompt=args.prompt, num_frames=args.frames,
                            height=384, width=672,
                            num_inference_steps=args.steps + 1,
                            generator=torch.Generator(device).manual_seed(args.seed + 3 * part),
                            output=["latents", "audio_latents"],
                            **({"references": references} if references else window_keyframes))
                    preview_latents, preview_audio = preview["latents"], preview["audio_latents"]
                    if low_prefix:
                        low_prefix.finish(preview_latents, preview_audio)
                    save_preview(source_path, preview_latents, preview_audio, preview_fingerprint)
                    clear_native_sampler_stage(args, stage + "_preview")
                original_low_video = preview_latents.detach().float().cpu().contiguous().clone()
                original_low_audio = preview_audio.detach().float().cpu().contiguous().clone()
                preview_latents = preview_latents.to(device=device, dtype=torch.float32)
                preview_audio = preview_audio.to(device=device, dtype=torch.float32)
                upscaled = functional.interpolate(
                    preview_latents, size=(preview_latents.shape[2], 48, 84),
                    mode="trilinear", align_corners=False)
                generator = torch.Generator(device).manual_seed(args.seed + 3 * part + 1)
                # Reintroduce noise for the new frames while preserving the
                # protected AV prefix throughout H3 denoising.
                upscaled = .90 * upscaled + .10 * torch.randn(
                    upscaled.shape, device=device, dtype=upscaled.dtype, generator=generator)
                with native_sampler_recovery(
                        args, stage + "_refine", pipe, model_identity=checkpoint,
                        temporal_context=prefix, temporal_transformer=denoiser):
                    sample = pipe(
                        prompt=args.prompt, num_frames=args.frames,
                        height=768, width=1344, num_inference_steps=args.refine_steps + 1,
                        generator=torch.Generator(device).manual_seed(args.seed + 3 * part + 2),
                        latents=upscaled, audio_latents=preview_audio,
                        output=["videos", "audio", "sampling_rate", "latents", "audio_latents"],
                        **({"references": references} if references else window_keyframes))
            else:
                with native_sampler_recovery(
                        args, stage, pipe, model_identity=checkpoint,
                        temporal_context=prefix, temporal_transformer=denoiser):
                    sample = pipe(
                        prompt=args.prompt, num_frames=args.frames,
                        num_inference_steps=args.steps + 1,
                        generator=torch.Generator(device).manual_seed(args.seed + part),
                        output=["videos", "audio", "sampling_rate", "latents", "audio_latents"],
                        **({"references": references} if references else window_keyframes))
            if prefix:
                prefix.finish(sample["latents"], sample["audio_latents"])
            if int(sample["sampling_rate"]) != AUDIO_SAMPLE_RATE:
                raise RuntimeError("Native H3 continuation needs 32-kHz generated audio")
            if sample_rate is None:
                sample_rate = int(sample["sampling_rate"])
            video_frames = sample["videos"][0]
            if len(video_frames) != args.frames:
                raise RuntimeError("H3 emitted an unexpected temporal segment length")
            overlap = OVERLAP_FRAMES if was_continuing else 0
            length = len(video_frames) - overlap
            if video_memory is None:
                first = np.asarray(video_frames[0])
                if (first.ndim != 3 or first.shape[2] != 3
                        or first.dtype != np.uint8 or
                        first.shape[0] % 32 or first.shape[1] % 32):
                    raise RuntimeError("Unexpected H3 RGB video canvas")
                video_file = workdir / "frames.raw"
                required = frames_total * first.size
                if shutil.disk_usage(workdir).free < required + 512 * 1024 * 1024:
                    raise RuntimeError("Insufficient disk space for native temporal video assembly")
                video_memory = np.memmap(str(video_file), dtype=np.uint8, mode="w+",
                                         shape=(frames_total, *first.shape))
            for local in range(overlap, len(video_frames)):
                frame = np.asarray(video_frames[local])
                if frame.shape != video_memory.shape[1:] or frame.dtype != np.uint8:
                    raise RuntimeError("Temporal video resolution changed between segments")
                video_memory[cursor + local - overlap] = frame
            decoded_audio = sample["audio"][0].detach().float().cpu().numpy()
            if decoded_audio.ndim != 2 or decoded_audio.shape[0] != 2:
                raise RuntimeError("H3 output lost its stereo soundtrack")
            # Trimming is in H3 audio-latent ticks, not frame-count guesses.
            audio_trim = (context["audio"].shape[2] * SAMPLES_PER_AUDIO_LATENT
                          if was_continuing else 0)
            next_cursor = cursor + length
            wanted = emitted_audio_samples(next_cursor, AUDIO_SAMPLE_RATE) - audio_cursor
            audio_slice = decoded_audio[:, audio_trim:]
            audio_slice = temporal_audio_window(
                audio_chunks[-1] if audio_chunks else None, audio_slice, wanted)
            audio_chunks.append(audio_slice)
            audio_cursor += audio_slice.shape[1]
            cursor = next_cursor
            context = capture_context(sample["latents"], sample["audio_latents"],
                                      source_frame, source_frames + cursor)
            if args.two_pass:
                low_context = capture_context(original_low_video, original_low_audio,
                                              source_frame, source_frames + cursor)
            else:
                low_context = preview_fallback(context)
            continuation_anchor = video_frames[-OVERLAP_FRAMES]
            clear_native_sampler_stage(args, stage + "_refine" if args.two_pass else stage)
            print(json.dumps({"phase": "temporal_part_completed", "part": part + 1,
                              "framesDelivered": cursor, "audioSamples": audio_cursor}),
                  flush=True)
            del sample
        if cursor != frames_total or audio_cursor != emitted_audio_samples(frames_total):
            raise RuntimeError("Native temporal A/V assembly did not reach its exact duration")
        video_memory.flush()
        full_audio = np.concatenate(audio_chunks, axis=1)
        target = output.with_suffix(".partial.mp4")
        target.unlink(missing_ok=True)
        print(json.dumps({"phase": "temporal_encoding", "frames": frames_total,
                          "audioSamples": audio_cursor}), flush=True)
        encode_video(torch.from_numpy(video_memory), fps=FPS, output_path=str(target),
                     audio=torch.from_numpy(full_audio), audio_sample_rate=AUDIO_SAMPLE_RATE)
        if not target.is_file() or target.stat().st_size < 32:
            raise RuntimeError("Native temporal encoder produced no video")
        os.replace(target, output)
        if stored is not None:
            # Publish a complete chain, not a second clip that merely depends
            # on an unpublished predecessor. Decode/re-mux from PCM; never
            # concatenate AAC bitstreams or duplicate 22 overlapped frames.
            from_path = Path(args.temporal_parent)
            merged = output.with_name("." + output.stem + ".joined.mp4")
            merged.unlink(missing_ok=True)
            try:
                print(json.dumps({"phase": "temporal_chain_join",
                                  "parentFrames": source_frames,
                                  "newFrames": frames_total}), flush=True)
                result = join_native_chain(
                    str(from_path), str(output), str(merged),
                    parent_frames=source_frames, child_frames=frames_total,
                    expected_parent_sha256=args.temporal_source_video_sha256)
                if result["frames"] != source_frames + frames_total:
                    raise RuntimeError("Native full chain frame total does not match context")
                os.replace(merged, output)
            finally:
                merged.unlink(missing_ok=True)
        if args.temporal_context_out:
            save_context(args.temporal_context_out, context, low_context,
                         output, checkpoint, args.job_id, source_frames + frames_total)
        del video_memory
    total_frames = source_frames + frames_total
    return {"videos": total_frames,
            "audioSamples": emitted_audio_samples(total_frames), "newFrames": frames_total}

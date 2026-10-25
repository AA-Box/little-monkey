"""Native AV latent continuation snapshots and per-segment preview checkpoints.

Only bounded torch.float32 tensors in safetensors; no pickle, remote code,
implicit Gallery fallback, or reuse across differing video hashes.
"""
import hashlib
import os
from pathlib import Path
import uuid

SCHEMA = "little-monkey-h3-av-tail-v1"
PREVIEW = "little-monkey-h3-window-preview-v1"
MAX_BYTES = 64 * 1024 * 1024
OVERLAP_FRAMES = 22
LATENT_FRAMES = 7


def audio_origin(frame):
    if type(frame) is not int or frame < 0:
        raise ValueError("Invalid global AV origin")
    return (5 * frame + 1) // 3


def digest(tensors):
    h = hashlib.sha256()
    for key in sorted(tensors):
        h.update(key.encode("ascii"))
        h.update(tensors[key].detach().float().cpu().contiguous().numpy().tobytes())
    return h.hexdigest()


def _save(path, tensors, metadata):
    from safetensors.torch import save_file
    import torch
    p = Path(path)
    if not p.is_absolute() or p.is_symlink():
        raise ValueError("Unsafe native H3 checkpoint destination")
    p.parent.mkdir(parents=True, exist_ok=True)
    if p.parent.is_symlink():
        raise ValueError("Unsafe native H3 checkpoint directory")
    # Safetensors rejects distinct keys that share storage (single-pass low/high
    # audio tails may intentionally be the same tensor). Give each key its own
    # immutable, contiguous allocation before the atomic write.
    safe = {k: v.detach().float().cpu().contiguous().clone() for k, v in tensors.items()}
    if any(not torch.isfinite(v).all() for v in safe.values()):
        raise ValueError("Nonfinite AV latent checkpoint")
    temp = p.parent / ("." + p.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        save_file(safe, str(temp), metadata=dict(metadata, payload_sha256=digest(safe)))
        if temp.stat().st_size > MAX_BYTES:
            raise ValueError("H3 temporal snapshot exceeds 64 MiB")
        with temp.open("rb") as handle:
            os.fsync(handle.fileno())
        os.replace(temp, p)
        if os.name == "posix":
            descriptor = os.open(p.parent, os.O_RDONLY)
            try:
                os.fsync(descriptor)
            finally:
                os.close(descriptor)
    finally:
        temp.unlink(missing_ok=True)


def _read(path, schema):
    from safetensors import safe_open, SafetensorError
    p = Path(path)
    if (not p.is_absolute() or p.is_symlink() or not p.is_file() or
            not 128 <= p.stat().st_size <= MAX_BYTES):
        raise ValueError("Unsafe or absent native H3 AV snapshot")
    try:
        with safe_open(str(p), framework="pt", device="cpu") as handle:
            metadata = handle.metadata() or {}
            if metadata.get("schema") != schema:
                raise ValueError("Invalid native H3 AV snapshot schema")
            values = {key: handle.get_tensor(key) for key in handle.keys()}
    except (SafetensorError, RuntimeError) as error:
        raise ValueError("Corrupt native H3 Safetensors snapshot") from error
    if (any(str(v.dtype) != "torch.float32" for v in values.values()) or
            metadata.get("payload_sha256") != digest(values)):
        raise ValueError("Corrupt native H3 snapshot payload")
    return values, metadata


def validate_context(high, low, total_frames):
    import torch
    if type(total_frames) is not int or total_frames <= OVERLAP_FRAMES:
        raise ValueError("Invalid native H3 source duration")
    for context, dims in ((high, (48, 84)), (low, (24, 42))):
        video, audio = context["video"], context["audio"]
        if (tuple(video.shape) != (1, 24, LATENT_FRAMES, *dims) or
                audio.ndim != 3 or tuple(audio.shape[:2]) != (2, 32) or
                not 1 <= audio.shape[2] <= 64 or
                context["frame_origin"] != total_frames - OVERLAP_FRAMES or
                context["audio_origin"] != audio_origin(context["frame_origin"])):
            raise ValueError("H3 context is not aligned to the source's final 22 frames")
        if not torch.isfinite(video).all() or not torch.isfinite(audio).all():
            raise ValueError("Nonfinite context source")


def preview_fallback(high):
    import torch.nn.functional as functional
    low = dict(high)
    low["video"] = functional.interpolate(
        high["video"], size=(7, 24, 42), mode="trilinear", align_corners=False)
    return low


def save_context(path, high, low, video_path, model, job_id, frames):
    validate_context(high, low, frames)
    with open(video_path, "rb") as source:
        sha = hashlib.sha256()
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            sha.update(chunk)
    _save(path, {
        "high_video": high["video"], "high_audio": high["audio"],
        "low_video": low["video"], "low_audio": low["audio"],
    }, {
        "schema": SCHEMA,
        "video_sha256": sha.hexdigest(),
        "model": model,
        "job_id": job_id,
        "total_frames": str(frames),
        "frame_origin": str(high["frame_origin"]),
        "audio_origin": str(high["audio_origin"]),
    })


def load_context(path, video_sha256, model, job_id):
    values, meta = _read(path, SCHEMA)
    if (len(video_sha256) != 64 or
            meta.get("video_sha256") != video_sha256 or
            meta.get("model") != model or
            meta.get("job_id") != job_id or
            set(values) != {"high_video", "high_audio", "low_video", "low_audio"}):
        raise ValueError("Native AV latent source does not match its Gallery artifact")
    try:
        frames = int(meta["total_frames"])
        frame_origin = int(meta["frame_origin"])
        tick = int(meta["audio_origin"])
    except (ValueError, KeyError, TypeError) as exc:
        raise ValueError("Invalid AV context timeline") from exc
    high = dict(video=values["high_video"], audio=values["high_audio"],
                frame_origin=frame_origin, audio_origin=tick)
    low = dict(video=values["low_video"], audio=values["low_audio"],
               frame_origin=frame_origin, audio_origin=tick)
    validate_context(high, low, frames)
    return dict(high=high, low=low, total_frames=frames)


def preview_path(checkpoint, segment):
    if type(segment) is not int or not 0 <= segment < 8:
        raise ValueError("Invalid preview segment")
    return str(checkpoint) + ".segment%d.safetensors" % segment


def preview_identity(request_digest, segment, prefix_digest):
    return hashlib.sha256(
        ("%s:%d:%s" % (request_digest, segment, prefix_digest or "none")).encode("ascii")
    ).hexdigest()


def save_preview(path, video, audio, identity):
    if (video.ndim != 5 or tuple(video.shape[:2]) != (1, 24) or
            tuple(video.shape[-2:]) != (24, 42) or
            audio.ndim != 3 or tuple(audio.shape[:2]) != (2, 32)):
        raise ValueError("Invalid native first-pass latent geometry")
    _save(path, {"video": video, "audio": audio},
          {"schema": PREVIEW, "identity": identity})


def load_preview(path, identity):
    values, meta = _read(path, PREVIEW)
    if meta.get("identity") != identity or set(values) != {"video", "audio"}:
        raise ValueError("First-pass latent context does not match this render")
    video, audio = values["video"], values["audio"]
    if (video.ndim != 5 or tuple(video.shape[:2]) != (1, 24)
            or tuple(video.shape[-2:]) != (24, 42)
            or audio.ndim != 3 or tuple(audio.shape[:2]) != (2, 32)):
        raise ValueError("Invalid saved preview tensor shapes")
    return video, audio

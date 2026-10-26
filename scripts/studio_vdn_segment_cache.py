"""Crash-safe native H3 temporal segment cache.

A finished window retains its *exact decoded* RGB8/float32 PCM bytes and its
high/low H3 AV latent tails. No pickle or training cache formats; each resume
validates every byte, predecessor digest, model identity and frame timeline.
The state Safetensors file is the atomic commit marker, written LAST.
"""
import hashlib
import os
from pathlib import Path
import uuid

MAX_SEGMENTS = 8
MAX_FRAMES = 360
MAX_CANVAS_PIXELS = 1344 * 768
MAX_AUDIO_SAMPLES = 600000
SCHEMA = "little-monkey-vdn-segment-cache-v1"

# During native execution all sources are embedded into one standalone worker;
# only direct CPU unit-test imports require an on-disk module lookup.
if "validate_context" not in globals() or "_save" not in globals() or "_read" not in globals():
    import sys
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from studio_vdn_temporal_store import validate_context, _save, _read


def _sha(path):
    value = hashlib.sha256()
    with open(path, "rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def _paths(root, part):
    root = Path(root)
    if (not root.is_absolute() or root.is_symlink()
            or type(part) is not int or not 0 <= part < MAX_SEGMENTS):
        raise ValueError("Unsafe native H3 temporal cache path")
    root.mkdir(parents=True, exist_ok=True)
    if root.is_symlink():
        raise ValueError("Native H3 cache root was replaced by a symlink")
    base = root / ("part-%d" % part)
    return base.with_suffix(".rgb"), base.with_suffix(".pcm"), base.with_suffix(".safetensors")


def _write_atomic(destination, raw):
    import numpy as np
    partial = destination.parent / ("." + destination.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with partial.open("xb") as out:
            raw.tofile(out)
            out.flush()
            os.fsync(out.fileno())
        os.replace(partial, destination)
    finally:
        partial.unlink(missing_ok=True)


def save_segment(root, part, frames, audio, high, low, *, model, request_hash,
                 predecessor_hash, total_frames):
    import numpy as np
    video_path, audio_path, state_path = _paths(root, part)
    if any(path.is_symlink() for path in (video_path, audio_path, state_path)):
        raise ValueError("Symlink in native segment cache")
    if (frames.ndim != 4 or frames.dtype != np.uint8 or
            not 1 <= frames.shape[0] <= MAX_FRAMES or
            frames.shape[1] * frames.shape[2] > MAX_CANVAS_PIXELS or
            frames.shape[3] != 3 or
            frames.shape[1] % 32 or frames.shape[2] % 32):
        raise ValueError("Invalid native segment RGB canvas")
    if (audio.ndim != 2 or audio.shape[0] != 2 or
            not 1 <= audio.shape[1] <= MAX_AUDIO_SAMPLES):
        raise ValueError("Invalid native segment PCM")
    validate_context(high, low, total_frames)
    rgb = np.ascontiguousarray(frames)
    pcm = np.ascontiguousarray(audio, dtype=np.float32)
    required = rgb.nbytes + pcm.nbytes
    import shutil
    if shutil.disk_usage(video_path.parent).free < required + 128 * 1024 * 1024:
        raise ValueError("Insufficient disk for a recoverable native segment")
    # Invalidate a stale commit marker before touching either data file.
    state_path.unlink(missing_ok=True)
    _write_atomic(video_path, rgb)
    _write_atomic(audio_path, pcm)
    _save(state_path, {
        "high_video": high["video"], "high_audio": high["audio"],
        "low_video": low["video"], "low_audio": low["audio"],
    }, {
        "schema": SCHEMA,
        "model": model,
        "request_hash": request_hash,
        "predecessor_hash": predecessor_hash,
        "total_frames": str(total_frames),
        "frame_origin": str(high["frame_origin"]),
        "audio_origin": str(high["audio_origin"]),
        "video_shape": ",".join(str(n) for n in frames.shape),
        "audio_samples": str(pcm.shape[1]),
        "video_sha256": _sha(video_path),
        "audio_sha256": _sha(audio_path),
    })


def load_segment(root, part, *, model, request_hash, predecessor_hash,
                 expected_frames, expected_total_frames):
    """Return disk-backed RGB and bounded PCM only on a complete match."""
    import numpy as np
    rgb_path, pcm_path, state_path = _paths(root, part)
    if any(path.is_symlink() for path in (rgb_path, pcm_path, state_path)):
        raise ValueError("Symlink in native segment cache")
    values, meta = _read(state_path, SCHEMA)
    if (meta.get("model") != model or
            meta.get("request_hash") != request_hash or
            meta.get("predecessor_hash") != predecessor_hash or
            meta.get("total_frames") != str(expected_total_frames) or
            set(values) != {"high_video", "high_audio", "low_video", "low_audio"}):
        raise ValueError("Native segment was generated for different inputs/context")
    try:
        shape = tuple(int(x) for x in meta["video_shape"].split(","))
        samples = int(meta["audio_samples"])
        frame_origin = int(meta["frame_origin"])
        audio_origin = int(meta["audio_origin"])
    except (ValueError, KeyError) as error:
        raise ValueError("Invalid native segment cache layout") from error
    if (len(shape) != 4 or shape[0] != expected_frames or
            not 1 <= shape[0] <= MAX_FRAMES or
            shape[1] <= 0 or shape[2] <= 0 or
            shape[1] % 32 or shape[2] % 32 or
            shape[1] * shape[2] > MAX_CANVAS_PIXELS or
            shape[3] != 3 or
            not 1 <= samples <= MAX_AUDIO_SAMPLES):
        raise ValueError("Invalid native segment cache dimensions")
    if (not rgb_path.is_file() or not pcm_path.is_file() or
            rgb_path.stat().st_size != int(np.prod(shape)) or
            pcm_path.stat().st_size != 2 * samples * 4 or
            _sha(rgb_path) != meta.get("video_sha256") or
            _sha(pcm_path) != meta.get("audio_sha256")):
        raise ValueError("Native segment payload changed since it was committed")
    high = dict(video=values["high_video"], audio=values["high_audio"],
                frame_origin=frame_origin, audio_origin=audio_origin)
    low = dict(video=values["low_video"], audio=values["low_audio"],
               frame_origin=frame_origin, audio_origin=audio_origin)
    validate_context(high, low, expected_total_frames)
    video = np.memmap(rgb_path, dtype=np.uint8, mode="r", shape=shape)
    pcm = np.memmap(pcm_path, dtype=np.float32, mode="r", shape=(2, samples))
    return {"video": video, "audio": pcm, "high": high, "low": low}


def discard_segment(root, part):
    paths = _paths(root, part)
    for destination in (paths[2], paths[0], paths[1]):
        if destination.is_symlink():
            raise ValueError("Symlink in native segment cache")
        destination.unlink(missing_ok=True)

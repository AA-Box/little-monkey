"""Native streaming long-video MP4 chain assembly (no FreeVideo dependency).

Decode each source, reconstruct a single continuous 24-fps video timeline,
and re-encode a *single* 32-kHz stereo soundtrack. Reject unsupported sources
instead of silently concatenating compressed AAC packets or duplicating frames.
"""
from fractions import Fraction
import hashlib
import os
from pathlib import Path
import uuid

FPS = 24
RATE = 32000
EDGE_FADE = 320
MAX_PARENT_BYTES = 256 * 1024 * 1024
MAX_FRAMES = 10000


def samples_for_frames(frames):
    if type(frames) is not int or frames < 0:
        raise ValueError("Invalid chain frame count")
    return (frames * RATE + FPS // 2) // FPS


def _validate(source, hash_sha256=None):
    p = Path(source)
    if not p.is_absolute() or p.is_symlink() or not p.is_file():
        raise ValueError("Missing or unsafe native chain video")
    if not 64 <= p.stat().st_size <= MAX_PARENT_BYTES:
        raise ValueError("Native chain video is absent or exceeds Gallery size")
    if hash_sha256 is not None:
        if len(hash_sha256) != 64 or any(x not in "0123456789abcdef" for x in hash_sha256):
            raise ValueError("Invalid original chain video digest")
        digest = hashlib.sha256()
        with p.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
        if digest.hexdigest() != hash_sha256:
            raise ValueError("Native chain video changed after source pinning")
    return p


def _get_audio(container, expected):
    """Resample AAC to actual 32-kHz stereo PCM, accounting for codec delay."""
    import av
    import numpy as np
    if not container.streams.audio:
        raise ValueError("A native temporal chain source lost its soundtrack")
    resampler = av.AudioResampler(format="fltp", layout="stereo", rate=RATE)
    parts = []
    for decoded in container.decode(audio=0):
        for frame in resampler.resample(decoded):
            data = frame.to_ndarray()
            if data.shape[0] != 2:
                raise ValueError("Native temporal source must decode to stereo")
            parts.append(np.asarray(data, dtype=np.float32))
    for frame in resampler.resample(None):
        data = frame.to_ndarray()
        if data.shape[0] != 2:
            raise ValueError("Native temporal source lost stereo during resampling")
        parts.append(np.asarray(data, dtype=np.float32))
    samples = np.concatenate(parts, axis=1) if parts else np.empty((2, 0), dtype=np.float32)
    # AAC encoders commonly pad at the end. Accept at most one AAC packet of
    # rounding per clip; anything larger indicates a desynchronized source.
    if abs(samples.shape[1] - expected) > 2048:
        raise ValueError("Native source audio/video clock drift exceeds one AAC packet")
    if samples.shape[1] > expected:
        samples = samples[:, :expected]
    elif samples.shape[1] < expected:
        if samples.shape[1] == 0:
            raise ValueError("Native chain source has no audio")
        samples = np.pad(samples, ((0, 0), (0, expected - samples.shape[1])), mode="edge")
    return np.ascontiguousarray(samples)


def _smooth_seam(left, right):
    """Repair 10-ms PCM boundary without dropping or duplicating samples."""
    import numpy as np
    n = min(EDGE_FADE, left.shape[1], right.shape[1])
    if n:
        target = (left[:, -1:] + right[:, :1]) * .5
        phase = np.linspace(0, 1, n + 2, dtype=np.float32)[1:-1][None, :]
        left[:, -n:] += (target - left[:, -n:]) * phase
        right[:, :n] += (target - right[:, :n]) * (1 - phase)
    return left, right


def join_native_chain(parent, newly_generated, destination, *, parent_frames,
                      child_frames, expected_parent_sha256=None):
    """Create an atomically published, continuously muxed parent+child video.

    The child MP4 contains ONLY the newly generated frames; the parent is
    already the whole prior chain. Only one extra encode occurs at this boundary.
    """
    import av
    import numpy as np
    parent = _validate(parent, expected_parent_sha256)
    child = _validate(newly_generated)
    dest = Path(destination)
    if not dest.is_absolute() or dest.is_symlink() or dest.parent.is_symlink():
        raise ValueError("Unsafe native chain output")
    if (type(parent_frames) is not int or type(child_frames) is not int or
            not 1 <= parent_frames <= MAX_FRAMES or
            not 1 <= child_frames <= MAX_FRAMES or
            parent_frames + child_frames > MAX_FRAMES):
        raise ValueError("Native chain exceeds safe frame count")
    if parent.resolve() == child.resolve() or dest.resolve() in (parent.resolve(), child.resolve()):
        raise ValueError("Native chain output must not overwrite its sources")

    expected_parent = samples_for_frames(parent_frames)
    expected_total = samples_for_frames(parent_frames + child_frames)
    expected_child = expected_total - expected_parent
    with av.open(str(parent), "r") as container:
        parent_pcm = _get_audio(container, expected_parent)
    with av.open(str(child), "r") as container:
        child_pcm = _get_audio(container, expected_child)
    parent_pcm, child_pcm = _smooth_seam(parent_pcm, child_pcm)
    combined = np.concatenate((parent_pcm, child_pcm), axis=1)
    if combined.shape != (2, expected_total):
        raise RuntimeError("Native joined soundtrack duration is not frame-exact")

    dest.parent.mkdir(parents=True, exist_ok=True)
    temporary = dest.parent / ("." + dest.name + "." + uuid.uuid4().hex + ".mp4")
    try:
        with av.open(str(parent), "r") as one, av.open(str(child), "r") as two, \
                av.open(str(temporary), "w", format="mp4") as output:
            source_video = one.streams.video[0] if one.streams.video else None
            newer_video = two.streams.video[0] if two.streams.video else None
            if source_video is None or newer_video is None:
                raise ValueError("Native temporal source is missing the video stream")
            if (source_video.width != newer_video.width or
                    source_video.height != newer_video.height or
                    source_video.width % 2 or source_video.height % 2):
                raise ValueError("The continuation changes native chain canvas")
            target_video = output.add_stream("libx264", rate=FPS)
            target_video.width = source_video.width
            target_video.height = source_video.height
            target_video.pix_fmt = "yuv420p"
            target_video.options = {"preset": "veryfast", "crf": "19"}
            target_audio = output.add_stream("aac", rate=RATE)
            target_audio.layout = "stereo"
            target_audio.bit_rate = 192000
            index = 0
            for container, expected in ((one, parent_frames), (two, child_frames)):
                count = 0
                for frame in container.decode(video=0):
                    rgb = frame.to_ndarray(format="rgb24")
                    encoded = av.VideoFrame.from_ndarray(rgb, format="rgb24")
                    encoded.pts = index
                    encoded.time_base = Fraction(1, FPS)
                    for packet in target_video.encode(encoded):
                        output.mux(packet)
                    index += 1
                    count += 1
                    if count > expected:
                        raise ValueError("Native chain contains extra decoded video frames")
                if count != expected:
                    raise ValueError("Native chain video is incomplete")
            for packet in target_video.encode(None):
                output.mux(packet)

            cursor = 0
            for start in range(0, combined.shape[1], 1024):
                values = np.ascontiguousarray(combined[:, start:start + 1024])
                frame = av.AudioFrame.from_ndarray(values, format="fltp", layout="stereo")
                frame.sample_rate = RATE
                frame.pts = cursor
                frame.time_base = Fraction(1, RATE)
                for packet in target_audio.encode(frame):
                    output.mux(packet)
                cursor += values.shape[1]
            for packet in target_audio.encode(None):
                output.mux(packet)
        if not temporary.is_file() or temporary.stat().st_size < 64:
            raise RuntimeError("Native joined MP4 is empty")
        with temporary.open("rb") as written:
            os.fsync(written.fileno())
        os.replace(temporary, dest)
        return {"frames": parent_frames + child_frames, "audioSamples": expected_total,
                "seamsRepaired": 1}
    finally:
        temporary.unlink(missing_ok=True)

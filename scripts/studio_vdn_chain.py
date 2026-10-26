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


def _iter_audio(container, expected):
    """Bounded resampling, exact presentation duration; never hold whole PCM."""
    import av
    import numpy as np
    if not container.streams.audio:
        raise ValueError("Native temporal source has no soundtrack")
    if not 0 <= expected <= samples_for_frames(MAX_FRAMES):
        raise ValueError("Unsafe target soundtrack duration")
    resampler = av.AudioResampler(format="fltp", layout="stereo", rate=RATE)
    produced = 0
    last = None

    def emit(frame):
        nonlocal produced, last
        audio = np.asarray(frame.to_ndarray(), dtype=np.float32)
        if audio.ndim != 2 or audio.shape[0] != 2 or not np.isfinite(audio).all():
            raise ValueError("Native chain lost valid stereo audio")
        size = audio.shape[1]
        until = max(0, min(size, expected - produced))
        produced += size
        if until:
            part = np.ascontiguousarray(audio[:, :until])
            last = part[:, -1:].copy()
            return part
        return None

    for decoded in container.decode(audio=0):
        for frame in resampler.resample(decoded):
            chunk = emit(frame)
            if chunk is not None:
                yield chunk
    for frame in resampler.resample(None):
        chunk = emit(frame)
        if chunk is not None:
            yield chunk
    # Reject genuine desynchronization; only limited encoder priming or final
    # AAC padding may be trimmed/padded to frame-perfect clock.
    if abs(produced - expected) > 2048:
        raise ValueError("Native source audio/video clock drift exceeds AAC padding")
    if produced < expected:
        if last is None:
            raise ValueError("Native chain source has no audio")
        yield np.repeat(last, expected - produced, axis=1)


def _get_audio(container, expected):
    """Small-file compatibility helper for tests; production uses _iter_audio."""
    import numpy as np
    return np.concatenate(list(_iter_audio(container, expected)), axis=1)


def _stream_joined_audio(parent, child, expected_parent, expected_child):
    """Length-preserving seam shaping with at most 320 retained samples.

    Emits the entire parent then child timeline without allocating a full
    parent soundtrack. Context near the cut is the only withheld PCM.
    """
    import numpy as np
    tail = np.empty((2, 0), dtype=np.float32)
    for block in _iter_audio(parent, expected_parent):
        pending = np.concatenate((tail, block), axis=1)
        if pending.shape[1] > EDGE_FADE:
            yield np.ascontiguousarray(pending[:, :-EDGE_FADE])
            tail = np.ascontiguousarray(pending[:, -EDGE_FADE:])
        else:
            tail = pending
    opening = []
    first_count = 0
    ahead = None
    source = iter(_iter_audio(child, expected_child))
    for block in source:
        need = EDGE_FADE - first_count
        if need > 0:
            first = np.ascontiguousarray(block[:, :need])
            opening.append(first)
            first_count += first.shape[1]
            if first_count < EDGE_FADE:
                continue
            ahead = np.ascontiguousarray(block[:, need:])
        else:
            ahead = block
        break
    if not opening:
        raise ValueError("Native chain child has no audio")
    head = np.ascontiguousarray(np.concatenate(opening, axis=1))
    left, right = _smooth_seam(tail, head)
    if left.shape[1]:
        yield left
    if right.shape[1]:
        yield right
    if ahead is not None and ahead.shape[1]:
        yield ahead
    yield from source


def _fixed_audio_blocks(chunks):
    """Feed AAC full-size frames without ever buffering the whole soundtrack."""
    import numpy as np
    pending = np.empty((2, 0), dtype=np.float32)
    for chunk in chunks:
        if not chunk.shape[1]:
            continue
        pending = np.concatenate((pending, chunk), axis=1)
        while pending.shape[1] >= 1024:
            yield np.ascontiguousarray(pending[:, :1024])
            pending = pending[:, 1024:]
    if pending.shape[1]:
        yield np.ascontiguousarray(pending)


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


def _can_copy_h264(left, right):
    """Only stream-copy native H264 when decoder configurations are identical."""
    return (
        left.codec_context.name == right.codec_context.name == "h264"
        and left.width == right.width and left.height == right.height
        and left.codec_context.extradata == right.codec_context.extradata
        and bool(left.codec_context.extradata)
        and left.average_rate == right.average_rate == Fraction(FPS, 1)
        and left.time_base is not None and right.time_base is not None
    )


def _mux_h264_copy(out, first, second, total_frames):
    """Packet-level remux with shifted PTS/DTS, preserving B-frame ordering."""
    a, b = first.streams.video[0], second.streams.video[0]
    video = out.add_stream_from_template(a)
    dest_tb = video.time_base or a.time_base
    for clip, stream, origin in (
        (first, a, Fraction(0)), (second, b, Fraction(total_frames, FPS))
    ):
        for packet in clip.demux(stream):
            if packet.dts is None:
                continue
            source_tb = packet.time_base or stream.time_base
            if source_tb is None or dest_tb is None:
                raise ValueError("H264 packet has no decode clock")
            if packet.pts is not None:
                packet.pts = round((packet.pts * source_tb + origin) / dest_tb)
            packet.dts = round((packet.dts * source_tb + origin) / dest_tb)
            if packet.duration:
                packet.duration = max(1, round(packet.duration * source_tb / dest_tb))
            packet.time_base = dest_tb
            packet.stream = video
            out.mux(packet)
    return video


def _validate_video_count(path, expected):
    import av
    with av.open(str(path)) as result:
        if not result.streams.video:
            raise ValueError("Native chain joined output lacks a picture track")
        count = 0
        for _ in result.decode(video=0):
            count += 1
            if count > expected:
                raise ValueError("Native joined video duplicates frames")
        if count != expected:
            raise ValueError("Native joined video is missing frames")


def join_native_chain(parent, newly_generated, destination, *, parent_frames,
                      child_frames, expected_parent_sha256=None):
    """Create an atomically published, continuously muxed parent+child video.

    The child MP4 contains ONLY newly generated frames. The previous full
    chain is recomposed with constant-memory PCM streaming and exact frame
    ordering, without adding duplicated overlap frames.
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

    dest.parent.mkdir(parents=True, exist_ok=True)
    temporary = dest.parent / ("." + dest.name + "." + uuid.uuid4().hex + ".mp4")

    def compose(copy_video):
        import av
        import numpy as np
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
            # MP4 writes its header on the first mux. Register BOTH
            # tracks before writing even one video packet.
            target_audio = output.add_stream("aac", rate=RATE)
            target_audio.layout = "stereo"
            target_audio.bit_rate = 192000
            if copy_video:
                if not _can_copy_h264(source_video, newer_video):
                    raise ValueError("H264 source parameters differ")
                # Decode/validate inputs before remux. No video encoder is
                # initialized on the stream-copy path.
                for clip, expected in ((one, parent_frames), (two, child_frames)):
                    count = sum(1 for _ in clip.decode(video=0))
                    if count != expected:
                        raise ValueError("Native H264 source count differs from Gallery journal")
                one.seek(0)
                two.seek(0)
                _mux_h264_copy(output, one, two, parent_frames)
            else:
                target_video = output.add_stream("libx264", rate=FPS)
                target_video.width = source_video.width
                target_video.height = source_video.height
                target_video.pix_fmt = "yuv420p"
                target_video.options = {"preset": "veryfast", "crf": "19"}
                index = 0
                for clip, expected in ((one, parent_frames), (two, child_frames)):
                    count = 0
                    for frame in clip.decode(video=0):
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

            # Video packet iterators consume the input containers. Rewind
            # before decoding each original soundtrack independently.
            one.seek(0)
            two.seek(0)
            cursor = 0
            for values in _fixed_audio_blocks(
                    _stream_joined_audio(one, two, expected_parent, expected_child)):
                frame = av.AudioFrame.from_ndarray(np.ascontiguousarray(values),
                                                 format="fltp", layout="stereo")
                frame.sample_rate = RATE
                frame.pts = cursor
                frame.time_base = Fraction(1, RATE)
                for packet in target_audio.encode(frame):
                    output.mux(packet)
                cursor += values.shape[1]
            if cursor != expected_total:
                raise ValueError("Streaming native chain produced an incorrect PCM length")
            for packet in target_audio.encode(None):
                output.mux(packet)
        if temporary.stat().st_size < 64:
            raise RuntimeError("Native joined MP4 is empty")
        _validate_video_count(temporary, parent_frames + child_frames)

    try:
        copied = False
        # Packet remux is O(file size) but avoids H264 re-encoding every
        # historical frame when the two streams have identical codec headers.
        # If a container cannot mux compatible B-frames, rebuild the scratch
        # file with native x264 instead of publishing a corrupt timeline.
        try:
            compose(True)
            copied = True
        except (OSError, RuntimeError, ValueError, av.error.FFmpegError):
            temporary.unlink(missing_ok=True)
            compose(False)
        with temporary.open("rb") as written:
            os.fsync(written.fileno())
        os.replace(temporary, dest)
        return {"frames": parent_frames + child_frames,
                "audioSamples": expected_total,
                "seamsRepaired": 1, "videoStreamCopy": copied}
    finally:
        temporary.unlink(missing_ok=True)

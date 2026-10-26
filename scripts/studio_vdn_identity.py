"""Native first-frame visual identity for an entire H3 temporal lineage.

The original frame is a separate, immutable *conditioning image* on every
continued FL2VA window. Its PNG and SHA256 follow the authenticated video
ancestry, rather than being replaced with the last frame of each window.
The protected 22-frame overlap still supplies the true initial video latents.
"""
import hashlib
import io
import os
from pathlib import Path
import uuid

MAX_IMAGE_BYTES = 8 * 1024 * 1024
MAX_SIDE = 2048
MIN_SIDE = 32
SHORT_SIDE = 384


def identity_digest(path):
    from PIL import Image
    image = Path(path)
    if not image.is_absolute() or image.is_symlink() or not image.is_file():
        raise ValueError("Unsafe or missing native first-frame identity")
    if not 64 <= image.stat().st_size <= MAX_IMAGE_BYTES:
        raise ValueError("Original visual identity has an unsafe size")
    raw = image.read_bytes()
    if not raw.startswith(b"\x89PNG\r\n\x1a\n"):
        raise ValueError("The original visual identity is not a PNG")
    try:
        with Image.open(io.BytesIO(raw)) as picture:
            picture.verify()
        with Image.open(io.BytesIO(raw)) as picture:
            if (picture.mode not in ("RGB", "RGBA") or
                    not MIN_SIDE <= min(picture.size) <= MAX_SIDE or
                    max(picture.size) > MAX_SIDE):
                raise ValueError("Unsafe original first-frame image geometry")
    except (OSError, SyntaxError, Image.DecompressionBombError) as err:
        raise ValueError("Damaged native first-frame identity") from err
    return hashlib.sha256(raw).hexdigest()


def save_first_identity(output, *, first_video=None, inherited=None, expected_sha=None):
    """Commit the original first picture; no subsequent child can replace it."""
    import av
    from PIL import Image
    out = Path(output)
    if not out.is_absolute() or out.is_symlink() or out.parent.is_symlink():
        raise ValueError("Unsafe native first-frame destination")
    out.parent.mkdir(parents=True, exist_ok=True)
    if inherited:
        if identity_digest(inherited) != expected_sha:
            raise ValueError("Original picture differs from pinned film lineage")
        raw = Path(inherited).read_bytes()
    else:
        if not first_video:
            raise ValueError("Missing original clip for native identity extraction")
        first = Path(first_video)
        if not first.is_file() or first.is_symlink():
            raise ValueError("Unsafe first generated video for identity extraction")
        with av.open(str(first)) as container:
            frame = next(container.decode(video=0), None)
            if frame is None:
                raise ValueError("The original video contains no first frame")
            picture = frame.to_image().convert("RGB")
        scale = min(1.0, SHORT_SIDE / min(picture.size))
        if scale < 1:
            picture = picture.resize(
                (max(MIN_SIDE, round(picture.width * scale)),
                 max(MIN_SIDE, round(picture.height * scale))), Image.Resampling.LANCZOS)
        target = io.BytesIO()
        picture.save(target, format="PNG")
        raw = target.getvalue()
    if not 64 <= len(raw) <= MAX_IMAGE_BYTES:
        raise ValueError("Native identity PNG is too large")
    temp = out.parent / ("." + out.name + "." + uuid.uuid4().hex + ".tmp")
    try:
        with temp.open("xb") as stream:
            stream.write(raw)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temp, out)
        result = identity_digest(out)
        if expected_sha and result != expected_sha:
            raise ValueError("First-frame identity changed when stored")
        return result
    finally:
        temp.unlink(missing_ok=True)

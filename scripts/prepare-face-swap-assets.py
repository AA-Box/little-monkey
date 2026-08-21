#!/usr/bin/env python3
"""Fetch the pinned public assets used by the managed face-swap build."""

from __future__ import annotations

import argparse
import hashlib
import shutil
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path


MAX_BYTES = 2 * 1024 * 1024 * 1024
DOWNLOADS = {
    "buffalo_l.zip": (
        "https://github.com/deepinsight/insightface/releases/download/v0.7/buffalo_l.zip",
        "80ffe37d8a5940d59a7384c201a2a38d4741f2f3c51eef46ebb28218a7b0ca2f",
    ),
    "ghost_3_256.onnx": (
        "https://github.com/facefusion/facefusion-assets/releases/download/models-3.0.0/ghost_3_256.onnx",
        "308d87f565e881b8872a5cbe711f97faeda6643e5d2b95ef757cc92a58662abd",
    ),
    "crossface_ghost.onnx": (
        "https://huggingface.co/facefusion/models-3.4.0/resolve/main/crossface_ghost.onnx",
        "9ec5862d9ff1f723a7380ea89baf87cddec9c56670d4db766702657939284957",
    ),
    "codeformer-v0.1.0.tar.gz": (
        "https://github.com/sczhou/CodeFormer/archive/refs/tags/v0.1.0.tar.gz",
        "b1dafa3c624d2e79587170d6ce77020753126b18e0656ff27f72622c61594c96",
    ),
    "codeformer.pth": (
        "https://github.com/sczhou/CodeFormer/releases/download/v0.1.0/codeformer.pth",
        "1009e537e0c2a07d4cabce6355f53cb66767cd4b4297ec7a4a64ca4b8a5684b7",
    ),
}

LICENSE_URLS = {
    "model-license.txt": {
        "InsightFace": "https://raw.githubusercontent.com/deepinsight/insightface/master/server/LICENSING.md",
        "GHOST": "https://raw.githubusercontent.com/ai-forever/ghost/master/LICENSE",
    },
    "codeformer-license.txt": {
        "CodeFormer": "https://raw.githubusercontent.com/sczhou/CodeFormer/master/LICENSE",
    },
}


def download(url: str, destination: Path, expected_sha256: str) -> None:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_file():
        digest = hashlib.sha256(destination.read_bytes()).hexdigest()
        if digest == expected_sha256:
            return
        destination.unlink()
    partial = destination.with_suffix(destination.suffix + ".partial")
    digest = hashlib.sha256()
    total = 0
    with urllib.request.urlopen(url, timeout=120) as response, partial.open("wb") as output:
        while block := response.read(1024 * 1024):
            total += len(block)
            if total > MAX_BYTES:
                raise RuntimeError(f"download exceeds {MAX_BYTES} bytes: {url}")
            digest.update(block)
            output.write(block)
    actual = digest.hexdigest()
    if actual != expected_sha256:
        partial.unlink(missing_ok=True)
        raise RuntimeError(f"SHA-256 mismatch for {url}: expected {expected_sha256}, found {actual}")
    partial.replace(destination)


def safe_members(archive: zipfile.ZipFile | tarfile.TarFile):
    for member in archive.namelist() if isinstance(archive, zipfile.ZipFile) else archive.getmembers():
        name = member if isinstance(member, str) else member.name
        path = Path(name)
        if path.is_absolute() or ".." in path.parts:
            raise RuntimeError(f"archive traversal path: {name}")
        if not isinstance(member, str) and (member.issym() or member.islnk()):
            raise RuntimeError(f"archive link is not allowed: {name}")
        yield member


def download_licenses(root: Path) -> None:
    licenses = root / "licenses"
    licenses.mkdir(parents=True, exist_ok=True)
    for filename, sources in LICENSE_URLS.items():
        destination = licenses / filename
        with destination.open("w", encoding="utf-8") as output:
            for name, url in sources.items():
                output.write(f"===== {name} ({url}) =====\n")
                output.write(urllib.request.urlopen(url, timeout=60).read().decode("utf-8"))
                output.write("\n\n")


def prepare(root: Path, include_codeformer: bool) -> None:
    root.mkdir(parents=True, exist_ok=True)
    cache = root / ".downloads"
    cache.mkdir(exist_ok=True)
    models = root / "models"
    models.mkdir(exist_ok=True)

    for filename in ("buffalo_l.zip", "ghost_3_256.onnx", "crossface_ghost.onnx"):
        url, digest = DOWNLOADS[filename]
        download(url, cache / filename, digest)

    buffalo = models / "buffalo_l"
    buffalo.mkdir(exist_ok=True)
    with zipfile.ZipFile(cache / "buffalo_l.zip") as archive:
        members = list(safe_members(archive))
        for filename in ("det_10g.onnx", "w600k_r50.onnx", "genderage.onnx"):
            member = next((item for item in members if str(item).endswith("/" + filename) or str(item) == filename), None)
            if member is None:
                raise RuntimeError(f"buffalo_l.zip is missing {filename}")
            with archive.open(member) as source, (buffalo / filename).open("wb") as destination:
                shutil.copyfileobj(source, destination)
    shutil.copy2(cache / "ghost_3_256.onnx", models / "ghost_3_256.onnx")
    shutil.copy2(cache / "crossface_ghost.onnx", models / "crossface_ghost.onnx")
    download_licenses(root)

    if not include_codeformer:
        return
    download(DOWNLOADS["codeformer-v0.1.0.tar.gz"][0], cache / "codeformer-v0.1.0.tar.gz", DOWNLOADS["codeformer-v0.1.0.tar.gz"][1])
    download(DOWNLOADS["codeformer.pth"][0], models / "codeformer.pth", DOWNLOADS["codeformer.pth"][1])
    source = root / "codeformer-source"
    with tempfile.TemporaryDirectory(dir=cache) as temporary:
        extracted = Path(temporary)
        with tarfile.open(cache / "codeformer-v0.1.0.tar.gz") as archive:
            list(safe_members(archive))
            archive.extractall(extracted)
        unpacked = next(extracted.glob("CodeFormer-*/basicsr"), None)
        if unpacked is None:
            raise RuntimeError("CodeFormer archive is missing basicsr/")
        source.mkdir(parents=True, exist_ok=True)
        shutil.copytree(unpacked.parent, source, dirs_exist_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--codeformer", action="store_true")
    args = parser.parse_args()
    prepare(args.root.resolve(), args.codeformer)


if __name__ == "__main__":
    main()

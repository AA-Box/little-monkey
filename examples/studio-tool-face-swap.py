#!/usr/bin/env python3
"""Local GHOST face-swap sidecar for Studio.

The process is intentionally small and local-only. It loads an
InsightFace face-analysis pack for detection/embeddings, then runs a selected
GHOST ONNX model directly. CodeFormer restoration is optional and only loads
from a pinned, user-supplied checkout and checkpoint. Install dependencies from
the requirements files and provide model files where docs/face-swap-tool.md
describes.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import ipaddress
import json
import os
import sys
import threading
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


MANIFEST = {
    "schemaVersion": 1,
    "id": "ghost-face-swap-local",
    "name": "Face Swap (GHOST 3_256)",
    "description": (
        "Local face replacement using user-supplied GHOST weights. Use only images and identities "
        "you have permission to edit."
    ),
    "licenseNotice": {
        "title": "Non-commercial use only",
        "message": (
            "This tool uses public InsightFace model weights licensed for non-commercial academic "
            "research. Do not use this tool for commercial work unless you have separate written "
            "permission. CodeFormer, when enabled, has separate license terms."
        ),
        "commercialUseAllowed": False,
        "url": "https://github.com/deepinsight/insightface/blob/master/server/LICENSING.md",
    },
    "inputs": [
        {
            "key": "source",
            "label": "Source face",
            "kind": "image",
            "required": True,
            "hint": "Use a clear, front-facing image with one visible face.",
        },
        {
            "key": "target",
            "label": "Target image",
            "kind": "image",
            "required": True,
            "hint": "The face in this image will be replaced.",
        },
        {
            "key": "face_swap_model",
            "label": "Swap model",
            "kind": "choice",
            "required": False,
            "options": [
                {"value": "ghost_3_256", "label": "GHOST 3_256"},
            ],
            "default": "ghost_3_256",
            "hint": "Both models require the matching GHOST ONNX file and crossface_ghost converter.",
        },
        {
            "key": "restorer",
            "label": "Face restorer",
            "kind": "choice",
            "required": False,
            "options": [
                {"value": "none", "label": "None"},
                {"value": "codeformer", "label": "CodeFormer (user-supplied)"},
            ],
            "default": "none",
            "hint": "CodeFormer can improve detail but may alter identity; it requires separately supplied files and license permission.",
        },
        {
            "key": "codeformer_weight",
            "label": "CodeFormer fidelity",
            "kind": "number",
            "required": False,
            "min": 0,
            "max": 1,
            "step": 0.05,
            "default": 0.5,
            "hint": "Lower values favor perceptual restoration; higher values preserve the input.",
        },
        {
            "key": "license_acknowledged",
            "label": "I understand the non-commercial license",
            "kind": "toggle",
            "required": True,
            "hint": "Confirm that you have rights to use every supplied model and will not use this tool commercially.",
        },
        {
            "key": "source_face_index",
            "label": "Source face",
            "kind": "number",
            "required": False,
            "min": 0,
            "max": 15,
            "step": 1,
            "default": 0,
            "hint": "Faces are ordered from left to right, then top to bottom.",
        },
        {
            "key": "target_face_index",
            "label": "Target face",
            "kind": "number",
            "required": False,
            "min": 0,
            "max": 15,
            "step": 1,
            "default": 0,
            "hint": "Used when Swap all faces is off.",
        },
        {
            "key": "swap_all",
            "label": "Swap all faces",
            "kind": "toggle",
            "required": False,
            "default": False,
            "hint": "Apply the selected source identity to every detected target face.",
        },
        {
            "key": "face_swap_weight",
            "label": "Identity weight",
            "kind": "number",
            "required": False,
            "min": 0,
            "max": 1,
            "step": 0.05,
            "default": 0.5,
            "hint": "Higher values preserve more target facial structure.",
        },
    ],
}

MAX_BODY_BYTES = 64 * 1024 * 1024
MAX_IMAGE_BASE64_CHARS = 32 * 1024 * 1024
MAX_IMAGE_EDGE = 12_000


class ToolError(RuntimeError):
    """An actionable error safe to show in Studio."""


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=9099)
    return parser.parse_args()


def ensure_loopback(host: str) -> None:
    if host == "localhost":
        return
    try:
        address = ipaddress.ip_address(host)
    except ValueError as error:
        raise ToolError("Face Swap must bind to localhost or a loopback address") from error
    if not address.is_loopback:
        raise ToolError("Face Swap must bind to localhost or a loopback address")


def bundled_root() -> Path | None:
    root = getattr(sys, "_MEIPASS", None)
    return Path(root) if isinstance(root, str) and root else None


def model_root() -> Path:
    configured = os.environ.get("FACE_SWAP_MODEL_ROOT") or os.environ.get("INSIGHTFACE_HOME")
    bundle = bundled_root()
    candidates = [
        Path(configured).expanduser() if configured else None,
        bundle / "face-swap-models" if bundle else None,
        Path.cwd() / "models",
        Path("~/.insightface").expanduser(),
    ]
    for candidate in candidates:
        if candidate is not None and candidate.is_dir():
            return candidate
    return Path(configured).expanduser() if configured else Path("~/.insightface").expanduser()


def face_analysis_model_dir(root: Path, pack_name: str) -> Path:
    return root / "models" / pack_name


def require_face_analysis_models(root: Path, pack_name: str) -> Path:
    model_dir = face_analysis_model_dir(root, pack_name)
    required = [model_dir / name for name in ("det_10g.onnx", "w600k_r50.onnx", "genderage.onnx")]
    missing = [str(path) for path in required if not path.is_file()]
    if missing:
        raise ToolError(
            f"Face-analysis pack '{pack_name}' is incomplete at {model_dir}; "
            f"missing: {', '.join(missing)}. Automatic model downloads are disabled."
        )
    for filename, env_name in (
        ("det_10g.onnx", "FACE_SWAP_DETECTOR_SHA256"),
        ("w600k_r50.onnx", "FACE_SWAP_RECOGNITION_SHA256"),
        ("genderage.onnx", "FACE_SWAP_GENDERAGE_SHA256"),
    ):
        verify_sha256(model_dir / filename, env_name, f"Face-analysis model {filename}")
    return model_dir


def find_model(root: Path, env_name: str, filenames: tuple[str, ...], label: str) -> Path:
    configured = os.environ.get(env_name)
    candidates = [
        Path(configured).expanduser() if configured else None,
    ]
    candidates.extend(root / "models" / filename for filename in filenames)
    candidates.extend(Path.cwd() / "models" / filename for filename in filenames)
    for candidate in candidates:
        if candidate is not None and candidate.is_file():
            return candidate
    searched = ", ".join(str(candidate) for candidate in candidates if candidate is not None)
    raise ToolError(
        f"{label} not found. Set {env_name} or place the required model at one of: {searched}"
    )


def ghost_model_name(value: Any) -> str:
    if value in {None, "", "ghost_3_256"}:
        return "ghost_3_256"
    raise ToolError("Swap model must be ghost_3_256")


def find_ghost_model(root: Path, model_name: str) -> Path:
    return find_model(
        root,
        "FACE_SWAP_MODEL",
        (f"{model_name}.onnx",),
        f"{model_name} model",
    )


def find_embedding_converter(root: Path) -> Path:
    return find_model(
        root,
        "FACE_SWAP_EMBEDDING_CONVERTER",
        ("crossface_ghost.onnx",),
        "GHOST embedding converter",
    )


def find_codeformer_model(root: Path) -> Path:
    return find_model(
        root,
        "FACE_SWAP_CODEFORMER_MODEL",
        ("codeformer.pth", "CodeFormer/codeformer.pth"),
        "CodeFormer model",
    )


def codeformer_root() -> Path:
    configured = os.environ.get("FACE_SWAP_CODEFORMER_HOME")
    bundle = bundled_root()
    candidates = [
        Path(configured).expanduser() if configured else None,
        bundle / "codeformer-source" if bundle else None,
        Path.cwd() / "CodeFormer",
        Path.cwd() / "codeformer",
    ]
    for candidate in candidates:
        if candidate is not None and (candidate / "basicsr").is_dir():
            return candidate
    searched = ", ".join(str(candidate) for candidate in candidates if candidate is not None)
    raise ToolError(
        "CodeFormer source is missing. Set FACE_SWAP_CODEFORMER_HOME to a pinned "
        f"checkout containing basicsr/ (searched: {searched})"
    )


def verify_sha256(path: Path, env_name: str, label: str) -> None:
    expected = os.environ.get(env_name, "").strip().lower()
    require_hash = os.environ.get("FACE_SWAP_REQUIRE_MODEL_HASH", "0") == "1"
    if not expected:
        if require_hash:
            raise ToolError(f"{env_name} is required when FACE_SWAP_REQUIRE_MODEL_HASH=1")
        return
    if len(expected) != 64 or any(character not in "0123456789abcdef" for character in expected):
        raise ToolError(f"{env_name} must be a 64-character SHA-256 hex digest")
    import hashlib

    digest = hashlib.sha256()
    try:
        with path.open("rb") as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b""):
                digest.update(block)
    except OSError as error:
        raise ToolError(f"Could not read {label} for checksum verification: {error}") from error
    actual = digest.hexdigest()
    if actual != expected:
        raise ToolError(f"{label} checksum mismatch: expected {expected}, found {actual}")


def providers() -> list[str]:
    try:
        import onnxruntime as ort
    except ImportError as error:
        raise ToolError(
            "Missing face-swap dependencies. Run the install command in "
            "docs/face-swap-tool.md."
        ) from error

    available = set(ort.get_available_providers())
    requested = os.environ.get("FACE_SWAP_PROVIDER", "auto").strip().lower()
    if requested in {"auto", ""}:
        return (
            ["CUDAExecutionProvider", "CPUExecutionProvider"]
            if "CUDAExecutionProvider" in available
            else ["CPUExecutionProvider"]
        )
    if requested in {"cpu", "cpuexecutionprovider"}:
        return ["CPUExecutionProvider"]
    if requested in {"cuda", "cudaexecutionprovider"}:
        if "CUDAExecutionProvider" not in available:
            raise ToolError(
                "FACE_SWAP_PROVIDER=cuda was requested, but this Python "
                "environment has no CUDAExecutionProvider."
            )
        return ["CUDAExecutionProvider", "CPUExecutionProvider"]
    raise ToolError("FACE_SWAP_PROVIDER must be auto, cpu, or cuda")


class FaceSwapRuntime:
    """Lazily loaded, serialized GHOST ONNX inference state."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._loaded = False
        self._analysis: Any = None
        self._ghost_models: dict[str, Any] = {}
        self._converter: Any = None
        self._codeformer: Any = None
        self._torch: Any = None
        self._cv2: Any = None
        self._np: Any = None
        self._ort: Any = None

    def load(self) -> None:
        if self._loaded:
            return
        with self._lock:
            if self._loaded:
                return
            try:
                import cv2
                import numpy as np
                import onnxruntime as ort
            except ImportError as error:
                raise ToolError(
                    "Missing face-swap dependencies. Run the install command in "
                    "docs/face-swap-tool.md."
                ) from error

            root = model_root()
            pack_name = os.environ.get("FACE_SWAP_FACE_PACK", "buffalo_l")
            require_face_analysis_models(root, pack_name)
            ghost_model = find_ghost_model(root, "ghost_3_256")
            converter_model = find_embedding_converter(root)
            verify_sha256(ghost_model, "FACE_SWAP_MODEL_SHA256", "GHOST model")
            verify_sha256(
                converter_model,
                "FACE_SWAP_EMBEDDING_CONVERTER_SHA256",
                "GHOST embedding converter",
            )
            selected_providers = providers()

            try:
                from insightface.app import FaceAnalysis

                analysis = FaceAnalysis(
                    name=pack_name, root=str(root), providers=selected_providers
                )
                analysis.prepare(ctx_id=0, det_size=(640, 640))
                ghost = ort.InferenceSession(str(ghost_model), providers=selected_providers)
                converter = ort.InferenceSession(
                    str(converter_model), providers=selected_providers
                )
                self._validate_session_input(ghost, "source")
                self._validate_session_input(ghost, "target")
                self._validate_session_input(converter, "input")
            except (ImportError, ModuleNotFoundError) as error:
                raise ToolError(
                    "The InsightFace face-analysis dependency cannot be imported. "
                    "Reinstall the dependencies from docs/face-swap-tool.md."
                ) from error
            except Exception as error:  # model/provider errors need a Studio-safe message
                raise ToolError(f"Face Swap models could not be loaded: {error}") from error
            self._cv2 = cv2
            self._np = np
            self._ort = ort
            self._analysis = analysis
            self._ghost_models["ghost_3_256"] = ghost
            self._converter = converter
            self._loaded = True
            print(
                f"face-swap ready: engine=ghost-3_256 model={ghost_model} "
                f"converter={converter_model} providers={selected_providers}",
                file=sys.stderr,
                flush=True,
            )

    def _ghost_model(self, model_name: str) -> Any:
        model_name = ghost_model_name(model_name)
        if model_name in self._ghost_models:
            return self._ghost_models[model_name]
        model_path = find_ghost_model(model_root(), model_name)
        verify_sha256(model_path, "FACE_SWAP_MODEL_SHA256", "GHOST model")
        session = self._ort.InferenceSession(
            str(model_path), providers=providers()
        )
        self._validate_session_input(session, "source")
        self._validate_session_input(session, "target")
        self._ghost_models[model_name] = session
        return session

    def _load_codeformer(self) -> None:
        if self._codeformer is not None:
            return
        try:
            import importlib

            torch = importlib.import_module("torch")
        except ImportError as error:
            raise ToolError(
                "CodeFormer needs the optional PyTorch dependencies. Install "
                "examples/studio-tool-face-swap-codeformer-requirements.txt."
            ) from error
        source = codeformer_root()
        model_path = find_codeformer_model(model_root())
        verify_sha256(model_path, "FACE_SWAP_CODEFORMER_SHA256", "CodeFormer model")
        if str(source) not in sys.path:
            sys.path.insert(0, str(source))
        try:
            registry = importlib.import_module("basicsr.utils.registry")
            importlib.import_module("basicsr.archs")
            arch_registry = registry.ARCH_REGISTRY

            net = arch_registry.get("CodeFormer")(
                dim_embd=512,
                codebook_size=1024,
                n_head=8,
                n_layers=9,
                connect_list=["32", "64", "128", "256"],
            )
            checkpoint = torch.load(str(model_path), map_location="cpu", weights_only=True)
            net.load_state_dict(checkpoint.get("params_ema", checkpoint))
            device = "cuda" if "CUDAExecutionProvider" in providers() and torch.cuda.is_available() else "cpu"
            net.to(device)
            net.eval()
        except Exception as error:
            raise ToolError(f"CodeFormer could not be loaded: {error}") from error
        self._torch = torch
        self._codeformer = net

    @staticmethod
    def _validate_session_input(session: Any, name: str) -> None:
        if not any(item.name == name for item in session.get_inputs()):
            names = ", ".join(item.name for item in session.get_inputs())
            raise ToolError(f"ONNX model is missing the '{name}' input; found: {names}")

    @staticmethod
    def _ordered(faces: list[Any]) -> list[Any]:
        return sorted(faces, key=lambda face: (float(face.bbox[0]), float(face.bbox[1])))

    def _decode(self, encoded: Any, label: str) -> Any:
        if not isinstance(encoded, str) or not encoded:
            raise ToolError(f"{label} is required")
        if len(encoded) > MAX_IMAGE_BASE64_CHARS:
            raise ToolError(f"{label} exceeds the image size limit")
        if encoded.startswith("data:"):
            _, separator, encoded = encoded.partition(",")
            if not separator:
                raise ToolError(f"{label} is not a valid image")
        try:
            raw = base64.b64decode(encoded, validate=True)
        except (binascii.Error, ValueError) as error:
            raise ToolError(f"{label} is not valid base64") from error
        image = self._cv2.imdecode(self._np.frombuffer(raw, dtype=self._np.uint8), self._cv2.IMREAD_COLOR)
        if image is None:
            raise ToolError(f"{label} is not a readable image")
        height, width = image.shape[:2]
        if max(height, width) > MAX_IMAGE_EDGE:
            raise ToolError(f"{label} is larger than the {MAX_IMAGE_EDGE}px edge limit")
        return image

    @staticmethod
    def _index(value: Any, label: str) -> int:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ToolError(f"{label} must be a whole number")
        result = int(value)
        if result != value or result < 0 or result > 15:
            raise ToolError(f"{label} must be between 0 and 15")
        return result

    def _face(self, faces: list[Any], index: int, label: str) -> Any:
        ordered = self._ordered(faces)
        if not ordered:
            raise ToolError(f"No face was detected in the {label} image")
        if index >= len(ordered):
            raise ToolError(f"{label} face {index} was not found; detected {len(ordered)}")
        return ordered[index]

    @staticmethod
    def _weight(value: Any) -> float:
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise ToolError("Identity weight must be a number between 0 and 1")
        result = float(value)
        if not 0 <= result <= 1:
            raise ToolError("Identity weight must be a number between 0 and 1")
        return result

    @staticmethod
    def _restorer(value: Any) -> str:
        if value in {None, "", "none"}:
            return "none"
        if value == "codeformer":
            return "codeformer"
        raise ToolError("Face restorer must be none or codeformer")

    def _landmarks(self, face: Any) -> Any:
        landmarks = getattr(face, "kps", None)
        if landmarks is None:
            landmarks = getattr(face, "landmark_5", None)
        if landmarks is None:
            raise ToolError("Face analysis did not provide five-point landmarks")
        landmarks = self._np.asarray(landmarks, dtype=self._np.float32).reshape(-1, 2)
        if landmarks.shape != (5, 2):
            raise ToolError("Face analysis returned invalid five-point landmarks")
        return landmarks

    def _embedding(self, face: Any, label: str) -> Any:
        embedding = getattr(face, "embedding", None)
        if embedding is None:
            raise ToolError(f"Face analysis did not provide a 512-dimensional {label} embedding")
        embedding = self._np.asarray(embedding, dtype=self._np.float32).reshape(-1)
        if embedding.size != 512:
            raise ToolError(f"Face analysis returned an invalid {label} embedding")
        return embedding.reshape(1, 512)

    def _ghost_input(self, session: Any, name: str, value: Any) -> dict[str, Any]:
        return {name: value}

    def _source_embedding(self, source_face: Any, target_face: Any, weight: float) -> Any:
        source_embedding = self._embedding(source_face, "source")
        converter_input = self._ghost_input(self._converter, "input", source_embedding)
        converted = self._converter.run(None, converter_input)[0].reshape(-1).astype(self._np.float32)
        norm = self._np.linalg.norm(converted)
        if not norm:
            raise ToolError("GHOST embedding conversion returned an empty identity")
        converted = converted / norm

        # This follows FaceFusion's GHOST weight interpolation: 0 favors the
        # source identity, while 1 preserves more of the target embedding.
        blend = self._np.interp(weight, [0, 1], [0.35, -0.35]).astype(self._np.float32)
        target_embedding = self._embedding(target_face, "target")
        return (converted.reshape(1, -1) * (1 - blend) + target_embedding * blend).astype(
            self._np.float32
        )

    def _swap_one(
        self,
        image: Any,
        source_face: Any,
        target_face: Any,
        weight: float,
        model_name: str,
    ) -> Any:
        template = self._np.array(
            [
                [0.35473214, 0.45658929],
                [0.64526786, 0.45658929],
                [0.50000000, 0.61154464],
                [0.37913393, 0.77687500],
                [0.62086607, 0.77687500],
            ],
            dtype=self._np.float32,
        ) * 256
        affine, _ = self._cv2.estimateAffinePartial2D(
            self._landmarks(target_face),
            template,
            method=self._cv2.RANSAC,
            ransacReprojThreshold=100,
        )
        if affine is None:
            raise ToolError("Could not align the target face for GHOST")
        crop = self._cv2.warpAffine(
            image,
            affine,
            (256, 256),
            borderMode=self._cv2.BORDER_REPLICATE,
            flags=self._cv2.INTER_AREA,
        )
        crop_rgb = crop[:, :, ::-1].astype(self._np.float32) / 255.0
        crop_rgb = (crop_rgb - 0.5) / 0.5
        crop_input = crop_rgb.transpose(2, 0, 1)[None].astype(self._np.float32)
        ghost_inputs = {
            "source": self._source_embedding(source_face, target_face, weight),
            "target": crop_input,
        }
        output = self._ghost_model(model_name).run(None, ghost_inputs)[0]
        output = self._np.asarray(output).squeeze(0)
        if output.shape[0] == 3:
            output = output.transpose(1, 2, 0)
        if output.shape != (256, 256, 3):
            raise ToolError(f"GHOST returned an unexpected output shape: {output.shape}")
        output = self._np.clip(output * 0.5 + 0.5, 0, 1)
        output = (output[:, :, ::-1] * 255).astype(self._np.uint8)

        mask = self._np.zeros((256, 256), dtype=self._np.float32)
        self._cv2.ellipse(mask, (128, 132), (104, 116), 0, 0, 360, 1, -1)
        mask = self._cv2.GaussianBlur(mask, (0, 0), 8)
        inverse = self._cv2.invertAffineTransform(affine)
        height, width = image.shape[:2]
        pasted = self._cv2.warpAffine(
            output, inverse, (width, height), borderMode=self._cv2.BORDER_REPLICATE
        )
        pasted_mask = self._cv2.warpAffine(mask, inverse, (width, height)).clip(0, 1)[..., None]
        return (
            image.astype(self._np.float32) * (1 - pasted_mask)
            + pasted.astype(self._np.float32) * pasted_mask
        ).astype(self._np.uint8)

    def _restore_one(self, image: Any, target_face: Any, weight: float) -> Any:
        self._load_codeformer()
        template = self._np.array(
            [
                [192.98138, 239.94708],
                [318.90277, 240.19360],
                [256.63416, 314.01935],
                [201.26117, 371.41043],
                [313.08905, 371.15118],
            ],
            dtype=self._np.float32,
        )
        affine, _ = self._cv2.estimateAffinePartial2D(
            self._landmarks(target_face),
            template,
            method=self._cv2.RANSAC,
            ransacReprojThreshold=100,
        )
        if affine is None:
            raise ToolError("Could not align the face for CodeFormer")
        crop = self._cv2.warpAffine(
            image,
            affine,
            (512, 512),
            borderMode=self._cv2.BORDER_REPLICATE,
            flags=self._cv2.INTER_AREA,
        )
        crop_rgb = crop[:, :, ::-1].copy().astype(self._np.float32) / 255.0
        tensor = self._torch.from_numpy(crop_rgb.transpose(2, 0, 1)).unsqueeze(0)
        tensor = (tensor - 0.5) / 0.5
        device = next(self._codeformer.parameters()).device
        try:
            with self._torch.no_grad():
                restored = self._codeformer(
                    tensor.to(device), w=weight, adain=True
                )[0].clamp(-1, 1)
            restored = (
                ((restored[0].detach().cpu().numpy().transpose(1, 2, 0) + 1) * 127.5)
                .clip(0, 255)
                .astype(self._np.uint8)
            )[:, :, ::-1]
        except Exception as error:
            raise ToolError(f"CodeFormer inference failed: {error}") from error

        mask = self._np.zeros((512, 512), dtype=self._np.float32)
        self._cv2.ellipse(mask, (256, 284), (208, 236), 0, 0, 360, 1, -1)
        mask = self._cv2.GaussianBlur(mask, (0, 0), 12)
        inverse = self._cv2.invertAffineTransform(affine)
        height, width = image.shape[:2]
        pasted = self._cv2.warpAffine(
            restored, inverse, (width, height), borderMode=self._cv2.BORDER_REPLICATE
        )
        pasted_mask = self._cv2.warpAffine(mask, inverse, (width, height)).clip(0, 1)[..., None]
        return (
            image.astype(self._np.float32) * (1 - pasted_mask)
            + pasted.astype(self._np.float32) * pasted_mask
        ).astype(self._np.uint8)

    def run(self, inputs: dict[str, Any]) -> str:
        if inputs.get("license_acknowledged") is not True:
            raise ToolError(
                "Confirm that you have permission to use every supplied model and checkpoint"
            )
        self.load()
        source = self._decode(inputs.get("source"), "Source image")
        target = self._decode(inputs.get("target"), "Target image")
        model_name = ghost_model_name(inputs.get("face_swap_model", "ghost_3_256"))
        restorer = self._restorer(inputs.get("restorer", "none"))
        source_index = self._index(inputs.get("source_face_index", 0), "Source face")
        target_index = self._index(inputs.get("target_face_index", 0), "Target face")
        weight = self._weight(inputs.get("face_swap_weight", 0.5))
        codeformer_weight = self._weight(inputs.get("codeformer_weight", 0.5))
        swap_all = inputs.get("swap_all", False)
        if not isinstance(swap_all, bool):
            raise ToolError("Swap all faces must be a boolean")

        with self._lock:
            source_face = self._face(self._analysis.get(source), source_index, "source")
            target_faces = self._ordered(self._analysis.get(target))
            if not target_faces:
                raise ToolError("No face was detected in the target image")
            if not swap_all and target_index >= len(target_faces):
                raise ToolError(
                    f"Target face {target_index} was not found; detected {len(target_faces)}"
                )

            result = target.copy()
            selected_faces = target_faces if swap_all else [target_faces[target_index]]
            for target_face in selected_faces:
                result = self._swap_one(result, source_face, target_face, weight, model_name)
            if restorer == "codeformer":
                for target_face in selected_faces:
                    result = self._restore_one(result, target_face, codeformer_weight)

            encoded_ok, encoded = self._cv2.imencode(
                ".png", result, [self._cv2.IMWRITE_PNG_COMPRESSION, 3]
            )
            if not encoded_ok:
                raise ToolError("Face Swap could not encode the result")
            return base64.b64encode(encoded.tobytes()).decode("ascii")


def read_body(request: BaseHTTPRequestHandler) -> bytes:
    content_length = request.headers.get("Content-Length")
    if content_length is None:
        raise ToolError("The run request needs a Content-Length")
    try:
        length = int(content_length)
    except ValueError as error:
        raise ToolError("The run request has an invalid Content-Length") from error
    if length < 0 or length > MAX_BODY_BYTES:
        raise ToolError("The run request exceeds the image size limit")
    body = request.rfile.read(length)
    if len(body) != length:
        raise ToolError("The run request ended before its body was read")
    return body


def write_json(request: BaseHTTPRequestHandler, status: HTTPStatus, payload: dict[str, Any]) -> None:
    body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
    request.send_response(status)
    request.send_header("Content-Type", "application/json")
    request.send_header("Content-Length", str(len(body)))
    request.send_header("Connection", "close")
    request.end_headers()
    request.wfile.write(body)


def make_handler(runtime: FaceSwapRuntime) -> type[BaseHTTPRequestHandler]:
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
            if self.path != "/tool/v1/manifest":
                write_json(self, HTTPStatus.NOT_FOUND, {"error": "unknown route"})
                return
            try:
                runtime.load()
                write_json(self, HTTPStatus.OK, MANIFEST)
            except ToolError as error:
                write_json(self, HTTPStatus.SERVICE_UNAVAILABLE, {"error": str(error)})

        def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
            if self.path != "/tool/v1/run":
                write_json(self, HTTPStatus.NOT_FOUND, {"error": "unknown route"})
                return
            try:
                payload = json.loads(read_body(self).decode("utf-8"))
                inputs = payload.get("inputs") if isinstance(payload, dict) else None
                if not isinstance(inputs, dict):
                    raise ToolError("The run request needs an inputs object")
                result = runtime.run(inputs)
                write_json(
                    self,
                    HTTPStatus.OK,
                    {"media": [{"mediaType": "image/png", "dataBase64": result}]},
                )
            except (ToolError, UnicodeDecodeError, json.JSONDecodeError) as error:
                write_json(self, HTTPStatus.BAD_REQUEST, {"error": str(error)})

        def log_message(self, format: str, *args: Any) -> None:
            print(f"face-swap: {format % args}", file=sys.stderr, flush=True)

    return Handler


def main() -> int:
    args = parse_args()
    try:
        ensure_loopback(args.host)
        if not 0 < args.port < 65_536:
            raise ToolError("Port must be between 1 and 65535")
        server = ThreadingHTTPServer((args.host, args.port), make_handler(FaceSwapRuntime()))
    except (OSError, ToolError) as error:
        print(f"face-swap: {error}", file=sys.stderr)
        return 1
    print(f"face-swap listening on {args.host}:{args.port}", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

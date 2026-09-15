"""Opt-in real-model smoke test for the packaged MLX service.

This deliberately does not download weights. Point it at a model directory
that is already present on an Apple-silicon test host:

    LITTLE_MONKEY_MLX_E2E_MODEL=/models/Qwen3.6-35B-A3B-4bit \
      python3 packaging/mlx/service/test_mlx_model_e2e.py

For the Qwen3.6 release-gate shape, also set
``LITTLE_MONKEY_MLX_E2E_REQUIRE_QWEN36=1``. That verifies the checkpoint itself
is the expected qwen3_5_moe + vision-tower layout before loading it.
"""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))

import mlx_server  # noqa: E402


MODEL_ENV = "LITTLE_MONKEY_MLX_E2E_MODEL"
REQUIRE_QWEN36_ENV = "LITTLE_MONKEY_MLX_E2E_REQUIRE_QWEN36"


def _checkpoint_config(model_dir: Path) -> dict:
    with (model_dir / "config.json").open("r", encoding="utf-8") as handle:
        value = json.load(handle)
    if not isinstance(value, dict):
        raise AssertionError("config.json must be an object")
    return value


def main() -> int:
    raw = os.environ.get(MODEL_ENV)
    if not raw:
        print(f"skip: set {MODEL_ENV} to run the real-model MLX smoke test")
        return 0

    model_dir = Path(raw).expanduser().resolve()
    assert model_dir.is_dir(), model_dir
    config = _checkpoint_config(model_dir)

    if os.environ.get(REQUIRE_QWEN36_ENV) == "1":
        assert config.get("model_type") == "qwen3_5_moe", config.get("model_type")
        assert isinstance(config.get("vision_config"), dict), "Qwen3.6 checkpoint must carry vision_config"
        quantization = config.get("quantization")
        assert isinstance(quantization, dict), "Qwen3.6 checkpoint must declare quantization"
        assert quantization.get("bits") == 4, quantization
        assert quantization.get("group_size") == 64, quantization
        assert quantization.get("mode") == "affine", quantization

    runtime = mlx_server._load_model(str(model_dir))
    prompt = runtime.render(
        [{"role": "user", "content": "Reply with exactly the word OK."}],
        0,
    )
    chunks = list(runtime.stream(prompt, [], 8, 0.0))
    text = "".join(chunks).strip()
    assert text, "model loaded but returned no text"
    print(
        "ok: real MLX model loaded and generated text "
        f"(model_type={config.get('model_type')}, chunks={len(chunks)}, text={text!r})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

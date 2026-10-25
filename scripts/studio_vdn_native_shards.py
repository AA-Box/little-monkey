"""Little Monkey first-party *direct original SafeTensors* streaming.

Unlike Accelerate's disk-offload mode this does not export a second copy of
any model parameter to .dat files. Models start on meta; leaf/module hooks
materialize only parameters needed for the current forward from the *original*
Hugging Face snapshot shards. They return to meta even on a failed forward.

Strict fail-closed contract: only inference, sharded safetensors, complete
parameter coverage, batch-synchronous forward and supported adapters.
"""
import contextlib
import json
from pathlib import Path
import re
import threading

_SAFE_SHARD = re.compile(r"^[A-Za-z0-9_][A-Za-z0-9_.-]*\.safetensors$")


def validate_weight_map(index, *, limit=250000):
    if not isinstance(index, dict) or not isinstance(index.get("weight_map"), dict):
        raise ValueError("Native direct loader requires a SafeTensors shard index")
    mapping = index["weight_map"]
    if not 0 < len(mapping) <= limit:
        raise ValueError("Missing or oversized SafeTensors weight map")
    for tensor, filename in mapping.items():
        if (not isinstance(tensor, str) or not tensor or len(tensor) > 512
                or not isinstance(filename, str)
                or not _SAFE_SHARD.fullmatch(filename)
                or filename.startswith(".")):
            raise ValueError("Invalid SafeTensors weight map entry")
    return mapping


class OriginalShardStore:
    """Only one tensor at a time; shard files are fetched once to HF cache."""
    def __init__(self, index, files):
        self.index = validate_weight_map(index)
        self.files = {name: Path(path) for name, path in files.items()}
        if set(self.index.values()) != set(self.files):
            raise ValueError("Original shard files do not match index")
        for name, path in self.files.items():
            if not path.is_file() or path.suffix != ".safetensors":
                raise ValueError("Missing original SafeTensors shard: " + name)

    @classmethod
    def from_hub(cls, repo, folder, revision=None, *, fetch=None):
        """Download only named original shards; no copies/transcodes."""
        from huggingface_hub import hf_hub_download
        fetch = fetch or hf_hub_download
        if folder and (Path(folder).is_absolute() or ".." in Path(folder).parts):
            raise ValueError("Unsafe model subfolder")
        # Diffusers uses diffusion_pytorch_model.*, Transformers uses model.*.
        filename = ("model.safetensors.index.json" if
                    folder.split("/")[-1] == "text_encoder" else
                    "diffusion_pytorch_model.safetensors.index.json")
        params = dict(subfolder=folder or None, revision=revision)
        try:
            index_path = Path(fetch(repo, filename=filename, **params))
        except Exception:
            # Small audio VAEs may be published as a single SafeTensors
            # file rather than a weight-map index. Never fall back to pickle.
            from safetensors import safe_open
            single = filename.replace(".index.json", "")
            file_path = Path(fetch(repo, filename=single, **params))
            with safe_open(str(file_path), framework="pt", device="cpu") as reader:
                index = {"weight_map": {key: single for key in reader.keys()}}
            return cls(index, {single: file_path})
        if not index_path.is_file() or index_path.stat().st_size > 16 * 1024**2:
            raise ValueError("Invalid original SafeTensors index file size")
        index = json.loads(index_path.read_text(encoding="utf8"))
        names = set(validate_weight_map(index).values())
        if len(names) > 128:
            raise ValueError("Too many original SafeTensors shards")
        paths = {name: fetch(repo, filename=name, **params) for name in sorted(names)}
        return cls(index, paths)

    def tensor(self, key):
        from safetensors import safe_open
        shard = self.index.get(key)
        if shard is None:
            raise KeyError("Missing original checkpoint tensor: " + key)
        with safe_open(str(self.files[shard]), framework="pt", device="cpu") as source:
            return source.get_tensor(key)


def instantiate_meta_component(cls, repo, subfolder, revision=None, *,
                               transformers_model=False, dtype=None):
    """Instantiate a *real* architecture from config, with zero initialized weights."""
    from accelerate import init_empty_weights
    with init_empty_weights(include_buffers=True):
        if transformers_model:
            config = cls.config_class.from_pretrained(
                repo, subfolder=subfolder or None, revision=revision)
            module = cls(config)
        else:
            module = cls.from_config(repo, subfolder=subfolder or None,
                                     revision=revision)
    if dtype is not None:
        module.to(dtype=dtype)
    # Model constructors can register buffers on CPU despite meta context;
    # no fallback to allocating the full model is permitted.
    import torch
    for name, parameter in module.named_parameters():
        if not parameter.is_meta:
            raise RuntimeError("Initial H3 weights were allocated instead of meta: " + name)
    module.eval().requires_grad_(False)
    return module


class NativeOriginalShardHooks:
    """Bounded per-module weights, with fail-closed shape/dtype checks.

    Modules access their parameters through regular PyTorch forward. Legacy
    weight_norm gets the weights first because our pre-hook is prepended.
    FP8 is applied to resident Linear weights at each forward, never to
    Audio/Video VAEs (whose released precision is materially significant).
    """
    def __init__(self, model, store, *, device, fp8=False, component=""):
        import torch
        self.model = model
        self.store = store
        self.device = torch.device(device)
        self.fp8 = fp8
        self.component = component
        self.lock = threading.RLock()
        self.handles = []
        self.loaded = set()
        self.layers = 0
        self.weight_names = set()
        if fp8 and component in ("vae", "audio_vae"):
            raise ValueError("FP8 VAE weights are unsupported: video/audio fidelity risk")
        if fp8:
            if self.device.type != "cuda" or torch.cuda.get_device_capability(self.device)[0] < 9:
                raise ValueError("FP8 original-shard stream requires NVIDIA Hopper or later")
            try:
                from torchao.quantization import Float8WeightOnlyConfig, quantize_
            except ImportError as exc:
                raise RuntimeError("FP8 requires torchao Float8WeightOnlyConfig") from exc
            self.fp8_config = Float8WeightOnlyConfig
            self.fp8_quantize = quantize_
        registered = []
        for prefix, module in model.named_modules():
            names = []
            for name, param in module.named_parameters(recurse=False):
                if param is not None:
                    names.append((name, True, param.shape, param.dtype))
            for name, buffer in module.named_buffers(recurse=False):
                if buffer is not None and buffer.is_meta:
                    names.append((name, False, buffer.shape, buffer.dtype))
            if not names:
                continue
            paths = [prefix + "." + n if prefix else n for n, *_ in names]
            for key in paths:
                if key not in store.index:
                    raise ValueError("Missing original weight for " + component + "/" + key)
                self.weight_names.add(key)
            registered.append((module, tuple(names), tuple(paths)))
        # A silent partial load is dangerous with meta parameters and LoRA.
        unexpected = set(store.index) - self.weight_names
        if unexpected:
            raise ValueError("Unmapped original SafeTensors weights: " +
                             ", ".join(sorted(unexpected)[:4]))
        if not registered:
            raise ValueError("No native sharded modules matched the model")
        for module, entries, paths in registered:
            module.register_forward_pre_hook(
                self._make_before(module, entries, paths), prepend=True)
            module.register_forward_hook(
                self._make_after(module, entries), always_call=True)
            self.layers += 1

    def _make_before(self, module, entries, paths):
        def before(_module, _inputs):
            import torch
            with self.lock:
                if module in self.loaded:
                    raise RuntimeError("Recursive forward on live streamed module is unsupported")
                hydrated = []
                try:
                    for (name, parameter, shape, dtype), key in zip(entries, paths):
                        original = self.store.tensor(key)
                        if tuple(original.shape) != tuple(shape):
                            raise RuntimeError("Checkpoint shape mismatch: " + key)
                        with torch.no_grad():
                            materialized = original.to(device=self.device, dtype=dtype,
                                                       non_blocking=False)
                            if parameter:
                                module._parameters[name] = torch.nn.Parameter(
                                    materialized, requires_grad=False)
                            else:
                                module._buffers[name] = materialized
                        hydrated.append((name, parameter, shape, dtype))
                    if self.fp8 and isinstance(module, torch.nn.Linear):
                        # An explicitly installed TorchAO FP8 weight-only
                        # operator; no unsafe BF16 fallback when FP8 requested.
                        self.fp8_quantize(module, self.fp8_config())
                    self.loaded.add(module)
                except BaseException:
                    self._evict(module, hydrated)
                    raise
        return before

    def _evict(self, module, entries):
        import torch
        with torch.no_grad():
            for name, parameter, shape, dtype in entries:
                empty = torch.empty(tuple(shape), dtype=dtype, device="meta")
                if parameter:
                    module._parameters[name] = torch.nn.Parameter(empty, requires_grad=False)
                else:
                    module._buffers[name] = empty
            # torch.nn.utils.weight_norm uses a legacy forward pre-hook and
            # caches computed weight as a plain Tensor attribute. Release it.
            if getattr(module, "weight", None) is not None and "weight_v" in module._parameters:
                module.weight = torch.empty_like(module.weight, device="meta")
        self.loaded.discard(module)

    def _make_after(self, module, entries):
        def after(_module, _inputs, _output):
            with self.lock:
                if module in self.loaded:
                    self._evict(module, entries)
            # None preserves the original module result.
        return after


def attach_native_lora(model, files, *, scale=1.0):
    """Native inference-time LoRA overlay from original adapter SafeTensors.

    One pair of low-rank tensors is read only when the target Linear executes.
    Accepts common lora_A/lora_B and lora_down/lora_up naming, fails closed
    on every unmatched tensor or incompatible shape. Does not merge weights.
    """
    from safetensors import safe_open
    import torch
    import torch.nn.functional as F
    if not 0 <= scale <= 4:
        raise ValueError("LoRA multiplier must be between 0 and 4")
    found = 0
    model_modules = dict(model.named_modules())
    for filename in files:
        path = Path(filename)
        if path.suffix != ".safetensors" or not path.is_file():
            raise ValueError("LoRA must reference a local SafeTensors file")
        with safe_open(str(path), framework="pt", device="cpu") as source:
            keys = set(source.keys())
        prefixes = {}
        for key in keys:
            for suffix, role in ((".lora_A.weight", "A"), (".lora_B.weight", "B"),
                                 (".lora_down.weight", "A"), (".lora_up.weight", "B")):
                if key.endswith(suffix):
                    prefixes.setdefault(key[:-len(suffix)], {})[role] = key
                    break
            else:
                if not key.endswith((".alpha", ".lora_alpha")):
                    raise ValueError("Unsupported LoRA state key: " + key)
        if not prefixes:
            raise ValueError("No LoRA A/B pairs in SafeTensors")
        for prefix, pair in prefixes.items():
            if set(pair) != {"A", "B"}:
                raise ValueError("Incomplete LoRA pair for " + prefix)
            # Common adapters add a wrapper prefix to module names.
            match = [name for name in model_modules if
                     prefix == name or prefix.endswith("." + name)]
            if len(match) != 1 or not match[0]:
                raise ValueError("Ambiguous or unknown LoRA target: " + prefix)
            module = model_modules[match[0]]
            if not isinstance(module, torch.nn.Linear):
                raise ValueError("Native LoRA target must be Linear: " + match[0])
            with safe_open(str(path), framework="pt", device="cpu") as src:
                a_shape = tuple(src.get_slice(pair["A"]).get_shape())
                b_shape = tuple(src.get_slice(pair["B"]).get_shape())
            if (len(a_shape) != 2 or len(b_shape) != 2 or
                a_shape[1] != module.in_features or
                b_shape[0] != module.out_features or
                b_shape[1] != a_shape[0]):
                raise ValueError("Incompatible LoRA rank/target " + prefix)
            rank = a_shape[0]

            def apply_lora(_module, inputs, output, _path=str(path),
                           _a=pair["A"], _b=pair["B"], _r=rank):
                from safetensors import safe_open
                with safe_open(_path, framework="pt", device="cpu") as src:
                    a = src.get_tensor(_a).to(inputs[0].device, inputs[0].dtype)
                    b = src.get_tensor(_b).to(inputs[0].device, inputs[0].dtype)
                return output + F.linear(F.linear(inputs[0], a), b) * (scale / _r)

            module.register_forward_hook(apply_lora)
            found += 1
    return found



def load_native_h3_components(pipe, denoiser_partition, *, device, fp8=False,
                              lora_files=(), video_vram_bytes=0):
    """Instantiate all four generation models on meta, then register direct hooks.

    Diffusers uses these models unchanged. H3/Qwen are BF16 by default;
    the two original VAEs stay FP32. Exactly one copy of their original
    SafeTensors checkpoint shards is retained in the HF model cache.
    """
    import torch
    from diffusers import (MiniMaxH3Transformer3DModel,
                           AutoencoderKLMiniMaxH3, AutoencoderKLMiniMaxH3Audio)
    from transformers import Qwen3VLForConditionalGeneration
    from studio_vdn_disk_loader import component_spec_source
    from studio_vdn_decoder import install_lowmem_decoders

    if denoiser_partition not in ("transformer", "transformer_ref"):
        raise ValueError("Unsupported H3 checkpoint partition")
    registry = {
        denoiser_partition: (MiniMaxH3Transformer3DModel, False, torch.bfloat16),
        "text_encoder": (Qwen3VLForConditionalGeneration, True, torch.bfloat16),
        "vae": (AutoencoderKLMiniMaxH3, False, torch.float32),
        "audio_vae": (AutoencoderKLMiniMaxH3Audio, False, torch.float32),
    }
    attached = {}
    info = {}
    for name, (cls, transformers_model, precision) in registry.items():
        repo, folder, revision = component_spec_source(pipe, name)
        store = OriginalShardStore.from_hub(repo, folder, revision)
        model = instantiate_meta_component(
            cls, repo, folder, revision, transformers_model=transformers_model,
            dtype=precision)
        hooked = NativeOriginalShardHooks(
            model, store, device=device,
            fp8=(bool(fp8) and name in (denoiser_partition, "text_encoder")),
            component=name)
        pipe.update_components(**{name: model})
        attached[name] = hooked
        info[name] = {"mappedParameters": len(hooked.weight_names),
                      "forwardModules": hooked.layers,
                      "originalShards": len(store.files),
                      "dtype": str(precision)}
    if lora_files:
        info["loraPairs"] = attach_native_lora(
            getattr(pipe, denoiser_partition), lora_files)
    install_lowmem_decoders(
        pipe.vae, pipe.audio_vae, free_vram=video_vram_bytes)
    # ModularPipeline.device normally reads the first parameter, which is
    # meta when no layer is executing. Keep pipeline state CUDA while each
    # individual parameter is directly sourced at its forward hook.
    cuda_device = torch.device(device)
    base = type(pipe)
    class NativeOriginalShardPipeline(base):
        @property
        def device(self):
            return cuda_device

        @property
        def _execution_device(self):
            return cuda_device
    pipe.__class__ = NativeOriginalShardPipeline
    # Hold references for job lifetime; hook closures also retain them.
    pipe._little_monkey_original_shard_hooks = attached
    print(json.dumps({"phase": "native_original_shards_ready",
                      "mode": "native_original_no_offload_copy",
                      "components": info, "fp8": bool(fp8)}),
          flush=True)
    return info

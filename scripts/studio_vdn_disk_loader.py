"""First-party low-RAM sharded SafeTensors loading plan for MiniMax H3 + Qwen.

No FreeVideo dependency. This chooses memory-bounded *initial* loading of
Diffusers and Transformers components using their Accelerate-backed
`from_pretrained(device_map=..., offload_state_dict=True)` paths.

Unlike apply_group_offloading (which runs AFTER all weights are loaded), this
avoids materializing the entire H3/Qwen pair in host RAM in the first place.
A disk device-map streams individual dispatched modules at their forward
boundaries. Weight shards are never loaded with torch.load/pickle.
"""
import json
from pathlib import Path
import re

GIB = 1024 ** 3
INDEX_NAMES = {
    "denoiser": "diffusion_pytorch_model.safetensors.index.json",
    "text_encoder": "model.safetensors.index.json",
}
_SHARD_RE = re.compile(r"^[a-zA-Z0-9_][a-zA-Z0-9_.-]*\.safetensors$")


def verify_safetensors_index(index, *, component, limit=100000):
    """Fail closed before model allocation; accepts only sharded SafeTensors."""
    if component not in INDEX_NAMES:
        raise ValueError("Unknown sharded model component")
    if not isinstance(index, dict) or not isinstance(index.get("weight_map"), dict):
        raise ValueError("A sharded SafeTensors weight map is required")
    mapping = index["weight_map"]
    if not 1 <= len(mapping) <= limit:
        raise ValueError("Empty or oversized SafeTensors weight map")
    names = set()
    for tensor, shard in mapping.items():
        if (not isinstance(tensor, str) or not tensor or len(tensor) > 512
                or not isinstance(shard, str) or not _SHARD_RE.fullmatch(shard)
                or "/" in shard or "\\" in shard or shard.startswith(".")):
            raise ValueError("Unsafe or invalid indexed SafeTensors entry")
        names.add(shard)
    if not 1 <= len(names) <= 64:
        raise ValueError("Unsupported shard count")
    return {"tensors": len(mapping), "shards": len(names),
            "files": sorted(names)}


def component_spec_source(pipe, name):
    """Honor Diffusers' actual model-index source (Qwen may be external)."""
    spec = getattr(pipe, "_component_specs", {}).get(name)
    if spec is None:
        raise ValueError("Requested H3/Qwen component is absent from pipeline")
    source = getattr(spec, "pretrained_model_name_or_path", None)
    folder = getattr(spec, "subfolder", None)
    revision = getattr(spec, "revision", None)
    if not isinstance(source, str) or not source.strip():
        raise ValueError("Native H3 model source is not pinned in the pipeline")
    if folder is not None and (
            not isinstance(folder, str) or folder.startswith("/")
            or ".." in Path(folder).parts or "\\" in folder):
        raise ValueError("Unsafe H3 model subfolder")
    return source, folder or "", revision


def verify_model_indexes(pipe, denoiser_partition):
    """Read small index JSONs only; never download weight blobs in probe."""
    from huggingface_hub import hf_hub_download
    report = {}
    for name, role in ((denoiser_partition, "denoiser"),
                       ("text_encoder", "text_encoder")):
        repo, folder, revision = component_spec_source(pipe, name)
        filename = INDEX_NAMES[role]
        result = hf_hub_download(repo, filename=filename,
                                 subfolder=folder or None, revision=revision)
        path = Path(result)
        if not path.is_file() or path.stat().st_size > 4 * 1024 * 1024:
            raise ValueError("Unsafe H3/Qwen SafeTensors index size")
        index = json.loads(path.read_text(encoding="utf-8"))
        report[name] = verify_safetensors_index(index, component=role)
    return report


def low_ram_disk_budget(free_cuda, free_ram, free_disk):
    """Reserve substantial CUDA memory for H3 activations and OS page cache.

    These are admission budgets, not evidence that an 8 GiB card actually
    completes a video. Disk staging needs the full BF16 components available.
    """
    if any(not isinstance(x, (int, float)) or x < 0
           for x in (free_cuda, free_ram, free_disk)):
        raise ValueError("Invalid H3 memory/disk budget")
    if free_cuda < 8 * GIB:
        raise ValueError("Native H3 disk streaming currently requires at least 8 GiB free CUDA VRAM")
    if free_ram < 12 * GIB:
        raise ValueError("Native H3 disk streaming currently requires 12 GiB available host RAM")
    if free_disk < 180 * GIB:
        raise ValueError("Native H3 disk streaming currently requires 180 GiB free scratch storage")
    # Each model has a separate disk offload index. Never budget full VRAM
    # separately to both: they stay loaded concurrently in one pipeline.
    cuda_per_component = int(min(2 * GIB, max(GIB, free_cuda * .18)))
    cpu_per_component = int(min(4 * GIB, max(2 * GIB, free_ram * .22)))
    return {"cuda_model_bytes": cuda_per_component,
            "cpu_model_bytes": cpu_per_component,
            "cuda_activation_reserve_bytes": int(free_cuda - 2 * cuda_per_component),
            "cpu_reserve_bytes": int(free_ram - 2 * cpu_per_component)}


def disk_component_kwargs(denoiser_partition, spill_root, budget):
    """Per-component arguments understood by native ModularPipeline loader.

    ModelMixin and Transformers consume these arguments when *reading*
    individual SafeTensors shards, before any group-offload hook is applied.
    """
    if denoiser_partition not in ("transformer", "transformer_ref"):
        raise ValueError("Unexpected MiniMax H3 denoiser partition")
    root = Path(spill_root)
    if not root.is_absolute():
        raise ValueError("Disk offload folder must be absolute")
    if root.exists() and (root.is_symlink() or not root.is_dir()):
        raise ValueError("Unsafe native offload folder")
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    memory = {0: int(budget["cuda_model_bytes"]),
              "cpu": int(budget["cpu_model_bytes"])}
    if min(memory.values()) < 512 * 1024**2:
        raise ValueError("Insufficient component dispatch memory")
    folders = {}
    for name in (denoiser_partition, "text_encoder"):
        directory = root / name
        if directory.exists() and (directory.is_symlink() or not directory.is_dir()):
            raise ValueError("Unsafe native offload component directory")
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        folders[name] = str(directory)
    return {
        "device_map": {denoiser_partition: "auto", "text_encoder": "auto"},
        "max_memory": {denoiser_partition: dict(memory),
                       "text_encoder": dict(memory)},
        "offload_folder": folders,
        "offload_state_dict": {denoiser_partition: True, "text_encoder": True},
        "low_cpu_mem_usage": {denoiser_partition: True, "text_encoder": True},
        "use_safetensors": {denoiser_partition: True, "text_encoder": True},
    }


def assert_disk_mapped_components(pipe, denoiser_partition):
    """Reject a fake low-memory path that actually kept all weights in RAM."""
    report = {}
    for name in (denoiser_partition, "text_encoder"):
        module = getattr(pipe, name, None)
        mapping = getattr(module, "hf_device_map", None)
        if not isinstance(mapping, dict) or not any(
                str(device) == "disk" for device in mapping.values()):
            raise RuntimeError(
                "Native %s weight loader did not dispatch modules to disk" % name)
        if not any(str(device) != "disk" for device in mapping.values()):
            raise RuntimeError(
                "Native %s has no CPU/CUDA execution anchor for disk hooks" % name)
        report[name] = {"moduleGroups": len(mapping),
                        "diskGroups": sum(str(v) == "disk" for v in mapping.values())}
    return report

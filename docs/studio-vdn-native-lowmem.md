# Native low-memory MiniMax-H3 execution in Little Monkey

This path belongs to Little Monkey and **does not invoke FreeVideo**. It runs in
the local Diffusers worker selected in Studio, behind the existing territory /
weight-license check in Rust.

## Implemented and connected to real inference

1. **Full-context SDPA tiling.** `studio_vdn_lowmem.py` replaces the native
   MiniMax-H3 attention processor with first-party streaming QKV projections
   by attention-head group, followed by query-token tiles. Every query retains
   all key and value rows across the *entire packed video/audio/text sequence*.
   Normalization, rotary positions, projection weights and output projection
   remain the installed H3 model's own modules. This is *not* local-window or
   sparse attention. Masks and unsupported tensor layouts fail over to the
   original processor without dropping the mask.
2. **Standard and TorchAO-style projections.** Ordinary `nn.Linear` QKV
   weights are sliced by output head before projection. Quantized/fused/custom
   modules with non-sliceable weights run on bounded token tiles and only
   retain the active head group; this can be significantly slower. Both paths
   are exact-context, not truncated-context approximations.
3. **Token-chunked FFN.** Each MiniMax-H3 `FeedForward` layer performs its
   own forward pass in bounded token groups, reducing intermediate SwiGLU/FF
   activations while preserving token-independent math.
4. **Placement and offloading.** Existing Diffusers native group-offloading
   streams denoiser and text-encoder weights from CPU or disk. Real measured
   free VRAM and host RAM now select layer grouping and asynchronous prefetch:
   small budgets avoid a second pinned-memory/VRAM transfer slot.
5. **Recovery and observability.** The Rust-owned worker passes low-memory
   settings to subprocesses. CUDA OOM triggers one stricter, smaller-tile
   attempt in a *fresh process* before moving to the next tier. Per-step
   sampler checkpoints include the effective tile settings, preventing an
   incompatible resume. Successful runs log peak CUDA allocated/reserved
   memory, Linux high-water RSS, whether disk streaming occurred, and whether
   native tiling was active.
6. **Studio.** `auto` tiles CPU/Disk offload plans, `force` also tiles
   resident-model runs, and `off` uses upstream reference attention. Query
   tokens / head count are bounded adaptive ceilings; 0 means auto. FFN
   tokens default to 256 (0 disables FFN tiling).

## How this is validated

- Native VDN contract suite: imports/compiles the exact concatenated Rust
  worker + native sampler + low-memory source; checks tile planning and
  transfer policy without CUDA.
- PyTorch CPU parity suite: checks ordinary and fused QKV, actual
  full-context attention against PyTorch SDPA across multiple chunk widths,
  custom quantized-style projections limited to token tiles, FFN numerical
  equivalence, and fail-closed mask handling.
- Full product CI: type-checks Studio, builds Rust on Windows/macOS/Linux.

## What is **not yet proven or included**

- Full model inference on a consumer NVIDIA GPU, target video quality or
  reproducibility against a real licensed checkpoint.
- 8 GiB VRAM / 16 GiB RAM production support. The existing Diffusers
  `load_components` weight loader still builds large H3/Qwen components
  before the disk/CPU offload hooks can stream them; chunking activations
  **does not remove that initial host-RAM peak**. Conservative memory
  admission is intentional, rather than claiming the model fits.
- A native, low-RAM, disk-backed *checkpoint loader* that materializes only
  individual blocks from sharded SafeTensors at initialization. This is the
  architectural feature needed to challenge FreeVideo's 16 GiB RAM result.
- Guaranteed speedups: smaller chunks trade memory for more kernel launches
  and may be slower, especially with quantized module projections.

The original MiniMax/OpenVDN checkpoint license restrictions still apply.
Model-weight inference must go through the application license check.

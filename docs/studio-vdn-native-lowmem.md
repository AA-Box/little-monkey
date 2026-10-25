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
4. **Primary disk tier: first-party original-shard streaming.** The
   `studio_vdn_native_shards.py` loader resolves original model indexes and
   constructs H3, Qwen, Video VAE and Audio VAE as meta modules. Each module
   reads just its required original SafeTensors weights during forward and
   returns to meta afterward. This **does not create Accelerate disk-copy
   files**. See the dedicated section below.
5. **Alternative disk/CPU placements.** The existing
   `--weight-loader accelerate` diagnostic mode still uses Accelerate
   shard dispatch for H3/Qwen; its disk copies and resource behavior differ.
   The ordinary CPU-resident tier uses Diffusers group-offload hooks.
   The 12 GiB available host RAM + 180 GiB scratch + 8 GiB free VRAM
   admission for direct disk streaming is only an experimental lower-bound
   check, **not** a verified end-to-end hardware requirement.
6. **Recovery and observability.** The Rust-owned worker passes low-memory
   settings to subprocesses. CUDA OOM triggers one stricter, smaller-tile
   attempt in a *fresh process* before moving to the next tier. Per-step
   sampler checkpoints include the effective tile settings, preventing an
   incompatible resume. Successful runs log peak CUDA allocated/reserved
   memory, Linux high-water RSS, whether disk streaming occurred, and whether
   native tiling was active.
7. **Studio.** `auto` tiles CPU/Disk offload plans, `force` also tiles
   resident-model runs, and `off` uses upstream reference attention. Query
   tokens / head count are bounded adaptive ceilings; 0 means auto. FFN
   tokens default to 256 (0 disables FFN tiling).

## New native original-shard runtime (stream_disk default)

The primary native `stream_disk` tier now **does not create Accelerate
offload .dat copies**. The first-party `studio_vdn_native_shards.py` loader:

- Reads the exact component source and subfolder from the live Diffusers
  component spec, including the Ref2VA `transformer_ref` partition.
- Downloads the original indexed SafeTensors shards into the regular HF cache
  and preserves their original bytes. A single SafeTensors file is also
  supported for smaller components. This must complete before inference.
- Instantiates **H3, Qwen3-VL, Video VAE and Audio VAE** as real model objects
  with meta parameters, then installs per-module pre/post-forward hydration
  and eviction hooks. Parameters are only materialized on the CUDA device
  during their own forward; an exception triggers the post-hook and releases
  them. Small nonpersistent RoPE buffers remain device-resident.
- Preserves mixed precision: H3's official FP32 patch/time/output/RoPE
  modules remain FP32 and the released VAE/audio weights retain FP32.
- Retains the official video VAE's temporal overlap/crossfade and enables
  smaller spatial tiles at constrained VRAM. Audio is encoded and decoded
  one mono channel at a time, without truncation or time-domain splitting.
- Supports direct original-shard inference-time LoRA A/B linear overlays
  (including optional per-pair alpha/rank) without merging or permanently
  modifying base weights. Invalid/unknown adapter formats fail closed.
- Provides **experimental** TorchAO FP8 weight-only conversion for supported
  CUDA capability 9.0+ Linear modules on hydration. It is never applied
  to Video/Audio VAE; absent kernels fail explicitly without BF16 fallback.
- The existing `--weight-loader accelerate` remains available for diagnostic
  comparison. It may materialize a second offload copy and does not promise
  the same peak-RAM behavior.

The model's original weights may still occupy significant storage in the
Hugging Face cache, and CUDA execution of a large operator can exceed VRAM
even if its weights are streamed individually. Actual supported hardware
requirements must be measured on the complete model.

### Tested behavior versus unverified behavior

- CI uses **actual CPU PyTorch** with original SafeTensors test shards to
  exercise true load/forward/meta-evict/repeat, exception cleanup, legacy
  audio `weight_norm` hook ordering, native LoRA numerical parity and
  stereo-audio operations. No synthetic `from_pretrained` eager-load
  shortcut is involved in the streaming fixture.
- End-to-end licensed 60+ GB Qwen/H3 inference and multi-frame GPU encoding
  have **not** been performed here. In particular FP8 TorchAO kernel support,
  VRAM peaks and LoRA on the original giant H3 layers remain to be proven
  on an eligible NVIDIA device; the existence of a functional CPU toy test
  does not establish 8 GiB VRAM / 16 GiB total system RAM production support.
- Direct hook coverage is intentionally strict. Missing, renamed or tied
  checkpoint weights, model-code paths that access a leaf parameter outside
  that leaf's `forward`, and unsupported LoRA formats fail rather than
  silently running a different model.

## How this is validated

- Native VDN contract suite: compiles the exact concatenated Rust worker,
  sampler, low-memory code and shard-loader; checks tier admission, SafeTensors
  weight-map handling, source mapping, disk dispatch requirements and transfer
  policy without CUDA.
- Accelerate CPU acceptance: builds real sharded SafeTensors checkpoint files
  for a small actual PyTorch network, initializes a meta model, performs a
  real `load_checkpoint_and_dispatch` (part CPU, part disk), runs two forwards,
  compares to eager output and verifies the offloaded layer is evicted.
- PyTorch CPU parity suite: checks ordinary and fused QKV, actual
  full-context attention against PyTorch SDPA across multiple chunk widths,
  custom quantized-style projections limited to token tiles, FFN numerical
  equivalence, and fail-closed mask handling.
- Full product CI: type-checks Studio, builds Rust on Windows/macOS/Linux.

## What is **not yet proven or included**

- Full model inference on a consumer NVIDIA GPU, target video quality or
  reproducibility against a real licensed checkpoint.
- **8 GiB VRAM / 16 GiB RAM production support is not established.**
  Direct original SafeTensors loading avoids building all four components
  in RAM first, but large individual operators and temporary activations may
  still cause OOM. Full-model CUDA acceptance is required for actual peak
  memory and output validation. The 12 GiB check means *available* RAM.
- CUDA FP8 TorchAO with the complete giant H3/Qwen model, and LoRA adapter
  parity on the real checkpoint, remain to be validated on a licensed GPU.
  Unsupported LoRA schemas and configurations fail closed.
- Direct module hooks assume weights are accessed through the hooked
  module's forward, not through unhooked direct parameter access; such
  model-code paths need real-model integration tests.
- Guaranteed speedups: smaller chunks trade memory for more kernel launches
  and may be slower, especially with quantized module projections.

The original MiniMax/OpenVDN checkpoint license restrictions still apply.
Model-weight inference must go through the application license check.

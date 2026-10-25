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
4. **Initial disk-mapped SafeTensors loading for both H3 and Qwen.**
   For the native `stream_disk` tier, `studio_vdn_disk_loader.py` reads each
   component's small shard index from the *actual* Diffusers component spec
   (OpenVDN H3 is under `h3-base/transformer`; Qwen may resolve to the
   separate `MiniMaxAI/MiniMax-H3/text_encoder` source). It verifies that
   indexed weights are SafeTensors rather than pickle and passes
   `device_map="auto"`, separate bounded `max_memory`, per-component
   `offload_folder`, `low_cpu_mem_usage=True`, and
   `offload_state_dict=True` to the models' original
   `from_pretrained` loaders. **These options act during shard loading,**
   before the full model is resident. Accelerate can offload most modules to
   disk and materialize them as their forward hooks run. The app **checks
   both H3 and Qwen have real disk-dispatched modules** before sampling.
   Accelerate's disk format may create extra cache files; this is not yet a
   zero-copy original-SafeTensors mmap engine.
5. **Other placements.** Existing Diffusers group-offload hooks still
   stream CPU-resident weights for non-disk tiers. Disk-dispatched modules
   retain Accelerate's hooks; applying group hooks a second time is
   prohibited. Available VRAM/RAM governs prefetch and block grouping.
   Streaming with 12 GiB *available* host RAM and 180 GiB scratch space is
   admitted for a trial with at least 8 GiB *free* CUDA VRAM; this is not a
   guarantee that a whole video will finish or fit.
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
  The loader no longer requires full BF16 H3 and Qwen residency at initial
  load in the disk tier, but Accelerate may materialize individual shards,
  temporary weights and execution buffers. Full model CUDA acceptance is
  still required to measure process RSS, true peak allocation and output.
  The 12 GiB minimum refers to *currently available* host RAM, not installed.
- The new loader dispatches standard Hugging Face shards into Accelerate's
  disk offload storage. A fully first-party zero-copy loader that reads
  exact per-block tensor slices directly from the *original* SafeTensors
  files during each forward remains future work. No training/quantization
  changes are implied by this PR.
- FP8 and LoRA injection into disk-mapped layers are not yet validated and
  fail explicitly rather than silently reverting to eager loading.
- Guaranteed speedups: smaller chunks trade memory for more kernel launches
  and may be slower, especially with quantized module projections.

The original MiniMax/OpenVDN checkpoint license restrictions still apply.
Model-weight inference must go through the application license check.

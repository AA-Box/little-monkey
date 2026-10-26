# Adaptive model-load execution plan (development)

The Runtime Hub already contains a deterministic `LocalOffloadPlanner` that estimates context,
accelerator residency, batch and concurrency from a hardware snapshot. Previously that
calculation was advisory: the actual M3 `load_model` path passed the saved runtime settings
to the backend unchanged. This change connects the same planner to real managed
`llama.cpp` loads.

## Production path

1. `M3RuntimeHub::load_model` verifies the selected installed model and saved
   runtime configuration, including speculative-decoding gates.
2. It takes a **fresh** hardware snapshot and combines it with the installed
   model's weight size, estimated runtime footprints and verified projector
   metadata. Live available memory already includes existing residents; their
   footprints are not counted twice.
3. `LocalOffloadPlanner::plan` returns a per-load offload plan.
4. `adaptive_llama_settings` converts only trustworthy, applicable decisions
   into supported llama.cpp settings:
   - It may **reduce an implicit** 4096-token context when the current estimated
     budget is tighter. A user-configured context is never overwritten.
   - For discrete accelerators, it may reduce **implicit** GPU layer count
     when the GGUF itself contains a nonzero, bounded `<architecture>.block_count`.
     The number of layers is never inferred from weights alone.
   - It does **not** automatically change Metal's GPU layers (CPU and GPU share
     physical memory), nor inject unsupported runtime settings into MLX/Ollama.
   - A load replacing a resident model leaves defaults untouched: the outgoing
     model's memory cannot yet be treated as available until it actually exits.
5. The runtime validates the **effective** settings again and calls its existing
   `M3RuntimeDriver::load` method with them. The stored settings remain unchanged.

The public `plan_model_load` method resolves the same plan without starting
the model. Production loading and previews use the same implementation.

## Limitations and correctness boundaries

This is *model-load adaptation*, not an in-flight token or kernel scheduler.
The declared model footprint is an **estimate**, and it is not a measured
proof of available memory or successful inference. No automatic speculative
model switch, KV-cache quantization, user setting override, or action replay is
performed. This feature does not turn MLX or Ollama into engines that accept
llama.cpp offload flags.

A failed hardware probe leaves backend defaults in force. GGUF metadata parsing
errors likewise cannot substitute a guessed layer count. An explicitly selected
context or layer count is never silently reduced.

## Automated verification

- `cargo test adaptive_load_` in `src-tauri` validates implicit-vs-explicit
  setting precedence, a verified block count, unavailable block metadata,
  resident replacement handling, and Metal's unified-memory boundary.
- `cargo test sniffs_actual_architecture_block_count` and
  `cargo test ignores_zero_gguf_block_count` validate the binary GGUF
  metadata reader used to decide whether automatic GPU layer selection is safe.
- The existing Runtime Hub and runtime adapter suites must continue to pass.
- Hardware validation requires separate physical tests with an actual installed
  GGUF model, checking the resulting llama-server argv, GPU residency, successful
  inference and resource peaks on a GPU-constrained host. CI that does not run
  that hardware cannot claim this acceptance test passed.

The follow-on measured calibration and OOM-recovery PRs must remain separate,
as requested. Do not enable automatic tuning from incomplete or failed
generation samples.

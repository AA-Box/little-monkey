# Verified runtime benchmark history (PR 2)

The existing **Benchmark** panel runs real streamed model generations. This change makes each saved result distinguishable and refuses to preserve fictitious successes.

## Evidence captured for each run

All these fields are computed in Rust, not accepted from the WebView:

- **Workload SHA-256**: normalized prompt, maximum output tokens, repeat count, and warm-up policy. Different workloads must not be compared.
- **Runtime-reported model digest and byte size**: collected before and after sampling; any change aborts the run. Some runtimes do not expose digests; absence is displayed as unknown, never synthesized from a filename.
- **Runtime process ID**: sampled after warm-up (lazy servers can start during warm-up); if a process changes while the counted measurements run, the benchmark is discarded.
- **Configured-settings SHA-256**: captures the app's requested configuration, not actual backend command-line flags. The configuration must remain unchanged during sampling. This hash is useful for distinguishing runs but must not be mistaken for a runtime-executed placement guarantee.
- **Count of completed, non-warm-up samples with real token usage**: refuses to save a run with none.
- **Stable machine identity**: OS, architecture, CPU count, RAM, accelerator names. Entries from another machine remain hidden by the existing UI freshness guard.

Memory readings are `process_usage` high-water marks, **not** a live sampled VRAM peak or a model-specific allocation. Only a mark that rose in the measured window may be labelled as a run peak. The UI states when only a lifetime upper bound is available.

## Safety and persistence

Every completed run is appended newest-first, capped at 128 records. The existing `benchmarks.json` file remains backward compatible: entries saved by previous versions deserialize with `evidence: null` and are visibly marked as legacy. Writes are guarded by a shared command mutex, written to a sibling temporary file and renamed. A persistence failure is surfaced rather than silently claiming durable success.

A generation that emits partial text without `ResponseCompleted` is recorded as failed, not counted in throughput or TTFT statistics. A changed runtime model, process, configured settings or machine identity aborts the mixed measurement.

## Validation

Run `cargo test benchmark::tests` and `cargo test m3_commands::tests`, plus the full repository CI suite, on a runner with the required Rust/Tauri dependencies. For real hardware acceptance, benchmark a pinned GGUF/MLX/Ollama model twice on the same host; confirm two retained records with matching workload/config/model hashes, process consistency, accurate token usage, and correct stale-machine behavior. Then change the model/config and verify the fingerprint change; induce mid-stream termination and verify it cannot be saved as a verified run.

**Limits:** This PR establishes measurement provenance and history, not automatic closed-loop planner calibration, GPU peak monitoring, a cryptographic attestation by the model server, or physical multi-device qualification. Those must not be inferred from the existence of records.

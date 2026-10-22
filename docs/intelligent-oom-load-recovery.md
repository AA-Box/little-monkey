# PR 3 — guarded managed llama.cpp OOM load recovery

## Actual execution boundary

The Runtime Hub performs a real managed llama.cpp `load_model_with_plan` call. When the launcher reports an **owned process exiting before readiness** and its bounded log tail carries a recognized allocator OOM diagnostic, the hub may attempt up to **two** lower-memory re-launches (three loads total). This is not a generic substring match against arbitrary generation errors.

1. Recompute lower *implicit* llama.cpp context reservations, never overwriting an explicit `context_size`. The minimum suggested fallback is 512 tokens.
2. Reduce implicit `gpu_layers` only with confirmed GPU offload and a locally verified GGUF block count. Never guess layer count, never alter explicitly set `gpu_layers`.
3. Before each retry, check the live driver status: no running models and a `stopped` or `error` lifecycle. Also check unchanged user config and installed model identity, and cancellation. An ambiguous status ends retries.
4. Do not retry anything other than model-load startup; never replay inference, tool actions, embeddings, or tasks. Do not evict unrelated running models or switch to a different model without user approval.
5. Serialize load/unload hub mutations during a retry sequence so other hub load/unload calls cannot interleave.

## Evidence

`m3_runtime_load_model` returns the actual effective settings and an ordered `recoveryAttempts` array. After recovery, the original offload estimate is deliberately cleared, because it no longer represents the executed lower-memory placement. A bounded history of 64 records is atomically stored in the app-private `state/load-recovery-history.json`, viewable in Runtime Hub's llama.cpp model load section and via `m3_runtime_load_recovery_history`. Records contain model and runtime IDs, timestamps, numeric context/GPU-layer controls, outcome, and recovery status. No prompts, inference output, raw logs or credentials are stored in this new file.

## Test/acceptance plan

- Rust pure-policy tests: reject timeouts/unproven process deaths, decrease settings monotonically, respect explicit overrides, fail closed without verified GGUF layer count.
- Rust integration: injected allocator-confirmed failures produce exactly the expected sequence of real driver load invocations and persistent history; ambiguous live process and changed config forbid a retry; non-OOM failures cause no retries.
- Repo CI: Rust format/lint/tests and UI typechecks/tests.
- **Physical acceptance still required:** on an installed managed llama.cpp GGUF model with genuine constrained RAM/VRAM, capture startup logs showing an actual allocator OOM, verify retries use progressively smaller `-c`/`-ngl` argv and produce a working generation after successful load. An intentionally insufficient physical machine might not recover even on the third attempt. Do not claim physical GPU acceptance without those observations.

### Deliberate non-goals

No transparent downgrade of user-selected models, daemon-wide speculative unloading, automatic retry of already-started LLM streams or side-effecting tools, or global reduction of explicitly configured settings. Those would violate user authority or exactly-once execution.

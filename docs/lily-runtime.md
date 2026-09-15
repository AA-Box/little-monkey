# Lily acceleration inside the managed MLX runtime

Little Monkey uses Perplexity's Lily as an **optional acceleration engine inside the signed managed MLX runtime**. There is no custom provider, API-key sentinel, manually started bridge, or user-PATH Lily executable.

The runtime package pins and builds Lily from Perplexity `pplx-garden` commit `1ed972ed3f0bd5616c997c9507c25616c63394fc`, includes the resulting executable in the signed package manifest, and launches only that verified package copy.

## Product path

```text
Little Monkey model selection
        ↓
M3 managed MLX runtime
        ↓
signed runtime_router.py
        ├─ ordinary Apple Silicon/model → mlx_server.py
        │
        └─ M5+ + macOS 26+ + Lily Qwen3.6 layout
                    ↓
             lily_managed.py
                    ↓
          signed packaged Lily binary
```

The app still sees one managed MLX runtime. Install/update, model registration, start, stop, cancellation, process supervision, usage accounting, and run-ledger persistence all use the existing M3 production path.

## Eligibility

Lily is selected conservatively. The router requires all of the following before trying it:

- macOS 26 or newer;
- Apple Silicon with an `Apple M5` generation or newer chip reported by `system_profiler`;
- the signed package contains an executable `bin/lily`;
- the model config identifies the Qwen3.6 MoE family and MLX affine 4-bit/group-64 layout expected by Lily.

Lily itself then performs its stricter architecture/weight validation before becoming ready. Hosts such as M1–M4 use normal MLX automatically.

## No capability regression

Lily is intentionally narrower than Little Monkey's Qwen3.6 support: it accepts greedy text chat but not image input, tools, sampling, or structured output. `lily_managed.py` checks each production request.

A Lily-compatible request is sent to Lily and its response is translated directly into Little Monkey's private `MlxStreamEvent` protocol. Lily's reported `prompt_tokens_details.cached_tokens` becomes `cached_input_tokens` in the terminal MLX event.

If a request needs an unsupported Lily capability, the managed adapter terminates Lily, starts the normal signed `mlx_server.py` for the same model, waits for that service to become ready, and forwards the original request unchanged. The process then remains on MLX for that load. This avoids simultaneously keeping two copies of the 35B model resident while preserving vision/tools/sampling behavior.

## Prefix-cache accounting

There are two real cache paths:

- Lily owns its two-entry strict-prefix decode-state cache when Lily is active.
- The managed text MLX service owns its separate two-entry strict-prefix cache when normal `mlx_lm` is active.

M3 supplies a bounded deterministic `promptCacheKey` as a cache preference. Neither engine trusts the key as proof of cache compatibility: token-prefix equality remains authoritative.

The terminal private MLX event carries `cached_input_tokens`. `ProductionMlxServiceController` propagates that value into `MlxGenerationSummary`, and the canonical M3 sink records it as `CanonicalUsage.cached_input_tokens`, so run-ledger/usage telemetry sees measured reuse rather than a service-only log line.

Vision generation remains uncached in the normal MLX VLM path until equivalent image/token decode-state reuse can be proven.

## Supply chain

`scripts/build-mlx-package.mjs`:

1. creates the private Python runtime;
2. installs exact `mlx`, `mlx-lm`, and `mlx-vlm` versions;
3. runs `pip check` and verifies those direct versions did not drift;
4. fetches exactly the pinned `pplx-garden` commit and verifies `FETCH_HEAD` equals that SHA;
5. builds Lily with its locked Cargo graph/toolchain;
6. includes Lily, its LICENSE/NOTICE, the router, adapters, and MLX services in the package manifest;
7. signs the complete runtime tree through the existing managed-runtime signing path.

The app therefore never downloads or executes an unverified Lily binary at runtime.

## Verification

Hardware-free production-contract checks:

```sh
python3 packaging/mlx/service/test_mlx_server.py
python3 packaging/mlx/service/test_lily_managed.py
```

The managed-Lily test launches a fake Lily child through the real adapter, proves cached-token evidence crosses the private service protocol, then submits an unsupported tool request and proves the adapter transitions to the normal MLX fallback service.

The real model smoke remains available on Apple Silicon:

```sh
LITTLE_MONKEY_MLX_E2E_MODEL=/path/to/model \
  python3 packaging/mlx/service/test_mlx_model_e2e.py
```

Actual Lily Metal inference requires an M5-or-newer host and the exact supported Qwen3.6 checkpoint. CI that lacks that hardware can prove selection, lifecycle, protocol, fallback, signing/package composition, and canonical usage wiring, but must not claim an M5 Metal inference run occurred.

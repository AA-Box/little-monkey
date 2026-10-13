# Real Apple Silicon MLX training, fused serving and rollback

This acceptance is not an ordinary CI unit test. It trains local weights on
physical Apple Silicon, applies MLX-LM held-out gates, fuses a new checkpoint,
launches the production MLX Python service from packaging/mlx/service/mlx_server.py,
and executes real HTTP generation.

The previous model must answer a native canary before training. After
training, every fused shard and configuration digest is checked against the
quality receipt. The fused checkpoint is served by the same Python service
shipped with Little Monkey and must produce real SSE output and token counts.
The harness then starts the fused candidate again, injects a failure after
successful live generation, stops it, restores the previous model, and requires
a successful new inference canary. No preexisting desktop processes or model
registrations are touched.

## Operator-owned hardware run

Requirements: macOS arm64, Node 20+, compatible mlx-lm[train] Python, enough
Unified Memory and disk, a reviewed LOCAL MLX base model, operator-reviewed
separate train/valid/test JSONL, and a NEW absolute output directory.
Quantized fusion failures and unsupported models must fail rather than pass.

    node scripts/swarm-mlx-training-deployment-rollback.mjs \
      --model /absolute/local/MLX-model \
      --data /absolute/private/three-splits \
      --output /absolute/new/acceptance-run \
      --python /absolute/venv/bin/python \
      --iters 200 --max-test-ppl 10 \
      --previous-model /absolute/known-good/MLX-model \
      --confirm-disposable-native-switch

Alternatively, register a trusted self-hosted GitHub Actions runner labeled
self-hosted, macOS, ARM64, mlx-live-acceptance and manually dispatch the
Apple Silicon MLX real lifecycle workflow on a reviewed branch.
Pushes and PRs never trigger this expensive process. The workflow publishes
only sanitized metadata; training records, weights and private logs stay local.

The candidate directory retains training-receipt.json and the terminal
native-lifecycle-acceptance.json. A pass is ONLY
status = verified_native_service_lifecycle after all real stages including
the injected fault and restoration. A failed canary, missing model, changed
shard, incomplete training, or incomplete rollback returns nonzero.

## Contract tests without MLX hardware

    node --test scripts/swarm-mlx-lora.contract.test.mjs \
      scripts/swarm-mlx-training-deployment-rollback.contract.test.mjs

## Verification boundary

This exercises an isolated instance of the actual production MLX inference
service, NOT a running desktop/Tauri UI session. Existing desktop promotion
and rollback code has separate simulated-runtime tests. Actual GUI import,
activation and crash-recovery handoff are NOT proven by this acceptance.
Neither are physical run results established until a genuine macOS arm64 run
produces the evidence file. Do not claim all-device end-to-end verification
from contract tests or an unexecuted workflow.

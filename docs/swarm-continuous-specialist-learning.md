# Continuous, independently verified specialist routing

An explicitly operator-enabled learning-to-deployment loop for real Ollama-backed native CLI tasks. It does not fine-tune weights, hot-swap desktop MLX, override desktop model preferences, or grant tools.

## Policy

Create a local directory and a private JSON policy using canonical absolute paths:

```json
{
  "version": 1,
  "monkeyBin": "/usr/local/bin/monkey",
  "stateDir": "/Users/me/Library/Application Support/LittleMonkey/specialist-learning",
  "targets": [
    "ollama:local-reviewed-model-a",
    "ollama:local-reviewed-model-b"
  ],
  "domains": [
    "security",
    "backend"
  ],
  "repetitions": 2,
  "intervalSeconds": 86400,
  "timeoutMs": 600000,
  "minAccuracy": 0.8,
  "minImprovement": 0.05,
  "maxAgeHours": 48
}
```

Run a real independent grading cycle:

```sh
node scripts/swarm-specialist-continuous-loop.mjs --policy /absolute/path/policy.json --confirm-unattended-local-evaluation --once
```

Remove `--once` to keep running under an operator-owned service supervisor such as launchd. A lock prevents concurrent cycles. A dead worker PID is automatically reclaimed with a quarantine rename; a live or unverified owner always blocks, rather than risking concurrent model runs.

Dispatch a real CLI recipe through the verified active domain:

```sh
node scripts/swarm-specialist-continuous-loop.mjs --policy /absolute/path/policy.json --confirm-unattended-local-evaluation --dispatch --domain security --recipe /absolute/path/recipe.json
```

The dispatcher privately copies the original native recipe, replaces the already-configured Ollama target, appends a bounded fixed reviewer-focus hint chosen from the independently ranked specialist roles, and runs the real `monkey task run` executable. It preserves the original task text and system rules and never changes tool access, workspace or permission mode; `bypass` is refused.

## Install unattended current-user service on macOS

A supported Apple Silicon or Intel macOS login user can enable the LaunchAgent
after reviewing the exact policy and configured local CLI/model targets:

```sh
node scripts/swarm-specialist-service.mjs install --policy /absolute/path/policy.json --confirm-install-service
node scripts/swarm-specialist-service.mjs status
node scripts/swarm-specialist-service.mjs uninstall
```

The installer writes a user-only `com.littlemonkey.verified-specialist-learning`
LaunchAgent with `RunAtLoad`, bounded launchd restarts, no root privileges,
no shell command interpretation and private logs under the configured state
directory. The existing PID lease prevents concurrent learning cycles and
recovers after a verified dead owner. A bootstrapping failure removes the
generated service file. It does not bypass task authorization or start on
machines where it was not deliberately installed.

## Quality and rollback

Each candidate is independently checked on three source fixtures (two defects and a negative control) for all three fixed specialist roles and every repetition. The benchmark executor checks native run receipts, executable source behavior and unchanged repositories. The controller rejects missing/duplicate/contradictory results, verifies complete coverage, recomputes ranks, and records per-domain evidence hashes.

Promotion requires `minAccuracy` and an observed `minImprovement` versus a still-eligible current target. On quality regression, the controller chooses a freshly reverified qualifying target; if none pass, it disables automatic routing for that domain. No older result alone can authorize a rollback. An atomic active manifest is the deployment handoff consumed by the dispatcher.

This is local operator-owned execution, not cryptographic or remote attestation. The dispatcher integrates with the native **CLI** task route, not the desktop UI or a model-weight serving process. It cannot claim Apple Silicon MLX training, background desktop hot-swap, or two-device acceptance.

## Tests and live acceptance

```sh
node --check scripts/swarm-specialist-continuous-loop.mjs
node --test scripts/swarm-specialist-continuous-loop.contract.test.mjs
node scripts/swarm-specialist-live-benchmark.mjs --self-check --domain all
```

The deterministic contract tests stub the model process; they establish validation, promotion, rollback and native dispatch behavior, not live model quality. Run `--once` with configured real Ollama models and inspect the native run receipts and source-oracle outcomes to establish real-model acceptance. Verify service restart, model loss, damaged evidence, malformed policy, repeated regression and CLI task errors on the intended host.
## Unattended Apple Silicon MLX training-to-verified-local-serving

The opt-in `scripts/swarm-mlx-verified-serving.mjs` supervisor bridges approved-batch MLX LoRA training, held-out evaluation, fused-weight hashing and real inference acceptance to a stable local OpenAI-compatible model endpoint. It runs the independent native CLI source oracle against each candidate *before* activating the production route. It is independent of desktop/Tauri model selection.

Create a private serving state directory distinct from the training inbox and output tree. Use the existing `swarm-mlx-auto-learning.mjs` learning policy with `fuseForImport: true`, and configure:

```json
{
  "version": 1,
  "trainingPolicy": "/Users/me/private/approved-mlx-learning-policy.json",
  "monkeyBin": "/Users/me/bin/monkey",
  "stateDir": "/Users/me/private/mlx-serving-state",
  "listenPort": 8765,
  "backendPort": 8766,
  "probePort": 8767,
  "modelAlias": "little-monkey-mlx",
  "domains": ["security", "backend"],
  "repetitions": 2,
  "timeoutMs": 600000,
  "checkSeconds": 3600,
  "minAccuracy": 0.8,
  "minImprovement": 0.05
}
```

`backendPort` must be exactly `8766`. Unlike the operator-facing `listenPort` and the temporary grading `probePort`, the MLX backend destination is pinned in code to `127.0.0.1:8766`. This prevents a tampered policy file from directing model inference traffic to arbitrary local services. The supervisor refuses to start if this port is already occupied.

Start on an operator-owned Apple Silicon Mac with a verified native CLI and installed MLX-LM Python environment:

```sh
node scripts/swarm-mlx-verified-serving.mjs --policy /absolute/path/serving-policy.json --confirm-unattended-mlx-deployment
```

Use the stable model in a native CLI recipe with:

```json
{ "local_url": "http://127.0.0.1:8765", "model": "little-monkey-mlx" }
```

To include the served MLX model in specialist learning, add the string `local-url:http://127.0.0.1:8765|little-monkey-mlx` to the `targets` array in the specialist controller's policy. The controller rejects arbitrary remote addresses for this target type.

The supervisor verifies candidate weights and runs a fresh source-oracle benchmark through a separate local-only probe port. It returns HTTP 503 when no independently passing candidate is active or while switching. Real candidate regressions trigger independent regrading of the prior model before rollback. Failed deployment attempts cannot repeat unattended after a crash; the operator must explicitly review an attempt marker.

**Verification boundary:** the test suite verifies subprocess wiring, safety gates and rollback contracts using stand-ins. Apple Silicon MLX training, actual fused serving and tool-call compatibility are **not** demonstrated by CI. If `mlx_lm.server` does not support the native CLI's tools for a fixture, source-oracle grading fails and promotion is blocked. This integration does not automatically activate the Tauri desktop's resident MLX model and is not a production-hardened remote API.

To make this operator-approved supervisor survive login/restart, install its
current-user macOS LaunchAgent (never root, no shell, no inherited provider secrets):

```sh
node scripts/swarm-mlx-service.mjs install --policy /absolute/path/serving-policy.json --confirm-install-service
node scripts/swarm-mlx-service.mjs status
node scripts/swarm-mlx-service.mjs uninstall
```

The agent starts automatically with the user session and is restarted by
`launchd` if the supervised process fails. State ownership and pre-existing
backend ports are checked before resuming; an unknown orphan process is
not killed merely to get the service running. The selected live backend must
report the exact model identity, and a new model must pass real generation
and independent native-source grading before publication.

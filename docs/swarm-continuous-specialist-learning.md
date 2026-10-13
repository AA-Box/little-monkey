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
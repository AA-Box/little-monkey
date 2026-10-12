# Native team coordination, learning, packs and mission telemetry

Little Monkey implements its own permission-controlled agent team runtime. The
interfaces below operate on first-party workflow, session and daemon records
and never add an external orchestration framework.

## Team topologies and failure boundaries

Specify `topology` on a read-only local workflow or analysis goal:

| Mode | Dispatch | Communication and leader recovery |
| --- | --- | --- |
| `phased` (default) | existing bounded independent agents per phase | previous-phase reports |
| `star` | initial coordinator, followed by other agents | successful leader report is relayed as untrusted source text |
| `hierarchical` | lead then bounded successive worker tiers | reports from earlier tiers are relayed, never as tool commands |
| `ring` | single successive agent per wave | each successor receives bounded prior successful reports |
| `mesh` | independent bounded simultaneous agents | findings are reconciled by existing downstream validation/consensus phases, not repeated file edits |
| `adaptive` | highest observed reliability first; failed coordinators replaced sequentially; after the first successful coordinator remaining workers run together | each replacement is a different already-authorized read-only agent, successful coordinator evidence is forwarded without replaying any completed work |

A failed initial read-only coordinator is **not re-executed** automatically. Adaptive teams dynamically change from single-coordinator election to parallel worker execution after a successful report, or continue electing replacements after failures.
Subsequent waves continue without the failed report, which permits an
eligible worker to synthesize and challenge sources. There is no privileged
leader token. Prompts relay bounded JSON-quoted **untrusted** reports and
never grant a new tool, model, workspace or peer. Because agent writes cannot
be made exactly-once with model-controlled retries, all non-default
intra-team topologies are rejected for code agents, worktrees and remote
peers. Write-capable and remote execution continue to use frozen, phase/
dependency-driven daemon DAGs, checked receipts and operator approval.

A goal tool call may use:

```json
{"goal":"Investigate API access correctness","mode":"analyze",
 "topology":"adaptive","consensus":"strict"}
```

## Four-stage evaluated learning

1. **Retrieve:** With `learning:true` in a local read-only analysis goal,
   the runner retrieves only operator-approved, unexpired patterns of the
   **same domain**, wrapped as untrusted historical reference. The configured
   Knowledge 2.0 stack and versioned notebook can also retrieve explicitly
   selected source-attributed records.
2. **Judge:** A real installed model is evaluated on executable source
   fixtures, including safe negative controls, by independent Node oracles.
   Completion status and model self-report are not correctness scores.
3. **Distill:** A source-verified benchmark with at least six independently
   correct observations and 80% aggregate accuracy can stage a bounded
   reviewer-selection candidate. The candidate stays **pending**.
4. **Consolidate:** An operator approves, rejects or forgets the candidate
   in Settings. A new pending revision never overwrites an older approved
   revision. Reimporting identical measured evidence is idempotent; revision
   numbers advance monotonically for genuinely new measurements. Stored patterns expire after 90 days; there is a 50-pattern,
   24-KiB cap and no raw prompts, tokens, credentials or source blobs.
   At capacity, rejected and then pending records may be evicted, but approved
   records are never silently displaced.

This is **non-parametric, review-gated learning**; it does not claim to have
performed GPU fine-tuning or to have trained LoRA weights. Such an adapter
requires a separately configured training backend, held-out evaluation
and an explicit deployment process. Neither a learning approval nor a
reviewer ranking ever modifies model weights or runtime permissions.

```json
{"goal":"Review service performance","mode":"analyze",
 "topology":"ring","learning":true}
```

The learning retrieval flag is refused on modifying or remotely placed
agents, because historical findings must not cross peer trust boundaries.

## Operator-reviewed JSON capability pack registry

The Swarms panel can import local JSON-only packs with schema version 1, an
ASCII id, title and description and up to four phases of four agents (twelve
agents maximum). The JSON manifest contains **only** phase names and
bounded agent description/prompt fields; no executable modules, tools, peer
placements, code agents, worktrees or hooks. Unknown fields fail closed.

Example:

```json
{"schema":1,"id":"api-readiness","title":"API readiness",
 "description":"Read-only API reliability review",
 "phases":[{"title":"Inspection","agents":[
   {"description":"API reviewer","prompt":"Inspect API contracts and cite source evidence."}
 ]}]}
```

The operator must confirm an import, then separately prepare and send the
workflow draft through ordinary agent authorization. Imported data is not
signed, certified or trusted; review instructions before dispatch. The local
registry has an eight-pack / 48-KB cap, supports removal and does not
install third-party code. MCP, executable hooks and signed extensions retain
separate explicit authorization.

## Native capability packs and extensions

First-party packs are static, bounded WorkflowSpec declarations under
`src/lib/swarmPresets.ts`, including frontend audit, backend resilience,
performance budgets, knowledge evidence, integration readiness and release
assurance, alongside the two security packs. Their normal lifecycle is
**select → inspect → prepare an editable chat draft → send under existing
tool/agent permissions**. Choosing a pack never downloads or executes
model-authored code. The application also supports operator-managed,
allowlisted MCP server lifecycle, import-reviewed executable command hooks,
and its signed extension marketplace / Wasm SDK. These integrations are
separate permission boundaries, not blanket compatibility guarantees for
every third-party plugin.

## Mission observability, attribution and optimization

Settings → Swarms displays actual session-journal outcomes (per-agent
success, failure, reuse, topology, duration) and installed daemon graphs.
It includes measured prompt/completion/total tokens **with coverage counts** so missing
usage does not masquerade as a zero. There is no assumed provider billing:
`cost` is *unavailable* unless an actual bill/rate is supplied elsewhere.
An explicitly entered USD-per-million-token value produces both a historical
**observed-token cost estimate** and a future projection. The estimate
attributes measured tokens to individual mission rows; it excludes reused
checkpoints from the new-inference denominator and labels partial measurement
coverage. It is not a bill or actual per-model invoicing; the session journal
does not currently retain authoritative provider-billed model/token classes. The four-agent token forecast requires
three measured agent samples, displays its measurement coverage, and shows
whether an operator-specified **advisory** token budget is exceeded. This
forecast does not replace existing runtime/process resource enforcement.

Initial subagent routing decisions (chosen model, matching policy and explanatory
reason) now persist in each session's bounded task metadata, are validated on
hydration and appear on the mission's expandable trace. Missing data on
older, reused or remotely placed agents are explicitly unavailable. This is
**dispatch intent**, not independently verified final model selection or a
provider billing statement.

The existing Resource Ledger is the detailed source of observed process,
CPU/GPU, I/O and scheduler decisions. Mission aggregates are read-only;
telemetry failures do not stop agents or modify placement privileges.

## Verification and release gates

`src/lib/swarmCoordination.test.ts`,
`src/lib/swarmLearningCycle.test.ts`,
`src/lib/swarmMissionDashboard.test.ts`,
`src/lib/swarmPresets.test.ts` and workflow runner tests cover topology
order, intra-wave evidence relay, approval and versioned learning, exact
pack allowlists and unknown-vs-measured token behavior. Full CI still needs
to pass on all platforms.

The operator-only real-model and two-installation acceptance gates in
`docs/swarm-release-acceptance.md` remain necessary for live peer loss,
remote cancellation, replicated memory, daemon restart, and actual
repository changes. Pure logic or local contract tests do not constitute
that real-device evidence. Adaptive topology is local-only; leader election
across untrusted machines, elastic mesh membership and gradient-based
fine-tuning are **not** represented as completed here.


### Bounded independent reviewer voting policies

`consensus: strict | majority | supermajority | unanimous` selects a read-only vote threshold for source-citing claims. Strict keeps two independent reviewers. The other policies run five separately tasked reviewers and require respectively three, four or five agreeing votes. Any explicit rejection means **disputed**, even if supporters reach quorum. Missing/malformed votes count against the planned threshold, not for it. The result is non-authoritative reviewer agreement, never a proof of correctness or a Raft/Paxos/BFT distributed protocol. No policy grants writes or overrides ordinary approvals.


### Opt-in quality-constrained model cost optimizer

The Swarms panel can independently enable model-cost optimization after
operator-approved source-fixture quality ranking. Inside a pre-existing
routing policy's **already eligible and unpinned** subagent candidates, the
optimizer requires at least six independently graded fixtures per model,
at least 80% accuracy, within two percentage points of the top model,
explicit input/output rates for both contenders, and at least three real
recorded input/output usage samples. It applies one shared measured token
mix to each model rate, choosing a cheaper equivalent only when the
estimated difference is at least 10%. Otherwise it returns the exact
original routing order; missing prices/quality/usage never become zero.
It never changes the active chat model, authorization, egress, or explicit
model pins. These are configured-rate **estimates**, not actual provider
invoices, and model-output quality is task dependent. Existing budget
enforcement and permission checks remain independently authoritative.


### Per-model mission cost attribution

The local subagent journal now records its canonical model target key alongside
the bounded routing explanation. The mission dashboard joins that key to the
existing, **operator-configured** per-model input and output rates and each
agent's measured prompt and completion tokens. New dispatches have a separate
known-priced coverage count. Reused checkpoint agents are excluded entirely
from new-dispatch cost accounting, while unknown token measurements and missing
model rates remain explicitly unknown. Older persisted routing records without
a model key remain valid but unpriced. This is a user-rate **estimate**;
provider invoice values, cached-token discounts and remote agent costs are
not asserted or fabricated.


### Daemon-resident read-only failover

A resident JSON graph may specify an explicit `fallback_recipe` absolute
path on an independent root node (no predecessor dependencies or injected
reports). Registration validates and freezes BOTH recipe files, refuses
self-fallback and requires **plan/read-only permission mode on both**. On
primary daemon job `Failed`, the resident scheduler changes the durable
SQLite node cursor to `pending_fallback`, then queues the alternate under
a separate deterministic id. It survives daemon restarts and cannot replay
the original job. The resulting successful fallback can satisfy downstream
dependencies, whose reports are still verified through the existing
hash-chained ledger. `Cancelled` and `NeedsReconciliation` are terminal
failures, **not** eligible for fallback; a second failed alternate fails
the graph. Operator cancellation covers both submission stages.

In the Swarms panel, enabling the optional resident fallback prompts for
a second, explicitly selected PLAN recipe using a different configured model
and the identical workspace; the compiler generates separate fallback
recipes only for independent first-phase read-only agents. Write-capable
resident swarms do not enable this option. An ordinary import of a JSON
graph can also supply preapproved `fallback_recipe` paths explicitly.
This is durable failover of a frozen **local** recipe, not automatic
cross-machine leader election or a way to replay external side effects.


#### Verifying real resident fallback on an installed daemon

Create a disposable read-only graph with an independent root whose primary
PLAN recipe is expected to fail and a different preapproved PLAN fallback
recipe that can succeed. After registration, **before the primary fails**:

```bash
node scripts/swarm-resident-fallback-live-acceptance.mjs \
  --phase capture --graph swarm-test --root p0a0 \
  --checkpoint /tmp/little-monkey-fallback-receipt.json --confirm-disposable
```

After the daemon runs the alternate to terminal graph success:

```bash
node scripts/swarm-resident-fallback-live-acceptance.mjs \
  --phase verify --graph swarm-test --root p0a0 \
  --checkpoint /tmp/little-monkey-fallback-receipt.json --confirm-disposable
```

This verifies two distinct deterministic durable job receipts and observed
terminal completion through the **installed** CLI. The offline `--self-check`
only verifies the receipt derivation and is **not** real acceptance. Do not use
the ordinary no-replay restart harness for graphs with fallbacks: these graphs
intentionally replace a failed primary receipt with one separately authorized
fallback receipt. Keep live operator evidence separately.


### Operator-run MLX LoRA/QLoRA adapter training (Apple Silicon)

A first-party local-only adapter-training script is available at
`scripts/swarm-mlx-lora.mjs`. It invokes the real installed
`python3 -m mlx_lm lora` training and held-out test runners, rather
than producing synthetic weights. It requires a pre-downloaded local MLX base
model and separately curated `train.jsonl`, `valid.jsonl` and
`test.jsonl` with disjoint records. It refuses to overwrite old adapters,
generates new safetensors weights/configuration, records file digests and
held-out perplexity, and requires an operator-specified acceptance ceiling.
Existing adapters can be used for continued training using MLX resume weights,
but only if a distinct **historical regression** test set is supplied. Both
old and new adapters are evaluated on that set; new weights fail if their test
perplexity worsens beyond the explicit percentage threshold (5% default).
This is a real measurable **regression-gated consolidation**, not EWC++.
It never promotes a new adapter without explicit runtime configuration, nor
claims source correctness solely from perplexity. Training logs may contain
private dataset/model details; they remain mode-0600 local files.

```bash
node scripts/swarm-mlx-lora.mjs \
  --model /absolute/local/mlx-base-model \
  --data /absolute/private/split-dataset \
  --output /absolute/private/new-adapter-run \
  --iters 200 --max-test-ppl 6.0 --validate-only

node scripts/swarm-mlx-lora.mjs \
  --model /absolute/local/mlx-base-model \
  --data /absolute/private/split-dataset \
  --output /absolute/private/new-adapter-run \
  --iters 200 --max-test-ppl 6.0 --confirm-local-training
```

For consecutive training, additionally pass
`--previous-adapter /absolute/approved/adapter` and
`--regression-data /absolute/independent/old-evaluation-data`.
The script requires macOS/ARM64 for actual execution and
`pip install 'mlx-lm[train]'` in the selected Python. The output is
`adapter/adapters.safetensors`, `adapter/adapter_config.json` and a
local receipt; **the active model is not silently switched**. Hardware/model
acceptance has not been executed by ordinary CI; CI only runs input/command
contract tests. Real adapter loading and quality should be independently
verified in the chosen runtime before an operator promotes it.


### Staged, operator-pinned Ruflo MCP interoperability

The Swarms panel offers an **inert** Ruflo MCP registration helper. Enter
an exact reviewed npm version; it creates an existing native MCP stdio
entry using `npx -y ruflo@VERSION mcp start`, but sets
`enabled: false` and `tool_allowlist: []`, never `null`. It does
**not launch the external process**, download/install packages, grant any
tool access, or install Claude host mods/hooks. The operator must review
the package, configure a limited allowlist and explicitly enable/connect
it in ordinary MCP Settings. This makes the documented Ruflo MCP tool
server callable through the existing Little Monkey MCP authorization path
when configured; it is not Ruflo-native mod or plugin compatibility and
does not import untrusted arbitrary scripts.


#### Optional fusion for the existing Little Monkey MLX runtime

Add `--fuse-for-import --confirm-fusion` to the **real** training command
after selecting the held-out perplexity ceiling. Only after the new adapter
passes the held-out and prior-task regression gates does the runner invoke
`python -m mlx_lm fuse --model BASE --adapter-path ADAPTER --save-path OUTPUT/fused-model`
to create a separate model directory. Fusion success requires an actual
`config.json` and `.safetensors` files; the receipt records its configuration
hash. It never overwrites the original base, installs the fused result or
changes the currently running model. To activate, explicitly import the
fused-model directory using Little Monkey's existing local MLX model import
and independently test representative model responses. A fused model can
behave differently from the unmerged adapter: an adapter's successful test
does **not** certify the fused result's accuracy without an additional live
runtime acceptance check.

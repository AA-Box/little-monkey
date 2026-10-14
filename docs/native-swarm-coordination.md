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


### One-shot opt-in cross-peer read-only failover

A phased read-only explorer may specify `peer: "node-a"`,
`peerCandidates: ["node-a","node-b"]`, and `peerFailover: true`.
After completing or reattaching the original durable peer receipt, a **second**
paired, task-granted and online/unknown-status peer may receive the **same**
read-only prompt with a **distinct deterministic correlation**, at most once,
ONLY if the original exact remote message was independently confirmed to be
terminal `failed`. Resumed runs search every selected peer's durable receipt
history for both stages, so completed alternatives are not resent. Network
uncertainty, timeouts, cancellation, rejection, history saturation, revoked
permissions and unconfirmed failure block migration. The receiving machine's
own recipe, tool and workspace policy remain authoritative. This does not
migrate processes, perform distributed leader election, or provide a
quorum-consensus protocol; physical two-device validation is still required.


### Safe fused-model promotion into the native model catalog

The Swarms panel now offers **Review and import evaluated MLX fusion**.
After the Apple Silicon runner trains/fuses a new model, select its local
`training-receipt.json`. The frontend requires the exact
`validated_fused_model_requires_manual_runtime_import` terminal state,
non-traversing absolute fused-model path, finite held-out and prior-task
regression scores, and a matching SHA-256 of the **actual local
fused-model/config.json** bytes. It then asks for an additional operator
confirmation before invoking the existing Rust `models_add_external_folder`
import route, which retains its independent native format/path checks.
No provider credentials are involved. Neither import nor training starts
or changes the active model. The operator must manually select the new
model and run real inference/quality regression checks; reverting to the
previous model remains possible because training never overwrites the
original weights. Training receipts are **not signed provenance**, so do
not import untrusted directories just because a JSON file says they passed.


### Fused safetensors provenance and integrity

The local MLX trainer now SHA-256 hashes **every safetensors shard** before
and after evaluating fused weights. Its success receipt includes each shard's
exact filename, byte length and digest. The Swarms import action requires
operator approval to verify this manifest. Native Rust streams every shard
on a blocking worker, compares the complete expected and present shard sets,
rejects symlinks and tampering and then permits the existing model catalog
import without automatically activating the model. This closes the former
config-only verification hole. These checks validate bytes, not real-world
model accuracy or cryptographic author identity; real inference is still an
explicit deployment gate.


### Atomic native verification-to-registration boundary

The UI does not accept a frontend-only digest or perform a separate native
verify followed by a separate import anymore. Its explicit confirmation now
calls `models_import_verified_fusion`. This streams and verifies both the
actual `config.json` and **all** fused safetensors in the operator-selected
folder, then immediately passes the same selected folder to the normal native
MLX-capable model registrar. The confirmation cannot activate a model. This
narrows (but cannot cryptographically eliminate) filesystem mutation races
between reading an external directory and later serving it for inference.


### Authenticated cross-machine reviewer quorum

Opt-in `authenticated_peer_voting: true` on an explicit `workflow` with
`consensus`, a penultimate read-only reviewer phase and a subsequent
verification phase. Every reviewer must name a different **pinned** paired
peer; candidate-pool failover is forbidden for voters. Before dispatch,
the frontend reads actual pairing records, requires the task grant and distinct
certificate fingerprints and device identities. Immediately before
reconciliation or proposing notebook claims it checks that the same peer
identities and grants remain valid; revoked/re-paired voters invalidate the
quorum. Votes are attributed to certificate fingerprint rather than editable
agent description or alias, with existing contradiction and evidence rules.
This is a bounded signed-peer reviewer vote, **not** Raft, Paxos, BFT, or
cryptographic proof of model correctness. It cannot by itself certify
physical installation independence; two-machine acceptance still applies.


### Real external Ruflo MCP discovery conformance

The safe Ruflo MCP bridge is a disabled, empty-allowlist registration by
default. To check **actual version-pinned** Ruflo interoperability on an
operator-controlled machine (without invoking tools or enabling access):

```sh
node scripts/swarm-ruflo-mcp-acceptance.mjs \\
  --binary /absolute/path/to/npx \\
  --ruflo-version 3.10.1 \\
  --args-json '["-y","ruflo@3.10.1","mcp","start"]' \\
  --confirm-external-process
```

The probe runs the chosen executable with no shell, without inheriting API
keys/tokens, negotiates real MCP JSON-RPC over standard input/output and
fetches the complete, unique bounded `tools/list`. It never calls `tools/call`,
connects an enabled server, alters policies, installs hooks, imports modules,
or grants a tool. An operator must approve execution of any third-party
package, and may instead point `--binary` at an already reviewed local tool.
The controlled subprocess test is in mandatory frontend CI; only a **live**
run against installed Ruflo demonstrates interoperability. Listing external
MCP tools is **not full binary Ruflo plugin, agent, worker or hook parity**.


### Real-model activation and automatic rollback

After verifying the complete training receipt, config SHA-256 and **all**
model weight shards using the Rust-native registrar, the Swarms panel offers
`Activate verified fusion · real inference + rollback`. This does actual
runtime work, not a simulated test: it switches the local MLX service to the
new model, then asks the registered, model-path-fenced loopback inference
endpoint for a **non-empty assistant completion**. The Rust canary requires
a bounded successful answer from the exact active candidate and rejects a
model switch or service failure during inference. A failed check restores
the previously running MLX model, or stops the candidate if no prior runtime
was running. A rollback failure **preserves a durable recovery journal** and
blocks normal Chat/agent target resolution until the operator opens Swarm
settings and the previous model can be restored. Nothing automatically
switches cloud providers or widens peer/tool permissions. Held-out MLX
perplexity and source-backed real-model benchmark results remain independent
of the availability canary; one generic completion does not certify those.


### Signed-peer cluster leader leases for resident read-only DAGs

Cluster mode is opt-in. It uses real TLS-pinned, HMAC-authenticated peer
requests, explicit task grants and a strict majority of durable votes.
Every node must have an operator-configured roster with exactly the same
set of **real server certificate fingerprints** and its own corresponding
inbound pairing-device IDs and outbound aliases. Enrollment never grants
peer permissions.

Example shape to author on EACH machine (replace the illustrative
fingerprint strings with ACTUAL 64-hex-character SHA-256 fingerprints):

~~~json
{
  "version": 1,
  "group": "research-cluster",
  "members": [
    {
      "fingerprint": "the-real-local-server-TLS-SHA256-fingerprint",
      "device_id": "@local",
      "alias": null
    },
    {
      "fingerprint": "the-real-remote-server-TLS-SHA256-fingerprint",
      "device_id": "the-real-inbound-pairing-device-id",
      "alias": "an-existing-outbound-pairing-alias"
    }
  ]
}
~~~

The fingerprint placeholders above are NOT valid hashes: enter genuine
64-character lowercase SHA-256. Cluster IDs, TLS identities, aliases and
device IDs are never inferred from untrusted model output. Configure,
query and optionally elect on each installed node:

~~~sh
monkey peers cluster-setup /absolute/path/to/roster.json
monkey peers cluster-status research-cluster
monkey peers cluster-elect research-cluster
~~~

An operator-enrolled resident graph includes a cluster_id property with
value research-cluster in its version-1 JSON. Every clustered recipe is
frozen and strictly read-only plan mode: neither edits nor arbitrary
deployments can be replayed by a newly elected leader. A native coordinator
heartbeat may schedule new jobs ONLY when this installation has a live
quorum certificate. A 20-second exclusive voting term is fenced by a
5-second local safety margin. The resident daemon attempts renewal or
higher-term re-election after expiry using ONLY the pinned roster.
Disagreement on roster SHA-256, changed certificate, revoked task grant,
overlapping vote, unknown identity or minority network partition cannot
establish a valid majority.

Run actual signed quorum acceptance on two or more independently installed,
pre-paired machines:

~~~sh
node scripts/swarm-cluster-live-acceptance.mjs \
  --group research-cluster --min-voters 2 --confirm-operator-cluster
~~~

The script checks signed native election responses and an independently
read back current local scheduling lease. Its acceptance does not stand
in for a real model run or a physical failure-injection exercise.

Each cluster node keeps its own locally submitted frozen DAG. New leaders
can resume pending **read-only computation** by scheduling their own graph
copy after the old quorum expires. State and output are NOT yet replicated
across host SQLite databases; previously completed remote model computation
may be repeated after failover. No automatic cluster membership changes,
Raft/BFT replicated logs or exactly-once external side effects are claimed.


### Ruflo-compatible lifecycle hook execution (bounded subset)

The built-in native user-hook engine now supports `PreCommand`,
`PostCommand`, `PreEdit`, `PostEdit`, `PreTask`, `PostTask`, and
`SessionEnd` in addition to its previous four events. Pre-command and
pre-edit hooks use the same native shell executor, 10-second bound,
operator-configured matcher and explicit deny decision as `PreToolUse`.
Post-operation, subagent-start/finish and explicit session-deletion hooks
run through that existing operator-owned executable path. They are not
prompt-injected scripts: user-authored hook configuration is required.

The safe external command-hook importer accepts only structurally
supported command events, rejects silent metadata changes and does not
pretend that arbitrary Ruflo plugin SDK hooks, background workers,
plugins installing npm packages, or intercepting native daemon tasks
are implemented. The Ruflo MCP bridge still needs explicit operator
registration, version pin and per-tool allowance before real use.


### Operator-opted automatic Apple Silicon adapter learning

The continuous watcher is a real local MLX-LoRA training worker, not a
simulated "learned" rank update. It requires the operator to launch it
on Apple Silicon and grants no network, peer, file-edit or plugin authority.

Create a protected local policy JSON with EXACTLY these fields (substitute
your reviewed absolute local paths):

~~~json
{
 "version":1,
 "model":"/your-local-mlx-base",
 "inbox":"/private/approved-dataset-batches",
 "outputRoot":"/private/new-model-candidates",
 "python":"/absolute/path/to/reviewed-mlx-python",
 "iters":200,
 "maxTestPpl":15,
 "pollSeconds":300,
 "batchLimit":1,
 "fuseForImport":true
}
~~~

For each approved subfolder such as inbox/operator-batch-1 place disjoint
train.jsonl, valid.jsonl and test.jsonl files, along with a ready.json:

~~~json
{
 "version":1,
 "approved":true,
 "note":"I reviewed the private dataset and test evidence",
 "files":{"train":"SHA256_OF_TRAIN","valid":"SHA256_OF_VALID","test":"SHA256_OF_TEST"}
}
~~~

Real SHA-256 digests are required; the strings above are placeholders,
not valid proofs. The worker recomputes hashes, applies the original
MLX preflight and isolated byte-for-byte dataset snapshots, invokes
actual mlx_lm LoRA training, runs the complete held-out and optional
fused-model quality checks, and records a permanent claim BEFORE any GPU
subprocess. It cannot silently repeat a batch after crash/failure; explicit
operator review and claim reset is necessary.

~~~sh
node scripts/swarm-mlx-auto-learning.mjs \
  --policy /absolute/path/to/learning-policy.json \
  --confirm-auto-training
~~~

Add --once to process only the current approved batch queue. A worker lock
rejects concurrent local training. **This worker produces verified
candidates only**: it never registers, activates or removes a runtime model.
The already implemented Swarms panel native SHA-256 importer and
actual-inference promotion/rollback remain the separate safety authority.
No unattended model rollout, and no test result from a stub, is claimed.


The desktop Swarms panel also includes **Signed multi-node leader quorum**
controls. An operator can select a local reviewed roster JSON, inspect
the actual enrolled group and lease, and explicitly request a signed
election. The button does not invent quorum state: it calls the native
peer-election CLI through a fixed Tauri command and then rereads
`cluster-status`. It explicitly warns that a successful majority may
start previously operator-approved read-only resident graph tasks. The
same bounds, pinned identities and native rejection policies apply
whether run through the GUI or CLI.


## P1 ambient maintenance discovery through the installed daemon

The opt-in `ambient-discovery` worker uses the existing durable native
swarm-worker trigger to inspect a configured workspace and suggest up to
five maintenance tasks with source evidence, uncertainty, impact, and
verification ideas. Nothing is automatically edited or queued from the
suggestions. The worker inherits the source recipe's preconfigured model,
workspace, timeout and read-only `plan` permission.

Install a reviewed plan-only JSON task recipe with a working model target:

```sh
monkey daemon ensure
monkey daemon swarm-worker-catalog
monkey daemon swarm-worker-install my-ambient \
  --role ambient-discovery \
  --source-recipe /absolute/path/to/plan-only-recipe.json \
  --workspace /absolute/path/to/repository \
  --cron "0 */6 * * *"
```

The daemon freezes the task recipe and applies its existing backpressure,
kill switch, persisted run ledger and watchdog. Each run is bounded to
at most 900 seconds and 12 model iterations. Inspect the daemon's run
history for suggested work, then explicitly authorize any resulting task.

This is proactive, scheduled codebase discovery, **not** automatic
cross-session semantic memory, resource-adaptive wake scheduling,
or unattended execution of newly proposed code changes.

## P1 verified headless concurrency benchmarks

```sh
pnpm test:swarm-headless:contract
pnpm test:swarm-headless:live -- --target ollama:YOUR_MODEL \
  --workers 1,2,4 --repetitions 2 --timeout-ms 600000
```

The live benchmark submits concurrent real installed `monkey task start`
runs in separate, disposable repositories where an external Node oracle
is known to fail before intervention. It waits for each model-run task,
executes the oracle, checks that only the authorized source changed and
reports each task's elapsed duration plus wall-clock verified throughput.
A failed run sends a cancellation request; its repository is retained
for reconciliation if the run might still be active. No self-reported
model success is scored as correctness.

The contract test uses an explicitly separate mock CLI executable and
**does not establish real-model performance**. This harness does not
measure CPU utilization, PSS, or establish a JCode performance comparison. It samples the installed daemon's PID and descendant process-tree resident RSS when available, recording a per-tier baseline, peak and growth. Shared pages may be counted twice; missing daemon/sampling permissions produce explicit null metrics instead of fabricated memory values.


## Live dynamic agent delegation and direct team mail

Desktop subagents can use the native `agent_spawn`, `agent_send`,
`agent_inbox` and `agent_join` model tools while running. These are
available only inside the live agent loop; a model cannot inject arbitrary
tools or skip normal permissions. Turn on **Subagents** before using them.

- `agent_spawn({description,prompt,profile})` creates a live child with
  its own unique task id, model loop and process entry. The parent continues
  working while the child executes. A child can delegate further.
- `agent_inbox({after})` returns active peers and bounded messages to the
  caller only. `agent_send({to,text})` delivers a message to an active
  member within the same session/run team, and it enters that agent's next
  model iteration as JSON-quoted, explicitly untrusted evidence.
- `agent_join({id})` awaits a directly spawned child and reports its
  final answer. Parent cancellation flows to children; unfinished children
  are cancelled when their parent finishes.

Runtime-created teams are bounded to depth four, 24 total agents and six
simultaneously active agents; an explore parent can spawn explore only.
Code children keep native workspace, checkpoint, permission, routing and
tool access constraints and never inherit broader capabilities. Nested
worktree children remain in the parent's already-isolated root.
There is no unlimited, unmetered agent creation.

The direct-message mailbox is kept in bounded profile-local app storage
(maximum 100 messages, 2000 characters per message, seven-day retention).
Messages survive app restart for inspection, but **a currently running
model cannot resume the same arbitrary in-flight tool call exactly once**.
The runtime registry only marks active processes during that process's
lifetime. These desktop tools do not claim native cross-host live delivery.

## Adaptive cross-session ambient agents

Unlike fixed-cadence `swarm-worker-install`, adaptive ambient runs are
owned by the daemon's normal tick and examine the profile's durable
completed run ledger across prior sessions, including after daemon restart.
They produce analysis proposals only, not unattended implementation. Run:

```sh
monkey daemon ensure
monkey daemon swarm-ambient-install project-scout \
  --source-recipe /absolute/path/to/review-plan-only-recipe.json \
  --workspace /absolute/path/to/repository
monkey daemon swarm-ambient-status
monkey daemon swarm-ambient-disable project-scout
```

By default, ambient context contains run IDs, terminal states and timestamps,
not task titles. To explicitly include up to twelve prior run titles, add
`--include-history-titles` at install time. The installer refuses that
flag for non-local model targets to prevent unreviewed transmission of
previous sessions to an external provider.

The scheduler derives a stable fingerprint from up to twelve previous
terminal run records. It initially wakes promptly, rechecks after new
history, uses 15 minutes as its minimum cadence on change and doubles
its delay on inactivity or failure, capped at 24 hours. It never
schedules an ambient run solely because its own output appeared in the
ledger. A frozen plan-only task recipe, immutable run snapshot and
deterministic job identity ensure crash reconciliation reattaches instead
of automatically duplicating a possibly-running task.

Ambient observations are source evidence, never a permission grant.
Existing daemon kill switches, resource admission, queue backpressure
and model budgets continue to apply. Changes require explicit user
authorization through normal task controls.

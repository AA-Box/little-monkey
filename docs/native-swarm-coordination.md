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

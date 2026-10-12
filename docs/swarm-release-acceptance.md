# Native swarm release acceptance

This document intentionally distinguishes **implemented code**, **automated
verification**, and **live acceptance**. An agent profile or a passing mocked
test is not proof that any arbitrary model can call tools or that two user
installations are reachable. Do not mark the full system production-ready until every required live gate
has reproducible evidence.

## Feature coverage

| Requested capability | Implemented in this branch | Not yet proven or delivered |
| --- | --- | --- |
| Multi-agent planning and execution | Existing bounded native workflow runner, 15-agent security preset, staged goal workflow | Live tool-capable model completing a real project run |
| Adaptive specialist selection | Bounded completion/speed observations; supplemental specialist rotation; real-model verified-fixture benchmark, strict import validation, optional accuracy ranking within existing model-routing policy's eligible subagent candidates | Live benchmark not yet executed on configured real models; three toy fixtures are not sufficient to claim broad model quality or multi-agent consensus accuracy |
| Task dependencies and recovery | Checked DAG frontiers, fail-fast, per-agent persisted checkpoints, explicit same-session manual resume with exact prompt matching | Unattended daemon recovery across power loss; exactly-once mutations |
| Security swarm | 15-agent read-only audit, distinct permission-gated edit/test workflow | Verified real repository vulnerability-to-fix scenario using a model |
| Shared writable memory | Approved claim notebook and separate durable per-namespace vector store with independent authenticated writer revisions, tombstones, model/dimension pinning, cosine queries, signed pinned-TLS replication and opt-in 60-second daemon sync | Automatic embedding of every agent turn without approval, true cross-node production acceptance, multi-device load/conflict stress tests |
| Distributed agents | Signed peer transport; restricted cancellation of an exact submitted peer task via pairing/thread/message, durable cancel request; receipt-safe retries | Confirmed remote process termination under real two-device load, work stealing or synchronized repository state |
| Autonomous execution | Daemon-resident SQLite-backed DAG coordinator added; frozen operator-selected recipes dispatch through ordinary supervised daemon jobs with deterministic IDs and manual/plan permissions; CLI/UI lifecycle | Unattended recovery of arbitrary interactive edit swarms, model-on-device acceptance |
| Evaluated self-learning | Successful swarm cards link their ledger IDs to existing Rust learning eligibility and candidate capture; native evaluation/promotion policies remain authoritative | Automatic collection/evaluation of swarm-specific outcomes and autonomous candidate promotion |
| Agent collaboration | Sequential stage reports, two structured independent reviewers and explicit contradiction / insufficient-evidence outcomes | Cryptographically independent verification, more complex consensus protocols |
| MCP connectors and command hooks | Existing operator-configured MCP lifecycle and narrow tool allowlists; reviewed import of four supported local command-hook events with rejection of unknown policy, timeout and async options | Arbitrary external plugin formats and unsupported lifecycle events are not automatically executable |
| User experience | Agent Swarms settings panel, draft-to-chat review, peer pool, notebook approvals, run status, MCP server lifecycle view | One-click/background launch of arbitrary swarm DAGs and rich task-level trace visualizer |

**Terminology:** a model's reviewer vote is not a verified test. A notebook
revision is deliberately **not** an indexed embedding in Knowledge 2.0. Peer
`explore` placement does not override the receiving node's configured recipe;
configure that recipe read-only to enforce the actual remote tool boundary.

## Paired multi-writer vector memory

Vector replication now imports each signed peer snapshot in **one SQLite
transaction**. If one record has a conflicting revision, incompatible embedding
space, malformed content or exceeds capacity, nothing from that snapshot
changes locally. Imported writer identities derive from the peer's pinned
certificate fingerprint rather than its editable alias, so re-pairing a new
installation with an old alias cannot overwrite the previous writer's history.
Certificate rotation creates a separate writer stream, not an implicit grant
to rewrite existing revisions.

The protocol now supports JSON-encoded **2,048-dimensional** finite f32
embeddings (up to 64 KiB per signed record), including the actual signed
TLS write/query/tombstone regression test. There is no silent truncation.
The remote namespace ACL and live peer capabilities are still enforced on
each request.


Memory is **disabled for other installations by default**. Both a peer
capability and a **local namespace-specific ACL** are required. The same
fixed secure paired-peer transport used for tasks carries the records; the
sender cannot specify a network URL or bypass a receiver's controls.

1. Pair A and B using the existing documented peer pairing flow. On **both**
   machines, grant the inbound pairing `memory-read,memory-write` in Peer
   Settings (or `monkey peers grant <inbound-device-id> --allow memory-read,memory-write`).
   Those grants alone do **not** disclose any namespace.
2. On **both** machines, share the exact namespace with the **inbound**
   paired device, after confirming its identity:

   ```bash
   monkey peers memory-share <inbound-device-id> project-alpha --read --write
   ```

3. Prefer automatically producing document and query vectors via a **locally
   configured Knowledge 2.0 stack**, with the exact embedding backend,
   prefixes, model and dimension shared by both nodes:

   ```bash
   monkey peers memory-add-text project-alpha finding-1 my-stack-id 'Verified evidence on A' --revision 1
   monkey peers memory-query-text project-alpha my-stack-id 'Verified evidence' --top-k 5
   ```

   The embedding operation calls the already-configured model backend and
   refuses missing or incompatible runtime dimensions. A SHA-256 fingerprint
   of the whole embedding specification pins the namespace; no human needs to
   paste arbitrary floating-point vectors.

   Alternatively, if you produce a genuine vector externally, store it
   manually using the same model identifier/dimension on both installations:

   ```bash
   monkey peers memory-put project-alpha finding-1 embed-local-v1 \
     '0.25,0.5,0.75' 'Example evidence, from A' --revision 1
   ```

4. From each node, sync its outbound alias to the other:

   ```bash
   monkey peers memory-sync node-b project-alpha
   monkey peers memory-search project-alpha embed-local-v1 '0.25,0.5,0.75' --top-k 5
   ```

5. To automatically retry every 60 seconds while the OS daemon is running,
   opt in explicitly on each node:

   ```bash
   monkey peers memory-watch node-b project-alpha
   monkey peers memory-watch node-b project-alpha --disable
   ```

All four controls are also in **Settings → Background Agents → Agent swarms →
Replicated vector memory**.
An independently reviewed, **approved** notebook claim also has an
**Embed approved revision** control. The operator first chooses a Knowledge
2.0 embedding stack, target namespace and a monotonically increasing vector
revision. Its evidence locations are previewed and require explicit
confirmation before being embedded in the local vector store; any peer
replication is separately granted/scheduled. Pending and rejected model
proposals cannot use this export path. After rolling back a notebook
revision, publish it with a *newer* vector revision: remote vector history
never permits rewinding an author's numeric version.

 The namespace stores at most **128** distinct
`(author,record ID)` records, with up to **2,048** vector dimensions and
**2,048** text bytes per record. Revisions are strictly monotonically
increasing **per authenticated author and record ID**. Older versions and
same-version conflicting payloads fail; exact retries are idempotent.
A deletion is a higher-revision tombstone retained through replication.

Only locally authored records are exported. Data arriving from another peer
is never re-exported under the current node's identity. Concurrent edits by
different authors coexist under separate author identities; there is **no
automatic semantic merge of conflicting claims**. Model/dimension identity is
checked and mismatches fail closed. The manual vector above is only an example; **`memory-add-text` and
`memory-query-text` do generate real vectors** through the selected existing
Knowledge embedding stack. No model service is bundled or started implicitly
by these commands. Full validation with two real operator installations
remains outstanding.

### Memory security validation

Verify: a pairing with no read/write capability is refused, a pairing with
capability but no **namespace ACL** is refused, a revoked pairing cannot read,
cross-namespace access fails, inconsistent embedding spaces fail, and duplicate
revisions do not cause duplicate records. The Rust `peer_e2e` and
`peer_memory` tests exercise these decisions with separate state directories;
the mandatory Linux loopback TLS test additionally checks **actual signed
memory writes, same-revision idempotence and conflict rejection, a
namespace-specific 403, cosine search, and tombstone readback** through the
production network listener. This is stronger than an in-process API unit
test, but the live two-installation test remains required.

## Resident daemon swarm coordinator

This is **a separate, unattended executor** for explicitly configured frozen
recipe DAGs; it does not retry mutating chat agents invisibly.

1. Create local JSON recipes for each task using existing Little Monkey task
   recipe format, with an absolute workspace and `permission_mode: "manual"`
   or `"plan"` (no unattended `acceptEdits`). The installed daemon reads
   a frozen snapshot and respects ordinary tool approval gates.
2. Copy [the example graph](examples/resident-swarm-graph.json) and replace
   recipe paths with absolute paths to your own local files.
3. Run `monkey daemon ensure --json`, then
   `monkey daemon swarm-submit /absolute/path/graph.json`.
   The same operation is available under **Settings → Background Agents →
   Agent swarms → Resident autonomous recipe graphs**.
4. Inspect `monkey daemon swarm-status --id project-audit-001`. After the
   resident service restarts, the graph and its per-node daemon job IDs survive.
   A dependency only unlocks after the preceding job is actually succeeded.
5. To request cancellation, run
   `monkey daemon swarm-cancel project-audit-001` and poll status until
   terminal. Cancellation may not retract side effects that already occurred.

New graph IDs must be unique. Exactly-once *submission* uses deterministic
daemon job IDs. A crash between enqueuing a job and recording its graph-node
cursor is repaired by reattaching to the existing job instead of submitting
work twice; a persisted SQLite regression test exercises this precise crash
window. Exactly-once **external mutation** is still not guaranteed.

## Real-model accuracy benchmark

A repeatable live benchmark now creates disposable Git workspaces containing
three deterministic bugs, invokes configured real `monkey task` models,
verifies fixes with Node tests and enforces a strictly scoped edit diff:

```bash
pnpm test:swarm-models:syntax
pnpm test:swarm-models:live -- --targets "ollama:YOUR_MODEL,ANOTHER_TARGET" --repetitions 3
```

Target names must match real locally configured execution targets and their
target-inventory keys (for example `ollama:qwen-model` and
`provider:openai:gpt-model`). The command fails for missing models or failed
tests. Save its JSON stdout as a file, then use **Settings → Background Agents →
Agent swarms → Verified-fixture model accuracy → Import**. The importer
recomputes ranking from the complete per-run fixtures and rejects missing,
duplicated, stale, mismatched, or mock-labelled results; a run-id string is
required for every reported successful fixture. These checks do **not**
cryptographically attest that an imported file was produced by the CLI.

Quality-based routing is **off by default**. Enabling it can reorder only
already eligible `subagent_explore` / `subagent_code` candidates from an
enabled routing policy **without explicit preferred model keys**. Per-profile
manual pins, routing eligibility, tool/vision requirements and the post-routing
Privacy Firewall remain authoritative. If fewer than two eligible models
have imported measurements, routing is unchanged.

The benchmark has **not** been executed against real configured models by CI
and does **not** measure general agent correctness or consensus quality.

## Automated checks

```bash
pnpm install --frozen-lockfile
pnpm build:budget
pnpm test -- src/lib/workflow.test.ts \
  src/lib/swarmPlanning.test.ts src/lib/swarmGraph.test.ts \
  src/lib/swarmPeer.test.ts src/lib/swarmPeerMetrics.test.ts \
  src/lib/swarmSpecialistMetrics.test.ts src/lib/swarmVerifiedBenchmarks.test.ts src/lib/swarmKnowledge.test.ts \
  src/lib/swarmConsensus.test.ts src/lib/swarmNotebook.test.ts \
  src/lib/swarmPresets.test.ts
pnpm test
pnpm i18n:lint
pnpm a11y
pnpm test:swarm-peer:contract
pnpm test:rust
LITTLE_MONKEY_REQUIRE_LIVE_PEER=1 cargo test \
  --manifest-path src-tauri/Cargo.toml --bin monkey-cli peer_live \
  -- --ignored --nocapture
cargo audit --file src-tauri/Cargo.lock
```

The GitHub Actions CI workflow owns those builds and real peer loopback
checks. The real loopback TLS case uses a deterministic fake receiver model,
but the sender/receiver HTTP/auth/ledger/result materialization is live.

## MCP connectors and operator-approved command hooks

**Settings → Background Agents → Agent swarms → MCP plugin interoperability**
offers controls for MCP servers configured by the operator. Third-party MCP
processes are separate from Little Monkey's native orchestration runtime.
There is no bundled external swarm framework or one-click third-party
installation; review each server's executable, package source, permissions
and tool allowlist in MCP settings before connecting.

**Review and import hooks.json** reads only a user-selected local JSON file,
previews supported executable commands, asks for explicit confirmation, and
copies a bounded subset into existing Little Monkey user hooks. The four
supported local events are `PreToolUse`, `PostToolUse`, `SessionStart`,
and `UserPromptSubmit`. Unknown events, unbounded commands, unsupported
execution policy settings, background workers, and arbitrary plugin code
are not executed by this importer.

## Real two-installation peer acceptance script

From the **actual sending installation**, after both installations are paired
and the receiving peer's read-only `peer-task` recipe is configured, run:

```bash
pnpm test:swarm-peer:live -- --peer lab-mac --timeout-seconds 300
```

You may add `--expect 'README'` to require an exact string in the real model
result. This command uses the installed `monkey` CLI, validates the task grant,
sends a uniquely correlated read-only request, checks the sender's durable
outbound receipt, and polls the **same** remote thread over the existing
certificate-pinned signed transport until the actual receiver finishes.

The script emits structured evidence with a real thread/message id and elapsed
time. Network failure or timeout is **not** proof of cancellation; it never
re-sends work automatically. On CI the script is syntax-checked only, because
CI does not possess real paired user machines or their model credentials.
Run this acceptance test on both directions and record the output before a
production release.

## Manual CI gate with actual models and two paired installations

The repository includes
`.github/workflows/swarm-live-acceptance.yml`, an **operator-triggered**
acceptance workflow. It is never run by push or pull-request events because
it requires real user-owned installations, installed runtimes, paired devices,
model credentials and a disposable test repository.

Prepare **two independently installed Little Monkey instances**. Register the
sending installation as a self-hosted runner with the labels
`swarm-acceptance` and either `swarm-node-a` or `swarm-node-b`.
The receiving installation must be paired with a task grant and an approved
read-only `peer-task` recipe pointed at a non-production repository.

After this workflow exists on the repository's default branch, open GitHub
Actions → **Native swarm live acceptance** → **Run workflow**. Select the
reviewed branch, the correct sender runner label, the receiver's *paired
alias*, and real configured model target IDs. The job refuses to proceed
without an installed `monkey` CLI. It runs the actual signed peer task
through completion and three independent bug-fix fixtures per configured
model. Both gates must succeed; missing models and failed real fixes fail
the job. The resulting evidence JSON artifacts are retained for 14 days.

Repeat with the two sender/receiver roles **reversed**. A completed
one-direction workflow is not evidence of reverse connectivity, remote
cancellation under load, or multi-writer memory convergence. Record
those separately in the release checklist below.

## Two real installations: required acceptance before production

Use a non-production test repository, local test credentials, and read-only
peer-task recipes with explicitly granted task capability, following
[peer-live-validation](peer-live-validation.md).

1. In Settings, verify a tool-capable local or configured provider model
   produces valid `workflow` tool calls and completes a read-only goal swarm.
   Record actual agent count, task reports, elapsed time and executed tools.
2. Pair machine A to machine B, confirm certificate fingerprint out of band,
   and ensure B's `peer-task` recipe is limited to read-only tools.
3. From A launch `{"goal":"Audit the test API","mode":"analyze","auto_peer_pool":["node-b"]}`.
   Check A's outbound receipt, B's inbound run, the resulting report and
   independent local verification. Never infer this from a green CI socket test.
4. Disconnect B after task acceptance and before result delivery. Reconnect,
   explicitly resume, and verify A reattaches to **the same** receipt. Modify
   the task prompt and verify its new correlation cannot reuse an old result.
5. Crash/restart A halfway through a multi-phase graph. Verify the previous
   run is shown as interrupted/error and requires explicit resume. Completed
   unchanged read-only reports may be reused; inspect the Git diff before
   manually retrying any agent with write privileges.
6. Exercise a **real** code-fix/test goal on a disposable worktree; confirm
   the user is asked for permissions, tests genuinely execute, and failures
   prevent downstream modifying stages. Never grant auto-push or deploy.
7. Stage two agreeing reviewer votes plus contradictory votes. A disagreement
   must remain disputed; only an operator approval should expose a new notebook
   revision to a subsequent local swarm. Confirm rollback and project isolation.
8. For evaluated learning, verify the parent Rust run ledger actually contains
   eligible successful/verified evidence before using **Check verified
   learning eligibility**; inspect the staged candidate in native Skill
   Learning and require a real isolated evaluation and configured promotion
   policy before installation.
9. Connect an explicitly configured, reviewed MCP server through the existing MCP UI
   with a narrow tool allowlist; disconnect/disable/remove it; verify no
   forbidden tool becomes available after disabling it.

Until those observations are recorded, keep the PR draft and **do not claim
end-to-end production parity**.

## Real multi-installation replication and cancellation evidence gates

Two additional operator-only acceptance harnesses now complement the existing
real task and model tests. Their `node:test` contract tests use a controlled
CLI to catch harness bugs; **only execution on two independently installed
Little Monkey nodes can produce actual product acceptance evidence.** All
real-device steps remain opt-in and are never run automatically on PRs.

**Vector replication:** Pair two installations, confirm pinned certificate
fingerprints, grant memory-read and memory-write explicitly **on both inbound
devices**, and share a disposable `lm-e2e-...` namespace using `peers
memory-share` on each receiver. On B, pre-seed one *locally authored* record:

```sh
monkey peers memory-put lm-e2e-disposable-a receiver-record lm-e2e-vector-v1 '1,0,0' 'receiver evidence' --revision 1
```

On A, invoke:

```sh
node scripts/swarm-two-peer-memory-live-acceptance.mjs \
  --peer node-b --namespace lm-e2e-disposable-a \
  --receiver-record receiver-record --confirm-disposable
```

It writes a randomly named local sentinel, synchronizes both ways, requires
a receiver-authored vector to be returned from local search, checks an
idempotent repeated pull, then publishes a revision-2 tombstone. The script
never changes grants or remote endpoints. Run again with A/B roles reversed
and a fresh disposable namespace. A successful local harness contract test
is not proof of real memory convergence. Incomplete outcomes must be
inspected, not automatically replayed.

**Remote cancellation:** Configure B with an explicitly read-only peer-task
recipe pointed at a disposable test repository. Its model must be able to
remain running long enough for a cancellation request (otherwise the test
correctly fails rather than pretending an already-completed task was killed).
From A, run:

```sh
node scripts/swarm-two-peer-cancellation-live-acceptance.mjs \
  --peer node-b --confirm-disposable --timeout-seconds 300
```

This creates **one** correlated disposable read-only request, recovers its
owned outbound receipt, sends exactly one cancellation, and polls the same
remote thread until its terminal status is **cancelled**. A cancellation
acknowledgement alone, an already completed job, a connection timeout, or
successful task completion do **not** count as verified cancellation.

For an operator-triggered self-hosted run, use GitHub Actions →
**Native swarm live acceptance** and optionally provide both
`memory_namespace` and `receiver_memory_record`, or enable
`verify_remote_cancellation`. The runner must be the actual installed sender
and the opposite node must be configured in advance. Retained artifacts
record observations without exposing vector text or provider credentials.

Outstanding release proof includes live network partition/recovery, parallel
author stress, real-model finding-to-fix-to-test success and independently
evaluated specialist accuracy. Do not mark the system production-ready solely
because these harnesses are installed.

## Independently verified specialist accuracy and adaptive selection

Runtime completion counters are not evidence of correctness. There is now
a separate, **default-disabled** quality-based selection path for the three
supplemental security specialists. Real observations follow this loop:

1. On a host with Little Monkey installed and a configured Ollama model, run
   `node scripts/swarm-specialist-live-benchmark.mjs --target ollama:<model>
   --repetitions 3 > specialist-results.json`.
2. The harness creates disposable security source fixtures including one
   correct, negative-control example. It executes a real headless model
   with PLAN permissions, MCP/rules disabled, and checks unchanged Git
   source, actual durable run IDs, and model reports with an independently
   coded source oracle. Neither a self-reported pass nor mere completion
   can count as a verified finding.
3. In Swarm settings, import the JSON; Little Monkey rechecks all three
   fixtures for each specialist, every repetition, unique run identifiers,
   the timestamp and all aggregate rankings. Incomplete or contradictory
   measurements are rejected. A user-provided JSON file is not
   cryptographic evidence of provenance.
4. Only after operator opt-in, verified accuracy ranks the supplemental
   *security* specialist among existing approved roles. Core review roles,
   all other engineering domains, model eligibility and tools remain
   unaffected. Data expires after 90 days; clearing it disables ranking.

These are small, controlled tests, not proof of general repository-audit
quality. The live evaluator requires a genuine model; CI runs its source
oracle self-check without pretending to have a configured inference server.

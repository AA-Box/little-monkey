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

The operator-run physical three-stage network-cut and post-restore checkpoint
stress procedure is documented in
[physical two-machine swarm acceptance](swarm-physical-two-machine-acceptance.md).
After a successful restore the manual workflow now requires verified concurrent
SQLite checkpoint publications, with stable peer identity and no duplicated
sequence. A contract test alone remains insufficient. No physical result has
been verified solely by adding these gates.

Outstanding release proof includes actual live network partition/recovery,
parallel-author stress, real-model finding-to-fix-to-test success and independently
evaluated specialist accuracy. Do not mark the system production-ready solely
because these harnesses are installed.

## Real-model verified specialist accuracy: all five domains

The accuracy-driven specialist selector is implemented for **security, frontend,
backend, performance, and general** goal domains. Every domain has its own
exactly three already-eligible supplemental reviewer roles, two defective
executable source fixtures, and one known-good negative control. The
read-only runner grades actual model output against an independently
executed Node regression oracle, unchanged committed Git files, the real
run receipt, and an exact source-symbol/category report format. A model
saying "passed" is never accepted as grading evidence.

### Execute domain-specific benchmarks

On an operator-controlled machine with Little Monkey and a working Ollama
model, run one of the following (the model is **not mocked**):

```sh
node scripts/swarm-specialist-live-benchmark.mjs --domain security    --target ollama:YOUR_MODEL --repetitions 3 > security.json
node scripts/swarm-specialist-live-benchmark.mjs --domain frontend    --target ollama:YOUR_MODEL --repetitions 3 > frontend.json
node scripts/swarm-specialist-live-benchmark.mjs --domain backend     --target ollama:YOUR_MODEL --repetitions 3 > backend.json
node scripts/swarm-specialist-live-benchmark.mjs --domain performance --target ollama:YOUR_MODEL --repetitions 3 > performance.json
node scripts/swarm-specialist-live-benchmark.mjs --domain general     --target ollama:YOUR_MODEL --repetitions 3 > general.json
```

The benchmark uses a disposable repository for **each attempt**, `plan`
permission, `--no-mcp` and `--no-rules`. It executes three fixtures for
each of three specialists per repetition. Failure to answer, missing real
run IDs, code changes, wrong classifications and false positives on the
control fixture all count against accuracy. The process returns a nonzero
exit status if some checks fail but still prints structured results for
operator review. It does not grant tools, pair peers, or deploy code.

Use `node scripts/swarm-specialist-live-benchmark.mjs --self-check --domain all`
to exercise **all 15 source oracles without inference**. The self-check
is for harness correctness only, not accuracy verification on a real model.

### Import, store and apply

In **Settings → Background Agents → Agent swarms → Verified specialist
quality**, select a domain and import its JSON. Imported reports are checked
for domain-specific exact role names, fixtures, every repetition, bounded
durations, fresh timestamps, unique actual-run identifiers, and scores
recomputed from each independently graded outcome. Existing security-v1
benchmarks are migrated transparently. Data is kept **per domain**: importing
backend never replaces security evidence or enables backend routing
automatically.

An explicit per-domain opt-in then allows only the supplemental reviewer
among already eligible roles for **that same domain** to be ranked by
independently measured accuracy (latency is a tie-breaker). Core reviewers,
model selection, permission policies, peer grants, and other domain results
are unchanged. Data expires after 90 days. A domain can be cleared without
removing other domains. Ordinary task completion remains a separate
reliability signal and never counts as independently verified correctness.

The manual self-hosted **Native swarm live acceptance** workflow has a
`specialist_domain` selector; it runs real-model checks only when
`specialist_target` is supplied. Run it separately for each desired
domain and import each resulting artifact.

**Evidence boundary:** A user-provided benchmark JSON is not signed
attestation. These intentionally small, source-checked tasks establish
narrow benchmark performance, not general coding intelligence or accuracy
on arbitrary production repositories. A real installed model must execute
each live suite before its accuracy can be asserted.

## Operator-controlled resident crash/restart proof

The installed daemon persists frozen graphs, child IDs and terminal states.
To test actual recovery, first register a **disposable active** graph on a
dedicated Little Monkey installation. While it has a pending/running node:

```sh
node scripts/swarm-resident-restart-live-acceptance.mjs --phase capture \
  --graph swarm-your-test-id --checkpoint /tmp/swarm-before-restart.json \
  --confirm-disposable
```

Only the operator may then restart the installed service. On a dedicated
test service, a graceful stop and explicit re-ensure look like:

```sh
monkey daemon stop
monkey daemon ensure --json
```

CAUTION: `daemon stop` cancels active work; do NOT do this on a machine with
unrelated jobs. After a restart, an interrupted mutating child may end in
failure/reconciliation rather than successful continuation. That is correct:
unsafe side effects must NOT silently repeat. After the original graph
reaches a terminal state, run:

```sh
node scripts/swarm-resident-restart-live-acceptance.mjs --phase verify \
  --graph swarm-your-test-id --checkpoint /tmp/swarm-before-restart.json \
  --confirm-disposable
```

The gate requires an actual changed OS service PID, fresh daemon heartbeat,
the same durable graph and node identities, preservation of original job
IDs and completed work, and no automatic redispatch of failed work. A
captured-but-not-verified checkpoint is NOT evidence of crash recovery.
Only controlled contract tests run in CI; they do not restart any service.

## Real source vulnerability → daemon swarm → repair → external tests

On a dedicated operator-owned Little Monkey installation with a real local
Ollama model, a running daemon and an operator present to approve mutations:

```sh
node scripts/swarm-repository-repair-live-acceptance.mjs \
  --target ollama:YOUR_INSTALLED_MODEL --confirm-manual-approvals \
  --timeout-seconds 2400
```

This constructs a disposable real Git repository containing a known owner
authorization regression and tests which fail before any model runs. It
queues a three-stage frozen DAG (read-only audit; MANUAL permission repair;
read-only independent review), freezes ledger-verified prior reports and
allows no auto-push or unapproved code edits. The operator must answer
ordinary daemon permission prompts through Little Monkey.

The harness waits for three distinct succeeded resident jobs, runs the
regression tests independently in Node, checks the exact Git diff (only
`src/module.mjs` may change), and refuses a remaining inverted owner check.
Failures retain the disposable workspace for review. Its `--self-check`
only confirms the fixture oracle catches the baseline defect, and does
not stand in for a real model or permission-approval acceptance run.

The optional real-device GitHub Actions `repair_target` input invokes the
same gate only when deliberately supplied. Real repairs may require an
available operator; without one the safe outcome is timeout/failure.


## Portable, operator-reviewed read-only swarm recovery bundles

The resident DAG still stores its own ledger and queued child jobs on one
machine; this is NOT automatic replicated-log failover. To deliberately
restart an existing **read-only, plan-mode** frozen graph on another operator
installation, export its bounded recipe and *observational-only* state:

```bash
monkey daemon swarm-export original-graph /secure-transfer/graph.json
```

Transfer that JSON file using a separately authenticated operator-controlled
channel, inspect it before importing, and select an existing **local** workspace
on the receiving installation:

```bash
monkey daemon swarm-import-read-only /secure-transfer/graph.json \
  --new-id after-failover-v2 --workspace /absolute/disposable/workspace \
  --confirm-read-only-reexecution
```

The importer compares whole-bundle and per-recipe SHA-256 checksums, rejects
unknown fields, oversized files and non-plan recipes, rewrites every recipe
to the receiving operator's workspace, and calls the *original* daemon graph
registration path. An import requires a new graph ID, and **every imported node
starts pending** with new deterministic job identities. A remote "succeeded"
observation never authorizes a downstream dependency or marks a job completed.
The original cluster roster is preserved if present; a matching, separately
enrolled local roster and current signed majority lease are still required
before a clustered graph schedules anything.

These checksums detect corruption, **not malicious replacement or sender
authenticity**. The import flag is an explicit agreement to re-run plan-only
compute and can consume actual model resources. Operators must separately
approve transferred prompts/target models, authenticate the origin of the
bundle, and inspect failures. There is no replica membership propagation,
automatic bundle transfer, persistent replicated commit index, quorum-backed
graph-log consensus or transparent exactly-once task migration. Those remain
needed before claiming cross-host automatic recovery.


## Native event-driven background worker catalog

The resident daemon now provides twelve optional, bounded review roles using
its **existing persisted cron and filesystem triggers**. The worker's model is
the operator-selected source recipe's own configured model. No background
worker is installed by default, and no plugin or remote peer is granted.

```bash
monkey daemon swarm-worker-catalog
monkey daemon swarm-worker-install security-nightly \
  --role security-review \
  --source-recipe /absolute/review-plan.json \
  --workspace /absolute/disposable/workspace \
  --cron '0 2 * * *'
monkey daemon swarm-worker-install test-file-changes \
  --role test-gap-review \
  --source-recipe /absolute/review-plan.json \
  --workspace /absolute/disposable/workspace \
  --watch /absolute/disposable/workspace
```

Source recipes must already be valid **plan-mode**, bounded and fully
parameterized. Explicit messaging, privileged placement, autonomous-task and
desktop-turn snapshots are rejected. The worker freezes its own private recipe
and uses ordinary native trigger delivery, deterministic job IDs, kill-switch,
backpressure and runtime approval policies. Scheduled results are ordinary
daemon runs; the worker does not auto-apply fixes or auto-promote models.

The installed trigger ID is prefixed with `swarm-worker-`. Disable it
using the existing `monkey daemon trigger remove <id>` command.
A disabled trigger is not silently reenabled by installing another worker with
the same ID; install rejects duplicates and requires review.

These twelve roles provide bounded native review automation, not literal
Ruflo background-worker binary compatibility. Complete event subscriptions,
cross-node worker leasing, automatic learned role creation, and physical-host
recovery acceptance remain separate work.


## Signed peer DAG state replication (monotonic, operator-rostered)

A current signed majority leader automatically publishes **observational**
DAG checkpoints at bounded intervals to explicitly enrolled paired peers with
the existing `peer-task-request` capability. No permissions are granted by
replication. Checkpoints carry group/graph identities, monotonic durable
sequence, leader term, source certificate fingerprint, enrolled roster digest,
bounded node/job states and a corruption-detection SHA-256. The peer TLS/HMAC
authentication plus its independently operator-enrolled roster membership
provide identity; the SHA-256 by itself is NOT a signature.

The receiver validates and transactionally stores only an advancing checkpoint
for the exact originating fingerprint. Duplicate retransmissions are
idempotent. Same-sequence mismatches, old terms, forged member IDs, oversized
messages, malformed nodes, stale timestamps, revoked task grants and kill
switches fail closed. A receiver never inserts foreign daemon jobs, treats a
claimed remote success as locally verified, or rewrites frozen recipes.

Operators can trigger a specific publish and inspect their own received copies:

```bash
monkey daemon swarm-replicate my-cluster audit-graph paired-node-b
monkey daemon swarm-replica-status my-cluster audit-graph
```

In Settings → Background Agents → Agent swarms, enroll/enter the reviewed
cluster group, and click **View authenticated peer checkpoints** on a known
graph. The UI explicitly labels those records non-executable observations.

**Boundaries:** This is real signed peer checkpoint *transport* and persistent
replication, not a distributed Raft log or transparent transfer of active
processes or mutable worktrees. A new leader may independently start its
locally prestaged read-only DAG from its own approved frozen recipes. It cannot
auto-skip previous leader's completions: externally signed receipts still need
evidence-chain verification. If a fresh local graph must be created, the
separate `swarm-import-read-only` path explicitly requires operator
approval and creates NEW job identities. No cross-host exactly-once effects
are claimed. Physical two-installation partition and TLS acceptance must be
recorded before certification.


## Real, externally isolated DAG replica acceptance

Checkpoint publication now requires TWO independent authenticated operations:
a signed POST of the monotonic source checkpoint, followed by a separately
signed, certificate-pinned GET that retrieves the **exact** JSON state from
the receiving daemon's SQLite replica store. If the read-back does not
match the source bytes, the CLI fails instead of claiming replication worked.

A three-member independent-voter regression also tests A+B majority while C
is disconnected, lease expiry, then B+C majority while A is disconnected.
These are real native Rust state-machine tests, **not physical peer acceptance**.

For the physical network test, use the operator-owned two-device workflow
`.github/workflows/swarm-live-acceptance.yml` with
`cluster_group`, `replica_graph_id` (already-prestaged plan-only graph),
`peer_alias`, `network_evidence_path` on a persistent self-hosted
machine, and `network_stage`. Use the **same** evidence path and machine
for all three separately invoked stages:

1. `baseline`: with network healthy and a current strict-majority lease, a real signed probe plus remote SQLite checkpoint readback creates new protected evidence.
2. `partition`: operator physically isolates the peer and confirms that condition; a fresh signed peer probe must fail. The harness records the outage only when it can observe the transport failure. A failed probe by itself does not distinguish network isolation from service failure or revoked credentials.
3. `restore`: operator restores connectivity and declares it; a fresh signed probe and **strictly newer** remotely persisted DAG checkpoint must succeed with the same enrolled source identity.

Direct invocation is available when testing outside GitHub Actions:

```bash
node scripts/swarm-dag-network-recovery-live-acceptance.mjs \
  --stage baseline --group my-cluster --graph disposable-graph \
  --peer paired-node-b --evidence /private/swarm-evidence.json \
  --confirm-operator-owned-readonly-cluster

# Operator isolates remote peer using their own reviewed network controls.
node scripts/swarm-dag-network-recovery-live-acceptance.mjs \
  --stage partition --group my-cluster --graph disposable-graph \
  --peer paired-node-b --evidence /private/swarm-evidence.json \
  --confirm-operator-owned-readonly-cluster --confirm-network-isolated-externally

# Operator restores the peer and confirms that independently.
node scripts/swarm-dag-network-recovery-live-acceptance.mjs \
  --stage restore --group my-cluster --graph disposable-graph \
  --peer paired-node-b --evidence /private/swarm-evidence.json \
  --confirm-operator-owned-readonly-cluster --confirm-network-restored-externally
```

No network permissions, firewall rules, remote tasks or security grants are
modified by this harness. It proves the operator-declared isolation/reconnect
and signed stored-state transfer, **not** process migration or exactly-once
tool execution. No result is certified until a real two-installation run has
produced the workflow artifact.


## Apple Silicon fused-model inference acceptance (real MLX-LM)

The operator-owned live workflow exposes an opt-in
`mlx_fuse_for_import` switch. On an actual `darwin/arm64` self-hosted
Mac, this invokes the existing actual MLX LoRA trainer, evaluates independent
held-out and historical regression datasets, fuses the resulting adapter,
then re-evaluates **the fused weights**. It fails closed if adapter or fused
weights degrade beyond the operator's configured held-out gate.

A second independent Node/Python acceptance runner now reopens the
`training-receipt.json`, SHA-256 checks the actual fused config and each
safetensors shard against the evaluated weight manifest, loads the fused
model in `mlx_lm` on Apple Silicon, and requires a bounded non-empty
**fresh model generation**. The optional `mlx_previous_model_dir` probes
the explicitly selected previous model on the same installed MLX runtime,
so operators can check that rollback weights remain loadable before changing
their running model.

The native desktop already has an operator-initiated verified import,
live-inference canary and crash-journal-protected rollback in
`src/lib/swarmMlxPromotion.ts`. This acceptance runner does NOT
automatically activate the candidate in the desktop or claim that desktop
rollback was exercised. Real desktop provider handoff, post-switch acceptance,
failure injection and actual rollback still require physical Apple Silicon
execution. Neither generated package/weight bytes nor private inference output
are uploaded as CI evidence; the workflow uploads bounded JSON receipts.


## One-shot, operator-armed automatic DAG leader handoff

A receiving installation can now be armed **before** the leader disappears with
an exact immutable bundle, known local workspace and new target graph ID:

```bash
monkey daemon swarm-failover-arm recovery-on-b \
  --group my-cluster --source-peer original-leader-a \
  --new-id read-only-recovery-b \
  --bundle /private/preapproved/read-only-graph.json \
  --workspace /absolute/disposable/approved-workspace \
  --confirm-read-only-reexecution

monkey daemon swarm-failover-status
```

Nothing runs when this command is issued. Both installations must already be
operator-enrolled in the exact same cluster; the **source** exports the
read-only bundle and the **destination** independently stages it. At arming,
the destination checks the bundle's per-recipe and graph digests, plan-mode
permissions, source cluster, destination workspace and the file SHA-256. The operator must also bind the plan to a specific, enrolled **original leader fingerprint** via `--source-peer`; an unrelated cluster member's checkpoint can never authorize the takeover. Older unbound draft plans are migrated to a disabled (non-eligible) state and must be re-armed, rather than silently inheriting an origin.

Every ten seconds the installed daemon checks armed plans. A plan can only
activate when ALL conditions hold: this host currently possesses a valid
strict-majority leader lease; the local election term is **newer** than an
authenticated other-member checkpoint for the exact original graph; that
checkpoint has the matching operator-enrolled roster digest and is no older
than ten minutes; and the staged bundle still matches the original approval
hash. The worker commits an exclusive one-shot claim to durable SQLite
**before** importing. The newly registered graph is assigned a new identity,
and **every task is pending** — no foreign reports, success states, model
tokens, receipts, conversations, approvals or mutable workspace are imported.

**Safety gate for ambiguous work:** Automatic takeover is refused whenever
any node in the specifically approved predecessor's latest authenticated
checkpoint is already submitted, completed, failed, or carries a foreign
job ID. Those tasks may still be running or may have had effects not
recorded in the receiving ledger. There is no false exactly-once guarantee:
the plan remains armed for operator reconciliation rather than automatically
replaying work with uncertain execution history. Even an all-pending
checkpoint is not a quorum-replicated pre-dispatch journal, so strict
cross-host at-most-once guarantees still require a replicated dispatch
commit protocol and actual physical fault-injection acceptance.

If the process crashes after the one-shot claim and before registration, the
plan remains in `claimed` state. It is deliberately **not** retried:
doing so could create ambiguous duplicate effects. Inspect the existing
graph and claim before explicitly arming a new ID. Imported/failed states
persist across daemon restarts and are operator-visible.

A native three-database test exercises prior A+B majority leadership,
authenticated source checkpoint replication, arming on B, absence of
scheduling authority before a second election, and B+C majority takeover
and automatic DAG registration while A is isolated. This is a functional locally simulated takeover test,
**not** a substitute for physical partition tests or full distributed
exactly-once process migration. It only supports read-only re-execution of
operator-prepared plans; it intentionally refuses unattended edits.


## Replicated pre-dispatch intent and majority-safe automatic recovery

Clustered read-only graphs now have a two-phase dispatch boundary:

1. Registration requires a **currently elected signed majority leader**. A
   durable graph ownership record pins that graph to its original leader
   certificate fingerprint and election term. Recoveries register NEW graph
   IDs under their successor's new term; a former leader cannot revive an
   old graph simply by winning a later election.
2. Before any newly scheduled clustered task is enqueued, its
   deterministic job identity is committed as `preparing` in the
   graph's durable SQLite node ledger.
3. The replication worker sends the exact state vector to separately
   operator-enrolled peers using the pinned signed peer transport. Only an
   **independent signed GET of the same peer's SQLite contents** earns one
   durable state-specific ACK. The leader dispatches only after the ACKs
   plus itself form a strict majority, under its CURRENT term, with evidence
   refreshed in the last fifteen seconds.
4. The coordinator repeats both its original-term ownership test and
   state-specific majority check immediately before the native job enqueue.
   A stale ACK for `pending`, an earlier term, or a different job ID
   cannot start a `preparing` task. Cancelled preparations are
   recorded as cancelled, never enqueued.

A successor faces a second quorum intersection check. Once elected, it makes
fresh signed GETs to a strict majority of peers to reconcile the original
leader's **latest** checkpoint, rather than trusting its locally cached
`pending` copy. An ACKed pre-dispatch intent must have reached an
overlapping peer, so a quorum successor cannot suppress that intent by
reading only stale local replicas. Any witness reporting `preparing`,
`submitted`, a former job ID or another attempted terminal status
prevents unattended re-execution. No response or insufficient peers also
blocks recovery; there is no speculative duplicate.

These guarantees apply to native **plan-mode, local read-only work** using
previously operator-staged bundles. They do not migrate active inference
processes, recover arbitrary mutable worktree edits, authenticate a model's
claimed answers, or provide exactly-once side effects to third-party tools.
The recovery worker is allowed to re-run only a graph with a quorum-proven
**no-dispatch** predecessor state and separate preapproved instructions.
Actual three-machine TLS partition + power-failure tests are still mandatory
before describing production fault tolerance as physically verified.

# Native agent swarms: goal planning and indexed memory

Little Monkey has an agent orchestration engine already. This feature builds
native automatic specialist selection on top of its existing `workflow`
tool, subagents, models, permission gates, process ledger and execution limits.
It extends Little Monkey's existing engine rather than replacing its operating-system
security boundary.

## How to run

1. Open the repository as the current Little Monkey workspace.
2. Select a tool-capable local Ollama/MLX/llama.cpp model, or a configured
   provider model. The selected model must actually emit tool calls.
3. Enable **Subagents** in Settings.
4. Ask the model to run a native workflow, for example:

> Analyze the UnifiedPlay API for reliability and architecture issues. Use
> an auto-swarm, and keep the workspace unchanged.

The model can call:

```json
{"goal":"Analyze API reliability and architecture issues","mode":"analyze","scope":"the selected workspace"}
```

This creates specialist discoverers, independent validators, and a final
synthesizer, using the existing phase scheduling and bounded parallelism.

For a change the user explicitly authorized:

```json
{"goal":"Implement resilient API retries with tests","mode":"implement","scope":"backend API"}
```

This creates discovery, planning, a **single** permission-gated modifying
worker, a test runner, an independent read-only reviewer, and final synthesis.
Changes and tests remain subject to Little Monkey's standard permissions.

A workflow tool call can also select the fixed `security-red-team` and
`security-audit-and-fix` presets described in [Security swarms](security-swarms.md).

## Review-gated read-only desktop → daemon handoff

In **Settings → Background Agents → Agent swarms**, enter a goal in
**Read-only analysis** mode and choose **Run approved read-only goal in resident
daemon**. Select an existing, previously reviewed **JSON task recipe** (which
fixes the model target and absolute workspace), then select a local directory
for the generated job snapshots. Review the final summary before submission.

The desktop compiles each local \`explore\` agent into a distinct frozen
headless task recipe, assigns each previous-phase task as a dependency of the
next phase, forces **plan** permission even if the source recipe says manual,
and registers the resulting graph with the resident SQLite-backed daemon.
Frozen recipes retain the selected target, workspace, bounded system rules,
timeout and iteration budgets. No remote peers, write agents, unknown
permissions, messaging destinations, browser-only indexed memory, command
hooks or automatic edits are forwarded. Unsafe configurations fail before
any file is written. Registered graphs survive application restarts and can
be inspected/cancelled in the same panel.

With `include_dependency_reports: true` on a dependent node, the resident
daemon verifies each direct prerequisite's run-ledger hash chain and
successful terminal event, then freezes bounded model output into its input
as clearly **untrusted evidence**. Incomplete, missing, altered or unsuccessful
run records prevent the dependent job from starting. This is report
transmission, not proof that the model's claims are correct; the dependent
agent must verify source evidence independently. Generated read-only goal
graphs enable the option only for later phases.

Execution requires a working local daemon and installed provider/model
credentials. Conversion is opt-in, never automatic from a model-generated
prompt.

## Native workflows in headless and daemon-owned model turns

The native `monkey` CLI now offers the **same explicit phase-based
workflow operation** to model turns when subagent delegation is enabled.
It uses the existing frozen task target, workspace permissions and checkpoint,
and delegates to the CLI's depth-one `explore` / `code` subagent runtime;
it does not load the desktop's in-memory session or invent credentials.

A model may invoke the `workflow` tool with:

```json
{
  "name": "api-review",
  "phases": [
    { "title": "Read-only review", "agents": [
      { "description": "Authorization", "prompt": "Review ownership checks", "profile": "explore" },
      { "description": "Error handling", "prompt": "Review retries and timeouts", "profile": "explore" }
    ]},
    { "title": "Evidence summary", "agents": [
      { "description": "Independent reviewer", "prompt": "Verify and summarize", "profile": "explore" }
    ]}
  ]
}
```

This is enabled only if the originating model turn was configured with
subagent delegation; a model-provided workflow call is rejected at dispatch
when that opt-in is absent. Its schema permits 1–6 phases, 1–6 agents per
phase and 16 agents total, with restricted profiles and bounded prompts.
CLI phases currently execute **sequentially**, stop on the first failed
agent, and never create remote peers or worktrees. Agents do not receive
recursive agent tools, network, MCP or memory permissions.

To keep an entire task graph resident across app restarts, use the installed
daemon's **frozen recipe DAG** from the Swarms settings screen or
`monkey daemon swarm-submit`. The daemon owns task-level state and restart
recovery; a single model-called CLI workflow does **not** promise atomic
replay of arbitrary mutating steps or per-agent restart durability.

## Dependency-aware task graphs

The workflow tool also accepts an explicit DAG of tasks. The graph compiler
checks for duplicate IDs, missing dependencies, cycles, agent caps and
oversized workloads before starting anything. Independent tasks run in the
same bounded wave. On a failed prerequisite, later waves are not dispatched.

```json
{
  "name": "api-hardening",
  "description": "Inspect, fix and verify authentication",
  "tasks": [
    {
      "id": "audit",
      "description": "Review authentication",
      "prompt": "Inspect token and permission boundaries; report source evidence.",
      "profile": "explore"
    },
    {
      "id": "fix",
      "description": "Repair confirmed issue",
      "prompt": "Implement only independently verified findings, with approval.",
      "profile": "code",
      "depends_on": ["audit"]
    },
    {
      "id": "verify",
      "description": "Verify regression tests",
      "prompt": "Run authorized tests and report actual pass/fail results.",
      "profile": "code",
      "depends_on": ["fix"]
    }
  ]
}
```

An explicit DAG is still a local bounded workflow, not a distributed task
scheduler. It uses the same permission boundary for code-profile agents and
does not automatically merge parallel worktrees.

## Persistent indexed knowledge (explicit opt-in)

Use **Knowledge 2.0** to create and index sources with an embeddings-capable
runtime before requesting a shared knowledge context. Then provide the IDs of
up to five indexed stacks:

```json
{
  "goal": "Review API authentication design",
  "mode": "analyze",
  "memory_stack_ids": ["existing-index-id"]
}
```

Each swarm retrieves up to five indexed snippets once at the start of the
workflow, then includes the **same source-labelled, bounded reference block**
in each specialist prompt. Earlier-phase agent reports are also shared with
later phases. The data is fenced as untrusted and should be checked against
current source files. An unknown stack, an index that has not been built, or
an unavailable embedding runtime **fails visibly** instead of fabricating
retrieval. Saving/replaying a workflow preserves its explicitly chosen IDs.

This is durable retrieval over Little Monkey's existing persisted Knowledge
stacks. It is **not** automatic embedding/indexing of every agent message.
Agent conversations are not implicitly converted into trusted memories. For
personal long-lived facts, Little Monkey already has Memory Studio; for
reusable procedures, it already has the separate Skill Learning pipeline.

If a configured cloud model is used, selected snippets become model context
subject to the existing Privacy Firewall. Do not select private indexes you
are unwilling to share with the configured model.

## What is reused

- **Routing:** existing per-profile policies and model pinning; the goal
  compiler chooses specialist responsibilities, not imaginary model IDs.
- **Execution:** existing `runWorkflow` with sequential phases, bounded
  concurrency, cancellation, checkpointing and persisted reports.
- **Memory:** Knowledge 2.0 vector retrieval from explicitly selected,
  already-indexed sources, plus previous-phase reports.
- **Permissions:** the existing single permission authority. The compiler
  never grants shell or write permissions to read-only agents.
- **Peers:** Little Monkey already offers independently paired, authenticated
  peer messaging and task requests. This change does not silently hand a
  swarm to remote nodes or increase peers' grants.

## Crash recovery, strict reviewer consensus, and versioned notebook

Workflow progress is journaled to the conversation's existing file-backed
session state at startup and after each completed agent. On a forced app
restart a previously `running` workflow restores as **interrupted/error**,
not "still running" and not "succeeded". Nothing replays automatically.

To recover, return to **the original conversation** and explicitly ask it to
invoke the workflow tool with only the original workflow call id:

```json
{"resume":"call_original"}
```

The recovered spec is revalidated with normal agent/phase bounds and the
normal permission boundary. A completed agent with a matching full-prompt
hash reuses its recorded report; failed or changed work must run again.
For mutating agents, a crash can occur after a side effect but before its
journal is saved. **Inspect the workspace and Git diff before choosing to
resume.** Automatic at-least-once retries cannot guarantee exactly-once
mutations. Existing approval prompts remain mandatory.

Read-only goals can request independent structured reviewer votes:

```json
{"goal":"Review API authorization boundary","mode":"analyze","consensus":"strict"}
```

Two independent reviewers use bounded JSON claim votes. The orchestrator
distinguishes agreement, dispute and insufficient evidence. The status
`reviewer_agreement` is **not factual verification**; untrusted votes
never substitute for checking source or executing tests.

A workflow can optionally reuse a local, **human-approved** notebook of
claim IDs and cited evidence locations:

```json
{"goal":"Review authorization changes","mode":"analyze","consensus":"strict","notebook":true}
```

Reviewer-agreed claims are staged as **pending** only. In
**Settings → Background Agents → Agent swarms**, the operator can approve,
reject or roll back a version; until approved, no new claim is injected.
Approved revisions are injected into subsequent local swarms as explicitly
untrusted historical notes. They are stored per app profile and project scope
with bounded version histories, and cannot be sent automatically to peers.
This is **not** a multi-machine synchronized or shared writable vector index;
Knowledge 2.0's existing indexed retrieval remains separately opt-in.

## Distributed read-only discovery over paired peers

Little Monkey's existing peer transport uses certificate-pinned HTTPS and
per-peer grants. This PR exposes it to desktop swarms via the native
`peers_send_task` bridge. It is **not** a new public remote-code endpoint.

### Prerequisites

1. Both installations must have the daemon/CLI configured, plus the local
   models or approved providers needed for their roles.
2. Pair both installations through the existing **Peers** settings. The
   receiving installation must explicitly grant this sender the `task`
   capability.
3. Configure a **peer-task** recipe on the receiver with only the tools,
   workspace and permission mode you are willing to let peer requests use.
   Those remote permissions cannot be selected or increased by the sender.
4. Confirm the receiver is reachable in Peers and first test a benign,
   read-only task. For pair/liveness and real socket acceptance, follow
   [Peer live validation](peer-live-validation.md).

Example: distribute two discovery tasks while verifying locally:

```json
{
  "goal": "Analyze authentication and authorization correctness",
  "mode": "analyze",
  "scope": "current project",
  "peer_aliases": ["desktop-peer", "lab-peer"]
}
```

For bounded, automatic placement **within an operator-approved pool**,
use `auto_peer_pool` rather than fixed `peer_aliases`:

```json
{
  "goal": "Audit HTTP endpoint correctness",
  "mode": "analyze",
  "auto_peer_pool": ["desktop-peer", "lab-peer"]
}
```

The sender inspects durable receipts on each named peer before dispatch.
If there is no existing result, it selects an online, task-granted peer using
recorded success/failure counts and latency (with a three-observation
minimum). The telemetry stores only counts and durations, not agent prompts,
repositories, reports or private material. An unpaired/ungranted/offline peer
is not selected; no installation outside the named pool is discovered.
Failures after uncertain delivery do not trigger blind retry elsewhere.

The first two specialist-discovery tasks execute on those peers (if paired
and authorized). Unassigned specialists and every later verification and
synthesis task run locally. Alternatively, a task DAG or explicit phases
can assign `"peer":"desktop-peer"` to a specific **explore** agent.

The receiving peer may have different files or a different repository
revision. Remote assignments are instructions for its own configured
peer-task recipe; they do not transfer the local Git workspace or magically
synchronize changes. A peer result is evidence that requires local checking.

The controller validates the alias, finds an existing durable outbound
receipt by stable correlation on resume, otherwise sends through the
existing CLI transport, then polls that exact thread for a terminal result.
It does not re-send work merely because a network response was lost.
Timeout/cancellation stops **local waiting**, not work already accepted by
the peer; check Peers' outbound history before retrying.

**Permission boundary:** the sender only exposes remote placement for
`explore` agents, and never transmits opt-in Knowledge 2.0 vector snippets.
However, the remote node's actual tool permissions come from its local
peer-task recipe. It is not guaranteed read-only unless that recipe's
permissions enforce read-only behavior. Do not grant write/shell
permissions there for untrusted peer review.

This is real authenticated peer task dispatch and result collection, but
not global distributed scheduling: no work stealing, automatic node
discovery, code synchronization, distributed lease cancellation, or
cross-machine accounting consensus.

## Native orchestration and external MCP connectors

Little Monkey owns the multi-agent workflow runtime, local and resident
coordinators, task state, model routing, permissions, and peer messaging.
All installed features described here are implemented within Little Monkey.

Little Monkey also supports **user-configured MCP servers** through Settings →
MCP. Third-party commands are optional external processes, not part of the
native swarm implementation. Operators must review packages, pin versions,
configure a restrictive tool allowlist, and explicitly approve connections.
External stdio servers execute outside the model-workspace sandbox.

For local lifecycle automation, **Settings → Background Agents → Agent
swarms** offers a bounded command-hook manifest importer. A JSON manifest
is parsed without execution, unsupported events and policy options are
rejected, and each command is shown for approval before installation.

The system still requires live validation on real configured models and
paired installations before being described as production-ready. See
[Swarm release acceptance](swarm-release-acceptance.md) for the implemented
capabilities, tested boundaries, and remaining acceptance gates.

## Test commands

```bash
pnpm test -- src/lib/swarmPlanning.test.ts src/lib/swarmSpecialistMetrics.test.ts src/lib/swarmPeerMetrics.test.ts src/lib/swarmConsensus.test.ts src/lib/swarmNotebook.test.ts src/lib/swarmKnowledge.test.ts src/lib/swarmPresets.test.ts src/lib/workflow.test.ts
pnpm typecheck
```

GitHub Actions CI is also required for repository acceptance. Passing mocked
workflow tests establishes scheduling contracts, **not** that a particular
local model will successfully complete a real codebase-level swarm.

## Manually approved resident implementation graphs

In **implementation** mode, the same reviewed Settings handoff can compile a
resident graph with code agents **only** if the operator selects an existing
recipe already configured with `manual` permissions. Every code task keeps
`manual` approval, each read-only task remains `plan`, and graph
parallelism is forced to **one** to avoid simultaneous edits to a shared
workspace. No remote execution, hidden worktree isolation, auto-push,
deployment, or automatic side-effect retries are allowed. Multiple code
phases are supported serially (implementation followed by test execution),
and each dependent task receives bounded verified prerequisite evidence.

This is a durable job graph, **not** permission to edit while the operator is
absent: write requests may pause until approved. A process interruption after
an external mutation requires human reconciliation before retrying; exact-once
external side effects cannot be guaranteed. Watch the daemon's approval and
reconciliation states before treating work as complete.

## Evaluated specialist quality — all domains

The supplemental specialist in each of security, frontend, backend, performance,
and general may optionally use independently verified source-fixture accuracy
instead of completion-based exploration. Each domain has three fixed eligible
roles, two defective source cases, and a safe negative control. No task is
graded on its own self-reported success. Import and opt-in are scoped to one
domain, with migration of previously stored security-v1 results. Core
workflow agents, peer placement, tool permissions, and other domains are never
promoted by imported scores.

Run `node scripts/swarm-specialist-live-benchmark.mjs --self-check --domain all`
for the no-model 15-fixture source-oracle check, or
`pnpm test:swarm-specialists:live -- --domain frontend --target ollama:MODEL`
with an installed model for real evidence (repeat for the other domains).
See `docs/swarm-release-acceptance.md` for per-domain invocation, result
import, and operational release gates.


# Universal Autonomous Tasks

The Autonomous Task panel and `monkey task` commands run one objective through a bounded coordinator. A task freezes its model target, workspace roots, permission policy, and budgets at creation. The coordinator creates a validated DAG, selects a direct, planned, delegated, or parallel-delegated strategy, schedules independent worktree workers, integrates patches serially, runs configured verification, performs a diff review, and records acceptance evidence before delivery.

Every coordinator transition is a `task_event` in the hash-chained run ledger. Events carry a replayable task snapshot, so restart and CLI attachment do not depend on React state. The Rust protocol validates task identifiers, event names, and payload size; the ledger projects the event without interpreting task-specific business state.

Execution placement is an executor concern layered around this coordinator.
Select Automatic or a configured target in the task panel; the selected target
identity and workspace transfer are frozen into each placed `RunSpec`. The
workspace is provisioned on the executor and remote results are represented as
reviewable artifacts. See [Execution targets](execution-targets.md) for Docker,
SSH, transfer, conflict, and trust details.

## CLI

```text
monkey task start "objective" --target ollama:model [--workspace path] --json
monkey task status [--run-id id] --json
monkey task attach id [--follow] --json
monkey task guide id "new constraint"
monkey task pause id
monkey task resume id
monkey task cancel id
```

CLI control operations append to the same durable run stream. `start` is queued with a read/write workspace grant and network/external mutations disabled by default; delivery still requires the relevant repository policy and explicit confirmation.

## Safety and recovery

External issue text is marked untrusted in worker prompts. Workers receive immutable snapshots and cannot expand roots, permissions, target, or budgets. Parallel mutation nodes require disjoint scopes and managed worktrees; integration is sequential and leaves conflicts visible. Cancellation preserves artifacts, verification evidence is staleable, and incomplete or non-authoritatively verified work is never reported as success.

The routing corpus in `src/lib/autonomousTaskEval.ts` is a deterministic smoke harness with sixteen fixture classes. It validates strategy selection and is intentionally separate from model-quality evaluation. Real repository checks remain authoritative only when the configured verification command returns success.

The desktop coordinator reattaches to non-terminal snapshots after restart; in-flight nodes are recovered as pending and completed worktree metadata is reused for integration. CLI `task start` submits an immutable recipe to the resident daemon, which supervises the headless task process; `task attach` reads the shared ledger and control commands append to it.

Issue-to-PR uses the same bounded plan and authoritative repository-check contract through `runIssueToPrAutonomousTask`. Its owned worktree is deliberately not applied to the shared checkout, and GitHub delivery/merge remain outside the worker behind the existing confirmation workflow. A task with no configured verification command remains `WAITING_USER`; a worker report or ordinary review prose cannot satisfy acceptance evidence.

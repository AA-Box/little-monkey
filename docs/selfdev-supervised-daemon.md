# Supervised selfdev daemon upgrade (isolated identity only)

The existing `selfdev-transaction.mjs` builds a verified native CLI from clean
source and stages the SHA-256-pinned candidate. The additional
`selfdev-supervised-daemon.mjs` defaults to an operator-approved **cold** deployment:

```sh
node scripts/selfdev-transaction.mjs build --repo /path/to/little-monkey
node scripts/selfdev-transaction.mjs activate --confirm-sha <initial-good-sha>
node scripts/selfdev-supervised-daemon.mjs enroll --confirm-sha <initial-good-sha>

# After building/staging a reviewed second version:
node scripts/selfdev-supervised-daemon.mjs upgrade --confirm-sha <new-sha>
node scripts/selfdev-supervised-daemon.mjs status
```

The supervisor requires the exact known SHA, the original selfdev manager root,
an exclusive local lock and dedicated, random `selfdev-...` profile. It
checks no service exists before enrollment. The service retains its own
persisted state in an isolated agent home, never adopts the default-profile
service and does not modify the signed desktop executable. The local
authorization here is OS-user and SHA approval; it is **not remotely
authenticated daemon control**.

A cold upgrade now enters a persistent `monkey daemon drain --enable` state.
The daemon completes active jobs normally without admitting new work. Queued,
paused, waiting-approval and pending run snapshots remain in the profile's
SQLite store rather than being cancelled by `daemon stop`. Once all active
jobs finish, the supervisor switches the verified binary, reinstalls the same
profile-owned service, checks the heartbeat/PID and that pending queue counts
have not decreased while the drain was held, and finally executes
`monkey daemon drain --disable` to resume pending jobs. On failure, it restores the previous
immutable binary and OS service, independently checking health before
reporting a rollback. If recovery fails, execution stops with a visible
manual-recovery error.

```sh
node --test scripts/selfdev-supervised-daemon.contract.test.mjs
```

**Outstanding:** transparent transfer of an in-flight model stream (not
attempted: active tasks are allowed to finish before a swap), remote
cryptographically authenticated orchestration, real provider handoff across
the restart, and hardware/OS service acceptance. The mocked contract tests cover the state machine,
not a real daemon hot reload. These operations explicitly fail closed rather
than abandoning a user's active job.

## Opt-in Linux live handoff (selfdev identity only)

The original provider connection belongs to a **separate `monkey task run`
process**, not to the resident daemon. A live upgrade keeps that task process
alive while systemd replaces the daemon, and the new daemon takes over the
original task's supervision. This is in-process *stream preservation*, **not**
a transfer of an open HTTP/TLS socket or GPU state to a new process.

Only an explicitly enrolled `selfdev-...` profile on **Linux systemd user
service** is eligible. macOS launchd, Windows Scheduled Tasks, ordinary
profiles, ambiguous worker identities, missing per-run leases and previous
owners that are still alive are all excluded. Cold recovery remains unchanged
for those environments.

```sh
# Only when starting a new isolated profile; opt-in is nonretroactive.
node scripts/selfdev-supervised-daemon.mjs enroll \
  --live-handoff --confirm-sha <approved-initial-sha>

# Start a real, long-running model task on this isolated profile.
# Then, while it is actively generating, stage a verified candidate as usual.
node scripts/selfdev-supervised-daemon.mjs upgrade-live \
  --confirm-sha <approved-new-sha>
```

On Linux, the opt-in service unit uses `KillMode=process` and explicitly
passes `LITTLE_MONKEY_DAEMON_LIVE_HANDOFF=1` to its daemon. Each spawned task
has its **original PID plus kernel process-start ticks** durably bound to the
job/run ID before it is marked running. The task's output is already in its
own log and event ledger; those event cursors can be replayed by a reconnected
controller. Old process group, provider TLS connection, and task/tool loop
continue uninterrupted while the original daemon is stopped.

The new daemon checks worker birth identity, verifies the old daemon PID has
exited, **atomically rotates the execution lease and generation**, and adds the
*original process* to its supervision map. It resumes the same PID's deadlines,
kill switch, signals, limits and terminal ledger reconciliation. It does
**not** submit a new LLM request or rerun non-idempotent tools.

The supervisor fails unless the previous live workers are attested and
the replacement daemon's status proves all of the following: new daemon PID,
recovery-owned worker count, live verified worker presence, exact
`job_id:PID:kernel_birth` identity overlap across the restart, and preserved
durable queued work. Rollback restarts the prior binary against the same
surviving workers, never blindly reexecutes their frozen recipes.

**Safety and limits:** `KillMode=process` intentionally allows orphaned
user-owned child processes to outlive a crashing daemon. A manually killed
daemon with no replacement could leave running work; the operator must use
the supervised upgrade and should retain an independent emergency process
kill mechanism. Corrupt, missing or mismatched identity fails closed, and
unverified PID reuse is never signaled. A failed provider stream is not
magically resumed at the network layer; the guarantee is preservation of a
still-running worker through a *planned* daemon update. Power loss, OS
shutdown, crashed provider worker and unsupported service managers are not
covered.

Native CI exercises Linux PID birth attestation and actual same-process
engine reattachment without restarting the provider call, but full production
acceptance additionally requires a genuine installed Linux user service and
a slow, streaming model/provider response observed across
`upgrade-live`. Do not claim this physical acceptance until that real
measurement has been performed.

## Physical end-to-end provider stream acceptance

Run a genuinely slow, already-started **model-backed** task in the isolated
live-enabled service. Observe at least two durable `model_delta` events
before invoking this acceptance command:

```sh
python3 scripts/selfdev-live-provider-stream-acceptance.py \
  --root ~/.littlemonkey/selfdev \
  --run-id REAL_RUNNING_DAEMON_RUN_ID \
  --candidate-sha SHA256_OF_REVIEWED_CANDIDATE \
  --out ./private-live-handoff-evidence.json
```

This command calls `upgrade-live` while the real provider request is active,
checks the exact original OS worker PID and kernel start identity on both
sides, verifies that the new daemon adopted it, and reads the **real**
`run_events` SQLite ledger read-only. It refuses success unless new streamed
`model_delta` events arrive **after the new daemon became healthy**, the
original run reaches a terminal event, and no second `started` event was
produced. It never prints the private provider content, credentials, or
tool results; the report contains event counts and process metadata only.
A single failed gate writes no acceptance artifact. The script may initiate
an approved production-like service upgrade and must only be invoked on an
operator-controlled selfdev profile.

**Do not equate the simulated OS-process CI test with this physical real-model
acceptance.** Genuine streaming verification requires running this script
against a real installed local service and provider; it has not been executed
on the GitHub-only implementation host.

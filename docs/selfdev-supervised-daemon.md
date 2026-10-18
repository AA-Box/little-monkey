# Supervised selfdev daemon upgrade (isolated identity only)

The existing `selfdev-transaction.mjs` builds a verified native CLI from clean
source and stages the SHA-256-pinned candidate. The additional
`selfdev-supervised-daemon.mjs` is an operator-approved **cold** deployment:

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

A cold upgrade refuses **all active, queued, paused, approval-waiting jobs and
managed run IDs**. It stops the old dedicated service, activates the new
staged binary, installs the profile's service, and demands fresh heartbeat,
service health and a changed PID. On failure, it restores the previous
immutable binary and OS service, independently checking health before
reporting a rollback. If recovery fails, execution stops with a visible
manual-recovery error.

```sh
node --test scripts/selfdev-supervised-daemon.contract.test.mjs
```

**Outstanding:** safe live session drain/handoff, seamless continuation of
in-flight model streams, remote authenticated orchestration, and hardware/
OS service acceptance. The mocked contract tests cover the state machine,
not a real daemon hot reload. These operations explicitly fail closed rather
than abandoning a user's active job.

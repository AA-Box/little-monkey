---
name: Dependency Auditor
description: Flag outdated or risky dependencies across the workspace
command: dep-audit
version: 1.0.0
requires:
  bins: []
  env: []
---
Detect which manifests are present in the workspace: `package.json`, `Cargo.toml`,
`requirements.txt` / `pyproject.toml`, `go.mod`. For each one found, run its
ecosystem's read-only check — `npm outdated` and `npm audit --production`,
`cargo outdated` and `cargo audit`, `pip list --outdated`, `go list -u -m all` —
but only if the corresponding binary is actually on PATH. Skip an ecosystem
silently if its tool is missing rather than failing the whole run.

Never run an install, update, or fix command. This skill reports; it does not
mutate `node_modules`, the lockfile, or `Cargo.lock`.

Summarize findings as a table: package, current version, latest version, and why
it matters — a known security advisory outranks plain version drift. Call out
major-version bumps separately since they may carry breaking changes the audit
tool doesn't flag.

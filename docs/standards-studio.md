# Standards Studio

Standards Studio is Little Monkey's structured engineering-standards layer. It is deliberately separate from `MONKEY.md`/`AGENTS.md` standing instructions and from Skills.

## Lifecycle

`Discover → evidence → candidate → review → approve → select → inject → drift`

Discovery is deterministic and bounded before any model is involved. It inspects repository-owned configuration and conventions through Little Monkey's existing workspace-confined file commands, currently including package/test frameworks, verification scripts, formatter/linter configuration, Cargo, CI workflows, and predominant test-file layout.

A discovered candidate is **never authoritative by itself**. It records supporting evidence (path, line when known, excerpt, SHA-256, evidence class) and explicit counterexamples where the detector found a competing pattern. The user approves, rejects, or later deprecates candidates in **Settings → Standards Studio**.

## Storage

Repository standards are portable and live at:

```text
.little-monkey/standards/index.json
```

The document is schema-versioned. Standards have stable IDs, versions, severity, applicability metadata, evidence, confidence, provenance, approval timestamps and drift state.

All workspace reads/writes go through the same Rust workspace resolver and permission boundary used by agent file tools. Standards Studio does not broaden Tauri's renderer filesystem scope. Portable import/export stays inside `.little-monkey/standards/` for the same reason:

```text
.little-monkey/standards/import.json
.little-monkey/standards/export.json
```

Copy these files with the normal OS file manager when moving them between repositories/machines.

## Selection and injection

Approved standards are not dumped into every prompt. The selector ranks them from:

- severity (`required` first)
- task keyword overlap
- language/framework applicability
- file hints when available
- drift state

It applies a bounded character budget and records the selection reasons. The Studio's **Injection preview** uses the same selector as normal agent turns.

At turn time, the existing rules refresh also refreshes the standards index. The system prompt includes only the approved standards selected for the current task.

Standards are guidance and verification constraints. They cannot grant tools, network, secret access, additional budget, a different permission mode, or any other execution authority.

## Drift

`Check drift` re-hashes the evidence files through the workspace boundary:

- `healthy`: all supporting evidence is unchanged
- `weakened`: some supporting evidence changed/disappeared
- `contradicted`: no supporting evidence remains unchanged

A contradicted approved standard becomes `stale` and is no longer injected. Standards Studio never silently rewrites or re-approves it.

## Security properties

Repository content is evidence, not authority. A README/config string that tells an agent to upload credentials cannot widen a standard's authority or bypass Little Monkey permissions. Discovered standards require explicit approval, and the injected prompt states the same authority boundary.

The deterministic discovery scan is bounded by file count, recursion depth and evidence size, skips dependency/build/VCS directories, and silently ignores binary/unreadable inputs.

## Current scope

This first production implementation intentionally focuses on repository standards that can be supported by deterministic local evidence. Model-assisted synthesis, mechanically executable custom checkers, and Agent OS import adapters should be added only when they can preserve the same evidence, authority and bounded-execution guarantees rather than turning a standards file into executable configuration.

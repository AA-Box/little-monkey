# Standards Studio

Standards Studio is Little Monkey's structured engineering-standards layer. It is deliberately separate from `MONKEY.md`/`AGENTS.md` standing instructions and from Skills.

## Lifecycle

`Discover → evidence → candidate/revision → conflict review → approve → select → inject → drift`

Discovery is deterministic and bounded before any model is involved. It inspects repository-owned configuration and conventions through Little Monkey's existing workspace-confined file commands, currently including package/test frameworks, verification scripts, formatter/linter configuration, Cargo, CI workflows, and predominant test-file layout.

A discovered candidate is **never authoritative by itself**. It records supporting evidence (path, line when known, excerpt, SHA-256, evidence class) and explicit counterexamples where the detector found a competing pattern. The user approves, rejects, or later deprecates candidates in **Settings → Standards Studio**.

Rediscovery never silently rewrites an approved policy. If candidate policy content changes, its revision number advances; if an approved policy's evidence/content no longer matches discovery, the approved text stays frozen and its drift becomes `weakened` until the user deliberately resolves it.

Manual/imported standards can declare `conflicts_with` IDs. Standards Studio marks both active sides as `conflicting` rather than letting ranking arbitrarily pick a winner. Unresolved approved conflicts are never injected.

## Storage

Repository standards are portable and live at:

```text
.little-monkey/standards/index.json
```

The document is schema-versioned. Standards have stable IDs, versions, severity, applicability metadata, evidence, confidence, provenance, approval timestamps, explicit conflicts/supersession, a content SHA-256 and drift state.

All workspace reads/writes go through the same Rust workspace resolver and permission boundary used by agent file tools. Standards Studio does not broaden Tauri's renderer filesystem scope. Portable import/export stays inside `.little-monkey/standards/` for the same reason:

```text
.little-monkey/standards/import.json
.little-monkey/standards/export.json
```

Copy these files with the normal OS file manager when moving them between repositories/machines.

## Selection and injection

Approved standards are not dumped into every prompt. `required` standards are repository-wide gates; `recommended` and `informational` standards require a concrete task/language/framework/file relevance signal. Relevant standards are then ranked from:

- severity (`required` first)
- task keyword overlap
- language/framework applicability
- file hints when available
- drift state

The selector applies a bounded character budget and records the selection reasons. The Studio's **Injection preview** uses the same selector as normal agent turns.

At turn time, the existing rules refresh also refreshes the standards index. The system prompt includes only the approved standards selected for the current task. Its frozen selection record contains each selected standard ID, version, full content digest, severity, drift, score and reasons; each rendered standard also includes evidence path/line plus an evidence digest prefix. This makes the exact policy snapshot visible in the prompt/run material instead of reconstructing it from a later workspace state.

Standards are guidance and verification constraints. They cannot grant tools, network, secret access, additional budget, a different permission mode, or any other execution authority.

## Executable checker bindings

An approved selected standard may bind to an existing Verification command by command ID. The selected checker IDs are frozen with the task and carried through both the normal in-process turn and the resident daemon recipe, so a later settings change cannot silently change which checker that accepted task requires.

Bound Standards checkers are completion gates, not a synonym for the global Verification toggle. After a workspace mutation, required bound checkers run even when global Verification is disabled. A failing checker is fed back through the bounded verification-fix loop and is re-run after that corrective round even if the model makes no additional edit. If the checker still fails when the fix budget is exhausted, or if its bound command is missing or disabled, the turn cannot report successful completion.

Global Verification remains independent: when enabled it continues to run the workspace's enabled verification commands in addition to any checker IDs required by the frozen Standards selection.

## Drift

`Check drift` re-hashes the evidence files through the workspace boundary:

- `healthy`: all supporting evidence is unchanged
- `weakened`: some supporting evidence changed/disappeared, or rediscovery proposes changed policy content
- `contradicted`: no supporting evidence remains unchanged

A contradicted approved standard becomes `stale` and is no longer injected. Standards Studio never silently rewrites or re-approves it.

## Security properties

Repository content is evidence, not authority. A README/config string that tells an agent to upload credentials cannot widen a standard's authority or bypass Little Monkey permissions. Discovered standards require explicit approval, and the injected prompt states the same authority boundary.

The deterministic discovery scan is bounded by file count, recursion depth and evidence size, skips dependency/build/VCS directories, and silently ignores binary/unreadable inputs.

Conflict handling is explicit rather than model-guessed. Selection is deterministic for a fixed standards document/task/file-hint set, and the selected content digests are frozen into the injected standards section.

## Scope and extension points

The production path is intentionally evidence-first. New detector/checker or Agent OS adapter integrations should emit the same structured candidate/evidence model and still require the same approval lifecycle; an imported adapter must never become an executable permission mechanism. Executable checker bindings may reference only existing Verification commands and therefore inherit Little Monkey's bounded execution and permission model; standards JSON never becomes arbitrary command-execution authority.

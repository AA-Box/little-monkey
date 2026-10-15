# Operator-controlled ambient memory gardening

This adds a **narrow, independently safe housekeeping operation** to the existing
ambient daemon. The ambient worker itself still runs a frozen PLAN-only,
read-only review recipe. A new flag authorizes only a deterministic,
same-project cleanup of byte-identical **agent-authored** memories.

## Enroll

Given an existing, operator-reviewed read-only recipe and a canonical
workspace, run:

```sh
monkey daemon swarm-ambient-install garden-01 \
  --source-recipe /absolute/path/to/plan-only.json \
  --workspace /absolute/path/to/your/project \
  --garden-agent-memories
```

The operation is OFF for every existing policy. The durable SQLite schema
migrates older policies with `garden_agent_memory=0`. Omit the flag
to keep the earlier read-only proposal behavior. No user-owned memory is
ever rewritten by this task.

Inspect `monkey daemon swarm-ambient-status` for
`garden_agent_memory` and `garden_merges_total`. Disable future runs with:

```sh
monkey daemon swarm-ambient-disable garden-01
```

## Guarantee

When an enrolled ambient policy is due, the daemon obtains the same
cross-process lock as Memory Studio and the `remember` tool, reloads
`memories.json`, and atomically consolidates at most eight groups per tick.
Only EXACT duplicate text, after trimming outer whitespace, qualifies.
Each group must be unpinned, enabled, non-expiring and agent-authored,
with no existing merge ancestry. A new fact retains the text verbatim,
records the IDs of all originals in `merged_from`, and soft-retires
the originals. Normal Memory Studio unmerge restores them.

The entire operation is canonical-workspace scoped; it cannot merge
memories from another project or the global scope. A failed write returns
an error; no number of successful merges is fabricated. A repeated tick
is idempotent for already-merged groups.

This makes a real, useful part of ambient gardening functional without
a generative model deleting preferences or inferring contradictions.
It **does not** claim semantic paraphrase deduplication, contradiction
resolution, cross-session missed-extraction backfill, autonomous
task execution or a model-judged quality gate. Those need separate
evidence-backed designs and acceptance, not an unsafe automatic edit.

## Verification

```sh
cargo test --manifest-path src-tauri/Cargo.toml ambient_exact_garden_tests
```

Production acceptance requires an explicitly installed ambient policy,
a live daemon tick, a real duplicate in the opted-in canonical project,
a stable `garden_merges_total` counter, and successful unmerge through
Memory Studio after process restart. The unit tests do not constitute
this physical acceptance.

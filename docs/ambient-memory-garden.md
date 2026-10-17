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


## Conservative natural-language consolidation and unresolved conflicts

Enrolled gardeners additionally consolidate unpinned, enabled, unexpired
agent-owned statements where the **full sentence** differs only in
whitespace, Unicode case or a final period. That is deliberately a narrow
semantic equivalence: URLs, code, paths, quotes, special markup and sentence
questions are excluded. Merged originals are soft-retired and reversible
through the existing Memory Studio unmerge mechanism.

The curator also recognizes a small set of explicit contradictory pairs,
including "User prefers X" vs "User does not prefer X", without changing
either side. The ambient status includes \`garden_conflicts_latest\` so the
operator can investigate, instead of silently choosing a winner. Conflict
review is **not** autonomous truth selection, broad paraphrase clustering
or reasoning over uncertain temporal claims. That remaining work needs
grounded evidence, a reliable independent quality gate, and an explicit
user-controlled dispute-resolution policy.

\`\`\`sh
cargo test --manifest-path src-tauri/Cargo.toml semantic_garden_tests
\`\`\`


## Human-confirmed contradiction resolution in Memory Studio

When two live **agent-authored** facts in the same project are unambiguously
opposite under a supported negation pattern, Memory Studio displays both
statements together. The operator can choose **Keep first statement** or
**Keep second statement**. Only the rejected fact is soft-disabled, via the
same authoritative native \`memory_studio_set_enabled\` call as any other
manual memory edit. The retained fact, any user-owned fact and every
other project's memory remain unchanged. Disabled facts remain visible and
can be re-enabled in Memory Studio.

This preserves the user's final authority over contradictory information
rather than silently letting an LLM decide which preference or project
statement is true. UI tests verify that the selected project fact is
disabled and unrelated/user-owned facts are preserved.

\`\`\`sh
pnpm exec vitest run src/components/Settings/MemoryStudioPanel.test.tsx
\`\`\`


## Model-assisted paraphrase review in Memory Studio

Open **Settings → Memory Studio** and click
**Review paraphrases with selected model**. Unlike the resident automatic
duplicate pass, this is a *deliberately initiated* model request:
up to 24 active, unpinned, non-secret, agent-authored facts from the SAME
scope may reach the selected model. Cloud targets still invoke the
regular outbound privacy firewall, cost budget, and usage ledger; local
model requests stay local. The UI explains that memories may be sent to a
configured cloud provider before the operator invokes the review.

The model can propose genuinely paraphrased memory merges (e.g. "likes
brief recaps" with "prefers short summaries") or two contradictory facts
that require adjudication. Returned IDs are checked against the exact
in-memory set; duplicate IDs, cross-scope proposals, excessive output,
credentials and malformed JSON fail closed. Nothing is persisted from an
unreviewed model answer.

Each proposal shows the original text, selected scope, model rationale,
and new merged wording. Only **Approve reversible merge** writes through
the existing canonical Rust merge API. For contradictions, the operator
must explicitly choose which statement remains enabled; the other is
soft-disabled and may be restored in Memory Studio. The app checks the
original IDs, texts and lifecycle states immediately before an approval
to detect intervening changes. Proposals can always be dismissed.

\`\`\`sh
pnpm exec vitest run src/lib/semanticMemoryReview.test.ts
pnpm exec vitest run src/components/Settings/MemoryStudioPanel.test.tsx
\`\`\`

Semantic *evidence* remains model-judged: a review proposal is **not**
independent proof of equivalence. The user must examine and authorize
meaning-changing edits. There is deliberately no unattended auto-resolution
of paraphrase ambiguities, medical claims, or conflicting memories.

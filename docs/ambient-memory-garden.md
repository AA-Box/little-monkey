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

## Evidence-backed background candidate queue (opt-in)

Each due ambient worker configured with `--garden-agent-memories` now also
performs a **read-only**, workspace-scoped review pass. It uses two distinct,
conservative lexical evaluators (word Jaccard and character-trigram Dice) to
flag *possible* paraphrases where both agent-created facts have nonempty
original source-turn IDs. It skips pinned, user-authored, disabled, expired,
retired, already-merged, and obvious negation-conflicting statements. This
algorithm makes **no model-semantic equivalence claim**; independent lexical
scores are candidate filters, not a proof of truth.

The daemon stores up to twelve deterministic candidate ID pairs with exact
source-turn identifiers, SHA-256 hashes of the original fact texts, the two
scores and `requires_user_approval=true`. The latest snapshot is returned in
`monkey daemon swarm-ambient-status` under
`garden_evidence_candidates`. Raw memory text is not copied to the daemon
status database. Memory Studio remains the ONLY way to compare original
texts and authorize a guarded merge. A stale review cannot override current
fact text: the existing Memory Studio guarded merge checks original contents
under a cross-process lock before changing anything. The daemon itself never
applies ambiguous proposals. All changes are opt-in and no new network model
calls are introduced.

```sh
cargo test --manifest-path src-tauri/Cargo.toml review_candidates_are_scoped_evidenced
cargo test --manifest-path src-tauri/Cargo.toml evidence_manifest_schema_is_durable
```

**Unresolved acceptance:** independently verified model-based semantic
judgment, end-to-end human approval flow surfaced directly from daemon
candidate IDs, longitudinal real-model quality evaluation, and live daemon
restart acceptance. The current evidence queue does not by itself satisfy
those requirements.

## Independent reviewer configuration and observed quality

Memory Studio now requires the operator to choose an explicit second provider
model with a configured credential. The primary selected model proposes; a
different provider/model identity independently evaluates the original facts
in reverse order. Only strictly agreeing candidate identities **and** merge
summaries are surfaced for human approval. The verifier never receives the
proposer output. A missing credential, an unselected verifier, or identical
model identities fails closed. Calls reuse ordinary provider privacy,
budget and usage accounting.

The app separately records counts of actual proposed, approved and dismissed
suggestions in local-only storage. No memory contents, IDs, workspace names,
raw prompts or credentials are retained in the metrics. The UI labels the
approval fraction as *human agreement among decided suggestions* and does not
misrepresent it as ground-truth semantic accuracy, precision or recall.

```sh
pnpm exec vitest run src/lib/semanticMemoryReview.test.ts src/lib/semanticMemoryQuality.test.ts
pnpm typecheck
```

Independent quality acceptance still requires a separately curated, consented
ground-truth set and provider-backed live comparisons. These were not run in
the repository authoring environment and have no manufactured scores.

## Live semantic precision and recall acceptance

`scripts/semantic-memory-live-quality.py` requires **20 or more independently
human-annotated private cases**, at least five positive and five negative
pair labels. It makes actual, separate provider calls for each case (one
proposer, one verifier), requires the returned model identities to match
the configured distinct names, and computes precision, recall, and F1 for
equivalent-memory merges and contradictory-memory flags.

Example private JSON structure (illustrative only; *not acceptance data*):

```json
{
  "human_annotated": true,
  "annotator_id": "independent-reviewer-identifier",
  "cases": [
    {"id": "case-1",
     "facts": [{"id": "a", "text": "The user prefers short updates."},
               {"id": "b", "text": "The user prefers concise updates."}],
     "truth": [{"kind": "merge", "ids": ["a", "b"]}]}
  ]
}
```

Complete the corpus with at least 20 unique, genuinely labeled cases, including
cases whose `truth` is `[]` (hard negatives). Truth labels must be exhaustive
within each case; do not derive them from the model being graded.

```bash
python3 scripts/semantic-memory-live-quality.py \
  --gold ./private/human-labeled-memories.json \
  --proposer-url http://127.0.0.1:11434/v1/chat/completions \
  --proposer-model MODEL_A \
  --verifier-url http://127.0.0.1:11434/v1/chat/completions \
  --verifier-model MODEL_B \
  --output ./private/semantic-quality-evidence.json
```

Models must actually be installed and running; the evaluator never fabricates
completions or labels. API keys for authenticated endpoints may be passed in
`LM_EVAL_PROPOSER_API_KEY` and `LM_EVAL_VERIFIER_API_KEY`.
Sending memory facts to non-loopback endpoints requires explicit
`--allow-remote-sensitive-facts`. Scores persist **only** case counts, source
SHA-256, model IDs and TP/FP/FN metrics, never plaintext facts or credentials.
One failed request means **no successful quality report**. CI tests only the
scoring implementation, not provider accuracy.

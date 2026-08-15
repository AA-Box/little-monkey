# Features

What Little Monkey does today on the `develop` tree. Where a feature is
narrower than its name suggests, the boundary is in [Limitations](limitations.md).

## Chat and collaboration

- Chat against managed `llama.cpp`, Ollama, MLX, or configured cloud/BYOK providers, with capability-aware routing, provider failover, context compaction, usage accounting, and rate-limit warnings.
- Compare one frozen prompt across two to four explicit targets with independent streaming, stop, retry, timing, usage, persistence, and response promotion. Compare runs default to no tools and keep their target snapshots when global model settings change.
- Choose a per-turn reasoning effort (Default through Max, plus Ultracode). Ultracode fans one turn across up to four available models through the Compare pipeline and runs a synthesis pass when the branches settle; it is front-end state only and never reaches the Rust-validated effort wire type.
- Run saved Crew chats with a coordinator and bounded parallel persona members. Member transcripts stay isolated, coordinator synthesis is explicit, actor usage is attributed, and cancel-all reaches outstanding members.
- Ask a side question with `/btw`. It renders as an aside, records no session usage, and every wire builder (agent loop, Compare, Crew) strips it from later turns, so neither question nor answer reaches a model again.
- Keep multiple sessions, forks, groups, and a two-pane split view with independent streams.
- Attach files, folders, and images; reference workspace paths with `@`; select personas and knowledge stacks; invoke skills with `/`.
- Search active and archived chats, messages, tool output, artifacts, and durable runs with date, model, persona, and workspace filters.
- Export a session as Markdown, JSON, or Word, translate individual messages or a whole thread while retaining the original, and create versioned portable backups.
- Create encrypted local snapshots with retention, preflight imports before changing live state, and use encrypted WebDAV backup with conflict copies and launch-time catch-up. Unattended backup runs through the installed daemon.
- Read a round's tool activity without expanding it: the summary folds by file, naming what was touched and each verb it received, plus the round's net line delta from applied calls. Opening a round lists steps that expand to command, diff, and output, each with a copy action; subagent steps are titled by the child's own narration.

## Workspace: files, review, terminal, and browser

- Reopen into the folders you were working in. The attached set is snapshotted on change and reattached at launch; folders deleted or moved since the last run are dropped rather than blocking the restore. Permission grants stay session-scoped and are never restored with a workspace.
- Work across eight right-sidebar tabs — code review, single-file diff, terminal, browser, side tasks, workspace files, background tasks, and processes. Tabs stay mounted, share one drag-resizable persisted width, support a region-wide fullscreen toggle, and each has a keyboard shortcut on every platform.
- Review changes in a git-backed panel using real porcelain output, with a per-file diff view and PR awareness. Pick the base — the branch's merge-base with its upstream, or HEAD — and the layout — every diff stacked, or one file at a time. Against HEAD the file list is uncapped and each diff loads on open; against the merge-base the panel is bounded by a 300-file payload cap and says so.
- Map acceptance criteria onto that diff. Paste the criteria and each returns covered, partial, or not covered with clickable citations into exact hunks. Facts computed from git — changed files, numbered hunks and line ranges, added or removed exported declarations, and a digest of all of it — are rendered separately from model claims about them, and every claim is checked against those facts: a claim citing a hunk the diff does not contain is discarded with the invented id shown, and a coverage claim without a valid citation is marked unsupported. Headline counts come from set arithmetic over surviving claims.
- Run a real terminal. Keystrokes go to the PTY through an embedded xterm.js emulator, so the shell supplies its own prompt, colors, line editing, history, and completions. A session auto-starts per workspace, with dock-right, drag-to-resize, and fullscreen.
- Browse from an in-app tabbed pane backed by real child webviews: tab strip, favicons and loading state, smart address bar, back/forward/reload, and `window.open` reopened as a tab. Only `http:`, `https:`, and `about:` load, and remote pages get no Tauri IPC surface.
- Track live background work from a running-tasks pill above the composer. The Background Tasks panel separates running from finished, offers per-card stop, token and tool-use counts, and an inline transcript. Parallel subagent calls in one turn collapse into a group card with per-agent status; each cancels independently of the parent turn.

## Agent tools, permissions, and egress policy

- Use workspace-scoped file read/list, glob, grep, edit/write, shell, memory, web fetch/search, knowledge search, MCP, subagent, plan, and verification tools.
- Choose `manual`, `plan`, `acceptEdits`, `smart`, `auto`, or `bypass` permission modes. Sensitive paths carry a deterministic risk floor, shell execution stays behind the stronger policy, and unattended recipes cannot use `bypass`.
- Checkpoint every mutating turn, then revert or re-apply file changes, rewind the conversation, or both. Preview a checkpoint's per-file diff, artifacts, screenshots, and verification state before restoring; compare any two read-only; run a rollback simulation that separates file, artifact, conversation, and external state and marks effects that cannot be safely undone as `needs_reconciliation`.
- Configure post-edit lint, build, and test commands. Failures can return to the model for a bounded repair loop and are recorded as verification events.
- Treat retrieved pages, RAG chunks, MCP results, subprocess output, GitHub content, browser evidence, subagent reports, and other model output as untrusted data before it re-enters a prompt.
- Police outbound requests by rule. Every SSRF guard enforces a named rule, and a denial carries that rule plus per-request detail instead of one flattened error, so a blocked attempt is recordable with its cause. Search-backend redirects are pinned to their own origin, the model-source realm-redirect bypass is closed, audit detail is bounded where it is written, and the IPv4-compatible and IPv4-mapped forms, NAT64, IPv6 literals in browser navigation, and several non-routable ranges no longer reach a public verdict in any guard.
- Grant private-network access per address class rather than by one switch. The allowance covers the six classes a host you actually run can answer on — loopback, RFC 1918, link-local, unique-local IPv6, CGNAT, and the unspecified address — while multicast, broadcast, `0/8`, the documentation and benchmarking ranges, and `240/4` stay refused.
- Enforce a run's own network permission. `allow_network` is read where the provider endpoint is resolved from the frozen run spec, so a run submitted without it reaches no provider. The permission is frozen at submission with no update path, and cannot be widened at runtime by a model, skill, package, or routing decision. Loopback is exempt, since a local-inference run correctly carries `allow_network: false`.
- Bound every outbound client with a total deadline and a silence budget, so a peer that accepts a connection and then stalls cannot hang a download, a search, or the UI.
- Inspect local posture in **Settings → Security Doctor** or with `monkey security audit`: app-data permissions, API and webhook listeners, remote TLS posture, MCP origins, installed skill integrity, and active browser and companion grants, all without contacting a model. `--fix` is limited to private app-owned modes and disabling clearly unsafe listeners.

## Knowledge Stacks 2.0

- Ingest local files and folders, projects, URLs, sitemaps, selected chats, and configured WebDAV sources.
- Extract text and source locations from text and code, HTML, PDF, DOCX headings and tables, XLSX sheets and cell ranges, and PPTX slides and notes. Macros, formulas, embedded scripts, and automatic external-link execution are not enabled.
- Refresh incrementally with content hashes, connector cursors, deletion propagation, progress, cancellation, retry state, and optional daemon scheduling.
- Add local OCR through a verified or explicitly selected worker, with language, provenance, size, digest, license, progress, and cancellation controls.
- Fuse lexical retrieval with vector similarity and optional reranking, retaining the existing local vector path.
- Inspect retrieval end to end: normalized query, filters, candidates, lexical and vector scores, fused rank, reranker score, exclusions, token budget, and final context. Copy a reproducible diagnostic bundle or preview local PII and secret redaction.
- Import a Knowledge 1.0 stack as a v2 generation without re-embedding. Existing vectors, chunk text, and per-file digests are reused and no model is invoked. The import seeds a real v2 source per v1 source and the imported objects carry those ids, so they refresh and prune like any other; the first refresh re-extracts them with true v2 boundaries. Imports are all-or-nothing, the v1 index stays readable, and an unsupported v1 source kind refuses by name rather than being dropped.

## Runtime and API Hub

- Inspect CPU, memory, and runtime inventory, estimate model fit, search configured catalogs, resume verified downloads, activate or roll back model versions, prune old versions, clean owned orphan data, and load or unload supported runtimes.
- Manage versioned runtime components — the `llama.cpp` server, MLX runtime, tokenizers, converters, projector runtimes, and Metal/CUDA/ROCm/Vulkan support packages — on stable, beta, or pinned channels, separately from installed models: digest-verified installs, update checks, activate-to-roll-back with bounded retention, and per-version compatibility notes over a local, operator-editable registry.
- Read a Hardware Compatibility Matrix ("Driver Doctor") before any model download, load, or runtime install: real detection of Metal, CUDA, ROCm, Vulkan, and best-effort DirectML, plus driver version, compute capability, Jetson, and hybrid or multi-GPU detection, with an `available`, `not_detected`, `driver_too_old`, `tool_missing`, or `unsupported` status per backend that never fails merely because a GPU tool or device is absent.
- Track each installed model's source registry, license, quantization, chat template, and multimodal projector in a content-addressed, digest-verified manifest. An already-verified payload is reused across asset variants and versions instead of re-downloaded, and a corrupt local copy is never trusted for reuse.
- Manage Ollama, `llama.cpp`, and MLX through one runtime contract with capability preflight, owned-process shutdown, logs, metrics, cancellation, and resource-aware scheduling. A pull in progress is cancellable, and a managed start that fails before spawning anything reports the error instead of waiting on a status event that will never arrive.
- Author, live-parse, and dry-run validate a full Ollama Modelfile in Modelfile Studio, including GGUF and safetensors header checks and short-name hardening, before anything is installed into the model library.
- Simulate a per-load offload plan from the live hardware snapshot before a model loads: recommended context size, batch size, GPU layers versus CPU spill, projector placement, and parallelism, each with a rationale and concrete suggestions for raising the budget.
- Manage multimodal projectors. The offload plan reserves a projector's resident memory before sizing context and GPU layers, an installed version's projector can be digest-verified against a local candidate, and a vision-capable model with a missing or unverified projector is flagged rather than loaded silently. The OpenAI-compatible wire and the MLX driver both carry an inline base64 image block end to end.
- Serve the OpenAI-compatible routes (`/v1/models`, `/v1/chat/completions` with SSE streaming, `/v1/responses`, `/v1/embeddings`, tool calls, JSON-schema structured output), the Anthropic-compatible Messages subset, native-Ollama `GET /api/tags` and `POST /api/chat`, and separately scoped model discovery, download, load, unload, status, and delete routes — all under one LAN and loopback authentication, pairing, rate-limit, and CORS policy.
- Verify that surface with a compatibility matrix backed by real HTTP-level regression tests (`src-tauri/tests/m3_compatibility_harness.rs`) that start the actual server and exercise every route, surfaced as a live per-route, per-backend, per-model status view in **Runtime Hub → Compatibility** derived from the same capability state that gates real requests.
- Admit every serving surface through one path, including `monkey api-serve`, and stop accepted connections when a server stops rather than reporting stopped while admitted requests keep streaming.
- Keep loopback the default. Non-loopback serving requires an exact interface, TLS identity, authentication, pairing, rate limits, an exact CORS allowlist, explicit backends and scopes, and a policy that excludes file, shell, Git, MCP, and other agent-tool routes.
- Report context window and KV-cache state honestly: the configured or default context size always, plus live figures from a managed `llama.cpp` process's `/props` and `/slots` endpoints when reachable. Anything a runtime does not report is labeled `unavailable`. An effective-context preview tightens a requested size against the offload plan, model metadata, and runtime bounds without bypassing them, and long-context failures are classified as prompt-too-long, cache-exhausted or context-shift, memory-pressure, runtime-limitation, or model-metadata-limit.
- Harden streamed tool-call and structured-output parsing against malformed, truncated, and adversarial output: brace-in-string arguments, mid-UTF-8 and mid-escape chunk splits, dropped connections, duplicate or out-of-order fragment indices, and tool calls naming a tool the request never offered all fail closed.
- Store private keys and provider credentials in the OS keychain; persisted configuration holds references rather than key material.
- Warn about model retirement and compatibility before a run starts. A cloud model known-retired against a maintained local list gets a reason and a replacement from that provider's live list; an installed local model with a newer catalog revision and no recent refresh is flagged before its load step. Both are updatable local signals, not live upstream verification.
- Generate ready-to-use configuration for external agent tools and editors (Continue.dev, aider, or a generic OpenAI-SDK `.env`) from **Settings → Runtime Hub → Agents**, pointed at the app's real endpoint, an installed model, and a real paired token — and check an existing config for a stale model, moved endpoint, missing auth header, oversized context length, or telemetry default worth revisiting.
- Quantize an installed model or an arbitrary GGUF or safetensors path from the Quantization workbench: header sniffing, a heuristic license risk check, a per-quant-level size and quality reference, and a real `llama-quantize` backend when present (an honest copy-only passthrough otherwise), with a reproducible report carrying source and output digests and a GGUF-parses check.
- Run the Chat Template and Renderer Compatibility Lab over real tool-call, system-prompt, stop-token, and structured-output fixtures against this app's own OpenAI-compatible renderer and the MLX driver's message flattening, grouped by chat-template family. Capability badges appear only once the matching fixtures pass for a model's declared template.
- Scan `ollama/ollama`'s closed pull requests on demand with the Runtime PR Watcher over the public GitHub REST API — rate-limit and network-failure tolerant, no `gh` dependency — classify which plausibly touch this app's GGUF, quantization, chat-template, API route, hardware backend, KV cache, or model registry surface, and keep a regenerable report with a suggested action per entry.
- Capture per-load and per-request traces in **Telemetry**: load timing, memory and VRAM headroom, offload placement, sampler stats actually used, and token counts and throughput. Export a redacted support bundle with a preview of exactly what is included; prompt and response text, keys and tokens, private keys, and home-directory usernames are stripped by default, and unreported fields are marked `unavailable`.
- Expose sampler, batch size, mixed-precision KV cache, and speculative-decoding draft-model controls for Ollama and `llama.cpp`, each gated on real support and enforced server-side at save and load time regardless of the UI. An unsupported control is disabled with a specific reason rather than silently ignored.

## Studio: image, video, and speech generation

- Switch the main view between **Chat** and **Studio** to run text-to-image, image-to-image, text-to-video, image-to-video, and speech — optionally in a voice cloned from a reference clip — from weights on your own machine, with no provider account or remote endpoint involved.
- Images and video run on an app-owned managed `sd-server` (stable-diffusion.cpp); speech runs on a separately pinned `llama-tts`. Both use the same rails as the managed `llama.cpp` chat runtime — pinned version, per-file SHA-256 against a manifest digest compiled into the app, atomic publish, and a per-runtime versioned directory and install lock — so installing or updating one cannot disturb another.
- Describe a model as the set of component files it is, because `sd-server` binds its whole model set at launch: typed slots (all-in-one checkpoint, diffusion model plus a mixture's high-noise stage, CLIP-L, CLIP-G, CLIP-vision, T5-XXL or an LLM text encoder, VAE, audio VAE, TAESD, mmproj, vocoder), per-model defaults, a RAM floor, and license terms. The add form prefills family and slot guesses from a weight file's name and lets you overwrite each one; a name that says nothing gets no guess. Adding a family is a registry entry, not new code, and switching models relaunches the engine.
- Download weights through the same Hugging Face downloader the model manager uses, so Studio and Runtime Hub share one progress stream and one cancellation path and an interrupted transfer leaves no partial file. Keep a LoRA stack and reusable component parts, choose sampler, scheduler, seed, steps, CFG, and a hires upscaler, browse and prune a gallery, cancel a run or a download, and unload the engine.
- Gate a license rather than mirror it: a model whose terms restrict territories shows those terms and requires acceptance before your own download begins, and such weights are never served from this project. Request validation snaps canvas edges onto the sampler's multiple-of-32 grid and clip length onto the backend's frame grid, so the duration the UI offers is the clip produced.

## Skills, plugins, MCP Apps, and workflows

- Install data-only `SKILL.md` skills globally or per workspace from a reviewed local folder or an immutable 40-character Git commit. Preview returns the exact SHA-256 approval digest; symlinks, special files, mutable Git refs, command collisions, oversized trees, and unmet OS, binary, or environment requirements fail closed.
- Invoke up to five installed skills at the start of a turn, for example `/review /testing check this patch`. The selected instructions, version, source, and digest are frozen into that turn and never expand tool permissions.
- Create a quarantined skill proposal with `/learn command | instructions`. It activates only after its risk flags are reviewed and its exact digest approved, and it can be rejected or rolled back.
- Learn a reusable skill from the agent's own verified work. After a run finishes, the backend classifies that run's durable events against fixed rules; only five things open a candidate: you explicitly asking for a procedure to be reusable, a correction of yours that then verified, a verification failure repaired inside the same run, a normalized failure that recurred and was finally resolved, or a multi-step procedure that changed files and ended with a passing verification. A turn without real execution evidence never opens one, whatever it says.
- A candidate is drafted by one bounded reflection pass, then built into an ordinary `SKILL.md` package by deterministic code under an app-owned staging directory, deduplicated against installed native, workspace, learned, and signed-package skills, and evaluated by really running it — and a baseline without it — in disposable copies of the workspace, with real tool execution and the workspace's own verification. It becomes a real versioned native skill only on installation, which is where rollback, disable, and uninstall come from. Learning Mode is **Suggest only** by default; even **Auto-promote safe improvements** stops for approval at a widened tool list, a new executable or environment requirement, global scope, or a possible duplicate, and refuses outright anything that would weaken permission policy. See [Learned skills](#learned-skills) for the full loop.
- Manage signed declarative packages in **Settings → Ecosystem** with install and update permission previews, pins, enable and disable, rollback, revocation state, uninstall, offline cache, and portable export and import. Local unsigned development packages stay data-only behind an explicit warning; unsigned Git packages and executable payloads are rejected.
- Start from a signed first-party catalog of six skills (review, testing, documentation, browser QA, release preparation, knowledge workflows) plus declarative GitHub, GitLab, WebDAV, and REST/webhook connector packages.
- Inspect plugin health and component setup, use package assistants, activate package workflow templates, and apply verified package rules to normal, Compare, and Crew turns with provenance.
- Configure remote MCP OAuth metadata and tokens, preserve structured MCP content, route relevant tools without bypassing allowlists, and host interactive MCP Apps in an opaque-origin window with a narrow declared bridge and a text fallback.
- Connect remote MCP servers over OAuth with no client credentials shipped in this binary: servers supporting dynamic client registration are one click, and the rest use an OAuth app you register yourself, stored in your keychain — see [BYO OAuth clients](byo-oauth-clients.md).
- Build typed workflow DAGs visually with model, agent and subagent, tool, MCP, browser, Git and PR, shell, verify, transform, condition, bounded-loop, human-approval, artifact, and output nodes. Validate before saving, run from the UI or CLI, inspect node history, cancel, replay from safe boundaries, and reconcile ambiguous external effects.
- Attach manual, in-app cron, persistent cron, filesystem, signed-webhook, and event-ingestion triggers. Persistent triggers are hosted by the explicitly installed daemon.

A minimal native skill:

```markdown
---
name: Project Review
description: Review this project for correctness and risk
command: project-review
version: 1.0.0
requires:
  bins: [git]
  env: []
---
Review the requested scope. Report evidence, severity, and a concrete fix.
```

## Learned skills

The loop, end to end:

```
agent run -> durable run events -> signal rules -> bounded reflection ->
staged SKILL.md package -> validate -> deduplicate -> evaluate ->
approval or policy gate -> versioned install -> use in later runs ->
effectiveness tracking -> update candidate or rollback
```

**What creates a candidate.** Only a run that reached a `Completed` terminal
event with at least one successful tool call, and then matches one of the five
source kinds: `explicit_user_instruction`, `user_correction`,
`verification_repair`, `repeated_failure_resolution`,
`successful_novel_procedure`. Classification reads the run's own events from
the durable ledger, plus your turn text (the one input the ledger does not
carry). A run only ever opens one candidate.

**What does not.** A conversational turn, a cancelled or failed run, a run
with no successful tool call, and any web page, MCP result, subprocess output,
or model claim that says it should be learned. Untrusted content can be
evidence; it can never authorize its own installation.

**Where they live.** Candidates, evaluations, provenance, effectiveness rows
and the learning mode are in a durable store under the active profile's data
directory (`skill-learning-v1/`), so they survive a restart and are shared
with the CLI. Staged packages are written under that store's own `staging/`
directory; resource paths are validated relative paths and nothing may be
written outside it. Once promoted, a skill is an ordinary native skill in the
global or workspace skill root.

**When approval is required.** Always, unless you selected **Auto-promote safe
improvements** *and* the candidate passed evaluation, adds no tool access,
declares no executables or environment variables, is workspace-scoped, and is
a new skill or an update to a learned one. Approval is required for a widened
allowed-tools list, removing an existing restriction, any new executable or
environment requirement, global scope, moving a workspace skill to global, and
a possible duplicate. Content that tries to talk a future turn out of its
permission gates, and a command that collides with a skill this loop did not
install, are refused outright rather than prompted for.

**Evaluation.** Each candidate gets a positive case reproducing the observed
task and a regression case an unrelated turn must not be hijacked by. Every arm
of every case runs in its own disposable copy of the workspace the candidate
was learned in, and all of those copies are made from the same starting state
*before* any arm runs — the baseline never hands its mutated files to the
candidate. Each arm is an ordinary agent run whose filesystem and shell calls
are pointed at that copy, so tool calls really execute, permission policy
really applies, and the workspace's own configured verification commands really
run against what the arm produced. Your live files are never touched.

Each copy is then rewound to the state your workspace was in *before* the run
the candidate was learned from, taken from that turn's own checkpoint. Learning
happens after a run succeeds, so the folder on disk already contains the
answer; without the rewind, "reproduce the observed task" would be asking both
arms to solve a solved problem. If the run changed files and its checkpoint is
gone — pruned, or never taken — the environment cannot be rebuilt and the
result is `unevaluated`.

The rewind covers every file this app's own write and edit tools changed. No
checkpoint captures what a shell command created, changed or deleted, so a run
that used the shell leaves a rewind that cannot be shown to be complete — its
copy could still hold part of the procedure's own result, and an arm could then
"reproduce" a step it never performed. That is refused up front: a real
isolated evaluation of such a run is `unevaluated`, not attempted.

The evaluation also checks its own starting state, for everything the rewind
was never going to see. Before any arm runs it verifies one untouched copy, and
if that copy *already* satisfies the workspace's verification, the case is a
solved problem and the result is `unevaluated`.

Verification uses the commands configured for the workspace the candidate was
learned in — not whichever folder happens to be open, and not anything read out
of the sandbox. An arm may write anywhere inside its own copy, so what a
sandbox *is*, and which workspace verifies it, come from a registry the app
keeps outside every sandbox. The file inside marks ownership and authorizes
nothing. Evaluating a candidate from a different workspace, or with none open,
therefore still runs the right checks, and a candidate that rewrites that file
does not change which commands judge it.

The acceptance conditions come from the evidence, not from the proposal. A
positive case requires every tool the working procedure actually succeeded
with, so a proposal that declares a narrower tool list fails its evaluation
rather than deleting the requirement it cannot meet. The candidate arm runs
under exactly the tool restriction the skill will carry once installed, so it
cannot pass using something it will not have afterwards. And when the observed
run ended on a passing verification, the arm has to verify too: a failed
verification fails the case, and a *missing* verification result leaves the
whole evaluation `unevaluated` — "we never checked" is not evidence.

Evaluations are recorded with the mode that produced them. A **preflight**
record only captured which tools a model asked for and executed none of them:
useful as a diagnostic, and never a pass — the backend downgrades even a clean
preflight to `unevaluated`. Only a **real isolated** evaluation, with actual
tool execution, can back an unattended promotion. If a reproducible environment
cannot be built (no workspace on record, a workspace too large to copy) or no
model target is reachable, the result is **unevaluated**, and an unevaluated
candidate can still be installed by you, but never unattended.

**Provenance.** Every promotion records origin, candidate id, source run ids,
parent hash, installed hash, evaluation ids, promotion policy, approval id and
timestamp, keyed by the installed content hash. Nothing later rewrites it: an
update writes a new record, and a rollback surfaces the restored version's own
provenance, so a historical run's evidence stays true.

**How approval works.** There is no "approved" flag a window can set. When you
install a candidate, the app raises its ordinary permission prompt describing
exactly what would be installed — the package digest, the tools it may use,
what it requires, why it needs approval, and which evaluation backs it — and on
an allow decision the backend receives a durable approval identity bound to a
digest computed from all of that. The digest is recomputed at install time: a
candidate edited or re-staged after you saw it no longer matches, and the
approval stops authorizing anything until you approve what is actually staged.
The approval id is stored in the skill's provenance. An explicit `--yes` on the
CLI is the same thing by another route, and it records its own auditable
decision.

**Which version a run used.** A run writes a durable `skill_invoked` event
naming the command, scope and exact content hash at the moment it freezes a
skill into its prompt. Effectiveness, correction attribution, regression
counting and provenance all key off that event, so a run that used a version
which has since been updated — or rolled back — still reports against the
version it actually ran. It is never inferred from whatever happens to be
installed when the question is asked, and never from parsing tool output.

**Effectiveness and regression.** Every run that invoked a learned skill is
recorded once it reaches a terminal state — completed, failed *or* cancelled.
The record carries the exact hash, the run id, the outcome, the run's final
verification result (`passed`, `failed`, or genuinely absent — never assumed),
the tool calls that failed, and whether you corrected it afterwards. A
cancellation is its own outcome and never counts as a failure of the skill; a
real execution or verification failure does.

Failures are counted by *comparable* failure: the failure text is normalized
into a stable signature (digits, paths, hex blobs and quoted values collapsed)
and counted per `hash + signature`. One failure changes nothing, and two
*unrelated* failures are two unrelated facts. Two comparable failures at the
same hash open an *update candidate* with that hash as its parent. So does a
correction — but only once the corrected procedure has itself run and verified:
telling the agent it was wrong, on its own, is recorded against the version it
was about and opens nothing. The correction is attributed to the previous
learned-skill use in the same session, read from the durable effectiveness
rows, so it survives a reload and a restart. The installed version is never
mutated in place; the previous version stays available through the ordinary
rollback path, and learning state reconciles a rollback at the next start —
the restored version becomes the active one again, with its own provenance.

**What the model may ask for.** The bounded `manage_skill_learning` tool can
propose a draft, inspect a candidate, request an evaluation, request a
promotion, and deprecate a learned skill. `request_evaluation` really does
reach the isolated executor — after the turn ends, and the verdict is still the
backend's; the model cannot report one. `request_promotion` parks the
candidate and nothing more. No model action can approve itself, forge an
approval id, write into a skills directory, change the learning mode, change
scope to global, or weaken permission policy.

**Enforcement, not persuasion.** A skill's `allowed-tools` list narrows what a
turn may call; it can never widen it. The effective capability of a run is its
own permissions ∩ the invoked skills' allowed tools ∩ the normal tool policy,
and it is enforced structurally: a tool that is not in that intersection is not
offered to the model *and* is refused if called anyway. The deny list on
proposed skill text is defense in depth against a skill that tries to talk a
future turn out of its gates — it is not the boundary.

**Settings.** Learning Mode and whether this loop may work in global scope at
all live in the backend store, so the UI and the CLI read the same values.
Turning global scope off confines every candidate to the workspace it was
observed in. Nothing is ever re-scoped on its own in either direction; moving a
workspace skill to global is a separate, explicitly approved change.

**Limits.** See [Limitations](limitations.md#scope-narrower-than-the-name).

## Agent workbenches

Each workbench is a real model-driven flow. Where its scope is narrower than its name, that boundary is in [Limitations](limitations.md).

- Turn a design source into a scaffolded local app (Design-to-App), score a spec for ambiguity and missing acceptance criteria, and compile an SOP into runnable steps.
- Draft a product plan from the composer with `/pm-plan <goal>`. Product Manager Copilot and Evidence Board claim extraction generate against the same model target the chat switcher sets, and each carries that picker in its own panel.
- Investigate a production issue from pasted logs, workspace files, terminal output, or browser evidence. The Production Debugging workbench prepares a fix in an owned worktree rather than editing the live workspace, and evidence is attached by the user — no log or APM provider is polled automatically.
- Run an incident timeline (Incident Commander), keep a claim and evidence board with explicit provenance, and search across repositories with Cross-Repo Intelligence.
- Plan and execute a migration in reviewable slices: the agent proposes, the user promotes.
- Generate an MCP server scaffold from a described API, diff two API contract versions, and build a connector from an OpenAPI document.
- Compare models side by side (Model Compare Lab), run a Golden Dataset Builder over real model calls with dedupe and privacy filtering, and hold a multi-model debate.
- Run a Red-Team Lab whose prompt-injection fixtures are scored by the real code path. One corpus (`src/lib/redTeamFixtures.json`) is read by both sides: the panel asks the Rust permission gate what it would decide through a read-only dry run over the same chain a live tool call takes, and `permissions.rs` compiles the same file in to walk every fixture across all six permission modes. The dry run treats a mode as an override, so asking what `acceptEdits` would do never changes the active mode or clears a grant. Separately, `redTeamLiveLoop.test.ts` drives the real turn loop with a scripted model and asserts the transcript carries the untrusted-content envelope. This corpus is what surfaced the risk-floor gap since closed: a floored path now prompts in every mode below `bypass`, and no remembered session grant can answer one.
- Score models, connectors, MCP servers, skills, workflows, and plugins in Trust Scorecards from live store state, weakest first. Every dimension cites the field it read; nothing is scored from measurements the app did not take.
- Build and run eval suites (Workflow and Agent Test Harness) with constraint, golden-answer, and model-judge scoring, failure clustering, and reproducible per-case fingerprints. A suite marked **release gate** blocks its target workflow from starting in **Settings → Ecosystem** until a complete passing run of the current suite revision exists; overriding takes a second explicit action.
- Explore a knowledge graph, run deep research, draft in Brief Studio and Work Canvas, work a data notebook and spreadsheet copilot, apply security autofixes, run synthetic monitoring, and use database admin guardrails.

## Runs, limits, and cost

- See the ten first-class tracked process kinds in one table. A chat turn, daemon job, `task`-tool subagent, Crew member, workflow run and each of its node instances, remote-queued work, background shell, side task, and browser session each create a record with a shared id scheme (`p-<kind>-<uuid>`), a parent id, one state machine (`admitted → running → suspended → exited`), the owning workspace and profile as queryable columns, a declared limit set, and a structured exit. Foreground shell calls, verify commands, and hooks are execution paths, not separate process rows. List records with `monkey processes` (`--kind`, `--all`, `--workspace`, `--parent`, `--json`) or `monkey processes show <id>` for a process and its descendants. The name is `processes` because `monkey ps` is the Ollama-compatible "list running models".
- Invalid transitions are refused rather than silently applied, and both that rule and "a row is `exited` if and only if it carries an exit status" are enforced in Rust and by SQL triggers, because companion stores reach the shared ledger connection directly.
- Stale records left by a killed app are reaped at startup, scoped to the kinds the app owns so live daemon work is never declared lost. Work with no fixed owner — any workflow run, since both the app and `monkey workflow run` host them — is reaped by whether its recorded host process still exists, so whichever host starts next cleans up after whichever died. A row that recorded no host is never reaped: pid reuse can only keep a stale row alive longer, never close one whose work is still running.
- End the spawned tree where the platform owner exists. Unix shell tools, verify commands, and sandboxed runs use process groups; Linux live shells cannot leave theirs, while a macOS child can change groups and outlive cleanup under the same Seatbelt policy. Windows live and disposable shells are assigned to kill-on-close jobs before their first instruction. Windows verify, hook, Chromium, and daemon owners do not yet have that job-backed tree guarantee.
- Hold tool children to the kernel bounds that are actually portable. Unix shell, verify, and sandbox children inherit the baseline rlimits installed between `fork` and `exec`; Windows live and disposable shells instead inherit fixed 512-process and 4 GiB job guardrails. These are not a universal per-class OS quota.
- Cap the shell output that reaches a model at 20,000 bytes per stream, keeping the end where a failing command puts its diagnostic and stating on the wire whether anything was dropped. Callers that parse output as a document rather than showing it to a model can request all of it.
- Record daemon and WebView budget kills as `limit_exceeded` rather than a plain cancel, naming which limit fired and what the measurement was. Workflow wall-clock kills also record `limit_exceeded` with a named cause, but not yet the configured threshold or elapsed measurement. Browser-session rows keep their named quota reason on the cancelled exit, so that owner also remains distinguishable from a generic failure without claiming a universal exit status.
- Finish teardown when a budget fires: a browser session that hits its action quota is stopped rather than left running unreachable, and a workflow run killed for exceeding its wall clock is recorded as stopped by that budget rather than left reading as running forever.
- Reclaim browser sessions nothing is driving. A sweep every 30 seconds retires sessions past their time limit or whose browser has died and records which bound fired. Unix records the Chromium process group for crash reclaim; Windows records no tree handle, so an app crash can still orphan renderer children.
- Give chat turns, subagents, Crew members, and side tasks a configurable wall-clock budget, enabled at six hours by default, enforced by the same sweep that delivers stop and pause and recorded as a limit rather than a cancel.
- Seed each kind's declared bounds from its class, then retain enforced daemon-job and browser-session overrides from their owners, so `monkey processes show` reports both defaults and owner policy. A caller value is now recorded only where that kind's owner reads the field; everything else is refused at admission and answers `unavailable` with the missing mechanism named, listed by `monkey processes limits`.
- Stop or suspend anything from anywhere, including a terminal. `monkey processes signal <id> stop|suspend|resume|kill` (with `monkey processes signals` for the support matrix) records durable intent on the process row rather than in a live handle, which is what lets it reach work this app is not running and survive a restart. A kind that cannot honour a signal refuses it with the reason instead of appearing to succeed. Delivery is per-owner: the daemon reads the latch once per tick, the desktop reads it through the `processes://changed` event plus a 2-second catch-up query, and each hands it to the primitive that kind already had.
- `stop` and `kill` are separate latches, not one bit with two names. A kill is a stop with a stronger delivery promise — immediate `SIGKILL` to the process group where the app owns one, against stop's TERM-grace-KILL wind-down — escalation is one-way, and a kill recorded without a stop is refused by a SQL trigger. The operator kill switch takes the immediate path; on Windows `taskkill /F` makes the two coincide, and the matrix says so.
- Pause and resume reach the loops that can honour them: chat turns, subagents, Crew members, workflow runs, background shells, daemon jobs, and side tasks each park at a safe point and only then report `suspended`. A workflow node refuses suspend with its reason — pausing operates at the owning run's level boundary, which the headless executor observes — and that same latch makes a daemon-hosted workflow run cancellable from anywhere that can write it. A paired controller gets pause and resume as its own scoped remote action (`monkey remote pause|resume`, `POST /v1/remote/runs/{id}/pause`), strictly weaker than cancel, so trust to suspend is not trust to destroy. Restart policy is declared per kind: exactly one kind, the daemon job, is restartable, because only it has both a supervisor outliving the process and a durable description of what to run.
- Attribute every recorded egress refusal. A blocked outbound request carries either the id of the run that caused it or one of five coded reasons why it has no run — user action, scheduled work, inbound request, startup, or shared transport — never a blank. Each site was scoped individually, because a `tokio::spawn` or `spawn_blocking` between the scope and the record voids the attribution. Allowed egress is recorded too, per destination in the run ledger, and `monkey security egress-evidence` reads the two halves together; it exits non-zero when a per-attribution destination cap means the list it printed is truncated rather than complete.
- Approve, inspect, and replay from one place: the Agent Inbox and Run Dashboard put approvals from desktop, daemon, and remote controller on one screen with a per-run event timeline.
- Export a Run Capsule — a redacted, replayable record of a run — and replay it by class.
- Track token usage and cost in **Settings → Usage**: per-request cost against rates you enter, daily and monthly budgets, and a `warn` or `pause` enforcement mode checked before every provider request. Rates are yours; the app never claims to read a provider invoice.
- Get a Daily Brief aggregating real run, task, and read-only MCP state, and search everything with Universal Search, whose workspace filter is validated against the roots actually attached to this instance.
- Keep separate identities on one machine in **Settings → Profiles**: each profile is its own data root — sessions, run history, artifacts, packages and keychain items — plus its own quota (max concurrent runs, memory, run time) and its own share of the machine, enforced by the daemon at admission. A profile cannot read another's runs, artifacts or credentials, because they are different files and different keychain services rather than differently-filtered rows. Switching restarts the app, since everything currently open belongs to the profile it started under; the default profile keeps the existing layout, so upgrading moves nothing. Local isolation only — no account service, no sign-in, nothing leaves the device. From a terminal: `monkey profiles list|create|switch|limits|delete`, or `--profile <id>` to run one command as another identity.

## Developer integrations

- Run `monkey acp` as an ACP v1 stdio server. Little Monkey remains the approval authority and carries streaming, tool status, cancellation, diagnostics, artifacts, checkpoints, and diffs through the durable run protocol.
- Use the VS Code extension in `extensions/little-monkey-vscode` for active-file, selection, and Problems context, native diff review, explicit selection edits, and optional local Ollama FIM completion. Completion is off by default, requires an explicitly allowed model whose live metadata advertises `insert`, cancels stale document versions, and never falls back to cloud.
- Use the JetBrains plugin in `extensions/little-monkey-jetbrains` for IntelliJ IDEA, Android Studio, and compatible IDEs. It captures exact editor context and diagnostics, opens read-only diff previews, and cannot silently approve or apply mutations.
- Start an owned disposable Chromium session from **Settings → Browser Verification**. Navigate, inspect DOM and accessibility state, click, type, scroll, capture screenshots, and retain console and network evidence as durable artifacts, under exact-origin grants, DNS rechecks, quotas, cancellation, and explicit loopback approval.
- Create and recover Little Monkey-owned Git worktrees, inspect HEAD, staged, and unstaged diffs, stage selected paths, commit, push only declared owned branches, and archive or clean owned worktrees.
- Read GitHub issues, PRs, unresolved review threads, and checks through existing `gh` authentication; create and update owned draft PRs, run a local Ollama PR reviewer, publish one deduplicated review report, and queue a selected review comment as an isolated daemon patch task. Merge, force-push, branch deletion, and automatic thread resolution are not exposed.

## Background agents and remote handoff

- Install a current-user `monkey daemon` service explicitly, with bounded concurrency, queue size, retention, notifications, and an optional loopback webhook listener.
- Queue immutable recipe and workflow runs with idempotency keys, budgets, approval waits, pause and resume, attach and detach, cancellation, retry, crash recovery, orphan detection, owned worktrees, and a durable global kill switch.
- Configure persistent cron, filesystem, signed webhook, and GitHub triggers with replay protection and deduplication.
- Pair a user-owned remote runner over direct, Tailscale, or SSH-forwarded HTTPS with pinned TLS, mutually scoped credentials, rotation and revocation, replay protection, and audit history. A controller may view events, inspect bounded artifacts, approve digest-bound requests, cancel runs, or engage the kill switch only when its invitation grants that exact action. Inference, tools, workspaces, and provider keys stay on the runner; Little Monkey operates no relay.
- Grant a paired controller a scoped Control Desktop action — real mouse and keyboard input on macOS, Windows, and Linux/X11, with Linux/Wayland failing closed. Every action is gated by local consent, per-action by default or batch only when the remote request and local operator agree. A cross-process session lock prevents the app and daemon from driving input at once, periodic screenshots are recorded to the run ledger, and revoking a device or engaging the kill switch force-stops a live session.

## Desktop companion and mobile

- Open a restricted always-on-top companion overlay on a configurable global shortcut. Context capture is explicit and visibly granted — pasted text, an approved file, or a selected screen area — and emergency stop revokes active capture and cancels owned media jobs.
- Transcribe audio files, push-to-talk clips, or meeting recordings through a configured local `whisper.cpp`-style worker or an explicit provider. Timed speaker segments are retained when the backend supplies diarization, and meeting text is prepared for user-reviewed notes, decisions, questions, and action items. Raw audio is retained only on request.
- Read text aloud with system TTS, cancellable through the same path.
- Configure user-owned ComfyUI or OpenAI-compatible image endpoints for remote generation and editing, retaining prompt, negative prompt, model, seed, dimensions, steps, CFG, source and output hashes, progress, cancellation, and metadata, with a gallery action that inserts an owned artifact into chat through the normal review path. This is the remote-endpoint path; Studio is the local-weights one.
- Pair the iOS and Android app ([little-monkey-mobile](https://github.com/AA-Box/little-monkey-mobile), React Native and Expo) to a desktop or homelab node with a versioned invitation. Requests are sequence-numbered and signed, and the client requires the invitation's pinned TLS fingerprint unless a trusted-LAN development override is visibly enabled.
- Browse runs, event timelines, pending approvals, and verified artifacts from the phone; approve an exact operation digest, cancel a run, or engage the kill switch, each only when the pairing grant contains that capability. Chat sessions, saved-workflow launch, capture upload, and device self-revocation run over the node's versioned `/v1/remote/mobile/*` extension, with chat turns executing through an operator-authored `mobile-chat` recipe so the node stays authoritative for models, prompts, and permission mode.
- Queue chat, workflow, text, image, file, and foreground voice captures while offline. File payloads are bounded, base64-encoded, and SHA-256 verified on both sides before storage.

## Desktop slash commands

The composer autocompletes built-ins, saved prompt and persona commands, native skills, and installed package skills.

| Command | Action |
| --- | --- |
| `/status` | Show the active runtime, workspace, and connections. |
| `/tools` | List tools available to the next model turn. |
| `/skills` | List enabled skills and invocation names. |
| `/plugins` | List installed declarative plugins and health. |
| `/model [provider:model-or-name]` | Show or switch the active model. |
| `/new` | Start a new chat without contacting a model. |
| `/compact` | Compact older completed turns. |
| `/stop` | Cancel the active turn. |
| `/usage` | Show reported token usage for the chat. |
| `/btw question` | Ask a side question that never joins the conversation. |
| `/pm-plan goal` | Draft a product plan from the composer. |
| `/learn command \| instructions` | Create a quarantined skill proposal for review. |
| `/<installed-skill> [request]` | Freeze and apply an installed skill to this turn; up to five may be stacked. |

Built-ins run locally and deterministically. Unknown leading `/text` stays ordinary input, so paths are not consumed as commands.

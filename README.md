# <img width="50" height="51" alt="LM-logo" src="https://github.com/user-attachments/assets/84651d01-f18b-4c49-b203-8d1b7e8f16b6" /> Little Monkey

Little Monkey is a local-first Tauri desktop workspace for agentic AI. It can run against managed `llama.cpp`, Ollama, MLX on supported Apple Silicon, or OpenAI-compatible providers that you configure. The React UI and Rust backend share workspace, permission, run, model, package, browser, Git, and background-service contracts instead of treating each surface as a separate product.

The current working tree includes the shipped foundations described below. Some release acceptance gates still require external hardware, credentials, services, signed publisher feeds, or cross-platform clean-machine testing; those are called out in [Current limitations](#current-limitations). Work that genuinely is not built yet — router policies, real benchmarking, prompt/workflow version control, mobile offline mode, a fine-tune lab, and multi-GPU — lives in [ROADMAP.md](ROADMAP.md) with its acceptance boundary. Each feature below is described with its real limits inline rather than as a checkmark.

## Features

### Chat, models, and collaboration

- Chat with managed `llama.cpp`, Ollama, MLX, or configured cloud/BYOK providers, with capability-aware routing, provider failover, context compaction, usage accounting, and rate-limit warnings.
- Compare one frozen prompt across two-to-four explicit local, Ollama, or provider targets with independent streaming, stop, retry, timing, usage, persistence, and response promotion. Compare runs default to no tools and keep their target snapshots even if global model settings change.
- Pick a 6th "Ultracode" stop on the per-turn reasoning-effort slider (Default/Light/Medium/High/Extra/Max/Ultracode). Ultracode is frontend-only state — it never touches the Rust-validated effort-level wire type — and auto-fans that one turn across up to four available models through the same Compare pipeline, then auto-fires a synthesis pass once the branches settle, landing in the normal Compare/Synthesis view.
- Run saved Crew chats with a coordinator and bounded parallel persona members. Member transcripts remain isolated, coordinator synthesis is explicit, actor usage is attributed, and cancel-all reaches outstanding members.
- Ask a one-off `/btw` side question against the current transcript without adding it to the conversation. The exchange renders as a distinct aside notice, records no session usage, and every wire builder (agent loop, Compare, Crew) strips it before building later turns, so neither the question nor its answer ever reaches a model again.
- Keep multiple sessions, forks, groups, and a two-pane split view with independent streams.
- Attach files, folders, and images; reference workspace paths with `@`; select personas and knowledge stacks; and invoke skills with `/`.
- Search active and archived chats, messages, tool output, artifacts, and durable runs with date, model, persona, and workspace filters.
- Export a session as Markdown, JSON, or Word (`.docx`), translate individual messages or a whole thread while retaining the original, and create versioned portable backups.
- Create encrypted local snapshots with retention, preflight imports before changing live state, and use encrypted WebDAV backup with conflict copies and launch-time catch-up. Reliable unattended backup moves through the installed daemon.

### Workspace: files, review, terminal, and browser

- Reopen the app into the folders you were working in: the attached set (primary plus any secondary folders) is snapshotted on every change and reattached at launch, so a session resumed after a restart can still read, edit, and review its files without re-picking the folder. Folders that were deleted or moved since the last run are dropped instead of blocking the restore, and permission grants are still session-scoped — restoring a workspace never restores its grants.
- Work across five right-sidebar tabs — workspace files, code review, terminal, in-app browser, and background tasks — opened as chips that all stay mounted, so switching tabs never loses state. A region-wide fullscreen toggle and one shared, drag-resizable, persisted width apply to the whole tab strip, and each tab has its own keyboard shortcut.
- Review changes in a git-backed panel (real `git` porcelain output) listing every changed file with a per-file diff view, PR-aware.
- Map that review's acceptance criteria onto the diff: paste the criteria for the change, and each one comes back as covered, partial, or not covered, with clickable citations into the exact hunks. The two halves stay separate on purpose. What Little Monkey computed from git — the changed files, the numbered citable hunks and their line ranges, the added/removed exported-or-`pub` declaration names, and a digest of all of it — is rendered apart from what a model claimed *about* those facts, and every claim is checked against them before it is shown: a claim citing a hunk this diff does not contain is discarded with the invented id displayed, and a claim of coverage carrying no valid citation is marked unsupported rather than counted. The headline counts (criteria not covered, and hunks no covered criterion accounts for) are computed by set arithmetic over surviving claims, so no number on screen is one a model chose. Real limits: criteria are pasted by hand — nothing in this app links a working-tree diff back to an issue, plan, or session, so there is no list to import automatically — declaration names are a text match on changed lines rather than a type-resolved reference graph (this repo is half Rust, and a TypeScript-only graph would be confidently wrong about the other half), binary and oversized files carry no content and so no citable hunks, hunk excerpts shown to the model are capped at 8 lines, and the report lives in memory for the session rather than being persisted or exported. A diff large enough to hit the review payload's 300-file cap or the pass's 200-hunk cap is reported as an incomplete view of itself — both the panel and the model are told the list may be tail-truncated, so a "not covered" verdict over one is shown as unproven rather than as a finding.
- Run a real terminal: keystrokes go straight to the PTY through an embedded xterm.js emulator, so the actual shell supplies its own prompt, colors, line editing, history, and completions instead of a simulated line-output view. A session auto-starts per workspace, and the panel supports dock-right, drag-to-resize, and fullscreen.
- Browse the web from an in-app tabbed browser pane — real child webviews with a tab strip, favicons/loading state, a smart address bar (URL/localhost/search), back/forward/reload, and `window.open` reopened as a new tab. Only `http:`/`https:`/`about:` load, and remote pages get no Tauri IPC surface. This is a general browsing pane for the user, separate from the disposable, artifact-recording session under **Settings → Browser Verification** used for agent-driven testing.
- Track live background work — side tasks and `task`-tool subagents — from a "N running tasks" pill above the composer that opens a Background Tasks panel: Running and collapsed "Finished N" sections, a per-card stop control, token/tool-use counts, and an inline transcript view. A live elapsed/tokens/thinking-or-tool status line follows the active turn, and two or more parallel subagent calls in one turn collapse into a single group card with per-agent status dots; each subagent run cancels independently without stopping the parent turn.

### Agent tools and safety

- Use workspace-scoped file read/list, glob, grep, edit/write, shell, memory, web fetch/search, knowledge search, MCP, subagent, plan, and verification tools.
- Choose `manual`, `plan`, `acceptEdits`, `smart`, `auto`, or `bypass` permission modes. Sensitive paths have a deterministic risk floor, shell execution is kept behind the stronger policy, and unattended recipes cannot use `bypass`.
- Checkpoint every mutating turn, then revert or re-apply file changes, rewind the conversation, or do both from the timeline.
- Preview a checkpoint's per-file diff, artifacts, screenshots, and verification state before restoring it, compare any two checkpoints read-only, and run a rollback simulation that shows exactly what will change — with file, artifact, conversation, and external (shell/network/MCP) state distinguished, and effects that can't be safely undone marked `needs_reconciliation` rather than silently skipped.
- Configure post-edit lint/build/test commands. Failures can return to the model for a bounded repair loop and are recorded as verification events.
- Treat retrieved pages, RAG chunks, MCP results, subprocess output, GitHub content, browser evidence, subagent reports, and other model output as untrusted data before it re-enters a model prompt.
- Inspect local posture in **Settings → Security Doctor**, or run `monkey security audit`. It checks app-data permissions, API/webhook listeners, remote TLS posture, MCP origins, installed skill integrity, and active browser/companion grants without contacting a model. `--fix` is limited to private app-owned modes and disabling clearly unsafe listeners; it does not delete user data or rotate credentials.

### Knowledge Stacks 2.0

- Ingest local files/folders, projects, URLs, sitemaps, selected chats, and manually configured WebDAV sources.
- Extract text and source locations from text/code, HTML, PDF, DOCX headings/tables, XLSX sheets/cell ranges, and PPTX slides/notes. Macros, formulas, embedded scripts, and automatic external-link execution are not enabled.
- Refresh incrementally with content hashes, connector cursors, deletion propagation, progress, cancellation, retry state, and optional daemon scheduling.
- Add optional local OCR through a verified or explicitly selected worker, with language, provenance, size, digest, license, progress, and cancellation controls.
- Fuse BM25/FTS-style lexical retrieval with vector similarity and optional reranking, while retaining the existing local vector path.
- Use the retrieval inspector to see the normalized query, filters, candidates, lexical/vector scores, fused rank, reranker score, exclusions, token budget, and final context; copy a reproducible diagnostic bundle or preview local PII/secret redaction.

### Runtime and API Hub

- Inspect CPU/memory and runtime inventory, estimate model fit, search configured catalogs, resume verified downloads, activate/roll back model versions, prune old versions, clean owned orphan data, and load/unload supported runtimes.
- Manage versioned runtime components — the `llama.cpp` server, MLX runtime, tokenizers, converters, projector runtimes, and Metal/CUDA/ROCm/Vulkan support packages the app itself depends on — on stable/beta/pinned channels, separate from installed models: digest-verified installs, update checks, activate-to-roll-back with bounded version retention, and per-version compatibility notes, backed by a local, operator-editable component registry.
- Show a Hardware Compatibility Matrix ("Driver Doctor") before any model download, model load, or runtime install: real detection of Metal, CUDA, ROCm, Vulkan, and (best-effort) DirectML, plus driver version, compute capability, Jetson, and hybrid/multi-GPU detection, with an honest `available`/`not_detected`/`driver_too_old`/`tool_missing`/`unsupported` status per backend that never fails just because a GPU tool or device is absent.
- Track each installed model's source registry, license, quantization, chat template, and multimodal projector in a content-addressed, digest-verified manifest; reuse an already-verified payload across asset variants/versions instead of re-downloading identical bytes, and never trust a corrupt local copy for reuse.
- Manage Ollama, `llama.cpp`, and MLX through one runtime contract with capability preflight, owned-process shutdown, logs, metrics, cancellation, and resource-aware scheduling.
- Modelfile Studio: author, live-parse, and dry-run validate a full Ollama Modelfile (`FROM`, `PARAMETER`, `TEMPLATE`, `SYSTEM`, `LICENSE`, `ADAPTER`, `MESSAGE`, `REQUIRES`) — including GGUF/safetensors header sanity checks and short-name hardening — before it is ever installed into the model library.
- Before a model loads, simulate a per-load offload plan from the live hardware snapshot: recommended context size, batch size, GPU layers offloaded vs. CPU spill, projector placement, and parallelism, each with a plain-language rationale and concrete suggestions for raising the budget.
- Manage multimodal projectors: the offload plan reserves a projector's own resident memory before sizing context/GPU layers, an installed version's projector can be digest-verified against a local candidate file, and a vision-capable model with a missing or unverified projector is flagged on the Models tab and near the runtime load flow instead of allowed to load silently. The OpenAI-compatible wire and the MLX driver's request composition can now carry an inline base64 image block end-to-end — a real gap the Chat Template Compatibility Lab's vision fixture identifies is closed for both — but the main chat UI does not yet route attached images through this path, so this is projector management and initial wire-transport plumbing, not full in-app vision chat.
- Serve the advertised OpenAI-compatible routes (`/v1/models`, `/v1/chat/completions` with SSE streaming, `/v1/responses`, `/v1/embeddings`, tool calls, and JSON-schema structured output), the Anthropic-compatible Messages subset, native-Ollama `GET /api/tags` and `POST /api/chat`, and separately scoped model discovery/download/load/unload/status/delete routes — all through the same LAN/loopback authentication, pairing, rate-limit, and CORS policy. `/v1/embeddings` genuinely produces vectors only when the resolved model's runtime driver reaches an embeddings-capable backend (Ollama's daemon today); otherwise it returns a clear unsupported error rather than a fabricated vector. Native Ollama `/api/generate`, `/api/pull`, and `/api/show` are not implemented yet, and `/api/chat` always returns the complete response (as one JSON object, or one NDJSON line when streaming was requested) rather than incremental per-token streaming — real per-token SSE streaming is only implemented for the OpenAI-compatible routes.
- A compatibility matrix — real HTTP-level regression tests (`src-tauri/tests/m3_compatibility_harness.rs`) that spin up the actual server and exercise every route above — backs a live per-route/per-backend/per-model status view in **Runtime Hub → Compatibility**, derived from the same runtime/model capability state that gates real requests.
- Inspect a runtime's context window and KV-cache state honestly: the context size this app has configured (or its runtime default) always shown, plus real live figures from a managed `llama.cpp` process's `/props`/`/slots` endpoints when reachable — anything a runtime doesn't actually report (Ollama's live cache occupancy, MLX's context window entirely) is labeled `unavailable`, never guessed. A safe effective-context-size preview tightens a requested size against the offload plan, model metadata, and runtime setting bounds without bypassing them, and long-context generation failures are classified as prompt-too-long, cache-exhausted/context-shift, memory-pressure, runtime-limitation, or model-metadata-limit with a plain-language explanation.
- Harden streamed tool-call and structured-output parsing against malformed, truncated, and adversarial model output: brace-in-string arguments, mid-UTF-8/mid-escape chunk splits, dropped connections, duplicate/out-of-order fragment indices, and tool calls naming a tool the request never offered all fail closed with a clear error instead of executing corrupted or unauthorized tool calls.
- Keep loopback as the default. Non-loopback serving requires an exact interface, TLS identity, authentication, pairing, rate limits, an exact CORS allowlist, explicit backends/scopes, and a policy that excludes file, shell, Git, MCP, and other agent-tool routes.
- Store private keys and provider credentials in the OS keychain; persisted configuration contains references rather than plaintext key material.
- Warn about model retirement and compatibility before a run starts, on both surfaces: a cloud provider model (Settings → AI Providers, or the chat model switcher) known-retired against a maintained, versioned local list gets a reason and a concrete replacement suggested from that provider's own live model list; an installed local Runtime Hub model with a different catalog revision available and no refresh in a long time gets flagged the same way before its "Load model" step runs. Neither check is a live-verified upstream source — both are honest, updatable local signals, not a guarantee.
- Generate ready-to-use local configuration for external agent tools/editors (Continue.dev, aider, or a generic OpenAI-SDK-compatible `.env`) from **Settings → Runtime Hub → Agents**, pointed at the app's real endpoint, a currently installed model, and a real paired token; check a previously generated or hand-edited config for a stale model, a moved endpoint, a missing auth header, an oversized context length, or a telemetry default worth revisiting.
- Quantize an installed model or an arbitrary GGUF/safetensors path from the Quantization workbench: real GGUF/safetensors header sniffing, a heuristic license risk check, a static per-quant-level size/quality tradeoff reference, and a real `llama-quantize` backend when it's found on the machine (an honest copy-only passthrough otherwise) — every run produces a reproducible report with source/output digests and a real GGUF-parses eval check.
- Run a Chat Template and Renderer Compatibility Lab that exercises real tool-call, system-prompt, stop-token, and structured-output fixtures against Little Monkey's own OpenAI-compatible request/response renderer and the MLX driver's message flattening, grouped by a coarse chat-template family (ChatML, Llama 3, Mistral, Gemma, or generic). A model's chat/tool/vision capability badges are only shown once the matching fixture(s) actually pass for its declared template — image blocks and reasoning ("thinking") content are not yet representable in the renderer, so vision is never advertised as ready and thinking-mode fixtures are informational only.
- Runtime PR Watcher: on demand, scan `ollama/ollama`'s closed pull requests over the public GitHub REST API (rate-limit- and network-failure-tolerant, no `gh` dependency), classify which ones plausibly touch this app's own GGUF/quantization, chat template/tool-calling, API route, hardware/GPU backend, KV cache/context, or model manifest/registry surface with a keyword heuristic, and keep a persisted, regenerable report of newly relevant upstream changes with a suggested Little Monkey action for each.
- Capture per-load and per-request runtime traces in a **Telemetry** tab — load timing, memory/VRAM headroom and offload placement reused from the offload planner, sampler stats actually used, and token counts/throughput — and export a redacted support bundle (recent traces, a bounded runtime log tail, hardware/compatibility context) with a preview of exactly what is included/excluded before it is written to disk. Prompt/response text, API keys/tokens, private keys, and home-directory usernames are stripped by default; fields a runtime genuinely does not report are marked `unavailable` rather than fabricated.
- Expose sampler (temperature, top-p, top-k, repeat penalty, min-p), batch size, mixed-precision KV cache, and speculative-decoding draft-model controls for Ollama and `llama.cpp`, each gated on real support: flash attention and mixed precision require a GPU backend confirmed by the Hardware Compatibility report, and speculative decoding requires a smaller, same-family draft model already installed — an unsupported control is always disabled with a specific reason, never a silent no-op, and the same gates are enforced server-side at save/load time regardless of what the UI shows.

### Skills, plugins, MCP Apps, and workflows

- Install data-only `SKILL.md` skills globally or per workspace from a reviewed local folder or an immutable 40-character Git commit. Preview returns the exact SHA-256 approval digest; symlinks, special files, mutable Git refs, command collisions, oversized trees, and unmet OS/binary/environment requirements fail closed.
- Invoke up to five installed skills at the beginning of a chat turn, for example `/review /testing check this patch`. The selected instructions, version, source, and digest are frozen into that turn and never expand tool permissions.
- Use `/learn command | instructions` to create a quarantined local skill proposal. It becomes active only after reviewing risk flags and approving the exact digest, and it can be rejected or rolled back.
- Manage signed declarative packages in **Settings → Ecosystem** with install/update permission previews, pins, enable/disable, rollback, revocation state, uninstall, offline cache, and portable export/import. Local unsigned development packages remain data-only and require an explicit warning/approval; unsigned Git packages and executable payloads are rejected.
- Seed a signed first-party catalog containing six skills (review, testing, documentation, browser QA, release preparation, and knowledge workflows) plus declarative GitHub, GitLab, WebDAV, and REST/webhook connector packages.
- Inspect plugin health and component setup, use explicit package assistants, activate package workflow templates, and apply verified package rules to normal, Compare, and Crew turns with provenance.
- Configure remote MCP OAuth metadata/tokens, preserve structured MCP content, route relevant tools without bypassing allowlists, and host interactive MCP Apps in an opaque-origin window with a narrow declared bridge and text fallback.
- Connect remote MCP servers over OAuth without any client credentials shipped in this binary: servers that support dynamic client registration are one click, and the rest (Google, Slack) use an OAuth app you register yourself, stored in your keychain — see [docs/byo-oauth-clients.md](docs/byo-oauth-clients.md).
- Build typed workflow DAGs visually with model, agent/subagent, tool, MCP, browser, Git/PR, shell, verify, transform, condition, bounded-loop, human-approval, artifact, and output nodes. Validate before saving, run from UI or CLI, inspect node history, cancel, replay from safe boundaries, and reconcile ambiguous external effects.
- Attach manual, in-app cron, persistent cron, filesystem, signed-webhook, and event-ingestion triggers. Persistent triggers are hosted by the explicitly installed daemon.

A minimal native skill looks like this:

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

### Agent workbenches

Each of these is a real, model-driven flow rather than a mockup, and each has
a scope boundary stated where it matters.

- Turn a design source into a scaffolded local app (Design-to-App), score a
  spec for ambiguity and missing acceptance criteria, and compile an SOP into
  runnable steps.
- Investigate a production issue from pasted logs, workspace files, terminal
  output, or browser evidence; the Production Debugging workbench prepares a
  fix in an owned worktree rather than editing the live workspace. Evidence is
  attached by the user — no log or APM provider is polled automatically.
- Run an incident timeline (Incident Commander), keep a claim/evidence board
  with explicit provenance, and search across repositories (Cross-Repo
  Intelligence uses MVP text search, not a semantic index).
- Plan and execute a migration in reviewable slices. Execution is
  slice-at-a-time by design: the agent proposes, the user promotes.
- Generate an MCP server scaffold from a described API, diff two API contract
  versions, and build a connector from an OpenAPI document.
- Compare models side by side (Model Compare Lab), run a Golden Dataset
  Builder over real model calls with dedupe and privacy filtering, and hold a
  multi-model debate.
- Run a Red-Team Lab whose prompt-injection fixtures are scored by the real
  code path rather than a copy of it. One corpus
  (`src/lib/redTeamFixtures.json`) is read by both sides: the panel asks the
  Rust permission gate what it would decide via a read-only dry run
  (`resolve_path_and_root` → `path_risk_floor` → `compute_risk` →
  `evaluate_gate`, the same chain a live tool call runs, remembered
  session/run grants included), and `permissions.rs` compiles the same file in
  to walk every fixture through that chain across all six permission modes.
  The dry run evaluates a mode as an override, so asking "what would happen in
  acceptEdits?" never changes the mode you are actually in or clears a grant.
  Separately, `redTeamLiveLoop.test.ts` drives the real `runAgentTurn` with a
  scripted model and asserts the transcript the loop produced actually carries
  the untrusted-content envelope — it fails if the wrapping call is removed,
  which the previous fixture-only suite did not. The corpus is what surfaced
  the floor gap this closed: `acceptEdits`/`auto` used to approve edits without
  consulting risk at all, so a write to `.github/workflows/`, a package
  manifest or `.zshenv` was promptless in those modes. A floored path now
  prompts in every mode below `bypass`, and no remembered "allow for session"
  grant can answer one. One honest limit remains: the panel's containment
  column exercises the real boundary functions but cannot prove from a panel
  that the loop *invokes* them — that claim is the CI test's.
- Score models, connectors, MCP servers, skills, workflows, and plugins in
  Trust Scorecards from live store state, with weaker profiles sorted first.
  Every dimension cites the exact field it read; nothing is scored from
  measurements the app did not take.
- Build and run eval suites (Workflow/Agent Test Harness) with constraint,
  golden-answer, and model-judge scoring, failure clustering, and reproducible
  per-case fingerprints. A suite marked **release gate** blocks its target
  workflow from being started in **Settings → Ecosystem** until a complete
  passing run of the current suite revision exists; overriding requires a
  second, explicit click. Suite state is desktop-local, so CLI and API-server
  workflow starts are not gated.
- Explore a knowledge graph, run deep research, draft in Brief Studio and Work
  Canvas, work a data notebook and spreadsheet copilot, apply security
  autofixes, run synthetic monitoring, and use database admin guardrails.
  Each of these supports a single primary format or flow rather than the full
  surface its name might suggest; external integrations are manual.

### Runs, review, and cost

- See everything the app is executing in one table. A chat turn, a daemon job,
  a `task`-tool subagent, a Crew member, a workflow run, each of that run's
  node instances, remote-queued work, a background shell, and a side task all
  create a record in one process table with a shared id scheme
  (`p-<kind>-<uuid>`), a parent id, one state machine
  (`admitted → running → suspended → exited`), the owning workspace and profile
  as queryable columns, a declared limit set, and a structured exit
  (status/code/signal/reason). List it from the CLI with `monkey processes`
  (`--kind`, `--all`, `--workspace`, `--parent`, `--json`), or `monkey
  processes show <id>` for a process and its descendants. Named `processes`
  because `monkey ps` is the Ollama-compatible "list running models".
  Transitions are refused rather than silently applied, and both that rule and
  "a row is `exited` if and only if it carries an exit status" are enforced in
  Rust *and* by SQL triggers, because companion stores reach the shared ledger
  connection directly. Stale records left by a killed app are reaped at
  startup, scoped to the kinds the app owns so live daemon work is never
  declared lost — and work with no fixed owner, which is any workflow run since
  the app and `monkey workflow run` both host them, is reaped by whether the
  process that recorded itself as its host still exists. Whichever host starts
  next cleans up after whichever died, so a daemon that crashes and never
  restarts no longer leaves rows only it could have closed. A row that recorded
  no host is never reaped: pid reuse can only leave a stale row alive longer,
  never close one whose work is still running. Real limits: the table records
  what each kind reports, so a
  declared memory or wall-clock limit is *not* enforced by any OS mechanism
  yet — there is no cgroup and no Windows job object, and `setrlimit` reaches only
  tool children (see below), never memory, which the relevant limit cannot bound on
  macOS at all; what enforcement
  exists is a userspace watchdog over daemon jobs plus per-tool timeouts. That
  watchdog measures a job's memory across its whole process group, so work moved
  into a grandchild is still counted, but the memory budget is opt-in
  (`--max-memory-mb`) and the wall-clock default is seven days, so both are off in
  practice unless a job asks for them;
  a Crew member carries no edge to its coordinator (actors initialize
  concurrently, coordinator last); and a retried daemon job becomes a new
  process rather than inheriting the original's parent.
- A job killed for exceeding a budget is recorded as `limit_exceeded`, not as a
  plain cancel. All three daemon budgets — wall clock, memory, log size — tear the
  child down by cancelling its run, so until now a job killed for holding 700 MiB
  and a job someone pressed Stop on left the same row: "the system worked" and
  "someone changed their mind" were indistinguishable after the fact. The exit now
  names which limit fired and what the measurement was ("held 8192 bytes against a
  4096 byte budget"), so a reader can tell whether the budget was wrong or the job
  was. Real limit: the run ledger itself still shows the run as cancelled, because
  `RunStatus` has no limit status and adding a terminal status to the event
  protocol is a compatibility change — the distinguishable exit is on the process
  record. The marker that carries the fact through the daemon's own database
  currently rides in that row's error text rather than a typed column, because the
  daemon store has no migration framework to add one; it is confined to two
  functions so that stays a contained shortcut rather than a spreading convention.
- A timeout ends the whole process tree, not just the command that was spawned.
  Shell tools, verify commands and sandboxed runs each get their own process
  group, and a timeout or Stop terminates that group — TERM first, so a build can
  flush its output and clean up its temp files, then KILL for anything that
  ignored it. Before this, a 120-second shell timeout reaped the shell and left
  the compiler it started running, still consuming the machine after the tool
  reported that it had timed out.
- Tool children carry a bound the kernel holds them to, not only one this app
  supervises — three of the app's four spawn sites, the exception being a
  — all four places this app starts a process — get their resource
  backgrounded shell, which is still unwired. Shell tools, verify commands and
  sandboxed runs get their resource
  limits installed between `fork` and `exec`, so the program never runs unbounded
  and everything it spawns inherits them — which reaches the grandchildren a
  watchdog cannot see. Real limits, and they are the honest majority of this: what
  is enabled today is refusing core dumps, because that is the only ceiling with a
  real hazard behind it (a crashing build dropping gigabytes into your workspace)
  and no value that breaks working code. File-size and descriptor ceilings are
  implemented and tested but left unset, since picking a number is a judgement
  about what the child is for — the agent shell is the same site that legitimately
  downloads a 40 GB model. CPU time is deliberately not capped: it accumulates per
  core, so a cap matching the 120-second wall timeout would kill `cargo build -j8`
  after about 15 seconds. Memory is not capped here either — the relevant limit is
  a no-op on macOS — so resident memory stays with the daemon's watchdog. On
  Windows this does nothing at all; the equivalent is a job object and it is not
  built yet.
- Browser sessions nothing is driving are reclaimed. A session used to be checked
  only while an agent was actively driving the page, so an abandoned Chromium was
  never looked at again — its time limit could not fire, and one that had crashed on
  its own still reported itself as running. A sweep every 30 seconds now retires
  sessions past their time limit or whose browser has died, and records which bound
  fired rather than logging them all as "stopped". Real limits: nothing can tell a
  browser tab you are still reading from one an agent abandoned, so the time limit
  applies to both — an idle tab past its 10-minute default loses its session. The
  disk limit is deliberately left out of the sweep, since measuring a profile
  directory on a timer is expensive and an idle session's profile does not grow. And
  the browser's process id is still not recorded anywhere, so a crash of this app can
  still orphan a Chromium that nothing is able to kill.
- Chat turns, subagents, crew members, and side tasks can carry a wall-clock time
  budget, enforced by the same 2-second sweep that already delivers stop and pause,
  and recorded as a limit being exceeded rather than as someone pressing Stop. Real
  limit, and it is the main one: **no default budget is set, so today this fires for
  nobody.** That is deliberate — a turn waiting on an unanswered permission prompt
  looks exactly like a turn that is working, so any default would kill turns for
  being slow to answer. Two further honest bounds: a budget is a floor rather than a
  ceiling, because a turn inside a 120-second shell command cannot notice it until
  that command returns; and time spent paused still counts, so a long-paused turn
  trips its budget as soon as it resumes.
- Shell output that reaches a model is bounded. Each of stdout and stderr from the
  shell tool is capped at 20,000 bytes, keeping the end — where a failing command
  puts its diagnostic — and saying on the wire whether anything was dropped. Of the
  app's four command-running paths this was the only uncapped one and the only one
  a model reads directly, so a single chatty command could fill most of a local
  model's context window. The number is the one verification commands already used,
  not the far larger one the terminal panel uses for human scrollback, because a
  model's context is the tighter constraint. Callers that parse output as a
  document rather than showing it to a model can ask for all of it — the dependency
  audit does, since a truncated JSON report would not be a shorter answer but an
  unreadable one, and would have quietly reported no vulnerabilities. Real limit:
  the cap applies to what is returned, not to how much is read, so a command that
  writes gigabytes still buffers them in memory for up to its 120-second timeout.
- A budget that fires finishes its own cleanup. Two places where it did not, both
  found by auditing the enforcement already in place rather than adding more. A
  browser session that hit its action quota was marked cancelled but its Chromium
  was never killed — and because a cancelled session refuses every later call,
  nothing could reach the shutdown path again, so the browser was left running,
  idle, and reachable only by an explicit stop. Separately, a workflow run killed
  for exceeding its wall clock stayed recorded as *running* forever: the run's
  outcome was written by the same step that saves its history, and an aborted run
  never reached that step. So the one kind of work with a real enforced time budget
  leaked a live record every single time the budget worked. That record now also
  says the budget was what stopped it, rather than reading as though the work
  itself broke.
- Every process kind declares the bounds it is actually subject to, derived from
  its kind rather than restated by each subsystem. This fixed a record that was
  wrong rather than merely quiet: a backgrounded shell's output has always been
  truncated at 256 KiB, but its row declared no output ceiling, so `monkey
  processes show` printed `limits none declared` for a process that had one. Real
  limit, and it is most of the table: exactly one kind carries a bound at this
  level. A chat turn, subagent or crew member runs an unbounded number of
  individually-bounded tool calls, so nothing caps the turn itself — and no
  wall-clock or memory number was invented to fill the gap, because that would be
  a guess presented as policy. Daemon jobs stay bounded by their own recipe rather
  than by their kind, which is a truer number than any class default.
- Stop or suspend anything from anywhere, including a terminal. `monkey
  processes signal <id> stop|suspend|resume|kill` (and `monkey processes
  signals` for the support matrix) records the request as durable intent on the
  process row rather than in a live handle, which is what lets it reach work
  this app is not running and survive a restart. A kind that cannot honour a
  signal **refuses it with the reason** instead of appearing to succeed — `kill`
  where the app owns no OS process, `suspend` where a loop has no pause point.
  Delivery is per-owner: the daemon reads the latch once per tick and maps it
  onto its own cancel/pause, and the desktop reads it through the
  `processes://changed` event plus a 2s catch-up query (the CLI writes from a
  different OS process and cannot emit an event), then hands it to the primitive
  that kind already had — a chat turn's and Crew member's registered
  `AbortController`, a subagent's or side task's own cancel, the commands
  `background_shell_kill` and `m4_workflows_cancel`.
  Real limits: `stop` and `kill` share one latch
  column, so a reader cannot tell them apart (honest only while every kind that
  honours `kill` delivers both identically); a workflow run started elsewhere
  still cannot be cancelled, because `WorkflowService` resolves runs from an
  in-memory registry; a workflow *node* has no cancellation of its own; pause
  exists for side tasks only, and no paused work survives a restart — durable
  intent does, durable execution does not, since these loops live in the WebView.
- Approve, inspect, and replay from one place: the Agent Inbox and Run
  Dashboard put approvals from every source (desktop, daemon, remote
  controller) on a single screen with a per-run event timeline.
- Export a Run Capsule — a redacted, replayable record of a run — and replay
  it by class.
- Preview a checkpoint before restoring it, compare any two read-only, and see
  effects that cannot be safely undone marked `needs_reconciliation` rather
  than silently skipped.
- Track token usage and cost in **Settings → Usage**: per-request cost against
  rates you enter yourself, daily/monthly budgets, and a `warn` or `pause`
  enforcement mode checked before every provider request. Rates are yours, not
  a billing feed — the app never claims to see a provider invoice.
- Get a Daily Brief aggregating real run, task, and read-only MCP state.
- Search everything with Universal Search; an explicit workspace filter is
  validated against the roots actually attached to this app instance.

### Developer integrations

- Run `monkey acp` as an ACP v1 stdio server. Little Monkey remains the approval authority and carries streaming, tool status, cancellation, diagnostics, artifacts, checkpoints, and diffs through the durable run protocol.
- Use the thin VS Code extension in `extensions/little-monkey-vscode` for active-file/selection/Problems context, native diff review, explicit selection edits, and optional local Ollama FIM completion. Completion is off by default, requires an explicitly allowed model whose live metadata advertises `insert`, cancels stale document versions, and never falls back to cloud.
- Use the JetBrains plugin in `extensions/little-monkey-jetbrains` for IntelliJ IDEA, Android Studio, and compatible IDEs. It captures exact editor context and diagnostics, opens read-only diff previews, and cannot silently approve or apply mutations.
- Start an owned disposable Chromium session from **Settings → Browser Verification**. Navigate, inspect DOM/accessibility state, click, type, scroll, capture screenshots, and retain console/network evidence as durable artifacts. Exact-origin grants, DNS rechecks, quotas, cancellation, and explicit loopback approval are enforced; file URLs, uploads/downloads, clipboard, extensions, persistent profiles, and general desktop control are unavailable.
- Create and recover Little Monkey-owned Git worktrees, inspect HEAD/staged/unstaged diffs, stage selected paths, commit, push only declared owned branches, and safely archive/clean owned worktrees.
- Read GitHub issues, PRs, unresolved review threads, and checks through existing `gh` authentication; create/update owned draft PRs, run a local Ollama PR reviewer, publish one deduplicated review report, and queue an explicitly selected review comment as an isolated daemon patch task. Merge, force-push, branch deletion, and automatic thread resolution are not exposed.

### Background agents and user-owned handoff

- Explicitly install a current-user `monkey daemon` service with bounded concurrency, queue size, retention, notifications, and an optional loopback webhook listener.
- Queue immutable recipe/workflow runs with idempotency keys, budgets, approval waits, pause/resume, attach/detach, cancellation, retry, crash recovery, orphan detection, owned worktrees, and a durable global kill switch.
- Configure persistent cron, filesystem, signed webhook, and GitHub triggers with replay protection and deduplication.
- Pair a user-owned remote runner over direct/Tailscale/SSH-forwarded HTTPS with pinned TLS, mutually scoped credentials, rotation/revocation, replay protection, and audit history. A responsive controller can view events, inspect bounded artifacts, approve digest-bound requests, cancel runs, or engage the kill switch only when its invitation grants that exact action. Inference, tools, workspaces, and provider keys remain on the runner; Little Monkey operates no relay.
- Grant a paired controller a scoped Control Desktop action — real mouse/keyboard input on macOS, Windows, and Linux/X11 (Linux/Wayland fails closed with an explicit unsupported message). Every action is gated by local consent: per-action approval by default, or batch mode only when both the remote request and the local operator agree. A cross-process session lock stops the local app and the daemon from ever driving input at once, periodic screenshots are recorded to the run ledger, and revoking a device or engaging the kill switch force-stops its live session immediately.

### Multimodal desktop companion

- Open a restricted always-on-top companion overlay with a configurable global shortcut. Context capture is explicit and visibly granted; supported inputs include pasted text, an approved file, and a selected screen area. Emergency stop revokes active capture and cancels owned media jobs.
- Transcribe audio files, push-to-talk clips, or meeting recordings through a configured local `whisper.cpp`-style worker or an explicit provider. Timed speaker segments are retained when the backend supplies diarization, and meeting text is prepared for user-reviewed notes, decisions, questions, and action items. Raw audio is retained only when explicitly requested.
- Read text aloud with system TTS and stop playback through the same cancellation path.
- Configure user-owned ComfyUI or OpenAI-compatible image endpoints, then generate or edit when the endpoint advertises editing. Jobs retain prompt, negative prompt, model, seed, dimensions, steps, CFG, source/output hashes, progress, cancellation, metadata, and a gallery action that inserts an owned artifact into chat through the normal review path.

### Mobile companion

- Pair a real iOS/Android app ([little-monkey-mobile](https://github.com/AA-Box/little-monkey-mobile), React Native/Expo) to a desktop or homelab node by scanning or pasting a versioned invitation. Requests are sequence-numbered and signed, and the client requires the invitation's pinned TLS certificate fingerprint unless the trusted-LAN development override is visibly enabled.
- Browse runs, event timelines, pending approvals, and verified artifacts; approve the exact operation digest, cancel a run, or engage the kill switch — each only when the pairing grant contains that capability.
- Use the node's versioned `/v1/remote/mobile/*` extension for chat sessions and messages, saved-workflow launch, capture upload, and device self-revocation. Chat turns execute through an operator-authored `mobile-chat` recipe, so the node stays authoritative for models, prompts, and permission mode.
- Grant these surfaces explicitly: mobile capabilities are separate from runner actions, and a pairing created without them (including any pairing made before they existed) cannot reach chat, workflow launch, or capture regardless of the phone's app version. Chat additionally requires session viewing, and workflow launch requires task viewing.
- Queue chat, workflow, text, image, file, and foreground voice captures while offline. File payloads are bounded, base64-encoded, and SHA-256 verified on both sides before anything is stored.

Offline *browsing*, push delivery, a QR-sized invitation payload, and app-store release remain roadmap items — see [ROADMAP.md](ROADMAP.md).

## Desktop slash commands

The chat composer autocompletes built-ins, saved prompt/persona commands, native skills, and installed package skills.

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
| `/btw question` | Ask a quick side question that never joins the conversation. |
| `/learn command \| instructions` | Create a quarantined skill proposal for review. |
| `/<installed-skill> [request]` | Freeze and apply an installed skill to this turn. Up to five may be stacked. |

Built-ins run locally and deterministically. Unknown leading `/text` remains ordinary input, so paths are not silently consumed as commands.

## Prerequisites

- Node.js, `pnpm`, Rust, Cargo, and the Tauri 2 prerequisites for your operating system.
- Desktop releases include a pinned, checksum-verified `llama.cpp` runtime. Source builds stage the same official runtime automatically before `tauri dev`/`tauri build`; a system `llama-server` remains a development fallback only.
- Optional Ollama runtime: Ollama reachable at `http://127.0.0.1:11434` when using the explicit Ollama provider or daemon-management commands.
- Optional MLX runtime: supported Apple Silicon plus the configured MLX Python environment.
- Optional browser verification: a supported Chromium/Chrome binary.
- Optional GitHub delivery: Git and an authenticated GitHub CLI (`gh`).
- Optional local OCR, transcription, image generation, IDE extensions, and remote handoff: their explicitly configured worker/model, endpoint, SDK, or TLS identity.

On macOS, developers who intentionally want the unmanaged fallback can use:

```sh
brew install llama.cpp
```

The Runtime Hub can also install checksum-pinned artifacts from a configured catalog. This repository does not claim a complete publisher-operated artifact feed for every platform/runtime.

## Development

```sh
pnpm install
pnpm tauri dev       # verify/stage llama.cpp + the CLI sidecar, then run the app
pnpm dev             # Vite frontend only
pnpm build           # TypeScript check and frontend production build
pnpm tauri build     # build a desktop bundle containing the managed runtime
```

## Testing

```sh
pnpm test
pnpm i18n:lint
pnpm test:rust
pnpm test:git-delivery-action
```

Extension checks:

```sh
cd extensions/little-monkey-vscode && npm test
cd ../little-monkey-jetbrains && gradle test --no-daemon
```

The live Compare smoke is opt-in because it uses installed Ollama models:

```sh
pnpm test:compare:live
```

The VS Code completion hardware gate is also opt-in and requires an explicitly selected local FIM model:

```sh
cd extensions/little-monkey-vscode
LITTLE_MONKEY_COMPLETION_MODEL='your-exact-fim-tag' npm run benchmark:completions
```

## CLI

The installed command is `monkey`. The preferred chat form is model first:

```sh
# Existing target/provider auto-resolution.
monkey llama3.2 "Summarize this project"

# Omit the prompt for the interactive REPL.
monkey llama3.2

# Select a provider only when you want to override/disambiguate resolution.
monkey --provider openai gpt-4.1-mini "Review this codebase"
monkey --provider ollama llama3.2 "Explain the failing test"

# Explicit OpenAI-compatible local endpoint.
monkey --local-url http://127.0.0.1:8090 local-model "Inspect the workspace"
```

For the app-owned path that does not require Ollama or a separate
`llama.cpp` installation, pull or run a public Ollama Registry tag or a
public Hugging Face single-file GGUF reference:

```sh
monkey pull llama3.2:3b
monkey run llama3.2:3b "Summarize this project"
monkey run hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M
```

`monkey run` resolves immutable metadata, verifies the expected model SHA-256,
inspects the checksum-bound GGUF's embedded llama.cpp/Jinja chat template
before advertising tool support, resumes interrupted downloads, reuses
verified installs offline, and starts the bundled loopback-only runtime for
that session. Ollama's separate Go-template layer is never passed to
llama.cpp. The runtime's
per-file manifest is itself authenticated by a digest embedded in the compiled
app. Private/gated Hugging Face repositories, non-GGUF or sharded artifacts,
and Ollama models that require separate adapters or projectors are rejected
with a clear error.

If a non-local model is exposed by more than one configured provider,
`monkey` asks for `--provider <id>` instead of guessing. The legacy
`--ollama` and `--model` forms remain compatibility aliases.

Useful chat flags:

- `--workspace <path>` — sandbox tool access to a workspace; defaults to the current directory.
- `--permission-mode manual|acceptEdits|smart|plan|auto|bypass` — terminal permission policy.
- `--provider <id>` — override or disambiguate `ollama`, managed `llama.cpp`, OpenAI, Anthropic, Gemini, OpenRouter, or a custom provider.
- `--local-url <url>` — explicit local OpenAI-compatible endpoint.
- `--persona <slash-command>` and repeatable `--stack <name>` — attach saved context.
- `--verify` / `--no-verify`, `--subagents`, `--no-rules`, and `--no-mcp` — opt into verification/subagents or suppress configured context.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose`, and `--attach-images` — generation controls.
- `--num-ctx` — managed-runtime/Ollama context size; `--keepalive`, `--think`, and `--hidethinking` remain Ollama-native controls.

App-owned install and run:

```sh
monkey pull <model>
monkey run <model> "Prompt text"
```

The remaining Ollama-daemon compatibility commands still require a
user-installed Ollama runtime:

```sh
monkey list
monkey ps
monkey rm <model> [model...]
monkey cp <source> <destination>
monkey show <model>
monkey stop <model>
monkey push <model>
monkey create <model> --file Modelfile
monkey signin
monkey signout
monkey serve
```

Shared desktop/headless commands:

```sh
monkey acp
monkey revert [checkpoint-id]
monkey api-serve [--port <port>]

monkey stacks list
monkey stacks reindex <name>
monkey stacks embed-server start --model-path <embedding.gguf>
monkey stacks embed-server status
monkey stacks embed-server stop

monkey task list
monkey task validate <recipe-file>
monkey task run <name-or-path> [--param key=value ...] [--json]
monkey task schedule <name-or-path> --cron "<expr>"

monkey workflow list
monkey workflow validate <definition.json>
monkey workflow run <workflow-id> [--inputs '{}'] [--secrets '{}']
monkey workflow history [run-id]
monkey workflow replay <workflow-id> <source-run-id> <boundary-node-id> --approval

monkey skills list [--json]
monkey skills preview-local <folder> [--scope global|workspace]
monkey skills install-local <folder> --approval-digest <sha256> --yes
monkey skills preview-git <repository-url> <40-char-commit> [--subdirectory <path>]
monkey skills install-git <repository-url> <40-char-commit> --approval-digest <sha256> --yes
monkey skills enable|disable|rollback|uninstall <command>

monkey plugins list [--json]
monkey plugins health [--json]
monkey security audit [--deep] [--fix] [--json]

monkey daemon install
monkey daemon status [--json]
monkey daemon run <recipe> [--owned-worktree] [--json]
monkey daemon attach <run-id> [--follow] [--json]
monkey daemon pause|resume|cancel <run-id>
monkey daemon retry <run-id> [--acknowledge-side-effects]
monkey daemon kill-switch engage|release|status
monkey daemon trigger --help
monkey daemon remote --help
```

Inside the REPL, `/help` lists terminal-only controls such as `/set`, `/show`, `/save`, `/load`, `/revert`, `/persona`, `/prompts`, `/verify`, `/clear`, and `/bye`. Installed skill invocations use the same frozen turn-scoped prompt composition as desktop chat.

The desktop bundle stages `monkey-cli` as a Tauri sidecar and performs a best-effort, non-elevated installation of the `monkey` command on first launch:

- **macOS/Linux:** `/usr/local/bin/monkey` when writable, otherwise `~/.local/bin/monkey`.
- **Windows:** `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe`, with that directory added to the user `PATH`.

It does not edit shell startup files. If the selected directory is not already on `PATH`, add it once yourself. A development launch stages the sidecar automatically; once staged or installed, use the same `monkey` commands shown above. The Rust target remains named `monkey-cli` internally.

## Model setup

1. For an app-owned local model, open **Settings → Local Models → Add custom model**, enter an Ollama tag such as `llama3.2:3b` or a Hugging Face reference such as `hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M`, review the resolved file/size/license/digest metadata, then install and start it. No Ollama installation is required.
2. For a user-managed Ollama daemon, open **Settings → Ollama**, confirm the daemon is reachable, pull/import a model, and select it.
3. For cloud/BYOK, open **Settings → AI Providers**, store the key, refresh the provider model list, and select a model.
4. For MLX, configure the supported Apple Silicon MLX runtime in **Settings → Runtime Hub → Runtimes**.

Other important Settings surfaces include **Security Doctor**, **Companion**, **Portability**, **Knowledge**, **Ecosystem**, **Browser Verification**, **Git Delivery**, **Background Agents**, **MCP**, **Prompts/Skills**, **API Server**, **Tasks**, **Rules**, **Automation**, **Usage**, and **Keyboard Shortcuts**.

## Workspace and trust boundaries

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git/GitHub, workflow, background, capture, and remote actions use their applicable permission/grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled/headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending/confirmed or `needs_reconciliation`; ambiguous effects are not retried as if they were known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

Security Doctor is a posture aid, not a replacement for operating-system updates, endpoint security, or a release penetration test.

## Current limitations

- The Runtime Hub supports checksum/provenance validation and configured catalogs, but this repository does not include a publisher-operated, platform-complete signed `llama.cpp`/MLX artifact feed. ROCm, Vulkan, and DirectML are not advertised as maintained managed runtimes.
- Hardware-fit estimates and runtime controls are implemented, but the plus-or-minus-15% memory matrix, clean-machine lifecycle checks, and MLX release gate still need maintained physical reference hardware. Edge-device profiles are static heuristics: no benchmark in this app measures throughput or latency yet (see [ROADMAP.md](ROADMAP.md)).
- Several surfaces are real but deliberately narrower than their names suggest: Memory Studio has two scopes and no pin/merge/expire; approval chains are sequential and answered by the same desktop user; the connector catalog covers 5 of ~17 providers using pasted tokens rather than branded OAuth; Local App Builder's five templates are cosmetically similar; inbox triage is read-only with no rules engine; and Team Mode's RBAC is enforced at one defined point, with its audit trail attributing the exporter rather than the approver.
- Record & Replay's draft/review/replay pipeline is real, including credential redaction, but "recording" means entering selectors in the workbench form — not demonstrating an interaction by clicking through it.
- Sandboxed execution uses a macOS Seatbelt profile plus a disposable workspace copy; there are no containers or VMs, and non-macOS platforms get the restricted-cwd/env isolation only. Every run reports which isolation actually applied, and the platform's capability is now reported *before* a run too — as a warning above the Run button and as a Security Doctor finding — because the panel offers the same button everywhere and generated MCP server code is probed through it. On a platform with no kernel boundary a command can still read and write your real files by absolute path. The Seatbelt profile's network denial is enforcement, not just configuration: a test asserts a connection that succeeds with network allowed fails when it is denied. The sandbox is an opt-in feature and not the app's execution boundary — the agent's own shell tool does not run under it on any platform.
- Control Desktop keeps no local audit log or screenshots on the desktop side (the daemon-hosted remote path does record them to the run ledger), does not block sensitive system dialogs, and matches its allowlist by application identity rather than verifying the frontmost window.
- VS Code completion requires a real installed Ollama model that advertises `insert`; the latency/compile gate cannot be claimed on a machine without one.
- Browser verification uses disposable profiles. Persistent authenticated profiles, file transfer, clipboard, browser extensions, and general host-computer control remain intentionally out of scope.
- The Windows and Linux/X11 Control Desktop input backends compile and their pure helper logic (Wayland detection, consent-dialog parsing, UTF-16 handling) is tested, but neither has had a full runtime pass on real Windows or Linux hardware yet — that verification remains a release gate, not a completed claim. The in-app browser pane also relies on Tauri's unstable multiwebview API.
- GitHub delivery needs local `git` plus authenticated `gh`; hosted Actions need user-supplied provider credentials, while Ollama review needs a user-owned self-hosted runner.
- The local OCR, speech, meeting, and image paths require configured binaries/models/endpoints. WER, diarization error rate, real-time factor, and image hardware behavior are not claimed until run against the documented external fixtures and hardware.
- Remote handoff requires a user-owned reachable network and valid TLS identity. There is no Little Monkey relay, account service, RBAC/SSO plane, or hosted GPU.
- The mobile companion pairs, browses, approves, chats, launches saved workflows, and uploads captures — but browsing is online-only, push delivery needs an operator-selected provider, and pairing transfers the invitation as a file or pasted text rather than a QR code. Physical-device, signing, and store-submission gates are unmet.
- Release hardening—full clean-profile migrations, signed/notarized installers on every platform, accessibility/locale completion, performance budgets, dependency review, and penetration testing—remains a release gate rather than a completed claim. The in-app updater is real on all three desktop platforms: it checks in the background (8s after launch, every 6h, and on window refocus when the last check is over an hour old), stages the bundle, and only then shows a relaunch card in the session sidebar — macOS/Linux install underneath the running app, while Windows defers its installer to the card click so an update can never kill a turn mid-flight. Its remaining limits: there is no manual "check for updates" control, and a failed check is silent, so an unreachable endpoint is indistinguishable from being up to date; Linux self-update covers the AppImage only, never a `.deb`/`.rpm` install; and because the release workflow creates its release as a draft and GitHub does not serve draft assets, no update reaches an installed app until that draft is published by hand. Signing is macOS-only; and the ten non-English locales each fall back to English for roughly a third of their keys.

## Project layout

- `src/` — React UI, Zustand stores, chat/Compare/Crew flows, the workspace sidebar (files, review, terminal, in-app browser, background tasks), portability/search, durable run clients, skills/slash commands, and Settings panels.
- `src-tauri/src/` — Rust model/runtime, permission, workspace, run ledger, assets, Knowledge 2.0, packages/workflows, browser, Git delivery, daemon bridge, companion, and Security Doctor services exposed through Tauri commands.
- `src-tauri/src/bin/monkey-cli/` — terminal chat/REPL, ACP, model management, workflows, skills/plugins/security, daemon, remote-controller, stacks, tasks, and shared headless tooling.
- `extensions/little-monkey-vscode/` and `extensions/little-monkey-jetbrains/` — thin IDE clients.
- `.github/actions/little-monkey-review/` — reusable PR-review action implementation and contract test.
- `src-tauri/fixtures/` — deterministic browser and knowledge acceptance fixtures.

## Contributing

Bug reports, fixes, and feature proposals are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers the development setup, the full check suite, what CI runs on each platform, and the invariants a change has to hold — honest capability claims, no fabricated runtime values, untrusted content that cannot approve its own operation, and unchanged permission and network boundaries.

Pull requests target `develop`; `main` is the release branch. Security issues go through a [private advisory](https://github.com/AA-Box/little-monkey/security/advisories/new) rather than a public issue — see [SECURITY.md](SECURITY.md).

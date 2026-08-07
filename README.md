# <img width="50" height="51" alt="LM-logo" src="https://github.com/user-attachments/assets/84651d01-f18b-4c49-b203-8d1b7e8f16b6" /> Little Monkey

Little Monkey is a local-first desktop workspace for agentic AI, built on Tauri 2 with a React front end and a Rust backend. It runs against a managed `llama.cpp`, Ollama, MLX on supported Apple Silicon, or any OpenAI-compatible provider you configure, and it generates images, video, and speech locally through its own managed `stable-diffusion.cpp` and `llama-tts` runtimes.

Every surface shares one set of contracts — workspace, permission, run, model, generation, package, browser, Git, and background service — rather than reimplementing them per feature.

Capability claims in this document describe the current `develop` tree. Where a feature is narrower than its name suggests, the boundary is stated in [Limitations](#limitations). Work that is not built yet lives in [ROADMAP.md](ROADMAP.md).

## Features

### Chat and collaboration

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

### Workspace: files, review, terminal, and browser

- Reopen into the folders you were working in. The attached set is snapshotted on change and reattached at launch; folders deleted or moved since the last run are dropped rather than blocking the restore. Permission grants stay session-scoped and are never restored with a workspace.
- Work across eight right-sidebar tabs — code review, single-file diff, terminal, browser, side tasks, workspace files, background tasks, and processes. Tabs stay mounted, share one drag-resizable persisted width, support a region-wide fullscreen toggle, and each has a keyboard shortcut on every platform.
- Review changes in a git-backed panel using real porcelain output, with a per-file diff view and PR awareness. Pick the base — the branch's merge-base with its upstream, or HEAD — and the layout — every diff stacked, or one file at a time. Against HEAD the file list is uncapped and each diff loads on open; against the merge-base the panel is bounded by a 300-file payload cap and says so.
- Map acceptance criteria onto that diff. Paste the criteria and each returns covered, partial, or not covered with clickable citations into exact hunks. Facts computed from git — changed files, numbered hunks and line ranges, added or removed exported declarations, and a digest of all of it — are rendered separately from model claims about them, and every claim is checked against those facts: a claim citing a hunk the diff does not contain is discarded with the invented id shown, and a coverage claim without a valid citation is marked unsupported. Headline counts come from set arithmetic over surviving claims.
- Run a real terminal. Keystrokes go to the PTY through an embedded xterm.js emulator, so the shell supplies its own prompt, colors, line editing, history, and completions. A session auto-starts per workspace, with dock-right, drag-to-resize, and fullscreen.
- Browse from an in-app tabbed pane backed by real child webviews: tab strip, favicons and loading state, smart address bar, back/forward/reload, and `window.open` reopened as a tab. Only `http:`, `https:`, and `about:` load, and remote pages get no Tauri IPC surface.
- Track live background work from a running-tasks pill above the composer. The Background Tasks panel separates running from finished, offers per-card stop, token and tool-use counts, and an inline transcript. Parallel subagent calls in one turn collapse into a group card with per-agent status; each cancels independently of the parent turn.

### Agent tools, permissions, and egress policy

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

### Knowledge Stacks 2.0

- Ingest local files and folders, projects, URLs, sitemaps, selected chats, and configured WebDAV sources.
- Extract text and source locations from text and code, HTML, PDF, DOCX headings and tables, XLSX sheets and cell ranges, and PPTX slides and notes. Macros, formulas, embedded scripts, and automatic external-link execution are not enabled.
- Refresh incrementally with content hashes, connector cursors, deletion propagation, progress, cancellation, retry state, and optional daemon scheduling.
- Add local OCR through a verified or explicitly selected worker, with language, provenance, size, digest, license, progress, and cancellation controls.
- Fuse lexical retrieval with vector similarity and optional reranking, retaining the existing local vector path.
- Inspect retrieval end to end: normalized query, filters, candidates, lexical and vector scores, fused rank, reranker score, exclusions, token budget, and final context. Copy a reproducible diagnostic bundle or preview local PII and secret redaction.
- Import a Knowledge 1.0 stack as a v2 generation without re-embedding. Existing vectors, chunk text, and per-file digests are reused and no model is invoked. The import seeds a real v2 source per v1 source and the imported objects carry those ids, so they refresh and prune like any other; the first refresh re-extracts them with true v2 boundaries. Imports are all-or-nothing, the v1 index stays readable, and an unsupported v1 source kind refuses by name rather than being dropped.

### Runtime and API Hub

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

### Studio: image, video, and speech generation

- Switch the main view between **Chat** and **Studio** to run text-to-image, image-to-image, text-to-video, image-to-video, and speech — optionally in a voice cloned from a reference clip — from weights on your own machine, with no provider account or remote endpoint involved.
- Images and video run on an app-owned managed `sd-server` (stable-diffusion.cpp); speech runs on a separately pinned `llama-tts`. Both use the same rails as the managed `llama.cpp` chat runtime — pinned version, per-file SHA-256 against a manifest digest compiled into the app, atomic publish, and a per-runtime versioned directory and install lock — so installing or updating one cannot disturb another.
- Describe a model as the set of component files it is, because `sd-server` binds its whole model set at launch: typed slots (all-in-one checkpoint, diffusion model plus a mixture's high-noise stage, CLIP-L, CLIP-G, CLIP-vision, T5-XXL or an LLM text encoder, VAE, audio VAE, TAESD, mmproj, vocoder), per-model defaults, a RAM floor, and license terms. The add form prefills family and slot guesses from a weight file's name and lets you overwrite each one; a name that says nothing gets no guess. Adding a family is a registry entry, not new code, and switching models relaunches the engine.
- Download weights through the same Hugging Face downloader the model manager uses, so Studio and Runtime Hub share one progress stream and one cancellation path and an interrupted transfer leaves no partial file. Keep a LoRA stack and reusable component parts, choose sampler, scheduler, seed, steps, CFG, and a hires upscaler, browse and prune a gallery, cancel a run or a download, and unload the engine.
- Gate a license rather than mirror it: a model whose terms restrict territories shows those terms and requires acceptance before your own download begins, and such weights are never served from this project. Request validation snaps canvas edges onto the sampler's multiple-of-32 grid and clip length onto the backend's frame grid, so the duration the UI offers is the clip produced.

### Skills, plugins, MCP Apps, and workflows

- Install data-only `SKILL.md` skills globally or per workspace from a reviewed local folder or an immutable 40-character Git commit. Preview returns the exact SHA-256 approval digest; symlinks, special files, mutable Git refs, command collisions, oversized trees, and unmet OS, binary, or environment requirements fail closed.
- Invoke up to five installed skills at the start of a turn, for example `/review /testing check this patch`. The selected instructions, version, source, and digest are frozen into that turn and never expand tool permissions.
- Create a quarantined skill proposal with `/learn command | instructions`. It activates only after its risk flags are reviewed and its exact digest approved, and it can be rejected or rolled back.
- Manage signed declarative packages in **Settings → Ecosystem** with install and update permission previews, pins, enable and disable, rollback, revocation state, uninstall, offline cache, and portable export and import. Local unsigned development packages stay data-only behind an explicit warning; unsigned Git packages and executable payloads are rejected.
- Start from a signed first-party catalog of six skills (review, testing, documentation, browser QA, release preparation, knowledge workflows) plus declarative GitHub, GitLab, WebDAV, and REST/webhook connector packages.
- Inspect plugin health and component setup, use package assistants, activate package workflow templates, and apply verified package rules to normal, Compare, and Crew turns with provenance.
- Configure remote MCP OAuth metadata and tokens, preserve structured MCP content, route relevant tools without bypassing allowlists, and host interactive MCP Apps in an opaque-origin window with a narrow declared bridge and a text fallback.
- Connect remote MCP servers over OAuth with no client credentials shipped in this binary: servers supporting dynamic client registration are one click, and the rest use an OAuth app you register yourself, stored in your keychain — see [docs/byo-oauth-clients.md](docs/byo-oauth-clients.md).
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

### Agent workbenches

Each workbench is a real model-driven flow. Where its scope is narrower than its name, that boundary is in [Limitations](#limitations).

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

### Runs, limits, and cost

- See everything the app is executing in one table. A chat turn, daemon job, `task`-tool subagent, Crew member, workflow run and each of its node instances, remote-queued work, background shell, and side task each create a record with a shared id scheme (`p-<kind>-<uuid>`), a parent id, one state machine (`admitted → running → suspended → exited`), the owning workspace and profile as queryable columns, a declared limit set, and a structured exit. List it with `monkey processes` (`--kind`, `--all`, `--workspace`, `--parent`, `--json`) or `monkey processes show <id>` for a process and its descendants. The name is `processes` because `monkey ps` is the Ollama-compatible "list running models".
- Invalid transitions are refused rather than silently applied, and both that rule and "a row is `exited` if and only if it carries an exit status" are enforced in Rust and by SQL triggers, because companion stores reach the shared ledger connection directly.
- Stale records left by a killed app are reaped at startup, scoped to the kinds the app owns so live daemon work is never declared lost. Work with no fixed owner — any workflow run, since both the app and `monkey workflow run` host them — is reaped by whether its recorded host process still exists, so whichever host starts next cleans up after whichever died. A row that recorded no host is never reaped: pid reuse can only keep a stale row alive longer, never close one whose work is still running.
- End the whole process tree on a timeout. Shell tools, verify commands, and sandboxed runs each get their own process group, and a timeout or Stop terminates that group — TERM first so a build can flush output and clean up temp files, then KILL for anything that ignored it.
- Hold tool children to a kernel bound, not only a supervised one. Shell tools, verify commands, and sandboxed runs get their resource limits installed between `fork` and `exec`, so the program never runs unbounded and everything it spawns inherits them, including grandchildren a watchdog cannot see.
- Cap the shell output that reaches a model at 20,000 bytes per stream, keeping the end where a failing command puts its diagnostic and stating on the wire whether anything was dropped. Callers that parse output as a document rather than showing it to a model can request all of it.
- Record a budget kill as `limit_exceeded` rather than a plain cancel, naming which limit fired and what the measurement was, so a budget that worked is distinguishable from someone pressing Stop.
- Finish teardown when a budget fires: a browser session that hits its action quota is stopped rather than left running unreachable, and a workflow run killed for exceeding its wall clock is recorded as stopped by that budget rather than left reading as running forever.
- Reclaim browser sessions nothing is driving. A sweep every 30 seconds retires sessions past their time limit or whose browser has died and records which bound fired.
- Give chat turns, subagents, Crew members, and side tasks an optional wall-clock budget, enforced by the same sweep that delivers stop and pause and recorded as a limit rather than a cancel.
- Derive each kind's declared bounds from its kind rather than restating them per subsystem, so `monkey processes show` reports the bounds a process actually has.
- Stop or suspend anything from anywhere, including a terminal. `monkey processes signal <id> stop|suspend|resume|kill` (with `monkey processes signals` for the support matrix) records durable intent on the process row rather than in a live handle, which is what lets it reach work this app is not running and survive a restart. A kind that cannot honour a signal refuses it with the reason instead of appearing to succeed. Delivery is per-owner: the daemon reads the latch once per tick, the desktop reads it through the `processes://changed` event plus a 2-second catch-up query, and each hands it to the primitive that kind already had.
- `stop` and `kill` are separate latches, not one bit with two names. A kill is a stop with a stronger delivery promise — immediate `SIGKILL` to the process group where the app owns one, against stop's TERM-grace-KILL wind-down — escalation is one-way, and a kill recorded without a stop is refused by a SQL trigger. The operator kill switch takes the immediate path; on Windows `taskkill /F` makes the two coincide, and the matrix says so.
- Pause and resume reach the loops that can honour them: chat turns, subagents, Crew members, workflow runs, background shells, daemon jobs, and side tasks each park at a safe point and only then report `suspended`. A workflow node refuses suspend with its reason — pausing operates at the owning run's level boundary, which the headless executor observes — and that same latch makes a daemon-hosted workflow run cancellable from anywhere that can write it. A paired controller gets pause and resume as its own scoped remote action (`monkey remote pause|resume`, `POST /v1/remote/runs/{id}/pause`), strictly weaker than cancel, so trust to suspend is not trust to destroy. Restart policy is declared per kind: exactly one kind, the daemon job, is restartable, because only it has both a supervisor outliving the process and a durable description of what to run.
- Attribute every recorded egress refusal. A blocked outbound request carries either the id of the run that caused it or one of five coded reasons why it has no run — user action, scheduled work, inbound request, startup, or shared transport — never a blank. Each site was scoped individually, because a `tokio::spawn` or `spawn_blocking` between the scope and the record voids the attribution.
- Approve, inspect, and replay from one place: the Agent Inbox and Run Dashboard put approvals from desktop, daemon, and remote controller on one screen with a per-run event timeline.
- Export a Run Capsule — a redacted, replayable record of a run — and replay it by class.
- Track token usage and cost in **Settings → Usage**: per-request cost against rates you enter, daily and monthly budgets, and a `warn` or `pause` enforcement mode checked before every provider request. Rates are yours; the app never claims to read a provider invoice.
- Get a Daily Brief aggregating real run, task, and read-only MCP state, and search everything with Universal Search, whose workspace filter is validated against the roots actually attached to this instance.

### Developer integrations

- Run `monkey acp` as an ACP v1 stdio server. Little Monkey remains the approval authority and carries streaming, tool status, cancellation, diagnostics, artifacts, checkpoints, and diffs through the durable run protocol.
- Use the VS Code extension in `extensions/little-monkey-vscode` for active-file, selection, and Problems context, native diff review, explicit selection edits, and optional local Ollama FIM completion. Completion is off by default, requires an explicitly allowed model whose live metadata advertises `insert`, cancels stale document versions, and never falls back to cloud.
- Use the JetBrains plugin in `extensions/little-monkey-jetbrains` for IntelliJ IDEA, Android Studio, and compatible IDEs. It captures exact editor context and diagnostics, opens read-only diff previews, and cannot silently approve or apply mutations.
- Start an owned disposable Chromium session from **Settings → Browser Verification**. Navigate, inspect DOM and accessibility state, click, type, scroll, capture screenshots, and retain console and network evidence as durable artifacts, under exact-origin grants, DNS rechecks, quotas, cancellation, and explicit loopback approval.
- Create and recover Little Monkey-owned Git worktrees, inspect HEAD, staged, and unstaged diffs, stage selected paths, commit, push only declared owned branches, and archive or clean owned worktrees.
- Read GitHub issues, PRs, unresolved review threads, and checks through existing `gh` authentication; create and update owned draft PRs, run a local Ollama PR reviewer, publish one deduplicated review report, and queue a selected review comment as an isolated daemon patch task. Merge, force-push, branch deletion, and automatic thread resolution are not exposed.

### Background agents and remote handoff

- Install a current-user `monkey daemon` service explicitly, with bounded concurrency, queue size, retention, notifications, and an optional loopback webhook listener.
- Queue immutable recipe and workflow runs with idempotency keys, budgets, approval waits, pause and resume, attach and detach, cancellation, retry, crash recovery, orphan detection, owned worktrees, and a durable global kill switch.
- Configure persistent cron, filesystem, signed webhook, and GitHub triggers with replay protection and deduplication.
- Pair a user-owned remote runner over direct, Tailscale, or SSH-forwarded HTTPS with pinned TLS, mutually scoped credentials, rotation and revocation, replay protection, and audit history. A controller may view events, inspect bounded artifacts, approve digest-bound requests, cancel runs, or engage the kill switch only when its invitation grants that exact action. Inference, tools, workspaces, and provider keys stay on the runner; Little Monkey operates no relay.
- Grant a paired controller a scoped Control Desktop action — real mouse and keyboard input on macOS, Windows, and Linux/X11, with Linux/Wayland failing closed. Every action is gated by local consent, per-action by default or batch only when the remote request and local operator agree. A cross-process session lock prevents the app and daemon from driving input at once, periodic screenshots are recorded to the run ledger, and revoking a device or engaging the kill switch force-stops a live session.

### Desktop companion and mobile

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

## Prerequisites

- Node.js, `pnpm`, Rust, Cargo, and the Tauri 2 prerequisites for your platform.
- Desktop releases include a pinned, checksum-verified `llama.cpp` runtime. Source builds stage the same official runtime before `tauri dev` and `tauri build`; a system `llama-server` is a development fallback only.
- Studio generation (optional): the managed `sd-server` and `llama-tts` runtimes, staged with `pnpm stage:runtime:sd` and `pnpm stage:runtime:tts`. `sd-server` exists for Apple Silicon (Metal), x86_64 Linux (Vulkan), and x86_64 Windows (Vulkan) only. Model weights are yours to supply.
- Ollama runtime (optional): reachable at `http://127.0.0.1:11434` for the explicit Ollama provider or daemon-management commands.
- MLX runtime (optional): supported Apple Silicon plus the configured MLX Python environment.
- Browser verification (optional): a supported Chromium or Chrome binary.
- GitHub delivery (optional): Git and an authenticated GitHub CLI (`gh`).
- Local OCR, transcription, image generation, IDE extensions, and remote handoff (optional): their configured worker, model, endpoint, SDK, or TLS identity.

On macOS, the unmanaged fallback is `brew install llama.cpp`. The Runtime Hub can also install checksum-pinned artifacts from a configured catalog; this repository does not provide a publisher-operated feed for every platform and runtime.

## Development

```sh
pnpm install
pnpm tauri dev       # stage llama.cpp + the CLI sidecar, then run the app
pnpm dev             # Vite front end only
pnpm build           # TypeScript check and production front-end build
pnpm tauri build     # desktop bundle containing the managed runtime
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

Opt-in checks that need real local models or hardware:

```sh
pnpm test:compare:live
```

```sh
cd extensions/little-monkey-vscode
LITTLE_MONKEY_COMPLETION_MODEL='your-exact-fim-tag' npm run benchmark:completions
```

## CLI

The installed command is `monkey`. The preferred chat form is model first:

```sh
monkey llama3.2 "Summarize this project"       # auto-resolve target/provider
monkey llama3.2                                # interactive REPL
monkey --provider openai gpt-4.1-mini "Review this codebase"
monkey --local-url http://127.0.0.1:8090 local-model "Inspect the workspace"
```

For the app-owned path that needs neither Ollama nor a separate `llama.cpp`, pull or run a public Ollama Registry tag or a public Hugging Face single-file GGUF reference:

```sh
monkey pull llama3.2:3b
monkey run llama3.2:3b "Summarize this project"
monkey run hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M
```

`monkey run` resolves immutable metadata, verifies the expected model SHA-256, inspects the checksum-bound GGUF's embedded chat template before advertising tool support, resumes interrupted downloads, reuses verified installs offline, and starts the bundled loopback-only runtime for that session. Ollama's separate Go-template layer is never passed to `llama.cpp`, and the runtime's per-file manifest is authenticated by a digest embedded in the compiled app. Private or gated Hugging Face repositories, non-GGUF or sharded artifacts, and Ollama models requiring separate adapters or projectors are rejected with a clear error.

If a non-local model is exposed by more than one configured provider, `monkey` asks for `--provider <id>` rather than guessing. The legacy `--ollama` and `--model` forms remain aliases.

Useful chat flags:

- `--workspace <path>` — sandbox tool access to a workspace; defaults to the current directory.
- `--permission-mode manual|acceptEdits|smart|plan|auto|bypass` — terminal permission policy.
- `--provider <id>` — override or disambiguate a configured provider.
- `--local-url <url>` — explicit local OpenAI-compatible endpoint.
- `--persona <slash-command>`, repeatable `--stack <name>` — attach saved context.
- `--verify` / `--no-verify`, `--subagents`, `--no-rules`, `--no-mcp` — opt into verification or subagents, or suppress configured context.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose`, `--attach-images` — generation controls.
- `--num-ctx` — managed-runtime or Ollama context size; `--keepalive`, `--think`, `--hidethinking` remain Ollama-native.

Ollama-daemon compatibility commands still require a user-installed Ollama runtime:

```sh
monkey list
monkey ps
monkey show <model>
monkey rm <model> [model...]
monkey cp <source> <destination>
monkey stop <model>
monkey push <model>
monkey create <model> --file Modelfile
monkey signin
monkey signout
monkey serve
```

Shared desktop and headless commands:

```sh
monkey acp
monkey revert [checkpoint-id]
monkey api-serve [--port <port>]
monkey processes [--kind <kind>] [--all] [--json]
monkey processes show <id>
monkey processes signal <id> stop|suspend|resume|kill

monkey stacks list | reindex <name>
monkey stacks embed-server start --model-path <embedding.gguf> | status | stop

monkey task list | validate <recipe-file>
monkey task run <name-or-path> [--param key=value ...] [--json]
monkey task schedule <name-or-path> --cron "<expr>"

monkey workflow list | validate <definition.json>
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

monkey daemon install | status [--json]
monkey daemon run <recipe> [--owned-worktree] [--json]
monkey daemon attach <run-id> [--follow] [--json]
monkey daemon pause|resume|cancel <run-id>
monkey daemon retry <run-id> [--acknowledge-side-effects]
monkey daemon kill-switch engage|release|status
monkey daemon trigger --help
monkey daemon remote --help
```

In the REPL, `/help` lists terminal-only controls such as `/set`, `/show`, `/save`, `/load`, `/revert`, `/persona`, `/prompts`, `/verify`, `/clear`, and `/bye`. Installed skill invocations use the same frozen, turn-scoped prompt composition as desktop chat.

The desktop bundle stages `monkey-cli` as a Tauri sidecar and performs a best-effort, non-elevated install of the `monkey` command on first launch — `/usr/local/bin/monkey` when writable, otherwise `~/.local/bin/monkey`; on Windows `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe`, with that directory added to the user `PATH`. Shell startup files are not edited. The Rust target remains named `monkey-cli`.

## Model setup

1. **App-owned local model** — **Settings → Local Models → Add custom model**: enter an Ollama tag such as `llama3.2:3b` or a Hugging Face reference such as `hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M`, review the resolved file, size, license, and digest metadata, then install and start. No Ollama installation required.
2. **User-managed Ollama** — **Settings → Ollama**: confirm the daemon is reachable, pull or import a model, and select it.
3. **Cloud or BYOK** — **Settings → AI Providers**: store the key, refresh the model list, and select a model.
4. **MLX** — **Settings → Runtime Hub → Runtimes**: configure the supported Apple Silicon MLX runtime.

Other Settings surfaces: **Security Doctor**, **Companion**, **Portability**, **Knowledge**, **Ecosystem**, **Browser Verification**, **Git Delivery**, **Background Agents**, **MCP**, **Prompts/Skills**, **API Server**, **Tasks**, **Rules**, **Automation**, **Usage**, and **Keyboard Shortcuts**.

## Workspace and trust boundaries

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git and GitHub, workflow, background, capture, and remote actions use their applicable permission or grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled and headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending, confirmed, or `needs_reconciliation`; ambiguous effects are not retried as if known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

Security Doctor is a posture aid, not a substitute for operating-system updates, endpoint security, or a release penetration test.

## Limitations

**Runtimes and hardware**

- No publisher-operated, platform-complete signed `llama.cpp` or MLX artifact feed ships with this repository. ROCm, Vulkan, and DirectML are not advertised as maintained managed runtimes. The MLX service package can be built locally with `pnpm mlx:package` and installed from the Runtime Hub, but it installs only when signed by the pinned release key — there is no hosted feed serving one.
- Hardware-fit estimates and runtime controls are implemented, but the ±15% memory matrix, clean-machine lifecycle checks, and the MLX release gate need maintained physical reference hardware. Edge-device profiles are static heuristics: no benchmark here measures throughput or latency.
- `/v1/embeddings` produces real vectors only when the resolved runtime reaches an embeddings-capable backend (Ollama today); otherwise it returns an unsupported error rather than a fabricated vector. Native Ollama `/api/generate`, `/api/pull`, and `/api/show` are not implemented, and `/api/chat` returns a complete response rather than per-token streaming — real SSE streaming is OpenAI-compatible only.
- Vision is projector management and wire transport, not in-app vision chat: the main chat UI does not yet route attached images through that path, and the renderer cannot represent image blocks or reasoning content, so vision is never advertised as ready.
- Studio ships no model catalog, and its RAM floor is a check rather than a guarantee. `sd-server` covers three host targets, and the surface is hidden elsewhere rather than offered and failed at launch.

**Enforcement and isolation**

- Declared memory and wall-clock limits are not enforced by any OS mechanism: there is no cgroup or Windows job object, and `setrlimit` reaches tool children only. What exists is a userspace watchdog over daemon jobs plus per-tool timeouts. The daemon memory budget is opt-in and the wall-clock default is seven days, so both are effectively off unless a job asks for them.
- Of the kernel-held bounds, only core dumps are refused today. File-size and descriptor ceilings are implemented and tested but unset, since the agent shell legitimately downloads multi-gigabyte models; CPU time is deliberately uncapped because it accumulates per core; memory is not capped here, as the relevant limit is a no-op on macOS. On Windows this does nothing — the equivalent is a job object and is not built.
- No default wall-clock budget is set for chat turns, so that limit currently fires for nobody. A turn awaiting an unanswered permission prompt is indistinguishable from a working turn, so any default would kill turns for being slow to answer. A budget is a floor, not a ceiling: a turn inside a 120-second shell command cannot notice it until the command returns, and paused time still counts.
- The shell output cap applies to what is returned, not to how much is read, so a command writing gigabytes still buffers them for up to its timeout.
- Browser session sweeps cannot distinguish a tab you are still reading from one an agent abandoned, so the time limit applies to both. The disk limit is left out of the sweep, and a browser process id is still not recorded, so a crash of this app can orphan a Chromium nothing can kill.
- Sandboxed execution uses a macOS Seatbelt profile plus a disposable workspace copy — no containers or VMs — and other platforms get restricted-cwd and environment isolation only. Every run reports which isolation applied, and platform capability is reported before a run as a warning and a Security Doctor finding. On a platform with no kernel boundary a command can still read and write real files by absolute path. The Seatbelt network denial is enforcement, verified by test. The sandbox is opt-in and is not the app's execution boundary — the agent's shell tool does not run under it.
- Paused desktop work does not survive a restart: a suspended desktop-owned row is reaped as `exited(lost)` rather than offering a Resume that cannot come back. Durable intent survives; durable execution does not. A workflow node has no cancellation of its own, a Crew member carries no edge to its coordinator, and a retried daemon job becomes a new process rather than inheriting the original's parent.
- A run killed for exceeding a budget is shown as cancelled in the run ledger, because `RunStatus` carries no limit status and adding a terminal status is a protocol compatibility change; the distinguishable exit is on the process record.
- Inside a shared MCP transport, only the OAuth token fetch and keychain read run in the caller's task; the client library issues every actual request from its own worker, so those record neither a run nor a reason.

**Scope narrower than the name**

- Memory Studio has two scopes and no pin, merge, or expiry. Approval chains are sequential and answered by the same desktop user. The connector catalog covers 5 of roughly 17 providers with pasted tokens rather than branded OAuth. Local App Builder's five templates are cosmetically similar. Inbox triage is read-only with no rules engine. Team Mode's RBAC is enforced at one defined point, and its audit trail attributes the exporter rather than the approver. Cross-Repo Intelligence uses text search, not a semantic index.
- Record & Replay's draft, review, and replay pipeline is real, including credential redaction, but recording means entering selectors in the workbench form rather than demonstrating an interaction.
- Acceptance criteria are pasted by hand, declaration names are a text match on changed lines rather than a resolved reference graph, binary and oversized files carry no citable hunks, hunk excerpts sent to the model are capped at 8 lines, and the coverage report lives in memory for the session. A diff hitting the 300-file or 200-hunk cap is reported as an incomplete view of itself, so a "not covered" verdict over one is shown as unproven.
- Release-gate eval state is desktop-local, so CLI and API-server workflow starts are not gated.
- The Red-Team Lab's containment column exercises the real boundary functions but cannot prove from a panel that the loop invokes them; that claim belongs to the CI test.
- Control Desktop keeps no local audit log or screenshots on the desktop side (the daemon-hosted remote path records them to the run ledger), does not block sensitive system dialogs, and matches its allowlist by application identity rather than verifying the frontmost window. The Windows and Linux/X11 input backends compile and their pure helper logic is tested, but neither has had a full runtime pass on real hardware — that remains a release gate.
- Browser verification uses disposable profiles. Persistent authenticated profiles, file transfer, clipboard, extensions, and general host control are out of scope. The in-app browser pane relies on Tauri's unstable multiwebview API.
- VS Code completion requires an installed Ollama model advertising `insert`; its latency and compile gate cannot be claimed without one.
- GitHub delivery needs local `git` and authenticated `gh`; hosted Actions need user-supplied provider credentials, and Ollama review needs a user-owned self-hosted runner.
- Local OCR, speech, meeting, and image paths require configured binaries, models, or endpoints. WER, diarization error rate, real-time factor, and image hardware behavior are not claimed until run against the documented external fixtures and hardware.
- Remote handoff requires a user-owned reachable network and valid TLS identity. There is no relay, account service, RBAC/SSO plane, or hosted GPU.
- The mobile companion pairs, browses, approves, chats, launches saved workflows, and uploads captures, but browsing is online-only, push delivery needs an operator-selected provider, and pairing transfers the invitation as a file or pasted text rather than a QR code. Physical-device, signing, and store-submission gates are unmet.

**Release**

- Release hardening — clean-profile migrations, signed and notarized installers on every platform, accessibility and locale completion, performance budgets, dependency review, and penetration testing — remains a release gate rather than a completed claim. Signing is macOS-only, and the ten non-English locales each fall back to English for roughly a third of their keys.
- The in-app updater is real on all three desktop platforms: it checks in the background (8 seconds after launch, every 6 hours, and on window refocus when the last check is over an hour old), stages the bundle, then shows a relaunch card. macOS and Linux install underneath the running app; Windows defers its installer to the card click so an update cannot kill a turn mid-flight. Releases publish themselves: the workflow drafts while its six targets build, then flips the release once all of them have uploaded, so a failed target leaves a draft rather than shipping a partial release. Remaining limits: there is no manual check control, a failed check is silent, and Linux self-update covers the AppImage only.

## Project layout

- `src/` — React UI, Zustand stores, chat, Compare and Crew flows, the Studio generation section, the workspace sidebar, portability and search, durable run clients, skills and slash commands, and Settings panels.
- `src-tauri/src/` — Rust model and runtime services, managed runtimes and Studio generation, permissions, workspace, run ledger and egress attribution, assets, Knowledge 2.0, packages and workflows, browser, Git delivery, daemon bridge, companion, and Security Doctor, exposed through Tauri commands.
- `src-tauri/src/bin/monkey-cli/` — terminal chat and REPL, ACP, model management, workflows, skills, plugins, security, daemon, remote controller, stacks, tasks, and shared headless tooling.
- `extensions/little-monkey-vscode/`, `extensions/little-monkey-jetbrains/` — thin IDE clients.
- `.github/actions/little-monkey-review/` — reusable PR-review action and its contract test.
- `src-tauri/fixtures/` — deterministic browser and knowledge acceptance fixtures.

## Contributing

Bug reports, fixes, and feature proposals are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers development setup, the full check suite, what CI runs per platform, and the invariants a change must hold: honest capability claims, no fabricated runtime values, untrusted content that cannot approve its own operation, and unchanged permission and network boundaries.

Pull requests target `develop`; `main` is the release branch. Security issues go through a [private advisory](https://github.com/AA-Box/little-monkey/security/advisories/new) rather than a public issue — see [SECURITY.md](SECURITY.md).

# <img width="50" height="51" alt="LM-logo" src="https://github.com/user-attachments/assets/84651d01-f18b-4c49-b203-8d1b7e8f16b6" /> Little Monkey

Little Monkey is a local-first Tauri desktop workspace for agentic AI. It can run against managed `llama.cpp`, Ollama, MLX on supported Apple Silicon, or OpenAI-compatible providers that you configure. The React UI and Rust backend share workspace, permission, run, model, package, browser, Git, and background-service contracts instead of treating each surface as a separate product.

The current working tree includes the shipped foundations described below. Some release acceptance gates still require external hardware, credentials, services, signed publisher feeds, or cross-platform clean-machine testing; those are called out in [Current limitations](#current-limitations). Future product proposals and their acceptance boundaries live in [ROADMAP.md](ROADMAP.md).

## Features

### Chat, models, and collaboration

- Chat with managed `llama.cpp`, Ollama, MLX, or configured cloud/BYOK providers, with capability-aware routing, provider failover, context compaction, usage accounting, and rate-limit warnings.
- Compare one frozen prompt across two-to-four explicit local, Ollama, or provider targets with independent streaming, stop, retry, timing, usage, persistence, and response promotion. Compare runs default to no tools and keep their target snapshots even if global model settings change.
- Run saved Crew chats with a coordinator and bounded parallel persona members. Member transcripts remain isolated, coordinator synthesis is explicit, actor usage is attributed, and cancel-all reaches outstanding members.
- Keep multiple sessions, forks, groups, and a two-pane split view with independent streams.
- Attach files, folders, and images; reference workspace paths with `@`; select personas and knowledge stacks; and invoke skills with `/`.
- Search active and archived chats, messages, tool output, artifacts, and durable runs with date, model, persona, and workspace filters.
- Export a session as Markdown, JSON, or Word (`.docx`), translate individual messages or a whole thread while retaining the original, and create versioned portable backups.
- Create encrypted local snapshots with retention, preflight imports before changing live state, and use encrypted WebDAV backup with conflict copies and launch-time catch-up. Reliable unattended backup moves through the installed daemon.

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
- Show a Hardware Compatibility Matrix ("Driver Doctor") before any model download, model load, or runtime install: real detection of Metal, CUDA, ROCm, Vulkan, and (best-effort) DirectML, plus driver version, compute capability, Jetson, and hybrid/multi-GPU detection, with an honest `available`/`not_detected`/`driver_too_old`/`tool_missing`/`unsupported` status per backend that never fails just because a GPU tool or device is absent.
- Track each installed model's source registry, license, quantization, chat template, and multimodal projector in a content-addressed, digest-verified manifest; reuse an already-verified payload across asset variants/versions instead of re-downloading identical bytes, and never trust a corrupt local copy for reuse.
- Manage Ollama, `llama.cpp`, and MLX through one runtime contract with capability preflight, owned-process shutdown, logs, metrics, cancellation, and resource-aware scheduling.
- Before a model loads, simulate a per-load offload plan from the live hardware snapshot: recommended context size, batch size, GPU layers offloaded vs. CPU spill, projector placement, and parallelism, each with a plain-language rationale and concrete suggestions for raising the budget.
- Serve the advertised OpenAI-compatible routes and Anthropic-compatible Messages subset, plus separately scoped model discovery/download/load/unload/status/delete routes.
- Keep loopback as the default. Non-loopback serving requires an exact interface, TLS identity, authentication, pairing, rate limits, an exact CORS allowlist, explicit backends/scopes, and a policy that excludes file, shell, Git, MCP, and other agent-tool routes.
- Store private keys and provider credentials in the OS keychain; persisted configuration contains references rather than plaintext key material.

### Skills, plugins, MCP Apps, and workflows

- Install data-only `SKILL.md` skills globally or per workspace from a reviewed local folder or an immutable 40-character Git commit. Preview returns the exact SHA-256 approval digest; symlinks, special files, mutable Git refs, command collisions, oversized trees, and unmet OS/binary/environment requirements fail closed.
- Invoke up to five installed skills at the beginning of a chat turn, for example `/review /testing check this patch`. The selected instructions, version, source, and digest are frozen into that turn and never expand tool permissions.
- Use `/learn command | instructions` to create a quarantined local skill proposal. It becomes active only after reviewing risk flags and approving the exact digest, and it can be rejected or rolled back.
- Manage signed declarative packages in **Settings → Ecosystem** with install/update permission previews, pins, enable/disable, rollback, revocation state, uninstall, offline cache, and portable export/import. Local unsigned development packages remain data-only and require an explicit warning/approval; unsigned Git packages and executable payloads are rejected.
- Seed a signed first-party catalog containing six skills (review, testing, documentation, browser QA, release preparation, and knowledge workflows) plus declarative GitHub, GitLab, WebDAV, and REST/webhook connector packages.
- Inspect plugin health and component setup, use explicit package assistants, activate package workflow templates, and apply verified package rules to normal, Compare, and Crew turns with provenance.
- Configure remote MCP OAuth metadata/tokens, preserve structured MCP content, route relevant tools without bypassing allowlists, and host interactive MCP Apps in an opaque-origin window with a narrow declared bridge and text fallback.
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

### Multimodal desktop companion

- Open a restricted always-on-top companion overlay with a configurable global shortcut. Context capture is explicit and visibly granted; supported inputs include pasted text, an approved file, and a selected screen area. Emergency stop revokes active capture and cancels owned media jobs.
- Transcribe audio files, push-to-talk clips, or meeting recordings through a configured local `whisper.cpp`-style worker or an explicit provider. Timed speaker segments are retained when the backend supplies diarization, and meeting text is prepared for user-reviewed notes, decisions, questions, and action items. Raw audio is retained only when explicitly requested.
- Read text aloud with system TTS and stop playback through the same cancellation path.
- Configure user-owned ComfyUI or OpenAI-compatible image endpoints, then generate or edit when the endpoint advertises editing. Jobs retain prompt, negative prompt, model, seed, dimensions, steps, CFG, source/output hashes, progress, cancellation, metadata, and a gallery action that inserts an owned artifact into chat through the normal review path.

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
| `/learn command \| instructions` | Create a quarantined skill proposal for review. |
| `/<installed-skill> [request]` | Freeze and apply an installed skill to this turn. Up to five may be stacked. |

Built-ins run locally and deterministically. Unknown leading `/text` remains ordinary input, so paths are not silently consumed as commands.

## Prerequisites

- Node.js, `pnpm`, Rust, Cargo, and the Tauri 2 prerequisites for your operating system.
- Optional managed GGUF runtime: `llama-server` from `llama.cpp` on `PATH` or in a supported Homebrew location.
- Optional Ollama runtime: Ollama reachable at `http://127.0.0.1:11434`.
- Optional MLX runtime: supported Apple Silicon plus the configured MLX Python environment.
- Optional browser verification: a supported Chromium/Chrome binary.
- Optional GitHub delivery: Git and an authenticated GitHub CLI (`gh`).
- Optional local OCR, transcription, image generation, IDE extensions, and remote handoff: their explicitly configured worker/model, endpoint, SDK, or TLS identity.

On macOS, the existing unmanaged GGUF setup can use:

```sh
brew install llama.cpp
```

The Runtime Hub can also install checksum-pinned artifacts from a configured catalog. This repository does not claim a complete publisher-operated artifact feed for every platform/runtime.

## Development

```sh
pnpm install
pnpm tauri dev       # build/stage the CLI sidecar and run the desktop app
pnpm dev             # Vite frontend only
pnpm build           # TypeScript check and frontend production build
pnpm tauri build     # desktop bundles for the current platform
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
# Local-first automatic resolution; an installed Ollama tag wins.
monkey llama3.2 "Summarize this project"

# Omit the prompt for the interactive REPL.
monkey llama3.2

# Select a provider only when you want to override/disambiguate resolution.
monkey --provider openai gpt-4.1-mini "Review this codebase"
monkey --provider ollama llama3.2 "Explain the failing test"

# Explicit OpenAI-compatible local endpoint.
monkey --local-url http://127.0.0.1:8090 local-model "Inspect the workspace"
```

If a non-local model is exposed by more than one configured provider, `monkey` asks for `--provider <id>` instead of guessing. The legacy `--ollama`, `--model`, and `monkey run` forms remain compatibility aliases, but new scripts should use `monkey [--provider ID] MODEL [PROMPT]`.

Useful chat flags:

- `--workspace <path>` — sandbox tool access to a workspace; defaults to the current directory.
- `--permission-mode manual|acceptEdits|smart|plan|auto|bypass` — terminal permission policy.
- `--provider <id>` — override or disambiguate `ollama`, managed `llama.cpp`, OpenAI, Anthropic, Gemini, OpenRouter, or a custom provider.
- `--local-url <url>` — explicit local OpenAI-compatible endpoint.
- `--persona <slash-command>` and repeatable `--stack <name>` — attach saved context.
- `--verify` / `--no-verify`, `--subagents`, `--no-rules`, and `--no-mcp` — opt into verification/subagents or suppress configured context.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose`, and `--attach-images` — generation controls.
- `--num-ctx`, `--keepalive`, `--think`, and `--hidethinking` — Ollama-native controls.

Ollama-compatible model management remains available:

```sh
monkey list
monkey ps
monkey pull <model>
monkey run <model> "Prompt text"
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

1. For managed GGUF, install `llama-server` or configure a verified Runtime Hub catalog, then use **Settings → Local Models** or **Runtime Hub**.
2. For Ollama, open **Settings → Ollama**, confirm the daemon is reachable, pull/import a model, and select it.
3. For cloud/BYOK, open **Settings → AI Providers**, store the key, refresh the provider model list, and select a model.
4. For MLX, configure the supported Apple Silicon MLX runtime in **Settings → Runtime Hub → Runtimes**.

Other important Settings surfaces include **Security Doctor**, **Companion**, **Portability**, **Knowledge**, **Ecosystem**, **Browser Verification**, **Git Delivery**, **Background Agents**, **MCP**, **Prompts/Skills**, **API Server**, **Tasks**, **Rules**, **Automation**, **Usage**, and **Keyboard Shortcuts**.

## Workspace and trust boundaries

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes. Read-only workspace operations do not mutate files; mutating file, shell, memory, MCP, browser, Git/GitHub, workflow, background, capture, and remote actions use their applicable permission/grant boundary. A remote server's `readOnlyHint`, model output, webpage text, package instructions, or imported archive can never approve its own operation.

Shell commands run inside the workspace with bounded time and cancellation. Scheduled/headless recipes require an explicit permission mode and cannot use unattended `bypass`. External mutations are recorded as pending/confirmed or `needs_reconciliation`; ambiguous effects are not retried as if they were known safe. API keys, OAuth tokens, bearer secrets, remote device keys, and TLS private keys use the OS keychain where the feature supports credentials.

Security Doctor is a posture aid, not a replacement for operating-system updates, endpoint security, or a release penetration test.

## Current limitations

- The Runtime Hub supports checksum/provenance validation and configured catalogs, but this repository does not include a publisher-operated, platform-complete signed `llama.cpp`/MLX artifact feed. ROCm, Vulkan, and DirectML are not advertised as maintained managed runtimes.
- Hardware-fit estimates and runtime controls are implemented, but the roadmap's plus-or-minus-15% memory matrix, clean-machine lifecycle checks, and MLX release gate still need maintained physical reference hardware.
- VS Code completion requires a real installed Ollama model that advertises `insert`; the latency/compile gate cannot be claimed on a machine without one.
- Browser verification uses disposable profiles. Persistent authenticated profiles, file transfer, clipboard, browser extensions, and general host-computer control remain intentionally out of scope.
- GitHub delivery needs local `git` plus authenticated `gh`; hosted Actions need user-supplied provider credentials, while Ollama review needs a user-owned self-hosted runner.
- The local OCR, speech, meeting, and image paths require configured binaries/models/endpoints. WER, diarization error rate, real-time factor, and image hardware behavior are not claimed until run against the documented external fixtures and hardware.
- Remote handoff requires a user-owned reachable network and valid TLS identity. There is no Little Monkey relay, account service, RBAC/SSO plane, or hosted GPU.
- Release hardening—full clean-profile migrations, signed/notarized installers on every platform, accessibility/locale completion, performance budgets, dependency review, and penetration testing—remains a release gate rather than a completed claim.

## Project layout

- `src/` — React UI, Zustand stores, chat/Compare/Crew flows, portability/search, durable run clients, skills/slash commands, and Settings panels.
- `src-tauri/src/` — Rust model/runtime, permission, workspace, run ledger, assets, Knowledge 2.0, packages/workflows, browser, Git delivery, daemon bridge, companion, and Security Doctor services exposed through Tauri commands.
- `src-tauri/src/bin/monkey-cli/` — terminal chat/REPL, ACP, model management, workflows, skills/plugins/security, daemon, remote-controller, stacks, tasks, and shared headless tooling.
- `extensions/little-monkey-vscode/` and `extensions/little-monkey-jetbrains/` — thin IDE clients.
- `.github/actions/little-monkey-review/` — reusable PR-review action implementation and contract test.
- `src-tauri/fixtures/` — deterministic browser and knowledge acceptance fixtures.
- `graphify-out/` — generated architecture graph and wiki; run `graphify update .` after code changes.
- [ROADMAP.md](ROADMAP.md) — future product phases, scoped acceptance boundaries, research items, and explicit non-goals.
- [roadmap_audit_report.md](roadmap_audit_report.md) — preserved historical audit followed by a current working-tree closeout.

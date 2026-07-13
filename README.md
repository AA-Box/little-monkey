# Little Monkey

Little Monkey is a Tauri desktop app for running an agentic AI coding chat against local models, Ollama models, or OpenAI-compatible cloud providers. It combines a React chat UI with a Rust backend that manages model processes, workspace access, permissions, sessions, tool execution, and a growing set of agentic-coding staples (checkpoints, MCP, RAG, subagents, automations) — see [ROADMAP.md](ROADMAP.md) for what shipped in each milestone.

## Features

**Core chat and models**
- Chat with a local `llama.cpp` `llama-server`, Ollama, or cloud providers, with provider failover, vision-aware model switching, context compaction, and rate-limit warnings.
- Manage curated GGUF models, external `.gguf` files, Ollama tags, and custom OpenAI-compatible providers.
- Attach files, folders, and images to prompts; reference workspace paths with `@` mentions; insert saved prompts with `/` slash commands.
- Browse the active workspace, preview files, refresh diffs, inspect git status, and commit from the workspace bar.
- Store cloud API keys and MCP/search bearer tokens in the OS keychain through the Rust backend.
- Switch between light/dark themes and localized UI copy (11 locales).

**Agent tools and safety**
- Run tool-calling agent turns over an attached workspace: read files, list directories, glob, grep, edit/write files, and run shell commands.
- Gate mutating tools with permission modes (`manual`, `acceptEdits`, `smart`, `plan`, `auto`, `bypass`) and per-action prompts. Smart mode auto-approves low-risk writes using an LLM risk judge backed by a deterministic Rust path floor (dotfiles, lockfiles, CI config, etc. always fall through to a real prompt, regardless of the model's own risk assessment) — `run_shell` is never auto-approved outside `bypass`.
- **Checkpoints & rollback**: every mutating turn is checkpointed; revert or re-apply file changes, rewind the conversation, or both, from the in-chat timeline. Manifest format is versioned with fallback deserialization for older checkpoints.
- **Plan/Act workflow**: in `plan` mode the agent proposes a plan via a `present_plan` tool card you approve before it can act — approval flips the session into `acceptEdits`/your prior act mode.
- **Post-edit verification**: configure verify commands (lint/build/test) that auto-run after edits; failures feed back to the model for a bounded number of fix rounds, reported inline as `[Verify]` notices.

**Knowledge and memory**
- **Project rules & memory**: drop a `MONKEY.md` file in your repo (global or per-workspace) for persistent instructions, plus a `remember` tool the agent can use to save facts across sessions — both editable from Settings.
- **Knowledge Stacks (RAG)**: index local documents/PDFs (brute-force embedding search, no vector DB dependency) into named stacks; attach a stack to a chat to give the agent a `search_docs` tool with citation chips, or use doc-chat mode for automatic retrieval.
- **Prompt & persona library**: save reusable prompt snippets and personas (with Cherry Studio import/export compatibility), insert via slash commands or attach a persona to a session.

**Extensibility**
- **MCP client**: connect stdio or streamable-HTTP MCP servers with per-server tool allowlists; tools are namespaced `mcp__<server>__<tool>` and every call still goes through Little Monkey's own permission prompts (server-asserted `readOnlyHint` is never trusted).
- **Local OpenAI-compatible API server**: expose `llama-server`/Ollama (and, opt-in, your configured cloud providers) on `127.0.0.1:1234` with scoped, hash-stored auth tokens — a drop-in target for other tools, loopback-only by design.
- **Web fetch & search**: an SSRF-guarded `web_fetch` tool (blocks loopback/private/link-local ranges and re-validates redirects) plus `web_search` over DuckDuckGo, Brave, or SearXNG — both permission-gated, never silently injected into context.
- **Subagents**: delegate scoped subtasks to a bounded pool of parallel subagents (explore-only or code profiles) via a `task` tool, with live per-subagent progress rows in chat.
- **Artifacts**: render HTML/SVG/Mermaid previews inline from fenced code blocks, sandboxed via `srcdoc` or an opaque-origin `artifact://` protocol for interactive content.
- **Scheduled automations**: save recipes (YAML/JSON) with a mandatory explicit `permission_mode` and run them from the GUI, on an in-app scheduler, or headlessly via the CLI with JSON output and CI-friendly exit codes; export a launchd plist or crontab line instead of the app self-daemonizing.

## Prerequisites

- Node.js and `pnpm`
- Rust and Cargo
- Tauri 2 system prerequisites for your OS
- Optional for local GGUF models: `llama-server` from `llama.cpp`
- Optional for Ollama models: Ollama installed and reachable at `http://127.0.0.1:11434`

On macOS, the local GGUF path expects `llama-server` on `PATH` or in a common Homebrew location:

```sh
brew install llama.cpp
```

## Development

Install dependencies:

```sh
pnpm install
```

Run the desktop app in development:

```sh
pnpm tauri dev
```

Run only the Vite frontend:

```sh
pnpm dev
```

Build the frontend:

```sh
pnpm build
```

Build a Tauri bundle:

```sh
pnpm tauri build
```

## Testing

```sh
pnpm test
pnpm test:rust
```

## CLI

Little Monkey has a terminal agent. The intended installed command is `monkey`:

```sh
monkey --help
```

One-shot chat examples:

```sh
# Chat with an Ollama model
monkey --ollama llama3.2 "Summarize this project"

# Chat with a local OpenAI-compatible server such as llama-server
monkey --local-url http://127.0.0.1:8090 "Inspect the workspace"

# Chat with a configured cloud provider key from the desktop app
monkey --provider openai --model <model-id> "Review this codebase"
```

Omit the prompt to start the interactive REPL:

```sh
monkey --ollama llama3.2
```

Useful global chat flags:

- `--workspace <path>` - sandbox tool access to a workspace; defaults to the current directory.
- `--permission-mode manual|acceptEdits|smart|plan|auto|bypass` - controls terminal prompts for mutating tools; matches the desktop app's modes.
- `--provider <id> --model <model-id>` - use OpenAI, Anthropic, Gemini, OpenRouter, or a custom provider configured by the app.
- `--ollama <tag>` - use the local Ollama daemon.
- `--local-url <url>` - use a local OpenAI-compatible endpoint.
- `--persona <slash-command>` - append a saved persona (Settings > Prompts) to the system prompt.
- `--stack <name>` - attach a knowledge stack (repeatable), offering the agent `search_docs`.
- `--verify` / `--no-verify` - auto-run configured verification commands after edits, feeding failures back for a bounded number of fix rounds. Off by default.
- `--subagents` - offer the `task` tool for delegating explore-only subtasks. Off by default.
- `--no-rules` - skip auto-loading `MONKEY.md` rules and remembered facts into the system prompt.
- `--no-mcp` - skip loading MCP servers configured in Settings > MCP for this invocation.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose`, `--attach-images` - generation controls.
- `--num-ctx`, `--keepalive`, `--think`, `--hidethinking` - Ollama-native options.

Ollama-compatible model commands:

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

Other commands, sharing config and state with the desktop app:

```sh
# Revert a checkpoint's file changes (defaults to the most recent one from this CLI)
monkey revert [checkpoint-id]

# Run the local OpenAI-compatible API server headlessly on 127.0.0.1
monkey api-serve [--port <port>]

# Knowledge Stacks: list/reindex stacks created in Settings > Knowledge
monkey stacks list
monkey stacks reindex <name>
monkey stacks embed-server start|stop ...

# Saved recipes: headless runner with JSON output and CI exit codes
monkey task run <name-or-path> [--param key=value ...] [--json]
monkey task validate <path>
monkey task list
monkey task schedule <name-or-path> --cron "<expr>"   # prints a launchd plist / crontab line, installs nothing
```

Inside the REPL, use `/help` to list slash commands. Supported commands include `/set`, `/show`, `/save <model>`, `/load <model>`, `/revert [checkpoint-id]`, `/persona <slash-command>`, `/prompts`, `/verify`, `/clear`, and `/bye`.

The desktop app installs `monkey` onto your `PATH` automatically the first time it launches — no separate installer step or opt-in checkbox. It's bundled as a Tauri `externalBin` sidecar (built and staged by `pnpm stage:cli` before every `tauri dev`/`tauri build`, per `src-tauri/tauri.conf.json`) and linked/copied onto `PATH` on startup by `src-tauri/src/cli_install.rs`:

- **macOS/Linux**: symlinked as `monkey` into `/usr/local/bin` if already writable (macOS, no prompt), else `~/.local/bin` (created if missing).
- **Windows**: copied to `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe`, with that folder added to your user `PATH` (`HKCU\Environment`, no admin).

It never asks for elevated permissions and never edits your shell rc files — if the chosen directory isn't already on your `PATH` (e.g. a stock macOS shell without `~/.local/bin` added), add it yourself once. This install step is best-effort and silent: a failure never blocks the app from starting.

Developer note: the source tree builds the CLI from `src-tauri/src/bin/monkey-cli/`. Running from a checkout before the app has installed the shim, use the same arguments after Cargo's `--` separator:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --help
cargo run --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --ollama llama3.2 "Summarize this project"
```

## Model Setup

Local GGUF models:

1. Install `llama.cpp` so `llama-server` is available.
2. Open Settings -> Local Models.
3. Download one of the curated models or add an existing `.gguf` file.
4. Start the model. Little Monkey launches `llama-server` with OpenAI-compatible tool calling enabled.

Ollama:

1. Install Ollama.
2. Open Settings -> Ollama.
3. Start Ollama if it is not already running.
4. Pull or import a model, then select it for chat.

Cloud providers:

1. Open Settings -> AI Providers.
2. Add an API key for OpenAI, Anthropic, Google Gemini, OpenRouter, or a custom OpenAI-compatible endpoint.
3. Refresh models and select the provider model to use.

Other Settings tabs:

- **Rules** - view/edit `MONKEY.md` project instructions and remembered facts.
- **MCP** - add stdio or streamable-HTTP MCP servers, set per-server tool allowlists, store bearer tokens in the keychain.
- **Prompts** - manage prompt snippets and personas, import/export (including a Cherry Studio adapter).
- **Knowledge** - create and reindex Knowledge Stacks for RAG.
- **API Server** - toggle the local OpenAI-compatible proxy, mint/revoke scoped tokens, opt into cloud-provider proxying.
- **Automation** - provider failover, context compaction, artifacts, checkpoint retention, and web fetch/search settings.
- **Tasks** - manage and schedule saved recipes.
- **Usage** - token/cost activity.

## Workspace And Safety

Little Monkey only lets agent tools operate inside attached workspace folders. Path resolution canonicalizes requests and rejects traversal outside the workspace, including symlink escapes.

Read-only tools (`read_file`, `list_dir`, `glob`, `grep`, `search_docs`, `web_fetch`, `web_search`, MCP tools marked read-only by their own server) are still routed through the active permission mode. Mutating tools are always permission-gated by default:

- `write_file`
- `edit_file`
- `run_shell`
- `remember`
- MCP tool calls

Permission modes, from most to least restrictive: `manual` (prompt every mutation), `plan` (read-only, propose a plan and get approval before acting), `acceptEdits` (auto-approve file writes, still prompt for shell), `smart` (auto-approve low-risk file writes per an LLM risk judge with a deterministic Rust path floor that can't be overridden), `auto` (auto-approve everything except shell), `bypass` (auto-approve everything, including shell — never used for unattended recipes, see below).

Shell commands run inside the workspace, have a 120 second timeout, and can be cancelled from the chat turn. Every mutating turn is checkpointed so file changes (and optionally the conversation) can be reverted or re-applied later. Scheduled/headless recipes require an explicit `permission_mode` in their YAML and can never set it to `bypass`. API keys and bearer tokens are stored in the OS keychain and are never persisted in plaintext config files.

## Project Layout

- `src/` - React frontend, Zustand stores (`src/store/`), the agent turn loop and shared turn engine (`src/lib/agentLoop.ts`, `src/lib/turnEngine.ts`), model/provider clients, and UI components (chat, settings panels, workspace bar).
- `src-tauri/` - Tauri 2 Rust backend. Notable modules beyond model/provider/workspace/permission/session/git basics: `checkpoints.rs` (rollback), `rules.rs`/`memory.rs` (MONKEY.md + remember), `mcp.rs` (MCP client), `server.rs` (local OpenAI-compatible API server), `web.rs` (SSRF-guarded fetch/search), `prompts.rs` (prompt/persona library), `verify.rs` (post-edit verification), `artifacts.rs` (sandboxed HTML/SVG previews), `stacks.rs` (Knowledge Stacks/RAG), `recipes.rs`/`automations.rs` (scheduled recipes).
- `src-tauri/src/bin/monkey-cli/` - the `monkey`/`monkey-cli` terminal agent: chat/REPL, model management, and CLI parity commands (`checkpoints_cli.rs`, `mcp_cli.rs`, `web_cli.rs`, `verify_cli.rs`, `stacks_cli.rs`, `task.rs`) that reuse the same `little_monkey_lib` code the desktop app calls.
- `docs/roadmap/` - per-feature design docs; [ROADMAP.md](ROADMAP.md) is the milestone-level plan and status.
- `public/` and `src/assets/` - static frontend assets.

## Useful Commands

```sh
pnpm tauri dev       # run the desktop app
pnpm dev             # run Vite only
pnpm build           # typecheck and build frontend
pnpm tauri build     # create app bundle
pnpm test            # run frontend tests
pnpm test:rust       # run Rust tests
```

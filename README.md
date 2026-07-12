# Little Monkey

Little Monkey is a Tauri desktop app for running an agentic AI coding chat against local models, Ollama models, or OpenAI-compatible cloud providers. It combines a React chat UI with a Rust backend that manages model processes, workspace access, permissions, sessions, and tool execution.

## Features

- Chat with a local `llama.cpp` `llama-server`, Ollama, or cloud providers.
- Manage curated GGUF models, external `.gguf` files, Ollama tags, and custom OpenAI-compatible providers.
- Run tool-calling agent turns over an attached workspace: read files, list directories, glob, grep, edit/write files, and run shell commands.
- Gate mutating tools with permission modes and per-action prompts.
- Attach files, folders, and images to prompts; reference workspace paths with `@` mentions.
- Browse the active workspace, preview files, refresh diffs, inspect git status, and commit from the workspace bar.
- Store cloud API keys in the OS keychain through the Rust backend.
- Use automation options for provider failover, vision-aware model switching, context compaction, and rate-limit warnings.
- Switch between light/dark themes and localized UI copy.

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
- `--permission-mode manual|acceptEdits|auto|bypass` - controls terminal prompts for mutating tools.
- `--provider <id> --model <model-id>` - use OpenAI, Anthropic, Gemini, OpenRouter, or a custom provider configured by the app.
- `--ollama <tag>` - use the local Ollama daemon.
- `--local-url <url>` - use a local OpenAI-compatible endpoint.
- `--temperature`, `--top-p`, `--seed`, `--stop`, `--num-predict`, `--system`, `--format`, `--verbose` - generation controls.
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

Inside the REPL, use `/help` to list slash commands. Supported commands include `/set`, `/show`, `/save <model>`, `/load <model>`, `/clear`, and `/bye`.

Developer note: the source tree currently builds the CLI from `src-tauri/src/bin/monkey-cli/`. If you are running from a checkout before an installer has put `monkey` on your `PATH`, use the same arguments after Cargo's `--` separator:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --help
cargo run --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --ollama llama3.2 "Summarize this project"
```

The current Tauri bundle configuration does not yet install a global `monkey` PATH shim by itself; release packaging needs to include that step.

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

## Workspace And Safety

Little Monkey only lets agent tools operate inside attached workspace folders. Path resolution canonicalizes requests and rejects traversal outside the workspace, including symlink escapes.

Read-only tools are available without prompts. Mutating tools are permission-gated:

- `write_file`
- `edit_file`
- `run_shell`

Shell commands run inside the workspace, have a 120 second timeout, and can be cancelled from the chat turn. API keys are stored in the OS keychain and are never persisted in provider config files.

## Project Layout

- `src/` - React frontend, Zustand stores, chat loop, model/provider clients, and UI components.
- `src-tauri/` - Tauri 2 Rust backend, model process management, provider proxying, workspace tools, permissions, sessions, and git/system commands.
- `src-tauri/src/bin/monkey-cli/` - CLI-oriented support code.
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

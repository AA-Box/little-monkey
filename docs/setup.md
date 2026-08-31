# Setup and development

Prerequisites, where Little Monkey keeps its files, how to build and test it,
and where the code lives.

## Prerequisites

- Node.js, `pnpm`, Rust, Cargo, and the Tauri 2 prerequisites for your platform.
- `cmake` and `libclang` — the built-in local Whisper engine compiles `whisper.cpp` through `whisper-rs-sys`, which drives cmake and generates bindings with bindgen. Release bundles ship the pinned Whisper model; source trees stage it with `pnpm stage:whisper` (without it, the app provisions the same pinned, SHA-256-verified model at launch instead).
- Desktop releases include a pinned, checksum-verified `llama.cpp` runtime. Source builds stage the same official runtime before `tauri dev` and `tauri build`; a system `llama-server` is a development fallback only.
- Studio generation (optional): the managed `sd-server` and `llama-tts` runtimes, staged with `pnpm stage:runtime:sd` and `pnpm stage:runtime:tts`. `sd-server` exists for Apple Silicon (Metal), x86_64 Linux (Vulkan), and x86_64 Windows (Vulkan) only. Model weights are yours to supply.
- Ollama runtime (optional): reachable at `http://127.0.0.1:11434` for the explicit Ollama provider or daemon-management commands.
- MLX runtime (optional): supported Apple Silicon; Runtime Hub downloads and verifies the signed package automatically.
- Browser verification (optional): a supported Chromium or Chrome binary.
- GitHub delivery (optional): Git and an authenticated GitHub CLI (`gh`).
- Local OCR, transcription, image generation, IDE extensions, and remote handoff (optional): their configured worker, model, endpoint, SDK, or TLS identity.

On macOS, the unmanaged fallback is `brew install llama.cpp`. Runtime Hub installs the published, signed MLX package automatically on Apple Silicon and keeps the local component registry available for offline or self-hosted catalogs.

## Storage and agent home

Little Monkey separates portable, user-authored agent configuration from managed desktop state:

- `LITTLE_MONKEY_HOME` selects the agent home when set to an absolute path; otherwise it is `~/.littlemonkey`.
- The default profile uses that directory directly. Named profiles use `<agent-home>/profiles/<id>` so rules and hooks retain the same profile isolation as the desktop app.
- Global `MONKEY.md`/`AGENTS.md`, `hooks.json`, recipes, and `monkey` CLI input history use the agent home on new installations. Existing legacy files are discovered automatically and continue working in place, preserving rules history, recipe-relative workspace paths, and binary rollback; no manual copying or path edits are required. New items use the agent home.
- Workspace-authored recipes and managed skills remain under the repository's `.littlemonkey/` directory. Little Monkey also discovers standard read-only skills from `<workspace>/.agents/skills/` and `~/.agents/skills/`, so skills shared with other agents can be used without being copied or rewritten.
- Models, runtimes, sessions, memories, checkpoints, databases, MCP configuration, managed native skills/packages, caches, logs, and other managed data remain in the operating system's application-data locations. Credentials remain in the OS keychain.

The app creates agent-home directories with mode `0700` on Unix and rejects a relative `LITTLE_MONKEY_HOME`, preventing GUI and CLI launches from resolving different directories because their working directories differ.

Desktop and CLI startup perform this setup automatically. Daemon installation records the resolved agent home and profile in its service configuration, so background runs use the same configuration without shell `PATH` or environment setup. Reinstalling a named-profile daemon transactionally upgrades a matching legacy fixed-ID service and restores it if the replacement cannot start.

The resident execution service is runtime infrastructure, not an optional feature: every desktop chat turn executes on it. The desktop app therefore runs `monkey daemon ensure` at each launch, which installs the service if it is missing, republishes and restarts it if its definition or its running build was left behind by a previous app version, starts it if it is stopped, and does nothing if it is already healthy. Nothing has to be installed by hand; when the service cannot be brought up, chat says so and offers Repair, which runs the same command. **Settings → Background Agents** manages the service afterwards — stopping it, concurrency and queue limits, the kill switch, remote handoff.

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

## IDE extension releases

The VS Code extension and JetBrains plugin currently use independent version
1.0.0. Desktop GitHub releases package them as
`little-monkey-vscode-1.0.0.vsix` and `little-monkey-jetbrains-1.0.0.zip`.
Download the assets from the release page and install them through the host
IDE; marketplace publishing is not automatic. Both clients require a local
Little Monkey daemon and `monkey acp`.

To reproduce the release checks locally:

```sh
cd extensions/little-monkey-vscode
npm test
npx --yes @vscode/vsce@3.6.0 package --no-dependencies

cd ../little-monkey-jetbrains
gradle test buildPlugin --no-daemon
```

Opt-in checks that need real local models or hardware:

```sh
pnpm test:compare:live
```

```sh
cd extensions/little-monkey-vscode
LITTLE_MONKEY_COMPLETION_MODEL='your-exact-fim-tag' npm run benchmark:completions
```

## Model setup

0. **From the composer** — the model picker above the composer is searchable across local, Ollama, and provider models and adds a new one inline, so none of the steps below require opening Settings first.
1. **App-owned local model** — **Settings → Local Models → Add custom model**: enter an Ollama tag such as `llama3.2:3b`, a Hugging Face reference such as `hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M`, or a Hugging Face MLX safetensors repository URL (the `/tree/<revision>` form works), review the resolved file, size, license, and digest metadata, then install and start. No Ollama installation required.
2. **User-managed Ollama** — **Settings → Ollama**: confirm the daemon is reachable, pull or import a model, and select it.
3. **Cloud or BYOK** — **Settings → AI Providers**: store the key, refresh the model list, and select a model.
4. **MLX** — **Settings → Runtime Hub → Runtimes**: the signed Apple Silicon runtime is fetched and installed automatically when needed.

Other Settings surfaces: **Security Doctor**, **Companion**, **Portability**, **Knowledge**, **Ecosystem**, **Browser Verification**, **Git Delivery**, **Background Agents**, **MCP**, **Prompts/Skills**, **API Server**, **Tasks**, **Rules**, **Automation**, **Usage**, and **Keyboard Shortcuts**.

## Project layout

- `src/` — React UI, Zustand stores, chat, Compare and Crew flows, the Studio generation section, the workspace sidebar, portability and search, durable run clients, skills and slash commands, and Settings panels.
- `src-tauri/src/` — Rust model and runtime services, managed runtimes and Studio generation, permissions, workspace, run ledger and egress attribution, assets, Knowledge 2.0, packages and workflows, browser, Git delivery, daemon bridge, companion, and Security Doctor, exposed through Tauri commands.
- `src-tauri/src/bin/monkey-cli/` — terminal chat and REPL, ACP, model management, workflows, skills, plugins, security, daemon, remote controller, stacks, tasks, and shared headless tooling.
- `extensions/little-monkey-vscode/`, `extensions/little-monkey-jetbrains/` — thin IDE clients.
- `.github/actions/little-monkey-review/` — reusable PR-review action and its contract test.
- `src-tauri/fixtures/` — deterministic browser and knowledge acceptance fixtures.

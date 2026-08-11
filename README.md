# <img width="50" height="51" alt="LM-logo" src="https://github.com/user-attachments/assets/84651d01-f18b-4c49-b203-8d1b7e8f16b6" /> Little Monkey — a local-first agent runtime and control plane

Little Monkey is a desktop workspace for agentic AI, built on Tauri 2 with a React front end and a Rust backend. It runs against a managed `llama.cpp`, Ollama, MLX on supported Apple Silicon, or any OpenAI-compatible provider you configure, and it generates images, video, and speech locally through its own managed `stable-diffusion.cpp` and `llama-tts` runtimes.

Every surface shares one set of contracts — workspace, permission, run, model, generation, package, browser, Git, and background service — rather than reimplementing them per feature.

Capability claims in this repository describe the current `develop` tree. Where a feature is narrower than its name suggests, the boundary is stated in [Limitations](docs/limitations.md). Work that is not built yet lives in [ROADMAP.md](ROADMAP.md).

## Install

Desktop bundles ship a pinned, checksum-verified `llama.cpp` runtime. From source:

```sh
pnpm install
pnpm tauri dev       # stage llama.cpp + the CLI sidecar, then run the app
```

Node.js, `pnpm`, Rust, Cargo, and your platform's Tauri 2 prerequisites are required. Optional runtimes — Studio generation, Ollama, MLX, browser verification, GitHub delivery — are listed in [Setup](docs/setup.md).

The desktop bundle also installs the `monkey` command on first launch, non-elevated: `/usr/local/bin/monkey` when writable, otherwise `~/.local/bin/monkey`; on Windows `%LOCALAPPDATA%\Programs\monkey-cli\monkey.exe`.

## Quick start

```sh
monkey llama3.2 "Summarize this project"       # chat, model first
monkey pull llama3.2:3b                        # app-owned runtime, no Ollama needed
monkey run hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M
```

In the desktop app, pick a model in **Settings → Local Models**, **Settings → Ollama**, or **Settings → AI Providers**, then chat. The composer takes `@` for workspace paths and `/` for skills.

Full command surface: [CLI](docs/cli.md).

## How it fits together

- **One process table.** Chat turns, daemon jobs, subagents, Crew members, workflow runs and their nodes, remote work, background shells, side tasks, and browser sessions each get a record with a shared id scheme, a parent, one state machine, a declared limit set, and a structured exit — listable with `monkey processes` and signalable from anywhere, including a terminal.
- **One permission boundary.** Six modes from `manual` to `bypass`, a deterministic risk floor on sensitive paths, and a rule that no remote server, model output, webpage, package, or archive can approve its own operation.
- **One egress policy.** Every SSRF guard enforces a named rule; a refusal carries that rule plus its cause, allowed destinations are recorded per run, and a run's `allow_network` is frozen at submission where the provider endpoint is resolved.
- **One kernel-backed shell boundary.** Agent foreground, background, and CLI shells run under macOS Seatbelt, Linux Landlock+seccomp, or a Windows AppContainer plus job object, confined to the selected workspace with a scrubbed environment.
- **Portable agent home.** `LITTLE_MONKEY_HOME` (default `~/.littlemonkey`) holds user-authored rules, hooks, recipes, and CLI history; managed state stays in OS application-data locations and credentials in the OS keychain.

Everything the app can actually do: [Features](docs/features.md).

## Security

Little Monkey canonicalizes workspace paths and rejects traversal and symlink escapes, keeps loopback the default for every served surface, and stores keys in the OS keychain. Inspect a machine's posture with `monkey security audit` or **Settings → Security Doctor**, and the audit trail with `monkey security subsystem-events`, `permission-trail`, `permission-gaps`, `egress-evidence`, and `admission-trail`.

The boundaries, and what they deliberately do not cover: [Workspace and trust boundaries](docs/security.md). Report vulnerabilities through a [private advisory](https://github.com/AA-Box/little-monkey/security/advisories/new), not a public issue — see [SECURITY.md](SECURITY.md).

## Documentation

| To do this | Read |
| --- | --- |
| See what the app does today | [Features](docs/features.md) |
| Drive it from a terminal | [CLI](docs/cli.md) |
| Build, test, or find the code | [Setup and development](docs/setup.md) |
| Understand the trust model | [Workspace and trust boundaries](docs/security.md) |
| Know where a claim stops | [Limitations](docs/limitations.md) |
| Follow the kernel-level plan | [Agent OS roadmap](docs/agent-os-roadmap.md) |
| Connect remote MCP over OAuth | [BYO OAuth clients](docs/byo-oauth-clients.md) |
| Check the conformance suite | [Conformance suite](docs/conformance-suite.md) |

## Development

```sh
pnpm dev             # Vite front end only
pnpm build           # TypeScript check and production front-end build
pnpm tauri build     # desktop bundle containing the managed runtime
pnpm test            # front-end suite
pnpm test:rust       # Rust suite, all targets
```

The full check suite, extension tests, and the opt-in checks that need real models or hardware are in [Setup and development](docs/setup.md).

## Contributing

Bug reports, fixes, and feature proposals are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers development setup, the full check suite, what CI runs per platform, and the invariants a change must hold: honest capability claims, no fabricated runtime values, untrusted content that cannot approve its own operation, and unchanged permission and network boundaries.

Pull requests target `develop`; `main` is the release branch.

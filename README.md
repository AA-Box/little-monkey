<div align="center">

<img width="96" height="98" alt="Little Monkey" src="https://github.com/user-attachments/assets/84651d01-f18b-4c49-b203-8d1b7e8f16b6" />

# Little Monkey

**A local-first agent runtime and control plane.**

Chat, tools, workspace, models, images, video and speech — on your own machine,
behind one permission boundary, with a process table you can actually inspect.

[![CI](https://github.com/AA-Box/little-monkey/actions/workflows/ci.yml/badge.svg?branch=develop)](https://github.com/AA-Box/little-monkey/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/AA-Box/little-monkey?color=6f4e37&label=release)](https://github.com/AA-Box/little-monkey/releases)
[![License](https://img.shields.io/badge/license-MIT-6f4e37)](LICENSE)
[![Platforms](https://img.shields.io/badge/macOS%20%C2%B7%20Windows%20%C2%B7%20Linux-6f4e37)](docs/setup.md)
[![Built with](https://img.shields.io/badge/Tauri%202%20%C2%B7%20Rust%20%C2%B7%20React-6f4e37)](docs/setup.md)

[**Features**](docs/features.md) · [**CLI**](docs/cli.md) · [**Setup**](docs/setup.md) · [**Security**](docs/security.md) · [**Limitations**](docs/limitations.md) · [**Roadmap**](ROADMAP.md)

</div>

---

Little Monkey runs against a managed `llama.cpp`, Ollama, MLX on supported Apple Silicon, or any OpenAI-compatible provider you configure — and generates images, video and speech locally through its own managed `stable-diffusion.cpp` and `llama-tts` runtimes. No account, no relay, no hosted GPU.

Every surface shares one set of contracts — workspace, permission, run, model, generation, package, browser, Git, background service — rather than reimplementing them per feature. Capability claims here describe the current `develop` tree; where a feature is narrower than its name suggests, the boundary is stated in **[Limitations](docs/limitations.md)** rather than left for you to discover.

## How it fits together

```mermaid
flowchart LR
    subgraph S["Surfaces"]
        D["Desktop app"]
        C["monkey CLI"]
        B["Background daemon"]
        R["Remote and mobile"]
    end

    subgraph K["Shared contracts"]
        P["Permission gate"]
        L["Run ledger and process table"]
        E["Per-run egress policy"]
        W["Workspace boundary"]
    end

    subgraph X["Execution"]
        M["llama.cpp · Ollama · MLX"]
        G["sd-server · llama-tts"]
        V["Cloud / BYOK providers"]
    end

    S --> K --> X
```

Nothing reaches a runtime without passing the middle row. That is the whole design: a tool call, a scheduled recipe and a request from a paired phone are the same kind of thing by the time they arrive.

## Install

```sh
pnpm install
pnpm tauri dev       # stage llama.cpp + the CLI sidecar, then run the app
```

Desktop bundles ship a pinned, checksum-verified `llama.cpp` runtime and install the `monkey` command on first launch without elevation. Node.js, `pnpm`, Rust, Cargo and your platform's Tauri 2 prerequisites are required; optional runtimes are listed in **[Setup](docs/setup.md)**.

## Quick start

<details open>
<summary><b>From the terminal</b></summary>

```sh
monkey llama3.2 "Summarize this project"   # chat, model first
monkey pull llama3.2:3b                    # app-owned runtime, no Ollama needed
monkey run hf.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF:Q4_K_M
```

`monkey run` resolves immutable metadata, verifies the model's SHA-256, reads the checksum-bound GGUF's own chat template before advertising tool support, and starts a loopback-only runtime for that session.

</details>

<details>
<summary><b>From the desktop app</b></summary>

Pick a model in **Settings → Local Models**, **Settings → Ollama**, or **Settings → AI Providers**, then chat. In the composer, `@` references workspace paths, `/` invokes skills, and `/btw` asks a side question that never rejoins the conversation.

</details>

<details>
<summary><b>As an API server</b></summary>

```sh
monkey api-serve --port 8080
```

Serves the OpenAI-compatible routes, the Anthropic Messages subset, and native-Ollama `GET /api/tags` and `POST /api/chat` under one authentication, pairing, rate-limit and CORS policy. Loopback is the default; anything else demands an exact interface, TLS identity and allowlist.

</details>

## What it does

| | |
| :-- | :-- |
| **Chat &amp; collaboration** | Compare one frozen prompt across up to four targets, run Crew chats with a coordinator and parallel members, fork sessions, split-pane, search everything |
| **Workspace** | Code review over real git porcelain, acceptance-criteria mapping whose citations are checked against the diff, a real PTY terminal, a tabbed browser pane |
| **Agent tools** | File, shell, memory, web, knowledge, MCP, subagent, plan and verification tools — every one behind the permission gate, with checkpoints you can rewind |
| **Knowledge 2.0** | Ingest files, sites, chats and WebDAV; hybrid lexical and vector retrieval with reranking; inspect the whole pipeline end to end |
| **Runtime hub** | Offload planning from a live hardware snapshot, versioned runtime components, and a Driver Doctor that says what *executes*, not only what is detected |
| **Studio** | Text-to-image, image-to-image, text-to-video, image-to-video and speech, from weights on your own disk |
| **Skills &amp; workflows** | Digest-approved skills, signed packages, MCP Apps, and typed workflow DAGs with triggers, replay and human-approval nodes |
| **Runs &amp; limits** | Ten tracked process kinds, durable stop/suspend/resume from anywhere, and budgets that record *which* limit fired |

Every one of these has its boundary written down: **[full feature list](docs/features.md)** · **[what each stops short of](docs/limitations.md)**

## Security

Workspace paths are canonicalized and traversal and symlink escapes refused. Agent shells run kernel-confined — macOS Seatbelt, Linux Landlock + seccomp, Windows AppContainer + job object — writable only inside the selected workspace, with a scrubbed environment. Loopback is the default for every served surface, keys live in the OS keychain, and no remote server, model output, web page, package or archive can approve its own operation.

The audit trail is queryable, and each command exits non-zero on the condition that is the bug:

```sh
monkey security audit               # posture, without contacting a model
monkey security permission-gaps     # a mutating call with no decision behind it
monkey security subsystem-events    # the hash-chained cross-subsystem stream
monkey security egress-evidence     # allowed destinations beside refused ones
monkey security admission-trail     # the run behind each scheduling decision
```

Boundaries in full: **[docs/security.md](docs/security.md)**. Vulnerabilities go through a [private advisory](https://github.com/AA-Box/little-monkey/security/advisories/new), never a public issue — see [SECURITY.md](SECURITY.md).

## Documentation

| To do this | Read |
| :-- | :-- |
| See what the app does today | [Features](docs/features.md) |
| Drive it from a terminal | [CLI](docs/cli.md) |
| Build, test, or find the code | [Setup and development](docs/setup.md) |
| Understand the trust model | [Workspace and trust boundaries](docs/security.md) |
| Know where a claim stops | [Limitations](docs/limitations.md) |
| Follow the kernel-level plan | [Agent OS roadmap](docs/agent-os-roadmap.md) |
| Connect remote MCP over OAuth | [BYO OAuth clients](docs/byo-oauth-clients.md) |
| Use a paired phone's camera, mic or location | [Paired devices](docs/paired-devices.md) |
| Check the conformance suite | [Conformance suite](docs/conformance-suite.md) |

## Development

```sh
pnpm dev             # Vite front end only
pnpm build           # TypeScript check and production front-end build
pnpm tauri build     # desktop bundle containing the managed runtime
pnpm test            # front-end suite
pnpm test:rust       # Rust suite, all targets
```

Extension tests, the opt-in checks that need real models or hardware, and the project layout are in **[Setup and development](docs/setup.md)**.

## Contributing

Bug reports, fixes and feature proposals are welcome. [CONTRIBUTING.md](CONTRIBUTING.md) covers setup, the full check suite, what CI runs per platform, and the invariants a change must hold: honest capability claims, no fabricated runtime values, untrusted content that cannot approve its own operation, and unchanged permission and network boundaries.

Pull requests target `develop`; `main` is the release branch.

<div align="center">
<sub>MIT licensed · built for people who want the agent on their own machine</sub>
</div>

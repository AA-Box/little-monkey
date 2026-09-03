# Competitor Parity Audit

Audit date: 2026-09-03
Supersedes: the 2026-07-16 revision of this file.

This file has no owning roadmap item today. The item that commissioned it shipped, and `ROADMAP.md` must not accumulate "Done" entries, so the audit is refreshed on demand rather than tracked as future work.

It compares Little Monkey's **actually shipped** capabilities (as documented in `README.md`, `docs/features.md`, and `docs/limitations.md`) against public capability claims for fifteen named competitors, across ten gap categories. It is meant to be re-run periodically, not treated as a one-time snapshot. The 2026-07-16 revision is the evidence for that: three of its findings — no mobile app, no in-app terminal, no desktop control — were falsified by shipped work within seven weeks, and one of those categories (PC control) changed a second time: it went from "no competitor creates pressure" to three competitors documenting it.

## Methodology and honesty rules

- **Little Monkey column**: grounded in `README.md`, `docs/features.md`, and `docs/limitations.md` as they stand on `develop`, plus the docs those files themselves link where a capability lives in one of them. `ROADMAP.md` is consulted only for the linkage line. It holds future work, so an absent item means this audit found no open roadmap commitment for the capability — not that the capability exists. Items marked *(built)* remain in the file for their stated remainder.
- **Competitor columns**: based on public product pages, docs, and press coverage checked on the audit date. Where a claim could not be verified from a primary or reputable secondary source, it is marked **"unclear / not publicly documented"** rather than guessed. Beta and preview status is flagged in the note; where the source gives a date it is recorded, so staleness is visible later.
- **Support levels**: `Yes` (shipped and documented), `Partial` (shipped but narrower/beta/opt-in/third-party), `No` (not offered), `Unclear` (could not verify).
- **Risk** describes what happens to Little Monkey's competitive position if the gap is left open, not a security risk rating.
- **Priority** is the audit's recommendation for roadmap sequencing pressure, not a commitment — actual sequencing stays with `ROADMAP.md`.

One consequence of the second rule is worth stating plainly: most competitors here are scored from a single landing page or README, so several categories read heavily "unclear / not publicly documented" — model comparison worst at 12 of fifteen rows, then PC control at 8, then memory, mobile and governance at 5 each. That marker means the source consulted does not say, not that the vendor lacks the capability.

## Competitors at a glance

| Competitor | Category | One-line description | Primary evidence |
| --- | --- | --- | --- |
| Agent-Zoey / Zoey | Local-first agent framework (Rust) | Alpha-stage agent runtime for Ollama/llama.cpp/LocalAI with a five-kind plugin architecture (Actions, Providers, Evaluators, Functors, Services); framework, not an end-user desktop app. MIT, 24 stars read 2026-09-03 | [github.com/Agent-Zoey/Zoey](https://github.com/Agent-Zoey/Zoey) |
| Hermes Agent | Local-first agent runtime + messaging gateway (Python) | Nous Research's agent with a self-improving skills loop, FTS5 session memory plus Honcho user modelling, seven terminal backends, and a gateway onto six messaging platforms. MIT, 240.1k stars read 2026-09-03 | [github.com/nousresearch/hermes-agent](https://github.com/nousresearch/hermes-agent) |
| OpenClaw | Local-first agent gateway (TypeScript/Node) | Gateway control plane for sessions, tools, events, and channel connections, with companion nodes, a ClawHub skills hub, and a plugin SDK. MIT (OpenClaw Foundation), 389k stars read 2026-09-03 | [github.com/openclaw/openclaw](https://github.com/openclaw/openclaw) |
| Msty (Msty Studio) | Local-first multi-model chat app | Desktop chat hub with Split Chats for side-by-side comparison, versioned Knowledge Stacks, an Agent Mode, and a Go surface offering bounded task agents from desktop or mobile | [msty.ai](https://msty.ai/), [msty.ai/studio/features](https://msty.ai/studio/features) |
| AnythingLLM | Local-first RAG/agent workspace | Self-hostable document chat plus agents and Agent Flows, MCP compatibility, a Model Router, a mobile app, and an "AI Computer use" beta preview | [docs.anythingllm.com](https://docs.anythingllm.com/), [anythingllm.com/mobile](https://anythingllm.com/mobile) |
| Open WebUI | Self-hosted multi-user AI platform | Self-hosted platform over Ollama/OpenAI-compatible backends with tools, pipes, functions and MCP, cross-conversation memory, side-by-side model runs, and roles/groups/per-resource permissions | [openwebui.com](https://openwebui.com/), [docs.openwebui.com/features](https://docs.openwebui.com/features/) |
| Jan | Local-first offline chat app | Offline desktop app for local models with MCP connectors, a local API server, and the self-hosted Tokamak router for model switching | [jan.ai/docs](https://www.jan.ai/docs) |
| LM Studio | Local model runtime + chat UI | Desktop app for discovering and running local models with an OpenAI-compatible server and, since 0.3.17, local and remote MCP server support | [lmstudio.ai/docs/app/mcp](https://lmstudio.ai/docs/app/mcp) |
| Cursor | AI-native code editor | VS Code-derived editor with background and cloud agents, parallel subagents each picking their own model, shell and Design Mode surfaces, MCP connectors, and a native iOS app in public beta | [cursor.com/product](https://cursor.com/product) |
| Devin | Autonomous software engineering agent | Cognition's cloud "autonomous AI software engineer that can write, run and test code", with Shell and Browser workspace tools and Slack/Microsoft Teams entry points | [docs.devin.ai](https://docs.devin.ai/) |
| Replit Agent | Cloud IDE + autonomous builder | Browser-hosted IDE whose Agent 3 "runs on its own for up to 200 minutes", tests and fixes the app it builds in an in-pane browser preview, and can build other agents and automations | [replit.com/blog — Agent 3](https://replit.com/blog/introducing-agent-3-our-most-autonomous-agent-yet) |
| GitHub Copilot (coding agent) | Cloud coding agent | Cloud agent started from the agents panel or an `@copilot` mention, running in an ephemeral GitHub Actions environment, with MCP servers (GitHub and Playwright enabled by default) and a Copilot Memory public preview | [docs.github.com — about coding agent](https://docs.github.com/copilot/concepts/agents/coding-agent/about-coding-agent) |
| OpenAI Codex (CLI, IDE extension, cloud) | Cloud + local coding agent | The non-desktop Codex surfaces — Codex CLI, the Codex IDE extension, Codex cloud, Remote, and ChatGPT on the web | [learn.chatgpt.com/codex](https://learn.chatgpt.com/codex) |
| OpenAI Codex desktop app (in the ChatGPT app) | Desktop coding agent | Codex inside the ChatGPT desktop app; the only surface the documentation maps a feature to by name (browser work in Edge, Brave, Opera, Vivaldi and Chrome "from the ChatGPT desktop app") | [learn.chatgpt.com/codex](https://learn.chatgpt.com/codex) |
| Claude Desktop (Chat / Cowork / Code) | Desktop agent workspace | Anthropic's desktop app with Chat, Cowork, and Code tabs: per-session Git worktrees, a draggable pane layout including terminal and browser, computer use, cloud and SSH environments, and Dispatch from a phone | [code.claude.com/docs/en/desktop](https://code.claude.com/docs/en/desktop) |

Rows 1-8 are the local-first / self-hosted block, 9-13 the hosted and CLI coding agents, 14-15 the desktop-app agents. Rows 13 and 14 are two surfaces of one OpenAI product, kept separate because the desktop app is where the documentation locates the in-app browser and where categories 3 and 10 would otherwise silently merge two different things. The source consulted maps very few features to a named surface, so wherever it does not, both rows carry the same verdict and say so rather than inventing a difference.

Little Monkey itself: a local-first Tauri desktop workspace with a shared Rust/React contract across chat, workflows, background daemon runs, knowledge, an in-app PTY terminal and browser pane, Git delivery, paired remote runners, a paired mobile companion, and scoped desktop control — see `docs/features.md` for the shipped list this audit draws from and `docs/limitations.md` for where each claim stops.

## Gap category matrix

Each section below states what the capability means, how the fifteen competitors currently stack up, Little Monkey's actual current status, the risk of leaving the gap open, a priority recommendation, and the roadmap linkage it maps to: a live roadmap item, an explicit non-goal, or shipped-with-no-open-item.

### 1. Autonomous task orchestration

*Can the product take one instruction and autonomously carry it through a multi-step task — planning, executing, self-testing, and producing a reviewable result — without a human first authoring the exact steps?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | "Multi-step workflow execution" with "Task scheduling with cron support" and conditional branching, but Alpha and developer-facing |
| Hermes Agent | Yes | Isolated subagents for parallel workstreams, a built-in cron scheduler "with delivery to any platform", and seven execution backends including Modal, Daytona, and Vercel Sandbox |
| OpenClaw | Partial | The Gateway is "the local control plane for sessions, tools, events, and channel connections"; long-horizon unattended autonomy is not described in the README (unclear / not publicly documented) |
| Msty | Partial | "Agent Mode — Move past one-off prompting with multi-step, tool-aware execution built for real tasks"; the Go surface offers "Bounded task agents" with "Reviewable execution", explicitly scoped rather than long-horizon |
| AnythingLLM | Partial | AI Agents and Agent Flows are documented; long-horizon self-testing autonomy is not |
| Open WebUI | Partial | "Models maintain structured task lists for multi-step workflows" and agentic retrieval; no first-party long-horizon orchestrator documented |
| Jan | Unclear | Not documented on the docs home |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Yes | Cloud agents and parallel subagents: "Delegate implementation to focus on higher-level direction", "Run cloud agents from your browser or phone" |
| Devin | Yes | "Autonomous AI software engineer that can write, run and test code" |
| Replit Agent | Yes | "Agent 3 runs on its own for up to 200 minutes, handling full tasks autonomously—building, testing and fixing your app" |
| GitHub Copilot (coding agent) | Yes | Starts from the agents panel or an `@copilot` mention and works in "its own ephemeral development environment, powered by GitHub Actions, where it can explore your code, make changes, execute automated tests and linters" |
| OpenAI Codex (CLI, IDE extension, cloud) | Yes | Codex cloud and Remote are documented surfaces; scheduled tasks "can now start when a supported event occurs in Gmail, Slack, or GitHub" (August 2026) |
| OpenAI Codex desktop app | Yes | Same product documentation; the page does not scope autonomy differently by surface |
| Claude Desktop | Yes | Parallel sessions each in their own Git worktree; cloud sessions "run on Anthropic-managed infrastructure by default and continue even if you close the app or shut down your computer"; scheduled recurring work and an Auto permission mode |

**Little Monkey status:** Shipped. `README.md`'s documentation table points at [Autonomous tasks](autonomous-tasks.md) for "a bounded autonomous task with durable evidence", and that file describes the Autonomous Task panel and `monkey task` commands running one objective through a bounded coordinator: the task freezes its model target, workspace roots, permission policy and budgets at creation, the coordinator creates a validated DAG, selects a direct, planned, delegated or parallel-delegated strategy, schedules independent worktree workers, integrates patches serially, runs configured verification, performs a diff review, and records acceptance evidence before delivery. Issue-to-PR runs through the same bounded plan. Around it sit the shipped primitives in `docs/features.md`: typed workflow DAGs with model, agent and subagent, tool, MCP, browser, Git and PR, shell, verify, transform, condition, bounded-loop, human-approval, artifact and output nodes; an explicitly installed `monkey daemon` service running queued immutable recipe and workflow runs with idempotency keys, budgets, approval waits, pause and resume, crash recovery, orphan detection, owned worktrees and a durable global kill switch; persistent cron, filesystem, signed-webhook and GitHub triggers; queueing a selected GitHub review comment as an isolated daemon patch task; the Agent Inbox and Run Dashboard; Run Capsules; side tasks; and Crew. The real remainder is delivery, not planning: `monkey task start` is "queued with a read/write workspace grant and network/external mutations disabled by default; delivery still requires the relevant repository policy and explicit confirmation", "GitHub delivery/merge remain outside the worker behind the existing confirmation workflow", and "A task with no configured verification command remains `WAITING_USER`". One documentation defect belongs here: `docs/features.md` — the file this audit treats as the shipped-feature ground truth — carries no Autonomous Task section at all, so the capability is only discoverable through `README.md`'s link.

**Risk if open:** Medium — the orchestration itself is present and in places stricter than the competition (a run cannot claim success on non-authoritative evidence). What differs is the last mile: competitors default to opening the PR, Little Monkey stops at an explicit confirmation. That is a deliberate boundary, but it is also what a prospective user benchmarks first.

**Priority:** Medium

**Roadmap linkage:** shipped — see `docs/autonomous-tasks.md`; no roadmap item. `docs/features.md` does not describe it, which is the defect this row records.

### 2. Model comparison

*Can users run the same prompt across multiple models/providers and compare results directly?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | Not mentioned in the README; not a chat product in the same sense |
| Hermes Agent | Unclear | Not mentioned in the README |
| OpenClaw | Unclear | Not mentioned in the README |
| Msty | Yes | "Split Chats — Run models side by side to compare tone, depth, and accuracy without losing context" |
| AnythingLLM | Unclear | No side-by-side comparison surface documented on the docs home |
| Open WebUI | Yes | "Run two models side-by-side and compare responses" |
| Jan | Unclear | Not documented on the docs home |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Unclear | Model selection per agent and per subagent is documented; side-by-side comparison is not mentioned on the product page consulted |
| Devin | Unclear | Not documented |
| Replit Agent | Unclear | Not mentioned in the Agent 3 announcement |
| GitHub Copilot (coding agent) | Partial | "Depending on how you start your Copilot cloud agent task, you may be able to select the model used" — selection, not comparison |
| OpenAI Codex (CLI, IDE extension, cloud) | Unclear | Model selection is documented; comparison is not mentioned on the page consulted |
| OpenAI Codex desktop app | Unclear | Same; the page does not scope this by surface |
| Claude Desktop | Unclear | One model per session with per-session selection and `availableModels` restrictions; side-by-side comparison is not mentioned on the page consulted |

**Little Monkey status:** Shipped and comparatively strong. `docs/features.md` describes Compare over two to four explicit targets with independent streaming, stop, retry, timing, usage, persistence and response promotion, defaulting to no tools and keeping target snapshots when global model settings change; Ultracode fans one turn across up to four available models through the same Compare pipeline and runs a synthesis pass; and the workbenches add Model Compare Lab, a Golden Dataset Builder over real model calls, multi-model debate, Trust Scorecards that cite the field each dimension read, and a Workflow and Agent Test Harness whose release-gate suites block a target workflow from starting until a complete passing run of the current suite revision exists. The one boundary: "Release-gate eval state is desktop-local, so CLI and API-server workflow starts are not gated."

**Risk if open:** Low — this is parity-plus. Only Msty and Open WebUI document side-by-side comparison at all, and neither documents scored eval suites.

**Priority:** Low

**Roadmap linkage:** shipped — see `docs/features.md`; no roadmap item.

### 3. Browser/terminal integration

*Is there a first-class in-app terminal and browser surface the agent and user can both use as evidence?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | A CLI/terminal interface and a Web UI/REST API exist; no in-app terminal or browser panel documented |
| Hermes Agent | Partial | "Seven terminal backends — local, Docker, SSH, Singularity, Modal, Daytona, and Vercel Sandbox" are execution backends for agent commands, not a shared in-app terminal surface; no browser surface documented in the README |
| OpenClaw | Partial | Companion nodes expose "voice, Canvas, camera, screen, and device-local actions on supported platforms", and there is a Control UI, CLI and TUI; a first-class in-app terminal or browser is not documented in the README |
| Msty | Unclear | Neither surface is documented on the features page |
| AnythingLLM | Partial | A "Private Browser Tool" for authenticated scraping, a browser extension, and Web Browsing and Web Scraping agent skills are documented; there is no in-app terminal and no user-facing browser pane |
| Open WebUI | Partial | "Live preview of web projects inside Open WebUI" plus in-chat code execution; no terminal panel documented |
| Jan | Unclear | Not documented |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Yes (terminal) / Partial (browser) | "Run shell commands directly from Cursor, from builds to tests to installs"; Design Mode edits a page visually, which is narrower than a general browser pane |
| Devin | Yes | "Shell" and "Browser" are listed as developer tools in the workspace |
| Replit Agent | Yes | Browser-hosted IDE; Agent 3 shows "a browser preview within the Agent pane, showing the Agent's cursor as it clicks around the app" |
| GitHub Copilot (coding agent) | Partial | Runs tests and linters in its ephemeral environment; the page lists no terminal or browser surface for the user, though the Playwright MCP server is enabled by default |
| OpenAI Codex (CLI, IDE extension, cloud) | Yes (terminal) / Unclear (browser) | An integrated terminal is listed under development workflows; the in-app browser is mapped to the desktop app, not to these surfaces |
| OpenAI Codex desktop app | Yes | The one surface-mapped claim on the page: "Use your browser: Work in Edge, Brave, Opera, or Vivaldi as well as Chrome from the ChatGPT desktop app", alongside the integrated terminal |
| Claude Desktop | Yes | "The integrated terminal lets you run commands alongside your session without switching to another app… The terminal is available in local sessions only." The Browser pane is tabbed, "uses a clean browser profile, separate from your personal browser", with per-site approval and safety classifiers on write actions; an iOS Simulator pane is available on macOS |

**Little Monkey status:** Shipped, and this category flipped since 2026-07-16. `docs/features.md` describes a real terminal whose keystrokes go to the PTY through an embedded xterm.js emulator, "so the shell supplies its own prompt, colors, line editing, history, and completions", auto-starting per workspace with dock-right, drag-to-resize and fullscreen; and an in-app tabbed browser pane backed by real child webviews with a tab strip, favicons and loading state, a smart address bar, back/forward/reload, and `window.open` reopened as a tab. Both are among the eight right-sidebar tabs. The separate disposable-Chromium Browser Verification session remains, with exact-origin grants, DNS rechecks, quotas and durable console and network evidence. Boundaries: `docs/features.md` states that in the browser pane "Only `http:`, `https:`, and `about:` load, and remote pages get no Tauri IPC surface"; `docs/limitations.md` adds "The in-app browser pane relies on Tauri's unstable multiwebview API" and "Browser verification uses disposable profiles. Persistent authenticated profiles, file transfer, clipboard, extensions, and general host control are out of scope."

**Risk if open:** Low — Little Monkey is now at or ahead of parity here. The one honest difference is that Claude Desktop's Browser pane lets a user sign in to sites, where Little Monkey's verification sessions deliberately stay disposable.

**Priority:** Low

**Roadmap linkage:** shipped — see `docs/features.md`; no roadmap item. The remaining boundary is the non-goal **"Browser verification stays disposable"**.

### 4. Memory

*Does the product retain durable, inspectable facts across sessions (not just document/RAG retrieval)?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Yes | Vector embeddings with BM25 search over SQLite, optionally PostgreSQL with pgvector, plus recall providers for context assembly |
| Hermes Agent | Yes | "FTS5 session search with LLM summarization for cross-session recall. Honcho dialectic user modeling", plus "Agent-curated memory with periodic nudges" |
| OpenClaw | Unclear | No memory mechanism described in the README |
| Msty | Partial | Versioned Knowledge Stacks are governed document context, not a cross-session personal memory feature |
| AnythingLLM | Yes | "Memories & Personalization": a memories sidebar with workspace (20) and global (5) scopes, manual add, edit, delete and scope moves, plus automatic extraction through an Observer/Reflector pipeline on a configurable schedule (default every three hours) |
| Open WebUI | Yes | "The AI remembers facts about you across conversations" |
| Jan | Unclear | Not documented |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Partial | Checkpoints and snapshot rollback are documented; a cross-session memory feature is not documented on the product page |
| Devin | Unclear | Not documented on the docs home |
| Replit Agent | Unclear | Not mentioned in the Agent 3 announcement |
| GitHub Copilot (coding agent) | Yes | "Copilot Memory" (public preview) lets Copilot "store useful details it has worked out for itself about a repository" |
| OpenAI Codex (CLI, IDE extension, cloud) | Yes | Memories are a documented Codex feature |
| OpenAI Codex desktop app | Yes | Same; the page does not scope this by surface |
| Claude Desktop | Partial | `CLAUDE.md` and `CLAUDE.local.md` project memory shared with the CLI; a separate cross-session personal memory store is not documented on this page |

**Little Monkey status:** `docs/features.md` lists a **memory** agent tool alongside file, shell, web, knowledge, MCP, subagent, plan and verification tools; Knowledge Stacks 2.0 fuses lexical retrieval with vector similarity and optional reranking and lets you inspect retrieval end to end (normalized query, filters, candidates, lexical and vector scores, fused rank, reranker score, exclusions, token budget, final context); and learned skills act as durable procedural memory derived only from a run's own verified events. There is an asymmetry to record plainly: the only mention of Memory Studio anywhere in the three grounding files is `docs/limitations.md`'s boundary sentence — "Memory Studio has two scopes and no pin, merge, or expiry." `docs/features.md` describes no such surface, so this audit states the boundary and does not describe the feature.

**Risk if open:** Medium — the retrieval foundation is strong and better instrumented than most competitors', but the governance layer a user would use to ask "why do you know this?" is documented only by its limits.

**Priority:** Medium

**Roadmap linkage:** shipped — see `docs/features.md`; no roadmap item.

### 5. Model routing

*Can the product automatically or policy-drivenly choose which model/provider handles a request?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | No | Backends are configured; no routing policy documented |
| Hermes Agent | Partial | "Nous Portal, OpenRouter, OpenAI, your own endpoint, and many others"; selection is configuration, not a documented policy engine |
| OpenClaw | Partial | "Works with hosted and local model providers" — the README states no provider count and no routing policy |
| Msty | Partial | Model-agnostic with "Model and feature controls" under team governance; no routing policy engine documented |
| AnythingLLM | Yes | A "Model Router" is documented with overview and setup guides |
| Open WebUI | Partial | Per-message model tagging ("Tag `@gpt-5.6` to draft a plan, then tag `@claude` to critique it") and per-group model restrictions; not a cost/latency policy engine |
| Jan | Partial | Tokamak, a "self-hosted router" where "Jan agents connect here for model switching" |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Partial | "Subagents run in parallel to explore your codebase, with each one using the best model for the task" — vendor-chosen, not user policy |
| Devin | Unclear | Not documented |
| Replit Agent | Unclear | Not mentioned in the Agent 3 announcement |
| GitHub Copilot (coding agent) | Partial | Model selection depending on how the task is started; not policy-driven |
| OpenAI Codex (CLI, IDE extension, cloud) | Partial | Model selection is documented; a routing policy is not |
| OpenAI Codex desktop app | Partial | Same; the page does not scope this by surface |
| Claude Desktop | Partial | Per-session model selection, `availableModels` managed-setting restrictions, and third-party providers — Amazon Bedrock, Google Cloud's Agent Platform, Microsoft Foundry, or a self-hosted gateway — with "Anthropic's API by default", so it is not Anthropic-only; no user-authored routing policy |

**Little Monkey status:** Shipped at the mechanism level. `docs/features.md` describes chat against managed `llama.cpp`, Ollama, MLX or configured cloud/BYOK providers "with capability-aware routing, provider failover, context compaction, usage accounting, and rate-limit warnings", and the Privacy Firewall overrides routing rather than being overridden by it. A user-authored routing-policy surface is described only in `ROADMAP.md`'s first item and appears in none of the three shipped-behaviour files, so — as with Memory Studio and Team Mode — this audit records the boundary and does not describe the feature.

**Risk if open:** Low — capability-aware routing and failover are shipped, and no competitor checked here documents a user-authored routing policy either, so the policy dimension is open ground rather than ground being lost.

**Priority:** Medium

**Roadmap linkage:** `1. Policy-driven model routing` *(partially built)*. Its three named remainders are subagent task classes, routing to managed llama.cpp rather than only Ollama for local-only policies, and recording the decision in the durable run ledger so "why this target" survives a restart.

### 6. Connector ecosystems

*How broad and how easy is the catalog of third-party app/data connectors?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | Five-kind plugin architecture plus Discord and Telegram adaptors; developer-facing, no curated catalog |
| Hermes Agent | Yes | Gateway onto "Telegram, Discord, Slack, WhatsApp, Signal, and CLI" plus Email; 40+ tools; full MCP integration "compatible with the agentskills.io open standard" |
| OpenClaw | Yes | Channels named in the README: WhatsApp, Telegram, Slack, Discord, Google Chat, Signal, iMessage; plus ClawHub skills and a plugin SDK |
| Msty | Unclear | No connector catalog or MCP support documented on the pages consulted |
| AnythingLLM | Yes | "MCP Compatibility" documented for Docker and Desktop, alongside integrations |
| Open WebUI | Yes | "Native Streamable HTTP for Model Context Protocol servers", a modular plugin framework, and in-chat Python tools |
| Jan | Partial | MCP connectors and an extension system; no broad SaaS catalog documented |
| LM Studio | Partial | "Starting 0.3.17 (b10), LM Studio supports both local and remote MCP servers", configured by editing `mcp.json`; no curated OAuth catalog |
| Cursor | Partial | "Connect external tools and data sources like GitHub and Figma directly to Cursor"; broader coverage via MCP |
| Devin | Partial | Slack and Microsoft Teams entry points documented; MCP is not |
| Replit Agent | Yes | "The Agent surfaces a simple UI flow to connect with Notion", and automations reach "Notion, Linear, Dropbox, Sharepoint etc." |
| GitHub Copilot (coding agent) | Yes | Native GitHub depth plus MCP, with "The GitHub MCP server and Playwright MCP server… enabled by default" |
| OpenAI Codex (CLI, IDE extension, cloud) | Yes | Plugins and skills are documented features; GitLab support "is available in beta on all ChatGPT plans" |
| OpenAI Codex desktop app | Yes | Same, plus the surface-mapped "Apple Messages: Find chats, summarize messages… on your Mac" |
| Claude Desktop | Yes | "Connectors… to add integrations like Google Calendar, Slack, GitHub, Linear, Notion, and more", which "are MCP servers with a graphical setup flow", plus skills and plugins from the **+** button |

**Little Monkey status:** Two different things live in this category and they score differently.

The **messaging** side is strong: `docs/limitations.md` names "messaging channels, SMS and calls, paired devices, peers, Talk" as shipped paths reaching an agent from outside the machine and points at [Reaching an agent from outside this machine](messaging-devices-and-phones.md), which `README.md` links from its documentation table; that file states thirteen providers reach the same durable ingress — Telegram, Discord, Slack, WhatsApp, Microsoft Teams, Google Chat, LINE, Matrix, Mattermost, Signal, iMessage, IRC, and SMS — with one gate deciding who may talk to an account and one router deciding what recipe runs. That is broader than any competitor's channel list found in this refresh. `docs/features.md` carries no roster of them.

The **app/data connector** side is the gap. `docs/features.md` describes MCP with remote OAuth metadata and tokens, BYO OAuth clients, preserved structured content, routed tools that never bypass allowlists, and MCP Apps in an opaque-origin window; signed declarative packages with install and update permission previews, pins, rollback, revocation and an offline cache; signed WASM extensions from verified M4 registry snapshots with a full `monkey extensions` developer loop; one **Discover** catalog; and "a signed first-party catalog of six skills (review, testing, documentation, browser QA, release preparation, knowledge workflows) plus declarative GitHub, GitLab, WebDAV, and REST/webhook connector packages". `docs/limitations.md` now bounds it as: "The connector catalog holds seventeen providers: eleven connect over authorization-code OAuth with an app you register yourself (no client credentials ship in this binary), three take a pasted token, one rides the `gh` CLI, one takes S3/R2 access keys, and one is extension-backed." The size disagreement this audit reported on 2026-09-03 — four named packages against "5 of roughly 17 providers" — is resolved: the catalog and the declarative connector packages are separate surfaces, and both files now say so. The boundary that replaces it is narrower and different in kind: an OAuth-connected account proves identity and records granted scopes, and no feature reads provider data through it — knowledge indexing and inbox triage still source only GitHub, Slack, Notion, Jira and S3. Also in force: "Inbox triage is read-only with no rules engine", and the non-goals **"No Gmail/Outlook inbox integration. Inbox triage covers Slack, Jira, and GitHub read-only."** and **"No Google Drive knowledge connector."**

**Risk if open:** High — connector breadth is a recurring adoption blocker for knowledge-worker use cases, and the named work-app catalogs sit with the hosted competitors: Claude Desktop names Google Calendar, Slack, GitHub, Linear and Notion behind a graphical setup flow, Codex names Gmail, Slack and GitHub event triggers plus Apple Messages and Linear, and Replit surfaces a UI flow onto Notion, Linear, Dropbox and SharePoint. Copilot ships the GitHub and Playwright MCP servers by default, and Open WebUI and AnythingLLM document MCP support generically with no named work-app catalog — so the rating rests on those three named catalogs, not on all six. Little Monkey's own catalog now reaches Notion (pasted token) and, over OAuth, Drive, Linear, Dropbox and SharePoint/OneDrive — but as connected accounts only, with no feature reading their data yet, and Calendar is not covered at all.

**Priority:** High

**Roadmap linkage:** the non-goals **"No Gmail/Outlook inbox integration"** and **"No Google Drive knowledge connector"** close part of this deliberately; the rest is shipped — see `docs/features.md` — with no roadmap item.

### 7. Remote execution

*Can work run on hardware other than the user's local machine?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | Self-hosted deployment with a REST API; no remote-execution product documented |
| Hermes Agent | Yes | The strongest local-first showing here: local, Docker, SSH, Singularity, Modal, Daytona, and Vercel Sandbox backends |
| OpenClaw | Partial | The Gateway runs on a laptop or as a shared team deployment, where "configuration is the only difference"; "Tools run on the host for the main session unless you configure sandboxing" |
| Msty | Unclear | Remote execution is not documented on the pages consulted |
| AnythingLLM | Partial | Self-hosted server deployment, with the mobile app syncing "across devices, all on your local network" |
| Open WebUI | Yes | "Run locally, in the cloud, or both", deployable on Docker, Kubernetes, pip, or bare metal |
| Jan | Partial | A local API server and the self-hosted Tokamak router; no remote execution of the app itself documented |
| LM Studio | Partial | Remote MCP *servers* are supported; remote execution of the app is not documented |
| Cursor | Yes | "Run cloud agents from your browser or phone" |
| Devin | Yes | Cloud-native: "cloud Devin" and the web app at app.devin.ai |
| Replit Agent | Yes | Fully browser-hosted by default |
| GitHub Copilot (coding agent) | Yes | Runs in an ephemeral environment "powered by GitHub Actions" |
| OpenAI Codex (CLI, IDE extension, cloud) | Yes | Codex cloud and Remote are documented surfaces |
| OpenAI Codex desktop app | Yes | Same; the page does not scope this by surface |
| Claude Desktop | Yes | Cloud sessions that "continue even if you close the app or shut down your computer", SSH sessions, and WSL on Windows |

**Little Monkey status:** Shipped, and deliberately shaped differently from the hosted competitors. `docs/features.md` describes configuring and probing local, Docker, paired Little Monkey and SSH-backed runner targets with target identity and capabilities frozen into each placed run; bounded, content-addressed transfer of clean-Git, dirty-Git and non-Git workspaces with executor-owned workspaces materialized and reviewable diffs, artifacts and verification evidence retrieved; and pairing "a user-owned remote runner over direct, Tailscale, or SSH-forwarded HTTPS with pinned TLS, mutually scoped credentials, rotation and revocation, replay protection, and audit history", where a controller may view events, inspect bounded artifacts, approve digest-bound requests, cancel runs or engage the kill switch "only when its invitation grants that exact action", and where "Inference, tools, workspaces, and provider keys stay on the runner; Little Monkey operates no relay." `docs/limitations.md` states the boundary: "Remote handoff requires a user-owned reachable network and valid TLS identity. There is no relay, account service, RBAC/SSO plane, or hosted GPU."

**Risk if open:** Medium — the trade-off is intentional and already a non-goal, but a user without a spare machine still gets none of the "close the laptop, the cloud keeps working" experience Cursor, Devin, Replit, Copilot, Codex and Claude Desktop all offer out of the box.

**Priority:** Medium

**Roadmap linkage:** the non-goal **"No hosted Little Monkey service — no relay, account service, hosted GPU, or RBAC/SSO plane. Remote access is user-owned infrastructure only."**

### 8. Mobile

*Is there a mobile app or mobile-equivalent surface?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | No | Framework/library; no mobile client documented |
| Hermes Agent | Partial | Reachable from a phone through the Telegram, WhatsApp, Signal, Slack and Discord gateway; no first-party mobile app documented |
| OpenClaw | Partial | "Companion apps and nodes" adding "voice, Canvas, camera, screen, and device-local actions on supported platforms" — the README names no mobile OS, so the platform list is unclear / not publicly documented |
| Msty | Yes | Go lets you "run scoped agents on real tasks, review each step, and keep control from desktop or mobile" |
| AnythingLLM | Yes | "A full AI assistant in your pocket — running on-device with no cloud, no API keys, and no limits", syncing "chats, threads, tools, and even run prompts across devices, all on your local network" |
| Open WebUI | Unclear | No mobile app or PWA claim on the pages consulted |
| Jan | Unclear | Not documented |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Yes | "Cursor is available as a native iOS app on your phone, now in public beta" |
| Devin | Unclear | Web app plus Slack and Teams entry points; no native mobile app documented |
| Replit Agent | Partial | Browser-hosted by default; "live monitoring on your phone" is documented, a native mobile app is not |
| GitHub Copilot (coding agent) | Unclear | The page consulted makes no mention of GitHub Mobile |
| OpenAI Codex (CLI, IDE extension, cloud) | Partial | Reachable through the ChatGPT surfaces; the page does not document a mobile Codex surface explicitly |
| OpenAI Codex desktop app | Partial | Same |
| Claude Desktop | Yes | Dispatch sends a task from a phone and spawns a Code session ("Dispatch requires a Pro or Max plan and is not available on Team or Enterprise plans"); cloud sessions are monitorable "from claude.ai/code or the Claude mobile app" |

**Little Monkey status:** Shipped, and this category flipped since 2026-07-16. `docs/features.md` describes pairing the iOS and Android app ([little-monkey-mobile](https://github.com/AA-Box/little-monkey-mobile), React Native and Expo) to a desktop or homelab node with a versioned invitation, where "Requests are sequence-numbered and signed, and the client requires the invitation's pinned TLS fingerprint unless a trusted-LAN development override is visibly enabled". From the phone you browse runs, event timelines, pending approvals and verified artifacts, and approve an exact operation digest, cancel a run or engage the kill switch, "each only when the pairing grant contains that capability". Chat sessions, saved-workflow launch, capture upload and device self-revocation run over the node's versioned `/v1/remote/mobile/*` extension, with chat turns executing through an operator-authored `mobile-chat` recipe "so the node stays authoritative for models, prompts, and permission mode". Talk runs from the phone over a dedicated authenticated WebSocket gated on `voice_stream`, opened with a one-use thirty-second ticket, foreground only. Captures queue offline, bounded, base64-encoded and SHA-256 verified on both sides. The boundary, in `docs/limitations.md`'s words: the companion "pairs by QR code or pasted invitation, browses, approves, chats, launches saved workflows, and uploads captures, but browsing is online-only and push delivery needs an operator-selected provider. Physical-device, signing, and store-submission gates are unmet."

**Risk if open:** Medium — the oversight surface competitors ship is present, and Little Monkey's is capability-scoped in a way theirs are not. What is missing is distribution: an app nobody can install from a store reaches fewer people than one they can.

**Priority:** Medium

**Roadmap linkage:** `5. Mobile companion — remaining gaps` *(partially built)*, whose four remainders are offline browsing (captures queue offline today, browsing is online-only), push delivery through an operator-selected provider and a node-side notification bridge, a QR pairing payload short enough to scan (the invitation embeds the full server certificate PEM, which exceeds practical QR capacity), and store release.

### 9. Governance

*Multi-user roles, org policy, and audit trails beyond a single user's local approvals.*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | No governance layer documented |
| Hermes Agent | Partial | Command approval and a command allowlist; no multi-user RBAC documented |
| OpenClaw | Partial | Shared team deployment where "configuration is the only difference"; tools run on the host by default; no RBAC plane documented |
| Msty | Yes | "Team controls — Add SSO, role-based access, model and feature controls, and administrative audit logs when you need shared governance" |
| AnythingLLM | Partial | Multi-user self-hosted server mode is documented; specific role names are not on the docs home |
| Open WebUI | Yes | "Roles, groups, and per-resource permissions", "Restrict models to specific users or groups", and "SSO, RBAC, audit logs, data residency, and on-premises or air-gapped deployment" |
| Jan | Partial | Tokamak is "Self-hosted router, fusion model, and governance/audit"; no roles, RBAC model, or audit-export detail is documented on the docs home |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Unclear | A Teams product section exists; RBAC and audit specifics are not on the product page |
| Devin | Unclear | Not documented on the docs home |
| Replit Agent | Unclear | Not mentioned in the Agent 3 announcement |
| GitHub Copilot (coding agent) | Yes | "If you have a GitHub Copilot Business or GitHub Copilot Enterprise subscription, an administrator must enable the relevant policy", on top of GitHub's own org RBAC |
| OpenAI Codex (CLI, IDE extension, cloud) | Partial | Administrator plugin controls are documented; RBAC and audit export are not on the page consulted |
| OpenAI Codex desktop app | Partial | Same |
| Claude Desktop | Yes | Managed settings including `disableAutoMode`, `availableModels` and administrator-distributed `sshConfigs`; enterprise admins "can restrict which permission modes are available", and on Team and Enterprise plans "organization policy" controls bypass mode |

**Little Monkey status:** Strong for one desktop user, closed above that by design. `docs/features.md` describes six permission modes (`manual`, `plan`, `acceptEdits`, `smart`, `auto`, `bypass`) with a deterministic risk floor on sensitive paths, shell behind the stronger policy and `bypass` refused to unattended recipes; checkpoints with revert, re-apply, rewind, read-only comparison of any two, and a rollback simulation that marks effects that cannot be safely undone `needs_reconciliation`; per-rule egress policy where every denial carries its named rule and allowed destinations are recorded per run; Security Doctor plus `monkey security audit`, `permission-gaps`, `subsystem-events`, `egress-evidence` and `admission-trail`; and per-machine identity separation in **Settings → Profiles**, which is explicitly "Local isolation only — no account service, no sign-in, nothing leaves the device". Above one user, `docs/limitations.md` states the boundaries: "Team Mode's RBAC is enforced at one defined point, and its audit trail attributes the exporter rather than the approver", and "Approval chains are sequential and answered by the same desktop user." As with Memory Studio, `docs/features.md` describes no Team Mode surface, so this audit states the boundary without describing the feature.

**Risk if open:** Medium — low urgency for the individual and homelab audience, a real blocker for team deployments, and Msty, Open WebUI, GitHub Copilot and Claude Desktop all clear the bar.

**Priority:** Medium

**Roadmap linkage:** the non-goal **"No hosted Little Monkey service — no relay, account service, hosted GPU, or RBAC/SSO plane"** closes the multi-user plane deliberately. This is a positioning decision to restate, not a backlog item.

### 10. PC control

*Can the agent observe and control the user's desktop (mouse/keyboard/screen) beyond a sandboxed browser or shell tool?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | Not documented |
| Hermes Agent | Unclear | Not a documented product feature; a community Linux desktop-control MCP server is linked, which is third-party |
| OpenClaw | Partial | Companion nodes expose "screen, and device-local actions on supported platforms"; general mouse and keyboard control of a host is not documented in the README |
| Msty | Unclear | Not documented on the pages consulted |
| AnythingLLM | Partial | "AI Computer use" is listed under beta preview features on the docs home; scope and platforms are not detailed there |
| Open WebUI | Unclear | A product called "Computer" is linked from the landing page with no detail there |
| Jan | Unclear | Not documented |
| LM Studio | Unclear | Not mentioned on the MCP page consulted |
| Cursor | Unclear | Editor, shell and Design Mode surfaces are documented; desktop control is not mentioned on the product page consulted |
| Devin | Unclear | Shell and Browser workspace tools are documented; desktop control is not mentioned on the docs home |
| Replit Agent | No | Browser-hosted IDE with no access to the local host, so desktop control is outside the product's architecture rather than merely undocumented |
| GitHub Copilot (coding agent) | No | Runs in an ephemeral GitHub Actions environment with no access to the user's machine, so desktop control is architecturally out of scope |
| OpenAI Codex (CLI, IDE extension, cloud) | Partial | Computer use is a documented Codex feature; the page maps it to no surface and states no platform, plan or regional limits — unclear / not publicly documented |
| OpenAI Codex desktop app | Partial | Same; the surface split for computer use is unclear / not publicly documented |
| Claude Desktop | Yes | "Computer use lets Claude open your apps, control your screen, and work directly on your machine the way you would." A research preview on macOS and Windows requiring a Pro or Max plan, "not available on Team or Enterprise plans", off by default, needing Accessibility and Screen Recording on macOS, with per-action checks, prompt-injection flagging of on-screen content, and per-app tiers capping browsers at view-only and terminals and IDEs at click-only |

**Little Monkey status:** Shipped, and this category flipped twice — Little Monkey now has it, and it is no longer uncontested. `docs/features.md` describes granting a paired controller a scoped **Control Desktop** action: real mouse and keyboard input on macOS, Windows and Linux — "X11 directly, Wayland through the compositor's own xdg-desktop-portal RemoteDesktop consent, never a compositor bypass; a desktop without those portals fails closed" — where every action is gated by local consent, per-action by default or batch "only when the remote request and local operator agree", with a cross-process session lock preventing app and daemon from driving input at once, periodic screenshots recorded to the run ledger, and force-stop on device revocation or the kill switch. Separately, model-facing **Computer Use** for native applications runs behind an explicit, expiring application/window grant with "semantic accessibility inspection first, bounded screenshots and coordinates, frontmost/stale-target revalidation, per-action approval, sensitive-target refusal, redacted audit records, verification-aware outcomes, and a persistent pause/stop/emergency indicator", routing browser work to the existing browser worker and terminal work to `run_shell`. `docs/limitations.md` states where it stops: Computer Use "requires a live local grant, OS accessibility/screen-recording permissions, a visible target, and a frontmost revalidation; it refuses password managers, authentication/security surfaces, hidden password fields, OS permission dialogs, and sensitive targets", Linux AT-SPI availability varies and "a desktop without those portals stays fail-closed, and there is no XWayland or uinput fallback"; and "Control Desktop keeps no local audit log or screenshots on the desktop side (the daemon-hosted remote path records them to the run ledger), does not block sensitive system dialogs, and matches its allowlist by application identity rather than verifying the frontmost window. The Windows and Linux/X11 input backends compile and their pure helper logic is tested, but neither has had a full runtime pass on real hardware — that remains a release gate."

**Risk if open:** Medium, up from Low. The 2026-07-16 claim that no named competitor documents desktop control is dead: Claude Desktop documents it in detail, both Codex rows list computer use as a feature, AnythingLLM lists an "AI Computer use" beta preview, and OpenClaw's companion nodes reach a device's screen. The risk is no longer absence of the capability but the unfinished release gate on two of the three input backends.

**Priority:** Medium — framed as closing the real-hardware release gate on the Windows and Linux/X11 backends, not as adding capability.

**Roadmap linkage:** shipped — see `docs/features.md`; no roadmap item.

## Master gap ledger

Compact view of the ten categories: Little Monkey's current status, risk of leaving the gap open, recommended priority, and the roadmap linkage: a live roadmap item, an explicit non-goal, or shipped-with-no-open-item.

| # | Gap category | Little Monkey status today | Risk if open | Priority | Roadmap item(s) / non-goal |
| --- | --- | --- | --- | --- | --- |
| 1 | Autonomous task orchestration | Shipped: bounded coordinator (`monkey task`, Autonomous Task panel) with validated DAG, worktree workers, verification and acceptance evidence, on top of workflow DAGs, the daemon and triggers; remaining: delivery still needs explicit confirmation, a task with no verification command stays `WAITING_USER`, and `docs/features.md` documents none of it | Medium | Medium | shipped — see docs/autonomous-tasks.md; no roadmap item. docs/features.md does not describe it, which is the defect this row records |
| 2 | Model comparison | Shipped: Compare over two to four targets, Ultracode fan-out, Model Compare Lab, Golden Dataset Builder, debate, Trust Scorecards, release-gate eval suites; remaining: release-gate state is desktop-local so CLI and API-server starts are not gated | Low | Low | shipped — see docs/features.md; no roadmap item |
| 3 | Browser/terminal integration | Shipped: xterm.js PTY terminal tab and tabbed webview browser pane among the eight sidebar tabs, plus disposable-Chromium Browser Verification; remaining: the pane rides Tauri's unstable multiwebview API and verification profiles stay disposable | Low | Low | non-goal "Browser verification stays disposable"; otherwise shipped — see docs/features.md; no roadmap item |
| 4 | Memory | Shipped: `memory` agent tool, Knowledge Stacks 2.0 with end-to-end retrieval inspection, learned skills as procedural memory; remaining: Memory Studio "has two scopes and no pin, merge, or expiry" and appears in no feature documentation | Medium | Medium | shipped — see docs/features.md for the memory tool and Knowledge Stacks; Memory Studio itself is documented only by its limits |
| 5 | Model routing | Shipped: capability-aware routing and provider failover under the Privacy Firewall; remaining: the user-authored routing-policy surface is described only in `ROADMAP.md` (subagent task classes, managed llama.cpp for local-only policies, and the decision in the durable run ledger) | Low | Medium | `1. Policy-driven model routing` *(partially built)* |
| 6 | Connector ecosystems | Shipped: MCP with OAuth and BYO clients, signed packages, WASM extensions, one Discover catalog, six first-party skills, thirteen messaging providers on one ingress, and a seventeen-provider app/data catalog (eleven over BYO-client OAuth, including Drive, SharePoint/OneDrive, Linear and Dropbox); remaining: a connected account is identity plus granted scopes only — nothing reads provider data through it — and Calendar is not covered | Medium | High | non-goals "No Gmail/Outlook inbox integration" and "No Google Drive knowledge connector"; otherwise shipped — see docs/features.md; no roadmap item |
| 7 | Remote execution | Shipped: local, Docker, paired-node and SSH execution targets with frozen identity, bounded workspace transfer, and a user-owned paired runner over pinned TLS with scoped grants; remaining: no relay, account service, RBAC/SSO plane or hosted GPU, by design | Medium | Medium | non-goal "No hosted Little Monkey service — no relay, account service, hosted GPU, or RBAC/SSO plane. Remote access is user-owned infrastructure only." |
| 8 | Mobile | Shipped: paired iOS/Android companion with pinned-TLS invitation, run and approval browsing, digest approval, cancel and kill switch, chat, workflow launch, phone Talk and offline capture queueing; remaining: online-only browsing, operator-selected push, and unmet device, signing and store gates | Medium | Medium | `5. Mobile companion — remaining gaps` *(partially built)* |
| 9 | Governance | Shipped: six permission modes with a risk floor, checkpoints and rollback simulation, per-rule egress evidence, Security Doctor, and per-machine Profiles; remaining: Team Mode's RBAC "is enforced at one defined point, and its audit trail attributes the exporter rather than the approver", and approval chains are answered by one desktop user | Medium | Medium | non-goal "No hosted Little Monkey service — no relay, account service, hosted GPU, or RBAC/SSO plane." |
| 10 | PC control | Shipped: scoped Control Desktop for a paired controller on macOS, Windows and Linux, plus model-facing Computer Use behind an expiring grant with frontmost revalidation and sensitive-target refusal; remaining: the Windows and Linux/X11 input backends have had no full runtime pass on real hardware, which "remains a release gate" | Medium | Medium | shipped — see docs/features.md; no roadmap item |

## Biggest gaps (summary)

In priority order, the audit's three highest-confidence, highest-priority findings:

1. **Connector depth, not breadth, is what now trails the leaders.** The only survivor of the 2026-07-16 top three, and the one the connector-catalog OAuth work reshaped rather than closed. Claude Desktop names Calendar, Slack, GitHub, Linear and Notion behind a graphical setup flow, Codex names Gmail, Slack, GitHub, Apple Messages and Linear, and Replit names Notion, Linear, Dropbox and SharePoint. Little Monkey's own app/data catalog now names Notion, Drive, Linear, Dropbox and SharePoint/OneDrive among seventeen providers, so the breadth half of this gap has largely closed; what remains is depth — a connected account proves identity and records granted scopes, and no feature reads provider data through it — plus Calendar, which is not covered. (Copilot ships two MCP servers by default, and Open WebUI and AnythingLLM document MCP generically without a named work-app catalog, so this rests on three named catalogs, not six.) The documentation defect this refresh filed — `docs/features.md` naming four connector packages against `docs/limitations.md`'s "5 of roughly 17 providers" — is resolved: they were two different surfaces, and both files now say which is which. Note the asymmetry in the other direction too: thirteen messaging providers reach one durable ingress, more than any competitor's channel list found here, and `docs/features.md` does not say so either.
2. **The autonomous run stops one step short of delivery.** The coordinator, the worktree workers, the verification and the acceptance evidence are all shipped and in places stricter than the competition. What differs is the last mile: `monkey task start` runs "with network/external mutations disabled by default; delivery still requires the relevant repository policy and explicit confirmation", and GitHub delivery and merge stay outside the worker. Competitors default to opening the PR. That is a defensible boundary rather than a missing feature — but a reader of `docs/features.md` cannot find the capability at all, which is the part worth fixing.
3. **Governance stops at one desktop user by design.** Team Mode's RBAC is enforced at one defined point and its audit trail attributes the exporter rather than the approver; approval chains are sequential and answered by the same desktop user; and the multi-user plane is closed by an explicit non-goal. This is a positioning decision to restate deliberately, not a backlog item — but it is the reason four competitors clear a bar Little Monkey does not intend to clear.

Where Little Monkey is at or ahead of parity today: **model comparison** (two-to-four-target Compare, Ultracode fan-out, and release-gate eval suites, against two competitors documenting side-by-side chat and none documenting scored suites); **the in-app terminal and browser pane** (a real PTY and a tabbed webview pane, now level with Claude Desktop and the Codex desktop app); **the user-owned remote-runner model** (scoped invitations, pinned TLS, no relay); and **the Computer Use safety envelope** — an expiring application/window grant, frontmost revalidation, sensitive-target refusal, and screenshots recorded to the run ledger — which is narrower and better documented than any competitor's screen control found in this refresh.

Direction of travel since 2026-07-16 matters more than any single verdict here. Three of that revision's findings — no mobile app, no in-app terminal, no desktop control — were falsified by shipped work within seven weeks. One category went the other way: PC control moved from "no named competitor creates pressure" to three competitors documenting it plus a fourth exposing device screens. A parity audit that is re-run twice a year would have been wrong about three of ten categories — one of them for two independent reasons — for months at a time; the cadence is the finding.

This audit proposes no `ROADMAP.md` change. The highest-priority gaps either map to a live partially-built item or fall inside a stated non-goal, and the work that shipped since the last revision correctly carries no roadmap item at all under that file's no-Done-items rule. What it does propose is documentation work in the files it is grounded in: `docs/features.md` describes no Autonomous Task, Memory Studio, Team Mode, or routing-policy surface and carries no messaging-channel roster, and it disagrees with `docs/limitations.md` about the connector catalog's size. Each of those needs a source-of-truth decision from a code owner rather than an edit from this file.

## Sources consulted

- `README.md`, `docs/features.md`, and `docs/limitations.md` (this repository) — Little Monkey's shipped-capability ground truth, plus `docs/autonomous-tasks.md` and `docs/messaging-devices-and-phones.md` where `README.md`'s documentation table delegates to them. `ROADMAP.md` is used only for the roadmap-linkage line.
- [github.com/Agent-Zoey/Zoey](https://github.com/Agent-Zoey/Zoey) — Zoey framework README.
- [github.com/nousresearch/hermes-agent](https://github.com/nousresearch/hermes-agent) — Hermes Agent README.
- [github.com/openclaw/openclaw](https://github.com/openclaw/openclaw) — OpenClaw README.
- [msty.ai](https://msty.ai/) and [msty.ai/studio/features](https://msty.ai/studio/features).
- [docs.anythingllm.com](https://docs.anythingllm.com/) and [anythingllm.com/mobile](https://anythingllm.com/mobile).
- [openwebui.com](https://openwebui.com/) and [docs.openwebui.com/features](https://docs.openwebui.com/features/).
- [jan.ai/docs](https://www.jan.ai/docs).
- [lmstudio.ai/docs/app/mcp](https://lmstudio.ai/docs/app/mcp).
- [cursor.com/product](https://cursor.com/product).
- [docs.devin.ai](https://docs.devin.ai/) — Devin's own documentation. The 2026 Cognizant partnership announcement cited by the previous revision is a commercial announcement, not a capability source, and is not used here.
- [replit.com/blog — "Introducing Agent 3: Our Most Autonomous Agent Yet"](https://replit.com/blog/introducing-agent-3-our-most-autonomous-agent-yet).
- [docs.github.com — About GitHub Copilot coding agent](https://docs.github.com/copilot/concepts/agents/coding-agent/about-coding-agent).
- [learn.chatgpt.com/codex](https://learn.chatgpt.com/codex) and [learn.chatgpt.com/docs](https://learn.chatgpt.com/docs) — both OpenAI Codex rows. `https://openai.com/codex/` returns HTTP 403 to automated fetch as of 2026-09-03 and is therefore not a source here.
- [code.claude.com/docs/en/desktop](https://code.claude.com/docs/en/desktop) — Claude Desktop, Code tab reference.

Every URL above was fetched on 2026-09-03. Where a claim could not be traced to one of the sources above, it is marked "unclear / not publicly documented" in the tables rather than asserted.

# Competitor Parity Audit

Audit date: 2026-07-16
Owner surface: `ROADMAP.md` → Phase 1 → "Agent-Zoey / Competitor Parity Audit"

This document is the checked-in comparison matrix required by that roadmap item. It compares Little Monkey's **actually shipped** capabilities (as documented in `README.md`) against public capability claims for eleven named competitors, across the ten gap categories the roadmap item calls out. It is meant to be re-run periodically, not treated as a one-time snapshot — competitor capability claims move fast (several changed materially in the first half of 2026) and this file will drift out of date if it isn't refreshed.

## Methodology and honesty rules

- **Little Monkey column**: grounded only in `README.md` (shipped features) and `ROADMAP.md` (planned/research items). Nothing here is invented; where a capability is roadmap-only, it is marked as such, not counted as shipped.
- **Competitor columns**: based on public product pages, docs, and press coverage checked on the audit date. Where a claim could not be verified from a primary or reputable secondary source, it is marked **"unclear / not publicly documented"** rather than guessed. Fast-moving vendor claims (private beta features, changelog entries) are noted with their approximate date so staleness is visible later.
- **Support levels**: `Yes` (shipped and documented), `Partial` (shipped but narrower/beta/opt-in/third-party), `No` (not offered), `Unclear` (could not verify).
- **Risk** describes what happens to Little Monkey's competitive position if the gap is left open, not a security risk rating.
- **Priority** is the audit's recommendation for roadmap sequencing pressure, not a commitment — actual sequencing stays with `ROADMAP.md`.

## Competitors at a glance

| Competitor | Category | One-line description | Primary evidence |
| --- | --- | --- | --- |
| Agent-Zoey / Zoey | Local-first agent framework (Rust) | Alpha-stage, privacy-first agent runtime for Ollama/llama.cpp/LocalAI with a plugin (Actions/Providers/Evaluators/Services) architecture; framework/library, not a polished end-user desktop app | [github.com/Agent-Zoey/Zoey](https://github.com/Agent-Zoey/Zoey) |
| Msty (Msty Studio) | Local-first multi-model chat app | Desktop chat hub over Ollama/llama.cpp/MLX and cloud providers, with knowledge stacks, split/branching chat compare, and a beta autonomous task runner ("Msty Claw") | [msty.ai](https://msty.ai/), [msty.ai/studio/features](https://msty.ai/studio/features) |
| AnythingLLM | Local-first RAG/agent workspace | Self-hostable document chat + agents product with MCP support, multi-user server mode, and a mobile app that runs agents/web search on-device | [docs.anythingllm.com](https://docs.anythingllm.com/), [anythingllm.com/mobile](https://anythingllm.com/mobile) |
| Open WebUI | Self-hosted multi-user AI platform | Self-hosted web UI over Ollama/OpenAI-compatible backends with Tools/Pipes/MCP/MCPO connector framework, per-user/group access control, and a native desktop app | [openwebui.com](https://openwebui.com/), [docs.openwebui.com/features](https://docs.openwebui.com/features/) |
| Jan | Local-first offline chat app | Simple offline ChatGPT-style desktop app for local GGUF models plus an OpenAI-compatible local API server and an extension system for providers | [jan.ai/docs](https://www.jan.ai/docs), [github.com/janhq/jan](https://github.com/janhq/jan) |
| LM Studio | Local model runtime + chat UI | Desktop app for discovering/running local models with an OpenAI-compatible server and, since v0.3.17, local and remote MCP tool support with per-call confirmation | [lmstudio.ai/docs/app/mcp](https://lmstudio.ai/docs/app/mcp) |
| Cursor | AI-native code editor | VS Code-derived editor with Agent Mode, cloud-hosted "background agents"/"cloud agents" (isolated VMs), MCP support, and a mobile app for remote agent oversight | [cursor.com/product](https://cursor.com/product), [TechCrunch, 2026-06-29](https://techcrunch.com/2026/06/29/cursor-now-has-a-mobile-app-for-guiding-your-coding-agent-on-the-go/) |
| Devin | Autonomous software engineering agent | Cognition Labs' cloud-native agent that plans, codes, tests, and deploys with long-horizon reasoning, persistent memory, and browser/terminal/editor tool use | [Cognizant/Cognition partnership PR](https://investors.cognizant.com/news-and-events/news/news-details/2026/Cognizant-and-Cognition-Partner-to-Scale-Autonomous-Software-Engineering-and-Deliver-Business-Value-Across-Enterprise-Operations/default.aspx) |
| Replit Agent | Cloud IDE + autonomous builder | Browser-hosted IDE where "Agent 3" runs autonomously for extended sessions, self-tests, fixes its own bugs, and can build other agents/automations; ships iOS/Android apps | [blog.replit.com — Agent 3](https://blog.replit.com/introducing-agent-3-our-most-autonomous-agent-yet) |
| GitHub Copilot (coding agent) | IDE + cloud coding agent | VS Code/JetBrains agent mode plus a cloud "coding agent" that takes a GitHub issue, works in a GitHub Actions sandbox, and opens a PR; also a standalone desktop app (GA June 2026) and deep GitHub Mobile integration | [docs.github.com — about coding agent](https://docs.github.com/copilot/concepts/agents/coding-agent/about-coding-agent) |
| Codex-like workflows (OpenAI Codex) | Cloud + local coding agent | Agentic coding tool spanning CLI, desktop app, IDE extension, and ChatGPT, with cloud sandboxes, subagents, and parallel worktrees | [openai.com/codex](https://openai.com/codex/) |

Little Monkey itself: a local-first Tauri desktop workspace with a shared Rust/React contract across chat, workflows, background daemon runs, knowledge, browser verification, Git delivery, and a companion overlay — see `README.md` for the full shipped feature list this audit draws from.

## Gap category matrix

Each section below states what the capability means, how the eleven competitors currently stack up, Little Monkey's actual current status, the risk of leaving the gap open, a priority recommendation, and the roadmap item (or explicit non-goal) it maps to.

### 1. Autonomous task orchestration

*Can the product take one instruction and autonomously carry it through a multi-step task — planning, executing, self-testing, and producing a reviewable result — without a human first authoring the exact steps?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | Framework supports multi-step workflows, cron scheduling, and conditional branching, but is alpha and developer-facing, not a polished "just describe the task" experience |
| Msty | Partial | "Msty Claw" (beta) does autonomous multi-step task execution with sandboxed, folder-scoped file access |
| AnythingLLM | Partial | Agent Flows and tool-using agents exist; no evidence of long-horizon (hours), self-testing autonomy |
| Open WebUI | Partial | Extensible via Tools/Pipes/Actions and community agents (e.g. `cptr`); no first-party long-horizon autonomous orchestrator |
| Jan | No | Chat + custom assistants; no autonomous multi-step task runner found |
| LM Studio | No | Chat + per-call-confirmed tool use; not an autonomous task orchestrator |
| Cursor | Yes | Background/cloud agents provision isolated cloud VMs and run independently (announced Feb 2026) |
| Devin | Yes | Flagship capability: plans, codes, tests, debugs, and deploys with long-horizon autonomy |
| Replit Agent | Yes | Agent 3 runs autonomously up to ~200 minutes, self-tests, fixes its own issues, can build other agents |
| GitHub Copilot (coding agent) | Yes | Cloud coding agent takes a GitHub issue, works unattended in a sandbox, opens a PR; "Autopilot" permission level removes per-action stops |
| Codex-like workflows | Yes | Cloud sandbox mode runs tasks in parallel with subagents and worktrees, largely unattended |

**Little Monkey status:** Shipped orchestration primitives are strong — typed workflow DAGs, a background daemon with queued/idempotent/budgeted/approved runs, pause/resume/retry/crash recovery, and bounded-parallel Crew chats. What's missing is the *zero-authoring* entry point competitors default to: today a human must author (or select) a workflow/recipe before Little Monkey runs autonomously. There is no "give me one instruction, get an autonomous multi-step run and a PR" flow yet.

**Risk if open:** High — this is the single most visible parity axis against Devin, Replit Agent 3, Cursor cloud agents, Copilot coding agent, and Codex; prospective users benchmark on this first.

**Priority:** High

**Roadmap linkage:** `Issue-to-PR Agent Flow` (Phase 3, Next) is the direct match — it is exactly the "start from an issue, autonomously implement, open a PR" flow described above. `Side Tasks` (Phase 1, Next) covers the lighter-weight "start from selected context without a full workflow" case. `Agent Inbox and Run Dashboard` and `Run Capsules` (both Phase 1, Next) cover the visibility/evidence side of unattended runs.

### 2. Model comparison

*Can users run the same prompt across multiple models/providers and compare results directly?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | Not a chat product; not applicable in the same sense |
| Msty | Partial | Split/branching chats let users compare responses side by side |
| AnythingLLM | No | No dedicated side-by-side model compare surface found |
| Open WebUI | Partial | Multi-model chat exists; no dedicated scored comparison lab |
| Jan | No | Single-thread chat per assistant |
| LM Studio | No | One model per chat session |
| Cursor | Unclear | Model picker per agent session; not a side-by-side compare feature |
| Devin | No | Uses Cognition's own model stack; not user-facing model comparison |
| Replit Agent | Unclear | Auto model selection in Agent; no public side-by-side compare surface |
| GitHub Copilot (coding agent) | Partial | Model picker for a session (mobile added Feb 2026); not a scored comparison report |
| Codex-like workflows | No | GPT-5 family routing is internal, not a user-facing compare feature |

**Little Monkey status:** Already shipped and comparatively strong — Compare runs 2-4 explicit targets with independent streaming, stop/retry, timing, usage, persistence, and response promotion, deliberately with tools off by default. This is closer to parity-plus than a gap today.

**Risk if open:** Low for the shipped basic case; Medium for the "lab-grade" evaluation experience (batch prompts, rubric scoring, cost/latency/tool-success benchmarks) that neither Little Monkey nor most named competitors currently offer end-to-end.

**Priority:** Medium

**Roadmap linkage:** `Model Compare Lab` (Phase 2, Next) — extends the shipped Compare feature into batch/rubric/benchmark territory. `Workflow and Agent Test Harness` (Phase 2, Planned) is the adjacent eval-suite item.

### 3. Browser/terminal integration

*Is there a first-class in-app terminal and browser surface the agent and user can both use as evidence?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | Framework-level; no documented built-in terminal/browser UI |
| Msty | No | No dedicated terminal/browser panel documented; Msty Claw is sandboxed file access, not a visible terminal/browser workbench |
| AnythingLLM | Partial | Agents can browse/scrape the web; no terminal panel |
| Open WebUI | Partial | Community `cptr` (computer/coding agent) exposes terminal, files, and git from a browser tab and phone; not core Open WebUI |
| Jan | No | Chat-only surface |
| LM Studio | No | Chat + MCP tool calls; no terminal/browser panel |
| Cursor | Yes (terminal) / Partial (browser) | Deep terminal integration; cloud/background agents run tests and commands in sandboxes; no dedicated visual-QA browser workbench documented |
| Devin | Yes | Explicitly uses browsers, terminals, and code editors as tools |
| Replit Agent | Yes | Browser-hosted IDE is terminal-native by default; Agent 3 tests its own app in-browser automatically |
| GitHub Copilot (coding agent) | Partial | Runs shell commands in local/cloud sandboxes; browsing is not the primary surface |
| Codex-like workflows | Yes | CLI/terminal-centric across surfaces, plus built-in web search mid-session |

**Little Monkey status:** Shipped a disposable Chromium **Browser Verification** tool (Settings surface: navigate/inspect/click/type/scroll/screenshot, console/network evidence, exact-origin grants) and a workspace-scoped shell tool for the agent — but neither is a first-class panel next to chat, there is no persistent authenticated browsing, and there is no in-app terminal UI at all (agent shell calls happen, but there's no terminal tab a user opens directly).

**Risk if open:** High — this is a concrete, named-in-roadmap gap against exactly the workflow style Cursor, Devin, Replit, and Codex all lead with.

**Priority:** High

**Roadmap linkage:** `Integrated Terminal` (Phase 1, Next) and `Browser Workbench and Visual QA` (Phase 1, Next) are direct, already-scoped matches.

### 4. Memory

*Does the product retain durable, inspectable facts across sessions (not just document/RAG retrieval)?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Yes | Explicit memory subsystem: vector embeddings + BM25 search + state composition for context assembly |
| Msty | Partial | Knowledge stacks are document memory, not a dedicated cross-session personal-memory feature |
| AnythingLLM | Partial | Workspace-scoped vector knowledge; no dedicated cross-session user-memory UX found |
| Open WebUI | Unclear | Knowledge/RAG features documented; a dedicated personalization-memory feature could not be confirmed from primary sources checked |
| Jan | No | No dedicated memory feature found |
| LM Studio | No | No memory feature found |
| Cursor | Partial | Native "Memories" feature shipped mid-2025, then removed in v2.1.x; users pointed to Rules or third-party MCP memory servers as of 2026 |
| Devin | Yes | Persistent memory system called out as a core architectural feature |
| Replit Agent | Unclear | Not confirmed as a distinct feature in sources checked |
| GitHub Copilot (coding agent) | Partial | Repo-level custom instructions (e.g. `copilot-instructions.md`); not a cross-session personal memory system |
| Codex-like workflows | Partial | "Skills" teach standards persistently; ChatGPT-surface memory exists at the ChatGPT product level, not confirmed as Codex-agent-specific |

**Little Monkey status:** Ships a **memory** agent tool alongside file/shell/web/knowledge/MCP tools, plus Knowledge Stacks 2.0 for retrieval — so the foundation exists. What's missing is the governance/UX layer: inspecting memory by scope (project/workspace/user/device/connector), provenance ("why do you know this?"), pin/merge/expire/delete, and redacted export.

**Risk if open:** Medium — foundation already exists and is arguably ahead of Cursor's current (removed-native) state; the gap is UX/trust maturity, not a missing capability.

**Priority:** Medium

**Roadmap linkage:** `Memory Studio` (Phase 1, Planned).

### 5. Model routing

*Can the product automatically or policy-drivenly choose which model/provider handles a request?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | No | Not highlighted as a framework feature |
| Msty | No | Manual model selection per chat |
| AnythingLLM | No | Manual model selection per workspace |
| Open WebUI | Partial | Model presets/pipelines can approximate routing; not a policy engine |
| Jan | No | Manual selection |
| LM Studio | No | Manual selection |
| Cursor | Partial | "Auto" mode selects a model automatically; not user-defined policy |
| Devin | Unclear | Primarily Cognition's own model stack |
| Replit Agent | Partial | Auto model selection inside Agent |
| GitHub Copilot (coding agent) | Partial | Model picker per session; not budget/policy-driven |
| Codex-like workflows | Partial | GPT-5 family routing is internal to OpenAI, not user-policy-driven |

**Little Monkey status:** Already ships capability-aware routing with provider failover — ahead of most named competitors, none of which expose user-defined routing policy today.

**Risk if open:** Low — this is a forward differentiation opportunity, not a catch-up gap.

**Priority:** Medium (roadmap value, not competitive urgency)

**Roadmap linkage:** `Policy-Driven Model Router` (Phase 2, Next).

### 6. Connector ecosystems

*How broad and how easy is the catalog of third-party app/data connectors?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | Plugin architecture (Actions/Providers/Evaluators/Services) is extensible but developer-facing, no curated catalog |
| Msty | Unclear | Multi-provider model connections; no broad SaaS connector catalog found |
| AnythingLLM | Yes | Broad integrations plus MCP support for arbitrary tool servers |
| Open WebUI | Yes | Tools/Pipes/MCP/MCPO/OpenAPI framework; community knowledge connector (`oikb`) covers 45+ sources (GitHub, Jira, Slack, SharePoint, Notion, etc.) |
| Jan | Partial | Extension system adds providers, not a broad SaaS connector catalog |
| LM Studio | Partial | Local and remote MCP server support since v0.3.17, with per-call confirmation; no curated OAuth connector catalog |
| Cursor | Partial | MCP support plus GitHub-native integration; broader SaaS connectors mostly via community MCP servers |
| Devin | Yes | Enterprise workflow integrates Linear, Slack, GitHub, Jira |
| Replit Agent | Partial | GitHub import/export and deployment integrations; not a broad SaaS connector catalog |
| GitHub Copilot (coding agent) | Yes | Deep native GitHub ecosystem plus VS Code extension marketplace |
| Codex-like workflows | Partial | GitHub-centric; broader connectors via the separate ChatGPT connector ecosystem |

**Little Monkey status:** Ships MCP support (remote OAuth metadata/tokens, structured content, routed tools without bypassing allowlists) and a first-party declarative catalog seeded with GitHub, GitLab, WebDAV, and REST/webhook connector packages plus six skills. This is a real foundation but a narrow catalog next to AnythingLLM, Open WebUI, and GitHub Copilot's native depth.

**Risk if open:** High — connector breadth is a recurring adoption blocker for team/knowledge-worker use cases, and multiple competitors already cover the common work-app set (Slack, Jira, Notion, Drive) that Little Monkey doesn't yet.

**Priority:** High

**Roadmap linkage:** `Connector Catalog and OAuth Wizard` (Phase 3, Next) and `External Knowledge Sync Pipelines` (Phase 3, Next) are direct, already-scoped matches.

### 7. Remote execution

*Can work run on hardware other than the user's local machine?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Partial | Designed for self-hosted edge/home-server/air-gapped deployment; no cloud sandbox product |
| Msty | No | Fully local desktop app |
| AnythingLLM | Partial | Mobile app connects back to a user-run server, similar pattern to a self-hosted remote node |
| Open WebUI | Partial | Self-hosted server reachable remotely by design; community `cptr` gives phone control of a user's own machine |
| Jan | No | Local-only |
| LM Studio | No | Local desktop tool; only remote MCP *servers*, not remote execution of the app itself |
| Cursor | Yes | Cloud agents run in Cursor-hosted isolated VMs (announced Feb 2026) |
| Devin | Yes | Cloud-native by default (Cognition-hosted) |
| Replit Agent | Yes | Fully cloud/browser-hosted by default |
| GitHub Copilot (coding agent) | Yes | Runs in a GitHub Actions-hosted cloud sandbox |
| Codex-like workflows | Yes | Cloud sandbox mode (network-disabled container) alongside a local CLI mode |

**Little Monkey status:** Ships a **user-owned remote runner** — pairing over direct/Tailscale/SSH-forwarded HTTPS with pinned TLS, mutually scoped credentials, rotation/revocation, replay protection, and audit history. This is deliberately different from competitors' first-party hosted cloud sandboxes: Little Monkey explicitly has no relay, and inference/tools/keys stay on hardware the user owns.

**Risk if open:** Medium. The strategic gap (no hosted, zero-setup cloud sandbox for users without their own remote hardware) is an intentional trade-off, already captured as a non-goal, not an oversight — but it does mean users without a spare machine/homelab node get none of the "close the laptop, the cloud keeps working" experience Cursor/Devin/Replit/Copilot/Codex all offer out of the box.

**Priority:** Medium

**Roadmap linkage:** `Sandboxed Execution Environments` (Phase 5, Next) covers the closest first-party answer (local containers/VMs plus "optional homelab runner support for heavier workloads"). The absence of a Little-Monkey-hosted cloud relay is already covered by the existing non-goal **"Hosted Little Monkey relay by default"** in `ROADMAP.md`'s Non-Goals section — this audit does not propose changing that.

### 8. Mobile

*Is there a mobile app or mobile-equivalent surface?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | No | Framework/library, no mobile client |
| Msty | Unclear | No mobile app found in sources checked |
| AnythingLLM | Yes | Dedicated mobile app with on-device agents, web search, deep research, and MCP use |
| Open WebUI | Partial | PWA/responsive web access; separate community "Open MobileUI" project for native iOS/Android |
| Jan | No | Desktop-only |
| LM Studio | No | Desktop-only; sources checked found no mobile feature |
| Cursor | Yes | Dedicated mobile app for remote agent oversight, launched June 2026 |
| Devin | Unclear | Web-based; usable via mobile browser, no dedicated native app confirmed |
| Replit Agent | Yes | Dedicated iOS/Android apps; browser-hosted nature makes phone/tablet usable by default |
| GitHub Copilot (coding agent) | Yes | Deep integration into the GitHub Mobile app (not a standalone Copilot app): session monitoring, model picker, plan review, diff review |
| Codex-like workflows | Partial | Reachable via the ChatGPT mobile app surface for Codex cloud tasks |

**Little Monkey status:** No mobile app or mobile-equivalent surface exists today; `README.md` does not describe one.

**Risk if open:** High — this is the most one-sided category in the audit. Six of the ten competitors with mobile/near-mobile status now ship it, several within the last few months of the audit date, which is a fast-moving signal that "approve/monitor from your phone" is becoming a baseline expectation, not a differentiator.

**Priority:** High

**Roadmap linkage:** `iOS and Android Companion` (Phase 4, Planned), `Mobile Offline Mode` (Phase 4, Planned), and `Mobile-to-Homelab Pairing and Model Sharing` (Phase 4, Planned) are direct matches. **Recommendation:** given how quickly competitors have shipped mobile oversight surfaces in the first half of 2026 (Cursor, Replit, AnythingLLM, GitHub Mobile Copilot integration all shipped or expanded in this window), product owners may want to reassess whether Phase 4's mobile items should move from **Planned** to **Next** sooner than the current phase ordering implies. This audit flags the observation; it does not change the roadmap status itself.

### 9. Governance

*Multi-user roles, org policy, and audit trails beyond a single user's local approvals.*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | No | Single-tenant framework; no governance layer documented |
| Msty | No | Single-user desktop app |
| AnythingLLM | Partial | Multi-user mode (Admin/Manager/Default roles) in the self-hosted server deployment; the desktop app is single-user only |
| Open WebUI | Yes | Per-user/group access control is a documented feature of the self-hosted multi-user platform |
| Jan | No | Single-user desktop app |
| LM Studio | No | Single-user desktop app |
| Cursor | Unclear | Enterprise/Business plans exist; specific RBAC/audit-export feature set not verified in sources checked |
| Devin | Partial | Enterprise partnerships (e.g. Cognizant) imply org-level deployment; specific RBAC/audit features not publicly detailed in sources checked |
| Replit Agent | Partial | Teams/Enterprise plans reference role management; specifics not verified in sources checked |
| GitHub Copilot (coding agent) | Yes | Inherits GitHub's organization/enterprise RBAC, policy, and audit logging |
| Codex-like workflows | Partial | Inherits ChatGPT Enterprise/Team admin controls where used through that surface |

**Little Monkey status:** Strong single-user governance — permission modes (manual/plan/acceptEdits/smart/auto/bypass), a deterministic risk floor for sensitive paths, checkpoints with revert/rewind, and Security Doctor posture checks. There is no multi-user RBAC, org policy pack, or exportable audit trail across users/devices; this is by design so far (no account plane), not an oversight.

**Risk if open:** Medium — low urgency for the individual/homelab-first audience Little Monkey currently targets, but a real blocker if/when team or family deployments are a priority, and several competitors (Open WebUI, AnythingLLM server mode, GitHub Copilot) already clear this bar.

**Priority:** Medium

**Roadmap linkage:** `Team, Family, and Organization Mode` (Phase 6, Planned) and `Human Approval Chains` (Phase 3, Planned) are direct matches. That roadmap item already scopes SSO/SCIM as conditional ("only if the product deliberately introduces an account plane"), which this audit does not challenge.

### 10. PC control

*Can the agent observe and control the user's desktop (mouse/keyboard/screen) beyond a sandboxed browser or shell tool?*

| Competitor | Support | Notes |
| --- | --- | --- |
| Agent-Zoey / Zoey | Unclear | Not documented as a framework feature |
| Msty | No | Not offered |
| AnythingLLM | No | Not offered |
| Open WebUI | Unclear | Community `cptr` agent exposes files/terminal/git from a machine, but general mouse/keyboard/screen control was not confirmed |
| Jan | No | Not offered |
| LM Studio | No | Not offered |
| Cursor | No | Editor/terminal/browser-sandbox scope; no general desktop control found |
| Devin | No | Browser/terminal/editor tool use; no general desktop control found |
| Replit Agent | No | Browser-hosted IDE scope; no general desktop control found |
| GitHub Copilot (coding agent) | No | Sandboxed shell/editor scope; no general desktop control found |
| Codex-like workflows | No | CLI/cloud sandbox/editor scope; no general desktop control found |

**Little Monkey status:** Explicitly out of scope today. `README.md`'s browser verification section and Current Limitations both state general host-computer control is intentionally unavailable.

**Risk if open:** Low — unlike every other category in this audit, **none** of the eleven named competitors publicly document general desktop/PC control either. There is no competitive pressure forcing this open; it remains a deliberately hard, safety-gated research problem rather than a parity gap.

**Priority:** Low (proportionate to current roadmap status — no acceleration recommended)

**Roadmap linkage:** `Safe Desktop Control` and `Remote PC Control` (both Phase 5, Research) already track this at an appropriately conservative status. The existing non-goal **"Silent PC control"** already rules out the unsafe version of this capability; this audit does not propose adding a new non-goal or a new roadmap item here.

## Master gap ledger

Compact view of the ten categories: Little Monkey's current status, risk of leaving the gap open, recommended priority, and the roadmap linkage that satisfies "linked roadmap item or explicit non-goal."

| # | Gap category | Little Monkey status today | Risk if open | Priority | Roadmap item(s) / non-goal |
| --- | --- | --- | --- | --- | --- |
| 1 | Autonomous task orchestration | Workflow DAGs + background daemon runs + Crew; no zero-authoring "one instruction → autonomous run → PR" entry point | High | High | `Issue-to-PR Agent Flow`, `Side Tasks`, `Agent Inbox and Run Dashboard`, `Run Capsules` (Phase 1/3) |
| 2 | Model comparison | Shipped: 2-4 target Compare with streaming/timing/usage/persistence | Low–Medium | Medium | `Model Compare Lab` (Phase 2) |
| 3 | Browser/terminal integration | Disposable browser verification (Settings) + agent shell tool; no first-class panel, no in-app terminal UI | High | High | `Integrated Terminal`, `Browser Workbench and Visual QA` (Phase 1) |
| 4 | Memory | `memory` agent tool + Knowledge Stacks 2.0; no inspect/provenance/pin/expire UX | Medium | Medium | `Memory Studio` (Phase 1) |
| 5 | Model routing | Shipped: capability-aware routing + provider failover; ahead of most competitors | Low | Medium | `Policy-Driven Model Router` (Phase 2) |
| 6 | Connector ecosystems | MCP + first-party catalog (GitHub, GitLab, WebDAV, REST/webhook, 6 skills); narrow vs. leaders | High | High | `Connector Catalog and OAuth Wizard`, `External Knowledge Sync Pipelines` (Phase 3) |
| 7 | Remote execution | User-owned paired remote runner (Tailscale/SSH/HTTPS); deliberately no hosted cloud sandbox | Medium | Medium | `Sandboxed Execution Environments` (Phase 5); hosted-relay gap covered by existing non-goal "Hosted Little Monkey relay by default" |
| 8 | Mobile | None shipped | High | High | `iOS and Android Companion`, `Mobile Offline Mode`, `Mobile-to-Homelab Pairing and Model Sharing` (Phase 4) — see phase-timing recommendation above |
| 9 | Governance | Strong single-user permission modes/checkpoints/Security Doctor; no multi-user RBAC/org audit | Medium | Medium | `Team, Family, and Organization Mode`, `Human Approval Chains` (Phase 3/6) |
| 10 | PC control | Explicitly out of scope; no competitor covers it either | Low | Low | `Safe Desktop Control`, `Remote PC Control` (Phase 5, Research); non-goal "Silent PC control" |

## Biggest gaps (summary)

In priority order, the audit's three highest-confidence, highest-priority findings:

1. **Mobile is the starkest gap.** Little Monkey has zero mobile presence while six of ten comparable competitors now ship a mobile app or GitHub-Mobile-style integration, several shipped in the months immediately before this audit. Already tracked (`iOS and Android Companion` and related Phase 4 items), currently `Planned` rather than `Next`.
2. **Browser/terminal integration is half-shipped.** The disposable browser verification tool and agent shell tool are real, scoped, safety-conscious building blocks, but neither is exposed as the first-class workbench/terminal panel that Cursor, Devin, Replit, and Codex all lead with. Already tracked (`Integrated Terminal`, `Browser Workbench and Visual QA`, both Phase 1 `Next`).
3. **Connector ecosystem breadth trails category leaders.** AnythingLLM, Open WebUI, and GitHub Copilot all cover common work apps (Slack, Jira, Notion, Drive, SharePoint) more broadly than Little Monkey's current GitHub/GitLab/WebDAV/REST catalog. Already tracked (`Connector Catalog and OAuth Wizard`, `External Knowledge Sync Pipelines`, both Phase 3 `Next`).

Two categories where Little Monkey is at or ahead of parity today: **model comparison** (shipped Compare feature) and **model routing** (shipped capability-aware routing + failover) — both already have roadmap items to extend them further, but neither is a catch-up gap.

One category, **PC control**, is the only one where no named competitor creates competitive pressure; its `Research` status in `ROADMAP.md` is already proportionate and this audit does not recommend accelerating it.

No new roadmap items or non-goals were required by this audit — every high-priority gap found already has a scoped, matching `ROADMAP.md` item, and the one deliberate strategic trade-off this audit surfaced (no hosted Little Monkey cloud relay) was already an explicit non-goal before this audit.

## Sources consulted

- `README.md` and `ROADMAP.md` (this repository) — Little Monkey's shipped/planned capability ground truth.
- [github.com/Agent-Zoey/Zoey](https://github.com/Agent-Zoey/Zoey) — Zoey framework README.
- [msty.ai](https://msty.ai/) and [msty.ai/studio/features](https://msty.ai/studio/features).
- [docs.anythingllm.com](https://docs.anythingllm.com/) (MCP compatibility, agents, security/access) and [anythingllm.com/mobile](https://anythingllm.com/mobile).
- [openwebui.com](https://openwebui.com/) and [docs.openwebui.com/features](https://docs.openwebui.com/features/).
- [jan.ai/docs](https://www.jan.ai/docs) and [github.com/janhq/jan](https://github.com/janhq/jan).
- [lmstudio.ai/docs/app/mcp](https://lmstudio.ai/docs/app/mcp).
- [cursor.com/product](https://cursor.com/product); TechCrunch, "Cursor now has a mobile app for guiding your coding agent on the go," 2026-06-29; Cursor forum/changelog discussion of the Memories feature's mid-2025 launch and v2.1.x removal.
- Cognizant/Cognition Labs enterprise partnership announcement (2026) for Devin.
- [blog.replit.com — "Introducing Agent 3: Our Most Autonomous Agent Yet"](https://blog.replit.com/introducing-agent-3-our-most-autonomous-agent-yet).
- [docs.github.com — About GitHub Copilot coding agent](https://docs.github.com/copilot/concepts/agents/coding-agent/about-coding-agent); GitHub Changelog entries for GitHub Mobile Copilot integration (Feb/Apr/Jul 2026).
- [openai.com/codex](https://openai.com/codex/) and related 2026 coverage of Codex's desktop app, subagents, and cloud/local execution modes.

Where a claim could not be traced to one of the sources above, it is marked "unclear / not publicly documented" in the tables rather than asserted.

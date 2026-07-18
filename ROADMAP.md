# Little Monkey Roadmap

This roadmap tracks future product work only. Shipped foundations stay documented in `README.md`; this file should not keep "Done" items or restate capabilities the app already has.

Every item below needs a clear boundary, a security model, and acceptance evidence before it can be called finished.

## Status Key

- **Next**: highest-leverage near-term work.
- **Planned**: important, but depends on earlier foundations or product sequencing.
- **Research**: needs design validation, threat modeling, or platform proof first.

## Phase 1: Core Workspace Parity

### Integrated Terminal

**Status:** Next

Add an in-app terminal surface similar to the Codex task terminal.

- Workspace-scoped terminal tabs.
- Command history per workspace.
- Kill/restart controls.
- Shell output attachable to chat as evidence.
- Optional "send terminal context to model" with explicit user approval.
- Same permission/risk policy as existing shell tool calls.

**Acceptance**

- Start, stop, and restart terminal sessions without freezing chat.
- Terminal output is bounded, searchable, and can be attached to a model turn.
- Long-running processes show status and can be cancelled.
- No terminal can escape the selected workspace policy without explicit user approval.

### Browser Workbench and Visual QA

**Status:** Next

Turn the current browser verification foundation into a first-class workbench beside chat/workspace.

- Dedicated browser panel with URL bar, history, reload, viewport presets, and task-scoped state.
- Page annotations that can be sent back to the agent with element/screenshot context.
- Screenshot diffing across revisions, themes, and viewport sizes.
- Accessibility checks, console capture, network capture, DOM snapshot, and performance trace summaries.
- Visual regression artifacts that can be attached to chat, verification events, and PR reports.
- Optional future mode for authenticated/persistent profiles behind a separate high-risk grant.

**Acceptance**

- User can open local previews and public URLs from the main workspace, not only Settings.
- Agent can consume bounded browser evidence without raw untrusted page content bypassing policy.
- A page can be tested across desktop/mobile viewport presets with saved screenshots and console/network evidence.
- File upload/download, clipboard, browser extensions, persistent profiles, and signed-in browsing remain blocked unless a later explicit grant system ships.

### Side Tasks

**Status:** Next

Add side-task panes for parallel, interruptible work that is lighter than a daemon run and more visible than a hidden tool call.

- Start a side task from selected chat context, selected files, terminal output, browser evidence, or an MCP result.
- Show task state, active model, tools used, approvals waiting, and produced artifacts.
- Promote side-task output back into the main chat only by user action.
- Support pause, cancel, retry, archive, and "open as full task."

**Acceptance**

- A side task can run without blocking the main chat.
- Side-task tool permissions are isolated from the main turn.
- Outputs are traceable to task id, model, prompt snapshot, and tool evidence.

### Appearance Settings and Personalization

**Status:** Next

Keep a visible **Settings -> Appearance** roadmap item so personalization is treated as a first-class product surface.

- Theme preference: system, light, dark, and future high-quality theme packs.
- Accent colors, text size, code font size, density, sidebar layout, and chat bubble style.
- Reduced motion, high contrast, focus visibility, and accessibility-friendly presets.
- Live preview inside Settings before committing changes.
- Per-device defaults with optional per-workspace overrides.
- Import/export appearance profiles for teams, families, and homelab installs.

**Acceptance**

- User can find **Appearance** directly in Settings.
- Changes apply live and survive restart.
- Appearance choices do not break contrast, keyboard focus, responsive layout, or readable code blocks.

### Global Command Palette

**Status:** Next

Add a Raycast-style command surface that lets users reach Little Monkey from anywhere without opening a full chat first.

- Global shortcut with searchable commands, workflows, chats, models, tasks, snippets, files, and connectors.
- Capture selected text, clipboard text, selected file, or screenshot as explicit input.
- Quick actions for summarize, rewrite, translate, ask model, start workflow, search knowledge, create task, and approve pending action.
- Scope indicator showing which workspace, model, tools, and privacy policy will apply before execution.
- Optional mobile/desktop shared quick actions once pairing exists.

**Acceptance**

- User can trigger a safe command from outside the main app and see exactly what context will be sent.
- Commands that touch files, connectors, cloud models, or external writes use the same approval and privacy gates as chat.
- Every palette action is recorded as a normal run or task with evidence and cancellation.

### Agent Inbox and Run Dashboard

**Status:** Next

Create one inbox for side tasks, background agents, scheduled runs, PR agents, approvals, failed runs, and completed artifacts.

- Unified list of active, waiting, failed, completed, archived, and scheduled work.
- Filters by workspace, source trigger, model, connector, status, cost, and risk level.
- One place to approve/deny pending actions, cancel runs, inspect evidence, and open artifacts.
- Per-run timeline with prompt snapshot, model, tools, approvals, diffs, browser evidence, and verification results.

**Acceptance**

- User can answer "what is running and what needs me?" from one screen.
- Approval waits from side tasks, daemon runs, workflows, and connectors appear in the same inbox.
- Run details are audit-ready without requiring raw log spelunking.

### Run Capsules

**Status:** Next

Package any meaningful agent run into a reproducible, inspectable capsule.

- Include prompt snapshot, model/provider settings, routing rule, tool list, permissions, approvals, files changed, artifacts, browser screenshots, terminal excerpts, connector calls, costs, and verification results.
- Export as a redacted bundle for debugging, support, compliance, or team review.
- Re-run or replay from safe boundaries when dependencies are still available.
- Compare capsules across model, prompt, workflow, or code changes.

**Acceptance**

- User can open a run capsule and understand what happened without reading raw logs.
- Capsule export redacts secrets and private paths by default.
- Re-run clearly distinguishes deterministic replay, best-effort replay, and non-repeatable external effects.

### Agent-Zoey / Competitor Parity Audit

**Status:** Next

Turn competitor comparison into a repeatable audit, then track the gaps.

- Compare Little Monkey against Agent-Zoey/Zoey, Msty, AnythingLLM, Open WebUI, Jan, LM Studio, Cursor, Devin, Replit Agent, GitHub Copilot agents, and Codex-like workflows.
- Track gaps in autonomous task orchestration, model comparison, browser/terminal integration, memory, model routing, connector ecosystems, remote execution, mobile, governance, and PC control.
- Convert each gap into a scoped roadmap item with evidence, not vague parity claims.

**Acceptance**

- A checked-in comparison matrix lists competitor capability, Little Monkey status, risk, and priority.
- Each high-priority gap has a linked roadmap item or explicit non-goal.

See [`docs/competitor-parity-audit.md`](docs/competitor-parity-audit.md) for the current comparison matrix and gap-to-roadmap linkage.

### Record and Replay Workflows

**Status:** Next

Let users demonstrate a browser or desktop workflow once, then turn it into a reusable skill/workflow.

- Record user steps with visible recording state and narrow capture boundaries.
- Convert the recording into a draft skill/workflow with inputs, stable selectors, decision points, and verification.
- Require user review before enabling replay.
- Support browser replay first; desktop replay depends on the PC-control safety model.

**Acceptance**

- User can record a short browser workflow and receive a reusable draft workflow.
- Draft replay never receives hidden credentials or sensitive fields unless the user explicitly marks them as runtime inputs.
- Replayed actions are logged with screenshot/action evidence and can be cancelled.

### Checkpoint Preview and State-Aware Rollback

**Status:** Planned

Build on existing file/conversation checkpoints with richer previews and non-file state awareness.

- Timeline preview of code, artifacts, browser screenshots, and verification state at each checkpoint.
- Compare any two checkpoints without restoring them.
- Optional app/data-state capture for supported local services and test databases.
- Rollback simulation that shows what will change before restoring.

**Acceptance**

- User can preview a checkpoint before restoring it.
- Rollback clearly distinguishes file, artifact, conversation, and external state.
- Unsupported external effects are marked `needs_reconciliation` instead of being silently retried or reversed.

### Memory Studio

**Status:** Planned

Give users full control over what Little Monkey remembers and why.

- Inspect project, workspace, user, device, and connector-derived memories separately.
- Show source turn, file, connector, date, confidence, scope, and last-used timestamp.
- Edit, pin, merge, expire, delete, import, export, and disable memories.
- "Why do you know this?" links from chat answers back to memory evidence.
- Privacy controls for memories that may contain secrets, personal data, or proprietary project facts.

**Acceptance**

- User can see every durable memory and its scope.
- Deleting or disabling a memory prevents it from entering future prompts.
- Memory exports are portable and redacted by default.

## Phase 2: Model Intelligence and Evaluation

### Model Compare Lab

**Status:** Next

Expand the existing Compare flow into a lab for model selection, prompt testing, and quality review.

- Batch prompts across saved model sets.
- Side-by-side scoring by user rubric, latency, token use, cost, tool-use success, and verifier outcome.
- Saved benchmark suites for coding, writing, RAG, browser QA, and connector tasks.
- Promote the best response, prompt, or model rule back into normal chat/workflows.
- Export comparison reports for teams.

**Acceptance**

- User can run the same prompt suite across multiple models and compare results in one report.
- Results preserve model/provider settings, prompt snapshots, tool availability, timing, cost, and verifier evidence.
- Compare Lab does not grant tools by default unless the suite explicitly enables them.

### Policy-Driven Model Router

**Status:** Next

Move beyond existing capability-aware routing into user-defined routing policies.

- Rules such as local-first, cheapest acceptable, fastest, best reasoning, private-only, cloud-with-approval, or task-specific model.
- Per-workspace and per-workflow routing policies.
- Fallback chains with clear reasons when the first target is unavailable.
- Budget-aware routing that can ask before escalating to a more expensive model.
- Dry-run mode that shows which model would be selected and why.

**Acceptance**

- User can define a routing policy without editing config files.
- Every routed turn records the selected model and the rule that chose it.
- A cloud or higher-risk model cannot be selected silently when policy requires approval.

### Model Benchmark and Quantization Advisor

**Status:** Next

Help users pick the best local model and quantization for their machine and task.

- Run local benchmarks for latency, memory use, context length, tool calling, coding, RAG, summarization, and vision where supported.
- Recommend model family, size, quantization, runtime, context window, and keepalive settings.
- Warn when a model is too slow, too large, missing a needed capability, or likely to spill memory.
- Save benchmark profiles per device and update recommendations when hardware/runtime/model versions change.

**Acceptance**

- User can run a benchmark suite and receive a clear recommendation for coding, RAG, chat, and workflow tasks.
- Recommendations include evidence, hardware assumptions, and fallback choices.
- No model is auto-downloaded or auto-switched without user approval.

### Workflow and Agent Test Harness

**Status:** Planned

Give users a way to test prompts, tools, RAG stacks, agents, and workflows before trusting them.

- Saved eval cases with inputs, expected constraints, allowed tools, and scoring rubrics.
- Regression suites for workflows and skills.
- Golden-answer and judge-model scoring modes.
- Tool-call replay in dry-run mode where possible.
- Failure clustering by prompt, model, connector, retrieval source, or verifier.

**Acceptance**

- User can run an eval suite against a model, skill, connector, or workflow.
- Results show pass/fail, evidence, cost, latency, and reproducibility metadata.
- Evals can be scheduled or attached as release gates.

### Prompt and Workflow Version Control

**Status:** Planned

Add Git-like history for reusable AI behavior.

- Version prompts, personas, skills, workflows, routing rules, eval suites, connector presets, and policy packs.
- Diff text, schemas, tool permissions, model choices, and approval requirements.
- Tag stable versions, stage changes, roll back, fork, and promote from draft to active.
- Run evals before activating a new version.

**Acceptance**

- User can inspect what changed before enabling a prompt, workflow, skill, or routing rule.
- Rollback restores the previous active version without corrupting existing run history.
- Activation can require passing evals or human approval.

### Observability and Cost Controls

**Status:** Planned

Deepen existing usage accounting and rate-limit warnings into dashboards and guardrails.

- Per-model latency, token, cost, error, and tool-use dashboards.
- Local model hardware utilization.
- Budget alerts per provider, model, workspace, workflow, and user/device.
- MCP and connector rate-limit visibility.
- Trend reports for scheduled tasks, daemon runs, and side tasks.

**Acceptance**

- User can identify the most expensive models, workflows, and connectors.
- Budget thresholds can warn, pause, or require approval before a run continues.
- Dashboards separate local compute metrics from paid-provider usage.

## Phase 3: Knowledge, Connectors, and External Work

### External Knowledge Sync Pipelines

**Status:** Next

Extend existing Knowledge Stacks with deeper source connectors and sync operations.

- GitHub repo, GitLab repo, S3/R2, Confluence, Jira, Google Drive, Notion, Slack, SharePoint, WebDAV, and local watched-folder connectors.
- Incremental sync with source cursors, deletion propagation, stale-source warnings, and reconnect prompts.
- Connector-specific provenance, permission boundaries, redaction previews, and reindex health.
- Source freshness and coverage report for each stack.

**Acceptance**

- User can connect at least GitHub, Google Drive, Notion, Slack, and Jira as knowledge sources through a guided flow.
- Reindex status shows stale, failed, deleted, redacted, and skipped sources.
- RAG citations link back to source identity and sync timestamp.

### Connector Catalog and OAuth Wizard

**Status:** Next

Move beyond raw MCP templates into a guided connector flow for common work apps.

- GitHub OAuth or `gh`-backed setup.
- Slack setup with app identity, workspace/admin requirements, and scopes surfaced clearly.
- Atlassian Rovo setup for Jira, Confluence, Bitbucket, and Compass.
- GitLab, Linear, Notion, Google Drive, Gmail, Calendar, Outlook, Teams, Figma, Box, SharePoint, Sentry, and WebDAV.
- Show requested scopes, toolsets, write abilities, and storage location before connect.
- OAuth token storage through OS keychain or provider-approved secure storage.

**Acceptance**

- User can connect GitHub, Slack, and Jira without manually editing raw MCP config.
- Tool allowlist defaults to least privilege.
- User can revoke, refresh, inspect scopes, and export a redacted connection report.

### Issue-to-PR Agent Flow

**Status:** Next

Let Little Monkey pick up a GitHub/Jira/Linear issue and carry it through a reviewable branch/PR loop.

- Import issue requirements and linked context.
- Create or select an owned branch/worktree.
- Plan, implement, run checks, summarize diff, and open/update a PR.
- Watch CI/checks and review comments.
- Prepare follow-up commits only after user approval or policy-approved daemon action.

**Acceptance**

- User can start from an issue URL and receive a branch plus reviewable PR draft.
- The PR report links requirements, changed files, tests, and unresolved risks.
- Merge, force-push, branch deletion, and thread resolution remain outside the default flow.

### Inbox Triage Agents

**Status:** Planned

Create focused agents for daily external work queues.

- Gmail/Outlook triage: summarize, label, draft replies, extract tasks, and ask before sending.
- Slack/Teams triage: summarize mentions, threads, decisions, blockers, and follow-ups.
- Jira/Linear/GitHub triage: rank issues, detect stale items, draft updates, and propose assignments.
- User-defined rules for VIPs, projects, urgency, quiet hours, and auto-draft behavior.

**Acceptance**

- Triage agents can run read-only summaries without write permissions.
- Draft/send/post/update actions require digest-bound approval unless an explicit policy grants them.
- Triage outputs link back to source messages/issues and show connector scope.

### Human Approval Chains

**Status:** Planned

Support multi-person or multi-step approval workflows instead of a single yes/no prompt.

- Approval nodes for owner, reviewer, teammate, admin, or paired device.
- Sequential and parallel approvals with timeout, escalation, and rejection paths.
- Approval request bundles with action digest, source evidence, risk class, and rollback/reconciliation plan.
- Templates for publish, send email, open PR, deploy, update Jira, and remote PC control.

**Acceptance**

- A workflow can pause until required approvals are complete.
- Every approval records who approved, from which device/account, what digest they approved, and when.
- Rejected approvals stop or reroute the workflow safely.

### Private Developer API and Embeddable Chat Widget

**Status:** Planned

Turn the local/private API foundation into a product surface for other apps and sites.

- Local SDKs for TypeScript, Python, and shell.
- Scoped API tokens for chat, model inference, knowledge query, workflow run, and artifact read.
- Embeddable private chat widget for internal sites and homelab portals.
- Webhook/event API for run status and approval waits.
- Usage and revocation controls per token/client.

**Acceptance**

- A local app can call Little Monkey through a documented SDK without using internal Tauri APIs.
- An embedded widget can chat with a selected model/knowledge scope without exposing filesystem or agent tools by default.
- API tokens can be revoked and audited.

### Local App Builder

**Status:** Planned

Let users turn workflows into small private apps for internal and homelab use.

- Form, dashboard, approval page, report generator, and chat-widget templates.
- Bind form inputs to workflow parameters, knowledge stacks, connectors, and model-routing policies.
- Local-only hosting first, with optional private network exposure through existing pairing/TLS controls.
- Per-app permissions, audit log, access grants, and rollback.

**Acceptance**

- User can publish a workflow as a local private app without writing frontend code.
- App users only get the scopes granted to that app, not the owner's full workspace/tool access.
- Every app run produces a normal run capsule and audit trail.

## Phase 4: Mobile and Homelab Mesh

### iOS and Android Companion

**Status:** Planned

Build native or cross-platform mobile apps that can do core Little Monkey work from a phone/tablet without turning the product into hosted SaaS.

- Chat with desktop/homelab models.
- View sessions, tasks, approvals, runs, artifacts, and notifications.
- Approve or deny pending actions.
- Start lightweight tasks using saved workflows.
- Capture mobile inputs: text, image, file, voice note.
- Read-only browse of workspace evidence by default; write operations require explicit approval and scoped capability.

**Recommended implementation path**

- Use React Native or Flutter for shared iOS/Android delivery.
- Keep the desktop/homelab node as the authority for tools, files, models, and provider keys.
- Mobile app acts as a controller and capture surface, not a place where private model credentials are copied by default.

**Acceptance**

- Pair a mobile device to a desktop/homelab node.
- Resume chat and approve pending tool calls from mobile.
- Receive push/local notifications for approval waits and completed tasks.
- No model files, provider keys, or workspace secrets sync to mobile unless explicitly exported by the user.

### Mobile Offline Mode

**Status:** Planned

- Local-only notes, drafts, voice captures, and queued prompts.
- Queue sync when a paired node becomes reachable.
- Clear offline/queued state so users know what has not run yet.

**Acceptance**

- Mobile can capture prompts and evidence while offline.
- Queued work does not execute until the paired node is reachable and policy checks pass.

### Mobile-to-Homelab Pairing and Model Sharing

**Status:** Planned

Build on the existing user-owned remote runner model so mobile and desktop clients can securely use a homelab node.

- LAN QR code, Tailscale/ZeroTier address, SSH reverse tunnel, or user-provided HTTPS endpoint pairing.
- Mutual device identity with pinned keys and short-lived pairing codes.
- Per-device grants: chat, model inference, view tasks, approve actions, run workflows, read artifacts, admin.
- Paired clients can list available model profiles without downloading model files.
- Admin controls which models each device can use.
- Optional per-device token/usage limits.
- Revocation, key rotation, replay protection, and audit logs for every remote action.

**Acceptance**

- Mobile can chat through a homelab model without copying model files or provider keys.
- Desktop can switch between local and homelab targets.
- Revoking a paired device immediately prevents new actions.
- Every remote action has device id, user-visible capability, timestamp, digest, and result.

## Phase 5: Trust, Sandboxing, and PC Control

### Privacy Firewall

**Status:** Next

Add a visible data boundary before prompts, files, connector data, or tool results leave the user's private context.

- Detect likely secrets, PII, credentials, private paths, proprietary source, customer data, and sensitive screenshots.
- Show what will be sent to a cloud model, connector, remote runner, MCP server, or paired device.
- Redact, block, require approval, or route to local-only models based on policy.
- Keep per-workspace data classification and exception rules.

**Acceptance**

- A cloud-bound prompt or connector call can be previewed with redaction before execution.
- Sensitive findings are tied to concrete spans/files/screenshots, not vague warnings.
- User can choose local-only fallback when privacy policy blocks external processing.

### Sandboxed Execution Environments

**Status:** Next

Run risky code, test suites, unknown repos, and generated commands in disposable environments.

- Workspace snapshots into local containers, VMs, or isolated dev environments where available.
- Network, filesystem, secret, and process policy per sandbox.
- Disposable browser/terminal/test pairing for risky reproduction.
- Artifact promotion back to the real workspace only by explicit user action.
- Optional homelab runner support for heavier workloads.

**Acceptance**

- User can run an unknown command or generated test in a sandbox without exposing the main workspace secrets by default.
- Sandboxed runs produce logs, diffs, artifacts, and exit status that can be attached to chat.
- Promotion back to the workspace shows exact files/artifacts that will be copied.

### Safe Desktop Control

**Status:** Research

Allow Little Monkey to control the user's PC only through an explicit, auditable, revocable control mode.

- Screen observation with visible capture indicator.
- Mouse/keyboard automation gated by a "Control PC" session.
- App/window allowlist.
- Emergency stop hotkey.
- Step-by-step approval mode by default.
- Never run in unattended `bypass`.
- Separate from browser automation and shell tools.

**Acceptance**

- User can start and stop control mode clearly.
- Every click/key action is logged with screenshot/time context.
- Emergency stop immediately releases input control.
- Control is blocked on password dialogs, payment flows, OS security prompts, and sensitive system settings unless explicitly supported by a future high-risk flow.

### Remote PC Control

**Status:** Research

Only after secure pairing and local PC control are proven.

- Remote approval from paired mobile/desktop controller.
- Session recording or screenshot evidence.
- Strict device grants.
- No hosted relay by default.

**Acceptance**

- Remote control cannot start without local visible consent.
- Revocation and kill switch work mid-session.

## Phase 6: Product Growth and Administration

### First-Run Onboarding and Use-Case Templates

**Status:** Next

- First-run setup for local model, Ollama, BYOK provider, workspace, and safety defaults.
- Use-case templates: code review, research, docs, QA, release, homelab admin, Jira triage, Slack summary, model evaluation, and browser QA.
- Sample prompts, model-routing rules, eval suites, and workflows seeded locally.
- "Private by default" setup path that never asks for cloud credentials.

**Acceptance**

- A new user can reach a useful local-first chat in one guided flow.
- Each template declares its model, tool, connector, permission, and verification assumptions.

### Team, Family, and Organization Mode

**Status:** Planned

- Shared policy packs.
- RBAC for local/homelab users.
- Device/user grants for connectors, model inference, workflows, artifacts, and approvals.
- Exportable audit reports.
- Workspace templates.
- Optional team-managed connector catalog.
- Optional SSO/SCIM only if the product deliberately introduces an account plane.

**Acceptance**

- Admin can grant read-only, approver, operator, and owner roles without exposing provider keys.
- Audit export shows who approved which action, on what device, against which workspace/model/connector.
- A family/homelab install can manage multiple trusted devices without a hosted account dependency.

### Daily Brief and Command Center

**Status:** Planned

Create a daily operating dashboard for the user's AI work.

- Pending approvals, running agents, failed scheduled jobs, completed summaries, and stale tasks.
- GitHub PRs/checks, Jira/Linear issues, Slack/Teams mentions, calendar items, and email highlights when connectors are enabled.
- Homelab node health, model/runtime status, storage, queue depth, and backup state.
- Suggested actions that are clearly draft-only until approved.

**Acceptance**

- User can start the day and see what needs attention without opening every connector.
- Brief items link to source evidence and required permissions.
- No connector is queried unless it is connected and enabled for the brief.

### Self-Healing Diagnostics

**Status:** Planned

Make troubleshooting interactive and repair-oriented.

- Diagnose Ollama, runtime hub, MCP, browser workbench, daemon, remote pairing, keychain, API server, knowledge indexing, and connector failures.
- Explain root cause, confidence, safe fix, risky fix, and manual fallback.
- Offer one-click safe repairs such as restarting owned services, clearing stale locks, disabling unsafe listeners, refreshing tokens, or rebuilding local indexes.
- Capture repair evidence into a run capsule.

**Acceptance**

- User can run a guided diagnosis from an error state.
- Safe repairs never delete user data, rotate secrets, or change external state without approval.
- Diagnostics produce a concise support bundle with secrets redacted.

### Community Marketplace and Discovery Hub

**Status:** Planned

Extend the existing signed package/skill/plugin foundation into a discovery and review experience.

- Public and private catalogs.
- Security review status, provenance, permissions, and install counts.
- Team-approved collections.
- Skill, prompt, connector, workflow, eval, and knowledge-stack templates.
- Rollback, disable, uninstall, health checks, and vulnerability notices.

**Acceptance**

- User can discover and install a reviewed package without leaving Little Monkey.
- The install screen shows source, signature/provenance, permissions, tools, and update policy.
- Unsigned local development mode stays available but visibly separate.

## Phase 7: Market-Defining Differentiators

These are deliberately separate from the earlier phases: they are the next 30 game-changing features that are not already shipped and are not already represented as roadmap items above.

### 1. Design-to-App Studio

**Status:** Planned

- Import Figma frames, screenshots, sketches, design-system tokens, and reference URLs.
- Generate working UI/routes into an owned branch with visual diff, accessibility baseline, and source mapping.

**Acceptance:** User can turn a design input into a reviewable app patch without losing code ownership or bypassing review.

### 2. Visual Design Edit Mode

**Status:** Planned

- Let users click text, spacing, color, layout, and responsive states in preview.
- Convert visual edits into source patches with before/after screenshots.

**Acceptance:** Visual edits map back to exact files and can be accepted, rejected, or replayed like normal code changes.

### 3. Product Manager Copilot

**Status:** Planned

- Turn goals into PRDs, user stories, acceptance criteria, risks, milestones, and release plans.
- Sync approved specs into GitHub, Jira, Linear, or local roadmap files.

**Acceptance:** A product idea can become a scoped, testable work plan with linked issues and verification gates.

### 4. Agent-Ready Spec Scorer

**Status:** Planned

- Score issues/specs for clarity, scope, missing context, testability, dependencies, and AI-agent readiness.
- Suggest exact missing information before an autonomous implementation starts.

**Acceptance:** Issue-to-PR runs warn when the source issue is too vague and show how to fix it.

### 5. Deep Research Workspace

**Status:** Planned

- Build multi-step research plans across web, local files, knowledge stacks, and connected apps.
- Produce cited reports, evidence tables, source maps, and open-question lists.

**Acceptance:** Every research conclusion links to source evidence and shows which sources were searched or skipped.

### 6. Evidence Board and Claim Checker

**Status:** Planned

- Extract claims from chats, reports, specs, and docs.
- Track supporting evidence, conflicting evidence, confidence, owner, and unresolved questions.

**Acceptance:** User can audit a report by claim instead of trusting a single generated summary.

### 7. Source-Grounded Brief Studio

**Status:** Planned

- Convert selected sources into executive briefs, slide outlines, audio overviews, video outlines, quizzes, flashcards, and study guides.
- Preserve citations and privacy policy for every generated asset.

**Acceptance:** Generated briefs stay tied to source material and can run fully local when policy requires it.

### 8. Infinite Work Canvas

**Status:** Planned

- Provide a spatial canvas for chats, files, tasks, workflows, screenshots, diagrams, models, and connectors.
- Let users connect nodes into plans, research boards, architecture maps, and task flows.

**Acceptance:** A saved canvas can spawn tasks/workflows and remains inspectable as project context.

### 9. Permission-Aware Universal Search

**Status:** Planned

- Search chats, files, artifacts, code, tasks, knowledge stacks, browser evidence, and connected apps from one place.
- Respect source permissions, device grants, connector scopes, and workspace boundaries.

**Acceptance:** Search results never reveal content the current user/device cannot access.

### 10. Knowledge Graph Explorer

**Status:** Planned

- Build entity and relationship graphs from repos, docs, chats, tickets, decisions, and knowledge stacks.
- Show owners, stale nodes, conflicting facts, dependencies, and missing links.

**Acceptance:** User can ask "how is X related to Y?" and inspect the graph evidence behind the answer.

### 11. Cross-Repo Code Intelligence

**Status:** Planned

- Index symbols, APIs, tests, ownership, dependencies, and call paths across multiple repos.
- Use the graph to improve agent context, review quality, and change planning.

**Acceptance:** An impact query returns affected repos, files, owners, tests, and likely migration steps.

### 12. Cross-Repo Change Planner

**Status:** Planned

- Plan coordinated changes across services, packages, docs, CI, and client apps.
- Create linked branches/PRs with dependency order and rollback guidance.

**Acceptance:** User can approve a multi-repo plan before any repo is modified.

### 13. Migration and Upgrade Agent

**Status:** Planned

- Handle framework, runtime, dependency, language, Tauri, React, Rust, and API migrations.
- Break large upgrades into safe slices with compatibility checks and rollback points.

**Acceptance:** A migration run produces a plan, branch, tests, risks, and follow-up checklist.

### 14. AI Security Autofix Pipeline

**Status:** Planned

- Triage SAST, dependency, secret, license, and generated-code security findings.
- Propose fixes with exploitability notes, tests, and regression checks.

**Acceptance:** Security fixes are generated in isolated branches and verified before user approval.

### 15. Production Debugging Agent

**Status:** Planned

- Connect logs, traces, errors, releases, commits, deploys, and code context.
- Find likely root cause, reproduce where possible, and prepare a fix branch.

**Acceptance:** A production issue can produce a root-cause report, repro evidence, and reviewable patch.

### 16. Incident Commander

**Status:** Planned

- Coordinate alerts, runbooks, status updates, mitigations, owners, timelines, and postmortems.
- Keep human approval for customer-facing, destructive, or infrastructure-changing actions.

**Acceptance:** An incident run captures timeline, decisions, evidence, mitigations, and postmortem draft.

### 17. Synthetic Monitoring Agent

**Status:** Planned

- Schedule browser and API journeys against local, staging, and production targets.
- Capture screenshots, console/network logs, latency, uptime, and regression evidence.

**Acceptance:** A failing monitor opens a run with evidence and proposed diagnosis.

### 18. Data Notebook and SQL Lab

**Status:** Planned

- Add notebook-style SQL, Python, R, chart, and markdown cells with local execution.
- Let agents inspect outputs, rerun cells, explain results, and generate reproducible reports.

**Acceptance:** A data analysis can be reproduced from saved cells, inputs, environment, and outputs.

### 19. Spreadsheet Copilot

**Status:** Planned

- Analyze and edit CSV, XLSX, Google Sheets, and Excel workbooks with cell-level references.
- Build formulas, pivots, charts, forecasts, validations, and cleanup steps with approval.

**Acceptance:** Spreadsheet changes cite exact cells/ranges and ask before mutating live workbooks.

### 20. Database Admin Guardrails

**Status:** Planned

- Explore schemas, propose queries, explain plans, draft migrations, and detect PII/risky writes.
- Require dry-runs, backups, or approvals before destructive SQL.

**Acceptance:** No write migration runs without a preview, rollback plan, and explicit approval.

### 21. Connector Builder Studio

**Status:** Planned

- Generate connectors from OpenAPI, GraphQL, Postman collections, webhooks, CLIs, or documentation.
- Include auth setup, schemas, tests, rate-limit handling, and tool permission metadata.

**Acceptance:** A generated connector can be tested in a sandbox before it becomes available to agents.

### 22. MCP Server Generator and Simulator

**Status:** Planned

- Turn local APIs, CLIs, scripts, or workflows into MCP servers with typed tools.
- Simulate tool calls, prompt-injection attempts, schema drift, auth failures, and approval policies.

**Acceptance:** Generated MCP servers must pass the simulator before install.

### 23. API Contract Diff and Mock Lab

**Status:** Planned

- Compare OpenAPI, GraphQL, protobuf, webhook, and event schemas across versions.
- Generate mocks, contract tests, client-impact reports, and migration notes.

**Acceptance:** Breaking API changes are detected before a release branch is marked ready.

### 24. SOP-to-Agent Compiler

**Status:** Planned

- Import SOPs, runbooks, checklists, docs, and training materials.
- Compile them into workflows/skills with inputs, policy gates, tests, and evidence requirements.

**Acceptance:** A compiled workflow remains inactive until reviewed and tested by the user.

### 25. Knowledge Pack and Creator Storefront

**Status:** Planned

- Package docs, prompts, workflows, evals, policies, connector presets, and dashboards as installable packs.
- Support private team packs and optional creator monetization later.

**Acceptance:** A pack can be installed, audited, updated, rolled back, and removed without hidden permissions.

### 26. Multi-Agent Debate and Red-Team Mode

**Status:** Planned

- Spawn proposer, critic, security, reliability, cost, and user-advocate agents for important decisions.
- Preserve disagreements and evidence instead of flattening them into one answer too early.

**Acceptance:** Final recommendations show major objections, tradeoffs, and why one path won.

### 27. Prompt-Injection and Tool-Abuse Lab

**Status:** Planned

- Test agents against hostile webpages, PDFs, emails, MCP outputs, repo files, screenshots, and connector payloads.
- Generate policy hardening suggestions and regression cases.

**Acceptance:** A red-team suite can prove that known attack fixtures are blocked or require approval.

### 28. Trust Scorecards

**Status:** Planned

- Score models, connectors, plugins, skills, workflows, MCP servers, and generated apps for quality, cost, privacy, security, reliability, and provenance.
- Show what evidence produced the score.

**Acceptance:** Users can compare trust profiles before enabling a high-impact capability.

### 29. Local Fine-Tune, Adapter, and Distillation Lab

**Status:** Research

- Prepare private datasets, train LoRA/QLoRA/adapters on local or homelab hardware, and evaluate results.
- Distill expensive cloud behavior into smaller local models where licenses allow it.

**Acceptance:** A trained adapter includes dataset provenance, license checks, eval results, and rollback.

### 30. Synthetic Data and Golden Dataset Builder

**Status:** Planned

- Generate, label, clean, deduplicate, and version datasets for evals, fine-tunes, RAG tests, and workflow tests.
- Mix synthetic examples with production traces only after privacy filtering.

**Acceptance:** A dataset can be traced back to sources, generation prompts, labels, privacy filters, and eval results.

## Phase 8: Ollama-Inspired Runtime and Model Ops

This phase comes from a live sweep of `ollama/ollama` closed PRs on July 15, 2026: 4,931 closed PRs fetched through the GitHub API, with 3,430 merged PRs used as the main signal. The goal is not to clone Ollama, but to learn from the runtime, model, GPU, registry, API, and diagnostics work that made Ollama dependable.

### 3. Multi-GPU and Heterogeneous Runtime Orchestration

**Status:** Research

- Support multiple GPUs and mixed CPU/GPU scheduling where runtimes allow it.
- Track per-device memory, load, thermal pressure, and failure fallback.

**Acceptance:** Multi-device execution never silently degrades correctness and always reports actual placement.

### 6. Modelfile Studio and Import Hardening

**Status:** Planned

- Provide a UI for Modelfile editing, parsing, validation, dry-run creation, `requires`, parameters, templates, licenses, adapters, and short names.
- Harden GGUF and safetensors import with clear errors and source metadata.

**Acceptance:** User can preview and validate a custom model package before it enters the model library.

### 8. Chat Template and Renderer Compatibility Lab

**Status:** Planned

- Test model-specific chat templates, tool rendering, image blocks, thinking modes, system prompts, and stop tokens.
- Compare native model templates against Little Monkey's renderer before a model is marked ready.

**Acceptance:** A model cannot be advertised as chat/tool/vision-ready until renderer tests pass.

### 9. Context and KV Cache Control Center

**Status:** Planned

- Inspect context window, prompt cache reuse, KV cache size, context shift, generation headroom, and cache invalidation.
- Give users safe controls for long-context tradeoffs.

**Acceptance:** Long-context failures explain whether the limit was prompt, cache, memory, runtime, or model metadata.

### 16. Edge Device Runtime Profiles

**Status:** Research

- Add tuned profiles for Jetson, Raspberry Pi, mini PCs, Apple Silicon, old CUDA GPUs, AMD APUs, and low-memory homelabs.
- Prefer safe fallbacks over failed loads.

**Acceptance:** Device-specific profiles explain supported models, expected speed, and required runtime components.

## Phase 9: Release Hardening

**Status:** Planned

- Clean-machine install/update tests on macOS, Windows, and Linux.
- Signed/notarized installers.
- Accessibility pass for keyboard and screen reader flows.
- Locale completion pass.
- Dependency and supply-chain review.
- Penetration test focused on connector OAuth, remote pairing, PC control, browser automation, shell, plugin install, and embeddable API/widget.
- Performance budgets for chat, search, settings, model lists, side tasks, browser workbench, and large sessions.

## Non-Goals Until Explicitly Reopened

- Hosted Little Monkey relay by default.
- Silent PC control.
- Copying provider keys or model files to mobile by default.
- Letting MCP server self-declared metadata bypass user allowlists.
- Unattended destructive external writes without a digest-bound approval and reconciliation plan.

#![recursion_limit = "4096"]

// `pub` so every module below (and `monkey-cli`, which has no `AppHandle`)
// resolves the app-data directory through one shared `data_dir()` instead of
// each hardcoding the same identifier string independently â€” see the module
// doc for the drift risk this replaces.
pub mod agent_worktrees;
pub mod app_paths;
// `pub` so a future `monkey-cli` parity command (matching `checkpoints`/`rules`/
// `memory`/`web`/`verify` above) could reuse `publish_impl`/`remove_impl`
// directly â€” no such command exists yet (rendering has no terminal surface,
// per the design doc's phase-4 note), but there's no reason to make this one
// module-private when every sibling with reusable core logic already isn't.
pub mod artifacts;
// Durable, content-addressed storage for run artifacts and attachments. This
// stays Tauri-free so the same integrity checks are reusable by the desktop,
// CLI, daemon, export/import pipeline, and user-owned remote runner.
pub mod artifact_store;
// Versioned, checksummed runtime/model asset installation with atomic
// activation and rollback. The manager is Tauri-free so desktop, CLI, and a
// future user-owned runner share one ownership and integrity policy.
pub mod asset_manager;
// Measured benchmarking (ROADMAP #2): a timing sink over the hub's canonical
// stream, so time-to-first-token, decode throughput and peak memory are read
// off a real generation on this machine rather than a device-class table.
pub mod benchmark;
// Native in-app browser pane: real tabbed child webviews (Claude-Desktop-
// style) overlaid on the main webview via the `unstable` multiwebview API.
pub mod browser_pane;
// Disposable Chromium/CDP verification worker with request interception,
// explicit origin grants, DNS re-checks, quotas, and durable evidence.
pub mod browser_worker;
// Typed, fixed-argument desktop bridge to the bundled authoritative daemon
// and optional user-owned remote controller. No arbitrary CLI execution.
mod daemon_commands;
mod extension_commands;
mod m6a_desktop_bridge;
// Digest-confirmed owned-worktree/GitHub delivery and local PR review. The
// core is kept Tauri-light so repository identity and safety policy remain in
// one place while the commands expose only fixed, typed operations.
pub mod m5_delivery;
// Shared runtime lifecycle contract for Ollama, managed llama.cpp, and later
// platform-gated adapters. The core is Tauri-free so daemon/API/desktop use
// the same validation, cancellation, residency, and scheduling semantics.
pub mod runtime_adapter;
// Verified app-owned llama.cpp runtime shared by desktop, CLI, and M3.
// Model-agnostic image and video generation over the managed
// stable-diffusion.cpp runtime. Tauri-free so the CLI can share it.
pub mod generation;
mod generation_commands;
pub mod studio_tools;
// The two generation backends the app talks to but never ships: a ComfyUI the
// user installed, and hosted OpenAI-compatible image APIs. HTTP only.
mod generation_remote;
pub mod managed_runtime;
// K22: the startup self-integrity check every native launch path consults, and
// the one-step rollback the in-app updater takes before it replaces the install.
pub mod self_integrity;
pub mod update_rollback;
// The stack registry and the embedding path, shared by v1 Knowledge Stacks
// (`stacks`) and Knowledge 2.0 (`knowledge_service`/`knowledge_pipeline`).
// Extracted out of `stacks` so that nothing shared lives in the module the v1â†’v2
// collapse is going to delete. v2's call sites still reach it *through* `stacks`'s
// re-export, so the dependency is broken structurally and not yet in fact â€” see
// this module's own doc for the repointing that step needs.
pub mod knowledge_core;
// Knowledge Stacks 2.0 contracts and generation-based hybrid index. Kept
// Tauri-free so desktop, daemon, CLI workflows, and connector packages share
// the same hostile-input and citation semantics.
pub mod knowledge_pipeline;
// Bounded, inert local/Office/PDF/HTML extraction adapters for the v2
// Knowledge pipeline. Kept Tauri-free so daemon refresh and desktop indexing
// use exactly the same parsers and hostile-container checks.
pub mod knowledge_adapters;
// Persistent connector refresh, immutable generation publication, hybrid
// retrieval/inspection, and privacy-preview commands for Knowledge Stacks 2.
pub mod knowledge_service;
// Declarative marketplace, opaque MCP App/OAuth boundary, and versioned
// workflow DAG services. Their cores are Tauri-free for CLI/daemon parity;
// m4_commands is the thin desktop bridge.
pub mod m4_commands;
pub mod m4_runtime;
pub mod m4_services;
pub mod mcp_app_core;
mod native_skill_commands;
pub mod native_skills;
// Tauri-free Modelfile parser/validator/format-sniffer backing "Modelfile
// Studio" (Phase 8): real Ollama Modelfile grammar, short-name hardening,
// and GGUF/safetensors header sanity checks, independent of `ollama.rs`'s
// own `ollama create -f` invocation (which stays in `ollama.rs`, unchanged).
/// Sandboxed third-party WebAssembly components. This is deliberately
/// separate from `package_ecosystem`, whose bundles remain data-only.
pub mod executable_extensions;
pub mod modelfile;
pub mod package_ecosystem;
mod security_commands;
pub mod security_doctor;
pub mod support_bundle;
// Operational-health diagnostics (reachability/liveness of app-owned
// services), sibling to `security_doctor` (which audits posture, not
// health). Self-contained: engine + thin command layer live in one file,
// same convention as `ollama`/`llama`/`server`/`mcp`/`stacks`.
pub mod diagnostics;
pub mod workflow_core;
// Runtime/model hub service plus its thin desktop command layer. The hub
// composes Ollama, managed llama.cpp, MLX, catalog/download, and API policy
// through the shared adapter contracts.
pub mod m3_commands;
pub mod m3_http_server;
pub mod m3_production;
pub mod m3_runtime_hub;
pub mod mlx_ownership;
// Local Agent Integration Launcher (ROADMAP.md, Phase 8, item 13): generates
// safe external-tool (Continue.dev/aider/OpenAI-SDK-compatible) config
// pointed at the M3 HTTP server's real endpoint, and detects drift in a
// previously-generated or pasted config. Tauri-free; command glue lives in
// `m3_commands.rs` alongside the rest of the M3 command surface.
pub mod agent_launcher;
// Model Conversion and Quantization Workbench (ROADMAP.md Phase 8): Tauri-free
// GGUF/safetensors source detection, license risk surfacing, and pluggable
// quantization backends (a real `llama-quantize` shell-out plus an honest
// no-op passthrough fallback), with thin command glue in `m3_commands.rs`.
pub mod quantization;
// Context window / KV-cache observability and long-context failure
// classification (Phase 8, "Context and KV Cache Control Center"). Builds on
// `runtime_adapter`'s settings/offload-planner types rather than duplicating
// them.
pub mod context_cache;
// Runtime Telemetry and Memory Trace Viewer (Phase 8): bounded per-load/
// per-request trace capture, redaction, and support-bundle assembly. Reuses
// `runtime_adapter::OffloadPlan` and `m3_runtime_hub::M3RuntimeHub::runtime_logs`
// rather than computing memory/offload/log data itself; Tauri-free and
// unit-tested on its own, with thin command glue in `m3_commands`.
pub mod runtime_telemetry;
// Explicit-grant desktop companion, local/BYOK speech, and user-owned image
// endpoints. The module owns its media jobs so normal app shutdown can revoke
// every grant and cancel every child/network task before Tauri exits.
pub mod dictation;
pub mod m7_companion;
// Global Command Palette (ROADMAP.md, Phase 1): owns only the OS-level
// shortcut's persisted configuration and "bring the palette to the front"
// action. The palette itself renders inside the main window and dispatches
// every command through the exact same Tauri commands chat/recipes/
// knowledge/permissions already expose â€” see the module doc for why.
pub mod command_palette;
// Safe Desktop Control â€” a design-validation research spike (ROADMAP.md
// Phase 5, "Safe Desktop Control", Status: Research). Off by default,
// never reachable from bypass mode, every action gated behind an explicit
// per-action approval unless the session was started in "approved batch"
// mode, and wired into the same app-exit emergency-stop path as
// `m7_companion`. See `docs/safe-desktop-control-design.md` for the full
// threat model and explicit non-goals.
pub mod desktop_control;
// Apple-Silicon-only MLX lifecycle adapter. It is compiled only into the macOS
// build: MLX needs Metal, so a Windows or Linux binary that carried this module
// would ship an implementation it can never run.
#[cfg(target_os = "macos")]
pub mod mlx_runtime;
// Inbound OpenAI/Anthropic compatibility translations and the scoped,
// authenticated LAN policy shared by the API server and user-owned runners.
mod artifact_commands;
pub mod channels;
pub mod chat_template_lab;
pub mod checkpoints;
pub mod compatibility_hub;
pub mod conformance;
// `pub` only for the doc-comment convention every sibling module below
// follows (a future `monkey-cli` command could call `install_if_needed`
// directly, though none exists yet â€” the CLI installing itself onto its own
// `PATH` isn't a meaningful operation).
pub mod cli_install;
mod git;
// `pub` so `monkey-cli`'s `embed_cli` module (RAG design doc slice 4 CLI parity)
// can reuse `find_llama_server_binary`/`embed_server_args`/`EMBED_PORT`/
// `LlamaState::for_embeddings` directly instead of re-implementing the
// embeddings-only `llama-server` process's binary discovery and flags â€” the
// same AppHandle-free-core reasoning as `stacks`/`checkpoints`/`rules` below,
// just exposing a few specific items rather than a `*_impl` set (the rest of
// this module's Tauri-command surface stays desktop-app-only).
pub mod llama;
pub mod mcp;
// Small, dependency-free stdio MCP servers embedded in the binary via
// `include_str!` and materialized on demand under the app data dir â€” backs
// `McpPanel.tsx`'s "quick add" templates that need a real local file (e.g.
// the bundled AppleScript-control server) rather than an externally
// installed command like `docker`/`npx`.
pub mod bundled_mcp_servers;
// Generic MCP-spec OAuth 2.0 (RFC 8414 discovery, RFC 7591 dynamic client
// registration, PKCE authorization-code flow) for HTTP MCP servers â€” an
// additional, alternative way to obtain `mcp.rs`'s `McpTransport::Http`
// bearer token besides the manual `mcp_set_http_token` paste-a-token path.
// Kept as its own module (rather than growing `mcp.rs` further) since it's
// the one place a future `rmcp` OAuth API change would need editing.
pub mod mcp_oauth;
// Brokered OAuth for MCP servers whose provider requires a confidential
// client (a `client_secret`) â€” Slack and Google, confirmed via their own
// docs to not support `mcp_oauth.rs`'s dynamic-client-registration/loopback
// flow. A Cloudflare Worker (little-monkey-website/worker/) holds the
// secret and exchanges the code server-side; this module only ever sees the
// resulting access/refresh tokens, via a `littlemonkey://` deep link. See
// newApp/.claude/plans/linear-moseying-wolf.md for the full design.
pub mod hosted_oauth;
// Connector Catalog: guided GitHub (via `gh` CLI)/Slack/Notion/Jira/S3
// connections, verified live before saving, secrets in the OS keychain only.
// AppHandle-free core (bar the `AppState` config lock), same *_impl split as
// `mcp`/`providers` above.
pub mod connectors;
// Inbox Triage Agents (ROADMAP.md, Phase 3): read-only ranking/summarization
// of GitHub/Slack/Jira work queues built on the Connector Catalog above, plus
// draft-only reply/comment/status-update generation. Every write goes through
// `permissions::request_permission`, same as every other mutating tool.
pub mod model_sources;
// `pub` for `curated_models()` alone â€” monkey-cli's launcher offers the same
// curated recommendations the desktop model tab does. Everything else in here
// is Tauri commands the CLI can't call anyway.
pub mod models;
pub mod ollama;
mod process_lock;
pub mod providers;
pub mod triage;
// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
// Tauri-free static-registry + comparison logic shared by `providers.rs`'s
// cloud-model command and `m3_runtime_hub.rs`'s local-model staleness check.
pub mod model_retirement;
// Remote node as a scheduled device (roadmap K17): the placement plane's wire
// shapes and its pure decisions â€” which node qualifies, which one wins, whether
// a node is still alive, and what a vanished node means for the work placed on
// it. Tauri-free and I/O-free so the daemon binary, the CLI controller, and the
// desktop all read the same contract, and so every decision is testable without
// the second machine the rest of K17 genuinely needs.
pub mod node_placement;
// `pub` so `monkey-cli`'s `Stacks` subcommand (RAG design doc, slice 4) can call
// `stacks::query_stacks` directly, the same AppHandle-free-core reasoning as
// `checkpoints`/`rules`/`memory`. The registry half it also needs comes from
// `knowledge_core`.
pub mod stacks;
// `pub` so `monkey-cli` (slice 4) can reuse `load_impl`/`PromptEntry` directly,
// the same reasoning as `rules`/`checkpoints` above.
pub mod prompts;
// Local revision history (diff/restore/branch/compare) for everything the user
// authors â€” personas, snippets, skills, workflow definitions (roadmap K24 /
// ROADMAP #3). `pub` for the same reason as `prompts`: `WorkflowService` and
// `monkey-cli` record into it without an `AppHandle`.
pub mod config_revisions;
// The evidence-backed learning loop over the native `SKILL.md` runtime. `pub`
// for the same reason as `native_skills` itself: `monkey-cli`'s
// `skills learned` subcommands drive the identical store, so the desktop and
// the CLI cannot drift into two different learning behaviours.
mod login_path;
mod sessions;
pub mod skill_activation;
mod skill_activation_commands;
pub mod skill_learning;
mod skill_learning_commands;
mod system;
mod terminal;
mod tools;
// Long-running agent shell commands that outlive the turn that started them
// (`run_shell` with `run_in_background: true`) â€” see `background_shell.rs`.
pub mod background_shell;
// The process-table lifecycle every bounded agent-controlled execution shares:
// the verify runner, the hook runner and the sandbox run, which were bounded by
// a resource controller long before any of them had a row.
pub mod bounded_execution;
// What a new app session may conclude about native work an old one left behind â€”
// and, crucially, what it may not.
pub mod orphan_reclaim;
// The one Windows spawn ordering: suspended, assigned, verified, resumed â€” so no
// agent-controlled workload runs an instruction before its job holds it.
pub mod managed_spawn_windows;
// Real OS suspend/resume of a process group this app owns, shared by the
// daemon's job runner and by `background_shell.rs`.
pub mod os_signal;
// Kernel-enforced ceilings on a spawned child (`setrlimit` via `pre_exec`), as
// opposed to `os_signal`'s cooperative teardown. Sits beside it because the two
// are the same concern from opposite ends: one bounds a child the kernel holds to
// it, the other ends one this app is still watching.
pub mod os_limits;
// One bound on how much captured subprocess output may reach a model. Shared by
// `tools.rs` and `verify.rs` so the two command runners cannot drift on the
// number, the truncation direction, or the marker.
pub mod output_cap;
// Shared outbound-network primitives. Small on purpose: four independent SSRF
// guards already exist, and this holds only the narrow rules all four need to
// agree on. See `egress.rs` for why unifying their blocklists is a separate,
// riskier change.
pub mod denial_sink;
pub mod egress;
// The run a piece of work belongs to, carried as a `tokio::task_local!` rather
// than threaded through signatures that have no other reason to hold it. See
// `run_scope.rs` for why a thread-local would be a correctness bug here and not
// merely a lossy shortcut.
pub mod run_scope;
// `pub` (unlike `sessions`/`tools`/`system`/`models`/`git`/`llama` above) so
// `monkey-cli` (Plan/Act + risk-adaptive permissions design doc, phase 4) can
// call `permissions::path_risk_floor` directly for its own floor-only
// `"smart"` mode â€” the same AppHandle-free-core reasoning as `web`/`rules`/
// `memory`/`verify` above. Every other item in this module (the Tauri
// commands, `PermissionState`, `request_permission`) stays reachable too,
// but is only ever actually called from `main.rs`'s own Tauri app wiring,
// not from `monkey-cli`.
pub mod permissions;
// Peer-to-peer envelopes between two paired installations: pure types and the
// loop/bound/expiry rules, shared so the desktop can describe a peer exchange
// without the daemon's protocol module.
pub mod peers;
// `pub` (not `mod`, like `sessions`/`tools`/`system` above) so `monkey-cli`
// (slice 5) can call `read_rules_impl`/`load_impl`/`add_fact_impl` directly
// from `little_monkey_lib`, the same way it already reuses `checkpoints`.
pub mod hooks;
pub mod memory;
pub mod rules;
pub mod workspace;
// `pub` so `monkey-cli` (phase 4) can call `web::fetch_impl` directly, the same
// AppHandle-free-core reasoning as `checkpoints`/`rules`/`memory` above.
pub mod web;
// `pub` so a future `monkey-cli` `api-serve` subcommand (design doc phase 4) can
// call `server::handle_request` directly for headless use, the same
// AppHandle-free-core reasoning as `web`/`rules`/`memory` above.
pub mod server;
// `pub` so `monkey-cli` (a later slice) can call `verify::run_command_impl`
// directly, the same AppHandle-free-core reasoning as `web`/`rules`/`memory`
// above.
pub mod verify;
// `pub` so `monkey-cli`'s `task.rs` (design doc slice 1) can call
// `parse_recipe`/`render_recipe`/`discover_recipes`/`resolve_recipe`
// directly, the same AppHandle-free-core reasoning as every other module
// above.
pub mod recipes;
// Platform-neutral execution contracts shared by every current and future
// client. Keep this module free of Tauri types so the desktop app, CLI, ACP
// bridge, daemon, workflows, and user-owned remote runners serialize the same
// immutable run snapshots and append-only events.
pub mod run_protocol;
// Transactional SQLite ledger for ordered run events, durable approvals,
// idempotency, leases, triggers, and the migration-controlled profile schema.
// Like the protocol module, this remains reusable by non-Tauri clients.
pub mod run_ledger;
// The portable form of K13's frozen process image, plus the target node's
// admission decision over it (roadmap K18). Kept out of `checkpoints` because
// nothing here needs an AppHandle and both halves of a migration â€” the desktop
// that freezes and the daemon's remote runner that receives â€” have to build the
// same types.
pub mod migration;
// The one place a run-less subsystem writes to the unified event stream
// (`run_ledger`'s `subsystem_events`). Public because the three writing contexts
// â€” desktop, a process that owns its data directory, and a disabled test â€” do
// not all live in this crate's Tauri half.
pub mod subsystem_audit;
// The one process abstraction shared by every execution surface â€” desktop
// turns, daemon jobs, subagents, crew members, workflow runs/nodes, remote
// runs, background shells, side tasks. Public for the same reason the two
// modules above are: the CLI's `monkey ps` and the daemon both read it, and
// neither should grow a second copy of the state machine.
mod process_commands;
pub mod process_table;
// The measuring half of the process table's resource ledger: what one OS pid
// actually cost, plus an explicit reason for every field this platform will not
// report. Separate from `process_table` because it is all platform syscalls and
// no storage.
pub mod process_usage;
// The tree half of the same question: what does the workload rooted at this pid
// hold, and how many processes are in it. Reads the kernel's own process table
// rather than forking `ps`, so it can follow parent links a process group loses.
pub mod process_tree;
// The one contract every native child-process owner installs its limits
// through: capability, preparation, attachment, sampling and tree termination.
pub mod resource_control;
// The two kernel-held backends behind that contract. Gated per OS rather than
// stubbed: a host that cannot hold a bound must not compile a module that
// claims to.
#[cfg(target_os = "linux")]
pub mod resource_control_cgroup;
#[cfg(windows)]
pub mod resource_control_job;
// Policy shared by the two HTTP listeners, which default to the same port and
// today report a bare "address already in use" naming neither the winner nor
// the reason. Where the shared pieces accumulate as D1 collapses them into one.
pub mod http_policy;
// Pure, AppHandle-free allowlist and dispatch matrix shared by the legacy and
// M3 HTTP implementations while D1 collapses them into one listener.
pub mod http_route_registry;
// The agent tool schemas. In the library rather than beside the agent loop
// because `contract` generates the published tool contract from them.
pub mod agent_tools;
// The published, semver'd syscall ABI (K19), generated from the route table,
// the remote plane's dispatch, the ACP methods and `agent_tools`.
pub mod contract;
// Pure union model catalog used by the same D1 HTTP merge.
pub mod http_model_catalog;
pub mod http_model_service;
pub mod http_model_sources;
// One lifecycle/endpoint plan for the primary loopback and optional LAN HTTP
// surfaces. Multiple sockets remain one service and one admission domain.
pub mod unified_http_server;
// Migration-controlled authoritative profile/session/search storage. Kept
// reusable by the desktop, CLI, daemon, export/import, and restore paths.
pub mod portability;
// The resident daemon reuses the exact keychain-backed snapshot/WebDAV
// implementation exposed to Tauri. Keeping this module public prevents the
// CLI service from growing a second encryption, credential, or conflict path.
pub mod portability_commands;
mod profile_commands;
// K23's identity boundary. `pub` because `monkey-cli` and the daemon resolve
// the same active profile through it â€” a second copy of the resolution rule is
// a second answer to "whose data is this".
pub mod profiles;
use crate::profiles::ProfileScopedPaths;
pub mod profile_store;
mod run_commands;
// `pub` so a future `monkey-cli` `task schedule` subcommand could reuse
// `validate_cron_impl`/`next_occurrences_impl` directly, the same
// AppHandle-free-core reasoning as every other module above â€” no such
// subcommand exists yet (it emits a launchd/crontab line, no in-process
// scheduling), but there's no reason to make this one module-private either.
pub mod automations;
// Visible per-workspace data boundary in front of outbound sends to a cloud
// model: reuses `knowledge_pipeline::SensitiveDataScanner` for detection and
// adds only a persisted policy and the two-phase (`RequireApproval`)
// confirm-then-send commands. Kept destination-agnostic (see its own module
// doc) so a future connector/MCP-result/paired-device call site is additive.
pub mod privacy_firewall;
// Disposable-workspace-copy command execution: risky commands/tests run
// against `<app_data>/sandbox-runs/<run_id>/workspace` instead of the real
// workspace, with a restricted env, a wall-clock timeout, and (on macOS) a
// generated Seatbelt profile. Nothing reaches the real workspace except
// through the module's own explicit prepare-digest/confirm-phrase promote
// action. Reuses `run_protocol`/`run_ledger` for run modeling exactly like
// every other execution surface above.
pub mod sandbox;
// The agent shell's live-workspace policy, layered on the same Seatbelt,
// Landlock/seccomp and AppContainer/job primitives as disposable sandboxes.
// Public only for monkey-cli's AppHandle-free run_shell path.
pub mod workspace_shell;
// The Linux half of `sandbox`'s OS boundary (ROADMAP.md item K3): a Landlock
// filesystem ruleset plus a seccomp-BPF network filter, installed in `pre_exec`
// alongside `os_limits`. Linux-only because the crates behind it are, and
// because a stub would just be a second place to claim confinement from.
#[cfg(target_os = "linux")]
pub mod sandbox_linux;
// The Windows half: AppContainer filesystem/network confinement plus a job
// object for the process tree, memory and window-station reach. It owns the raw
// CreateProcess path needed to apply both at spawn.
#[cfg(target_os = "windows")]
pub mod sandbox_windows;
// Local, single-machine "Team, Family, and Organization Mode" (ROADMAP.md
// Phase 6): a named local profile switcher, capability-checked roles, and a
// redacted audit export layered over `run_ledger`/`permissions`. See the
// module doc for exactly what it is (and, just as importantly, is not).
pub mod team_mode;
// Issue-to-PR Agent Flow (ROADMAP.md Phase 3): orchestrates picking up a
// GitHub issue and carrying it through a reviewable owned-branch/PR loop on
// top of the `m5_delivery` GitHub/worktree primitives.
pub mod issue_to_pr;
// Runtime PR Watcher and Capability Feed (ROADMAP.md Phase 8, last item):
// fetches closed `ollama/ollama` PRs over the public GitHub REST API,
// classifies which ones plausibly touch Little Monkey's own runtime surface
// with a keyword heuristic, and persists a monthly-cadence report of newly
// relevant upstream changes with a suggested action each. Self-contained,
// same convention as `diagnostics`/`automations`/`privacy_firewall` above;
// see the module doc for the on-demand-vs-scheduled scope decision.
pub mod runtime_pr_watcher;
// Human Approval Chains (ROADMAP.md Phase 3): multi-step approval workflows
// (a sequence of stages, each with its own timeout/escalation) layered on top
// of `permissions.rs`'s existing single-shot request/response system. A new,
// independent state machine â€” see the module doc for why it isn't an
// extension of `PermissionState`.
pub mod approval_chains;
pub mod local_apps;

// Shared unit-test fixtures. Chiefly the mock app every test must build
// through, so no two of them share an app-data directory (or a run ledger).
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod extension_capability_tests;

#[cfg(test)]
mod programmatic_tool_e2e;

// `Manager` brings `AppHandle::state`/`state::<T>()` into scope â€” used by
// `run()`'s `RunEvent::Exit` handler below to reach `AppState::mcp` for
// `mcp::disconnect_all`.
use tauri::Manager;

/// Shared application state, managed by Tauri and accessed from every
/// #[tauri::command] via `tauri::State<'_, AppState>`.
///
/// No `#[derive(Default)]`: `embed_llama` needs a different starting port
/// (8091) than `llama`'s (8090), which a derived `Default` (calling
/// `LlamaState::default()` for both) can't express. See the manual `impl
/// Default for AppState` below.
pub struct AppState {
    /// The one `sd-server` behind Studio's image and video generation. Its
    /// weight set is bound at launch, so switching models restarts it; see
    /// `generation::GenerationEngineState`.
    pub generation_engine: generation::GenerationEngineState,
    /// The one running Studio tool sidecar â€” a face swapper, a detector, a
    /// segmenter. Not diffusion and not the engine: a separate program
    /// speaking `studio_tools`' small HTTP contract.
    pub studio_tool: studio_tools::StudioToolState,
    pub llama: std::sync::Mutex<llama::LlamaState>,
    /// The second, embeddings-only managed `llama-server` instance (port
    /// 8091, started with `--embeddings --pooling mean`) used by
    /// `stacks.rs`'s managed-llama embedding backend â€” a distinct
    /// `LlamaState` from `llama` above (not one process serving both
    /// roles), so a stack reindex never contends with the chat model for
    /// the same server slot. See `llama::embed_server_start`.
    pub embed_llama: std::sync::Mutex<llama::LlamaState>,
    pub ollama: std::sync::Mutex<ollama::OllamaState>,
    /// In-flight `ollama pull`/`ollama create` child processes, keyed by tag
    /// (or short name) â€” lets `ollama::ollama_cancel_pull` kill a pull the
    /// user started. See `ollama::ollama_pull_model`.
    pub ollama_pulls: std::sync::Mutex<std::collections::HashMap<String, tokio::process::Child>>,
    /// Cancellation handles for in-flight `models_download` calls, keyed by
    /// the destination file name â€” mirrors `index_cancels`/`ollama_pulls`,
    /// but a `CancellationToken` rather than a killable child process since
    /// the download is an in-process `reqwest` stream. See
    /// `models::models_cancel_download`.
    pub model_downloads: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<tokio_util::sync::CancellationToken>>,
    >,
    /// Attached workspace folders, primary first. Empty means no workspace
    /// is open. See `workspace.rs`.
    pub workspace_roots: std::sync::Mutex<Vec<workspace::WorkspaceRoot>>,
    /// Real PTY-backed interactive terminal tabs. Process ownership is kept
    /// in Rust so every WebView observes one lifecycle and workspace changes
    /// can terminate shells before their roots are detached.
    pub terminal: terminal::TerminalManager,
    /// Background agent shell commands (`run_shell` with
    /// `run_in_background: true`). Owned here rather than by the turn that
    /// spawned them â€” that is what lets a dev server or watcher keep running
    /// after the tool call returns, and what gives `shell_output`/`shell_kill`
    /// something to address later. Killed on app shutdown.
    pub background_shell: background_shell::BackgroundShellManager,
    pub permissions: permissions::PermissionState,
    /// Cancellation handles for in-flight `providers_stream_chat` requests,
    /// keyed by `request_id` â€” see `providers::providers_cancel_chat`.
    pub stream_cancels:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
    /// Per-turn file checkpoints currently in flight, keyed by checkpoint id.
    /// With the split pane open, two turns (and thus two checkpoints) can be
    /// active concurrently â€” see `checkpoints.rs`.
    pub checkpoints:
        std::sync::Mutex<std::collections::HashMap<String, checkpoints::ActiveCheckpoint>>,
    /// Checkpoint ids with a `checkpoint_revert`/`checkpoint_reapply` call
    /// currently in progress. `MessageList.tsx`'s `CheckpointRow` and
    /// `CheckpointTimeline.tsx`'s `TimelineRow` can both render controls for
    /// the same checkpoint at once, and both call these commands with only a
    /// component-local `busy` flag guarding each â€” nothing shared prevents
    /// two concurrent revert/reapply calls for the same id from racing on
    /// the same `redo/<n>.bak` files. Membership here is that lock â€” see
    /// `checkpoints::acquire_revert_lock`.
    pub checkpoint_locks: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Per-turn cancellation channels used by `tools::tools_cancel_running`
    /// to kill in-flight `tool_run_shell` child processes when the user hits
    /// Stop â€” keyed by the owning turn's id (empty string for callers that
    /// don't thread one) so stopping one pane's turn never kills a command
    /// the other pane's turn is still running.
    pub tool_cancel:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
    /// Process-group ids of the foreground `tool_run_shell` children each turn
    /// currently owns, keyed the same way `tool_cancel` is (the owning turn's
    /// id, empty string for callers that don't thread one).
    ///
    /// This is what makes suspending a chat turn honest. The turn's own
    /// cooperative pause only lands at the loop's next safe point, so a
    /// twenty-minute `run_shell` would otherwise keep burning CPU for twenty
    /// minutes after the user asked for a pause. Suspending the turn SIGSTOPs
    /// these groups immediately, and resuming SIGCONTs them â€” see
    /// `process_commands::process_signal`. Entries are removed the moment the
    /// child is reaped, so a pid is never signalled after the kernel could
    /// have reused it.
    pub shell_process_groups:
        std::sync::Mutex<std::collections::HashMap<String, tools::TurnShellGroups>>,
    /// Serializes `memories.json` read-modify-write cycles (see `memory.rs`)
    /// so two concurrent split-pane `tool_remember` calls can never race and
    /// clobber each other's fact â€” the whole file is rewritten on every add
    /// or delete, so unsynchronized concurrent writers could silently drop
    /// one of them.
    pub memory_lock: std::sync::Mutex<()>,
    /// Serializes `mcp_servers.json` read-modify-write cycles (see `mcp.rs`)
    /// â€” same reasoning as `memory_lock` above protects `memories.json`.
    /// `mcp_add_server`/`mcp_update_server` are synchronous commands (Tauri
    /// can dispatch those on genuinely concurrent OS threads) and
    /// `mcp_remove_server`/`mcp_set_enabled` are async commands (the tokio
    /// runtime can run those in parallel too), so without a shared lock two
    /// concurrent config-mutating calls (e.g. two Settings toggles fired
    /// close together) can both load the same "before" config and the
    /// later save silently clobbers the earlier one's change. A plain
    /// `std::sync::Mutex`, not `tokio::sync::Mutex` like `AppState::mcp`:
    /// every critical section this guards is the synchronous
    /// `loaduÛŞô¶‰ËkºwµçB6W'fW#£¦•÷6W'fW%övWEö6öæf–rÀ¢6W'fW#£¦•÷6W'fW%÷6WEö6öæf–rÀ¢6W'fW#£¦•÷6W'fW%ö7&VFU÷Fö¶VâÀ¢6W'fW#£¦•÷6W'fW%÷&Wfö¶U÷Fö¶VâÀ¢6W'fW#£¦•÷6W'fW%öÆ—7E÷Fö¶Vç2À¢6W'fW#£¦•÷6W'fW%öW‡÷'EöVF—BÀ¢öÆÆÖ£¦öÆÆÖ÷7FGW2À¢öÆÆÖ£¦öÆÆÖ÷7F'BÀ¢öÆÆÖ£¦öÆÆÖöÆ—7EöÖöFVÇ2À¢öÆÆÖ£¦öÆÆÖöÆ—7E÷'Vææ–æuöÖöFVÇ2À¢öÆÆÖ£¦öÆÆÖ÷VæÆöEöÖöFVÂÀ¢öÆÆÖ£¦öÆÆÖöW†×ÆUö6Æ÷VE÷Fw2À¢öÆÆÖ£¦öÆÆÖ÷VÆÅöÖöFVÂÀ¢öÆÆÖ£¦öÆÆÖö6æ6VÅ÷VÆÂÀ¢öÆÆÖ£¦öÆÆÖö–×÷'EöÖöFVÂÀ¢öÆÆÖ£¦öÆÆÖö7&VFUög&öÕöÖöFVÆf–ÆRÀ¢öÆÆÖ£¦öÆÆÖ÷&VÖ÷fUöÖöFVÂÀ¢öÆÆÖ£¦öÆÆÖ÷6–væ–âÀ¢ÖöFVÆf–ÆS£¦ÖöFVÆf–ÆU÷'6RÀ¢ÖöFVÆf–ÆS£¦ÖöFVÆf–ÆUöG'•÷'VâÀ¢ÖöFVÆf–ÆS£¦ÖöFVÆf–ÆU÷&VE÷FW‡Eöf–ÆRÀ¢6öææV7F÷'3£¦6öææV7F÷'5öÆ—7BÀ¢6öææV7F÷'3£¦6öææV7F÷'5öFEöv—F‡V"À¢6öææV7F÷'3£¦6öææV7F÷'5öFE÷Fö¶VâÀ¢6öææV7F÷'3£¦6öææV7F÷'5öFE÷32À¢6öææV7F÷'3£¦6öææV7F÷'5öFEöW‡FVç6–öâÀ¢6öææV7F÷'3£¦6öææV7F÷'5öÆ—7EöW‡FVç6–öåö÷F–öç2À¢6öææV7F÷'3£¦6öææV7F÷'5÷&VÖ÷fRÀ¢6öææV7F÷'3£¦6öææV7F÷'5÷&WfW&–g’À¢6öææV7F÷'3£¦6öææV7F÷'5öW‡÷'EöVF—BÀ¢G&–vS£§G&–vU÷&Vg&W6‚À¢G&–vS£§G&–vUöÆ—7BÀ¢G&–vS£§G&–vUövVæW&FUöG&gBÀ¢G&–vS£§G&–vU÷6VæEöG&gBÀ¢&÷f–FW'3£§&÷f–FW'5öÆ—7Eö6öæf–wW&VBÀ¢&÷f–FW'3£§&÷f–FW'5öFEö7W7FöÒÀ¢&÷f–FW'3£§&÷f–FW'5÷&VÖ÷fUö7W7FöÒÀ¢&÷f–FW'3£§&÷f–FW'5÷6WEö¶W’À¢&÷f–FW'3£§&÷f–FW'5÷&VÖ÷fUö¶W’À¢&÷f–FW'3£§&÷f–FW'5öÆ—7EöÖöFVÇ2À¢&÷f–FW'3£§&÷f–FW'5ö6†V6µöÖöFVÅ÷&WF—&VÖVçG2À¢&÷f–FW'3£§&÷f–FW'5÷7G&VÕö6†BÀ¢&÷f–FW'3£§&÷f–FW'5ö6æ6VÅö6†BÀ¢ÖöFVÇ3£¦ÖöFVÇ5öÆ—7Eö7W&FVBÀ¢ÖöFVÇ3£¦ÖöFVÇ5öÆ—7Eö–ç7FÆÆVBÀ¢ÖöFVÇ3£¦ÖöFVÇ5öF÷væÆöBÀ¢ÖöFVÇ3£¦ÖöFVÇ5ö6æ6VÅöF÷væÆöBÀ¢ÖöFVÇ3£¦ÖöFVÇ5÷&W6öÇfU÷&VfW&Væ6RÀ¢ÖöFVÇ3£¦ÖöFVÇ5ö–ç7FÆÅ÷&VfW&Væ6RÀ¢ÖöFVÇ3£¦ÖöFVÇ5öFVÆWFRÀ¢ÖöFVÇ3£¦ÖöFVÇ5öFEöW‡FW&æÂÀ¢ÖöFVÇ3£¦ÖöFVÇ5÷&VÖ÷fUöW‡FW&æÂÀ¢ÖöFVÇ3£¦ÖöFVÇ5öFWFV7E÷&ö¦V7F÷'2À¢ÖöFVÇ3£¦ÖöFVÇ5÷6WE÷&ö¦V7F÷"À¢ÖöFVÇ3£¦ÖöFVÇ5÷&VÖ÷fU÷&ö¦V7F÷"À¢&ö6W75ö6öÖÖæG3£§&ö6W75öÆ—7BÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75övWBÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75öFW66VæFçG2À¢&ö6W75ö6öÖÖæG3£§&ö6W75öÆ—fUö6÷VçG2À¢&ö6W75ö6öÖÖæG3£§&ö6W75öFÖ—BÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷&V6öæ6–ÆRÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷6–væÂÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷6–væÅ÷7W÷'BÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷VæF–æu÷6–væÇ2À¢&ö6W75ö6öÖÖæG3£§&ö6W75öFVÆ—fW%ö÷5÷6–væÂÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷G&ç6—F–öâÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75öÆ–æµ÷'VâÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷&VöÖ—76–ærÀ¢&ö6W75ö6öÖÖæG3£§&ö6W75÷W6vUöÆVFvW"À¢&ö6W75ö6öÖÖæG3£§&ö6W75÷&W6÷W&6U÷&W÷'BÀ¢W&Ö—76–öç3£§W&Ö—76–öå÷&W7öæBÀ¢W&Ö—76–öç3£§W&Ö—76–öåöG'•÷'VâÀ¢W&Ö—76–öç3£§6WE÷W&Ö—76–öåöÖöFRÀ¢W&Ö—76–öç3£§6WE÷W&Ö—76–öåöÖöFUöf÷%÷GW&âÀ¢W&Ö—76–öç3£¦6ÆV%÷W&Ö—76–öåöÖöFUöf÷%÷GW&âÀ¢FW&Ö–æÃ£§FW&Ö–æÅö–FVçF—G’À¢FW&Ö–æÃ£§FW&Ö–æÅö7&VFRÀ¢FW&Ö–æÃ£§FW&Ö–æÅöÆ—7BÀ¢FW&Ö–æÃ£§FW&Ö–æÅöW†V7WFRÀ¢FW&Ö–æÃ£§FW&Ö–æÅ÷w&—FRÀ¢FW&Ö–æÃ£§FW&Ö–æÅö–çFW''WBÀ¢FW&Ö–æÃ£§FW&Ö–æÅ÷&W6—¦RÀ¢FW&Ö–æÃ£§FW&Ö–æÅö¶–ÆÂÀ¢FW&Ö–æÃ£§FW&Ö–æÅ÷&W7F'BÀ¢FW&Ö–æÃ£§FW&Ö–æÅö6Æ÷6RÀ¢FW&Ö–æÃ£§FW&Ö–æÅö†—7F÷'’À¢FööÇ3£§FööÅ÷&VEöf–ÆRÀ¢FööÇ3£§FööÅöÆ—7EöF—"À¢FööÇ3£§FööÅöw&WÀ¢FööÇ3£§FööÅövÆö"À¢FööÇ3£§FööÅ÷w&—FUöf–ÆRÀ¢FööÇ3£§FööÅöVF—Eöf–ÆRÀ¢FööÇ3£§FööÅövVæW&FUö–ÖvRÀ¢FööÇ3£§v÷&·76U÷&VEö–ÖvRÀ¢FööÇ3£§FööÅ÷'Vå÷6†VÆÂÀ¢&6¶w&÷VæE÷6†VÆÃ£§FööÅ÷'Vå÷6†VÆÅö&6¶w&÷VæBÀ¢&6¶w&÷VæE÷6†VÆÃ£¦&6¶w&÷VæE÷6†VÆÅö÷WGWBÀ¢&6¶w&÷VæE÷6†VÆÃ£¦&6¶w&÷VæE÷6†VÆÅö¶–ÆÂÀ¢&6¶w&÷VæE÷6†VÆÃ£¦&6¶w&÷VæE÷6†VÆÅöÆ—7BÀ¢&6¶w&÷VæE÷6†VÆÃ£¦&6¶w&÷VæE÷6†VÆÅö6ÆV%öf–æ—6†VBÀ¢FööÇ3£§FööÇ5ö6æ6VÅ÷'Vææ–ærÀ¢FööÇ3£¦Æ—7E÷v÷&·76U÷F‡2À¢FööÇ3£§FööÅ÷&VÖVÖ&W"À¢FööÇ3£§FööÅ÷&VE÷6¶–ÆÅ÷&W6÷W&6RÀ¢FööÇ3£§FööÅöÖævU÷6¶–ÆÅöÆV&æ–ærÀ¢vV#£§FööÅ÷vV%öfWF6‚À¢vV#£§FööÅ÷vV%÷6V&6‚À¢vV#£§vV%övWE÷6WGF–æw2À¢vV#£§vV%÷6WE÷6WGF–æw2À¢vV#£§vV%ö†5ö'&fUö¶W’À¢vV#£§vV%÷6WEö'&fUö¶W’À¢vV#£§vV%÷&VÖ÷fUö'&fUö¶W’À¢'VÆW3£§'VÆW5÷&VBÀ¢'VÆW3£§'VÆW5÷w&—FRÀ¢'VÆW3£§'VÆW5ö7W'&VçE÷&Wf—6–öâÀ¢'VÆW3£§'VÆW5÷&Wf—6–öåöVçF—G’À¢†öö·3£¦†öö·5öÆöBÀ¢†öö·3£¦†öö·5÷6fRÀ¢†öö·3£¦†ööµöW†V2À¢ÖVÖ÷'“£¦ÖVÖ÷'•öÆ—7BÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•öFBÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•öFVÆWFRÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•÷WFFRÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•ö6ÆV"À¢ÖVÖ÷'“£¦ÖVÖ÷'•öÆ—7EöÆÂÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•÷7GVF–õ÷WFFRÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•÷7GVF–õ÷6WEöVæ&ÆVBÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•÷7GVF–õöFVÆWFRÀ¢ÖVÖ÷'“£¦ÖVÖ÷'•ö–×÷'BÀ¢6W76–öç3£§6W76–öç5öÆöBÀ¢6W76–öç3£§6W76–öç5÷6fRÀ¢&öf–ÆW3£§&öf–ÆW5öÆ—7BÀ¢&öf–ÆW3£§&öf–ÆW5ö7&VFRÀ¢&öf–ÆW3£§&öf–ÆW5÷&VæÖRÀ¢&öf–ÆW3£§&öf–ÆW5÷6WEöÆ–Ö—G2À¢&öf–ÆW3£§&öf–ÆW5öFVÆWFRÀ¢&öf–ÆW3£§&öf–ÆW5÷7v—F6‚À¢&öf–ÆUö6öÖÖæG3£§&öf–ÆUöÖ–w&F–öå÷7FGW2À¢&öf–ÆUö6öÖÖæG3£§&öf–ÆUöÖ–w&FRÀ¢&öf–ÆUö6öÖÖæG3£§&öf–ÆUövÆö&Å÷6V&6‚À¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆUöW‡÷'Eö'VæFÆRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷&VEö'VæFÆRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷&W7F÷&UöÇ’À¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷&W7F÷&U÷6WGF–æw5÷VæF–ærÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷&W7F÷&U÷6WGF–æw5ö6¶æ÷vÆVFvRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆUöW‡÷'E÷6W76–öâÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷6æ6†÷Eö7&VFRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷6æ6†÷EöÆ—7BÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷6æ6†÷Eö÷VâÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷6æ6†÷E÷7FvU÷6÷W&6RÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷vV&Feö6öæf–u÷6fRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷vV&Fe÷7FGW5övWBÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷vV&Fe÷'VåöGVRÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷vV&Fe÷FW7BÀ¢÷'F&–Æ—G•ö6öÖÖæG3£§÷'F&ÆU÷vV&FeöF÷væÆöE÷6æ6†÷BÀ¢&ö×G3£§&ö×G5öÆöBÀ¢&ö×G3£§&ö×G5÷6fRÀ¢&ö×G3£§&ö×G5÷&VEöW‡FW&æÂÀ¢&ö×G3£§&ö×G5÷w&—FUöW‡FW&æÂÀ¢&ö×G3£§&ö×G5ö7W'&VçE÷&Wf—6–öâÀ¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5÷&V6÷&BÀ¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5ö†—7F÷'’À¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5övWBÀ¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5ö†VBÀ¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5ö'&æ6‚À¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5ö'&æ6†W2À¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5öVçF—F–W2À¢6öæf–u÷&Wf—6–öç3£¦6öæf–u÷&Wf—6–öç5ö6†ævW2À¢6†V6·ö–çG3£¦6†V6·ö–çEö&Vv–âÀ¢6†V6·ö–çG3£¦6†V6·ö–çEöVæBÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷&WfW'BÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷&VÇ’À¢Ó5ö6öÖÖæG3£¦Ó5ö6öçFW‡E÷öÆ–6–W2À¢6†V6·ö–çG3£¦6†V6·ö–çEög&VW¦RÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷7FvVE÷F6µ÷7VvvW7F–öç2À¢6†V6·ö–çG3£¦6†V6·ö–çE÷&V6÷&E÷F6µ÷7VvvW7F–öâÀ¢6†V6·ö–çG3£¦6†V6·ö–çEög&VW¦UöÆ—fRÀ¢6†V6·ö–çG3£¦6†V6·ö–çEö6ÆV%ög&VW¦RÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷&W7F÷&&–Æ—G’À¢6†V6·ö–çG3£¦6†V6·ö–çEöÆ—7BÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷&Wf–WrÀ¢6†V6·ö–çG3£¦6†V6·ö–çEö6ö×&RÀ¢6†V6·ö–çG3£¦6†V6·ö–çE÷6–×VÆFU÷&W7F÷&RÀ¢'F–f7G3£¦'F–f7E÷V&Æ—6‚À¢'F–f7G3£¦'F–f7E÷&VÖ÷fRÀ¢'F–f7Eö6öÖÖæG3£¦'F–f7Eö&Æö%÷&VEö&6ScBÀ¢v÷&·76S£§6WE÷&–Ö'•÷v÷&·76U÷&ö÷BÀ¢v÷&·76S£¦FE÷6V6öæF'•÷v÷&·76U÷&ö÷BÀ¢v÷&·76S£§&VÖ÷fU÷6V6öæF'•÷v÷&·76U÷&ö÷BÀ¢v÷&·76S£¦vWE÷v÷&·76U÷&ö÷G2À¢v÷&·76S£§&W7F÷&U÷v÷&·76U÷&ö÷G2À¢v÷&·76S£¦vWE÷&V6VçE÷v÷&·76W2À¢v—C£¦v—E÷7FGW2À¢v—C£¦v—Eö6öÖÖ—BÀ¢v—C£¦v—E÷&Wf–WrÀ¢v—C£¦v—Eö6†ævVEöf–ÆW2À¢v—C£¦v—Eöf–ÆUöF–fbÀ¢vVçE÷v÷&·G&VW3£§v÷&·G&VUö7&VFRÀ¢vVçE÷v÷&·G&VW3£§v÷&·G&VU÷7FGW2À¢vVçE÷v÷&·G&VW3£§v÷&·G&VU÷&VÖ÷fRÀ¢vVçE÷v÷&·G&VW3£§v÷&·G&VUöÇ’À¢Ö7£¦Ö7öÆ—7E÷6W'fW'2À¢Ö7£¦Ö7öFE÷6W'fW"À¢Ö7£¦Ö7ö7W'&VçE÷&Wf—6–öâÀ¢Ö7£¦Ö7÷&W7F÷&Uö6öæf–rÀ¢Ö7£¦Ö7÷WFFU÷6W'fW"À¢Ö7£¦Ö7÷&VÖ÷fU÷6W'fW"À¢Ö7£¦Ö7÷6WEöVæ&ÆVBÀ¢Ö7£¦Ö7ö6öææV7BÀ¢Ö7£¦Ö7öF—66öææV7BÀ¢Ö7£¦Ö7÷6WEö‡GG÷Fö¶VâÀ¢Ö7£¦Ö7÷&VÖ÷fUö‡GG÷Fö¶VâÀ¢Ö7£¦Ö7ö6ÆÅ÷FööÂÀ¢'VæFÆVEöÖ7÷6W'fW'3£¦Ö7÷7FvUö'VæFÆVE÷6W'fW"À¢Ö7ööWFƒ£¦Ö7ööWF…ö6öææV7BÀ¢Ö7ööWFƒ£¦Ö7ööWF…÷&VF—&V7E÷W&’À¢Ö7ööWFƒ£¦Ö7ööWF…ö6æ6VÂÀ¢Ö7ööWFƒ£¦Ö7ööWF…öF—66öææV7BÀ¢†÷7FVEööWFƒ£¦†÷7FVEööWF…ö6öææV7BÀ¢†÷7FVEööWFƒ£¦†÷7FVEööWF…ö6æ6VÂÀ¢†÷7FVEööWFƒ£¦†÷7FVEööWF…öF—66öææV7BÀ¢7—7FVÓ£§&WfVÅö–åöf–æFW"À¢7—7FVÓ£¦÷Våö–å÷FW&Ö–æÂÀ¢7—7FVÓ£¦÷Våö–åöVF—F÷"À¢7—7FVÓ£¦÷Vå÷6W76–öå÷v–æF÷rÀ¢7—7FVÓ£§7—7FVÕöÖVÖ÷'•ö–æfòÀ¢fW&–g“£§fW&–g•övWEö6öæf–rÀ¢fW&–g“£§fW&–g•÷6WEö6öæf–rÀ¢fW&–g“£§fW&–g•÷'VâÀ¢7F6·3£§7F6·5öÆ—7BÀ¢7F6·3£§7F6·5ö7&VFRÀ¢7F6·3£§7F6·5öFVÆWFRÀ¢7F6·3£§7F6·5÷&VæÖRÀ¢7F6·3£§7F6·5öFE÷6÷W&6RÀ¢7F6·3£§7F6·5÷&VÖ÷fU÷6÷W&6RÀ¢7F6·3£§7F6·5÷VW'’À¢7F6·3£§FööÅ÷6V&6…öFö72À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%öÆ—7E÷6÷W&6W2À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%öFE÷6÷W&6RÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷WFFU÷6÷W&6RÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷&VÖ÷fU÷6÷W&6RÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷&Vg&W6‚À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%ö6æ6VÅ÷&Vg&W6‚À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%ö&6¶w&÷VæEö6öæf–uövWBÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%ö&6¶w&÷VæEö6öæf–u÷6fRÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷WFFUö6‡Væ¶–ærÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%ö—5÷7FÆRÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷VW'’À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%ö6æ6VÅ÷VW'’À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvU÷c%÷–•÷&Wf–WrÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvUöö7%÷7FGW2À¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvUöö7%ö6öæf–wW&UöW‡FW&æÂÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvUöö7%ö–ç7FÆÂÀ¢¶æ÷vÆVFvU÷6W'f–6S£¦¶æ÷vÆVFvUöö7%÷6WEöVæ&ÆVBÀ¢&V6—W3£§&V6—W5öÆ—7BÀ¢&V6—W3£§&V6—W5÷&VBÀ¢&V6—W3£§&V6—W5÷&VE÷&rÀ¢&V6—W3£§&V6—W5÷&VæFW"À¢&V6—W3£§&V6—W5÷6fRÀ¢&V6—W3£§&V6—W5öFVÆWFRÀ¢&V6—W3£§&V6—W5÷fÆ–FFRÀ¢WFöÖF–öç3£¦WFöÖF–öç5öÆöBÀ¢WFöÖF–öç3£¦WFöÖF–öç5÷6fRÀ¢WFöÖF–öç3£¦7&öå÷fÆ–FFRÀ¢WFöÖF–öç3£¦7&öåöæW‡BÀ¢WFöÖF–öç3£¦7&öå÷&Wf–÷W2À¢FVÕöÖöFS£§FVÕöÖVÖ&W'5öÆ—7BÀ¢FVÕöÖöFS£§FVÕöÖVÖ&W'5öFBÀ¢FVÕöÖöFS£§FVÕöÖVÖ&W'5÷WFFU÷&öÆRÀ¢FVÕöÖöFS£§FVÕöÖVÖ&W'5÷&VÖ÷fRÀ¢FVÕöÖöFS£§FVÕöÖVÖ&W'5÷6WEö7F—fRÀ¢FVÕöÖöFS£§FVÕöVF—EöW‡÷'BÀ¢'Våö6öÖÖæG3£§'Vå÷&÷Fö6öÅ÷fW'6–öâÀ¢'Våö6öÖÖæG3£§'Vå÷7V&Ö—BÀ¢'Våö6öÖÖæG3£§'VåöVæEöWfVçBÀ¢'Våö6öÖÖæG3£§'VåöFV6–FU÷W&Ö—76–öâÀ¢'Våö6öÖÖæG3£§'Vå÷&WVW7Eö6æ6VÆÆF–öâÀ¢'Våö6öÖÖæG3£§'VåövWBÀ¢'Våö6öÖÖæG3£§'VåöÆ—7BÀ¢'Våö6öÖÖæG3£§'Våö&6†—fRÀ¢'Våö6öÖÖæG3£§'Vå÷Væ&6†—fRÀ¢'Våö6öÖÖæG3£§'VåöWfVçG2À¢'Våö6öÖÖæG3£§'Våö–çFVw&—G•ö6†V6²À¢6æF&÷ƒ£§6æF&÷…÷'VâÀ¢6æF&÷ƒ£§6æF&÷…öVæf÷&6VÖVçE÷&ö&RÀ¢6æF&÷ƒ£§6æF&÷…öÆ—7BÀ¢6æF&÷ƒ£§6æF&÷…öF–fbÀ¢6æF&÷ƒ£§6æF&÷…÷&W&U÷&öÖ÷FRÀ¢6æF&÷ƒ£§6æF&÷…öW†V7WFU÷&öÖ÷FRÀ¢6æF&÷ƒ£§6æF&÷…öF—66&BÀ¢Ó5ö6öÖÖæG3£¦Ó5ö†&Gv&U÷6æ6†÷BÀ¢Ó5ö6öÖÖæG3£¦Ó5ö†&Gv&U÷&öf–ÆRÀ¢Ó5ö6öÖÖæG3£¦Ó5ö&Væ6†Ö&µ÷'VâÀ¢Ó5ö6öÖÖæG3£¦Ó5ö&Væ6†Ö&µö†—7F÷'’À¢Ó5ö6öÖÖæG3£¦Ó5ö†&Gv&Uö6ö×F–&–Æ—G•÷&W÷'BÀ¢Ó5ö6öÖÖæG3£¦Ó5÷7F÷&vU÷7FGW2À¢Ó5ö6öÖÖæG3£¦Ó5ö–ç7FÆÆVEöÖöFVÇ2À¢Ó5ö6öÖÖæG3£¦Ó5ö6FÆöu÷6÷W&6W2À¢Ó5ö6öÖÖæG3£¦Ó5ö6FÆöu÷&WÆ6U÷6÷W&6W2À¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖW2À¢Ó5ö6öÖÖæG3£¦Ó5÷&Vg&W6…÷'VçF–ÖW2À¢Ó5ö6öÖÖæG3£¦Ó5÷&W6öÇfU÷6WGF–æuö6&–Æ—F–W2À¢Ó5ö6öÖÖæG3£¦Ó5÷66†VGVÆU÷ÆâÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6†E÷FV×ÆFUöÆ%÷&W÷'BÀ¢Ó5ö6öÖÖæG3£¦Ó5ööffÆöE÷ÆâÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6FÆöu÷6V&6‚À¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅ÷7FÆVæW75ö6†V6²À¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅöF÷væÆöBÀ¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅ÷WFFRÀ¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅö7F—fFU÷fW'6–öâÀ¢Ó5ö6öÖÖæG3£¦Ó5÷fW&–g•÷&ö¦V7F÷"À¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅ÷'VæU÷fW'6–öç2À¢Ó5ö6öÖÖæG3£¦Ó5öÖöFVÅöFVÆWFRÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ÆVçWö÷'†ç2À¢Ó5ö6öÖÖæG3£¦Ó5ö6æ6VÅö÷W&F–öâÀ¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖU÷7FGW2À¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖUö–çfVçF÷'’À¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖUöÆöEöÖöFVÂÀ¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖU÷VæÆöEöÖöFVÂÀ¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖUöÆöw2À¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖUöÖWG&–72À¢Ó5ö6öÖÖæG3£¦Ó5ö6öçFW‡Eö66†U÷7FFRÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6öçFW‡EöVffV7F—fU÷6—¦RÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6Æ76–g•ö6öçFW‡Eöf–ÇW&RÀ¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖU÷6WEö6öæf–rÀ¢Ó5ö6öÖÖæG3£¦Ó5÷'VçF–ÖUö6öæf–rÀ¢Ó5ö6öÖÖæG3£¦Ó5ö•öF—7F6‚À¢Ó5ö6öÖÖæG3£¦Ó5ö•ö6æ6VÅö–æfW&Væ6RÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×F–&–Æ—G•öÖG&—‚À¢Ó5ö6öÖÖæG3£§'Våö6öæf÷&Öæ6U÷7V—FRÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆå÷fÆ–FFU÷öÆ–7’À¢Ó5ö6öÖÖæG3£¦Ó5öÆåö6öæf–wW&RÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆåöF—6&ÆRÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆå÷öÆ–7’À¢Ó5ö6öÖÖæG3£¦Ó5öÆåö&Vv–å÷—&–ærÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆåö6ö×ÆWFU÷—&–ærÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆå÷&Wfö¶U÷Fö¶VâÀ¢Ó5ö6öÖÖæG3£¦Ó5öÆå÷Fö¶Vç2À¢Ó5ö6öÖÖæG3£¦Ó5öÆåöVF—EöWfVçG2À¢Ó5ö6öÖÖæG3£§VçF—¦F–öåö&6¶VæG2À¢Ó5ö6öÖÖæG3£§VçF—¦F–öå÷VçE÷G—W2À¢Ó5ö6öÖÖæG3£§VçF—¦F–öåö6öçfW'E÷F‚À¢Ó5ö6öÖÖæG3£§VçF—¦F–öåö6öçfW'Eö–ç7FÆÆVEöÖöFVÂÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçE÷7F÷&vU÷7FGW2À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEö–ç7FÆÆVBÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçE÷&Vv—7G'•öVçG&–W2À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçE÷&WÆ6U÷&Vv—7G'•öVçG&–W2À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEöÖW&vU÷&Vv—7G'•öVçG&–W2À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEöfWF6…ö6FÆörÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçE÷7–æ5ö6FÆörÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEöÆ—7E÷&Vv—7G'’À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEö6†V6µ÷WFFW2À¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEö–ç7FÆÂÀ¢Ó5ö6öÖÖæG3£¦Ó5ö6ö×öæVçEö7F—fFU÷fW'6–öâÀ¢5¶6fr‡F&vWEö÷2Ò&Ö6÷2"•Ğ¢Ó5ö6öÖÖæG3£¦Ó5öÖÇ…ö–ç7FÆÂÀ¢5¶6fr‡F&vWEö÷2Ò&Ö6÷2"•Ğ¢Ó5ö6öÖÖæG3£¦Ó5öÖÇ…ö–ç7FÆÅö6ö×öæVçBÀ¢5¶6fr‡F&vWEö÷2Ò&Ö6÷2"•Ğ¢Ó5ö6öÖÖæG3£¦Ó5öÖfÇW…ö–ç7FÆÅö6ö×öæVçBÀ¢Ó5ö6öÖÖæG3£¦Ó5÷FVÆVÖWG'•÷&V6÷&EöÆöBÀ¢Ó5ö6öÖÖæG3£¦Ó5÷FVÆVÖWG'•÷&V6÷&E÷&WVW7BÀ¢Ó5ö6öÖÖæG3£¦Ó5÷FVÆVÖWG'•÷&V6VçE÷G&6W2À¢Ó5ö6öÖÖæG3£¦Ó5÷FVÆVÖWG'•÷7W÷'Eö'VæFÆRÀ¢Ó5ö6öÖÖæG3£¦vVçEöÆVæ6†W%övVæW&FUö6öæf–rÀ¢Ó5ö6öÖÖæG3£¦vVçEöÆVæ6†W%ö6†V6µöG&–gBÀ¢Ó5ö‡GG÷6W'fW#£¦Ó5ö‡GG÷6W'fW%÷7F'BÀ¢Ó5ö‡GG÷6W'fW#£¦Ó5ö‡GG÷6W'fW%÷7F÷À¢Ó5ö‡GG÷6W'fW#£¦Ó5ö‡GG÷6W'fW%÷7FGW2À¢Ó5ö‡GG÷6W'fW#£¦Ó5ö‡GG÷6W'fW%÷7F÷&U÷FÇ5ö–FVçF—G’À¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷6VVEöf—'7E÷'G’À¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5ö–×÷'E÷÷'F&ÆRÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5ö6FÆörÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5ö–ç7FÆÆVBÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5ö7F—fU÷6¶–ÆÇ2À¢ÓEö6öÖÖæG3£¦ÓE÷ÇVv–ç5ö7F—fU÷6æ6†÷BÀ¢ÓEö6öÖÖæG3£¦ÓE÷ÇVv–ç5÷'VçF–ÖRÀ¢ÓEö6öÖÖæG3£¦ÓE÷ÇVv–ç5ö7F—fFU÷v÷&¶fÆ÷rÀ¢ÓEö6öÖÖæG3£¦ÓE÷ÇVv–ç5öFV7F—fFU÷v÷&¶fÆ÷rÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5öF—66÷fW"À¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷&Wf–WuöÆö6ÂÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5ö–ç7FÆÅöÆö6ÂÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷&Wf–Wuöv—BÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5ö–ç7FÆÅöv—BÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5ö–ç7FÆÅöv—Eö'VÆ²À¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷6WEöVæ&ÆVBÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷6WEöVæ&ÆVEöÖç’À¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷Væ–ç7FÆÂÀ¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷Væ–ç7FÆÅöÖç’À¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷&öÆÆ&6²À¢æF—fU÷6¶–ÆÅö6öÖÖæG3£¦æF—fU÷6¶–ÆÇ5÷&öÆÆ&6µöÖç’À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöÖöFRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷6WEöÖöFRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöFWFV7BÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö6GW&UöVÆ–v–&–Æ—G’À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷66÷Uöf÷%÷'VâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö6GW&RÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöÆ—7Eö6æF–FFW2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö6æF–FFRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö&Vv–å÷&VfÆV7F–öâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷7FvRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷ÆåöWfÇVF–öâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷&W÷'EöWfÇVF–öâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöÖ&µ÷VæWfÇVFVBÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöWfÇVF–öç2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷&öÖ÷FRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷&V¦V7BÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöf–æÆ—¦U÷'VâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷&V6÷&Eö6÷'&V7F–öâÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö7&VFU÷6æF&÷†W2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöFW7G&÷•÷6æF&÷†W2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷6WGF–æw2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷6WE÷6WGF–æw2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷&VfÆV7F–öåö'&–VbÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöÆV&æVE÷6¶–ÆÇ2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷VÆ—G•÷7VÖÖ&–W2À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö–×&÷fVÖVçEöWf–FVæ6RÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æu÷'VåöWf–FVæ6RÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuö&Vv–åö–×&÷fVÖVçBÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöVffV7F—fVæW72À¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöFW&V6FRÀ¢6¶–ÆÅöÆV&æ–æuö6öÖÖæG3£§6¶–ÆÅöÆV&æ–æuöF—66÷fW"À¢6¶–ÆÅö7F—fF–öåö6öÖÖæG3£§6¶–ÆÅö7F—fF–öåöÆ—7BÀ¢6¶–ÆÅö7F—fF–öåö6öÖÖæG3£§6¶–ÆÅö7F—fF–öåövWBÀ¢6¶–ÆÅö7F—fF–öåö6öÖÖæG3£§6¶–ÆÅö7F—fF–öå÷6WBÀ¢6¶–ÆÅö7F—fF–öåö6öÖÖæG3£§6¶–ÆÅö7F—fF–öåöÖ–w&FRÀ¢6V7W&—G•ö6öÖÖæG3£§6V7W&—G•öVF—BÀ¢6VÆeö–çFVw&—G“£§6VÆeö–çFVw&—G•÷&W÷'BÀ¢WFFU÷&öÆÆ&6³£§WFFUö–ç7FÆÅö–æfòÀ¢WFFU÷&öÆÆ&6³£§WFFU÷6æ6†÷Eö7&VFRÀ¢WFFU÷&öÆÆ&6³£§WFFU÷&öÆÆ&6µ÷7FGW2À¢WFFU÷&öÆÆ&6³£§WFFU÷&öÆÆ&6µöF—66&BÀ¢WFFU÷&öÆÆ&6³£§WFFU÷&öÆÆ&6µöÇ’À¢F–væ÷7F–73£¦F–væ÷7F–75÷'VâÀ¢F–væ÷7F–73£¦F–væ÷7F–75öÇ•öf—‚À¢F–væ÷7F–73£¦F–væ÷7F–75öW‡÷'Eö'VæFÆRÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷&Wf–WrÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5ö–ç7FÆÂÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷WFFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷6WEöVæ&ÆVBÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷–âÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷&öÆÆ&6²À¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷Væ–ç7FÆÂÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5öW‡÷'BÀ¢ÓEö6öÖÖæG3£¦ÓE÷6¶vW5÷6WE÷FVÕö&÷fVBÀ¢ÓEö6öÖÖæG3£¦ÓE÷&Vv—7G&–W5öÆ—7BÀ¢ÓEö6öÖÖæG3£¦ÓE÷&Vv—7G&–W5öFBÀ¢ÓEö6öÖÖæG3£¦ÓE÷&Vv—7G&–W5÷&VÖ÷fRÀ¢ÓEö6öÖÖæG3£¦ÓE÷&Vv—7G&–W5÷fW&–g’À¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…÷&Vv—7FW"À¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…÷6W'fW'2À¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…ö&Vv–âÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…ö6ö×ÆWFRÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…÷&Vg&W6‚À¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…÷&Wfö¶RÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7ööWF…öÖWFFFÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7÷V•ö÷VâÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7÷V•öWF†÷&—¦Uö7F–öâÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7÷V•÷&W&Uö7F–öâÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7÷V•öFV6–FUö7F–öâÀ¢ÓEö6öÖÖæG3£¦ÓEöÖ7÷V•ö6Æ÷6RÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5öÆ—7BÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5öÆöBÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷fÆ–FFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷&Vg&W6…ö6&–Æ—F–W2À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö7&VFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷WFFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö–×÷'EöÆVv7’À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5öFVÆWFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷'VâÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö6æ6VÂÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷&W&Uö&÷fÂÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5öFV6–FUö&÷fÂÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷&WÆ’À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö†—7F÷&–W2À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö†—7F÷'’À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5ö–ç7V7EöæöFRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷&V6öæ6–ÆRÀ¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷&Vv—7FW%÷G&–vvW'2À¢ÓEö6öÖÖæG3£¦ÓE÷v÷&¶fÆ÷w5÷Vç&Vv—7FW%÷G&–vvW'2À¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷7F'BÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%öÆ—7BÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%öæf–vFRÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷&VÆöBÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷6WE÷f–Ww÷'BÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%ö–ç7V7BÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%öææ÷FFRÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%ö6Æ–6²À¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷G—U÷FW‡BÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷67&öÆÂÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%ö6GW&UöWf–FVæ6RÀ¢'&÷w6W%÷v÷&¶W#£¦'&÷w6W%÷7F÷À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5öF—66÷fW"À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5öÆ—7BÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5ö7F—fUö6&–Æ—F–W2À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5ö–ç7V7BÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5ö–ç7FÆÂÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷fÆ–FFRÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷6WEöVæ&ÆVBÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷6WE÷'Vææ–ærÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷&Wf–Wu÷WFFRÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷WFFRÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷&öÆÆ&6²À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷Væ–ç7FÆÂÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷7FGW2À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5öÆöw2À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷6WEö6öæf–rÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷6WE÷6V7&WBÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷&VÖ÷fU÷6V7&WBÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5ö–çfö¶RÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5ö6æ6VÂÀ¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷vV&†öö·2À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷&Vv—7FW%÷vV&†öö²À¢W‡FVç6–öåö6öÖÖæG3£¦W‡FVç6–öç5÷&VÖ÷fU÷vV&†öö²À¢FVÖöåö6öÖÖæG3£¦6öçfW'6F–öç5öÆ—7BÀ¢FVÖöåö6öÖÖæG3£¦6öçfW'6F–öç5÷6†÷rÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öÆ—7BÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öFBÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷&ö&RÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öVæ&ÆRÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷6WE÷öÆ–7’À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷6WEö7&VFVçF–ÂÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷6VæFW'2À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öFV6–FU÷6VæFW"À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷&÷WFW2À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öFE÷&÷WFRÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷WFFU÷&÷WFRÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öVæ&ÆU÷&÷WFRÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷&VÖ÷fU÷&÷WFRÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷6WEö6öæf–rÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5ö6ÆÆ&6µ÷W&ÂÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷6WE÷V&Æ–5÷W&ÂÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öW‡÷7W&U÷7FGW2À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öW‡÷7W&UöÖçVÂÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öW‡÷7W&U÷6WE÷GVææVÂÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öW‡÷7W&U÷6WE÷Fö¶VâÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öW‡÷7W&Uö6ÆV%÷Fö¶VâÀ¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5öWfVçG2À¢FVÖöåö6öÖÖæG3£¦6†ææVÇ5÷&VÖ÷fRÀ¢FVÖöåö6öÖÖæG3£¦–æw&W75÷GW&ç2À¢FVÖöåö6öÖÖæG3£¦–æw&W75÷GW&å÷6†÷rÀ¢FVÖöåö6öÖÖæG3£¦–æw&W75÷GW&å÷&W7VÖRÀ¢FVÖöåö6öÖÖæG3£§VW'5öÆ—7BÀ¢FVÖöåö6öÖÖæG3£§VW'5ö–çf—FRÀ¢FVÖöåö6öÖÖæG3£§VW'5ö66WBÀ¢FVÖöåö6öÖÖæG3£§VW'5öw&çBÀ¢FVÖöåö6öÖÖæG3£§VW'5÷&Wfö¶RÀ¢FVÖöåö6öÖÖæG3£§VW'5÷&÷FFRÀ¢FVÖöåö6öÖÖæG3£§VW'5ö66WE÷&÷FF–öâÀ¢FVÖöåö6öÖÖæG3£§VW'5ö6ÆV"À¢FVÖöåö6öÖÖæG3£§VW'5öf÷&vWBÀ¢FVÖöåö6öÖÖæG3£§VW'5÷7FGW2À¢FVÖöåö6öÖÖæG3£§VW'5÷F‡&VG2À¢FVÖöåö6öÖÖæG3£§VW'5ö÷WF&÷VæBÀ¢FVÖöåö6öÖÖæG3£§VW'5÷&VÖ÷FU÷F‡&VBÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕöÆ—7BÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕöFBÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷&ö&RÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕöVæ&ÆRÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷6WE÷öÆ–7’À¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷6WEöÆ–Ö—G2À¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷6WEö7&VFVçF–ÂÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷6WEöw&VWF–ærÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕö6ÆÇ2À¢FVÖöåö6öÖÖæG3£§FVÆV6öÕöÖW76vW2À¢FVÖöåö6öÖÖæG3£§FVÆV6öÕö6ÆÆ&6µ÷W&ÂÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷6WE÷V&Æ–5÷W&ÂÀ¢FVÖöåö6öÖÖæG3£§FVÆV6öÕ÷&VÖ÷fRÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷7FGW2À¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷öFV6—6–öç2À¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷ö–ç7FÆÂÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷öVç7W&RÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷7F'BÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷7F÷À¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷Væ–ç7FÆÂÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷VWVRÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷W6RÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷&W7VÖRÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷ö6æ6VÂÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷&WG'’À¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷ö¶–ÆÅ÷7v—F6‚À¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷G&–vvW'2À¢ÓföFW6·F÷ö'&–FvS£¦ÓföFW6·F÷÷GW&å÷7V&Ö—BÀ¢FVÖöåö6öÖÖæG3£¦FVÖöåöFW6·F÷÷7–æ5÷&V6—U÷66†VGVÆW2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FUö†÷7E÷7FGW2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FUö†÷7Eö6öæf–wW&RÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUö†÷7EöF—6&ÆRÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷—%ö7&VFRÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷—%öÆ—7BÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷—%÷&Wfö¶RÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷—%÷&÷FFRÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöVF—BÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöFWf–6UöÆ—7BÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöFWf–6Uöw&çBÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöFWf–6Uö6öÖÖæG2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöFWf–6Uö6æ6VÂÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷W6…÷7FGW2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷W6…ö6öæf–wW&RÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷W6…öF—6&ÆRÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷W6…÷FW7BÀ¢FVÖöåö6öÖÖæG3£§FööÅöFWf–6Uö7F–öâÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöæöFUöÆ—7BÀ¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷Æ6VÖVçG2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöæöFU÷&Vg&W6‚À¢FVÖöåö6öÖÖæG3£§&VÖ÷FU÷Æ6VÖVçE÷7–æ2À¢FVÖöåö6öÖÖæG3£§&VÖ÷FUöæöFUöÆ&VÂÀ¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•÷&W&Uö×WFF–öâÀ¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•öW†V7WFUö×WFF–öâÀ¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•öÆ—7E÷v÷&·G&VW2À¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•ö–ç7V7E÷v÷&·G&VRÀ¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•öVF—BÀ¢ÓUöFVÆ—fW'“£¦ÓUöFVÆ—fW'•÷&V6öæ6–Æ–F–öç2À¢ÓUöFVÆ—fW'“£¦ÓUöv—F‡V%öWF…÷7FGW2À¢ÓUöFVÆ—fW'“£¦ÓUöv—F‡V%ö—77VRÀ¢ÓUöFVÆ—fW'“£¦ÓUöv—F‡V%÷VÆÅ÷&WVW7BÀ¢ÓUöFVÆ—fW'“£¦ÓUöv—F‡V%÷&Wf–Wu÷F‡&VG2À¢ÓUöFVÆ—fW'“£¦ÓUöv—F‡V%ö6†V6·2À¢ÓUöFVÆ—fW'“£¦ÓU÷&Wf–Wu÷VÆÅ÷&WVW7BÀ¢ÓUöFVÆ—fW'“£¦ÓU÷&Wf–Wu÷&W÷'G2À¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%÷7F'BÀ¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%÷7FGW2À¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%öÆ—7BÀ¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%ö6æ6VÂÀ¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%öGfæ6RÀ¢—77VU÷Fõ÷#£¦—77VU÷Fõ÷%÷'Våö6†V6·2À¢&÷fÅö6†–ç3£¦&÷fÅö6†–ç5öÆ—7E÷FV×ÆFW2À¢&÷fÅö6†–ç3£¦&÷fÅö6†–ç5÷7F'BÀ¢&÷fÅö6†–ç3£¦&÷fÅö6†–å÷&W7öæBÀ¢&÷fÅö6†–ç3£¦&÷fÅö6†–ç5ö†—7F÷'’À¢Æö6Åö3£¦Æö6Åö5÷V&Æ—6‚À¢Æö6Åö3£¦Æö6Åö5öÆ—7BÀ¢Æö6Åö3£¦Æö6Åö5÷VçV&Æ—6‚À¢Æö6Åö3£¦Æö6Åö5ö÷VâÀ¢Óuö6ö×æ–öã£¦Óuö÷fW&Æ•÷6†÷rÀ¢Óuö6ö×æ–öã£¦Óuö÷fW&Æ•ö†–FRÀ¢Óuö6ö×æ–öã£¦Óuö÷fW&Æ•÷7V&Ö—BÀ¢Óuö6ö×æ–öã£¦Óuö6öæf–uövWBÀ¢Óuö6ö×æ–öã£¦Óuö6öæf–u÷6fRÀ¢Óuö6ö×æ–öã£¦Óu÷FÆµ÷7FGW2À¢Óuö6ö×æ–öã£¦Óu÷FÆµöÖWG&–72À¢Óuö6ö×æ–öã£¦Óu÷FÆµöÖWG&–5÷&V6÷&BÀ¢Óuö6ö×æ–öã£¦Óu÷FÆµöÖWG&–75ö6ÆV"À¢Óuö6ö×æ–öã£¦Óu÷FÆµ÷G&ç67&–&RÀ¢Óuö6ö×æ–öã£¦Óuö6GW&Uöw&çBÀ¢Óuö6ö×æ–öã£¦Óuö6GW&U÷&Wfö¶RÀ¢Óuö6ö×æ–öã£¦Óuö6GW&Uöw&çG2À¢Óuö6ö×æ–öã£¦Óuö6GW&U÷FW‡BÀ¢Óuö6ö×æ–öã£¦Óuö6GW&Uöf–ÆRÀ¢Óuö6ö×æ–öã£¦Óuö6GW&U÷67&VVâÀ¢Óuö6ö×æ–öã£¦Óu÷G&ç67&–&Uöf–ÆRÀ¢Óuö6ö×æ–öã£¦Óu÷G&ç67&–&UöVF–òÀ¢Óuö6ö×æ–öã£¦Óu÷GG5÷7V²À¢Óuö6ö×æ–öã£¦Óu÷GG5÷7–çF†W6—¦RÀ¢Óuö6ö×æ–öã£¦Óuö¦ö%ö6æ6VÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöVæv–æU÷7FGW2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöÖöFVÇ2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöFEöÖöFVÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷&VÖ÷fUöÖöFVÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåö66WEöÆ–6Vç6RÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷6WEö‡Vvv–æuöf6U÷Fö¶VâÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöF÷væÆöEöÖöFVÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåö6æ6VÅöF÷væÆöBÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷'G2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöFE÷'BÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷&VÖ÷fU÷'BÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöÆ÷&2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöFEöÆ÷&À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷&VÖ÷fUöÆ÷&À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåö&6¶VæG2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöFEö&6¶VæBÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷&VÖ÷fUö&6¶VæBÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷'VâÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåö6æ6VÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåövÆÆW'’À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöFVÆWFUöVçG'’À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåöÖVF–öFF÷W&ÂÀ¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öåö6&–Æ—F–W2À¢vVæW&F–öåö6öÖÖæG3£¦vVæW&F–öå÷VæÆöEöVæv–æRÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÇ2À¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅöFBÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅ÷&VÖ÷fRÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅöÖæ–fW7BÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅ÷'VâÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅ÷7F÷À¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÇ5÷'Vææ–ærÀ¢vVæW&F–öåö6öÖÖæG3£§7GVF–õ÷FööÅö–×÷'Eö6FÆörÀ¢Óuö6ö×æ–öã£¦Óuö–ÖvUövVæW&FRÀ¢Óuö6ö×æ–öã£¦Óuö–ÖvUövÆÆW'’À¢Óuö6ö×æ–öã£¦Óuö–ÖvUöFF÷W&ÂÀ¢Óuö6ö×æ–öã£¦Óuö–ÖvUö–ç6W'Eö6†BÀ¢Óuö6ö×æ–öã£¦ÓuöVÖW&vVæ7•÷7F÷À¢F–7FF–öã£¦F–7FF–öåö6&–Æ—F–W2À¢F–7FF–öã£¦F–7FF–öåö÷Vå÷W&Ö—76–öå÷6WGF–æw2À¢F–7FF–öã£¦F–7FF–öå÷7F'BÀ¢F–7FF–öã£¦F–7FF–öå÷7F÷À¢F–7FF–öã£¦F–7FF–öåö6æ6VÂÀ¢6öÖÖæE÷ÆWGFS£§ÆWGFU÷6†÷rÀ¢6öÖÖæE÷ÆWGFS£§ÆWGFUö6öæf–uövWBÀ¢6öÖÖæE÷ÆWGFS£§ÆWGFUö6öæf–u÷6fRÀ¢&—f7•öf—&WvÆÃ£§&—f7•öf—&WvÆÅövWE÷öÆ–7’À¢&—f7•öf—&WvÆÃ£§&—f7•öf—&WvÆÅ÷6fU÷öÆ–7’À¢&—f7•öf—&WvÆÃ£§&—f7•öf—&WvÆÅ÷&Wf–WrÀ¢&—f7•öf—&WvÆÃ£§&—f7•öf—&WvÆÅ÷&W&U÷6VæBÀ¢&—f7•öf—&WvÆÃ£§&—f7•öf—&WvÆÅöW†V7WFU÷6VæBÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷7F'E÷6W76–öâÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷7F÷÷6W76–öâÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷W6U÷6W76–öâÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷6W76–öç2À¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷&WVW7Eö7F–öâÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷&W7öæEö7F–öâÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅöVÖW&vVæ7•÷7F÷À¢FW6·F÷ö6öçG&öÃ£¦6ö×WFW%÷W6UögVÆÅ÷&öGV7E÷&W÷'BÀ¢FW6·F÷ö6öçG&öÃ£¦FW6·F÷ö6öçG&öÅ÷&÷f–FW%ö–æfòÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%öÆ—7E÷F&vWG2À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷67&VVç6†÷BÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%ö6Æ—&ö&E÷&VBÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%ö–ç7V7BÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%öfö7W2À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%ö6Æ–6²À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%öF÷V&ÆUö6Æ–6²À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷67&öÆÂÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷G—RÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%ö¶W’À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%ö†÷F¶W’À¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷v—BÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷6VÆV7BÀ¢FW6·F÷ö6öçG&öÃ£§FööÅö6ö×WFW%÷6WE÷fÇVRÀ¢'VçF–ÖU÷%÷vF6†W#£§'VçF–ÖU÷%÷vF6†W%÷7FFRÀ¢'VçF–ÖU÷%÷vF6†W#£§'VçF–ÖU÷%÷vF6†W%ö6†V6µöæ÷rÀ¢Ò¢æ'V–ÆB‡FW&“£¦vVæW&FUö6öçFW‡B‚’¢æW‡V7B‚&W'&÷"v†–ÆR'V–ÆF–ærFW&’Æ–6F–öâ"“° ¢ç'Vâ‡Æö†æFÆRÂWfVçGÂ°¢òò£§'VææWfW"&WGW&ç2(	Böæ6RF†RWfVçBÆö÷—2FöæRÂF†P¢òòVæFW&Ç––ærFö'VçF–ÖR6ÆÇ27FC£§&ö6W73£¦W†—FF—&V7FÇ¢òò‡6VR—G2÷vâFö26öÖÖVçB’Âv†–6‚6¶—2'W7Bw2G&÷Ö&6VB6ÆVçW ¢òòVçF—&VÇ’âF†BÖVç2ç’Æ—fRÔ57FF–ò6†–ÆB&ö6W72††VÆB–à¢òò7FFS£¦Ö7Â6ÆVæVBWöæÇ’f–Ö76öææV7F–öã£§6W'f–6Vw0¢òòG&÷öæ6æ6VÂ‚–’(	BæBV—F†W"ÖævVBÆÆÖ×6W'fW&6†–Æ@¢òò&ö6W72†7FFS£¦ÆÆÖö7FFS£¦VÖ&VEöÆÆÖÂæV—F†W"ö`¢òòv†–6‚†2G&÷–×ÂV—F†W"’(	Bv÷VÆB÷F†W'v—6R&R6–ÆVçFÇ¢òò÷'†æVBöâWfW'’æ÷&ÖÂV—Bâ'VäWfVçC£¤W†—Ff—&W0¢òò7–æ6‡&öæ÷W6Ç’öâF†RÖ–âF‡&VB&–v‡B&Vf÷&RF†B†Vç2Â6ğ¢òòFö–ær&÷F‚¶–æG2öb6ÆVçW†W&R(	BÖ7£¦F—66öææV7EöÆÆ†&÷VæFV@¢òòæB&W7BÖVff÷'B(	B6VR—G2÷vâFö26öÖÖVçB’æ@¢òòÆÆÖ£§7F÷öÆÅö&Æö6¶–æv‡7–æ6‡&öæ÷W2Âæò†æFÆVö7–æ0¢òò'VçF–ÖR&WV—&VBÂ6–æ6RÆ–â7FC£§&ö6W73£¤6†–ÆC£¦¶–ÆÆ—2ÆÀ¢òòV—F†W"&ö6W72æVVG2’(	B—2F†RöæÇ’6†æ6RF†÷6R6†–ÆB&ö6W76W0¢òòvWBFò7GVÆÇ’&R¶–ÆÆVB&Vf÷&RF†R&ö6W72—G6VÆbW†—G2à¢–bÆWBFW&“£¥'VäWfVçC£¤W†—BÒWfVçB°¢ÆWBòÒFW&“£¦7–æ5÷'VçF–ÖS£¦&Æö6µööâ‡6W'fW#£§6‡WFF÷vå÷Væ–f–VE÷6W'fW"†ö†æFÆR’“°¢ÆWBòÒW†V7WF&ÆUöW‡FVç6–öç3£¦6Æ÷6UöÆÅ÷6W76–öç2‚“°¢ÆWBòÒW†V7WF&ÆUöW‡FVç6–öç3£¦6æ6VÅöÆÂ‚“° ¢ÆWBÓ2Òö†æFÆRç7FFS££ÆÓ5ö6öÖÖæG3£¤Ó46öÖÖæE7FFSâ‚“°¢ÆWBòÒÓ2æ6æ6VÅöÆÅöæE÷6‡WFF÷våö÷væVB‡7FC£§F–ÖS£¤GW&F–öã£¦g&öÕ÷6V72ƒR’“° ¢ÆWBÓBÒö†æFÆRç7FFS££ÆÓEö6öÖÖæG3£¤ÓD6öÖÖæE7FFSâ‚“°¢ÆWBòÒÓBç6‡WFF÷våöÆÅö&Æö6¶–ær‚“° ¢ÆWB'&÷w6W"Òö†æFÆRç7FFS££Æ'&÷w6W%÷v÷&¶W#£¤'&÷w6W$6öÖÖæE7FFSâ‚“°¢ÆWBòÒ'&÷w6W"ç6‡WFF÷våöÆÂ‚“° ¢ÆWB6ö×æ–öâÒö†æFÆRç7FFS££ÆÓuö6ö×æ–öã£¤Ót6ö×æ–öå7FFSâ‚“°¢ÆWBòÒ6ö×æ–öâæVÖW&vVæ7•÷7F÷‚“° ¢ÆWBF–7FF–öâÒö†æFÆRç7FFS££ÆF–7FF–öã£¤F–7FF–öå'VçF–ÖSâ‚“°¢F–7FF–öã£§6‡WFF÷vâ†F–7FF–öâæ–ææW"‚’“° ¢ÆWBFW6·F÷ö6öçG&öÂÒö†æFÆRç7FFS££ÆFW6·F÷ö6öçG&öÃ£¤FW6·F÷6öçG&öÅ7FFSâ‚“°¢ÆWBòÒFW6·F÷ö6öçG&öÂæVÖW&vVæ7•÷7F÷‚“° ¢ÆWB7FFRÒö†æFÆRç7FFS££Ä7FFSâ‚“°¢7FFRçFW&Ö–æÂæ¶–ÆÅöÆÂ…6öÖR†ö†æFÆR’“°¢7FFRæ&6¶w&÷VæE÷6†VÆÂæ¶–ÆÅöÆÂ‚“°¢FW&“£¦7–æ5÷'VçF–ÖS£¦&Æö6µööâ†Ö7£¦F—66öææV7EöÆÂ‡7FFRæ–ææW"‚’’“°¢ÆÆÖ£§7F÷öÆÅö&Æö6¶–ær‡7FFRæ–ææW"‚’“°¢Ğ¢Ò“°§Ğ ¢5¶6fr‡FW7B•Ğ¦ÖöBFW7G2°¢òòò5·FW&“£¦6öÖÖæEÖF†B—2æWfW"æÖVB–â'Væw0¢òòòvVæW&FUö†æFÆW"Æ—7B6ö×–ÆW2ÂG—RÖ6†V6·2ÂæB76W2WfW'’Væ—@¢òòòFW7Bw&—GFVâv–ç7B—B(	Bv†–ÆR&V–ærVç&V6†&ÆRg&öÒF†Rg&öçFVæBÀ¢òòò&V6W6R–çfö¶V&W6öÇfW2v–ç7BF†BÆ—7BÆöæRâF†Rc(i'c"¶æ÷vÆVFvP¢òòò–×÷'B‡6–æ6RFVÆWFVBÆöærv—F‚c’7VçB—G2v†öÆRÆ–fR–âF†B7FFS ¢òòògVÆÇ’–×ÆVÖVçFVBÂ’76–ærFW7G2ÂFö7VÖVçFVB2FöæRÂæB–×÷76–&ÆP¢òòòf÷"ç’W6W"Fò'Vâà¢òòğ¢òòò2v‡’6÷W&6R66à¢òòğ¢òòòF†Rv—2&WGvVVâGvò¦Æ—7G2¢(	BF†R6öÖÖæG2F†BW†—7BæBF†R6öÖÖæG0¢òòòF†B&R&Vv—7FW&VB(	BæBæ÷F†–ærB'VçF–ÖR6ö×&W2F†VÒâföÆÆ÷v–ærF†P¢òòò&V6VFVçBöbVw&W72ç'6w2æõöæWuö&&U÷&WvW7Eö6Æ–VçEö6åö&UöFFVE÷Vææ÷F–6VF ¢òòòæBÓ5ö‡GG÷6W'fW"ç'6w26VÆb×66ç2ÂF†—2&VG2F†RG&VRæB6ö×&W0¢òòòF†VÒF—&V7FÇ’â—BæVVG2æòÆÆ÷vÆ—7C¢BF†RF–ÖR—Bv2w&—GFVâWfW'¢òòòöæRöbF†R7&FRw2S36öÖÖæG2v2&Vv—7FW&VBW†6WBF†RöæR'Vr&÷fRÀ¢òòò6òF†R76W'F–öâ—26–×Ç’'F†RF–ffW&Væ6R—2V×G’"à¢òòğ¢òòò2v†B—B6÷fW'0¢òòğ¢òòòWfW'’5·FW&“£¦6öÖÖæEÖ–âWfW'’ç'6f–ÆRVæFW"7&2öÂ–æ6ÇVF–æp¢òòòf–ÆW2F†BFòæ÷BW†—7B–WB(	BæWr6öÖÖæB–â'&æBÖæWrÖöGVÆR—2F†P¢òòòÆ–¶VÆ–W7Bv’F†RæW‡BVç&Vv—7FW&VBöæR'&—fW2Âv†–6‚—2v‡’F†—2vÆ·0¢òòò4$tõôÔä”dU5EôD•&–ç7FVBöb–æ6ÇVFU÷7G"Ö–ær†&BÖ6öFVBÆ—7Bà¢òòğ¢òòò2v†B—B6ææ÷B6÷fW ¢òòğ¢òòò¢¢¥w&öærÖöGVÆRF‚â¢¢—BÖF6†W2F†R&&RgVæ7F–öâæÖRÂ6ğ¢òòòw&öæuöÖöGVÆS£§6öÖUö6öÖÖæF6÷VçG22&Vv—7FW&VBâF†R6ö×–ÆW"&V¦V7G0¢òòòF†Bç—v’à¢òòò¢¢¥&Vv—7FW&VB'WBVæ6ÆÆVBâ¢¢6öÖÖæB–âF†RÆ—7Bv—F‚æòg&öçFVæ@¢òòò–çfö¶VæBæò4Ä’7V&6öÖÖæB—27F–ÆÂFVB6öFS²'&V6†&ÆR"†W&P¢òòòÖVç2öæÇ’&F—7F6†&ÆR"à¢òòò¢¢¤6öÖÖæBFV6Æ&VB–ç6–FR5¶6fr‡FW7B•Öâ¢¢FW‡B66â6ææ÷B6VP¢òòò6fvvF–ærÂ6ò7V6‚6öÖÖæBv÷VÆB&RfÇ6R÷6—F—fRâæöæRW†—7BÀ¢òòòæBFW7BÖöæÇ’FW&’6öÖÖæBv÷VÆB&RÖVæ–ævÆW72à¢5·FW7EĞ¢fâWfW'•÷FW&•ö6öÖÖæEö—5÷&V6†&ÆUög&öÕ÷F†Uö–çfö¶Uö†æFÆW"‚’°¢6öç7BEE$”%UDS¢g7G"Ò"5·FW&“£¦6öÖÖæB#° ¢ÆWB6÷W&6RÒ–æ6ÇVFU÷7G"‚&Æ–"ç'2"“°¢ÆWB†æFÆW"Ò6÷W&6P¢ç7Æ—Eööæ6R‚'FW&“£¦vVæW&FUö†æFÆW"²"¢æW‡V7B‚''Vâ‚’'V–ÆG2—G2†æFÆW"v—F‚vVæW&FUö†æFÆW""¢ã¢ç7Æ—Eööæ6R‚%ÆâÒ’"¢æW‡V7B‚'F†R†æFÆW"Æ—7B—26Æ÷6VBB—G2÷Væ–ær–æFVçFF–öâ"¢ã°¢òò6öÖÖVçBÆ–æW2–ç6–FRF†RÆ—7B‡F†W&R—2öæR’7W'f—fRF†R7Æ—B0¢òòæöâÖ–FVçF–f–W"FW‡C²&WV—&–ær&&R–FVçF–f–W"G&÷2F†VÒà¢ÆWB&Vv—7FW&VC¢7FC£¦6öÆÆV7F–öç3£¤†6…6WCÂg7G#âÒ†æFÆW ¢æÆ–æW2‚¢æf–ÇFW%öÖ‡ÆÆ–æWÂÆ–æRç7Æ—B‚rÂr’ææW‡B‚’¢æÖ‡ÆVçG'—ÂVçG'’ç'7Æ—B‚#£¢"’ææW‡B‚’çVçw&ö÷"†VçG'’’çG&–Ò‚’¢æf–ÇFW"‡ÆVçG'—Â°¢VçG'’æ—5öV×G’‚’bbVçG'’æ6†'2‚’æÆÂ‡Æ7Â2æ—5öÇ†çVÖW&–2‚’ÇÂ2ÓÒuòr¢Ò¢æ6öÆÆV7B‚“° ¢ÆWB7&2Ò7FC£§Fƒ£¥Fƒ£¦æWr†Vçb‚$4$tõôÔä”dU5EôD•""’’æ¦ö–â‚'7&2"“°¢ÆWB×WBFV6Æ&VC¢7FC£¦6öÆÆV7F–öç3£¤†6…6WCÅ7G&–æsâÒ7FC£¦6öÆÆV7F–öç3£¤†6…6WC£¦æWr‚“°¢ÆWB×WBVç&Vv—7FW&VC¢fV3Å7G&–æsâÒfV3£¦æWr‚“°¢f÷"VçG'’–âvÆ¶F—#£¥vÆ´F—#£¦æWr‚g7&2¢æ–çFõö—FW"‚¢æf–ÇFW%öÖ…&W7VÇC£¦ö²¢°¢–bVçG'’çF‚‚’æW‡FVç6–öâ‚’æ—5öæöæUö÷"‡ÆW‡GÂW‡BÒ''2"’°¢6öçF–çVS°¢Ğ¢ÆWBf–ÆRÒ7FC£¦g3£§&VE÷Fõ÷7G&–ær†VçG'’çF‚‚’’æW‡V7B‚'6÷W&6Rf–ÆR&VG2"“°¢f÷"FV6Æ&F–öâ–âf–ÆRç7Æ—B„EE$”%UDR’ç6¶—ƒ’°¢ÆWB6öÖR‚†&WGvVVâÂ6–væGW&R’’ÒFV6Æ&F–öâç7Æ—Eööæ6R‚"fâ"’VÇ6R°¢6öçF–çVS°¢Ó°¢òòWfW'—F†–ærF†RGG&–'WFR—G6VÆbÖ’6''’(	Bâ÷F–öæÀ¢òò‡&VæÖUöÆÂÒ"âââ"–æB—G26Æ÷6–ærÖ(	BF†VâöæÇ¢òòV&ö7–æ6âç—F†–ærVÇ6RÖVç2F†—25·FW&“£¦6öÖÖæFv0¢òò&÷6R–â6öÖÖVçBæBF†Rfæf÷VæB&VÆöæw2Fò6öÖVöæRVÇ6Rà¢ÆWB&W6–GVRÒ&WGvVVà¢çG&–Õ÷7F'EöÖF6†W2‡Æ7Â2ÒuÒr¢çG&–Õ÷7F'EöÖF6†W2‚uÒr¢ç&WÆ6R‚'V""Â""¢ç&WÆ6R‚&7–æ2"Â""“°¢–b&W6–GVRçG&–Ò‚’æ—5öV×G’‚’°¢6öçF–çVS°¢Ğ¢ÆWBæÖS¢7G&–ærÒ6–væGW&P¢çG&–Õ÷7F'B‚¢æ6†'2‚¢çF¶U÷v†–ÆR‡Æ7Â2æ—5öÇ†çVÖW&–2‚’ÇÂ¦2ÓÒuòr¢æ6öÆÆV7B‚“°¢–bæÖRæ—5öV×G’‚’°¢6öçF–çVS°¢Ğ¢–b&Vv—7FW&VBæ6öçF–ç2†æÖRæ5÷7G"‚’’°¢ÆWB&VÆF—fRÒVçG'¢çF‚‚¢ç7G&—÷&Vf—‚‚g7&2¢æW‡V7B‚'vÆ¶VBF‚—2VæFW"7&2ò"¢çFõ÷7G&–æuöÆ÷77’‚¢ç&WÆ6R‚uÅÂrÂ"ò"“°¢Vç&Vv—7FW&VBçW6‚†f÷&ÖB‚'·&VÆF—fWÓ£§¶æÖWÒ"’“°¢Ğ¢FV6Æ&VBæ–ç6W'B†æÖR“°¢Ğ¢Ğ¢Vç&Vv—7FW&VBç6÷'B‚“° ¢òòF†R&WfW'6RF—&V7F–öâÂv†–6‚—2v†B¶VW2F†R66â&÷fR†öæW7C¢–b¢òògWGW&R6–væGW&R6†R7F÷2ÖF6†–ærÂF†R6öÖÖæG2—B6âæòÆöævW ¢òò6VRGW&âW†W&R2'&Vv—7FW&VB'WBæWfW"FV6Æ&VB"–ç7FVBö`¢òò6–ÆVçFÇ’76–ærà¢ÆWB×WB†çFöÓ¢fV3Âg7G#âÒ&Vv—7FW&V@¢æ—FW"‚¢æ6÷–VB‚¢æf–ÇFW"‡ÆæÖWÂFV6Æ&VBæ6öçF–ç2‚¦æÖR’¢æ6öÆÆV7B‚“°¢†çFöÒç6÷'E÷Vç7F&ÆR‚“°¢76W'B€¢†çFöÒæ—5öV×G’‚’À¢'·ÒæÖR‡2’–â'Væw2vVæW&FUö†æFÆW"Æ—7BÖF6‚æò5·FW&“£¦6öÖÖæEÖVæFW"À¢7&2ó¢·ÒåÆäV—F†W"F†RVçG'’—27FÆRæB6†÷VÆB&RFVÆWFVBÂ÷"F†—2FW7Bw266ææW"À¢æòÆöævW"&V6övæ—¦W2†÷rF†R6öÖÖæB—2w&—GFVâ(	Bf—‚F†R66ææW"Â&V6W6RVçF–Â—BÀ¢—2f—†VBF†÷6R6öÖÖæG2&RVæwV&FVBâ"À¢†çFöÒæÆVâ‚’À¢†çFöÒæ¦ö–â‚"Â"’À¢“° ¢76W'B€¢Vç&Vv—7FW&VBæ—5öV×G’‚’À¢'·Ò5·FW&“£¦6öÖÖæEÖ‡2’&Ræ÷B–â'Væw2vVæW&FUö†æFÆW"Æ—7BÂ6òæòÀ¢g&öçFVæB–çfö¶V6â&V6‚F†VÓ¥Æâ·ÕÆåÀ¢FBV6‚öæRFòF†BÆ—7Bâ–b6öÖÖæB—2FVÆ–&W&FVÇ’æ÷BW‡÷6VBFòF†RÀ¢g&öçFVæBÂ—B6†÷VÆBæ÷B6''’5·FW&“£¦6öÖÖæEÖBÆÂ(	BÖ¶R—BÆ–âÀ¢gVæ7F–öâæB6ÆÂ—BF—&V7FÇ’â"À¢Vç&Vv—7FW&VBæÆVâ‚’À¢Vç&Vv—7FW&VBæ¦ö–â‚%Æâ"’À¢“°¢Ğ§Ğ
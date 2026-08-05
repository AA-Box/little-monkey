#![recursion_limit = "2048"]

// `pub` so every module below (and `monkey-cli`, which has no `AppHandle`)
// resolves the app-data directory through one shared `data_dir()` instead of
// each hardcoding the same identifier string independently — see the module
// doc for the drift risk this replaces.
pub mod app_paths;
// `pub` so a future `monkey-cli` parity command (matching `checkpoints`/`rules`/
// `memory`/`web`/`verify` above) could reuse `publish_impl`/`remove_impl`
// directly — no such command exists yet (rendering has no terminal surface,
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
// Native in-app browser pane: real tabbed child webviews (Claude-Desktop-
// style) overlaid on the main webview via the `unstable` multiwebview API.
pub mod browser_pane;
// Disposable Chromium/CDP verification worker with request interception,
// explicit origin grants, DNS re-checks, quotas, and durable evidence.
pub mod browser_worker;
// Typed, fixed-argument desktop bridge to the bundled authoritative daemon
// and optional user-owned remote controller. No arbitrary CLI execution.
mod daemon_commands;
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
pub mod managed_runtime;
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
pub mod modelfile;
pub mod package_ecosystem;
mod security_commands;
pub mod security_doctor;
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
pub mod m7_companion;
// Global Command Palette (ROADMAP.md, Phase 1): owns only the OS-level
// shortcut's persisted configuration and "bring the palette to the front"
// action. The palette itself renders inside the main window and dispatches
// every command through the exact same Tauri commands chat/recipes/
// knowledge/permissions already expose — see the module doc for why.
pub mod command_palette;
// Safe Desktop Control — a design-validation research spike (ROADMAP.md
// Phase 5, "Safe Desktop Control", Status: Research). Off by default,
// never reachable from bypass mode, every action gated behind an explicit
// per-action approval unless the session was started in "approved batch"
// mode, and wired into the same app-exit emergency-stop path as
// `m7_companion`. See `docs/safe-desktop-control-design.md` for the full
// threat model and explicit non-goals.
pub mod desktop_control;
// Apple-Silicon-only MLX lifecycle adapter. The module reports explicit
// unsupported capability on every other platform rather than implying a
// portable backend.
pub mod mlx_runtime;
// Inbound OpenAI/Anthropic compatibility translations and the scoped,
// authenticated LAN policy shared by the API server and user-owned runners.
mod artifact_commands;
pub mod chat_template_lab;
pub mod checkpoints;
pub mod compatibility_hub;
// `pub` only for the doc-comment convention every sibling module below
// follows (a future `monkey-cli` command could call `install_if_needed`
// directly, though none exists yet — the CLI installing itself onto its own
// `PATH` isn't a meaningful operation).
pub mod cli_install;
mod git;
// `pub` so `monkey-cli`'s `embed_cli` module (RAG design doc slice 4 CLI parity)
// can reuse `find_llama_server_binary`/`embed_server_args`/`EMBED_PORT`/
// `LlamaState::for_embeddings` directly instead of re-implementing the
// embeddings-only `llama-server` process's binary discovery and flags — the
// same AppHandle-free-core reasoning as `stacks`/`checkpoints`/`rules` below,
// just exposing a few specific items rather than a `*_impl` set (the rest of
// this module's Tauri-command surface stays desktop-app-only).
pub mod llama;
pub mod mcp;
// Small, dependency-free stdio MCP servers embedded in the binary via
// `include_str!` and materialized on demand under the app data dir — backs
// `McpPanel.tsx`'s "quick add" templates that need a real local file (e.g.
// the bundled AppleScript-control server) rather than an externally
// installed command like `docker`/`npx`.
pub mod bundled_mcp_servers;
// Generic MCP-spec OAuth 2.0 (RFC 8414 discovery, RFC 7591 dynamic client
// registration, PKCE authorization-code flow) for HTTP MCP servers — an
// additional, alternative way to obtain `mcp.rs`'s `McpTransport::Http`
// bearer token besides the manual `mcp_set_http_token` paste-a-token path.
// Kept as its own module (rather than growing `mcp.rs` further) since it's
// the one place a future `rmcp` OAuth API change would need editing.
pub mod mcp_oauth;
// Brokered OAuth for MCP servers whose provider requires a confidential
// client (a `client_secret`) — Slack and Google, confirmed via their own
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
pub mod triage;
pub mod model_sources;
mod process_lock;
mod models;
pub mod ollama;
pub mod providers;
// Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
// Tauri-free static-registry + comparison logic shared by `providers.rs`'s
// cloud-model command and `m3_runtime_hub.rs`'s local-model staleness check.
pub mod model_retirement;
// `pub` so a future `monkey-cli` `Stacks` subcommand (RAG design doc, slice 4)
// can call `stacks::list_impl`/`reindex_impl`/`query_impl` directly, the
// same AppHandle-free-core reasoning as `checkpoints`/`rules`/`memory`.
pub mod stacks;
// `pub` so `monkey-cli` (slice 4) can reuse `load_impl`/`PromptEntry` directly,
// the same reasoning as `rules`/`checkpoints` above.
pub mod prompts;
mod sessions;
mod system;
mod terminal;
mod tools;
// Long-running agent shell commands that outlive the turn that started them
// (`run_shell` with `run_in_background: true`) — see `background_shell.rs`.
pub mod background_shell;
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
// `pub` (unlike `sessions`/`tools`/`system`/`models`/`git`/`llama` above) so
// `monkey-cli` (Plan/Act + risk-adaptive permissions design doc, phase 4) can
// call `permissions::path_risk_floor` directly for its own floor-only
// `"smart"` mode — the same AppHandle-free-core reasoning as `web`/`rules`/
// `memory`/`verify` above. Every other item in this module (the Tauri
// commands, `PermissionState`, `request_permission`) stays reachable too,
// but is only ever actually called from `main.rs`'s own Tauri app wiring,
// not from `monkey-cli`.
pub mod permissions;
// `pub` (not `mod`, like `sessions`/`tools`/`system` above) so `monkey-cli`
// (slice 5) can call `read_rules_impl`/`load_impl`/`add_fact_impl` directly
// from `little_monkey_lib`, the same way it already reuses `checkpoints`.
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
// The one process abstraction shared by every execution surface — desktop
// turns, daemon jobs, subagents, crew members, workflow runs/nodes, remote
// runs, background shells, side tasks. Public for the same reason the two
// modules above are: the CLI's `monkey ps` and the daemon both read it, and
// neither should grow a second copy of the state machine.
pub mod process_table;
mod process_commands;
// Policy shared by the two HTTP listeners, which default to the same port and
// today report a bare "address already in use" naming neither the winner nor
// the reason. Where the shared pieces accumulate as D1 collapses them into one.
pub mod http_policy;
// Migration-controlled authoritative profile/session/search storage. Kept
// reusable by the desktop, CLI, daemon, export/import, and restore paths.
pub mod portability;
// The resident daemon reuses the exact keychain-backed snapshot/WebDAV
// implementation exposed to Tauri. Keeping this module public prevents the
// CLI service from growing a second encryption, credential, or conflict path.
pub mod portability_commands;
mod profile_commands;
pub mod profile_store;
mod run_commands;
// `pub` so a future `monkey-cli` `task schedule` subcommand could reuse
// `validate_cron_impl`/`next_occurrences_impl` directly, the same
// AppHandle-free-core reasoning as every other module above — no such
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
// independent state machine — see the module doc for why it isn't an
// extension of `PermissionState`.
pub mod approval_chains;
pub mod local_apps;

// `Manager` brings `AppHandle::state`/`state::<T>()` into scope — used by
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
    pub llama: std::sync::Mutex<llama::LlamaState>,
    /// The second, embeddings-only managed `llama-server` instance (port
    /// 8091, started with `--embeddings --pooling mean`) used by
    /// `stacks.rs`'s managed-llama embedding backend — a distinct
    /// `LlamaState` from `llama` above (not one process serving both
    /// roles), so a stack reindex never contends with the chat model for
    /// the same server slot. See `llama::embed_server_start`.
    pub embed_llama: std::sync::Mutex<llama::LlamaState>,
    pub ollama: std::sync::Mutex<ollama::OllamaState>,
    /// In-flight `ollama pull`/`ollama create` child processes, keyed by tag
    /// (or short name) — lets `ollama::ollama_cancel_pull` kill a pull the
    /// user started. See `ollama::ollama_pull_model`.
    pub ollama_pulls: std::sync::Mutex<std::collections::HashMap<String, tokio::process::Child>>,
    /// Cancellation handles for in-flight `models_download` calls, keyed by
    /// the destination file name — mirrors `index_cancels`/`ollama_pulls`,
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
    /// spawned them — that is what lets a dev server or watcher keep running
    /// after the tool call returns, and what gives `shell_output`/`shell_kill`
    /// something to address later. Killed on app shutdown.
    pub background_shell: background_shell::BackgroundShellManager,
    pub permissions: permissions::PermissionState,
    /// Cancellation handles for in-flight `providers_stream_chat` requests,
    /// keyed by `request_id` — see `providers::providers_cancel_chat`.
    pub stream_cancels:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
    /// Per-turn file checkpoints currently in flight, keyed by checkpoint id.
    /// With the split pane open, two turns (and thus two checkpoints) can be
    /// active concurrently — see `checkpoints.rs`.
    pub checkpoints:
        std::sync::Mutex<std::collections::HashMap<String, checkpoints::ActiveCheckpoint>>,
    /// Checkpoint ids with a `checkpoint_revert`/`checkpoint_reapply` call
    /// currently in progress. `MessageList.tsx`'s `CheckpointRow` and
    /// `CheckpointTimeline.tsx`'s `TimelineRow` can both render controls for
    /// the same checkpoint at once, and both call these commands with only a
    /// component-local `busy` flag guarding each — nothing shared prevents
    /// two concurrent revert/reapply calls for the same id from racing on
    /// the same `redo/<n>.bak` files. Membership here is that lock — see
    /// `checkpoints::acquire_revert_lock`.
    pub checkpoint_locks: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Per-turn cancellation channels used by `tools::tools_cancel_running`
    /// to kill in-flight `tool_run_shell` child processes when the user hits
    /// Stop — keyed by the owning turn's id (empty string for callers that
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
    /// these groups immediately, and resuming SIGCONTs them — see
    /// `process_commands::process_signal`. Entries are removed the moment the
    /// child is reaped, so a pid is never signalled after the kernel could
    /// have reused it.
    pub shell_process_groups:
        std::sync::Mutex<std::collections::HashMap<String, tools::TurnShellGroups>>,
    /// Serializes `memories.json` read-modify-write cycles (see `memory.rs`)
    /// so two concurrent split-pane `tool_remember` calls can never race and
    /// clobber each other's fact — the whole file is rewritten on every add
    /// or delete, so unsynchronized concurrent writers could silently drop
    /// one of them.
    pub memory_lock: std::sync::Mutex<()>,
    /// Serializes `mcp_servers.json` read-modify-write cycles (see `mcp.rs`)
    /// — same reasoning as `memory_lock` above protects `memories.json`.
    /// `mcp_add_server`/`mcp_update_server` are synchronous commands (Tauri
    /// can dispatch those on genuinely concurrent OS threads) and
    /// `mcp_remove_server`/`mcp_set_enabled` are async commands (the tokio
    /// runtime can run those in parallel too), so without a shared lock two
    /// concurrent config-mutating calls (e.g. two Settings toggles fired
    /// close together) can both load the same "before" config and the
    /// later save silently clobbers the earlier one's change. A plain
    /// `std::sync::Mutex`, not `tokio::sync::Mutex` like `AppState::mcp`:
    /// every critical section this guards is the synchronous
    /// `load_config_impl`/`save_config_impl` pair with no `.await` in
    /// between, so there's nothing async to ever hold it across.
    pub mcp_config_lock: std::sync::Mutex<()>,
    /// Serializes `connectors.json` read-modify-write cycles (see
    /// `connectors.rs`) — same reasoning as `mcp_config_lock` protects
    /// `mcp_servers.json`: `connectors_add_github`/`connectors_remove` are
    /// synchronous commands (Tauri can dispatch those onto genuinely
    /// concurrent OS threads) and `connectors_add_token`/`connectors_add_s3`/
    /// `connectors_reverify` are async commands (the tokio runtime can run
    /// those in parallel too), so without a shared lock two concurrent
    /// config-mutating calls could both load the same "before" catalog and
    /// the later save silently clobbers the earlier one's change. A plain
    /// `std::sync::Mutex`: every critical section this guards is a
    /// synchronous `load_config_impl`/`save_config_impl` pair, acquired only
    /// around that pair (never across the `.await`ed verification call
    /// itself), so there's nothing async to ever hold it across.
    pub connectors_config_lock: std::sync::Mutex<()>,
    /// Serializes `triage.json` read-modify-write cycles (see `triage.rs`) —
    /// same reasoning as `connectors_config_lock` protects `connectors.json`:
    /// `triage_refresh`/`triage_generate_draft`/`triage_send_draft` are all
    /// async commands the tokio runtime can run concurrently, so without a
    /// shared lock two concurrent config-mutating calls could both load the
    /// same "before" queue and the later save silently clobbers the earlier
    /// one's change. Acquired only around synchronous `load_config_impl`/
    /// `save_config_impl` pairs, never across an awaited network call.
    pub triage_state_lock: std::sync::Mutex<()>,
    /// Serializes the permission-granted mutation itself (checkpoint backup +
    /// the actual file write) in `tool_write_file`/`tool_edit_file` — same
    /// "two unsynchronized concurrent writers can silently clobber each
    /// other" reasoning as `memory_lock` above, now reachable from ordinary
    /// agent use since subagents (p3) let multiple `code`-profile `task`
    /// calls run genuinely concurrently in the same round
    /// (`agentLoop.ts::runToolCallsForRound`) and share the parent turn's
    /// checkpoint id. Without this, two concurrent calls that both resolve to
    /// the SAME workspace path can interleave past `request_permission`'s
    /// `.await` and race on `checkpoints::record_original` + the mutation
    /// itself, silently discarding whichever write lands first with no error
    /// surfaced to either caller. Acquired only around the synchronous
    /// backup+write critical section (after permission has already been
    /// granted) — never across an `.await` — so a plain `std::sync::Mutex` is
    /// enough, same as `mcp_config_lock`/`web_settings_lock`. A single
    /// workspace-wide lock (not keyed per-path) is deliberately simpler than a
    /// per-path lock map: file writes are fast, and correctness here matters
    /// far more than intra-turn write parallelism.
    pub file_write_lock: std::sync::Mutex<()>,
    /// Live MCP server connections, keyed by server id (see `mcp.rs`). A
    /// `tokio::sync::Mutex` — unlike every other map here — because
    /// connecting and calling a tool are both `.await`-ing operations; every
    /// caller clones the cheap `Peer` handle out (or swaps the whole
    /// connection in/out) and drops the guard before awaiting anything on
    /// the connection itself, so this lock is never held across a
    /// `call_tool`/`connect`/`disconnect` await.
    pub mcp: tokio::sync::Mutex<std::collections::HashMap<String, mcp::McpConnection>>,
    /// Per-server cancellation signal for an in-flight `mcp_oauth_connect`
    /// (see `mcp_oauth.rs`) — keyed by server id. `mcp_oauth_cancel` calls the
    /// shared `CancellationToken`'s `cancel()` method; every overlapping
    /// connect races against `cancelled()`. Cancellation is sticky, so a
    /// cancel arriving immediately before the waiter is polled cannot be lost
    /// (unlike `Notify::notify_waiters`). The tuple's active-attempt count is
    /// updated under the same mutex as the map, so the final overlapping call
    /// removes the entry atomically and the next independent attempt receives
    /// a fresh token.
    pub mcp_oauth_cancel: std::sync::Mutex<
        std::collections::HashMap<
            String,
            (
                std::sync::Arc<tokio_util::sync::CancellationToken>,
                usize,
            ),
        >,
    >,
    /// Per-server async lock serializing OAuth access-token retrieval/refresh
    /// (see `mcp_oauth::get_access_token_if_connected`) — keyed by server id.
    /// `connect_impl`'s `Http` branch calls that function on every
    /// `mcp_connect`, and nothing else serializes concurrent connects for the
    /// same server id; without this, two overlapping connects (e.g. a
    /// double-click on Reconnect, or an auto-reconnect racing a manual one)
    /// can both read the same still-current refresh token and POST it to the
    /// token endpoint concurrently. For an authorization server that rotates
    /// refresh tokens on use, the second request is rejected and that connect
    /// fails with a misleading "authorization expired/revoked" error even
    /// though the other attempt just saved valid, fresh credentials. A plain
    /// `std::sync::Mutex` guarding the *map* (never held across an `.await`);
    /// each server id's own `tokio::sync::Mutex` is what's actually held
    /// across the refresh call.
    pub mcp_oauth_refresh_locks:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// Pending hosted-OAuth attempts (see `hosted_oauth.rs`), keyed by the
    /// random `state` value embedded in the authorize URL sent to the
    /// browser — looked up when the `littlemonkey://oauth/callback` deep
    /// link comes back, so a spoofed/stale deep link (wrong or unknown
    /// `state`) is dropped rather than trusted. Entries are removed once
    /// redeemed (success or error) or found stale (see
    /// `hosted_oauth::PENDING_TTL`) on next insert.
    pub hosted_oauth_pending:
        std::sync::Mutex<std::collections::HashMap<String, hosted_oauth::PendingHostedOAuth>>,
    /// Serializes `web_settings.json` writes (see `web.rs::web_set_settings`)
    /// — same reasoning as `mcp_config_lock` protects `mcp_servers.json`.
    /// `web_set_settings` is a synchronous command (Tauri can dispatch two
    /// calls onto genuinely concurrent OS threads), and its save is a plain
    /// temp-file-write-then-rename with a deterministic temp path; without
    /// this lock, two concurrent saves can both `std::fs::write` the same
    /// `web_settings.json.tmp` at once and leave a torn/interleaved file
    /// behind for whichever `rename` lands last to publish. A plain
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the guarded section is
    /// synchronous with no `.await` in between.
    pub web_settings_lock: std::sync::Mutex<()>,
    /// In-memory lifecycle state for the local OpenAI-compatible API server
    /// (`server.rs`) — mirrors `llama: Mutex<llama::LlamaState>` above.
    /// Binds `127.0.0.1` only and exposes just `/health`, `/v1/models`,
    /// `/v1/chat/completions` — never the agent tools (`tool_run_shell` et
    /// al. over HTTP would be a remote-code-execution surface, an explicit
    /// non-goal of this feature — see `server.rs`'s module doc).
    pub api_server: std::sync::Mutex<server::ApiServerState>,
    /// Serializes `api_server.json` read-modify-write cycles (config
    /// get/set, token create/revoke, and the per-request `last_used_at`
    /// bump) — same reasoning as `web_settings_lock`/`mcp_config_lock`
    /// protect their own files: several of these are synchronous commands
    /// Tauri can dispatch onto genuinely concurrent OS threads, and without
    /// a shared lock two concurrent read-modify-write cycles (e.g. minting
    /// two tokens back to back, or a token-used bump racing a revoke) could
    /// both load the same "before" file and the later save silently
    /// clobbers the earlier one's change.
    pub api_server_config_lock: std::sync::Mutex<()>,
    /// Published tier-2 (interactive HTML) artifacts, keyed by a
    /// server-generated uuid — served by the `artifact://` custom protocol
    /// registered below. See `artifacts.rs`'s module doc for the full
    /// security model; content lives only here in memory, never on disk.
    pub artifacts:
        std::sync::Mutex<std::collections::HashMap<String, artifacts::PublishedArtifact>>,
    /// Lazily opened app-private content-addressed store. The handle itself is
    /// cloneable and performs atomic filesystem publication, so the mutex only
    /// protects one-time initialization rather than serializing blob I/O.
    pub durable_artifacts: std::sync::Mutex<Option<artifact_store::ArtifactStore>>,
    /// Single SQLite writer owned by this desktop host. The CLI/daemon open
    /// the same ledger through `RunLedger` directly; Tauri commands serialize
    /// desktop mutations through this mutex and assign audit metadata in Rust.
    pub run_ledger: std::sync::Mutex<Option<run_ledger::RunLedger>>,
    /// Lazily-loaded (chunks + vectors) cache for `stacks_query`, keyed by
    /// stack id, invalidated (removed) whenever a stack is reindexed or
    /// deleted — see `stacks.rs::load_stack_cached`.
    pub stack_cache:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<stacks::LoadedStack>>>,
    /// Cancellation handles for in-flight `stacks_reindex` calls, keyed by
    /// stack id — mirrors `stream_cancels`/`tool_cancel` above, but a
    /// `tokio_util::sync::CancellationToken` rather than a plain
    /// `tokio::sync::Notify`: a `CancellationToken`'s cancelled state is
    /// persisted, so a cancel request is never silently lost just because it
    /// arrived before anything happened to be awaiting it (`Notify`'s
    /// `notify_waiters()` has exactly that gap — see
    /// `stacks::reindex_impl`'s doc comment for the failure mode this
    /// avoids). See `stacks::stacks_cancel_index`.
    pub index_cancels: std::sync::Mutex<
        std::collections::HashMap<String, std::sync::Arc<tokio_util::sync::CancellationToken>>,
    >,
    /// In-flight Human Approval Chain stages (ROADMAP.md, Phase 3) — see
    /// `approval_chains.rs`'s module doc. A separate state machine from
    /// `permissions` above, not an extension of it.
    pub approval_chains: approval_chains::ApprovalChainState,
    /// Serializes `local_apps.json` read-modify-write cycles (publish/
    /// unpublish) — same reasoning as `api_server_config_lock`.
    pub local_apps_config_lock: std::sync::Mutex<()>,
    /// Serializes `privacy_firewall/<workspace>.json` read-modify-write
    /// cycles (see `privacy_firewall::privacy_firewall_save_policy`) — same
    /// reasoning as `mcp_config_lock`/`web_settings_lock` above: a
    /// synchronous command Tauri can dispatch onto genuinely concurrent OS
    /// threads, so without a shared lock two concurrent policy edits (e.g.
    /// two Settings toggles fired close together) could both load the same
    /// "before" file and the later save silently clobbers the earlier one's
    /// change.
    pub privacy_firewall_lock: std::sync::Mutex<()>,
    /// Previewed-but-not-yet-decided `RequireApproval` sends, keyed by the
    /// previewed content's own SHA-256 digest — the server-side half of
    /// `privacy_firewall`'s two-phase prepare/execute pattern (mirrors
    /// `m5_delivery`'s confirmation-preview shape). Entries are single-use
    /// and TTL-bounded; see `privacy_firewall::{prepare_send_impl,
    /// execute_send_impl}`.
    pub pending_privacy_sends: privacy_firewall::PendingPrivacySends,
    /// In-memory registry of prepared-but-unconfirmed sandbox promote
    /// previews (see `sandbox.rs`'s module doc for why this is intentionally
    /// not persisted like `m5_delivery`'s SQLite-backed preview store).
    pub sandbox: sandbox::SandboxState,
    /// Serializes `team_members.json` read-modify-write cycles (see
    /// `team_mode.rs`) so two concurrent member-roster mutations (e.g. an add
    /// racing a role change, or two removes racing each other) can never both
    /// load the same "before" file and have the later save silently clobber
    /// the earlier one's change — same reasoning as `connectors_config_lock`/
    /// `memory_lock` above protect their own files.
    pub team_members_lock: std::sync::Mutex<()>,
    /// Serializes `runtime_pr_watcher/state.json`'s load-merge-save cycle
    /// (see `runtime_pr_watcher::check_now_core`) — same reasoning as
    /// `team_members_lock`/`connectors_config_lock` above. Only guards the
    /// final synchronous save, not the network fetch that precedes it (a
    /// `std::sync::Mutex` guard can't be held across an `.await`); see that
    /// function's doc comment for how a concurrent check is still
    /// reconciled correctly.
    pub runtime_pr_watcher_lock: std::sync::Mutex<()>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            generation_engine: Default::default(),
            llama: Default::default(),
            embed_llama: std::sync::Mutex::new(llama::LlamaState::for_embeddings()),
            ollama: Default::default(),
            ollama_pulls: Default::default(),
            model_downloads: Default::default(),
            workspace_roots: Default::default(),
            terminal: Default::default(),
            background_shell: Default::default(),
            permissions: Default::default(),
            stream_cancels: Default::default(),
            checkpoints: Default::default(),
            checkpoint_locks: Default::default(),
            tool_cancel: Default::default(),
            shell_process_groups: Default::default(),
            memory_lock: Default::default(),
            mcp_config_lock: Default::default(),
            connectors_config_lock: Default::default(),
            triage_state_lock: Default::default(),
            file_write_lock: Default::default(),
            mcp: Default::default(),
            mcp_oauth_cancel: Default::default(),
            mcp_oauth_refresh_locks: Default::default(),
            hosted_oauth_pending: Default::default(),
            web_settings_lock: Default::default(),
            api_server: Default::default(),
            api_server_config_lock: Default::default(),
            artifacts: Default::default(),
            durable_artifacts: Default::default(),
            run_ledger: Default::default(),
            stack_cache: Default::default(),
            index_cancels: Default::default(),
            approval_chains: Default::default(),
            local_apps_config_lock: Default::default(),
            privacy_firewall_lock: Default::default(),
            pending_privacy_sends: Default::default(),
            sandbox: Default::default(),
            team_members_lock: Default::default(),
            runtime_pr_watcher_lock: Default::default(),
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app_data_dir = app_paths::data_dir()
        .expect("the operating system must provide an application data directory");
    let m3_state = m3_production::build_m3_command_state(&app_data_dir)
        .expect("failed to initialize the local runtime and API hub");
    let quantization_state = m3_production::build_quantization_command_state(&app_data_dir)
        .expect("failed to initialize the model conversion and quantization workbench");
    let m4_state = m4_commands::M4CommandState::production(&app_data_dir)
        .expect("failed to initialize packages, MCP Apps, and workflow services");
    let native_skills_state =
        native_skill_commands::NativeSkillsCommandState::production(&app_data_dir)
            .expect("failed to initialize the native SKILL.md runtime");
    let browser_state = browser_worker::BrowserCommandState::production(&app_data_dir)
        .expect("failed to initialize the isolated browser worker");
    let m7_state = m7_companion::M7CompanionState::production(&app_data_dir)
        .expect("failed to initialize the desktop companion");
    let configured_companion_shortcut = m7_state
        .overlay_shortcut()
        .expect("failed to load the configured companion shortcut");
    let palette_state = command_palette::CommandPaletteState::production(&app_data_dir)
        .expect("failed to initialize the command palette");
    let configured_palette_shortcut = palette_state
        .shortcut()
        .expect("failed to load the configured command palette shortcut");
    // Machine-wide lock at <app_data>/desktop_control.lock so the local app
    // and the resident daemon (which constructs its own DesktopControlState)
    // can never drive real OS input simultaneously.
    let desktop_control_state = desktop_control::DesktopControlState::production_with_lock(
        app_data_dir.join("desktop_control.lock"),
    );
    // Fixed (not user-configurable, unlike the companion overlay shortcut
    // above) global emergency-stop hotkey — see ROADMAP.md's Safe Desktop
    // Control acceptance criteria ("Emergency stop hotkey") and the design
    // doc's kill-switch requirement. Registered on the same shortcut manager
    // as the companion overlay shortcut (one `tauri_plugin_global_shortcut`
    // plugin instance per app), disambiguated in the shared handler below by
    // comparing the fired `Shortcut` against this parsed constant.
    const DESKTOP_CONTROL_EMERGENCY_STOP_SHORTCUT: &str = "CommandOrControl+Shift+Escape";
    let desktop_control_emergency_stop_shortcut: tauri_plugin_global_shortcut::Shortcut =
        DESKTOP_CONTROL_EMERGENCY_STOP_SHORTCUT
            .parse()
            .expect("the desktop control emergency-stop hotkey must be valid");
    // All three global OS-level shortcuts (the companion overlay's, the
    // command palette's, and desktop control's fixed emergency stop) share
    // one `tauri_plugin_global_shortcut` plugin registration — a Tauri app
    // manages exactly one instance of each plugin — and one dispatching
    // handler that tells them apart by comparing the fired `Shortcut`
    // against each feature's configured, already-parsed value
    // (`Shortcut`/`HotKey` derives `PartialEq`).
    let companion_shortcut_parsed = configured_companion_shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .expect("the configured companion shortcut must be valid");
    let palette_shortcut_parsed = configured_palette_shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .expect("the configured command palette shortcut must be valid");
    let global_shortcuts = tauri_plugin_global_shortcut::Builder::new()
        .with_shortcut(configured_companion_shortcut.as_str())
        .expect("the configured companion shortcut must be valid")
        .with_shortcut(configured_palette_shortcut.as_str())
        .expect("the configured command palette shortcut must be valid")
        .with_shortcut(DESKTOP_CONTROL_EMERGENCY_STOP_SHORTCUT)
        .expect("the desktop control emergency-stop hotkey must be valid")
        .with_handler(move |app, shortcut, event| {
            if event.state != tauri_plugin_global_shortcut::ShortcutState::Pressed {
                return;
            }
            if *shortcut == desktop_control_emergency_stop_shortcut {
                let state = app.state::<desktop_control::DesktopControlState>();
                let _ = state.emergency_stop();
                if let Some(overlay) = app.get_webview_window("companion-overlay") {
                    let _ = overlay.hide();
                }
            } else if *shortcut == companion_shortcut_parsed {
                let _ = m7_companion::show_overlay(app);
            } else if *shortcut == palette_shortcut_parsed {
                let _ = command_palette::show_palette(app);
            }
        })
        .build();
    let app = tauri::Builder::default()
        // Must be registered first (per tauri-plugin-single-instance's own
        // docs) so it can intercept a second launch before anything else
        // runs. Only matters on Windows/Linux, where a `littlemonkey://`
        // deep link while the app is already running spawns a second OS
        // process rather than delivering a live event to the first one (see
        // `hosted_oauth.rs`'s module doc) — this pipes that second launch's
        // argv into the already-running instance instead of letting a
        // second, windowless copy of the app start up.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(url) = hosted_oauth::extract_deep_link_from_argv(&argv) {
                hosted_oauth::handle_deep_link_urls(app, vec![url]);
            }
        }))
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .plugin(global_shortcuts)
        .manage(AppState::default())
        .manage(m3_state)
        .manage(quantization_state)
        .manage(m3_http_server::M3HttpServerState::default())
        .manage(m4_state)
        .manage(native_skills_state)
        .manage(browser_state)
        .manage(browser_pane::BrowserPaneState::default())
        .manage(m7_state)
        .manage(palette_state)
        .manage(desktop_control_state)
        // Tier-2 interactive-artifact protocol — serves a previously
        // `artifact_publish`-ed document by id with a strict per-document
        // CSP (`connect-src 'none'`, no capability granted to this scheme —
        // see `artifacts.rs`'s module doc for the full security argument).
        // The frontend's consuming iframe uses `sandbox="allow-scripts"`
        // WITHOUT `allow-same-origin`, so this protocol is never reachable
        // with IPC/cookies/storage regardless of what it serves.
        .register_uri_scheme_protocol("artifact", |ctx, request| {
            artifacts::handle_request(ctx.app_handle().state::<AppState>().inner(), &request)
        })
        .setup(|app| {
            // Listens for `littlemonkey://` deep links (macOS/mobile live
            // events; Windows/Linux fresh-launch CLI-arg parsing — a
            // still-running instance on those platforms is covered by the
            // single-instance plugin's closure above instead). See
            // `hosted_oauth.rs`'s module doc for the full flow.
            hosted_oauth::register(app.handle());

            // Verify and copy the bundled llama.cpp runtime into app data at
            // launch so the separately installed `monkey` CLI can use the
            // same app-owned runtime without Ollama or a system install.
            // A source/dev build may intentionally have no staged bundle;
            // model start still fails closed if a present bundle is invalid.
            if let Ok(runtime_app_data) = app.path().app_data_dir() {
                let resource_dir = app.path().resource_dir().ok();
                if let Err(error) = managed_runtime::materialize_bundled_runtime(
                    resource_dir.as_deref(),
                    &runtime_app_data,
                ) {
                    eprintln!("Managed llama.cpp runtime setup failed: {error}");
                }
            }

            // Finish or roll back any portable-profile transaction interrupted
            // between staged file publication and its durable commit marker.
            // This runs before session/prompt hydration and before the profile
            // migration task, so no consumer can observe a mixed restore.
            {
                let state = app.state::<AppState>();
                portability_commands::recover_pending_portable_restores(
                    app.handle(),
                    state.inner(),
                )
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
            }

            // Best-effort, silent `monkey` PATH shim install — see
            // `cli_install.rs`'s module doc. Spawned on a blocking thread
            // (it does filesystem/registry I/O, no `.await` points) so
            // `setup` still returns promptly; failures are swallowed here on
            // purpose, matching every other autostart-style setup step in
            // this function — a missing/failed CLI shim is never worth
            // interrupting the GUI launching.
            tauri::async_runtime::spawn_blocking(|| {
                let _ = cli_install::install_if_needed();
            });

            // Idempotently migrate the exact legacy JSON snapshot into the
            // transactional profile/index while preserving that file and its
            // first recovery copy. A corrupt source is left untouched and
            // remains visible through profile_migration_status.
            let profile_app = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                let state = profile_app.state::<AppState>();
                let Ok(status) =
                    profile_commands::current_migration_status(&profile_app, state.inner())
                else {
                    return;
                };
                if matches!(
                    status.state,
                    profile_store::MigrationState::Pending
                        | profile_store::MigrationState::SourceChanged
                ) {
                    let _ = profile_commands::migrate_current_profile(&profile_app, state.inner());
                }
            });

            // Autostart the local API server if `api_server.json` says to —
            // the only reader of that file at launch time, since every
            // other consumer (the Settings panel) fetches it on demand via
            // `api_server_get_config`. Spawned rather than awaited here:
            // `setup` must return promptly, and a bind failure just leaves
            // the server in its default "stopped"/"error" state for the
            // user to retry from the panel, the same as any other failed
            // manual start.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let Ok(path) = server::config_file_path(&app_handle) else {
                    return;
                };
                let Ok(config) = server::load_config_impl(&path) else {
                    return;
                };
                if config.autostart {
                    let _ = server::api_server_start(app_handle).await;
                }
            });

            // A previous session's chat turns, subagents, crew members,
            // background shells and side tasks died with that process, so any
            // still marked live in the process table are stale. Reaped here,
            // before any new turn can admit one, and scoped to the kinds this
            // app owns so live daemon work is never declared lost.
            {
                let reap_app = app.handle().clone();
                let reap_state = reap_app.state::<AppState>();
                process_commands::reap_desktop_processes_at_startup(&reap_app, reap_state.inner());
            }

            // A persisted M3 policy represents an explicit user opt-in. Start
            // its separate, capability-scoped compatibility listener without
            // blocking app launch; failures remain visible in Runtime Hub.
            // Reconciles every enabled `WatchedFolder` Knowledge Sync source
            // against the live filesystem-watcher registry so a watcher
            // started in a previous session resumes across app restarts —
            // afterward, `knowledge_service`'s add/update/remove-source
            // commands keep it in sync as the catalog changes.
            knowledge_service::sync_watched_folder_watchers(app.handle());

            let m3_http_app = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let hub = m3_http_app
                    .state::<m3_commands::M3CommandState>()
                    .hub
                    .clone();
                if hub.lan_policy().ok().flatten().is_none() {
                    return;
                }
                let server = m3_http_app.state::<m3_http_server::M3HttpServerState>();
                let _ = m3_http_server::start_server_core(&server, hub).await;
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            browser_pane::browser_pane_open_tab,
            browser_pane::browser_pane_close_tab,
            browser_pane::browser_pane_select_tab,
            browser_pane::browser_pane_set_bounds,
            browser_pane::browser_pane_set_visible,
            browser_pane::browser_pane_navigate,
            browser_pane::browser_pane_go_back,
            browser_pane::browser_pane_go_forward,
            browser_pane::browser_pane_reload,
            browser_pane::browser_pane_favicon,
            cli_install::cli_install_status,
            cli_install::cli_install_set_enabled,
            llama::llama_start,
            llama::llama_stop,
            llama::llama_status,
            llama::embed_server_start,
            llama::embed_server_stop,
            llama::embed_server_status,
            server::api_server_start,
            server::api_server_stop,
            server::api_server_status,
            server::api_server_get_config,
            server::api_server_set_config,
            server::api_server_create_token,
            server::api_server_revoke_token,
            server::api_server_list_tokens,
            server::api_server_export_audit,
            ollama::ollama_status,
            ollama::ollama_start,
            ollama::ollama_list_models,
            ollama::ollama_list_running_models,
            ollama::ollama_unload_model,
            ollama::ollama_example_cloud_tags,
            ollama::ollama_pull_model,
            ollama::ollama_cancel_pull,
            ollama::ollama_import_model,
            ollama::ollama_create_from_modelfile,
            ollama::ollama_remove_model,
            ollama::ollama_signin,
            modelfile::modelfile_parse,
            modelfile::modelfile_dry_run,
            modelfile::modelfile_read_text_file,
            connectors::connectors_list,
            connectors::connectors_add_github,
            connectors::connectors_add_token,
            connectors::connectors_add_s3,
            connectors::connectors_remove,
            connectors::connectors_reverify,
            connectors::connectors_export_audit,
            triage::triage_refresh,
            triage::triage_list,
            triage::triage_generate_draft,
            triage::triage_send_draft,
            providers::providers_list_configured,
            providers::providers_add_custom,
            providers::providers_remove_custom,
            providers::providers_set_key,
            providers::providers_remove_key,
            providers::providers_list_models,
            providers::providers_check_model_retirements,
            providers::providers_stream_chat,
            providers::providers_cancel_chat,
            models::models_list_curated,
            models::models_list_installed,
            models::models_download,
            models::models_cancel_download,
            models::models_resolve_reference,
            models::models_install_reference,
            models::models_delete,
            models::models_add_external,
            models::models_remove_external,
            process_commands::process_list,
            process_commands::process_get,
            process_commands::process_descendants,
            process_commands::process_live_counts,
            process_commands::process_admit,
            process_commands::process_reconcile,
            process_commands::process_signal,
            process_commands::process_signal_support,
            process_commands::process_pending_signals,
            process_commands::process_deliver_os_signal,
            process_commands::process_transition,
            process_commands::process_link_run,
            process_commands::process_reap_missing,
            permissions::permission_respond,
            permissions::permission_dry_run,
            permissions::set_permission_mode,
            permissions::set_permission_mode_for_turn,
            permissions::clear_permission_mode_for_turn,
            terminal::terminal_identity,
            terminal::terminal_create,
            terminal::terminal_list,
            terminal::terminal_execute,
            terminal::terminal_write,
            terminal::terminal_interrupt,
            terminal::terminal_resize,
            terminal::terminal_kill,
            terminal::terminal_restart,
            terminal::terminal_close,
            terminal::terminal_history,
            tools::tool_read_file,
            tools::tool_list_dir,
            tools::tool_grep,
            tools::tool_glob,
            tools::tool_write_file,
            tools::tool_edit_file,
            tools::tool_generate_image,
            tools::workspace_read_image,
            tools::tool_run_shell,
            background_shell::tool_run_shell_background,
            background_shell::background_shell_output,
            background_shell::background_shell_kill,
            background_shell::background_shell_list,
            background_shell::background_shell_clear_finished,
            tools::tools_cancel_running,
            tools::list_workspace_paths,
            tools::tool_remember,
            tools::tool_read_skill_resource,
            web::tool_web_fetch,
            web::tool_web_search,
            web::web_get_settings,
            web::web_set_settings,
            web::web_has_brave_key,
            web::web_set_brave_key,
            web::web_remove_brave_key,
            rules::rules_read,
            rules::rules_write,
            memory::memory_list,
            memory::memory_add,
            memory::memory_delete,
            memory::memory_update,
            memory::memory_clear,
            memory::memory_list_all,
            memory::memory_studio_update,
            memory::memory_studio_set_enabled,
            memory::memory_studio_delete,
            memory::memory_import,
            sessions::sessions_load,
            sessions::sessions_save,
            profile_commands::profile_migration_status,
            profile_commands::profile_migrate,
            profile_commands::profile_global_search,
            portability_commands::portable_export_bundle,
            portability_commands::portable_read_bundle,
            portability_commands::portable_restore_apply,
            portability_commands::portable_restore_settings_pending,
            portability_commands::portable_restore_settings_acknowledge,
            portability_commands::portable_export_session,
            portability_commands::portable_snapshot_create,
            portability_commands::portable_snapshot_list,
            portability_commands::portable_snapshot_open,
            portability_commands::portable_snapshot_stage_source,
            portability_commands::portable_webdav_config_save,
            portability_commands::portable_webdav_status_get,
            portability_commands::portable_webdav_run_due,
            portability_commands::portable_webdav_test,
            portability_commands::portable_webdav_download_snapshot,
            prompts::prompts_load,
            prompts::prompts_save,
            prompts::prompts_read_external,
            prompts::prompts_write_external,
            checkpoints::checkpoint_begin,
            checkpoints::checkpoint_end,
            checkpoints::checkpoint_revert,
            checkpoints::checkpoint_reapply,
            checkpoints::checkpoint_list,
            checkpoints::checkpoint_preview,
            checkpoints::checkpoint_compare,
            checkpoints::checkpoint_simulate_restore,
            artifacts::artifact_publish,
            artifacts::artifact_remove,
            artifact_commands::artifact_blob_read_base64,
            workspace::set_primary_workspace_root,
            workspace::add_secondary_workspace_root,
            workspace::remove_secondary_workspace_root,
            workspace::get_workspace_roots,
            workspace::restore_workspace_roots,
            workspace::get_recent_workspaces,
            git::git_status,
            git::git_commit,
            git::git_review,
            git::git_changed_files,
            git::git_file_diff,
            mcp::mcp_list_servers,
            mcp::mcp_add_server,
            mcp::mcp_update_server,
            mcp::mcp_remove_server,
            mcp::mcp_set_enabled,
            mcp::mcp_connect,
            mcp::mcp_disconnect,
            mcp::mcp_set_http_token,
            mcp::mcp_remove_http_token,
            mcp::mcp_call_tool,
            bundled_mcp_servers::mcp_stage_bundled_server,
            mcp_oauth::mcp_oauth_connect,
            mcp_oauth::mcp_oauth_redirect_uri,
            mcp_oauth::mcp_oauth_cancel,
            mcp_oauth::mcp_oauth_disconnect,
            hosted_oauth::hosted_oauth_connect,
            hosted_oauth::hosted_oauth_cancel,
            hosted_oauth::hosted_oauth_disconnect,
            system::reveal_in_finder,
            system::open_in_terminal,
            system::open_in_editor,
            system::open_session_window,
            system::system_memory_info,
            verify::verify_get_config,
            verify::verify_set_config,
            verify::verify_run,
            stacks::stacks_list,
            stacks::stacks_create,
            stacks::stacks_delete,
            stacks::stacks_rename,
            stacks::stacks_add_source,
            stacks::stacks_remove_source,
            stacks::stacks_reindex,
            stacks::stacks_cancel_index,
            stacks::stacks_query,
            stacks::stacks_is_stale,
            stacks::tool_search_docs,
            knowledge_service::knowledge_v2_list_sources,
            knowledge_service::knowledge_v2_add_source,
            knowledge_service::knowledge_v2_update_source,
            knowledge_service::knowledge_v2_remove_source,
            knowledge_service::knowledge_v2_refresh,
            knowledge_service::knowledge_v2_cancel_refresh,
            knowledge_service::knowledge_v2_background_config_get,
            knowledge_service::knowledge_v2_background_config_save,
            knowledge_service::knowledge_v2_update_chunking,
            knowledge_service::knowledge_v2_query,
            knowledge_service::knowledge_v2_cancel_query,
            knowledge_service::knowledge_v2_pii_preview,
            knowledge_service::knowledge_ocr_status,
            knowledge_service::knowledge_ocr_configure_external,
            knowledge_service::knowledge_ocr_install,
            knowledge_service::knowledge_ocr_set_enabled,
            recipes::recipes_list,
            recipes::recipes_read,
            recipes::recipes_read_raw,
            recipes::recipes_render,
            recipes::recipes_save,
            recipes::recipes_delete,
            recipes::recipes_validate,
            automations::automations_load,
            automations::automations_save,
            automations::cron_validate,
            automations::cron_next,
            automations::cron_previous,
            team_mode::team_members_list,
            team_mode::team_members_add,
            team_mode::team_members_update_role,
            team_mode::team_members_remove,
            team_mode::team_members_set_active,
            team_mode::team_audit_export,
            run_commands::run_protocol_version,
            run_commands::run_submit,
            run_commands::run_append_event,
            run_commands::run_decide_permission,
            run_commands::run_request_cancellation,
            run_commands::run_get,
            run_commands::run_list,
            run_commands::run_archive,
            run_commands::run_unarchive,
            run_commands::run_events,
            run_commands::run_integrity_check,
            sandbox::sandbox_run,
            sandbox::sandbox_enforcement_probe,
            sandbox::sandbox_list,
            sandbox::sandbox_diff,
            sandbox::sandbox_prepare_promote,
            sandbox::sandbox_execute_promote,
            sandbox::sandbox_discard,
            m3_commands::m3_hardware_snapshot,
            m3_commands::m3_hardware_profile,
            m3_commands::m3_hardware_compatibility_report,
            m3_commands::m3_storage_status,
            m3_commands::m3_installed_models,
            m3_commands::m3_catalog_sources,
            m3_commands::m3_catalog_replace_sources,
            m3_commands::m3_runtimes,
            m3_commands::m3_refresh_runtimes,
            m3_commands::m3_resolve_setting_capabilities,
            m3_commands::m3_schedule_plan,
            m3_commands::m3_chat_template_lab_report,
            m3_commands::m3_offload_plan,
            m3_commands::m3_catalog_search,
            m3_commands::m3_model_staleness_check,
            m3_commands::m3_model_download,
            m3_commands::m3_model_update,
            m3_commands::m3_model_activate_version,
            m3_commands::m3_verify_projector,
            m3_commands::m3_model_prune_versions,
            m3_commands::m3_model_delete,
            m3_commands::m3_cleanup_orphans,
            m3_commands::m3_cancel_operation,
            m3_commands::m3_runtime_status,
            m3_commands::m3_runtime_inventory,
            m3_commands::m3_runtime_load_model,
            m3_commands::m3_runtime_unload_model,
            m3_commands::m3_runtime_logs,
            m3_commands::m3_runtime_metrics,
            m3_commands::m3_context_cache_state,
            m3_commands::m3_context_effective_size,
            m3_commands::m3_classify_context_failure,
            m3_commands::m3_runtime_set_config,
            m3_commands::m3_runtime_config,
            m3_commands::m3_api_dispatch,
            m3_commands::m3_api_cancel_inference,
            m3_commands::m3_compatibility_matrix,
            m3_commands::m3_lan_validate_policy,
            m3_commands::m3_lan_configure,
            m3_commands::m3_lan_disable,
            m3_commands::m3_lan_policy,
            m3_commands::m3_lan_begin_pairing,
            m3_commands::m3_lan_complete_pairing,
            m3_commands::m3_lan_revoke_token,
            m3_commands::m3_lan_tokens,
            m3_commands::m3_lan_audit_events,
            m3_commands::quantization_backends,
            m3_commands::quantization_quant_types,
            m3_commands::quantization_convert_path,
            m3_commands::quantization_convert_installed_model,
            m3_commands::m3_component_storage_status,
            m3_commands::m3_component_installed,
            m3_commands::m3_component_registry_entries,
            m3_commands::m3_component_replace_registry_entries,
            m3_commands::m3_component_list_registry,
            m3_commands::m3_component_check_updates,
            m3_commands::m3_component_install,
            m3_commands::m3_component_activate_version,
            m3_commands::m3_telemetry_record_load,
            m3_commands::m3_telemetry_record_request,
            m3_commands::m3_telemetry_recent_traces,
            m3_commands::m3_telemetry_support_bundle,
            m3_commands::agent_launcher_generate_config,
            m3_commands::agent_launcher_check_drift,
            m3_http_server::m3_http_server_start,
            m3_http_server::m3_http_server_stop,
            m3_http_server::m3_http_server_status,
            m3_http_server::m3_http_server_store_tls_identity,
            m4_commands::m4_packages_seed_first_party,
            m4_commands::m4_packages_import_portable,
            m4_commands::m4_packages_catalog,
            m4_commands::m4_packages_installed,
            m4_commands::m4_packages_active_skills,
            m4_commands::m4_plugins_active_snapshot,
            m4_commands::m4_plugins_runtime,
            m4_commands::m4_plugins_activate_workflow,
            m4_commands::m4_plugins_deactivate_workflow,
            native_skill_commands::native_skills_discover,
            native_skill_commands::native_skills_preview_local,
            native_skill_commands::native_skills_install_local,
            native_skill_commands::native_skills_preview_git,
            native_skill_commands::native_skills_install_git,
            native_skill_commands::native_skills_install_git_bulk,
            native_skill_commands::native_skills_set_enabled,
            native_skill_commands::native_skills_set_enabled_many,
            native_skill_commands::native_skills_uninstall,
            native_skill_commands::native_skills_uninstall_many,
            native_skill_commands::native_skills_rollback,
            native_skill_commands::native_skills_rollback_many,
            security_commands::security_audit,
            diagnostics::diagnostics_run,
            diagnostics::diagnostics_apply_fix,
            diagnostics::diagnostics_export_bundle,
            m4_commands::m4_packages_preview,
            m4_commands::m4_packages_install,
            m4_commands::m4_packages_update,
            m4_commands::m4_packages_set_enabled,
            m4_commands::m4_packages_pin,
            m4_commands::m4_packages_rollback,
            m4_commands::m4_packages_uninstall,
            m4_commands::m4_packages_export,
            m4_commands::m4_packages_set_team_approved,
            m4_commands::m4_registries_list,
            m4_commands::m4_registries_add,
            m4_commands::m4_registries_remove,
            m4_commands::m4_registries_verify,
            m4_commands::m4_mcp_oauth_register,
            m4_commands::m4_mcp_oauth_servers,
            m4_commands::m4_mcp_oauth_begin,
            m4_commands::m4_mcp_oauth_complete,
            m4_commands::m4_mcp_oauth_refresh,
            m4_commands::m4_mcp_oauth_revoke,
            m4_commands::m4_mcp_oauth_metadata,
            m4_commands::m4_mcp_ui_open,
            m4_commands::m4_mcp_ui_authorize_action,
            m4_commands::m4_mcp_ui_prepare_action,
            m4_commands::m4_mcp_ui_decide_action,
            m4_commands::m4_mcp_ui_close,
            m4_commands::m4_workflows_list,
            m4_commands::m4_workflows_load,
            m4_commands::m4_workflows_validate,
            m4_commands::m4_workflows_refresh_capabilities,
            m4_commands::m4_workflows_create,
            m4_commands::m4_workflows_update,
            m4_commands::m4_workflows_import_legacy,
            m4_commands::m4_workflows_delete,
            m4_commands::m4_workflows_run,
            m4_commands::m4_workflows_cancel,
            m4_commands::m4_workflows_prepare_approval,
            m4_commands::m4_workflows_decide_approval,
            m4_commands::m4_workflows_replay,
            m4_commands::m4_workflows_histories,
            m4_commands::m4_workflows_history,
            m4_commands::m4_workflows_inspect_node,
            m4_commands::m4_workflows_reconcile,
            m4_commands::m4_workflows_register_triggers,
            m4_commands::m4_workflows_unregister_triggers,
            browser_worker::browser_start,
            browser_worker::browser_list,
            browser_worker::browser_navigate,
            browser_worker::browser_reload,
            browser_worker::browser_set_viewport,
            browser_worker::browser_inspect,
            browser_worker::browser_annotate,
            browser_worker::browser_click,
            browser_worker::browser_type_text,
            browser_worker::browser_scroll,
            browser_worker::browser_capture_evidence,
            browser_worker::browser_stop,
            daemon_commands::daemon_desktop_status,
            daemon_commands::daemon_desktop_install,
            daemon_commands::daemon_desktop_start,
            daemon_commands::daemon_desktop_stop,
            daemon_commands::daemon_desktop_uninstall,
            daemon_commands::daemon_desktop_queue,
            daemon_commands::daemon_desktop_pause,
            daemon_commands::daemon_desktop_resume,
            daemon_commands::daemon_desktop_cancel,
            daemon_commands::daemon_desktop_retry,
            daemon_commands::daemon_desktop_kill_switch,
            daemon_commands::daemon_desktop_triggers,
            m6a_desktop_bridge::m6a_desktop_turn_submit,
            daemon_commands::daemon_desktop_sync_recipe_schedules,
            daemon_commands::remote_host_status,
            daemon_commands::remote_host_configure,
            daemon_commands::remote_host_disable,
            daemon_commands::remote_pair_create,
            daemon_commands::remote_pair_list,
            daemon_commands::remote_pair_revoke,
            daemon_commands::remote_pair_rotate,
            daemon_commands::remote_audit,
            m5_delivery::m5_delivery_prepare_mutation,
            m5_delivery::m5_delivery_execute_mutation,
            m5_delivery::m5_delivery_list_worktrees,
            m5_delivery::m5_delivery_inspect_worktree,
            m5_delivery::m5_delivery_audit,
            m5_delivery::m5_delivery_reconciliations,
            m5_delivery::m5_github_auth_status,
            m5_delivery::m5_github_issue,
            m5_delivery::m5_github_pull_request,
            m5_delivery::m5_github_review_threads,
            m5_delivery::m5_github_checks,
            m5_delivery::m5_review_pull_request,
            m5_delivery::m5_review_reports,
            issue_to_pr::issue_to_pr_start,
            issue_to_pr::issue_to_pr_status,
            issue_to_pr::issue_to_pr_list,
            issue_to_pr::issue_to_pr_cancel,
            issue_to_pr::issue_to_pr_advance,
            issue_to_pr::issue_to_pr_run_checks,
            approval_chains::approval_chains_list_templates,
            approval_chains::approval_chains_start,
            approval_chains::approval_chain_respond,
            approval_chains::approval_chains_history,
            local_apps::local_apps_publish,
            local_apps::local_apps_list,
            local_apps::local_apps_unpublish,
            local_apps::local_apps_open,
            m7_companion::m7_overlay_show,
            m7_companion::m7_overlay_hide,
            m7_companion::m7_overlay_submit,
            m7_companion::m7_config_get,
            m7_companion::m7_config_save,
            m7_companion::m7_capture_grant,
            m7_companion::m7_capture_revoke,
            m7_companion::m7_capture_grants,
            m7_companion::m7_capture_text,
            m7_companion::m7_capture_file,
            m7_companion::m7_capture_screen,
            m7_companion::m7_transcribe_file,
            m7_companion::m7_transcribe_audio,
            m7_companion::m7_tts_speak,
            m7_companion::m7_job_cancel,
            generation_commands::generation_engine_status,
            generation_commands::generation_models,
            generation_commands::generation_add_model,
            generation_commands::generation_remove_model,
            generation_commands::generation_accept_license,
            generation_commands::generation_download_model,
            generation_commands::generation_cancel_download,
            generation_commands::generation_loras,
            generation_commands::generation_add_lora,
            generation_commands::generation_remove_lora,
            generation_commands::generation_run,
            generation_commands::generation_cancel,
            generation_commands::generation_gallery,
            generation_commands::generation_delete_entry,
            generation_commands::generation_media_data_url,
            generation_commands::generation_unload_engine,
            m7_companion::m7_image_generate,
            m7_companion::m7_image_gallery,
            m7_companion::m7_image_data_url,
            m7_companion::m7_image_insert_chat,
            m7_companion::m7_emergency_stop,
            command_palette::palette_show,
            command_palette::palette_config_get,
            command_palette::palette_config_save,
            privacy_firewall::privacy_firewall_get_policy,
            privacy_firewall::privacy_firewall_save_policy,
            privacy_firewall::privacy_firewall_preview,
            privacy_firewall::privacy_firewall_prepare_send,
            privacy_firewall::privacy_firewall_execute_send,
            desktop_control::desktop_control_start_session,
            desktop_control::desktop_control_stop_session,
            desktop_control::desktop_control_sessions,
            desktop_control::desktop_control_request_action,
            desktop_control::desktop_control_respond_action,
            desktop_control::desktop_control_emergency_stop,
            runtime_pr_watcher::runtime_pr_watcher_state,
            runtime_pr_watcher::runtime_pr_watcher_check_now,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // `App::run` never returns — once the event loop is done, the
        // underlying `tao` runtime calls `std::process::exit` directly
        // (see its own doc comment), which skips Rust's Drop-based cleanup
        // entirely. That means any live MCP stdio child process (held in
        // `AppState::mcp`, cleaned up only via `McpConnection::service`'s
        // `Drop`/`.cancel()`) — and either managed `llama-server` child
        // process (`AppState::llama`/`AppState::embed_llama`, neither of
        // which has a `Drop` impl either) — would otherwise be silently
        // orphaned on every normal app quit. `RunEvent::Exit` fires
        // synchronously on the main thread right before that happens, so
        // doing both kinds of cleanup here — `mcp::disconnect_all` (bounded
        // and best-effort — see its own doc comment) and
        // `llama::stop_all_blocking` (synchronous, no `AppHandle`/async
        // runtime required, since a plain `std::process::Child::kill` is all
        // either process needs) — is the only chance those child processes
        // get to actually be killed before the process itself exits.
        if let tauri::RunEvent::Exit = event {
            let m3_http = app_handle.state::<m3_http_server::M3HttpServerState>();
            let _ = tauri::async_runtime::block_on(m3_http_server::stop_server_core(&m3_http));

            let m3 = app_handle.state::<m3_commands::M3CommandState>();
            let _ = m3.cancel_all_and_shutdown_owned(std::time::Duration::from_secs(5));

            let m4 = app_handle.state::<m4_commands::M4CommandState>();
            let _ = m4.shutdown_all_blocking();

            let browser = app_handle.state::<browser_worker::BrowserCommandState>();
            let _ = browser.shutdown_all();

            let companion = app_handle.state::<m7_companion::M7CompanionState>();
            let _ = companion.emergency_stop();

            let desktop_control = app_handle.state::<desktop_control::DesktopControlState>();
            let _ = desktop_control.emergency_stop();

            let state = app_handle.state::<AppState>();
            state.terminal.kill_all(Some(app_handle));
            state.background_shell.kill_all();
            tauri::async_runtime::block_on(mcp::disconnect_all(state.inner()));
            llama::stop_all_blocking(state.inner());
        }
    });
}

//! Read-only operational-health diagnostics across every subsystem Little
//! Monkey manages itself, plus a conservative, always-explicit repair pass.
//!
//! Sibling to `security_doctor.rs`, not a replacement for it: that module
//! audits security *posture* (file permissions, TLS, insecure MCP origins,
//! runtime grants); this one audits operational *health* — whether a
//! service the app itself owns (or a thing the app depends on) is actually
//! reachable, not whether it is configured safely. Same shape for the same
//! reason: a Tauri-free, injectable-runtime-snapshot engine (so every check
//! is unit-testable without a live process, network call, or OS keychain)
//! plus a thin `#[tauri::command]` layer that gathers the real snapshot from
//! `AppState` and the filesystem.
//!
//! The engine never mutates anything itself. Every fixable finding names an
//! id this module's dispatch table recognizes, and `diagnostics_apply_fix`
//! calls the exact existing start/stop/enable/reindex command that subsystem
//! already exposes for that exact purpose — this module never re-implements
//! process management, config mutation, or index rebuilding.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::AppState;

pub const DIAGNOSTICS_SCHEMA_VERSION: u32 = 1;

/// Keychain service identifier — same constant Little Monkey uses everywhere
/// else it touches the OS keychain (see `mcp.rs::KEYCHAIN_SERVICE`).
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.app";
/// A single fixed, namespaced, never-user-facing account used only for the
/// write/read/delete round trip below. Never holds anything but a fresh
/// random probe value, and is deleted again before the probe returns.
const KEYCHAIN_PROBE_ACCOUNT: &str = "diagnostics:keychain-probe";

/// GPU-layer default used when restarting the chat `llama-server` instance —
/// mirrors `src/store/modelStore.ts`'s own `DEFAULT_GPU_LAYERS` exactly (the
/// same value a normal "Start" click in Settings > Local models already
/// launches with), so a diagnostics-triggered restart behaves identically to
/// a manual one rather than inventing a second, potentially-drifting
/// default. Context size isn't listed here: both paths pass `None` and let
/// `llama::llama_start`'s own `resolve_ctx_size` auto-detect it from the
/// model's GGUF metadata, so there's only one place that decision is made.
const LLAMA_RESTART_GPU_LAYERS: i32 = 999;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticStatus {
    Pass,
    Info,
    Warning,
    Critical,
    Fixed,
    /// The subsystem isn't present in this build/codebase revision at all
    /// (e.g. no connector catalog, no mobile pairing) — deliberately
    /// distinct from `Info`, which means "checked and fine".
    NotConfigured,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticFinding {
    pub id: String,
    pub subsystem: String,
    pub title: String,
    pub detail: String,
    pub status: DiagnosticStatus,
    pub fixable: bool,
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticSummary {
    pub passed: usize,
    pub informational: usize,
    pub warnings: usize,
    pub critical: usize,
    pub fixed: usize,
    pub not_configured: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticReport {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub summary: DiagnosticSummary,
    pub findings: Vec<DiagnosticFinding>,
}

/// A redacted support bundle: the same report `diagnostics_run` returns,
/// plus coarse non-identifying environment context. Findings never carry
/// prompts, chat content, tokens, or secrets in the first place (`id`/
/// `subsystem`/`title`/`detail`/`remediation` are all either static strings
/// or app-owned ids such as an MCP server id or stack id), so no separate
/// scrubbing pass is needed to hold the same "no prompts, no secrets, by
/// default" bar `security_audit` holds for its own reports.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsBundle {
    pub schema_version: u32,
    pub generated_at_ms: u64,
    pub app_version: String,
    pub platform: String,
    pub report: DiagnosticReport,
}

// ---------------------------------------------------------------------
// Injectable runtime snapshot — everything that requires a live process,
// network call, or `AppState` lock, gathered once by the command layer so
// the engine itself stays pure and directly unit-testable (mirrors
// `security_doctor::SecurityRuntimeSnapshot`).
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ManagedServerSnapshot {
    /// Mirrors `llama::LlamaState::status` / `server::ApiServerState::status`
    /// exactly (already a plain lowercase string on both, not an enum).
    pub status: String,
    pub model_path: Option<String>,
    /// Only meaningful when `status == "ready"`: the result of a live
    /// `GET /health` probe against the managed instance's port. `None` when
    /// no probe was attempted (status wasn't "ready").
    pub health_reachable: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct DiagnosticsRuntimeSnapshot {
    pub ollama_reachable: bool,
    pub llama: ManagedServerSnapshot,
    pub embed_llama: ManagedServerSnapshot,
    /// Mirrors `server::ApiServerState::status` — `"stopped" | "starting" |
    /// "running" | "error"`.
    pub api_server_status: String,
    /// Server ids currently present in `AppState::mcp` (i.e. actually
    /// connected right now), not merely configured.
    pub mcp_connected_ids: BTreeSet<String>,
    pub daemon_installed: bool,
    pub daemon_service_running: bool,
    /// Outcome of a real OS-keychain write/read/delete round trip. `None`
    /// only ever appears in a snapshot a test built by hand and chose not to
    /// populate; the production command always fills this in.
    pub keychain_probe: Option<Result<(), String>>,
}

#[derive(Debug, Clone)]
pub struct DiagnosticsRequest {
    pub app_data_dir: PathBuf,
    pub runtime: DiagnosticsRuntimeSnapshot,
}

pub fn run_diagnostics(request: &DiagnosticsRequest) -> Result<DiagnosticReport, String> {
    let mut findings = Vec::new();
    audit_ollama(&request.runtime, &mut findings);
    audit_managed_server(
        "llama",
        "Local chat model (llama-server)",
        &request.runtime.llama,
        &mut findings,
    );
    audit_managed_server(
        "embed_llama",
        "Local embeddings model (llama-server)",
        &request.runtime.embed_llama,
        &mut findings,
    );
    audit_api_server(&request.app_data_dir, &request.runtime, &mut findings);
    audit_mcp(&request.app_data_dir, &request.runtime, &mut findings);
    audit_knowledge_index(&request.app_data_dir, &mut findings);
    audit_automation_daemon(&request.runtime, &mut findings);
    audit_keychain(&request.runtime, &mut findings);
    audit_connectors(&mut findings);
    audit_remote_pairing(&mut findings);

    let summary = summarize(&findings);
    Ok(DiagnosticReport {
        schema_version: DIAGNOSTICS_SCHEMA_VERSION,
        generated_at_ms: now_ms(),
        summary,
        findings,
    })
}

// ---------------------------------------------------------------------
// Per-subsystem checks
// ---------------------------------------------------------------------

/// Ollama is an optional, independently-installed daemon Little Monkey never
/// owns the lifecycle of (see `ollama.rs`'s module doc) — being unreachable
/// is a normal, common state, not a health problem, so this is informational
/// only and never fixable from here.
fn audit_ollama(runtime: &DiagnosticsRuntimeSnapshot, findings: &mut Vec<DiagnosticFinding>) {
    if runtime.ollama_reachable {
        findings.push(finding(
            "ollama.reachability",
            "ollama",
            "Ollama is reachable",
            "The local Ollama daemon responded to a version check.",
            DiagnosticStatus::Pass,
            false,
            None,
        ));
    } else {
        findings.push(finding(
            "ollama.reachability",
            "ollama",
            "Ollama is not reachable",
            "Ollama is an optional, separately-installed model provider. It is either not installed or not currently running — this is expected if you don't use it.",
            DiagnosticStatus::Info,
            false,
            None,
        ));
    }
}

/// Shared health check for the two managed `llama-server` instances (chat
/// and embeddings) — `kind` is `"llama"` or `"embed_llama"`, matching the
/// `diagnostics_apply_fix` dispatch id prefix.
fn audit_managed_server(
    kind: &str,
    label: &str,
    snapshot: &ManagedServerSnapshot,
    findings: &mut Vec<DiagnosticFinding>,
) {
    let id = format!("{kind}.reachability");
    match snapshot.status.as_str() {
        "ready" => match snapshot.health_reachable {
            Some(true) => findings.push(finding(
                &id,
                kind,
                &format!("{label} is running"),
                "The managed process reports ready and its /health endpoint responds.",
                DiagnosticStatus::Pass,
                false,
                None,
            )),
            Some(false) => findings.push(finding(
                &id,
                kind,
                &format!("{label} claims to be running but is not responding"),
                "Little Monkey's in-memory state says this process is ready, but a live /health request to it failed. The process may have crashed or hung without Little Monkey observing the exit.",
                DiagnosticStatus::Critical,
                true,
                Some("Apply the safe fix to stop and restart it, or restart it manually from Settings."),
            )),
            None => findings.push(finding(
                &id,
                kind,
                &format!("{label} status could not be verified"),
                "The process reports ready but no live health probe was performed.",
                DiagnosticStatus::Info,
                false,
                None,
            )),
        },
        "error" => findings.push(finding(
            &id,
            kind,
            &format!("{label}'s last start attempt failed"),
            "The managed process is stopped after a failed start or health check.",
            DiagnosticStatus::Warning,
            true,
            Some("Apply the safe fix to retry starting it, or start it manually from Settings."),
        )),
        "starting" => findings.push(finding(
            &id,
            kind,
            &format!("{label} is starting"),
            "The managed process is still coming up.",
            DiagnosticStatus::Info,
            false,
            None,
        )),
        _ => findings.push(finding(
            &id,
            kind,
            &format!("{label} is stopped"),
            "Not currently running. This is expected unless you rely on it being always on.",
            DiagnosticStatus::Pass,
            false,
            None,
        )),
    }
}

/// The local OpenAI-compatible API server (`server.rs`/`m3_http_server.rs`)
/// is only an operational problem when it's configured to autostart but its
/// in-memory lifecycle state isn't `"running"` — if the user never asked for
/// it to be always-on, any status is fine.
fn audit_api_server(
    app_data: &Path,
    runtime: &DiagnosticsRuntimeSnapshot,
    findings: &mut Vec<DiagnosticFinding>,
) {
    let config = match crate::server::load_config_impl(&app_data.join("api_server.json")) {
        Ok(config) => config,
        Err(error) => {
            findings.push(finding(
                "api_server.reachability",
                "api_server",
                "Local API server configuration is invalid",
                &error,
                DiagnosticStatus::Critical,
                false,
                Some("Repair or reset the configuration in Settings > Local API server."),
            ));
            return;
        }
    };
    if !config.autostart {
        findings.push(finding(
            "api_server.reachability",
            "api_server",
            "Local API server is not set to always run",
            &format!("Current status: {}.", runtime.api_server_status),
            DiagnosticStatus::Pass,
            false,
            None,
        ));
        return;
    }
    match runtime.api_server_status.as_str() {
        "running" => findings.push(finding(
            "api_server.reachability",
            "api_server",
            "Local API server is running",
            "Configured to always run, and its in-memory lifecycle state confirms it currently is.",
            DiagnosticStatus::Pass,
            false,
            None,
        )),
        "starting" => findings.push(finding(
            "api_server.reachability",
            "api_server",
            "Local API server is starting",
            "Configured to always run and currently coming up.",
            DiagnosticStatus::Info,
            false,
            None,
        )),
        "error" => findings.push(finding(
            "api_server.reachability",
            "api_server",
            "Local API server is configured to always run but failed to start",
            "Its last start attempt ended in an error state.",
            DiagnosticStatus::Critical,
            true,
            Some("Apply the safe fix to retry starting it, or start it manually from Settings."),
        )),
        _ => findings.push(finding(
            "api_server.reachability",
            "api_server",
            "Local API server is configured to always run but is stopped",
            "Autostart is enabled, but the server is not currently running.",
            DiagnosticStatus::Warning,
            true,
            Some("Apply the safe fix to start it, or start it manually from Settings."),
        )),
    }
}

/// A configured, enabled MCP server that isn't currently in the live
/// connection map is surfaced individually (mirrors
/// `security_doctor::audit_mcp_origins`'s exact aggregate-Pass-or-per-item
/// branching). Read-only: this never attempts a new connection itself, only
/// observes whatever `AppState::mcp` already reflects.
fn audit_mcp(
    app_data: &Path,
    runtime: &DiagnosticsRuntimeSnapshot,
    findings: &mut Vec<DiagnosticFinding>,
) {
    let config = match crate::mcp::load_config_impl(&app_data.join("mcp_servers.json")) {
        Ok(config) => config,
        Err(error) => {
            findings.push(finding(
                "mcp.config_invalid",
                "mcp",
                "MCP server configuration is invalid",
                &error,
                DiagnosticStatus::Critical,
                false,
                Some("Repair the MCP configuration in Settings."),
            ));
            return;
        }
    };
    let mut unhealthy = Vec::new();
    let mut healthy = 0usize;
    for server in &config.servers {
        if !server.enabled {
            continue;
        }
        if runtime.mcp_connected_ids.contains(&server.id) {
            healthy += 1;
        } else {
            unhealthy.push(server);
        }
    }
    if unhealthy.is_empty() {
        findings.push(finding(
            "mcp.connected",
            "mcp",
            "Enabled MCP servers are connected",
            &format!("Checked {healthy} enabled server(s); all are currently connected."),
            DiagnosticStatus::Pass,
            false,
            None,
        ));
        return;
    }
    for server in unhealthy {
        findings.push(finding(
            &format!("mcp.{}", server.id),
            "mcp",
            "Enabled MCP server is not connected",
            &format!(
                "'{}' is enabled but is not currently in Little Monkey's live connection set — it may have failed to connect or never been connected this session.",
                server.label
            ),
            DiagnosticStatus::Warning,
            true,
            Some("Apply the safe fix to disable it (credentials are preserved), or reconnect it manually from Settings > MCP servers."),
        ));
    }
}

/// A knowledge stack whose registry entry claims to be indexed
/// (`indexed_at.is_some()`) but whose on-disk `chunks.jsonl`/`vectors.bin`
/// are missing is corrupt relative to its own manifest. A stack that has
/// simply never been indexed (`indexed_at` is `None`) is not a health
/// problem — it's just unused.
/// Whether a stack marked indexed actually has an index behind it.
///
/// `indexed_at` is set by *both* pipeline generations, so the presence of v1's
/// `chunks.jsonl`/`vectors.bin` is not evidence either way on its own. A stack
/// is healthy if either store has it; only a stack with neither is corrupt.
fn stack_index_is_healthy(v1_files_present: bool, active_v2_generation: bool) -> bool {
    v1_files_present || active_v2_generation
}

/// Id prefix for the "still on the v1 index" finding, carrying the stack id
/// after it. It nests under `knowledge_index.` so the subsystem stays readable
/// in a support bundle, which means [`diagnostics_apply_fix`] must match it
/// *before* the bare `knowledge_index.` case — otherwise that case strips only
/// the shorter prefix and passes `v1_import.<id>` in as a stack id.
const V1_IMPORT_FINDING_PREFIX: &str = "knowledge_index.v1_import.";

fn audit_knowledge_index(app_data: &Path, findings: &mut Vec<DiagnosticFinding>) {
    let base = app_data.join("stacks");
    let stacks = match crate::knowledge_core::list_impl(&base) {
        Ok(stacks) => stacks,
        Err(error) => {
            findings.push(finding(
                "knowledge_index.registry_invalid",
                "knowledge_index",
                "Knowledge stack registry could not be read",
                &error,
                DiagnosticStatus::Critical,
                false,
                Some("Repair or reset the knowledge stacks registry in Settings > Knowledge."),
            ));
            return;
        }
    };
    // A stack can be indexed by either generation of the pipeline, and
    // `indexed_at` is set by both (`stacks::mark_v2_indexed_impl` sets it for a
    // v2 index too). Checking only for v1's `chunks.jsonl`/`vectors.bin`
    // therefore reported every v2-only stack as permanently corrupt — and the
    // "safe fix" it offered, `stacks_reindex`, then failed with "No indexable
    // files found" because a v2 stack has no v1 sources to walk. So ask both
    // stores before calling anything corrupt.
    let v2_indexes = app_data.join("knowledge-v2").join("indexes");
    let v2_store = v2_indexes
        .is_dir()
        .then(|| crate::knowledge_pipeline::GenerationStore::new(&v2_indexes).ok())
        .flatten();
    let has_active_v2_generation = |stack_id: &str| -> bool {
        v2_store
            .as_ref()
            .and_then(|store| store.active(stack_id).ok().flatten())
            .is_some()
    };

    let mut corrupt = Vec::new();
    let mut unimported = Vec::new();
    let mut healthy = 0usize;
    for stack in &stacks {
        if stack.indexed_at.is_none() {
            continue;
        }
        let dir = base.join(&stack.id);
        let v1_ok = dir.join("chunks.jsonl").is_file() && dir.join("vectors.bin").is_file();
        let v2 = has_active_v2_generation(&stack.id);
        if !stack_index_is_healthy(v1_ok, v2) {
            corrupt.push(stack);
            continue;
        }
        // Healthy, but served by v1 only. Counted separately below because
        // "how many stacks are still in this state?" is the question that
        // decides when the v1 read path can be deleted, and until now a support
        // bundle could only be guessed at for the answer. Still counted healthy:
        // its chunk and vector files really are intact.
        healthy += 1;
        if !v2 {
            unimported.push(stack);
        }
    }
    for stack in unimported {
        findings.push(finding(
            &format!("{V1_IMPORT_FINDING_PREFIX}{}", stack.id),
            "knowledge_index",
            "Knowledge stack is still served by the v1 index",
            &format!(
                "'{}' has a v1 index but no Knowledge 2.0 generation, so it misses hybrid search, \
                 citations and incremental refresh. Importing reuses the embeddings it already \
                 has — nothing is embedded again and no embeddings model needs to be running.",
                stack.name
            ),
            DiagnosticStatus::Info,
            true,
            Some(
                "Apply the safe fix to import the existing index, or use Import from v1 index in Settings > Knowledge.",
            ),
        ));
    }
    if corrupt.is_empty() {
        findings.push(finding(
            "knowledge_index.healthy",
            "knowledge_index",
            "Knowledge stack indexes are intact",
            &format!("Checked {healthy} indexed stack(s); their chunk and vector files are present."),
            DiagnosticStatus::Pass,
            false,
            None,
        ));
        return;
    }
    for stack in corrupt {
        findings.push(finding(
            &format!("knowledge_index.{}", stack.id),
            "knowledge_index",
            "Knowledge stack index is missing or corrupt",
            &format!(
                "'{}' is marked as indexed in the stacks registry, but its chunk or vector file is missing on disk.",
                stack.name
            ),
            DiagnosticStatus::Critical,
            true,
            Some("Apply the safe fix to reindex this stack, or reindex it manually from Settings > Knowledge."),
        ));
    }
}

/// Surfaces the same "daemon owns schedules but its service is stopped"
/// condition `ScheduledTasksPanel.schedulerDaemonStopped` already shows in
/// the Automation tab. Not fixable from here: starting/reinstalling the
/// daemon service is a deliberately separate, more consequential action than
/// this module's narrow restart/disable/reindex repairs, so it's surfaced
/// for visibility only.
fn audit_automation_daemon(
    runtime: &DiagnosticsRuntimeSnapshot,
    findings: &mut Vec<DiagnosticFinding>,
) {
    if runtime.daemon_installed && !runtime.daemon_service_running {
        findings.push(finding(
            "automation_daemon.status",
            "automation_daemon",
            "Background automation daemon is installed but stopped",
            "The installed daemon still owns scheduled recipes but its service is stopped. Schedules are paused; in-app fallback stays off to prevent duplicate runs.",
            DiagnosticStatus::Warning,
            false,
            Some("Start the daemon service from Settings > Scheduled tasks."),
        ));
    } else if runtime.daemon_installed {
        findings.push(finding(
            "automation_daemon.status",
            "automation_daemon",
            "Background automation daemon is running",
            "The installed daemon service is active and owns scheduled recipes.",
            DiagnosticStatus::Pass,
            false,
            None,
        ));
    } else {
        findings.push(finding(
            "automation_daemon.status",
            "automation_daemon",
            "Background automation daemon is not installed",
            "Scheduled recipes fall back to in-app scheduling while Little Monkey is running.",
            DiagnosticStatus::Info,
            false,
            None,
        ));
    }
}

fn audit_keychain(runtime: &DiagnosticsRuntimeSnapshot, findings: &mut Vec<DiagnosticFinding>) {
    match &runtime.keychain_probe {
        Some(Ok(())) => findings.push(finding(
            "keychain.roundtrip",
            "keychain",
            "OS keychain is accessible",
            "A write/read/delete round trip against a throwaway probe entry succeeded.",
            DiagnosticStatus::Pass,
            false,
            None,
        )),
        Some(Err(error)) => findings.push(finding(
            "keychain.roundtrip",
            "keychain",
            "OS keychain is not accessible",
            error,
            DiagnosticStatus::Critical,
            false,
            Some("Saved provider keys, MCP tokens, and connector credentials cannot be read or written until this is resolved. Check the OS keychain/credential manager's availability and permissions."),
        )),
        None => findings.push(finding(
            "keychain.roundtrip",
            "keychain",
            "OS keychain was not probed",
            "No keychain round-trip result was supplied for this run.",
            DiagnosticStatus::Info,
            false,
            None,
        )),
    }
}

/// This codebase revision has no connector catalog (no `connectors.rs`) — a
/// future feature per the roadmap, not something to fake a check for.
fn audit_connectors(findings: &mut Vec<DiagnosticFinding>) {
    findings.push(finding(
        "connectors.not_configured",
        "connectors",
        "Connector catalog is not available in this build",
        "This build has no saved external connectors (e.g. GitHub, Slack, Notion, Jira, S3) to check reachability for.",
        DiagnosticStatus::NotConfigured,
        false,
        None,
    ));
}

/// Remote pairing to a mobile companion app is not shipped yet — surfaced
/// explicitly as not-configured rather than silently omitted, so the report
/// never implies a check ran when it didn't.
fn audit_remote_pairing(findings: &mut Vec<DiagnosticFinding>) {
    findings.push(finding(
        "remote_pairing.not_configured",
        "remote_pairing",
        "Remote pairing / mobile is not shipped yet",
        "This subsystem does not exist in Little Monkey yet, so no health check runs for it.",
        DiagnosticStatus::NotConfigured,
        false,
        None,
    ));
}

// ---------------------------------------------------------------------
// Real (non-test) runtime probes — used only by the command layer below,
// never by `run_diagnostics` itself.
// ---------------------------------------------------------------------

/// `GET http://127.0.0.1:{port}/health` with a short timeout — same probe
/// shape `llama.rs::spawn_and_wait_healthy` and `server.rs`'s own CLI-serve
/// probe use. Never errors: an unreachable process is exactly the condition
/// being checked for, not a failure of the check itself.
async fn probe_health(port: u16) -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    crate::egress::send(client.get(format!("http://127.0.0.1:{port}/health")))
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

/// A harmless write/read/delete round trip against a single fixed, namespaced
/// probe entry in the OS keychain — proves the keychain is reachable and
/// writable without touching any real credential. Deletes its own entry
/// before returning, success or failure, so no probe residue survives a run.
fn probe_keychain() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_PROBE_ACCOUNT)
        .map_err(|error| format!("Could not open a keychain entry: {error}"))?;
    let probe_value = format!("lm-diagnostics-{}", uuid::Uuid::new_v4());
    let write_result = entry
        .set_password(&probe_value)
        .map_err(|error| format!("Keychain write failed: {error}"));
    let read_result = write_result.and_then(|()| {
        entry
            .get_password()
            .map_err(|error| format!("Keychain read failed: {error}"))
    });
    // Always attempt cleanup, even if the write or read above failed, so a
    // failed probe never leaves a stray entry behind.
    let delete_result = entry.delete_credential();
    let read_back = read_result?;
    if read_back != probe_value {
        return Err("Keychain round trip returned an unexpected value".to_string());
    }
    match delete_result {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(format!("Keychain cleanup (delete) failed: {error}")),
    }
}

// ---------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------

/// Gathers a fresh runtime snapshot from `AppState` and the filesystem, then
/// runs the pure engine above. The only place in this module that touches
/// live processes, the network, or the OS keychain.
#[tauri::command]
pub async fn diagnostics_run(
    state: tauri::State<'_, AppState>,
) -> Result<DiagnosticReport, String> {
    let app_data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the app data directory".to_string())?;
    let runtime = gather_runtime_snapshot(&state).await?;
    run_diagnostics(&DiagnosticsRequest {
        app_data_dir,
        runtime,
    })
}

async fn gather_runtime_snapshot(
    state: &tauri::State<'_, AppState>,
) -> Result<DiagnosticsRuntimeSnapshot, String> {
    let mut runtime = DiagnosticsRuntimeSnapshot::default();

    let ollama_status = crate::ollama::ollama_status().await?;
    runtime.ollama_reachable = ollama_status.reachable;

    {
        let guard = state.llama.lock().map_err(|_| "Llama state lock poisoned".to_string())?;
        runtime.llama.status = guard.status.clone();
        runtime.llama.model_path = guard.model_path.clone();
    }
    if runtime.llama.status == "ready" {
        runtime.llama.health_reachable = Some(probe_health(crate::llama::CHAT_PORT).await);
    }

    {
        let guard = state
            .embed_llama
            .lock()
            .map_err(|_| "Embed-llama state lock poisoned".to_string())?;
        runtime.embed_llama.status = guard.status.clone();
        runtime.embed_llama.model_path = guard.model_path.clone();
    }
    if runtime.embed_llama.status == "ready" {
        runtime.embed_llama.health_reachable = Some(probe_health(crate::llama::EMBED_PORT).await);
    }

    {
        let guard = state
            .api_server
            .lock()
            .map_err(|_| "API server state lock poisoned".to_string())?;
        runtime.api_server_status = guard.status.clone();
    }

    {
        let guard = state.mcp.lock().await;
        runtime.mcp_connected_ids = guard.keys().cloned().collect();
    }

    let daemon_status = crate::daemon_commands::daemon_desktop_status().await?;
    runtime.daemon_installed = daemon_status.installed;
    runtime.daemon_service_running = daemon_status.service_running;

    runtime.keychain_probe = Some(probe_keychain());

    Ok(runtime)
}

/// Dispatches `finding_id` to the exact existing repair command the finding
/// it names owns, then returns a fresh `DiagnosticFinding` reflecting the
/// outcome. Never re-implements the mutation itself — every branch below is
/// a direct call into `llama.rs`/`server.rs`/`mcp.rs`/`stacks.rs`'s own
/// commands, the same functions Settings panels call for the equivalent
/// manual action.
#[tauri::command]
pub async fn diagnostics_apply_fix(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    finding_id: String,
) -> Result<DiagnosticFinding, String> {
    if finding_id == "llama.reachability" {
        let model_path = state
            .llama
            .lock()
            .map_err(|_| "Llama state lock poisoned".to_string())?
            .model_path
            .clone();
        crate::llama::llama_stop(state.clone()).await?;
        if let Some(model_path) = model_path {
            crate::llama::llama_start(
                app.clone(),
                state.clone(),
                model_path,
                None,
                LLAMA_RESTART_GPU_LAYERS,
                false,
            )
            .await?;
            return Ok(fixed_finding(
                &finding_id,
                "llama",
                "Restarted the local chat model",
                "Stopped the wedged process and started it again with the same model.",
            ));
        }
        return Ok(fixed_finding(
            &finding_id,
            "llama",
            "Stopped the local chat model",
            "No model path was recorded to restart with — start it again from Settings > Local models.",
        ));
    }

    if finding_id == "embed_llama.reachability" {
        let model_path = state
            .embed_llama
            .lock()
            .map_err(|_| "Embed-llama state lock poisoned".to_string())?
            .model_path
            .clone();
        crate::llama::embed_server_stop(state.clone()).await?;
        if let Some(model_path) = model_path {
            crate::llama::embed_server_start(app.clone(), state.clone(), model_path).await?;
            return Ok(fixed_finding(
                &finding_id,
                "embed_llama",
                "Restarted the local embeddings model",
                "Stopped the wedged process and started it again with the same model.",
            ));
        }
        return Ok(fixed_finding(
            &finding_id,
            "embed_llama",
            "Stopped the local embeddings model",
            "No model path was recorded to restart with — start it again from Settings > Knowledge.",
        ));
    }

    if finding_id == "api_server.reachability" {
        crate::server::api_server_start(app.clone()).await?;
        return Ok(fixed_finding(
            &finding_id,
            "api_server",
            "Restarted the local API server",
            "Started it again using its saved configuration.",
        ));
    }

    if let Some(server_id) = finding_id.strip_prefix("mcp.") {
        crate::mcp::mcp_set_enabled(app.clone(), state.clone(), server_id.to_string(), false)
            .await?;
        return Ok(fixed_finding(
            &finding_id,
            "mcp",
            "Disabled the failing MCP server",
            "Its configuration and any saved credential were preserved — re-enable it from Settings > MCP servers once it's fixed.",
        ));
    }

    // Ordered before the bare `knowledge_index.` case below — see
    // `V1_IMPORT_FINDING_PREFIX`.
    //
    // Import rather than reindex, for the stacks where it applies: a reindex
    // re-embeds every chunk, which costs the user a running embeddings server
    // and a long wait to arrive back at vectors they already have on disk. The
    // import reuses them verbatim. The reverse substitution is not available —
    // the corrupt case below is by definition a stack whose v1 `chunks.jsonl` or
    // `vectors.bin` is gone, so there is nothing to import and reindexing is the
    // only repair.
    if let Some(stack_id) = finding_id.strip_prefix(V1_IMPORT_FINDING_PREFIX) {
        let report = crate::knowledge_service::knowledge_v2_import_from_v1(
            app.clone(),
            stack_id.to_string(),
        )?;
        return Ok(fixed_finding(
            &finding_id,
            "knowledge_index",
            "Imported the v1 index into Knowledge 2.0",
            &format!(
                "Reused {} existing embedding(s) across {} object(s) — nothing was embedded again.",
                report.chunk_count, report.object_count
            ),
        ));
    }

    if let Some(stack_id) = finding_id.strip_prefix("knowledge_index.") {
        crate::stacks::stacks_reindex(app.clone(), state.clone(), stack_id.to_string()).await?;
        return Ok(fixed_finding(
            &finding_id,
            "knowledge_index",
            "Reindexed the knowledge stack",
            "Triggered a full reindex to rebuild the missing or corrupt index files.",
        ));
    }

    Err(format!(
        "Finding '{finding_id}' has no safe fix available from Diagnostics."
    ))
}

/// A redacted support bundle: the current diagnostic report plus coarse,
/// non-identifying environment context. See `DiagnosticsBundle`'s doc
/// comment for why no separate redaction pass is needed.
#[tauri::command]
pub async fn diagnostics_export_bundle(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<DiagnosticsBundle, String> {
    let report = diagnostics_run(state).await?;
    Ok(DiagnosticsBundle {
        schema_version: DIAGNOSTICS_SCHEMA_VERSION,
        generated_at_ms: now_ms(),
        app_version: app.package_info().version.to_string(),
        platform: std::env::consts::OS.to_string(),
        report,
    })
}

// ---------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------

fn fixed_finding(id: &str, subsystem: &str, title: &str, detail: &str) -> DiagnosticFinding {
    DiagnosticFinding {
        id: id.to_string(),
        subsystem: subsystem.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status: DiagnosticStatus::Fixed,
        fixable: true,
        remediation: None,
    }
}

fn summarize(findings: &[DiagnosticFinding]) -> DiagnosticSummary {
    let mut summary = DiagnosticSummary::default();
    for finding in findings {
        match finding.status {
            DiagnosticStatus::Pass => summary.passed += 1,
            DiagnosticStatus::Info => summary.informational += 1,
            DiagnosticStatus::Warning => summary.warnings += 1,
            DiagnosticStatus::Critical => summary.critical += 1,
            DiagnosticStatus::Fixed => summary.fixed += 1,
            DiagnosticStatus::NotConfigured => summary.not_configured += 1,
        }
    }
    summary
}

fn finding(
    id: &str,
    subsystem: &str,
    title: &str,
    detail: &str,
    status: DiagnosticStatus,
    fixable: bool,
    remediation: Option<&str>,
) -> DiagnosticFinding {
    DiagnosticFinding {
        id: id.to_string(),
        subsystem: subsystem.to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status,
        fixable,
        remediation: remediation.map(str::to_string),
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-diagnostics-{label}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn request(root: &Path) -> DiagnosticsRequest {
        DiagnosticsRequest {
            app_data_dir: root.to_path_buf(),
            runtime: DiagnosticsRuntimeSnapshot::default(),
        }
    }

    fn find<'a>(report: &'a DiagnosticReport, id: &str) -> &'a DiagnosticFinding {
        report
            .findings
            .iter()
            .find(|finding| finding.id == id)
            .unwrap_or_else(|| panic!("no finding with id '{id}'"))
    }

    #[test]
    fn ollama_unreachable_is_informational_not_a_failure() {
        let temp = TestDirectory::new("ollama");
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        let finding = find(&report, "ollama.reachability");
        assert_eq!(finding.status, DiagnosticStatus::Info);
        assert!(!finding.fixable);
    }

    #[test]
    fn llama_ready_but_unreachable_is_critical_and_fixable() {
        let temp = TestDirectory::new("llama-critical");
        let mut audit = request(&temp.0);
        audit.runtime.llama.status = "ready".to_string();
        audit.runtime.llama.model_path = Some("/models/chat.gguf".to_string());
        audit.runtime.llama.health_reachable = Some(false);
        let report = run_diagnostics(&audit).unwrap();
        let finding = find(&report, "llama.reachability");
        assert_eq!(finding.status, DiagnosticStatus::Critical);
        assert!(finding.fixable);
        assert_eq!(report.summary.critical, 1);
    }

    #[test]
    fn llama_ready_and_healthy_passes() {
        let temp = TestDirectory::new("llama-pass");
        let mut audit = request(&temp.0);
        audit.runtime.llama.status = "ready".to_string();
        audit.runtime.llama.health_reachable = Some(true);
        let report = run_diagnostics(&audit).unwrap();
        assert_eq!(find(&report, "llama.reachability").status, DiagnosticStatus::Pass);
    }

    #[test]
    fn llama_stopped_is_a_pass_not_a_problem() {
        let temp = TestDirectory::new("llama-stopped");
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        let finding = find(&report, "llama.reachability");
        assert_eq!(finding.status, DiagnosticStatus::Pass);
        assert!(!finding.fixable);
    }

    #[test]
    fn embed_llama_error_state_is_a_fixable_warning() {
        let temp = TestDirectory::new("embed-error");
        let mut audit = request(&temp.0);
        audit.runtime.embed_llama.status = "error".to_string();
        let report = run_diagnostics(&audit).unwrap();
        let finding = find(&report, "embed_llama.reachability");
        assert_eq!(finding.status, DiagnosticStatus::Warning);
        assert!(finding.fixable);
    }

    #[test]
    fn api_server_autostart_but_stopped_is_a_fixable_warning() {
        let temp = TestDirectory::new("api-server-warning");
        fs::write(
            temp.0.join("api_server.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "port": 1234,
                "autostart": true,
                "require_token": true,
                "expose_ollama": true,
                "expose_providers": false,
                "tokens": []
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.runtime.api_server_status = "stopped".to_string();
        let report = run_diagnostics(&audit).unwrap();
        let finding = find(&report, "api_server.reachability");
        assert_eq!(finding.status, DiagnosticStatus::Warning);
        assert!(finding.fixable);
    }

    #[test]
    fn api_server_without_autostart_passes_regardless_of_status() {
        let temp = TestDirectory::new("api-server-pass");
        let mut audit = request(&temp.0);
        audit.runtime.api_server_status = "stopped".to_string();
        let report = run_diagnostics(&audit).unwrap();
        assert_eq!(
            find(&report, "api_server.reachability").status,
            DiagnosticStatus::Pass
        );
    }

    #[test]
    fn enabled_mcp_server_not_connected_is_a_fixable_warning() {
        let temp = TestDirectory::new("mcp-warning");
        fs::write(
            temp.0.join("mcp_servers.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "servers": [{
                    "id": "flaky",
                    "label": "Flaky server",
                    "transport": {"type": "http", "url": "https://example.com/mcp"},
                    "enabled": true
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        let finding = find(&report, "mcp.flaky");
        assert_eq!(finding.status, DiagnosticStatus::Warning);
        assert!(finding.fixable);
        assert_eq!(report.summary.warnings, 1);
    }

    #[test]
    fn all_enabled_mcp_servers_connected_is_one_aggregate_pass() {
        let temp = TestDirectory::new("mcp-pass");
        fs::write(
            temp.0.join("mcp_servers.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "version": 1,
                "servers": [{
                    "id": "healthy",
                    "label": "Healthy server",
                    "transport": {"type": "http", "url": "https://example.com/mcp"},
                    "enabled": true
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut audit = request(&temp.0);
        audit.runtime.mcp_connected_ids.insert("healthy".to_string());
        let report = run_diagnostics(&audit).unwrap();
        assert_eq!(find(&report, "mcp.connected").status, DiagnosticStatus::Pass);
        assert!(!report.findings.iter().any(|f| f.id.starts_with("mcp.") && f.id != "mcp.connected"));
    }

    #[test]
    fn knowledge_stack_missing_vectors_is_critical_and_fixable() {
        let temp = TestDirectory::new("knowledge-corrupt");
        let stacks_dir = temp.0.join("stacks");
        let stack_id = "11111111-1111-1111-1111-111111111111";
        fs::create_dir_all(stacks_dir.join(stack_id)).unwrap();
        fs::write(
            stacks_dir.join("index.json"),
            serde_json::to_vec_pretty(&serde_json::json!([{
                "id": stack_id,
                "name": "Docs",
                "sources": [],
                "embedding": {
                    "backend": "ollama",
                    "model_id_or_tag": "nomic-embed-text",
                    "dim": 768,
                    "query_prefix": "",
                    "doc_prefix": ""
                },
                "chunk_chars": 1600,
                "chunk_overlap": 200,
                "indexed_at": 1700000000000u64,
                "chunk_count": 3
            }]))
            .unwrap(),
        )
        .unwrap();
        // Deliberately no chunks.jsonl/vectors.bin written for this stack.
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        let finding = find(&report, &format!("knowledge_index.{stack_id}"));
        assert_eq!(finding.status, DiagnosticStatus::Critical);
        assert!(finding.fixable);
    }

    /// The population count behind D2: a stack with an intact v1 index and no v2
    /// generation is healthy but un-migrated, and a support bundle has to be able
    /// to say how many of those are left before the v1 read path can be deleted.
    #[test]
    fn a_v1_only_stack_is_reported_as_importable_not_corrupt() {
        let temp = TestDirectory::new("knowledge-v1-only");
        let stacks_dir = temp.0.join("stacks");
        let stack_id = "22222222-2222-2222-2222-222222222222";
        fs::create_dir_all(stacks_dir.join(stack_id)).unwrap();
        fs::write(
            stacks_dir.join("index.json"),
            serde_json::to_vec_pretty(&serde_json::json!([{
                "id": stack_id,
                "name": "Docs",
                "sources": [],
                "embedding": {
                    "backend": "ollama",
                    "model_id_or_tag": "nomic-embed-text",
                    "dim": 768,
                    "query_prefix": "",
                    "doc_prefix": ""
                },
                "chunk_chars": 1600,
                "chunk_overlap": 200,
                "indexed_at": 1700000000000u64,
                "chunk_count": 3
            }]))
            .unwrap(),
        )
        .unwrap();
        // A complete v1 index, and no `knowledge-v2/indexes` at all.
        fs::write(stacks_dir.join(stack_id).join("chunks.jsonl"), b"").unwrap();
        fs::write(stacks_dir.join(stack_id).join("vectors.bin"), b"").unwrap();

        let report = run_diagnostics(&request(&temp.0)).unwrap();
        let finding = find(&report, &format!("{V1_IMPORT_FINDING_PREFIX}{stack_id}"));
        assert_eq!(finding.status, DiagnosticStatus::Info);
        assert!(finding.fixable, "the import is the safe fix");
        assert!(
            !report
                .findings
                .iter()
                .any(|f| f.id == format!("knowledge_index.{stack_id}")),
            "an intact v1 index is not corrupt"
        );
        assert_eq!(
            find(&report, "knowledge_index.healthy").status,
            DiagnosticStatus::Pass,
            "and it still counts as an intact index"
        );
    }

    /// `diagnostics_apply_fix` matches finding ids by prefix, and
    /// `knowledge_index.` is a prefix of [`V1_IMPORT_FINDING_PREFIX`]. If the two
    /// branches are ever reordered, the import finding falls through to the
    /// reindex branch with `v1_import.<uuid>` as its stack id — a reindex of a
    /// stack that does not exist, in place of the free import. Nothing but order
    /// prevents that, so it is asserted here rather than left to a comment.
    #[test]
    fn the_v1_import_finding_id_is_matched_before_the_reindex_case() {
        assert!(
            V1_IMPORT_FINDING_PREFIX.starts_with("knowledge_index."),
            "the ordering hazard this guards is that one prefix contains the other"
        );
        let source = include_str!("diagnostics.rs");
        let fix = source
            .split_once("pub async fn diagnostics_apply_fix")
            .expect("the safe-fix dispatcher exists")
            .1;
        let import = fix
            .find("strip_prefix(V1_IMPORT_FINDING_PREFIX)")
            .expect("the import branch dispatches on the constant");
        let reindex = fix
            .find("strip_prefix(\"knowledge_index.\")")
            .expect("the reindex branch dispatches on the bare prefix");
        assert!(
            import < reindex,
            "the v1-import branch must come first, or the reindex branch swallows its findings"
        );
    }

    #[test]
    fn a_stack_indexed_only_by_the_v2_pipeline_is_not_reported_corrupt() {
        // The regression this replaces: `indexed_at` is set by
        // `stacks::mark_v2_indexed_impl` too, so checking only for v1's files
        // marked every v2-only stack Critical forever — and the "safe fix" it
        // offered then failed with "No indexable files found", because a v2 stack
        // has no v1 sources to walk. There was no way for a user to clear it.
        assert!(
            stack_index_is_healthy(false, true),
            "a stack with a live v2 generation and no v1 files must be healthy"
        );
        assert!(
            stack_index_is_healthy(true, false),
            "a v1-only stack must stay healthy"
        );
        assert!(stack_index_is_healthy(true, true));
        assert!(
            !stack_index_is_healthy(false, false),
            "a stack with neither index is genuinely corrupt and must still be reported"
        );
    }

    #[test]
    fn knowledge_stack_never_indexed_is_not_a_problem() {
        let temp = TestDirectory::new("knowledge-unindexed");
        let stacks_dir = temp.0.join("stacks");
        fs::create_dir_all(&stacks_dir).unwrap();
        fs::write(
            stacks_dir.join("index.json"),
            serde_json::to_vec_pretty(&serde_json::json!([{
                "id": "22222222-2222-2222-2222-222222222222",
                "name": "Unused",
                "sources": [],
                "embedding": {
                    "backend": "ollama",
                    "model_id_or_tag": "nomic-embed-text",
                    "dim": 768,
                    "query_prefix": "",
                    "doc_prefix": ""
                },
                "chunk_chars": 1600,
                "chunk_overlap": 200,
                "indexed_at": null,
                "chunk_count": 0
            }]))
            .unwrap(),
        )
        .unwrap();
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        assert_eq!(find(&report, "knowledge_index.healthy").status, DiagnosticStatus::Pass);
    }

    #[test]
    fn daemon_installed_but_stopped_is_a_non_fixable_warning() {
        let temp = TestDirectory::new("daemon-warning");
        let mut audit = request(&temp.0);
        audit.runtime.daemon_installed = true;
        audit.runtime.daemon_service_running = false;
        let report = run_diagnostics(&audit).unwrap();
        let finding = find(&report, "automation_daemon.status");
        assert_eq!(finding.status, DiagnosticStatus::Warning);
        assert!(!finding.fixable);
    }

    #[test]
    fn keychain_failure_is_critical_but_not_fixable_from_here() {
        let temp = TestDirectory::new("keychain-failure");
        let mut audit = request(&temp.0);
        audit.runtime.keychain_probe = Some(Err("keychain locked".to_string()));
        let report = run_diagnostics(&audit).unwrap();
        let finding = find(&report, "keychain.roundtrip");
        assert_eq!(finding.status, DiagnosticStatus::Critical);
        assert!(!finding.fixable);
    }

    #[test]
    fn connectors_and_remote_pairing_are_not_configured() {
        let temp = TestDirectory::new("not-configured");
        let report = run_diagnostics(&request(&temp.0)).unwrap();
        assert_eq!(
            find(&report, "connectors.not_configured").status,
            DiagnosticStatus::NotConfigured
        );
        assert_eq!(
            find(&report, "remote_pairing.not_configured").status,
            DiagnosticStatus::NotConfigured
        );
        assert_eq!(report.summary.not_configured, 2);
    }

    /// Pins down every fix dispatch's target so a future edit can't silently
    /// start reimplementing a mutation here instead of calling the owning
    /// module's existing command — this test only checks routing (which
    /// prefixes map to which subsystem), not the mutation itself, since the
    /// actual dispatch functions require a live Tauri `AppHandle`/`AppState`
    /// exercised instead by each owning module's own tests.
    #[test]
    fn every_fixable_finding_id_routes_to_a_known_existing_command_prefix() {
        let known_prefixes = ["llama.", "embed_llama.", "api_server.", "mcp.", "knowledge_index."];
        for prefix in known_prefixes {
            assert!(
                "llama.reachability".starts_with(prefix)
                    || "embed_llama.reachability".starts_with(prefix)
                    || "api_server.reachability".starts_with(prefix)
                    || "mcp.some-server".starts_with(prefix)
                    || "knowledge_index.some-stack".starts_with(prefix),
                "prefix '{prefix}' matched no known finding id shape"
            );
        }
        // A finding id outside every known prefix must be rejected by the
        // dispatch table's structure, not silently accepted — this is
        // exercised end to end in `diagnostics_apply_fix`'s final `Err`
        // branch above, which every other branch's `if`/`strip_prefix`
        // funnels down to.
    }

    #[test]
    fn probe_keychain_round_trips_and_cleans_up() {
        // Exercises the real OS keychain, same as `mcp.rs`'s own
        // `read_http_token_is_none_when_nothing_is_saved` test does — this
        // repo's CI keychain backend already supports that.
        assert!(probe_keychain().is_ok());
        // Calling it again must still succeed (overwrite-then-clean, not
        // "already exists" failure) and must not leave the entry behind.
        assert!(probe_keychain().is_ok());
        let leftover = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_PROBE_ACCOUNT)
            .and_then(|entry| entry.get_password());
        assert!(leftover.is_err(), "probe entry must be deleted after the round trip");
    }
}

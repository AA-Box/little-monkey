pub(crate) mod adapters;
mod admission;
mod call_audio;
pub(crate) mod call_media;
pub(crate) mod call_socket;
pub(crate) mod channel_adapter;
pub(crate) mod channel_ingress;
pub(crate) mod channel_store;
pub(crate) mod channel_tool;
pub(crate) mod channel_worker;
mod engine;
pub(crate) mod ingress_store;
mod ledger;
pub(crate) mod remote;
mod scheduler;
mod service;
pub(crate) mod store;
pub(crate) mod telecom_store;
pub(crate) mod telecom_tool;
pub(crate) mod telecom_worker;
pub(crate) mod telephony;
mod trigger;
mod webhook;
mod workflow_trigger;
mod worktree;

use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Subcommand, ValueEnum};
use little_monkey_lib::recipes::{self, Recipe};
use little_monkey_lib::run_protocol::{
    ClientIdentity, ClientKind, PermissionDecision, RunEvent, RunStatus,
};
use little_monkey_lib::workflow_core::WorkflowTrigger;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::durable_run::{bounded_text, CliRunEventSink, DurableRunRecorder};

use self::engine::{DaemonEngine, OsNotificationAdapter, RealProcessAdapter, SystemClock};
use self::ledger::SharedLedger;
use self::service::{DaemonLock, ServiceManager};
use self::store::{
    DaemonConfig, DaemonPaths, DaemonStore, JobState, NewDaemonJob, PendingDelivery,
};
use self::trigger::{
    ingest_signed_delivery, poll_persistent_triggers, IngestOutcome, KeyringSecretStore,
    SecretStore, SignedDelivery, TriggerConfig, TriggerTarget, WorkflowTriggerBinding,
    DEFAULT_SIGNATURE_SKEW_MS,
};
use self::workflow_trigger::WorkflowBatchSynchronizer;
use self::worktree::{OwnedWorktree, WorktreeRequest};

#[derive(Subcommand, Debug)]
pub enum DaemonCmd {
    /// Explicitly install and start the current-user OS service.
    Install(DaemonInstallArgs),
    /// Start the previously installed user service.
    Start,
    /// Show service, heartbeat, kill-switch, queue, backpressure, and
    /// active-run state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Show recent scheduling decisions: what was chosen, over what, and which
    /// measurement decided it.
    Decisions {
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Gracefully cancel active work and stop the user service.
    Stop,
    /// Remove the user service. Durable run history is retained by default.
    Uninstall {
        /// Also delete daemon queue/config/log/snapshot state. Shared run
        /// history is never deleted. Refused while jobs or owned worktrees remain.
        #[arg(long)]
        purge_state: bool,
    },
    /// Queue an immutable recipe snapshot for resident execution.
    Run(DaemonRunArgs),
    /// Print durable events for a run; optionally follow until terminal.
    Attach {
        run_id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long)]
        json: bool,
    },
    /// Pause a queued or active run. Active task process groups are suspended.
    Pause { run_id: String },
    /// Resume a paused queued or active run.
    Resume { run_id: String },
    /// Cancel a queued or active run and its supervised process group.
    Cancel {
        run_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Submit a new run from a terminal prior snapshot; never replays in place.
    Retry {
        run_id: String,
        /// Required when the prior run reached reconciliation or confirmed an
        /// external mutation. This remains an explicit operator action.
        #[arg(long)]
        acknowledge_side_effects: bool,
    },
    /// Decide a daemon-hosted approval request bound to its operation digest.
    Approve {
        run_id: String,
        request_id: String,
        decision: ApprovalChoice,
    },
    /// Engage, release, or inspect the durable global kill switch.
    #[command(subcommand)]
    KillSwitch(KillSwitchCmd),
    /// Local-only emergency controls for remote desktop-control sessions.
    /// No network or pairing needed — the escape hatch for when you are
    /// physically at the machine and the remote link is untrustworthy.
    #[command(subcommand)]
    DesktopControl(DesktopControlCmd),
    /// Configure or deliver persistent cron/filesystem/signed/GitHub triggers.
    #[command(subcommand)]
    Trigger(TriggerCmd),
    /// Pair and control a user-owned remote runner over pinned TLS. Provider
    /// keys, inference, tools, and workspace access stay on the runner.
    #[command(subcommand)]
    Remote(remote::RemoteCmd),
    /// Resident service entrypoint used only by the installed OS manifest.
    #[command(hide = true)]
    Serve,
}

#[derive(Args, Debug)]
pub struct DaemonInstallArgs {
    #[arg(long, default_value_t = store::DEFAULT_CONCURRENCY)]
    concurrency: u32,
    #[arg(long, default_value_t = store::DEFAULT_MAX_QUEUE)]
    max_queue: u32,
    #[arg(long, default_value_t = store::DEFAULT_RETENTION_DAYS)]
    retention_days: u32,
    /// Optional localhost-only HTTP port for signed webhook delivery.
    #[arg(long)]
    webhook_port: Option<u16>,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    notifications: bool,
}

#[derive(Args, Debug, Clone)]
pub struct DaemonRunArgs {
    pub name_or_path: String,
    #[arg(long = "param", value_name = "KEY=VALUE")]
    pub param: Vec<String>,
    /// Caller-owned key. Only its digest affects the durable job id; the raw
    /// value is not persisted.
    #[arg(long)]
    pub run_key: Option<String>,
    #[arg(long, default_value_t = 0)]
    pub priority: i32,
    #[arg(long, default_value_t = 1)]
    pub max_attempts: u32,
    #[arg(long, default_value_t = 7 * 24 * 60 * 60)]
    pub max_runtime_seconds: u64,
    #[arg(long)]
    pub max_memory_mb: Option<u64>,
    /// Create and require an app-owned isolated git worktree.
    #[arg(long)]
    pub owned_worktree: bool,
    /// Repository used for --owned-worktree; defaults to the recipe workspace.
    #[arg(long)]
    pub repository: Option<PathBuf>,
    #[arg(long, default_value = "codex/")]
    pub branch_prefix: String,
    #[arg(long = "remote", default_value = "origin")]
    pub allowed_remotes: Vec<String>,
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub allow_commit: bool,
    #[arg(long)]
    pub allow_push: bool,
    #[arg(long)]
    pub allow_create_pull_request: bool,
    #[arg(long)]
    pub allow_review_comment: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Subcommand, Debug)]
pub enum KillSwitchCmd {
    Engage,
    Release,
    Status,
}

#[derive(Subcommand, Debug)]
pub enum DesktopControlCmd {
    /// Immediately force-stop every active remote desktop-control session on
    /// the local daemon. Talks to the resident process through its own state
    /// db — no network, no pairing, no signed request.
    EmergencyStop,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
pub enum ApprovalChoice {
    AllowOnce,
    AllowForRun,
    Deny,
}

impl From<ApprovalChoice> for PermissionDecision {
    fn from(value: ApprovalChoice) -> Self {
        match value {
            ApprovalChoice::AllowOnce => Self::AllowOnce,
            ApprovalChoice::AllowForRun => Self::AllowForRun,
            ApprovalChoice::Deny => Self::Deny,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum TriggerCmd {
    AddCron {
        id: String,
        #[command(flatten)]
        target: TriggerTargetArgs,
        #[arg(long)]
        cron: String,
        #[arg(long = "param")]
        param: Vec<String>,
        #[arg(long)]
        payload_param: Option<String>,
    },
    AddFilesystem {
        id: String,
        #[command(flatten)]
        target: TriggerTargetArgs,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        recursive: bool,
        #[arg(long = "param")]
        param: Vec<String>,
        #[arg(long)]
        payload_param: Option<String>,
    },
    AddWebhook {
        id: String,
        #[command(flatten)]
        target: TriggerTargetArgs,
        /// Name of an environment variable containing the HMAC secret.
        #[arg(long)]
        secret_env: String,
        #[arg(long = "param")]
        param: Vec<String>,
        #[arg(long)]
        payload_param: Option<String>,
        #[arg(long, default_value_t = DEFAULT_SIGNATURE_SKEW_MS)]
        max_skew_ms: u64,
    },
    AddGithub {
        id: String,
        #[command(flatten)]
        target: TriggerTargetArgs,
        #[arg(long)]
        secret_env: String,
        #[arg(long)]
        repository: String,
        #[arg(long)]
        local_repository: PathBuf,
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long = "branch-prefix", required = true)]
        branch_prefixes: Vec<String>,
        #[arg(long = "event", required = true)]
        events: Vec<String>,
        #[arg(long)]
        allow_push: bool,
        #[arg(long)]
        allow_create_pull_request: bool,
        #[arg(long)]
        allow_review_comment: bool,
        #[arg(long = "param")]
        param: Vec<String>,
        #[arg(long)]
        payload_param: Option<String>,
        #[arg(long, default_value_t = DEFAULT_SIGNATURE_SKEW_MS)]
        max_skew_ms: u64,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Remove {
        id: String,
    },
    /// Store a workflow webhook HMAC secret under an opaque OS-keychain
    /// reference. The secret value is read from the named environment variable
    /// and never written to daemon JSON/SQLite state.
    SecretSet {
        reference: String,
        #[arg(long)]
        secret_env: String,
    },
    /// Remove an opaque workflow webhook HMAC secret from the OS keychain.
    SecretRemove {
        reference: String,
    },
    /// Offline/forwarder-friendly signed ingestion path. The resident HTTP
    /// endpoint uses the exact same verifier and dedupe transaction.
    Deliver {
        id: String,
        #[arg(long)]
        delivery_id: String,
        #[arg(long)]
        timestamp_ms: u64,
        #[arg(long)]
        nonce: String,
        #[arg(long)]
        signature: String,
        #[arg(long)]
        payload: PathBuf,
        #[arg(long)]
        event: Option<String>,
    },
}

#[derive(Args, Debug)]
pub struct TriggerTargetArgs {
    /// Existing recipe name/path. Omit when selecting an M4 workflow target.
    #[arg(value_name = "RECIPE")]
    recipe: Option<String>,
    /// Exact M4 workflow identifier (mutually exclusive with RECIPE).
    #[arg(long)]
    workflow_id: Option<String>,
    /// SHA-256 of the immutable M4 workflow definition.
    #[arg(long)]
    definition_sha256: Option<String>,
    /// Positive M4 workflow definition version.
    #[arg(long)]
    workflow_version: Option<u32>,
    /// Exact serialized WorkflowTrigger declaration. Required for a workflow
    /// target and checked against the daemon trigger kind/configuration.
    #[arg(long)]
    workflow_trigger_json: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct QueuedRun {
    pub(crate) job_id: String,
    pub(crate) run_id: String,
    pub(crate) state: JobState,
}

#[derive(Debug, Serialize)]
struct DaemonStatus {
    installed: bool,
    service_running: bool,
    heartbeat_fresh: bool,
    pid: Option<u32>,
    kill_switch: bool,
    queued: u32,
    active: u32,
    waiting_approval: u32,
    paused: u32,
    managed_run_ids: Vec<String>,
    platform: service::ServicePlatform,
    /// The K8 backpressure signal. Added to *this* payload rather than given its
    /// own command because this is already the thing every producer polls — the
    /// desktop reads it through `daemon_desktop_status`, which shells out to
    /// `monkey daemon status --json`, so the JSON shape is the API.
    ///
    /// Producers should branch on `backpressure.state` (`accepting` / `slow` /
    /// `closed`) or, if they only care about the hard case, on
    /// `backpressure.accepting`. A `closed` signal is already enforced by
    /// `enqueue`, so a producer that ignores it gets an error instead of a
    /// silently overfull queue; `slow` is advisory and only a producer can honour
    /// it, because only a producer knows whether its work can wait.
    backpressure: scheduler::Backpressure,
}

/// The backpressure signal, composed once (K8).
///
/// One function so the signal a producer polls on `status` and the refusal
/// `enqueue` returns cannot disagree about which counters feed it. Everything it
/// needs is already a cheap indexed count on the daemon's own database.
pub(crate) fn backpressure_for(
    store: &DaemonStore,
    config: &DaemonConfig,
) -> Result<scheduler::Backpressure, String> {
    Ok(scheduler::backpressure(
        store.kill_switch()?,
        store.nonterminal_count()?,
        config.max_queue,
        store.queued_count()?,
        store.held_count()?,
        config.poll_interval_ms,
    ))
}

pub async fn run(cli: &crate::Cli, action: &DaemonCmd) -> Result<(), String> {
    match action {
        DaemonCmd::Install(args) => install(args),
        DaemonCmd::Start => {
            let (paths, manager) = service_context()?;
            manager.start(&paths)
        }
        DaemonCmd::Status { json } => status(*json),
        DaemonCmd::Decisions { limit, json } => decisions(*limit, *json),
        DaemonCmd::Stop => stop().await,
        DaemonCmd::Uninstall { purge_state } => uninstall(*purge_state),
        DaemonCmd::Run(args) => queue_command(cli, args),
        DaemonCmd::Attach {
            run_id,
            follow,
            json,
        } => attach(run_id, *follow, *json).await,
        DaemonCmd::Pause { run_id } => pause(run_id),
        DaemonCmd::Resume { run_id } => resume(run_id),
        DaemonCmd::Cancel { run_id, reason } => cancel(run_id, reason.as_deref()),
        DaemonCmd::Retry {
            run_id,
            acknowledge_side_effects,
        } => retry(cli, run_id, *acknowledge_side_effects),
        DaemonCmd::Approve {
            run_id,
            request_id,
            decision,
        } => approve(run_id, request_id, (*decision).into()),
        DaemonCmd::KillSwitch(action) => kill_switch(action),
        DaemonCmd::DesktopControl(action) => desktop_control_command(action),
        DaemonCmd::Trigger(action) => trigger_command(action),
        DaemonCmd::Remote(action) => remote::run(action).await,
        DaemonCmd::Serve => serve(cli).await,
    }
}

fn service_context() -> Result<(DaemonPaths, ServiceManager<service::RealCommandRunner>), String> {
    let roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let paths = DaemonPaths::under(&roots.legacy);
    let manager = ServiceManager::<service::RealCommandRunner>::real(roots)?;
    Ok((paths, manager))
}

fn install(args: &DaemonInstallArgs) -> Result<(), String> {
    let (paths, manager) = service_context()?;
    let config = DaemonConfig {
        concurrency: args.concurrency,
        max_queue: args.max_queue,
        retention_days: args.retention_days,
        webhook_port: args.webhook_port,
        notifications: args.notifications,
        ..DaemonConfig::default()
    };
    let mut store = DaemonStore::open(&paths)?;
    store.set_meta("stop_requested", "0")?;
    let manifest = manager.install(&paths, &config)?;
    println!(
        "Installed {} user service at {}",
        format!("{:?}", manager.platform()).to_lowercase(),
        manifest.display()
    );
    Ok(())
}

fn status(json: bool) -> Result<(), String> {
    let (paths, manager) = service_context()?;
    let installed = paths.config.is_file() && manager.is_installed(&paths)?;
    let mut queued = 0;
    let mut active = 0;
    let mut waiting_approval = 0;
    let mut paused = 0;
    let mut kill_switch = false;
    let mut heartbeat = None;
    let mut pid = None;
    let mut managed_run_ids = Vec::new();
    let mut held = 0;
    let mut nonterminal = 0;
    // The installed config when there is one, defaults otherwise: an uninstalled
    // daemon still has to report a coherent capacity rather than zero, or every
    // producer reads "queue full" before the daemon exists.
    let config = DaemonConfig::load(&paths).unwrap_or_default();
    if paths.state_db.is_file() {
        let store = DaemonStore::open(&paths)?;
        kill_switch = store.kill_switch()?;
        held = store.held_count()?;
        nonterminal = store.nonterminal_count()?;
        heartbeat = store
            .get_meta("heartbeat_ms")?
            .and_then(|value| value.parse::<u64>().ok());
        pid = store
            .get_meta("pid")?
            .and_then(|value| value.parse::<u32>().ok());
        managed_run_ids = store.managed_run_ids(512)?;
        for job in store.nonterminal_jobs()? {
            match job.state {
                JobState::Preparing | JobState::Queued => queued += 1,
                JobState::WaitingApproval => waiting_approval += 1,
                JobState::Paused => paused += 1,
                JobState::Running | JobState::Cancelling => active += 1,
                _ => {}
            }
        }
    }
    let now = now_ms()?;
    let service_running = manager.status(&paths).unwrap_or(false);
    let result = DaemonStatus {
        installed,
        service_running,
        heartbeat_fresh: heartbeat.is_some_and(|value| now.saturating_sub(value) < 5_000),
        pid,
        kill_switch,
        queued,
        active,
        waiting_approval,
        paused,
        managed_run_ids,
        platform: manager.platform(),
        backpressure: scheduler::backpressure(
            kill_switch,
            nonterminal,
            config.max_queue,
            queued,
            held,
            config.poll_interval_ms,
        ),
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result).unwrap_or_default()
        );
    } else {
        println!(
            "installed={} service={} heartbeat={} pid={} kill_switch={} queued={} active={} waiting_approval={} paused={} backpressure={} held={}",
            result.installed,
            result.service_running,
            result.heartbeat_fresh,
            result.pid.map(|value| value.to_string()).unwrap_or_else(|| "-".into()),
            result.kill_switch,
            result.queued,
            result.active,
            result.waiting_approval,
            result.paused,
            result.backpressure.state.token(),
            result.backpressure.held,
        );
    }
    Ok(())
}

/// `monkey daemon decisions` — the scheduling decision log, newest first.
///
/// Its own command rather than more fields on `status`, which the desktop polls
/// several times a second: a decision log is read when somebody is asking why,
/// not continuously.
fn decisions(limit: u32, json: bool) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    if !paths.state_db.is_file() {
        return Err("Daemon has no state to inspect".to_string());
    }
    let entries = DaemonStore::open(&paths)?.recent_decisions(limit)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&entries).unwrap_or_default()
        );
        return Ok(());
    }
    if entries.is_empty() {
        println!("No scheduling decisions recorded yet.");
        return Ok(());
    }
    for entry in entries {
        println!(
            "{} {} {} class={}/{} over=[{}] {}={} @{} — {}",
            entry.decided_at_ms,
            entry.outcome,
            entry.job_id,
            entry.process_class,
            entry.effective_class,
            entry.passed_over.join(","),
            entry.measurement,
            entry
                .measured_value
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            entry
                .measured_at_ms
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
            entry.detail,
        );
    }
    Ok(())
}

async fn stop() -> Result<(), String> {
    let (paths, manager) = service_context()?;
    if paths.state_db.is_file() {
        let mut store = DaemonStore::open(&paths)?;
        store.set_meta("stop_requested", "1")?;
        store.request_cancel_all(now_ms()?)?;
        for _ in 0..20 {
            if store.active_jobs()?.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    if manager.is_installed(&paths)? {
        manager.stop(&paths)?;
    }
    println!("Daemon stopped.");
    Ok(())
}

fn uninstall(purge_state: bool) -> Result<(), String> {
    let (paths, manager) = service_context()?;
    manager.uninstall(&paths)?;
    if purge_state && paths.root.exists() {
        let store = DaemonStore::open(&paths)?;
        if store.nonterminal_count()? != 0 || !store.terminal_worktree_jobs(now_ms()?)?.is_empty() {
            return Err(
                "Refusing --purge-state while jobs or owned worktrees remain; retain state and inspect them"
                    .to_string(),
            );
        }
        std::fs::remove_dir_all(&paths.root)
            .map_err(|error| format!("Failed to purge daemon state: {error}"))?;
    } else {
        let _ = std::fs::remove_file(&paths.config);
    }
    println!("Daemon service uninstalled; shared durable run history was retained.");
    Ok(())
}

fn queue_command(cli: &crate::Cli, args: &DaemonRunArgs) -> Result<(), String> {
    let config_roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let paths = DaemonPaths::under(&config_roots.legacy);
    let global_config_roots = config_roots.ordered();
    let config = DaemonConfig::load(&paths)?;
    let mut store = DaemonStore::open(&paths)?;
    if store.kill_switch()? {
        return Err("Global kill switch is engaged; release it before queueing work".to_string());
    }
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    let options = QueueOptions::from_run_args(args);
    let queued = enqueue(
        Some(cli),
        &paths,
        &global_config_roots,
        &config,
        &mut store,
        &mut shared,
        options,
    )?;
    if args.json {
        println!("{}", serde_json::to_string(&queued).unwrap_or_default());
    } else {
        println!("Queued {} as durable run {}", queued.job_id, queued.run_id);
    }
    Ok(())
}

/// Queue an immutable recipe on behalf of a protocol client without writing
/// human-oriented text to stdout. ACP uses this bridge so the resident daemon
/// remains the execution authority while the stdio connection only relays the
/// shared run protocol.
pub(crate) fn queue_client_recipe(
    cli: &crate::Cli,
    recipe_path: &Path,
    client_key: &str,
) -> Result<QueuedRun, String> {
    if client_key.is_empty() || client_key.len() > 4_096 || client_key.contains('\0') {
        return Err("Protocol client run key is invalid".to_string());
    }
    let config_roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let paths = DaemonPaths::under(&config_roots.legacy);
    let global_config_roots = config_roots.ordered();
    let config = DaemonConfig::load(&paths).map_err(|error| {
        format!(
            "The Little Monkey background runner is not configured. Install it from the app or run `monkey daemon install`: {error}"
        )
    })?;
    let mut store = DaemonStore::open(&paths)?;
    if store.kill_switch()? {
        return Err("Global kill switch is engaged; protocol runs cannot be queued".to_string());
    }
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    let options = QueueOptions {
        recipe: recipe_path.to_string_lossy().into_owned(),
        params: Vec::new(),
        origin: QueueOrigin::Remote {
            request_id: client_key.to_string(),
        },
        deterministic_job_id: Some(format!(
            "job-{}",
            &sha256_hex(format!("protocol-client:{client_key}").as_bytes())[..32]
        )),
        priority: 0,
        max_attempts: 1,
        max_runtime_ms: 24 * 60 * 60 * 1_000,
        max_memory_bytes: None,
        owned_worktree: false,
        repository: None,
        branch_prefix: "codex/".to_string(),
        allowed_remotes: vec!["origin".to_string()],
        allow_commit: false,
        allow_push: false,
        allow_create_pull_request: false,
        allow_review_comment: false,
        parent_run_id: None,
        snapshot_is_frozen: false,
    };
    enqueue(
        Some(cli),
        &paths,
        &global_config_roots,
        &config,
        &mut store,
        &mut shared,
        options,
    )
}

/// Recipe name the mobile chat route executes. The node operator authors it
/// once (Settings → Tasks, or a recipe file) with their chosen model, system
/// prompt, permission mode, and a `prompt` parameter — that recipe IS the
/// mobile chat contract, which keeps the node authoritative for models and
/// keeps this path from inventing an implicit target resolution of its own.
pub(crate) const MOBILE_CHAT_RECIPE: &str = "mobile-chat";

pub(crate) fn mobile_chat_job_id(client_key: &str) -> String {
    format!(
        "job-{}",
        &sha256_hex(format!("mobile-chat:{client_key}").as_bytes())[..32]
    )
}

/// Queues one mobile chat turn against the operator's `mobile-chat` recipe.
/// Frozen snapshot (no CLI rules merging), one attempt, no worktree — the
/// same conservative posture as `queue_client_recipe`.
pub(crate) fn queue_mobile_chat_recipe(
    paths: &DaemonPaths,
    session_id: &str,
    client_key: &str,
    prompt: &str,
) -> Result<QueuedRun, String> {
    if client_key.is_empty() || client_key.len() > 256 {
        return Err("Mobile chat queue key is invalid".to_string());
    }
    // The same durable description every other external turn is built as. The
    // queue options below are unchanged — a mobile turn keeps its own job id,
    // its own frozen recipe contract and its own runtime budget — but the text
    // now travels as a `ConversationIngress`, which is what decides that a
    // paired phone's words are the operator's instructions rather than
    // untrusted data. See `ConversationSource::author_is_operator`.
    let ingress = little_monkey_lib::channels::ingress::ConversationIngress::direct(
        little_monkey_lib::channels::ingress::ConversationSource::Mobile,
        session_id,
        client_key,
        format!("mobile:{session_id}"),
        prompt,
        little_monkey_lib::channels::routing::RouteTarget::new(MOBILE_CHAT_RECIPE),
        i64::try_from(now_ms()?).unwrap_or(i64::MAX),
    );
    let config = DaemonConfig::load(paths).map_err(|error| {
        format!(
            "The Little Monkey background runner is not configured. Install it from the app or run `monkey daemon install`: {error}"
        )
    })?;
    let mut store = DaemonStore::open(paths)?;
    if store.kill_switch()? {
        return Err("Global kill switch is engaged; mobile chat cannot be queued".to_string());
    }
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    let options = QueueOptions {
        recipe: MOBILE_CHAT_RECIPE.to_string(),
        params: vec![format!(
            "prompt={}",
            channel_ingress::message_param(
                &ingress,
                "a paired mobile device",
                little_monkey_lib::channels::ingress::MAX_LISTED_ATTACHMENTS,
            )
        )],
        origin: QueueOrigin::Remote {
            request_id: client_key.to_string(),
        },
        deterministic_job_id: Some(mobile_chat_job_id(client_key)),
        priority: 0,
        max_attempts: 1,
        max_runtime_ms: 30 * 60 * 1_000,
        max_memory_bytes: None,
        owned_worktree: false,
        repository: None,
        branch_prefix: "codex/".to_string(),
        allowed_remotes: vec!["origin".to_string()],
        allow_commit: false,
        allow_push: false,
        allow_create_pull_request: false,
        allow_review_comment: false,
        parent_run_id: None,
        // The recipe is used verbatim: its own system prompt is the mobile
        // contract, never merged with whatever rules exist in the daemon
        // process's cwd.
        snapshot_is_frozen: true,
    };
    let global_config_roots = global_config_roots_for_paths(paths)?;
    enqueue(
        None,
        paths,
        &global_config_roots,
        &config,
        &mut store,
        &mut shared,
        options,
    )
}

/// Production implementation of the channel worker's run seam.
///
/// Opens its own handles per submission rather than holding them, because the
/// inbound loop can sit idle for hours between messages and a long-lived
/// connection to the ledger buys nothing over that interval.
pub(crate) struct DaemonChannelQueue {
    paths: DaemonPaths,
}

impl DaemonChannelQueue {
    pub(crate) fn new(paths: DaemonPaths) -> Self {
        Self { paths }
    }
}

impl channel_worker::RunQueue for DaemonChannelQueue {
    fn submit(
        &self,
        ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
        params: Vec<String>,
    ) -> Result<String, String> {
        let config = DaemonConfig::load(&self.paths).map_err(|error| {
            format!("The Little Monkey background runner is not configured: {error}")
        })?;
        let mut store = DaemonStore::open(&self.paths)?;
        if store.kill_switch()? {
            return Err("Global kill switch is engaged; the message was not run".to_string());
        }
        let mut shared = SharedLedger::open(&self.paths.ledger_db)?;
        let options = channel_ingress::queue_options_for(ingress, params);
        let global_config_roots = global_config_roots_for_paths(&self.paths)?;
        enqueue(
            None,
            &self.paths,
            &global_config_roots,
            &config,
            &mut store,
            &mut shared,
            options,
        )
        .map(|queued| queued.job_id)
    }
}

/// Production implementation of the remote API's mobile chat seam.
pub(crate) struct DaemonMobileChatQueue {
    paths: DaemonPaths,
}

impl DaemonMobileChatQueue {
    pub(crate) fn new(paths: DaemonPaths) -> Self {
        Self { paths }
    }
}

impl remote::api::MobileChatQueue for DaemonMobileChatQueue {
    fn queue_chat(
        &self,
        session_id: &str,
        client_key: &str,
        prompt: &str,
    ) -> Result<String, String> {
        queue_mobile_chat_recipe(&self.paths, session_id, client_key, prompt)
            .map(|queued| queued.run_id)
    }

    fn chat_run_id(&self, client_key: &str) -> Result<Option<String>, String> {
        let store = DaemonStore::open(&self.paths)?;
        Ok(store
            .get_job(&mobile_chat_job_id(client_key))?
            .and_then(|job| job.run_id))
    }
}

/// Where a node keeps the frozen recipes it built from foreign specs.
///
/// Beside `paths.snapshots` rather than in it: `enqueue` writes the queue's own
/// immutable copy into `snapshots/<job_id>.json`, and these are the *sources* it
/// copies from. Two directories so a placement source can never be mistaken for
/// the queue snapshot the executing child actually reads.
fn placements_dir(paths: &DaemonPaths) -> PathBuf {
    paths.root.join("placements")
}

/// **Roadmap K17 S2/S3: a foreign `RunSpec` becomes work on this machine.**
///
/// # Why this is a conversion and not a hand-off
///
/// The daemon's unit of work is a recipe; the run protocol's unit is a
/// `RunSpec`, and the `RunSpec` for a queued job is built *downstream*, by the
/// `monkey-cli task run` child. So a spec cannot simply be handed to the queue —
/// it has to arrive at that child, and the only thing that reaches it is the
/// recipe snapshot.
///
/// The conversion therefore has two halves that must not be confused. The
/// **execution** half (target, workspace, permission mode, prompt) becomes
/// ordinary recipe fields, because that is what the executor reads. The
/// **policy** half rides verbatim in `recipe.placed_run`, because a recipe has
/// nowhere to put an egress allowlist or a token budget and re-deriving them
/// here would silently substitute this node's defaults for the submitter's — the
/// exact failure S3 exists to prevent.
///
/// Everything this node cannot satisfy is refused here, before anything is
/// written, and each refusal names the fact that is missing rather than saying
/// the placement failed.
pub(crate) struct DaemonPlacementQueue {
    paths: DaemonPaths,
}

impl DaemonPlacementQueue {
    pub(crate) fn new(paths: DaemonPaths) -> Self {
        Self { paths }
    }
}

/// The recipe target this node will execute a placed spec through.
///
/// A `ManagedLlama` placement is resolved against **this node's** runtime hub,
/// never against the spec's `model_path`: that path is a location on the
/// submitter's disk and means nothing here. The model id is the portable half,
/// so a node that has the same model installed can run the placement and one
/// that has not refuses it by name — which is the answer an operator can act on,
/// unlike a spawn failure later.
fn placed_recipe_target(
    target: &little_monkey_lib::run_protocol::ModelTargetSnapshot,
    app_data: &Path,
) -> Result<little_monkey_lib::recipes::RecipeTarget, String> {
    use little_monkey_lib::run_protocol::ModelTargetSnapshot;
    match target {
        // The provider *identity* travels; the credential never does. This node
        // resolves the endpoint and key from its own configuration, which is
        // what keeps "keys stay on the runner" true for placed work too.
        ModelTargetSnapshot::Provider {
            provider_id, model, ..
        } => Ok(little_monkey_lib::recipes::RecipeTarget {
            provider: Some(provider_id.clone()),
            model: Some(model.clone()),
            ollama: None,
            local_url: None,
            managed_model: None,
        }),
        ModelTargetSnapshot::Ollama { model, .. } => Ok(little_monkey_lib::recipes::RecipeTarget {
            provider: None,
            model: None,
            ollama: Some(model.clone()),
            local_url: None,
            managed_model: None,
        }),
        ModelTargetSnapshot::ManagedLlama { model_id, .. } => {
            if little_monkey_lib::m3_runtime_hub::installed_model_artifact(app_data, model_id)
                .is_none()
            {
                return Err(format!(
                    "this node has no managed model '{model_id}' installed; install it here or place the run on a node that advertises it"
                ));
            }
            Ok(little_monkey_lib::recipes::RecipeTarget {
                provider: None,
                model: None,
                ollama: None,
                local_url: None,
                // The origin deliberately is not named here: the managed runtime
                // is started on a fresh loopback port when the run starts, so
                // any port written now would be wrong by then.
                managed_model: Some(model_id.clone()),
            })
        }
    }
}

fn placed_permission_mode(
    mode: &little_monkey_lib::run_protocol::PermissionMode,
) -> Result<String, String> {
    use little_monkey_lib::run_protocol::PermissionMode;
    Ok(match mode {
        PermissionMode::Manual => "manual",
        PermissionMode::AcceptEdits => "acceptEdits",
        PermissionMode::Smart => "smart",
        PermissionMode::Plan => "plan",
        PermissionMode::Auto => "auto",
        // The one mode a foreign spec may never buy on this machine. A placed
        // run is unattended here by definition — nobody on this node is watching
        // its prompts — and `bypass` auto-approves every tool including
        // `run_shell`. `validate_recipe` refuses it too; this refuses it earlier
        // and with a sentence the submitter can act on.
        PermissionMode::Bypass => {
            return Err(
                "this node refuses a placed run in 'bypass' permission mode: it auto-approves every tool, including shell, with nobody here to catch it"
                    .to_string(),
            )
        }
    }
    .to_string())
}

/// The deterministic job id for a placement, so a resubmitted spec resolves to
/// the job it already has rather than starting a second one.
fn placed_job_id(submitted_run_id: &str) -> String {
    format!(
        "job-{}",
        &sha256_hex(format!("placed-run:{submitted_run_id}").as_bytes())[..32]
    )
}

/// The frozen recipe a placed spec becomes on this node.
///
/// Extracted from [`DaemonPlacementQueue::place`] because it is the whole of
/// S3's node half and the only part of the placement path that can be exercised
/// without a running daemon: everything after it opens the queue database and
/// spawns a submission child. Every refusal this node owns is decided here, and
/// the four frozen fields are attached here rather than re-derived downstream.
fn placed_recipe(
    spec: &little_monkey_lib::run_protocol::RunSpec,
) -> Result<little_monkey_lib::recipes::Recipe, String> {
    spec.validate().map_err(|error| error.to_string())?;
    let snapshot = little_monkey_lib::node_placement::PlacedRunSnapshot::from_spec(spec);
    snapshot.validate()?;
    let app_data = crate::app_data_dir().ok_or("Could not resolve app data directory")?;
    let target = placed_recipe_target(&spec.target, &app_data)?;
    let permission_mode = placed_permission_mode(&spec.permission_policy.mode)?;

    // The workspace is checked against this machine's filesystem before
    // anything is written. A placed run whose root does not exist here is
    // refused rather than silently rehomed onto the daemon's working directory,
    // which would run the submitter's task against the wrong files — the
    // quietest possible way to get this wrong.
    let workspace = match snapshot.primary_root() {
        Some(root) => {
            let canonical = PathBuf::from(root).canonicalize().map_err(|error| {
                format!("this node cannot resolve the placed workspace root '{root}': {error}")
            })?;
            if !canonical.is_dir() {
                return Err(format!(
                    "the placed workspace root '{root}' is not a directory on this node"
                ));
            }
            Some(canonical.to_string_lossy().to_string())
        }
        None => None,
    };

    let recipe = little_monkey_lib::recipes::Recipe {
        version: little_monkey_lib::recipes::RECIPE_SCHEMA_VERSION,
        // Recipe names are slugs and a run id is not guaranteed to be one, so
        // the name is derived from its digest.
        name: format!("placed-{}", &sha256_hex(spec.run_id.as_bytes())[..16]),
        description: Some(format!(
            "Run {} placed on this node by an owned machine",
            spec.run_id
        )),
        target,
        workspace,
        permission_mode,
        // The submitter's `instructions` are the system prompt, used verbatim:
        // the placement is enqueued with `snapshot_is_frozen`, which stops this
        // node's own rules from being merged into another machine's run.
        system: spec.instructions.clone(),
        prompt: spec.task.clone(),
        params: Default::default(),
        max_iterations: usize::try_from(spec.budgets.max_iterations).ok(),
        timeout_seconds: Some(spec.budgets.wall_time_ms.div_ceil(1_000).max(1)),
        output: little_monkey_lib::recipes::RecipeOutput { json: true },
        desktop_turn: None,
        placed_run: Some(snapshot),
    };
    little_monkey_lib::recipes::validate_recipe(&recipe)
        .map_err(|error| format!("the placed spec does not form a runnable recipe: {error}"))?;
    Ok(recipe)
}

impl remote::api::PlacementQueue for DaemonPlacementQueue {
    fn place(
        &self,
        spec: &little_monkey_lib::run_protocol::RunSpec,
    ) -> Result<remote::api::PlacedJob, String> {
        let recipe = placed_recipe(spec)?;

        let config = DaemonConfig::load(&self.paths)
            .map_err(|error| format!("this node's background runner is not configured: {error}"))?;
        let mut store = DaemonStore::open(&self.paths)?;
        if store.kill_switch()? {
            return Err("this node's global kill switch is engaged".to_string());
        }
        let mut shared = SharedLedger::open(&self.paths.ledger_db)?;

        let directory = placements_dir(&self.paths);
        std::fs::create_dir_all(&directory)
            .map_err(|error| format!("could not create the placement directory: {error}"))?;
        let source = directory.join(format!("{}.json", placed_job_id(&spec.run_id)));
        write_snapshot(&source, &recipe)?;

        let options = QueueOptions {
            recipe: source.to_string_lossy().into_owned(),
            params: Vec::new(),
            origin: QueueOrigin::Remote {
                request_id: spec.run_id.clone(),
            },
            deterministic_job_id: Some(placed_job_id(&spec.run_id)),
            priority: 0,
            // A placed run is not retried on this node. A submitter that wants
            // another attempt places it again — possibly somewhere else — which
            // is the decision `node_placement::reconcile_placement` makes with
            // information this node does not have.
            max_attempts: 1,
            // The submitter's wall-time budget, enforced here as the node's own
            // job ceiling as well. Two independent enforcements of one number:
            // this one kills the process group, and the child's own
            // `tokio::time::timeout` ends the turn. Neither is a substitute for
            // the other — the first survives a wedged child.
            max_runtime_ms: spec.budgets.wall_time_ms,
            max_memory_bytes: None,
            owned_worktree: false,
            repository: None,
            branch_prefix: "codex/".to_string(),
            allowed_remotes: vec!["origin".to_string()],
            allow_commit: false,
            // Remote Git mutations are never granted by a placement. Enabling
            // them needs an owned worktree and an explicit repository policy,
            // which is an operator decision on *this* machine.
            allow_push: false,
            allow_create_pull_request: false,
            allow_review_comment: false,
            parent_run_id: None,
            snapshot_is_frozen: true,
        };
        let global_config_roots = global_config_roots_for_paths(&self.paths)?;
        let queued = enqueue(
            None,
            &self.paths,
            &global_config_roots,
            &config,
            &mut store,
            &mut shared,
            options,
        )?;
        Ok(remote::api::PlacedJob {
            node_run_id: queued.run_id,
            job_id: queued.job_id,
            state: format!("{:?}", queued.state).to_ascii_lowercase(),
        })
    }

    fn placed_state(&self, job_id: &str) -> Result<Option<remote::api::PlacedJobState>, String> {
        let store = DaemonStore::open(&self.paths)?;
        let Some(job) = store.get_job(job_id)? else {
            return Ok(None);
        };
        Ok(Some(remote::api::PlacedJobState {
            state: format!("{:?}", job.state).to_ascii_lowercase(),
            terminal: matches!(
                job.state,
                JobState::Succeeded | JobState::Failed | JobState::Cancelled
            ),
            updated_at_ms: job.updated_at_ms,
            last_error: job.last_error.clone(),
        }))
    }
}

pub(crate) fn cancel_client_run(run_id: &str, reason: &str) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let job = store.request_cancel(run_id, now_ms()?)?;
    if let Some(run_id) = job.run_id.as_deref() {
        append_cancellation(&paths, run_id, reason)?;
    }
    Ok(())
}

/// Where a queued job came from, for the unified process table.
///
/// The daemon cannot infer this later — a job row looks identical whether a CLI,
/// the desktop, or a paired phone queued it — so the enqueuer states it here and
/// the projection is written while that knowledge is still in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum QueueOrigin {
    /// A local CLI or desktop enqueue. The desktop separately creates the job's
    /// record with its own turn as parent (see `agentLoop.ts`), so nothing is
    /// projected here for this case.
    Local,
    /// A paired remote controller or mobile device. Projected as a `remote_run`
    /// parent with the daemon job as its child, which is what makes remote work
    /// distinguishable from local work in the listing. The remote request and
    /// the job it queued are genuinely two different things, so they are two
    /// records rather than one relabelled kind.
    Remote { request_id: String },
}

struct QueueOptions {
    recipe: String,
    params: Vec<String>,
    origin: QueueOrigin,
    deterministic_job_id: Option<String>,
    priority: i32,
    max_attempts: u32,
    max_runtime_ms: u64,
    max_memory_bytes: Option<u64>,
    owned_worktree: bool,
    repository: Option<PathBuf>,
    branch_prefix: String,
    allowed_remotes: Vec<String>,
    allow_commit: bool,
    allow_push: bool,
    allow_create_pull_request: bool,
    allow_review_comment: bool,
    parent_run_id: Option<String>,
    /// The source recipe is already an immutable, fully rendered snapshot.
    /// In particular, do not merge current rules into its captured system
    /// prompt again when an operator explicitly retries it.
    snapshot_is_frozen: bool,
}

impl QueueOptions {
    fn from_run_args(args: &DaemonRunArgs) -> Self {
        let deterministic_job_id = args.run_key.as_ref().map(|key| {
            format!(
                "job-{}",
                &sha256_hex(format!("daemon-user:{key}").as_bytes())[..32]
            )
        });
        Self {
            recipe: args.name_or_path.clone(),
            params: args.param.clone(),
            deterministic_job_id,
            priority: args.priority,
            max_attempts: args.max_attempts,
            max_runtime_ms: args.max_runtime_seconds.saturating_mul(1000),
            max_memory_bytes: args
                .max_memory_mb
                .and_then(|value| value.checked_mul(1024 * 1024)),
            owned_worktree: args.owned_worktree,
            repository: args.repository.clone(),
            branch_prefix: args.branch_prefix.clone(),
            allowed_remotes: args.allowed_remotes.clone(),
            allow_commit: args.allow_commit,
            allow_push: args.allow_push,
            allow_create_pull_request: args.allow_create_pull_request,
            allow_review_comment: args.allow_review_comment,
            parent_run_id: None,
            origin: QueueOrigin::Local,
            snapshot_is_frozen: false,
        }
    }
}

/// Records a remotely-originated job's lineage: a `remote_run` for the request,
/// and the daemon job as its child.
///
/// Fail-soft. The job is already queued and durable by the time this runs; a
/// projection failure must not turn a successful enqueue into an error the
/// caller sees.
/// `workspace` is the canonicalized root this job will run in. It is threaded
/// through because `reconcile` only writes the `workspace` column when it
/// *admits* a row, and this function is what admits the row for a remote-origin
/// job — omitting it would make every mobile and ACP run permanently invisible to
/// the fair-share charge, which is the class of work most worth charging.
fn project_queue_origin(
    shared: &SharedLedger,
    origin: &QueueOrigin,
    job_id: &str,
    run_id: &str,
    workspace: Option<&str>,
) {
    use little_monkey_lib::process_table::{
        ExitStatus, ProcessExit, ProcessKind, ProcessProjection, ProcessState,
    };

    let QueueOrigin::Remote { request_id } = origin else {
        return;
    };
    let now_ms = match now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("monkey daemon: could not project remote origin: {error}");
            return;
        }
    };

    // Closed in the same write that creates it, rather than left `running`.
    //
    // The request is what this row records, and the request is over: it was
    // accepted and turned into a queued job. The work continues under that job,
    // which is this row's child and has its own state, its own signals and its
    // own restart policy. Projecting the request as `running` — which is what
    // this did — left a row nothing would ever close, because no supervisor
    // owns a request: not the engine tick, which sweeps `daemon_job` rows, and
    // not the desktop reaper, which deliberately skips kinds it does not own.
    // Every remote enqueue leaked one row that claimed to be live forever.
    let table = shared.process_table();
    if let Err(error) = table.reconcile(
        &ProcessProjection::exited(
            ProcessKind::RemoteRun,
            request_id,
            ProcessExit {
                status: ExitStatus::Succeeded,
                code: None,
                signal: None,
                reason: Some("request accepted; the work continues as its daemon job".to_string()),
            },
        ),
        now_ms,
    ) {
        eprintln!("monkey daemon: could not project remote request {request_id}: {error}");
        // Without the parent the edge is unresolvable, so admitting the child
        // here would produce a parentless duplicate of what the engine tick will
        // create anyway. Leave it to the tick.
        return;
    }

    // Created with the parent edge before the engine's own tick reconciles this
    // job: whoever gets there first admits, and the tick then only moves state.
    //
    // Attempt 0 is not a guess. The only caller runs immediately after
    // `insert_preparing` for a job id that `enqueue` has already established is
    // new — an id that resolves to an existing job returns before reaching here
    // — so this job has not started, let alone retried.
    if let Err(error) = table.reconcile(
        &ProcessProjection::new(
            ProcessKind::DaemonJob,
            engine::process_external_id(job_id, 0),
            ProcessState::Admitted,
        )
        .with_parent(ProcessKind::RemoteRun, request_id)
        .with_run(Some(run_id.to_string()))
        .with_workspace(workspace.map(str::to_string)),
        now_ms,
    ) {
        eprintln!("monkey daemon: could not project remote job {job_id}: {error}");
    }
}

fn enqueue(
    // `None` is only valid for frozen-snapshot submissions (see
    // `snapshot_is_frozen`): the CLI value is consulted exclusively to merge
    // rules/facts into a NON-frozen recipe's system prompt.
    cli: Option<&crate::Cli>,
    paths: &DaemonPaths,
    global_config_roots: &[PathBuf],
    config: &DaemonConfig,
    store: &mut DaemonStore,
    shared: &mut SharedLedger,
    options: QueueOptions,
) -> Result<QueuedRun, String> {
    if options.max_attempts == 0 || options.max_attempts > 100 {
        return Err("daemon max_attempts must be between 1 and 100".to_string());
    }
    if options.max_runtime_ms == 0 || options.max_runtime_ms > 7 * 24 * 60 * 60 * 1_000 {
        return Err("daemon max runtime must be between 1 second and 7 days".to_string());
    }
    // Backpressure is honoured here, before anything is created, and that is the
    // point of doing it in this function rather than at the five call sites:
    // every producer — CLI, desktop, ACP, mobile, remote, retry — routes through
    // `enqueue`, so one check covers all of them and none of them can forget it.
    // Checking early also matters because the work below creates an owned git
    // worktree and writes a snapshot; refusing after that would leave both behind
    // for a job that never existed.
    //
    // `insert_preparing`'s own transactional cap stays where it is. That one is
    // authoritative under concurrent enqueues; this one is the informative refusal
    // that names the reason and a retry delay.
    if let Some(refusal) = backpressure_for(store, config)?.refusal() {
        return Err(refusal);
    }
    let job_id = options
        .deterministic_job_id
        .clone()
        .unwrap_or_else(|| format!("job-{}", uuid::Uuid::new_v4()));
    if let Some(existing) = store.get_job(&job_id)? {
        let run_id = existing
            .run_id
            .ok_or_else(|| format!("Existing job '{job_id}' is still preparing"))?;
        return Ok(QueuedRun {
            job_id,
            run_id,
            state: existing.state,
        });
    }
    let workspace_root = std::env::current_dir().ok();
    let (recipe, recipe_path) = recipes::resolve_recipe_with_path(
        &options.recipe,
        workspace_root.as_deref(),
        global_config_roots,
    )?;
    let overrides = parse_params(&options.params)?;
    let rendered = recipes::render_recipe(&recipe, &overrides)?;
    let original_workspace = resolve_recipe_workspace(&recipe, &recipe_path)?;
    // M6A desktop submissions already contain the exact rules/persona/system
    // snapshot observed by the sending WebView. Re-merging whatever rules
    // happen to exist when the resident service dequeues it would violate
    // immutability just as surely as doing so during an explicit retry.
    let snapshot_is_frozen = options.snapshot_is_frozen || recipe.desktop_turn.is_some();
    let effective_system = if snapshot_is_frozen {
        rendered.system.clone()
    } else {
        let cli =
            cli.ok_or("A non-frozen recipe submission requires CLI context for rules merging")?;
        let state = crate::build_state(&Some(original_workspace.clone()))?;
        crate::effective_system(cli, &state, rendered.system.as_deref())
    };

    let mut worktree = None;
    let mut repository_policy = None;
    let workspace = if options.owned_worktree {
        let request = WorktreeRequest {
            repository: options
                .repository
                .clone()
                .unwrap_or_else(|| original_workspace.clone()),
            branch_prefix: options.branch_prefix.clone(),
            allowed_remote_names: options.allowed_remotes.clone(),
            allow_commit: options.allow_commit,
            allow_push: options.allow_push,
            allow_create_pull_request: options.allow_create_pull_request,
            allow_review_comment: options.allow_review_comment,
        };
        let owned = OwnedWorktree::create(paths, &job_id, &request)?;
        let policy = owned.repository_policy(&request);
        policy.validate().map_err(|error| error.to_string())?;
        let path = PathBuf::from(&owned.canonical_path);
        repository_policy = Some(policy);
        worktree = Some(owned);
        path
    } else {
        if options.allow_push || options.allow_create_pull_request || options.allow_review_comment {
            return Err(
                "Remote Git mutations require --owned-worktree and an explicit repository policy"
                    .to_string(),
            );
        }
        original_workspace
    };

    let mut snapshot = recipe;
    snapshot.prompt = rendered.prompt;
    snapshot.system = effective_system;
    snapshot.params.clear();
    snapshot.workspace = Some(
        workspace
            .canonicalize()
            .map_err(|error| format!("Cannot canonicalize daemon workspace: {error}"))?
            .to_string_lossy()
            .to_string(),
    );
    snapshot.output.json = true;
    let snapshot_path = paths.snapshots.join(format!("{job_id}.json"));
    write_snapshot(&snapshot_path, &snapshot)?;
    let created_at_ms = now_ms()?;
    let repository_policy_json = repository_policy
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| error.to_string())?;
    let worktree_json = worktree
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|error| error.to_string())?;
    store.insert_preparing(
        &NewDaemonJob {
            job_id: job_id.clone(),
            recipe_snapshot: snapshot_path.clone(),
            priority: options.priority,
            max_attempts: options.max_attempts,
            created_at_ms,
            max_runtime_ms: options.max_runtime_ms,
            max_memory_bytes: options.max_memory_bytes,
            max_log_bytes: config.max_log_bytes,
            repository_policy_json: repository_policy_json.clone(),
            worktree_json: worktree_json.clone(),
            parent_run_id: options.parent_run_id,
        },
        config.max_queue,
    )?;
    let run_id =
        match submit_queued_snapshot(&snapshot_path, &job_id, repository_policy_json.as_deref()) {
            Ok(run_id) => run_id,
            Err(error) => {
                store.transition(&job_id, JobState::Failed, now_ms()?, None, Some(&error))?;
                return Err(error);
            }
        };
    store.mark_queued(&job_id, &run_id, now_ms()?)?;
    project_queue_origin(
        shared,
        &options.origin,
        &job_id,
        &run_id,
        snapshot.workspace.as_deref(),
    );
    if let Some(owned) = &worktree {
        shared.record_worktree_lease(
            &owned.lease_id,
            &run_id,
            &owned.repository_id,
            &owned.common_git_dir,
            &owned.canonical_path,
            &owned.branch,
            &owned.base_oid,
            Some(&owned.expected_head),
            "active",
            now_ms()?,
        )?;
    }
    Ok(QueuedRun {
        job_id,
        run_id,
        state: JobState::Queued,
    })
}

fn global_config_roots_for_paths(paths: &DaemonPaths) -> Result<Vec<PathBuf>, String> {
    let roots = little_monkey_lib::app_paths::agent_config_roots()?;
    if !daemon_paths_match_profile(paths, &roots.legacy) {
        return Err(
            "The active profile changed while resolving daemon paths; retry the command"
                .to_string(),
        );
    }
    Ok(roots.ordered())
}

fn daemon_paths_match_profile(paths: &DaemonPaths, profile_root: &Path) -> bool {
    paths.root == profile_root.join("daemon")
}

fn submit_queued_snapshot(
    snapshot: &Path,
    job_id: &str,
    repository_policy_json: Option<&str>,
) -> Result<String, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Could not resolve monkey executable: {error}"))?;
    let mut command = Command::new(executable);
    command
        .arg("--no-rules")
        .arg("task")
        .arg("run")
        .arg(snapshot)
        .arg("--run-key")
        .arg(format!("daemon:{job_id}"))
        .arg("--json")
        .env("LITTLE_MONKEY_TASK_QUEUE_ONLY", "1")
        .env("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT", "1")
        .stdin(Stdio::null());
    if let Some(policy) = repository_policy_json {
        command.env("LITTLE_MONKEY_DAEMON_REPOSITORY_POLICY_JSON", policy);
    }
    let output = command
        .output()
        .map_err(|error| format!("Failed to submit queued durable run: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "Queue submission did not return JSON ({error}): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    if !output.status.success()
        || value.get("status").and_then(|value| value.as_str()) != Some("queued")
    {
        return Err(value
            .get("final_message")
            .and_then(|value| value.as_str())
            .unwrap_or("durable queue submission failed")
            .to_string());
    }
    value
        .get("run_id")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| "Queue submission omitted run_id".to_string())
}

fn write_snapshot(path: &Path, recipe: &Recipe) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(recipe).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Failed to write immutable recipe snapshot: {error}"))?;
    store::restrict_file(&tmp)?;
    std::fs::rename(&tmp, path)
        .map_err(|error| format!("Failed to publish immutable recipe snapshot: {error}"))?;
    store::restrict_file(path)
}

fn resolve_recipe_workspace(recipe: &Recipe, recipe_path: &Path) -> Result<PathBuf, String> {
    let value = match &recipe.workspace {
        Some(value) => recipe_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(value),
        None => std::env::current_dir().map_err(|error| error.to_string())?,
    };
    value.canonicalize().map_err(|error| {
        format!(
            "Cannot resolve recipe workspace '{}': {error}",
            value.display()
        )
    })
}

fn parse_params(values: &[String]) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    for value in values {
        let (key, item) = value
            .split_once('=')
            .ok_or_else(|| format!("--param '{value}' must be key=value"))?;
        if key.is_empty() {
            return Err(format!("--param '{value}' has an empty key"));
        }
        out.insert(key.to_string(), item.to_string());
    }
    Ok(out)
}

async fn attach(run_id: &str, follow: bool, json: bool) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let shared = SharedLedger::open(&paths.ledger_db)?;
    let mut sequence = 0;
    loop {
        let events = shared.events(run_id, sequence, 1000)?;
        for event in events {
            sequence = event.sequence;
            if json {
                println!("{}", serde_json::to_string(&event).unwrap_or_default());
            } else {
                println!("{:>6}  {}", event.sequence, event_type(&event.event));
            }
        }
        let run = shared
            .load_run(run_id)?
            .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
        if !follow || run.status.is_terminal() {
            if !json {
                println!("status={:?}", run.status);
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn event_type(event: &RunEvent) -> &'static str {
    match event {
        RunEvent::Queued { .. } => "queued",
        RunEvent::Started { .. } => "started",
        RunEvent::ModelDelta { .. } => "model_delta",
        RunEvent::ToolProposed { .. } => "tool_proposed",
        RunEvent::PermissionRequested { .. } => "permission_requested",
        RunEvent::PermissionDecided { .. } => "permission_decided",
        RunEvent::RoutingDecided { .. } => "routing_decided",
        RunEvent::ToolStarted { .. } => "tool_started",
        RunEvent::ToolFinished { .. } => "tool_finished",
        RunEvent::ArtifactAdded { .. } => "artifact_added",
        RunEvent::CheckpointLinked { .. } => "checkpoint_linked",
        RunEvent::VerificationFinished { .. } => "verification_finished",
        RunEvent::UsageRecorded { .. } => "usage_recorded",
        RunEvent::CancellationRequested { .. } => "cancellation_requested",
        RunEvent::ExternalMutationPrepared { .. } => "external_mutation_prepared",
        RunEvent::ExternalMutationConfirmed { .. } => "external_mutation_confirmed",
        RunEvent::AwaitingApproval { .. } => "awaiting_approval",
        RunEvent::Paused { .. } => "paused",
        RunEvent::Cancelling { .. } => "cancelling",
        RunEvent::Completed { .. } => "completed",
        RunEvent::Failed { .. } => "failed",
        RunEvent::Cancelled { .. } => "cancelled",
        RunEvent::NeedsReconciliation { .. } => "needs_reconciliation",
        RunEvent::MigrationDeparted { .. } => "migration_departed",
        RunEvent::MigrationArrived { .. } => "migration_arrived",
    }
}

fn pause(run_id: &str) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let job = store.request_pause(run_id, true, now_ms()?)?;
    println!("Pause requested for {}", job.run_id.unwrap_or(job.job_id));
    Ok(())
}

fn resume(run_id: &str) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let job = store.request_pause(run_id, false, now_ms()?)?;
    println!("Resume requested for {}", job.run_id.unwrap_or(job.job_id));
    Ok(())
}

fn cancel(run_id: &str, reason: Option<&str>) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let job = store.request_cancel(run_id, now_ms()?)?;
    if let Some(run_id) = job.run_id.as_deref() {
        append_cancellation(&paths, run_id, reason.unwrap_or("Cancelled by daemon user"))?;
    }
    println!("Cancellation requested for {run_id}");
    Ok(())
}

fn append_cancellation(paths: &DaemonPaths, run_id: &str, reason: &str) -> Result<(), String> {
    let shared = SharedLedger::open(&paths.ledger_db)?;
    let run = shared
        .load_run(run_id)?
        .ok_or_else(|| format!("Unknown run '{run_id}'"))?;
    if run.status.is_terminal() || run.status == RunStatus::Cancelling {
        return Ok(());
    }
    let recorder = control_recorder(&shared, run_id)?;
    recorder.emit(RunEvent::CancellationRequested {
        requested_by: recorder.client_identity(),
        reason: Some(bounded_text(reason, 60 * 1024)),
    })?;
    recorder.emit(RunEvent::Cancelling {
        reason: Some(bounded_text(reason, 60 * 1024)),
    })
}

fn retry(cli: &crate::Cli, run_id: &str, acknowledge: bool) -> Result<(), String> {
    let config_roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let paths = DaemonPaths::under(&config_roots.legacy);
    let global_config_roots = config_roots.ordered();
    let config = DaemonConfig::load(&paths)?;
    let mut store = DaemonStore::open(&paths)?;
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    let prior = store
        .get_job(run_id)?
        .ok_or_else(|| format!("Unknown daemon run '{run_id}'"))?;
    if !prior.state.is_terminal() {
        return Err("Only terminal daemon runs can be retried".to_string());
    }
    let prior_run = prior.run_id.as_deref().unwrap_or(run_id);
    let mutations = shared.mutations(prior_run)?;
    if (!mutations.is_empty() || prior.state == JobState::NeedsReconciliation) && !acknowledge {
        return Err(
            "Retry requires --acknowledge-side-effects because the prior run reached an external-mutation boundary"
                .to_string(),
        );
    }
    let recipe: Recipe = serde_json::from_slice(
        &std::fs::read(&prior.recipe_snapshot)
            .map_err(|error| format!("Cannot read prior immutable snapshot: {error}"))?,
    )
    .map_err(|error| format!("Prior immutable snapshot is invalid: {error}"))?;
    let retry_source = paths
        .snapshots
        .join(format!("retry-source-{}.json", uuid::Uuid::new_v4()));
    write_snapshot(&retry_source, &recipe)?;
    let options = QueueOptions {
        origin: QueueOrigin::Local,
        recipe: retry_source.to_string_lossy().to_string(),
        params: vec![],
        deterministic_job_id: None,
        priority: prior.priority,
        max_attempts: prior.max_attempts,
        max_runtime_ms: prior.max_runtime_ms,
        max_memory_bytes: prior.max_memory_bytes,
        owned_worktree: false,
        repository: None,
        branch_prefix: "codex/".into(),
        allowed_remotes: vec!["origin".into()],
        allow_commit: true,
        allow_push: false,
        allow_create_pull_request: false,
        allow_review_comment: false,
        parent_run_id: prior.run_id,
        snapshot_is_frozen: true,
    };
    let queued = enqueue(
        Some(cli),
        &paths,
        &global_config_roots,
        &config,
        &mut store,
        &mut shared,
        options,
    )?;
    let _ = std::fs::remove_file(retry_source);
    println!("Queued retry {} as {}", queued.job_id, queued.run_id);
    Ok(())
}

fn approve(run_id: &str, request_id: &str, decision: PermissionDecision) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let config = DaemonConfig::load(&paths)?;
    let store = DaemonStore::open(&paths)?;
    let shared = SharedLedger::open(&paths.ledger_db)?;
    let engine = DaemonEngine::new(
        store,
        shared,
        paths,
        config,
        RealProcessAdapter::current()?,
        OsNotificationAdapter,
        SystemClock,
        format!("daemon-control-{}", std::process::id()),
    );
    engine.decide_approval(run_id, request_id, decision)?;
    println!("Approval {request_id} recorded for {run_id}");
    Ok(())
}

fn kill_switch(action: &KillSwitchCmd) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    match action {
        KillSwitchCmd::Engage => {
            store.set_kill_switch(true)?;
            let count = store.request_cancel_all(now_ms()?)?;
            println!("Kill switch engaged; cancellation requested for {count} run(s).");
        }
        KillSwitchCmd::Release => {
            store.set_kill_switch(false)?;
            println!("Kill switch released. New work may start.");
        }
        KillSwitchCmd::Status => println!(
            "{}",
            if store.kill_switch()? {
                "engaged"
            } else {
                "released"
            }
        ),
    }
    Ok(())
}

fn desktop_control_command(action: &DesktopControlCmd) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    match action {
        DesktopControlCmd::EmergencyStop => {
            // Set the durable flag the resident serve loop polls; it force-
            // stops every live desktop-control session on its next tick via
            // `DesktopControlRuntime::enforce`. Setting it while no daemon is
            // running is harmless — there are no live sessions to stop, and the
            // flag is cleared when the service next starts.
            store.set_meta("desktop_control_stop_requested", "1")?;
            println!(
                "Emergency stop requested; the resident daemon will force-stop every active \
                 remote desktop-control session."
            );
        }
    }
    Ok(())
}

fn trigger_command(action: &TriggerCmd) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    DaemonConfig::load(&paths)?;
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    let mut state = DaemonStore::open(&paths)?;
    let secrets = KeyringSecretStore;
    match action {
        TriggerCmd::AddCron {
            id,
            target,
            cron,
            param,
            payload_param,
        } => {
            let (target, workflow) = build_trigger_target(target, param, payload_param)?;
            let config = TriggerConfig::Cron {
                target,
                workflow,
                schedule: cron.clone(),
            };
            add_trigger(&mut shared, id, config, None)?;
        }
        TriggerCmd::AddFilesystem {
            id,
            target,
            path,
            recursive,
            param,
            payload_param,
        } => {
            let (target, workflow) = build_trigger_target(target, param, payload_param)?;
            let pattern = workflow
                .as_ref()
                .and_then(|binding| match &binding.trigger {
                    WorkflowTrigger::Filesystem { pattern, .. } => Some(pattern.clone()),
                    _ => None,
                });
            let config = TriggerConfig::Filesystem {
                target,
                workflow,
                path: trigger::canonicalize_trigger_path(path.clone())?,
                recursive: *recursive,
                pattern,
                last_fingerprint: None,
            };
            add_trigger(&mut shared, id, config, None)?;
        }
        TriggerCmd::AddWebhook {
            id,
            target,
            secret_env,
            param,
            payload_param,
            max_skew_ms,
        } => {
            let secret = std::env::var(secret_env)
                .map_err(|_| format!("Environment variable '{secret_env}' is not set"))?;
            let (target, workflow) = build_trigger_target(target, param, payload_param)?;
            let secret_reference = workflow
                .as_ref()
                .and_then(|binding| match &binding.trigger {
                    WorkflowTrigger::SignedWebhook {
                        secret_reference, ..
                    } => Some(secret_reference.clone()),
                    _ => None,
                });
            let config = TriggerConfig::SignedWebhook {
                target,
                workflow,
                secret_reference,
                max_skew_ms: *max_skew_ms,
            };
            let secret_slot = config.secret_reference(id).to_string();
            add_trigger(
                &mut shared,
                id,
                config,
                Some((&secrets, &secret_slot, &secret)),
            )?;
        }
        TriggerCmd::AddGithub {
            id,
            target,
            secret_env,
            repository,
            local_repository,
            remote,
            branch_prefixes,
            events,
            allow_push,
            allow_create_pull_request,
            allow_review_comment,
            param,
            payload_param,
            max_skew_ms,
        } => {
            let secret = std::env::var(secret_env)
                .map_err(|_| format!("Environment variable '{secret_env}' is not set"))?;
            let (target, workflow) = build_trigger_target(target, param, payload_param)?;
            let local_repository = local_repository
                .canonicalize()
                .map_err(|error| format!("Cannot canonicalize local repository: {error}"))?;
            let config = TriggerConfig::Github {
                target,
                workflow,
                repository: repository.clone(),
                local_repository: local_repository.to_string_lossy().to_string(),
                remote_name: remote.clone(),
                branch_prefixes: branch_prefixes.clone(),
                events: events.clone(),
                allow_push: *allow_push,
                allow_create_pull_request: *allow_create_pull_request,
                allow_review_comment: *allow_review_comment,
                max_skew_ms: *max_skew_ms,
            };
            add_trigger(&mut shared, id, config, Some((&secrets, id, &secret)))?;
        }
        TriggerCmd::List { json } => {
            let triggers = shared.list_triggers()?;
            if *json {
                let values = triggers
                    .iter()
                    .map(|trigger| {
                        serde_json::json!({
                            "id": trigger.trigger_id,
                            "kind": trigger.kind,
                            "enabled": trigger.enabled,
                            "created_at_ms": trigger.created_at_ms,
                            "updated_at_ms": trigger.updated_at_ms,
                            "next_fire_at_ms": trigger.next_fire_at_ms,
                            "last_delivery_at_ms": trigger.last_delivery_at_ms,
                            "config": serde_json::from_slice::<serde_json::Value>(&trigger.config_json)
                                .unwrap_or(serde_json::Value::Null),
                        })
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&values).unwrap_or_default()
                );
            } else {
                for trigger in triggers {
                    println!(
                        "{}\t{}\t{}",
                        trigger.trigger_id, trigger.kind, trigger.enabled
                    );
                }
            }
        }
        TriggerCmd::Remove { id } => {
            let stored = shared
                .trigger(id)?
                .ok_or_else(|| format!("Unknown trigger '{id}'"))?;
            let config: TriggerConfig = serde_json::from_slice(&stored.config_json)
                .map_err(|error| format!("Invalid trigger '{id}': {error}"))?;
            if config
                .workflow_binding()
                .is_some_and(|binding| binding.managed_by_batch)
            {
                return Err(
                    "M4-managed trigger must be removed by unregistering its workflow batch"
                        .to_string(),
                );
            }
            if !shared.disable_trigger(id, now_ms()?)? {
                return Err(format!("Unknown trigger '{id}'"));
            }
            secrets.delete(config.secret_reference(id))?;
            println!("Trigger '{id}' disabled and its signing secret removed.");
        }
        TriggerCmd::SecretSet {
            reference,
            secret_env,
        } => {
            trigger::validate_secret_reference(reference)?;
            let secret = std::env::var(secret_env)
                .map_err(|_| format!("Environment variable '{secret_env}' is not set"))?;
            secrets.put(reference, &secret)?;
            println!("Webhook secret reference '{reference}' stored in the OS keychain.");
        }
        TriggerCmd::SecretRemove { reference } => {
            trigger::validate_secret_reference(reference)?;
            secrets.delete(reference)?;
            println!("Webhook secret reference '{reference}' removed from the OS keychain.");
        }
        TriggerCmd::Deliver {
            id,
            delivery_id,
            timestamp_ms,
            nonce,
            signature,
            payload,
            event,
        } => {
            let bytes = read_payload(payload)?;
            let outcome = ingest_signed_delivery(
                &mut shared,
                &mut state,
                &secrets,
                &SignedDelivery {
                    trigger_id: id,
                    delivery_id,
                    timestamp_ms: *timestamp_ms,
                    nonce,
                    signature,
                    event_name: event.as_deref(),
                    payload: &bytes,
                },
                now_ms()?,
            )?;
            println!("{}", serde_json::to_string(&outcome).unwrap_or_default());
            if outcome == IngestOutcome::Rejected {
                return Err("Signed delivery was rejected".to_string());
            }
        }
    }
    Ok(())
}

fn add_trigger(
    shared: &mut SharedLedger,
    id: &str,
    config: TriggerConfig,
    secret: Option<(&dyn SecretStore, &str, &str)>,
) -> Result<(), String> {
    validate_trigger_id(id)?;
    config.validate()?;
    validate_trigger_recipe(&config)?;
    if let Some((store, slot, secret)) = secret {
        store.put(slot, secret)?;
    }
    let next = match &config {
        TriggerConfig::Cron { schedule, .. } => Some(trigger::next_cron_ms(schedule, now_ms()?)?),
        _ => None,
    };
    shared.upsert_trigger(
        id,
        config.kind_token(),
        &serde_json::to_vec(&config).map_err(|error| error.to_string())?,
        now_ms()?,
        next,
    )?;
    println!("Trigger '{id}' installed ({})", config.kind_token());
    Ok(())
}

fn validate_trigger_recipe(config: &TriggerConfig) -> Result<(), String> {
    let Some((recipe_name, params, payload_param)) = config.recipe_target() else {
        return Ok(());
    };
    let global_config_roots = recipes::global_config_roots()?;
    let cwd = std::env::current_dir().ok();
    let recipe = recipes::resolve_recipe(recipe_name, cwd.as_deref(), &global_config_roots)?;
    for key in params.keys() {
        if !recipe.params.contains_key(key) {
            return Err(format!(
                "Trigger parameter '{key}' is not declared by the recipe"
            ));
        }
    }
    if let Some(key) = payload_param {
        if !recipe.params.contains_key(key) {
            return Err(format!(
                "Payload parameter '{key}' is not declared by the recipe"
            ));
        }
    }
    Ok(())
}

fn build_trigger_target(
    args: &TriggerTargetArgs,
    params: &[String],
    payload_param: &Option<String>,
) -> Result<(TriggerTarget, Option<WorkflowTriggerBinding>), String> {
    let workflow_fields_present = args.workflow_id.is_some()
        || args.definition_sha256.is_some()
        || args.workflow_version.is_some()
        || args.workflow_trigger_json.is_some();
    if let Some(recipe) = &args.recipe {
        if workflow_fields_present {
            return Err("RECIPE and workflow target flags are mutually exclusive".to_string());
        }
        return Ok((
            TriggerTarget::Recipe {
                recipe: recipe.clone(),
                params: parse_btree_params(params)?,
                payload_param: payload_param.clone(),
            },
            None,
        ));
    }
    if !params.is_empty() || payload_param.is_some() {
        return Err("--param/--payload-param are supported only for recipe targets".to_string());
    }
    let workflow_id = args
        .workflow_id
        .clone()
        .ok_or_else(|| "Provide either a RECIPE or all M4 workflow target flags".to_string())?;
    let definition_sha256 = args
        .definition_sha256
        .clone()
        .ok_or_else(|| "Workflow target requires --definition-sha256".to_string())?;
    let workflow_version = args
        .workflow_version
        .ok_or_else(|| "Workflow target requires --workflow-version".to_string())?;
    let trigger_json = args
        .workflow_trigger_json
        .as_deref()
        .ok_or_else(|| "Workflow target requires --workflow-trigger-json".to_string())?;
    let declared_trigger: WorkflowTrigger = serde_json::from_str(trigger_json)
        .map_err(|error| format!("Invalid --workflow-trigger-json: {error}"))?;
    Ok((
        TriggerTarget::Workflow {
            workflow_id,
            definition_sha256,
        },
        Some(WorkflowTriggerBinding {
            workflow_version,
            managed_by_batch: false,
            trigger: declared_trigger,
        }),
    ))
}

fn validate_trigger_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        Err("Trigger id must contain only letters, digits, '-' or '_'".to_string())
    } else {
        Ok(())
    }
}

fn parse_btree_params(values: &[String]) -> Result<BTreeMap<String, String>, String> {
    Ok(parse_params(values)?.into_iter().collect())
}

fn read_payload(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    if path == Path::new("-") {
        std::io::stdin()
            .take((trigger::MAX_WEBHOOK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Failed to read webhook payload: {error}"))?;
    } else {
        bytes = std::fs::read(path)
            .map_err(|error| format!("Failed to read webhook payload: {error}"))?;
    }
    if bytes.len() > trigger::MAX_WEBHOOK_BYTES {
        return Err("Webhook payload exceeds 1 MiB".to_string());
    }
    Ok(bytes)
}

/// Closes workflow rows whose host process is gone, once, before the first tick.
///
/// The daemon's own crash coverage is the engine tick, which sweeps `daemon_job`
/// and nothing else; the desktop's is a startup reap scoped to the kinds it owns.
/// A workflow run belongs to neither — both processes host runs through the same
/// service into the same ledger — so it had none. Host liveness answers it for
/// both, and running the pass here as well as in the app means a headless machine
/// that never opens the desktop still gets cleaned up.
///
/// Logged and swallowed: a stale row is not a reason to refuse to serve.
fn reap_dead_workflow_hosts(shared: &SharedLedger) {
    let now = match now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    {
        Ok(value) => value,
        Err(error) => {
            eprintln!("monkey daemon: workflow host reap skipped: {error}");
            return;
        }
    };
    match little_monkey_lib::process_table::reap_processes_whose_host_died(
        &shared.process_table(),
        now,
    ) {
        Ok(reaped) if !reaped.is_empty() => eprintln!(
            "monkey daemon: reaped {} workflow process(es) whose host is gone",
            reaped.len()
        ),
        Ok(_) => {}
        Err(error) => eprintln!("monkey daemon: workflow host reap failed: {error}"),
    }
}

async fn serve(cli: &crate::Cli) -> Result<(), String> {
    let roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let paths = DaemonPaths::under(&roots.legacy);
    let config = DaemonConfig::load(&paths)?;
    paths.ensure()?;
    let _lock = DaemonLock::acquire(&paths.lock)?;
    let mut store = DaemonStore::open(&paths)?;
    store.set_meta("profile_id", &roots.profile_id)?;
    store.set_meta("stop_requested", "0")?;
    // A stale escape-hatch flag from a prior run must not instantly kill the
    // first session of this run.
    store.set_meta("desktop_control_stop_requested", "0")?;
    recover_preparing(&paths, &mut store)?;
    let mut shared = SharedLedger::open(&paths.ledger_db)?;
    reconcile_reserved_deliveries(&mut store, &mut shared)?;
    let owner_id = format!("daemon-{}-{}", std::process::id(), uuid::Uuid::new_v4());
    // The identity this daemon serves (K23). Its quota bounds what this profile
    // may run at once, and its weight bounds the share of the machine it may
    // claim when another profile's daemon is also running — the two identities
    // have separate queues, so hardware is the only thing they contend for and
    // the only place a cross-profile share can be enforced.
    let profile_limits = match little_monkey_lib::app_paths::base_data_dir()
        .ok_or_else(|| "Could not resolve the app data directory".to_string())
        .and_then(|base| {
            little_monkey_lib::profiles::ProfileLimits::for_active(&base)
                .map_err(|error| error.to_string())
        }) {
        Ok(limits) => limits,
        Err(error) => {
            eprintln!("monkey daemon: running without profile limits: {error}");
            little_monkey_lib::profiles::ProfileLimits::unbounded()
        }
    };
    let mut engine = DaemonEngine::new(
        store,
        shared,
        paths.clone(),
        config.clone(),
        RealProcessAdapter::current()?,
        OsNotificationAdapter,
        SystemClock,
        owner_id,
    )
    .with_profile_limits(profile_limits);
    engine.recover()?;
    reap_dead_workflow_hosts(&engine.shared);
    // One machine-wide desktop-control runtime, shared with the remote API so
    // the serve loop can enforce revoke / kill-switch / escape-hatch stops on
    // the very same live sessions the API creates.
    let desktop_control = remote::DesktopControlRuntime::production(&paths);
    remote::spawn_if_configured(
        paths.clone(),
        desktop_control.clone(),
        std::sync::Arc::new(DaemonMobileChatQueue::new(paths.clone())),
        std::sync::Arc::new(DaemonPlacementQueue::new(paths.clone())),
    )
    .await?;
    spawn_knowledge_refresh_scheduler()?;
    // What tells a paired phone that a run wants an approval, or has finished.
    // Its own task, reading the job table the rest of this process writes, so a
    // transition raises its notification whichever code path caused it — the
    // scheduler, a reporting child, or the crash reconciler.
    remote::watch::spawn(paths.clone());
    // Messaging channels. Its own task rather than a step in the loop below: a
    // long-polling provider blocks for half a minute at a time, and the queue
    // must keep ticking while it does.
    channel_worker::spawn_channel_runtime(paths.clone());
    // Call limits. Separate from the channel runtime because it enforces
    // deadlines rather than moving messages: a call nobody is watching keeps
    // costing money whether or not any provider is polling.
    telecom_worker::spawn_telecom_runtime(paths.clone());
    spawn_webdav_backup_scheduler()?;
    if let Some(port) = config.webhook_port {
        webhook::spawn_local_listener(paths.clone(), port).await?;
    }
    let mut workflow_trigger_sync = WorkflowBatchSynchronizer::default();
    // Roadmap K17 S4: the heartbeat. Every machine's daemon is both a node and a
    // placer, so this runs here rather than in a separate controller process —
    // there isn't one. On a machine that has placed nothing it is a table read
    // and no network at all, which is why it can afford to sit in the main loop.
    let mut next_placement_sync_ms = 0u64;
    loop {
        let now = now_ms()?;
        if now >= next_placement_sync_ms {
            next_placement_sync_ms =
                now.saturating_add(little_monkey_lib::node_placement::HEARTBEAT_INTERVAL_MS);
            // A node that is down must never take the resident service with it:
            // every failure here is already recorded against the placement it
            // belongs to, and the loop keeps its own queue running regardless.
            if let Err(error) = remote::placement_sync(&paths).await {
                eprintln!("monkey daemon: placement sync paused: {error}");
            }
        }
        if let Err(error) =
            workflow_trigger_sync.sync_if_changed(&paths.root, &mut engine.shared, now)
        {
            // A malformed or rolled-back batch must leave the prior atomic
            // registration active, but it must not take the resident service
            // (and unrelated queued work) offline.
            eprintln!("Rejected M4 workflow trigger batch: {error}");
        }
        poll_persistent_triggers(&mut engine.shared, &mut engine.store, now)?;
        if let Err(error) =
            process_pending_deliveries(cli, &paths, &config, &mut engine.store, &mut engine.shared)
        {
            eprintln!("Persistent trigger delivery paused: {error}");
        }
        engine.tick()?;
        // Enforce cross-process desktop-control stops: an engaged kill switch,
        // a local `desktop-control emergency-stop` escape hatch, or a revoked
        // device with a still-live session. Runs every tick; cheap when idle.
        let kill_switch_engaged = engine.store.kill_switch().unwrap_or(false);
        let escape_hatch = engine
            .store
            .get_meta("desktop_control_stop_requested")
            .ok()
            .flatten()
            .as_deref()
            == Some("1");
        desktop_control.enforce(kill_switch_engaged, escape_hatch);
        if escape_hatch {
            let _ = engine.store.set_meta("desktop_control_stop_requested", "0");
        }
        if engine.store.get_meta("stop_requested")?.as_deref() == Some("1") {
            engine.store.request_cancel_all(now)?;
            engine.tick()?;
            if engine.active_count() == 0 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(config.poll_interval_ms)).await;
    }
    engine.store.set_meta("pid", "")?;
    engine.store.set_meta("heartbeat_ms", "0")?;
    Ok(())
}

fn spawn_knowledge_refresh_scheduler() -> Result<(), String> {
    let app_data = crate::app_data_dir()
        .ok_or_else(|| "Could not resolve app data for Knowledge background refresh".to_string())?;
    tokio::spawn(async move {
        loop {
            let now = now_ms().unwrap_or_default();
            match little_monkey_lib::knowledge_service::run_due_background_refresh(&app_data, now)
                .await
            {
                Ok(outcome) if !outcome.failures.is_empty() => {
                    eprintln!(
                        "Knowledge background refresh completed with errors: {}",
                        outcome.failures.join("; ")
                    );
                }
                Err(error) => eprintln!("Knowledge background refresh check failed: {error}"),
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    Ok(())
}

fn spawn_webdav_backup_scheduler() -> Result<(), String> {
    let app_data = crate::app_data_dir()
        .ok_or_else(|| "Could not resolve app data for WebDAV background backup".to_string())?;
    let owner = format!(
        "daemon-webdav-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    tokio::spawn(async move {
        loop {
            let now = now_ms().unwrap_or_default();
            if let Err(error) = little_monkey_lib::portability_commands::run_due_webdav_backup(
                &app_data, &owner, now, false,
            )
            .await
            {
                eprintln!("WebDAV background backup check failed: {error}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
    Ok(())
}

fn recover_preparing(paths: &DaemonPaths, store: &mut DaemonStore) -> Result<(), String> {
    for job in store.stale_preparing(now_ms()?)? {
        match submit_queued_snapshot(
            &job.recipe_snapshot,
            &job.job_id,
            job.repository_policy_json.as_deref(),
        ) {
            Ok(run_id) => store.mark_queued(&job.job_id, &run_id, now_ms()?)?,
            Err(error) => {
                store.transition(&job.job_id, JobState::Failed, now_ms()?, None, Some(&error))?
            }
        }
    }
    let _ = paths;
    Ok(())
}

/// Completes the cross-database hand-off for signed deliveries that were
/// durably reserved in daemon state before a crash. The shared ledger remains
/// authoritative for replay/conflict decisions; an already-submitted delivery
/// is safe to reactivate because its downstream job id is deterministic.
fn reconcile_reserved_deliveries(
    store: &mut DaemonStore,
    shared: &mut SharedLedger,
) -> Result<(), String> {
    for pending in store.reserved_delivery_payloads(256)? {
        let payload_sha256 = sha256_hex(pending.payload_json.as_bytes());
        let disposition = match shared.delivery(&pending.trigger_id, &pending.delivery_id)? {
            None => shared.accept_delivery(
                &pending.trigger_id,
                &pending.delivery_id,
                &payload_sha256,
                pending.received_at_ms,
            )?,
            Some((status, stored_sha256, _)) if stored_sha256 == payload_sha256 => {
                if status == "accepted" || status == "submitted" {
                    ledger::DeliveryDisposition::Duplicate
                } else {
                    ledger::DeliveryDisposition::ConflictingDuplicate
                }
            }
            Some(_) => ledger::DeliveryDisposition::ConflictingDuplicate,
        };
        match disposition {
            ledger::DeliveryDisposition::Accepted | ledger::DeliveryDisposition::Duplicate => {
                store.activate_delivery_payload(&pending.trigger_id, &pending.delivery_id)?;
            }
            ledger::DeliveryDisposition::ConflictingDuplicate => {
                store.discard_delivery_payload(&pending.trigger_id, &pending.delivery_id)?;
            }
        }
    }
    Ok(())
}

fn process_pending_deliveries(
    cli: &crate::Cli,
    paths: &DaemonPaths,
    config: &DaemonConfig,
    store: &mut DaemonStore,
    shared: &mut SharedLedger,
) -> Result<(), String> {
    for pending in store.pending_delivery_payloads(64)? {
        if let Err(error) =
            process_one_pending_delivery(cli, paths, config, store, shared, &pending)
        {
            eprintln!(
                "Persistent trigger delivery '{}/{}' paused: {error}",
                pending.trigger_id, pending.delivery_id
            );
        }
    }
    Ok(())
}

fn process_one_pending_delivery(
    cli: &crate::Cli,
    paths: &DaemonPaths,
    config: &DaemonConfig,
    store: &mut DaemonStore,
    shared: &mut SharedLedger,
    pending: &PendingDelivery,
) -> Result<(), String> {
    let Some(stored_trigger) = shared.trigger(&pending.trigger_id)? else {
        return Ok(());
    };
    let trigger: TriggerConfig =
        serde_json::from_slice(&stored_trigger.config_json).map_err(|error| error.to_string())?;
    trigger.validate()?;
    if let Some((workflow_id, definition_sha256, binding)) = trigger.workflow_target() {
        return dispatch_workflow_delivery(
            paths,
            store,
            shared,
            pending,
            workflow_id,
            definition_sha256,
            &binding.trigger,
        );
    }
    let (recipe_name, recipe_params, payload_param) = trigger
        .recipe_target()
        .ok_or_else(|| "Trigger target is neither a recipe nor a workflow".to_string())?;
    let mut params = recipe_params
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    if let Some(key) = payload_param {
        params.push(format!("{key}={}", pending.payload_json));
    }
    let deterministic_job_id = format!(
        "trigger-{}",
        &sha256_hex(format!("{}:{}", pending.trigger_id, pending.delivery_id).as_bytes())[..32]
    );
    let mut options = QueueOptions {
        origin: QueueOrigin::Local,
        recipe: recipe_name.to_string(),
        params,
        deterministic_job_id: Some(deterministic_job_id),
        priority: 0,
        max_attempts: 1,
        max_runtime_ms: 7 * 24 * 60 * 60 * 1_000,
        max_memory_bytes: None,
        owned_worktree: false,
        repository: None,
        branch_prefix: "codex/".into(),
        allowed_remotes: vec!["origin".into()],
        allow_commit: true,
        allow_push: false,
        allow_create_pull_request: false,
        allow_review_comment: false,
        parent_run_id: None,
        snapshot_is_frozen: false,
    };
    if let TriggerConfig::Github {
        local_repository,
        remote_name,
        branch_prefixes,
        allow_push,
        allow_create_pull_request,
        allow_review_comment,
        ..
    } = &trigger
    {
        options.owned_worktree = true;
        options.repository = Some(PathBuf::from(local_repository));
        options.allowed_remotes = vec![remote_name.clone()];
        options.branch_prefix = branch_prefixes
            .first()
            .cloned()
            .ok_or_else(|| "GitHub trigger has no branch policy".to_string())?;
        options.allow_push = *allow_push;
        options.allow_create_pull_request = *allow_create_pull_request;
        options.allow_review_comment = *allow_review_comment;
    }
    let global_config_roots = global_config_roots_for_paths(paths)?;
    let queued = enqueue(
        Some(cli),
        paths,
        &global_config_roots,
        config,
        store,
        shared,
        options,
    )?;
    shared.mark_delivery_submitted(
        &pending.trigger_id,
        &pending.delivery_id,
        &queued.run_id,
        now_ms()?,
    )?;
    store.mark_delivery_submitted(&pending.trigger_id, &pending.delivery_id, &queued.job_id)
}

fn dispatch_workflow_delivery(
    paths: &DaemonPaths,
    store: &mut DaemonStore,
    shared: &mut SharedLedger,
    pending: &PendingDelivery,
    workflow_id: &str,
    definition_sha256: &str,
    declared_trigger: &WorkflowTrigger,
) -> Result<(), String> {
    let deterministic_run_id = format!(
        "m4-trigger-{}",
        &sha256_hex(format!("{}:{}", pending.trigger_id, pending.delivery_id).as_bytes())[..32]
    );
    match shared.delivery(&pending.trigger_id, &pending.delivery_id)? {
        Some((status, _, None)) if status == "submitted" => {
            // Crash window: M4 history and the shared submission marker were
            // committed, but daemon-local payload bookkeeping was not.
            store.mark_delivery_submitted_external(&pending.trigger_id, &pending.delivery_id)?;
            return Ok(());
        }
        Some((status, _, None)) if status == "accepted" => {}
        Some((status, _, run_id)) => {
            return Err(format!(
                "Workflow delivery has incompatible shared state status={status} run_id={run_id:?}"
            ));
        }
        None => {
            return Err("Workflow delivery is missing from the shared replay ledger".to_string())
        }
    }

    let payload_json: serde_json::Value = serde_json::from_str(&pending.payload_json)
        .map_err(|error| format!("Stored trigger payload is invalid JSON: {error}"))?;
    let app_data = paths
        .root
        .parent()
        .ok_or_else(|| "Daemon root has no app-data parent".to_string())?;
    let history = little_monkey_lib::m4_runtime::run_daemon_workflow_delivery(
        app_data,
        workflow_id,
        definition_sha256,
        &deterministic_run_id,
        declared_trigger.clone(),
        payload_json,
    )?;
    if history.run_id != deterministic_run_id
        || history.workflow_id != workflow_id
        || history.definition_sha256 != definition_sha256
        || &history.trigger != declared_trigger
    {
        return Err("M4 workflow delivery returned mismatched durable history".to_string());
    }
    // The M4 append-only history commits first. A crash before either marker
    // below safely re-enters with the same run id and receives that history.
    shared.mark_delivery_submitted_external(
        &pending.trigger_id,
        &pending.delivery_id,
        now_ms()?,
    )?;
    store.mark_delivery_submitted_external(&pending.trigger_id, &pending.delivery_id)
}

fn control_recorder(
    shared: &SharedLedger,
    run_id: &str,
) -> Result<Arc<DurableRunRecorder>, String> {
    DurableRunRecorder::attach(
        shared.run_ledger()?,
        run_id,
        "daemon-controller".into(),
        ClientIdentity {
            client_id: "monkey-daemon".into(),
            instance_id: format!("daemon-control-{}", std::process::id()),
            kind: ClientKind::Daemon,
            version: env!("CARGO_PKG_VERSION").into(),
        },
    )
}

fn now_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|_| "timestamp overflow".to_string())
        })
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::workflow_core::{workflow_core_fixtures, WorkflowRunStatus};

    #[test]
    fn daemon_and_authored_roots_must_come_from_the_same_profile_snapshot() {
        let first = Path::new("/tmp/little-monkey/profiles/first");
        let second = Path::new("/tmp/little-monkey/profiles/second");
        let paths = DaemonPaths::under(first);

        assert!(daemon_paths_match_profile(&paths, first));
        assert!(!daemon_paths_match_profile(&paths, second));
    }

    #[test]
    fn a_remote_request_is_lineage_and_is_never_left_claiming_to_run() {
        use little_monkey_lib::process_table::{
            ExitStatus, ProcessFilter, ProcessKind, ProcessState,
        };
        use little_monkey_lib::run_ledger::RunLedger;

        // The regression: `project_queue_origin` wrote this row as `running`,
        // and nothing anywhere ever closed it. The engine tick sweeps
        // `daemon_job` rows; the desktop reaper skips kinds it does not own.
        // Every remote enqueue therefore leaked a row asserting live work.
        let root =
            std::env::temp_dir().join(format!("monkey_remote_origin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();

        // A real durable run: `agent_processes.run_id` is a foreign key, so the
        // child's run link would be rejected against an invented id.
        let run_id = "run-remoteorigin";
        let ledger = RunLedger::open(&paths.ledger_db).unwrap();
        let (_recorder, _) = DurableRunRecorder::submit(
            ledger,
            &engine::tests::spec(run_id, 1_000),
            "remote-origin-fixture".into(),
        )
        .unwrap();
        let shared = SharedLedger::open(&paths.ledger_db).unwrap();

        project_queue_origin(
            &shared,
            &QueueOrigin::Remote {
                request_id: "req-remoteorigin".into(),
            },
            "job-remoteorigin",
            run_id,
            Some("/tmp/remote-origin-workspace"),
        );

        let table = shared.process_table();
        let request = table
            .find_by_external_id(ProcessKind::RemoteRun, "req-remoteorigin")
            .unwrap()
            .expect("a remote enqueue records the request");
        assert_eq!(
            request.state,
            ProcessState::Exited,
            "the request is over once it becomes a job; nothing supervises it"
        );
        assert_eq!(
            request.exit.as_ref().map(|exit| exit.status),
            Some(ExitStatus::Succeeded),
            "the request was accepted — the job carries whether the work succeeds"
        );

        // The lineage edge is the whole point of the row, and it has to resolve
        // against a parent that is already closed.
        let job = table
            .find_by_external_id(
                ProcessKind::DaemonJob,
                &engine::process_external_id("job-remoteorigin", 0),
            )
            .unwrap()
            .expect("the job is projected as the request's child");
        assert_eq!(
            job.parent_process_id.as_deref(),
            Some(request.process_id.as_str())
        );
        assert!(!job.state.is_terminal(), "the work has not started yet");

        let live = table
            .list(&ProcessFilter {
                kinds: vec![ProcessKind::RemoteRun],
                live_only: true,
                ..ProcessFilter::default()
            })
            .unwrap();
        assert!(
            live.is_empty(),
            "a remote enqueue left a row claiming to be live: {live:?}"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn user_run_key_is_hashed_into_job_id() {
        let args = DaemonRunArgs {
            name_or_path: "x".into(),
            param: vec![],
            run_key: Some("raw-secret-key".into()),
            priority: 0,
            max_attempts: 1,
            max_runtime_seconds: 60,
            max_memory_mb: None,
            owned_worktree: false,
            repository: None,
            branch_prefix: "codex/".into(),
            allowed_remotes: vec!["origin".into()],
            allow_commit: true,
            allow_push: false,
            allow_create_pull_request: false,
            allow_review_comment: false,
            json: false,
        };
        let options = QueueOptions::from_run_args(&args);
        let id = options.deterministic_job_id.unwrap();
        assert!(!id.contains("raw-secret-key"));
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn remote_mutations_are_never_implicit_without_owned_worktree() {
        let mut args = DaemonRunArgs {
            name_or_path: "x".into(),
            param: vec![],
            run_key: None,
            priority: 0,
            max_attempts: 1,
            max_runtime_seconds: 60,
            max_memory_mb: None,
            owned_worktree: false,
            repository: None,
            branch_prefix: "codex/".into(),
            allowed_remotes: vec!["origin".into()],
            allow_commit: true,
            allow_push: true,
            allow_create_pull_request: false,
            allow_review_comment: false,
            json: false,
        };
        let options = QueueOptions::from_run_args(&args);
        assert!(!options.owned_worktree && options.allow_push);
        args.owned_worktree = true;
        assert!(QueueOptions::from_run_args(&args).owned_worktree);
    }

    #[test]
    fn trigger_target_args_keep_recipe_and_workflow_namespaces_distinct() {
        let recipe_args = TriggerTargetArgs {
            recipe: Some("fixture".into()),
            workflow_id: None,
            definition_sha256: None,
            workflow_version: None,
            workflow_trigger_json: None,
        };
        let (target, binding) = build_trigger_target(
            &recipe_args,
            &["topic=value".into()],
            &Some("payload".into()),
        )
        .unwrap();
        assert!(matches!(target, TriggerTarget::Recipe { .. }));
        assert!(binding.is_none());

        let workflow_args = TriggerTargetArgs {
            recipe: None,
            workflow_id: Some("workflow.one".into()),
            definition_sha256: Some("a".repeat(64)),
            workflow_version: Some(3),
            workflow_trigger_json: Some(
                serde_json::json!({
                    "kind": "event_ingestion",
                    "topic": "github.events",
                    "consumer_id": "consumer.one"
                })
                .to_string(),
            ),
        };
        let (target, binding) = build_trigger_target(&workflow_args, &[], &None).unwrap();
        assert!(matches!(target, TriggerTarget::Workflow { .. }));
        assert!(matches!(
            binding.unwrap().trigger,
            WorkflowTrigger::EventIngestion { .. }
        ));
    }

    #[test]
    fn trigger_target_args_reject_ambiguous_or_payload_expanding_workflows() {
        let ambiguous = TriggerTargetArgs {
            recipe: Some("fixture".into()),
            workflow_id: Some("workflow.one".into()),
            definition_sha256: Some("a".repeat(64)),
            workflow_version: Some(1),
            workflow_trigger_json: Some(r#"{"kind":"manual"}"#.into()),
        };
        assert!(build_trigger_target(&ambiguous, &[], &None).is_err());

        let workflow = TriggerTargetArgs {
            recipe: None,
            workflow_id: Some("workflow.one".into()),
            definition_sha256: Some("a".repeat(64)),
            workflow_version: Some(1),
            workflow_trigger_json: Some(r#"{"kind":"manual"}"#.into()),
        };
        assert!(build_trigger_target(&workflow, &["x=y".into()], &None).is_err());
    }

    #[test]
    fn workflow_delivery_commits_m4_history_then_cross_ledger_markers() {
        let app_data = std::env::temp_dir().join(format!(
            "little-monkey-m4-daemon-delivery-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&app_data).unwrap();
        let services = little_monkey_lib::m4_runtime::production_m4_services(&app_data).unwrap();
        let mut definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap()
            .workflow;
        let declared_trigger = WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".into(),
        };
        definition.triggers = vec![declared_trigger.clone()];
        let ir = services.workflows.create(definition.clone()).unwrap();

        let paths = DaemonPaths::under(&app_data);
        paths.ensure().unwrap();
        let mut store = DaemonStore::open(&paths).unwrap();
        let mut shared = SharedLedger::open(&paths.ledger_db).unwrap();
        let trigger_id = "workflow-cron";
        let config = TriggerConfig::Cron {
            target: TriggerTarget::Workflow {
                workflow_id: definition.workflow_id.clone(),
                definition_sha256: ir.definition_sha256.clone(),
            },
            workflow: Some(WorkflowTriggerBinding {
                workflow_version: definition.workflow_version,
                managed_by_batch: true,
                trigger: declared_trigger.clone(),
            }),
            schedule: "*/5 * * * *".into(),
        };
        shared
            .upsert_trigger(
                trigger_id,
                config.kind_token(),
                &serde_json::to_vec(&config).unwrap(),
                10,
                None,
            )
            .unwrap();
        let payload = serde_json::json!({"kind":"cron","scheduled_at_ms":11}).to_string();
        store
            .reserve_delivery_payload(trigger_id, "delivery-one", None, &payload, 11)
            .unwrap();
        store
            .activate_delivery_payload(trigger_id, "delivery-one")
            .unwrap();
        shared
            .accept_delivery(
                trigger_id,
                "delivery-one",
                &sha256_hex(payload.as_bytes()),
                11,
            )
            .unwrap();
        let pending = store.pending_delivery_payloads(1).unwrap().remove(0);
        dispatch_workflow_delivery(
            &paths,
            &mut store,
            &mut shared,
            &pending,
            &definition.workflow_id,
            &ir.definition_sha256,
            &declared_trigger,
        )
        .unwrap();

        let run_id = format!(
            "m4-trigger-{}",
            &sha256_hex(format!("{trigger_id}:delivery-one").as_bytes())[..32]
        );
        let history = services.workflows.history(&run_id).unwrap();
        assert_eq!(history.status, WorkflowRunStatus::Succeeded);
        assert!(matches!(
            shared.delivery(trigger_id, "delivery-one").unwrap(),
            Some((ref status, _, None)) if status == "submitted"
        ));
        assert!(store.pending_delivery_payloads(1).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(app_data);
    }
    // --- Roadmap K17: converting a foreign spec into work here -------------

    /// The refusals the *conversion* owns, as opposed to the ones the wire or
    /// the placement rule own. Each names the fact this machine has not got.
    #[test]
    fn a_placement_this_node_cannot_execute_is_refused_with_the_reason() {
        use little_monkey_lib::run_protocol::{ModelTargetSnapshot, PermissionMode};

        // A managed placement is resolved against THIS node's hub inventory,
        // never against the spec's `model_path` — that path is a location on
        // the submitter's disk. An empty app-data root has the model installed
        // nowhere, so the refusal names the model rather than the path.
        let empty_app_data =
            std::env::temp_dir().join(format!("little-monkey-no-hub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&empty_app_data).unwrap();
        let managed = ModelTargetSnapshot::ManagedLlama {
            target_id: "t".into(),
            label: "l".into(),
            model_id: "qwen3-8b".into(),
            // Deliberately a path that exists nowhere: if this were ever
            // consulted the refusal below would not fire.
            model_path: "/models/qwen3-8b.gguf".into(),
            capabilities: crate::task::cli_capabilities(),
            estimated_memory_bytes: None,
        };
        let refusal = placed_recipe_target(&managed, &empty_app_data).unwrap_err();
        assert!(
            refusal.contains("qwen3-8b") && refusal.contains("no managed model"),
            "the refusal must name the model this node has not got: {refusal}"
        );
        let _ = std::fs::remove_dir_all(&empty_app_data);

        // Provider and Ollama both convert, and the provider *credential* is
        // never part of it: only the identity travels.
        let provider = ModelTargetSnapshot::Provider {
            target_id: "t".into(),
            label: "l".into(),
            provider_id: "anthropic".into(),
            endpoint: "https://api.anthropic.com".into(),
            model: "claude".into(),
            credential_ref_id: "credential:anthropic".into(),
            capabilities: crate::task::cli_capabilities(),
        };
        let converted = placed_recipe_target(&provider, &empty_app_data).unwrap();
        assert_eq!(converted.provider.as_deref(), Some("anthropic"));
        assert_eq!(converted.model.as_deref(), Some("claude"));
        converted
            .validate()
            .expect("the converted target must satisfy the recipe XOR rule");

        // The one mode a foreign spec may never buy on this machine.
        let refusal = placed_permission_mode(&PermissionMode::Bypass).unwrap_err();
        assert!(
            refusal.contains("bypass") && refusal.contains("shell"),
            "bypass must be refused with the reason: {refusal}"
        );
        for mode in [
            PermissionMode::Manual,
            PermissionMode::AcceptEdits,
            PermissionMode::Smart,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            let token = placed_permission_mode(&mode).unwrap();
            // Round-tripped through the executor's own parser rather than
            // through a copied list of mode strings, so a renamed mode fails
            // here instead of at spawn on the node.
            crate::permission::PermissionMode::parse(&token)
                .unwrap_or_else(|error| panic!("{mode:?} produced an unusable mode: {error}"));
        }
    }

    /// Two placements of the same spec resolve to the same job id, so a
    /// resubmission after a lost response cannot start a second run.
    #[test]
    fn a_placements_job_id_is_derived_from_the_submitted_run_id() {
        assert_eq!(placed_job_id("run:one"), placed_job_id("run:one"));
        assert_ne!(placed_job_id("run:one"), placed_job_id("run:two"));
        assert!(placed_job_id("run:one").starts_with("job-"));
    }
    /// **The node half of S3, end to end within this process.**
    ///
    /// A frozen spec becomes the exact recipe the node writes to disk, that file
    /// is parsed back the way the executing child parses it, and the four frozen
    /// fields are still the submitter's. The allowlist and the budgets are the
    /// ones that would go missing silently — a recipe has nowhere else to put
    /// them — so both are asserted on the far side of the file, not on the
    /// struct in memory.
    #[test]
    fn a_placed_spec_becomes_a_recipe_that_still_carries_the_submitters_policy() {
        let root =
            std::env::temp_dir().join(format!("little-monkey-placed-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();

        let mut spec = engine::tests::spec("run-placed", 1_000);
        spec.instructions = Some("the submitter's frozen system prompt".to_string());
        spec.budgets.wall_time_ms = 90_000;
        spec.budgets.max_output_tokens = 4_321;
        spec.permission_policy.egress_allowlist =
            Some(little_monkey_lib::run_protocol::EgressAllowlist {
                hosts: vec!["api.example.com".to_string()],
                ports: vec![443],
                protocols: vec!["https".to_string()],
            });
        if let Some(workspace) = spec.workspace.as_mut() {
            workspace.roots[0].canonical_path = root.to_string_lossy().to_string();
        }

        let recipe = placed_recipe(&spec).expect("a placeable spec must convert");
        let path = root.join("placed.json");
        write_snapshot(&path, &recipe).unwrap();
        let parsed = little_monkey_lib::recipes::parse_recipe(
            &std::fs::read_to_string(&path).unwrap(),
            "json",
        )
        .expect("the child parses the node's own snapshot");

        // The execution half became ordinary recipe fields...
        assert_eq!(parsed.permission_mode, "auto");
        assert_eq!(parsed.prompt, spec.task);
        assert_eq!(parsed.system.as_deref(), spec.instructions.as_deref());
        assert_eq!(parsed.timeout_seconds, Some(90));
        assert_eq!(
            parsed.workspace.as_deref(),
            Some(root.canonicalize().unwrap().to_string_lossy().as_ref())
        );
        // ...and the policy half rode along untouched.
        let placed = parsed.placed_run.expect("the frozen spec travelled");
        assert_eq!(placed.submitted_run_id, "run-placed");
        assert_eq!(
            placed
                .permission_policy
                .egress_allowlist
                .expect("the allowlist must reach the executing process")
                .hosts,
            vec!["api.example.com".to_string()]
        );
        assert_eq!(placed.budgets.max_output_tokens, 4_321);
        assert_eq!(placed.budgets.wall_time_ms, 90_000);

        let _ = std::fs::remove_dir_all(root);
    }

    /// A placed workspace root this machine has not got is refused rather than
    /// rehomed onto the daemon's working directory, which would run the
    /// submitter's task against the wrong files.
    #[test]
    fn a_placement_whose_workspace_root_is_absent_here_is_refused() {
        let mut spec = engine::tests::spec("run-absent", 1_000);
        if let Some(workspace) = spec.workspace.as_mut() {
            workspace.roots[0].canonical_path =
                "/definitely/not/here/little-monkey-k17".to_string();
        }
        let error = placed_recipe(&spec).unwrap_err();
        assert!(
            error.contains("cannot resolve the placed workspace root"),
            "the refusal must name the missing root: {error}"
        );
    }
}

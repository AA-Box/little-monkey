pub(crate) mod api;
mod client;
mod desktop;
mod protocol;
mod server;
mod store;
mod web;

pub use desktop::DesktopControlRuntime;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use reqwest::Method;

use crate::daemon::store::{restrict_file, DaemonPaths};

use self::protocol::{PairingInvitation, RemoteAction, RemoteScopes, REMOTE_PROTOCOL_VERSION};
use self::store::{KeyringRemoteSecrets, RemoteStore};

#[derive(Subcommand, Debug)]
pub enum RemoteCmd {
    /// Configure the runner's TLS listener. Certificate/key are copied into
    /// app-private state; no Little Monkey relay is involved.
    HostConfigure(RemoteHostConfigureArgs),
    /// Show the local remote-runner host identity and endpoint.
    HostStatus {
        #[arg(long)]
        json: bool,
    },
    /// Disable new remote connections without deleting devices or audit.
    HostDisable,
    /// Run only the TLS remote listener in the foreground (the installed
    /// daemon starts it automatically when configured).
    #[command(hide = true)]
    HostServe,
    /// Create a one-time, expiring, capability-scoped invitation.
    PairCreate(RemotePairCreateArgs),
    /// List paired devices and their frozen scopes; never prints secrets.
    PairList,
    /// Revoke a paired device immediately.
    PairRevoke {
        device_id: String,
        #[arg(long, default_value = "revoked by runner owner")]
        reason: String,
    },
    /// Rotate a device key immediately and write a protected transfer bundle.
    PairRotate {
        device_id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Accept a one-time invitation on a control PC/phone client profile.
    Accept {
        invitation: PathBuf,
        #[arg(long)]
        alias: String,
        #[arg(long)]
        device_name: String,
    },
    /// Import a runner-owner-issued key rotation without expanding scope.
    AcceptRotation {
        bundle: PathBuf,
        #[arg(long)]
        alias: String,
    },
    /// List the runs visible through a paired controller profile.
    Runs { alias: String },
    /// Show one scoped run without exposing provider credentials.
    Status { alias: String, run_id: String },
    /// Resume the durable event feed from its persisted replay cursor.
    Events {
        alias: String,
        run_id: String,
        #[arg(long)]
        after: Option<u64>,
    },
    /// List pending digest-bound approvals for a scoped run.
    Approvals { alias: String, run_id: String },
    /// Decide one digest-bound approval; cannot modify the run policy.
    Approve {
        alias: String,
        run_id: String,
        request_id: String,
        operation_sha256: String,
        decision: RemoteApprovalChoice,
    },
    /// Request cancellation. Connection loss never implies cancellation.
    Cancel {
        alias: String,
        run_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Suspend a run without ending it. Strictly weaker than `cancel`: the run
    /// keeps its place and can be resumed.
    Pause { alias: String, run_id: String },
    /// Undo a `pause`.
    Resume { alias: String, run_id: String },
    /// Fetch one run-linked artifact with size and SHA-256 verification.
    Artifact {
        alias: String,
        run_id: String,
        artifact_id: String,
        #[arg(long)]
        output: PathBuf,
    },
    /// Engage the runner's global kill switch (requires an explicit kill scope).
    Kill { alias: String },
    /// Inspect runner-local connection/control audit without sharing it remotely.
    Audit {
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
}

#[derive(Args, Debug)]
pub struct RemoteHostConfigureArgs {
    #[arg(long, default_value = "127.0.0.1:48321")]
    listen: String,
    /// Credential-free HTTPS origin reachable over the user's SSH/Tailscale/
    /// direct network. Its hostname must appear in the supplied certificate.
    #[arg(long)]
    advertise_url: String,
    #[arg(long)]
    tls_certificate: PathBuf,
    #[arg(long)]
    tls_private_key: PathBuf,
}

#[derive(Args, Debug)]
pub struct RemotePairCreateArgs {
    #[arg(long)]
    output: PathBuf,
    #[arg(long, default_value_t = 15)]
    expires_minutes: u64,
    #[arg(long = "action", value_enum, required = true)]
    actions: Vec<PairAction>,
    #[arg(long = "run")]
    run_ids: Vec<String>,
    #[arg(long = "workspace")]
    workspace_ids: Vec<String>,
    #[arg(long, default_value_t = protocol::MAX_REMOTE_ARTIFACT_BYTES)]
    max_artifact_bytes: u64,
    /// Additional first-party mobile-companion capabilities to grant on top
    /// of `--action`. Omit for a runner-only controller: the mobile chat,
    /// workflow-launch, and capture surfaces then stay unreachable for this
    /// device even if it runs a newer client build.
    #[arg(long = "mobile", value_enum)]
    mobile_capabilities: Vec<PairMobileCapability>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PairAction {
    ViewRuns,
    ViewEvents,
    ReadArtifacts,
    Approve,
    Cancel,
    Pause,
    Kill,
    ControlDesktop,
}

/// Mobile-only grants. Deliberately separate from [`PairAction`]: these do
/// not widen the underlying run scope, and a legacy pairing can never
/// acquire them implicitly (see `protocol::legacy_capabilities`).
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PairMobileCapability {
    ViewSessions,
    Chat,
    ViewTasks,
    RunWorkflows,
    Capture,
}

impl From<PairMobileCapability> for protocol::DeviceCapability {
    fn from(value: PairMobileCapability) -> Self {
        match value {
            PairMobileCapability::ViewSessions => Self::ViewSessions,
            PairMobileCapability::Chat => Self::Chat,
            PairMobileCapability::ViewTasks => Self::ViewTasks,
            PairMobileCapability::RunWorkflows => Self::RunWorkflows,
            PairMobileCapability::Capture => Self::Capture,
        }
    }
}

impl From<PairAction> for RemoteAction {
    fn from(value: PairAction) -> Self {
        match value {
            PairAction::ViewRuns => Self::ViewRuns,
            PairAction::ViewEvents => Self::ViewEvents,
            PairAction::ReadArtifacts => Self::ReadArtifacts,
            PairAction::Approve => Self::Approve,
            PairAction::Cancel => Self::Cancel,
            PairAction::Pause => Self::Pause,
            PairAction::Kill => Self::Kill,
            PairAction::ControlDesktop => Self::ControlDesktop,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RemoteApprovalChoice {
    AllowOnce,
    AllowForRun,
    Deny,
}

impl RemoteApprovalChoice {
    fn token(self) -> &'static str {
        match self {
            Self::AllowOnce => "allow_once",
            Self::AllowForRun => "allow_for_run",
            Self::Deny => "deny",
        }
    }
}

pub async fn run(command: &RemoteCmd) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    match command {
        RemoteCmd::HostConfigure(args) => {
            let config = server::configure_host(
                &paths,
                &args.listen,
                &args.advertise_url,
                &args.tls_certificate,
                &args.tls_private_key,
            )?;
            RemoteStore::open(&paths.root)?.audit(
                now_ms()?,
                None,
                "host_configure",
                Some(&config.listen),
                "allowed",
                None,
            )?;
            println!(
                "Remote runner configured at {} (certificate pin {}). Controller: {}. Restart the daemon to activate it.",
                config.advertise_url,
                config.certificate_sha256,
                controller_url(&config.advertise_url)
            );
        }
        RemoteCmd::HostStatus { json } => {
            let config = server::load_host_config(&paths)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?
                );
            } else if let Some(config) = config {
                println!(
                    "enabled={} runner={} listen={} url={} controller={} pin={}",
                    config.enabled,
                    config.runner_id,
                    config.listen,
                    config.advertise_url,
                    controller_url(&config.advertise_url),
                    config.certificate_sha256
                );
            } else {
                println!("Remote runner is not configured.");
            }
        }
        RemoteCmd::HostDisable => {
            let mut config = server::load_host_config(&paths)?
                .ok_or_else(|| "Remote host is not configured".to_string())?;
            config.enabled = false;
            server::save_host_config(&paths, &config)?;
            println!("Remote runner disabled; restart the daemon to close existing sockets.");
        }
        RemoteCmd::HostServe => {
            let desktop = DesktopControlRuntime::production(&paths);
            let mobile_chat = std::sync::Arc::new(
                crate::daemon::DaemonMobileChatQueue::new(paths.clone()),
            );
            server::serve(paths, desktop, mobile_chat).await?
        }
        RemoteCmd::PairCreate(args) => pair_create(&paths, args)?,
        RemoteCmd::PairList => pair_list(&paths)?,
        RemoteCmd::PairRevoke { device_id, reason } => {
            let mut store = RemoteStore::open(&paths.root)?;
            // The one-shot CLI process holds no live sessions; the resident
            // daemon force-stops any live session for this device on its next
            // enforcement tick (see `DesktopControlRuntime::enforce`).
            if !store.revoke_device(device_id, reason, now_ms()?, &KeyringRemoteSecrets, None)? {
                return Err(format!("Unknown or already revoked device '{device_id}'"));
            }
            println!("Revoked {device_id}; its current key is invalid immediately.");
        }
        RemoteCmd::PairRotate { device_id, output } => {
            let config = enabled_host(&paths)?;
            let certificate = std::fs::read_to_string(&config.certificate_path)
                .map_err(|error| format!("Could not read runner certificate: {error}"))?;
            let bundle = RemoteStore::open(&paths.root)?.rotate_device(
                device_id,
                &config.runner_id,
                &config.advertise_url,
                &certificate,
                &config.certificate_sha256,
                now_ms()?,
                &KeyringRemoteSecrets,
            )?;
            protected_json(output, &bundle)?;
            println!(
                "Rotated {device_id} immediately; transfer {} securely to that controller.",
                output.display()
            );
        }
        RemoteCmd::Accept {
            invitation,
            alias,
            device_name,
        } => {
            let profile =
                client::accept_invitation(&paths, invitation, alias, device_name, now_ms()?)
                    .await?;
            println!(
                "Paired '{}' with runner {} as device {}.",
                profile.alias, profile.runner_id, profile.device_id
            );
        }
        RemoteCmd::AcceptRotation { bundle, alias } => {
            let profile = client::accept_rotation(&paths, alias, bundle, now_ms()?)?;
            println!(
                "Controller '{}' now uses key generation {}.",
                alias, profile.secret_generation
            );
        }
        RemoteCmd::Runs { alias } => {
            print_json(
                client::call(
                    &paths,
                    alias,
                    Method::GET,
                    "/v1/remote/runs",
                    vec![],
                    now_ms()?,
                )
                .await?,
            )?;
        }
        RemoteCmd::Status { alias, run_id } => {
            print_json(
                client::call(
                    &paths,
                    alias,
                    Method::GET,
                    &format!("/v1/remote/runs/{}", segment(run_id)?),
                    vec![],
                    now_ms()?,
                )
                .await?,
            )?;
        }
        RemoteCmd::Events {
            alias,
            run_id,
            after,
        } => print_json(client::events(&paths, alias, run_id, *after, now_ms()?).await?)?,
        RemoteCmd::Approvals { alias, run_id } => {
            print_json(
                client::call(
                    &paths,
                    alias,
                    Method::GET,
                    &format!("/v1/remote/runs/{}/approvals", segment(run_id)?),
                    vec![],
                    now_ms()?,
                )
                .await?,
            )?;
        }
        RemoteCmd::Approve {
            alias,
            run_id,
            request_id,
            operation_sha256,
            decision,
        } => {
            protocol::validate_sha256(operation_sha256)?;
            print_json(
                client::call(
                    &paths,
                    alias,
                    Method::POST,
                    &format!("/v1/remote/runs/{}/approve", segment(run_id)?),
                    serde_json::to_vec(&serde_json::json!({
                        "request_id": request_id,
                        "operation_sha256": operation_sha256,
                        "decision": decision.token(),
                    }))
                    .map_err(|error| error.to_string())?,
                    now_ms()?,
                )
                .await?,
            )?;
        }
        RemoteCmd::Pause { alias, run_id } => print_json(
            client::call(
                &paths,
                alias,
                Method::POST,
                &format!("/v1/remote/runs/{}/pause", segment(run_id)?),
                b"{}".to_vec(),
                now_ms()?,
            )
            .await?,
        )?,
        RemoteCmd::Resume { alias, run_id } => print_json(
            client::call(
                &paths,
                alias,
                Method::POST,
                &format!("/v1/remote/runs/{}/resume", segment(run_id)?),
                b"{}".to_vec(),
                now_ms()?,
            )
            .await?,
        )?,
        RemoteCmd::Cancel {
            alias,
            run_id,
            reason,
        } => print_json(
            client::call(
                &paths,
                alias,
                Method::POST,
                &format!("/v1/remote/runs/{}/cancel", segment(run_id)?),
                serde_json::to_vec(&serde_json::json!({ "reason": reason }))
                    .map_err(|error| error.to_string())?,
                now_ms()?,
            )
            .await?,
        )?,
        RemoteCmd::Artifact {
            alias,
            run_id,
            artifact_id,
            output,
        } => {
            client::fetch_artifact(&paths, alias, run_id, artifact_id, output, now_ms()?).await?;
            println!("Verified remote artifact written to {}", output.display());
        }
        RemoteCmd::Kill { alias } => print_json(
            client::call(
                &paths,
                alias,
                Method::POST,
                "/v1/remote/kill",
                b"{}".to_vec(),
                now_ms()?,
            )
            .await?,
        )?,
        RemoteCmd::Audit { limit } => {
            print_json(
                serde_json::to_value(RemoteStore::open(&paths.root)?.audit_entries(*limit)?)
                    .map_err(|error| error.to_string())?,
            )?;
        }
    }
    Ok(())
}

pub async fn spawn_if_configured(
    paths: DaemonPaths,
    desktop: std::sync::Arc<DesktopControlRuntime>,
    mobile_chat: std::sync::Arc<dyn api::MobileChatQueue>,
) -> Result<bool, String> {
    server::spawn_if_configured(paths, desktop, mobile_chat).await
}

fn pair_create(paths: &DaemonPaths, args: &RemotePairCreateArgs) -> Result<(), String> {
    let config = enabled_host(paths)?;
    if !(1..=24 * 60).contains(&args.expires_minutes) {
        return Err("Pairing invitation expiry must be between 1 and 1440 minutes".to_string());
    }
    let scopes = RemoteScopes {
        actions: args.actions.iter().copied().map(Into::into).collect(),
        run_ids: args.run_ids.iter().cloned().collect::<BTreeSet<_>>(),
        workspace_ids: args.workspace_ids.iter().cloned().collect::<BTreeSet<_>>(),
        max_artifact_bytes: args.max_artifact_bytes,
    };
    scopes.validate()?;
    let now = now_ms()?;
    let expires_at_ms = now
        .checked_add(args.expires_minutes.saturating_mul(60_000))
        .ok_or_else(|| "Pairing expiry overflow".to_string())?;
    // Mobile grants are additive to the legacy action set, and
    // `validate_capabilities` enforces the dependencies (chat needs
    // view_sessions, workflow launch needs view_tasks) before anything is
    // written, so an invitation can never carry an unusable combination.
    let mut capabilities = protocol::legacy_capabilities(&scopes);
    capabilities.extend(
        args.mobile_capabilities
            .iter()
            .copied()
            .map(protocol::DeviceCapability::from),
    );
    protocol::validate_capabilities(&capabilities, &scopes)?;
    let invitation = RemoteStore::open(&paths.root)?.create_invitation_with_capabilities(
        &scopes,
        &capabilities,
        now,
        expires_at_ms,
    )?;
    let certificate = std::fs::read_to_string(&config.certificate_path)
        .map_err(|error| format!("Could not read remote certificate: {error}"))?;
    let value = PairingInvitation {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        runner_id: config.runner_id,
        runner_url: config.advertise_url,
        server_certificate_pem: certificate,
        server_certificate_sha256: config.certificate_sha256,
        pairing_id: invitation.pairing_id,
        pairing_token: invitation.token,
        expires_at_ms: invitation.expires_at_ms,
        scopes: invitation.scopes,
        capabilities: invitation.capabilities,
    };
    protected_json(&args.output, &value)?;
    println!(
        "One-time pairing invitation written to {} (expires at {}). Transfer it securely, open {}, and choose the file.",
        args.output.display(),
        value.expires_at_ms,
        controller_url(&value.runner_url)
    );
    Ok(())
}

fn controller_url(runner_url: &str) -> String {
    format!("{}/remote", runner_url.trim_end_matches('/'))
}

fn pair_list(paths: &DaemonPaths) -> Result<(), String> {
    let devices = RemoteStore::open(&paths.root)?.devices()?;
    if devices.is_empty() {
        println!("No paired remote devices.");
    }
    for device in devices {
        println!(
            "{} name={:?} generation={} state={} last_sequence={} scopes={}",
            device.device_id,
            device.device_name,
            device.secret_generation,
            if device.active() { "active" } else { "revoked" },
            device.last_sequence,
            serde_json::to_string(&device.scopes).map_err(|error| error.to_string())?
        );
    }
    Ok(())
}

fn enabled_host(paths: &DaemonPaths) -> Result<protocol::RemoteHostConfig, String> {
    let config = server::load_host_config(paths)?
        .ok_or_else(|| "Remote host is not configured".to_string())?;
    if !config.enabled {
        return Err("Remote host is disabled".to_string());
    }
    Ok(config)
}

fn protected_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Could not create output directory: {error}"))?;
    }
    if std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| !metadata.file_type().is_file() || metadata.file_type().is_symlink())
    {
        return Err(format!(
            "Refusing to replace unsafe path '{}'",
            path.display()
        ));
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Could not create protected bundle: {error}"))?;
    file.write_all(&serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?)
        .map_err(|error| format!("Could not write protected bundle: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Could not sync protected bundle: {error}"))?;
    restrict_file(&temporary)?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("Could not publish protected bundle: {error}"))?;
    restrict_file(path)
}

fn segment(value: &str) -> Result<String, String> {
    protocol::validate_id(value)?;
    Ok(url::form_urlencoded::byte_serialize(value.as_bytes()).collect())
}

fn print_json(value: serde_json::Value) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn now_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|_| "timestamp overflow".to_string())
        })
}

pub(crate) mod api;
mod client;
mod desktop;
pub(crate) mod device;
/// The device plane end to end, against the real store and the real signed
/// API. Tests only — see the module's own documentation.
#[cfg(test)]
mod device_e2e;
pub(crate) mod migrate;
pub(crate) mod protocol;
pub(crate) mod push;
pub(crate) mod qr;
mod server;
pub(crate) mod store;
pub(crate) mod talk;
pub(crate) mod talk_socket;
pub(crate) mod voice;
pub(crate) mod watch;
mod web;

/// The bound-listener entry point the opt-in peer live-validation test serves
/// through. Test-only so the module itself stays private.
#[cfg(test)]
pub(crate) use server::serve_listener_for_test;

pub use desktop::DesktopControlRuntime;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use reqwest::Method;

use crate::daemon::store::{restrict_file, DaemonPaths};

use self::protocol::{
    DeviceCapability, PairingBootstrap, PairingInvitation, RemoteAction, RemoteScopes,
    REMOTE_PROTOCOL_VERSION,
};
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
    /// List paired physical devices with what they were granted, what they
    /// advertise, what their operating system permits, and what is therefore
    /// effective.
    DeviceList {
        #[arg(long)]
        json: bool,
    },
    /// Replace one device's physical capability grant. Pass no `--capability`
    /// to withdraw every physical grant while leaving run access untouched.
    DeviceGrant {
        device_id: String,
        #[arg(long = "capability")]
        capabilities: Vec<String>,
    },
    /// Ask a paired device to do something once, and wait for the answer.
    DeviceAction(RemoteDeviceActionArgs),
    /// Check a real paired device end to end and exit non-zero if it is not
    /// working.
    ///
    /// Safe by default: it reads the device's own advertised surface and asks
    /// it to describe itself, which touches no sensor. `--dangerous` adds the
    /// physical actions — a photograph, a short recording, a location fix, a
    /// notification — and is opt-in precisely because each of those happens in
    /// a room somebody is in. Needs no credentials of any kind: it uses the
    /// pairing the operator already made.
    DeviceCheck {
        /// Which paired device. Omit when exactly one is paired.
        #[arg(long = "device-id")]
        device_id: Option<String>,
        /// Also perform the physical actions this device is granted.
        #[arg(long)]
        dangerous: bool,
        #[arg(long, default_value_t = 60_000)]
        wait_ms: u64,
        #[arg(long)]
        json: bool,
    },
    /// Recent commands queued for one device.
    DeviceCommands {
        device_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Ask for a queued or running command to be cancelled.
    DeviceCancel { command_id: String },
    /// Open a live microphone stream on a paired device.
    ///
    /// Returns as soon as the stream is queued; the audio arrives while it
    /// runs. `voice-stop` ends it early, and the runner closes it on its own
    /// deadline whatever the device does.
    VoiceStart {
        #[arg(long)]
        device_id: Option<String>,
        /// How long to listen for, in milliseconds.
        #[arg(long, default_value_t = voice::DEFAULT_STREAM_MS)]
        duration_ms: u64,
    },
    /// Stop a live stream. The microphone closes on the device's next chunk.
    VoiceStop { session_id: String },
    /// Recent voice streams and how much audio each one holds.
    VoiceList {
        #[arg(long)]
        device_id: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Copy one stream's audio out of app-private state.
    VoiceSave {
        session_id: String,
        #[arg(long, short)]
        output: PathBuf,
    },
    /// Turn on push. Little Monkey ships no push project, no key and no relay:
    /// this is your configuration, stored in app-private state on this machine.
    ///
    /// `--web-push` needs no account anywhere and is what the bundled browser
    /// controller uses. The Firebase options are for a native client that holds
    /// its own registration token.
    PushConfigure {
        /// Mint this runner's own VAPID identity and send Web Push directly to
        /// each browser's push service.
        #[arg(long = "web-push", conflicts_with = "project_id")]
        web_push: bool,
        /// How a push service could contact whoever is sending: this runner's
        /// own advertised HTTPS URL, or a `mailto:` address.
        #[arg(long = "vapid-subject")]
        vapid_subject: Option<String>,
        /// Your Firebase project id.
        #[arg(long = "project-id", required_unless_present = "web_push")]
        project_id: Option<String>,
        /// Your service account JSON key, which is copied into app-private
        /// state rather than read from wherever it was downloaded.
        #[arg(long = "service-account", required_unless_present = "web_push")]
        service_account: Option<PathBuf>,
        /// Allow notifications to carry specifics. Off by default: the visible
        /// text of a push is the least private thing this system produces.
        #[arg(long = "include-detail")]
        include_detail: bool,
    },
    /// Show whether push is configured, and which devices are reachable.
    PushStatus {
        #[arg(long)]
        json: bool,
    },
    /// Stop sending push without forgetting the configuration.
    PushDisable,
    /// Send one harmless test notification to a registered device.
    PushTest { device_id: String },
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
    /// State what this machine is, for the schedulers allowed to place work on
    /// it (roadmap K17 S1). Both values are operator statements: nothing can
    /// infer which jurisdiction a machine's disks are in, and an unset label
    /// never satisfies a residency rule naming a real zone.
    NodeLabel {
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        residency: Option<String>,
    },
    /// Ask a paired node to describe itself and remember the answer.
    NodeRefresh {
        /// One alias, or every paired controller when omitted.
        alias: Option<String>,
    },
    /// Show the nodes this machine may place work on, and how long each has
    /// been silent.
    NodeList {
        #[arg(long)]
        json: bool,
    },
    /// Place a frozen `RunSpec` on an owned node (roadmap K17 S2/S5).
    ///
    /// With `--alias` the node is named; without it, the node is chosen by
    /// capability, the node's own admission verdict, and the data-residency
    /// rule — see `node_placement::select_node`, including why measured
    /// throughput is deliberately not an input.
    Place(RemotePlaceArgs),
    /// List the runs this machine has placed on nodes.
    Placements {
        #[arg(long)]
        json: bool,
    },
    /// Re-probe every node and reconcile the placements on it: refresh live
    /// states, and apply the restart policy to placements whose node vanished
    /// (roadmap K17 S4).
    PlacementSync,
    /// Move a *frozen process image* to an owned node and resume it there
    /// (roadmap K18).
    ///
    /// `place` submits a spec the node then runs from the start. This hands the
    /// node a turn already in flight — its conversation, its workspace and its
    /// checkpoint — and refuses before transferring anything when the target
    /// cannot satisfy it. The move is recorded on both nodes' run-event chains
    /// as one chain. Use `node-refresh` first to see what a target is.
    Migrate {
        alias: String,
        /// The frozen checkpoint to move. `monkey ps` names the process; the
        /// checkpoint is the image that process froze into.
        #[arg(long)]
        checkpoint: String,
        /// Require the target's data-residency label to be exactly this. The
        /// node checks it against its own rather than trusting it.
        #[arg(long)]
        residency: Option<String>,
        /// Check the target and stop, transferring nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Args, Debug)]
pub struct RemotePlaceArgs {
    /// A frozen `RunSpec` as JSON. Frozen on purpose: the placer authors the
    /// spec, and everything the node enforces travels inside it.
    #[arg(long)]
    spec: PathBuf,
    /// Place on this exact node instead of selecting one.
    #[arg(long)]
    alias: Option<String>,
    /// The data-residency rule. Only a node whose operator set this exact label
    /// is eligible, and the node re-checks the claim on arrival.
    #[arg(long)]
    residency: Option<String>,
    /// A backend the node must actually execute on — not merely detect.
    #[arg(long = "require-accelerator")]
    required_accelerator: Option<String>,
    /// Free system memory the node must report.
    #[arg(long, default_value_t = 0)]
    min_available_ram_bytes: u64,
    #[arg(long)]
    json: bool,
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
    /// `--workspace-id`, not `--workspace`: the CLI's global `--workspace` is a
    /// filesystem path and is declared `global = true`, so it reaches every
    /// subcommand — two arguments claiming that name made `pair-create` abort
    /// on clap's own uniqueness assert before it could parse anything at all.
    #[arg(long = "workspace-id")]
    workspace_ids: Vec<String>,
    #[arg(long, default_value_t = protocol::MAX_REMOTE_ARTIFACT_BYTES)]
    max_artifact_bytes: u64,
    /// Additional first-party mobile-companion capabilities to grant on top
    /// of `--action`. Omit for a runner-only controller: the mobile chat,
    /// workflow-launch, and capture surfaces then stay unreachable for this
    /// device even if it runs a newer client build.
    #[arg(long = "mobile", value_enum)]
    mobile_capabilities: Vec<PairMobileCapability>,
    /// Physical capabilities to grant this device over its own hardware
    /// (camera-capture, location-read, …). Omit for a device that is only a
    /// controller: the runner then has no way to ask its hardware for anything,
    /// whatever the device advertises.
    #[arg(long = "device")]
    device_capabilities: Vec<String>,
    /// Also print a compact pairing code the phone can scan, instead of only
    /// writing the invitation file.
    #[arg(long)]
    qr: bool,
    /// Print the result as JSON — the invitation path, the compact bootstrap
    /// URI, and (with `--qr`) the code as an SVG. This is what the desktop's
    /// pairing panel reads; a terminal wants the human form above it.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
pub struct RemoteDeviceActionArgs {
    /// device_info, camera_capture, microphone_capture, location_read,
    /// notification_post, screen_capture or audio_playback.
    pub action: String,
    /// Which paired device. Omit when exactly one device can do this.
    #[arg(long = "device-id")]
    pub device_id: Option<String>,
    #[arg(long)]
    pub position: Option<String>,
    #[arg(long = "duration-ms")]
    pub duration_ms: Option<u64>,
    #[arg(long)]
    pub accuracy: Option<String>,
    #[arg(long)]
    pub title: Option<String>,
    #[arg(long)]
    pub body: Option<String>,
    /// Text for audio_playback to speak. Use this or --run-id/--artifact-id.
    #[arg(long)]
    pub text: Option<String>,
    /// The run an audio artifact belongs to, for audio_playback.
    #[arg(long = "run-id")]
    pub run_id: Option<String>,
    /// Which audio artifact of that run to play.
    #[arg(long = "artifact-id")]
    pub artifact_id: Option<String>,
    #[arg(long = "wait-ms", default_value_t = 60_000)]
    pub wait_ms: u64,
    /// The durable invocation asking for this, when a caller has one.
    ///
    /// Two deliveries of the same invocation produce one command and therefore
    /// one physical effect. Omitted — an operator running this by hand — every
    /// invocation is its own command, because two deliberate asks are two asks.
    #[arg(long = "invocation-id")]
    pub invocation_id: Option<String>,
    #[arg(long)]
    pub json: bool,
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
    /// Roadmap K17 S1 — read this node's identity and inventory.
    DescribeNode,
    /// Roadmap K17 S2 — submit a frozen `RunSpec` to this node. Requires
    /// `--mobile describe-node` and `--action view-runs`; `validate_capabilities`
    /// refuses the invitation otherwise rather than silently widening either.
    PlaceRuns,
    /// Hand this node a frozen process image (roadmap K18). Strictly more than
    /// `place-runs`, which it also requires: a migration writes a workspace, a
    /// checkpoint and a conversation onto the target.
    Migrate,
}

impl From<PairMobileCapability> for protocol::DeviceCapability {
    fn from(value: PairMobileCapability) -> Self {
        match value {
            PairMobileCapability::ViewSessions => Self::ViewSessions,
            PairMobileCapability::Chat => Self::Chat,
            PairMobileCapability::ViewTasks => Self::ViewTasks,
            PairMobileCapability::RunWorkflows => Self::RunWorkflows,
            PairMobileCapability::Capture => Self::Capture,
            PairMobileCapability::DescribeNode => Self::DescribeNode,
            PairMobileCapability::PlaceRuns => Self::PlaceRuns,
            PairMobileCapability::Migrate => Self::Migrate,
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
            let mobile_chat =
                std::sync::Arc::new(crate::daemon::DaemonMobileChatQueue::new(paths.clone()));
            let placement =
                std::sync::Arc::new(crate::daemon::DaemonPlacementQueue::new(paths.clone()));
            let peer_runs =
                std::sync::Arc::new(crate::daemon::DaemonChannelQueue::new(paths.clone()));
            server::serve(paths, desktop, mobile_chat, placement, peer_runs).await?
        }
        RemoteCmd::PairCreate(args) => pair_create(&paths, args)?,
        RemoteCmd::PairList => pair_list(&paths)?,
        RemoteCmd::DeviceList { json } => device_list(&paths, *json)?,
        RemoteCmd::DeviceGrant {
            device_id,
            capabilities,
        } => device_grant(&paths, device_id, capabilities)?,
        RemoteCmd::DeviceAction(args) => device_action(&paths, args).await?,
        RemoteCmd::DeviceCheck {
            device_id,
            dangerous,
            wait_ms,
            json,
        } => device_check(&paths, device_id.as_deref(), *dangerous, *wait_ms, *json).await?,
        RemoteCmd::DeviceCommands {
            device_id,
            limit,
            json,
        } => device_commands(&paths, device_id, *limit, *json)?,
        RemoteCmd::PushConfigure {
            web_push,
            vapid_subject,
            project_id,
            service_account,
            include_detail,
        } => {
            let config = if *web_push {
                // Defaults to this runner's own advertised URL: a self-hosted
                // runner has no support address, and a push service only needs
                // somewhere to point if it has to complain about the sender.
                let subject = match vapid_subject {
                    Some(subject) => subject.clone(),
                    None => enabled_host(&paths)
                        .map(|config| config.advertise_url)
                        .unwrap_or_else(|_| "https://localhost".to_string()),
                };
                push::configure_web_push(&paths, &subject, *include_detail, &KeyringRemoteSecrets)?
            } else {
                push::configure(
                    &paths,
                    project_id.as_deref().unwrap_or_default(),
                    service_account.as_deref().unwrap_or(Path::new("")),
                    *include_detail,
                )?
            };
            match config.backend.as_str() {
                "web_push" => println!(
                    "Web Push is on. This runner minted its own VAPID key (kept in the system keychain) — no account anywhere, and each notification is encrypted to the device before your browser's push service carries it."
                ),
                _ => println!(
                    "Push will use your Firebase project '{}' with the key copied into app-private state.",
                    config.project_id
                ),
            }
            println!(
                "Notification detail is {}.",
                if config.include_detail {
                    "included"
                } else {
                    "withheld — a push says what kind of thing happened, not what it said"
                }
            );
        }
        RemoteCmd::PushStatus { json } => push_status(&paths, *json)?,
        RemoteCmd::PushDisable => {
            let mut config = push::load_config(&paths)?
                .ok_or_else(|| "Push is not configured on this machine".to_string())?;
            config.enabled = false;
            push::save_config(&paths, &config)?;
            println!("Push is disabled. Registrations and configuration are kept.");
        }
        RemoteCmd::PushTest { device_id } => {
            let delivered = push::notify_device(
                &paths,
                device_id,
                &push::PushNotification {
                    kind: push::PushKind::SecurityAlert,
                    target_id: Some(device_id.clone()),
                    detail: Some("Test notification from your own runner".to_string()),
                },
                &KeyringRemoteSecrets,
            )
            .await?;
            println!(
                "{}",
                if delivered {
                    "Delivered to your provider. If nothing arrives, the device has not granted notifications."
                } else {
                    "Nothing was sent: push is not configured here, or that device has not registered."
                }
            );
        }
        RemoteCmd::VoiceStart {
            device_id,
            duration_ms,
        } => {
            let record = voice::start(
                &paths,
                device_id.as_deref(),
                *duration_ms,
                None,
                None,
                now_ms()?,
            )
            .await?;
            println!(
                "Listening on {} as {} (command {}). The microphone opens when the device takes \
                 the command; stop it early with `monkey-cli daemon remote voice-stop {}`.",
                record.device_id, record.session_id, record.command_id, record.session_id
            );
        }
        RemoteCmd::VoiceStop { session_id } => {
            let record = voice::stop(&paths, session_id, now_ms()?)?;
            println!(
                "{} is {} with {} chunks ({} bytes).{}",
                record.session_id,
                record.state.as_str(),
                record.next_sequence,
                record.bytes,
                if record.state == protocol::VoiceSessionState::Open {
                    " The device is still holding the microphone; it closes on its next chunk."
                } else {
                    ""
                }
            );
        }
        RemoteCmd::VoiceList {
            device_id,
            limit,
            json,
        } => {
            let sessions = voice::sessions(&paths, device_id.as_deref(), *limit)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &sessions.iter().map(voice::session_json).collect::<Vec<_>>()
                    )
                    .map_err(|error| error.to_string())?
                );
            } else if sessions.is_empty() {
                println!("No voice streams have been opened on this machine.");
            } else {
                for record in &sessions {
                    println!(
                        "{}  {:<7}  {} chunks  {} bytes  {}",
                        record.session_id,
                        record.state.as_str(),
                        record.next_sequence,
                        record.bytes,
                        record.device_id
                    );
                }
            }
        }
        RemoteCmd::VoiceSave { session_id, output } => {
            let record = voice::session(&paths, session_id)?
                .ok_or_else(|| format!("Unknown voice session '{session_id}'"))?;
            let source = voice::audio_path(&paths.root, &record.session_id);
            if !source.exists() {
                return Err(format!("{session_id} has no audio yet"));
            }
            std::fs::copy(&source, output)
                .map_err(|error| format!("Could not write {}: {error}", output.display()))?;
            println!(
                "Wrote {} bytes of {} to {}.",
                record.bytes,
                record.media_type.as_deref().unwrap_or("audio"),
                output.display()
            );
        }
        RemoteCmd::DeviceCancel { command_id } => {
            let record =
                RemoteStore::open(&paths.root)?.request_device_cancel(command_id, now_ms()?)?;
            println!(
                "{} is now {}{}",
                record.command_id,
                record.state.as_str(),
                if record.cancel_requested && !record.state.terminal() {
                    " (cancellation requested; the device had already started, so any effect it \
                      already had stands)"
                } else {
                    ""
                }
            );
        }
        RemoteCmd::PairRevoke { device_id, reason } => {
            // Told to the *other* devices, before the revocation lands: a
            // revoked device is excluded from the push join by design, and the
            // people who need to know a device was cut off are the ones still
            // holding the others.
            let _ = push::notify_all(
                &paths,
                &push::PushNotification {
                    kind: push::PushKind::SecurityAlert,
                    target_id: Some(device_id.clone()),
                    detail: Some(format!("{device_id} was revoked on this runner")),
                },
                &KeyringRemoteSecrets,
            )
            .await;
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
        RemoteCmd::Migrate {
            alias,
            checkpoint,
            residency,
            dry_run,
        } => migrate_run(&paths, alias, checkpoint, residency.as_deref(), *dry_run).await?,
        RemoteCmd::Audit { limit } => {
            print_json(
                serde_json::to_value(RemoteStore::open(&paths.root)?.audit_entries(*limit)?)
                    .map_err(|error| error.to_string())?,
            )?;
        }
        RemoteCmd::NodeLabel { name, residency } => {
            node_label(&paths, name.as_deref(), residency.as_deref())?
        }
        RemoteCmd::NodeRefresh { alias } => node_refresh(&paths, alias.as_deref()).await?,
        RemoteCmd::NodeList { json } => node_list(&paths, *json)?,
        RemoteCmd::Place(args) => place(&paths, args).await?,
        RemoteCmd::Placements { json } => placements(&paths, *json)?,
        RemoteCmd::PlacementSync => placement_sync(&paths).await?,
    }
    Ok(())
}

// --- Roadmap K17 CLI ------------------------------------------------------

fn node_label(
    paths: &DaemonPaths,
    name: Option<&str>,
    residency: Option<&str>,
) -> Result<(), String> {
    let mut store = crate::daemon::store::DaemonStore::open(paths)?;
    if let Some(residency) = residency {
        little_monkey_lib::node_placement::validate_residency(residency)?;
        store.set_meta(api::NODE_RESIDENCY_META, residency)?;
    }
    if let Some(name) = name {
        if name.trim().is_empty() || name.len() > 128 {
            return Err("Node name must be 1-128 characters".to_string());
        }
        store.set_meta(api::NODE_NAME_META, name)?;
    }
    println!(
        "This node advertises name={:?} residency={:?}",
        store
            .get_meta(api::NODE_NAME_META)?
            .unwrap_or_else(|| "(runner id)".to_string()),
        store
            .get_meta(api::NODE_RESIDENCY_META)?
            .unwrap_or_else(|| {
                little_monkey_lib::node_placement::RESIDENCY_UNSPECIFIED.to_string()
            })
    );
    Ok(())
}

/// Every paired controller alias, which is the set of nodes this machine could
/// place work on. A controller without the `describe_node` grant simply answers
/// 403 and is reported as such rather than being filtered out silently — an
/// operator who granted the wrong capability needs to see that.
fn aliases(paths: &DaemonPaths, only: Option<&str>) -> Result<Vec<String>, String> {
    if let Some(alias) = only {
        return Ok(vec![alias.to_string()]);
    }
    RemoteStore::open(&paths.root)?.controller_aliases()
}

async fn node_refresh(paths: &DaemonPaths, alias: Option<&str>) -> Result<(), String> {
    let aliases = aliases(paths, alias)?;
    if aliases.is_empty() {
        println!("No paired controllers; nothing to describe.");
        return Ok(());
    }
    for alias in aliases {
        match client::refresh_node(paths, &alias, now_ms()?).await {
            Ok(descriptor) => println!(
                "{alias}: {} residency={} accepting={} queue={}/{} models={} backends={}",
                descriptor.node_name,
                descriptor.residency,
                descriptor.accepting,
                descriptor.queue_depth,
                descriptor.queue_capacity,
                descriptor.resident_models.len(),
                descriptor
                    .accelerators
                    .iter()
                    .filter(|entry| entry.executes && entry.available)
                    .map(|entry| entry.kind.as_str())
                    .collect::<Vec<_>>()
                    .join("+")
            ),
            Err(error) => println!("{alias}: unavailable ({error})"),
        }
    }
    Ok(())
}

fn node_list(paths: &DaemonPaths, json: bool) -> Result<(), String> {
    let now = now_ms()?;
    let nodes = RemoteStore::open(&paths.root)?.nodes()?;
    if json {
        let rows = nodes
            .iter()
            .map(|(alias, descriptor, last_seen)| {
                serde_json::json!({
                    "alias": alias,
                    "runner_id": descriptor.runner_id,
                    "node_name": descriptor.node_name,
                    "residency": descriptor.residency,
                    "accepting": descriptor.accepting,
                    "queue_depth": descriptor.queue_depth,
                    "queue_capacity": descriptor.queue_capacity,
                    "last_seen_at_ms": last_seen,
                    "liveness": liveness_token(*last_seen, now),
                })
            })
            .collect::<Vec<_>>();
        return print_json(serde_json::json!({ "nodes": rows }));
    }
    if nodes.is_empty() {
        println!("No described nodes. Run `monkey daemon remote node-refresh` first.");
    }
    for (alias, descriptor, last_seen) in nodes {
        println!(
            "{alias} runner={} residency={} accepting={} queue={}/{} liveness={}",
            descriptor.runner_id,
            descriptor.residency,
            descriptor.accepting,
            descriptor.queue_depth,
            descriptor.queue_capacity,
            liveness_token(last_seen, now)
        );
    }
    Ok(())
}

fn liveness_token(last_seen: Option<u64>, now_ms: u64) -> &'static str {
    match little_monkey_lib::node_placement::liveness(last_seen, now_ms) {
        little_monkey_lib::node_placement::NodeLiveness::Alive => "alive",
        little_monkey_lib::node_placement::NodeLiveness::Stale { .. } => "stale",
        little_monkey_lib::node_placement::NodeLiveness::Vanished { .. } => "vanished",
    }
}

/// The model id a spec's frozen target names, when it names a local one.
///
/// Only used as a placement *preference*, never a requirement — see
/// `select_node`. A provider target has no local model, so it contributes
/// nothing here, which is correct: the weights are on someone else's machine
/// either way.
fn preferred_model_id(
    target: &little_monkey_lib::run_protocol::ModelTargetSnapshot,
) -> Option<String> {
    use little_monkey_lib::run_protocol::ModelTargetSnapshot;
    match target {
        ModelTargetSnapshot::Provider { .. } => None,
        ModelTargetSnapshot::Ollama {
            model,
            is_cloud: false,
            ..
        } => Some(model.clone()),
        ModelTargetSnapshot::Ollama { .. } => None,
        ModelTargetSnapshot::ManagedLlama { model_id, .. } => Some(model_id.clone()),
    }
}

async fn place(paths: &DaemonPaths, args: &RemotePlaceArgs) -> Result<(), String> {
    let spec: little_monkey_lib::run_protocol::RunSpec = serde_json::from_slice(
        &std::fs::read(&args.spec)
            .map_err(|error| format!("Could not read the run spec: {error}"))?,
    )
    .map_err(|error| format!("Run spec is invalid: {error}"))?;
    spec.validate().map_err(|error| error.to_string())?;
    if let Some(residency) = &args.residency {
        little_monkey_lib::node_placement::validate_residency(residency)?;
    }

    let now = now_ms()?;
    let requirement = little_monkey_lib::node_placement::PlacementRequirement {
        residency: args.residency.clone(),
        model_id: preferred_model_id(&spec.target),
        required_accelerator: args.required_accelerator.clone(),
        min_available_ram_bytes: args.min_available_ram_bytes,
    };

    let store = RemoteStore::open(&paths.root)?;
    let described = store.nodes()?;
    drop(store);
    let candidates: Vec<little_monkey_lib::node_placement::NodeCandidate> = described
        .iter()
        .filter(|(alias, _, _)| args.alias.as_ref().is_none_or(|only| only == alias))
        .map(|(alias, descriptor, last_seen)| descriptor.candidate(alias, *last_seen))
        .collect();

    // An explicit `--alias` still goes through `select_node`, which is the
    // point: naming the node chooses *which* node, it does not waive the
    // residency rule, the liveness check, or the node's own refusal to accept
    // work. A named node that fails one of those is refused here with the same
    // sentence an unnamed one would get.
    let chosen = little_monkey_lib::node_placement::select_node(&candidates, &requirement, now)
        .map_err(|refusal| refusal.message())?;
    let runner_up = candidates
        .iter()
        .find(|candidate| candidate.alias != chosen.alias);
    let deciding = little_monkey_lib::node_placement::deciding_key(chosen, runner_up, &requirement);

    let request = little_monkey_lib::node_placement::PlaceRunRequest {
        protocol_version: little_monkey_lib::node_placement::NODE_PROTOCOL_VERSION,
        spec: spec.clone(),
        required_residency: args.residency.clone(),
        expected_runner_id: Some(chosen.runner_id.clone()),
    };
    let response = client::place_run(paths, &chosen.alias, &request, now).await?;

    let mut store = RemoteStore::open(&paths.root)?;
    store.save_placement(&store::PlacementRecord {
        submitted_run_id: response.submitted_run_id.clone(),
        alias: chosen.alias.clone(),
        runner_id: chosen.runner_id.clone(),
        node_run_id: response.node_run_id.clone(),
        job_id: response.job_id.clone(),
        state: little_monkey_lib::node_placement::PlacementState::Accepted
            .token()
            .to_string(),
        attempt: 1,
        residency: response.residency.clone(),
        deciding_key: deciding.to_string(),
        last_error: None,
        created_at_ms: now,
        updated_at_ms: now,
    })?;

    if args.json {
        return print_json(serde_json::json!({
            "alias": chosen.alias,
            "deciding_key": deciding,
            "placement": serde_json::to_value(&response).map_err(|error| error.to_string())?,
        }));
    }
    println!(
        "Placed {} on '{}' (chosen by {deciding}) as node run {} / job {} under residency '{}'.",
        response.submitted_run_id,
        chosen.alias,
        response.node_run_id,
        response.job_id,
        response.residency
    );
    Ok(())
}

fn placements(paths: &DaemonPaths, json: bool) -> Result<(), String> {
    let records = RemoteStore::open(&paths.root)?.placements()?;
    if json {
        let rows = records
            .iter()
            .map(|record| {
                serde_json::json!({
                    "submitted_run_id": record.submitted_run_id,
                    "alias": record.alias,
                    "node_run_id": record.node_run_id,
                    "job_id": record.job_id,
                    "state": record.state,
                    "attempt": record.attempt,
                    "residency": record.residency,
                    "deciding_key": record.deciding_key,
                    "last_error": record.last_error,
                    "updated_at_ms": record.updated_at_ms,
                })
            })
            .collect::<Vec<_>>();
        return print_json(serde_json::json!({ "placements": rows }));
    }
    if records.is_empty() {
        println!("No placed runs.");
    }
    for record in records {
        println!(
            "{} on {} state={} attempt={} residency={} chosen_by={}{}",
            record.submitted_run_id,
            record.alias,
            record.state,
            record.attempt,
            record.residency,
            record.deciding_key,
            record
                .last_error
                .map(|error| format!(" error={error}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// **Roadmap K17 S4, as a command.** Probe every node, refresh what it says
/// about the placements it holds, and apply the restart policy to the ones whose
/// node has gone away.
///
/// Re-placement is deliberately *not* automatic here: it marks the placement
/// `lost` and says what to run. A vanished node's run may have completed its
/// external side effects before the network went, and this machine cannot know —
/// so re-placing without a human is exactly the "confirmed mutations are not
/// replayed" rule the daemon's own `reconcile_interrupted` already holds to.
/// Terminates device work whose deadline has passed, including the capture rows
/// an open Talk socket registers.
///
/// **Why this needs its own tick.** Both expiry sweeps used to run in exactly
/// one place — the device long-poll — so nothing swept until some device asked
/// for work. A runner that died mid-conversation therefore left a `running`
/// `voice_stream` row behind, and the security audit went on reporting a
/// microphone in flight for a socket that no longer existed. A stale "something
/// is listening" is worse than useless: it is the alarm nobody believes.
pub(crate) fn expire_device_work(paths: &DaemonPaths) -> Result<u64, String> {
    let now = now_ms()?;
    let mut store = RemoteStore::open(&paths.root)?;
    let expired = store.expire_device_commands(now)?;
    voice::expire(&mut store, now)?;
    Ok(expired)
}

pub(crate) async fn placement_sync(paths: &DaemonPaths) -> Result<(), String> {
    let now = now_ms()?;
    for alias in aliases(paths, None)? {
        if let Err(error) = client::probe_node(paths, &alias, now).await {
            eprintln!("monkey remote: node '{alias}' did not answer: {error}");
        }
    }
    let store = RemoteStore::open(&paths.root)?;
    let nodes = store.nodes()?;
    let records = store.placements()?;
    drop(store);
    for record in records {
        let Some(state) = little_monkey_lib::node_placement::PlacementState::parse(&record.state)
        else {
            continue;
        };
        if state.terminal() {
            continue;
        }
        let last_seen = nodes
            .iter()
            .find(|(alias, _, _)| alias == &record.alias)
            .and_then(|(_, _, last_seen)| *last_seen);
        // The node is answering: ask it what became of this placement, which is
        // the only authority on it. The node's own denial record is what the S3
        // acceptance is read from — never this side's prediction of it.
        if matches!(
            little_monkey_lib::node_placement::liveness(last_seen, now),
            little_monkey_lib::node_placement::NodeLiveness::Alive
        ) {
            match client::placed_status(paths, &record.alias, &record.submitted_run_id, now).await {
                Ok(status) => {
                    let mapped = map_node_state(&status.state);
                    RemoteStore::open(&paths.root)?.set_placement_state(
                        &record.submitted_run_id,
                        mapped.token(),
                        status.last_error.as_deref(),
                        now,
                    )?;
                }
                Err(error) => {
                    eprintln!(
                        "monkey remote: could not read placement {} on '{}': {error}",
                        record.submitted_run_id, record.alias
                    );
                }
            }
            continue;
        }
        match little_monkey_lib::node_placement::reconcile_placement(
            state,
            last_seen,
            record.attempt,
            little_monkey_lib::node_placement::PLACEMENT_MAX_ATTEMPTS,
            now,
        ) {
            little_monkey_lib::node_placement::PlacementReconcile::Keep => {}
            little_monkey_lib::node_placement::PlacementReconcile::Degraded { silent_ms } => {
                RemoteStore::open(&paths.root)?.set_placement_state(
                    &record.submitted_run_id,
                    state.token(),
                    Some(&format!(
                        "node '{}' silent for {silent_ms} ms",
                        record.alias
                    )),
                    now,
                )?;
            }
            little_monkey_lib::node_placement::PlacementReconcile::Replace { reason, .. } => {
                RemoteStore::open(&paths.root)?.set_placement_state(
                    &record.submitted_run_id,
                    little_monkey_lib::node_placement::PlacementState::Lost.token(),
                    Some(&format!(
                        "{reason}; re-place it with `monkey daemon remote place`"
                    )),
                    now,
                )?;
                println!("Placement {} is lost: {reason}", record.submitted_run_id);
            }
            little_monkey_lib::node_placement::PlacementReconcile::Fail { reason } => {
                RemoteStore::open(&paths.root)?.set_placement_state(
                    &record.submitted_run_id,
                    little_monkey_lib::node_placement::PlacementState::Failed.token(),
                    Some(&reason),
                    now,
                )?;
                println!("Placement {} failed: {reason}", record.submitted_run_id);
            }
        }
    }
    Ok(())
}

/// The node's job state, translated into this side's placement vocabulary.
///
/// Anything unrecognised maps to `Running` rather than to a terminal state: a
/// node running a newer build may name a state this one has never heard of, and
/// guessing "failed" would discard live work.
fn map_node_state(state: &str) -> little_monkey_lib::node_placement::PlacementState {
    use little_monkey_lib::node_placement::PlacementState;
    match state {
        "queued" | "preparing" | "held" => PlacementState::Accepted,
        "succeeded" => PlacementState::Succeeded,
        "failed" | "unknown" => PlacementState::Failed,
        "cancelled" => PlacementState::Cancelled,
        _ => PlacementState::Running,
    }
}

pub async fn spawn_if_configured(
    paths: DaemonPaths,
    desktop: std::sync::Arc<DesktopControlRuntime>,
    mobile_chat: std::sync::Arc<dyn api::MobileChatQueue>,
    placement: std::sync::Arc<dyn api::PlacementQueue>,
    peer_runs: std::sync::Arc<dyn crate::daemon::channel_worker::RunQueue>,
) -> Result<bool, String> {
    server::spawn_if_configured(paths, desktop, mobile_chat, placement, peer_runs).await
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
    capabilities.extend(device::parse_capabilities(&args.device_capabilities)?);
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
    // The same one-time token, in the form a camera can read. It pins the
    // certificate by fingerprint rather than carrying the PEM — see
    // `PairingBootstrap`. Always computed, because the desktop's JSON reader
    // shows the code beside the file and a terminal only prints it on `--qr`.
    let bootstrap = PairingBootstrap {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        runner_id: value.runner_id.clone(),
        runner_url: value.runner_url.clone(),
        pairing_id: value.pairing_id.clone(),
        pairing_token: value.pairing_token.clone(),
        certificate_sha256: value.server_certificate_sha256.clone(),
        expires_at_ms: value.expires_at_ms,
    };
    let uri = bootstrap.to_uri()?;
    if args.json {
        // The SVG rather than the matrix: the caller is a webview, and handing
        // it a grid of booleans would put the rendering rules — quiet zone,
        // crisp edges, contrast — in a second place.
        let code = qr::encode(&uri)?;
        println!(
            "{}",
            serde_json::json!({
                "invitation_path": args.output.display().to_string(),
                "controller_url": controller_url(&value.runner_url),
                "expires_at_ms": value.expires_at_ms,
                "bootstrap_uri": uri,
                "bootstrap_bytes": uri.len(),
                "qr_svg": code.to_svg(4),
                "qr_modules": code.size,
            })
        );
        return Ok(());
    }
    println!(
        "One-time pairing invitation written to {} (expires at {}). Transfer it securely, open {}, and choose the file.",
        args.output.display(),
        value.expires_at_ms,
        controller_url(&value.runner_url)
    );
    if args.qr {
        println!("\nScan this with the device's camera (it expires with the invitation):\n");
        println!("{}", qr::encode(&uri)?.to_terminal());
        println!("Or paste this code into the controller's pairing field:\n{uri}");
    }
    Ok(())
}

// --- Physical devices ------------------------------------------------------

/// One device as the operator reads it: the four sets kept visibly apart,
/// because "why can it not take a photo" has four different answers.
fn device_rows(paths: &DaemonPaths) -> Result<Vec<serde_json::Value>, String> {
    let store = RemoteStore::open(&paths.root)?;
    let now = now_ms()?;
    store
        .devices()?
        .into_iter()
        .map(|device| {
            let surface = store.device_surface(&device.device_id)?;
            let effective =
                protocol::effective_capabilities(&device.capabilities, surface.as_ref());
            let commands = store.device_commands(&device.device_id, 5)?;
            Ok(serde_json::json!({
                "device_id": device.device_id,
                "device_name": device.device_name,
                "revoked": !device.active(),
                "secret_generation": device.secret_generation,
                "granted": device.capabilities,
                "advertised": surface.as_ref().map(|surface| surface.capabilities.clone()),
                "os_permissions": surface.as_ref().map(|surface| surface.permissions.clone()),
                "readiness": surface.as_ref().map(|surface| surface.readiness.clone()),
                "effective": effective,
                // One row per physical capability with the four axes kept
                // apart and the single reason it is not effective named. The
                // intersection alone tells an operator nothing they can act on.
                "physical": protocol::PHYSICAL_DEVICE_CAPABILITIES
                    .iter()
                    .map(|capability| {
                        let block = protocol::capability_block(
                            &device.capabilities,
                            surface.as_ref(),
                            *capability,
                        );
                        serde_json::json!({
                            "capability": capability,
                            "granted": device.capabilities.contains(capability),
                            "supported": surface
                                .as_ref()
                                .is_some_and(|surface| surface.capabilities.contains(capability)),
                            "permission": surface
                                .as_ref()
                                .map(|surface| surface.permission(*capability)),
                            "readiness": surface
                                .as_ref()
                                .map(|surface| surface.readiness(*capability)),
                            "effective": block.is_none(),
                            "blocked_by": block.map(|block| block.as_str()),
                            "reason": block.map(|block| block.explain(*capability)),
                        })
                    })
                    .collect::<Vec<_>>(),
                "platform": surface.as_ref().map(|surface| surface.platform.clone()),
                "platform_version": surface.as_ref().map(|surface| surface.platform_version.clone()),
                "app_version": surface.as_ref().map(|surface| surface.app_version.clone()),
                "device_model": surface.as_ref().map(|surface| surface.device_model.clone()),
                "constraints": surface.as_ref().map(|surface| surface.constraints.clone()),
                "last_seen_at_ms": surface.as_ref().map(|surface| surface.reported_at_ms),
                "now_ms": now,
                "recent_commands": commands.iter().map(command_json).collect::<Vec<_>>(),
            }))
        })
        .collect()
}

fn command_json(record: &self::store::DeviceCommandRecord) -> serde_json::Value {
    serde_json::json!({
        "command_id": record.command_id,
        "device_id": record.device_id,
        "capability": record.capability,
        "state": record.state.as_str(),
        "attempt": record.attempt,
        "cancel_requested": record.cancel_requested,
        "created_at_ms": record.created_at_ms,
        "updated_at_ms": record.updated_at_ms,
        "expires_at_ms": record.expires_at_ms,
        "source_run_id": record.source_run_id,
        // Which attempt owns a running command, so an operator can see that a
        // reconnect resumed the same one rather than starting a second.
        "execution_id": record.execution_id,
        "artifact": record.artifact.as_ref().map(|artifact| serde_json::json!({
            "sha256": artifact.sha256,
            "bytes": artifact.bytes,
            "media_type": artifact.media_type,
        })),
        "error": record.error,
    })
}

fn device_list(paths: &DaemonPaths, json: bool) -> Result<(), String> {
    let rows = device_rows(paths)?;
    if json {
        return print_json(serde_json::json!({ "devices": rows }));
    }
    if rows.is_empty() {
        println!("No paired devices.");
        return Ok(());
    }
    for row in rows {
        let names = |key: &str| match row.get(key) {
            Some(serde_json::Value::Array(values)) => values
                .iter()
                .filter_map(|value| value.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Some(serde_json::Value::Null) | None => "(not reported)".to_string(),
            Some(other) => other.to_string(),
        };
        println!(
            "{} \"{}\"{}\n  device: {} {} (app {})\n  granted:   {}\n  supported: {}\n  OS:        {}\n  effective: {}",
            row["device_id"].as_str().unwrap_or_default(),
            row["device_name"].as_str().unwrap_or_default(),
            if row["revoked"] == serde_json::json!(true) {
                "  [revoked]"
            } else {
                ""
            },
            row["platform"].as_str().unwrap_or("(not reported)"),
            row["platform_version"].as_str().unwrap_or(""),
            row["app_version"].as_str().unwrap_or("?"),
            names("granted"),
            names("advertised"),
            match row["os_permissions"].as_object() {
                Some(map) => map
                    .iter()
                    .map(|(key, value)| format!("{key}={}", value.as_str().unwrap_or("?")))
                    .collect::<Vec<_>>()
                    .join(", "),
                None => "(not reported)".to_string(),
            },
            names("effective"),
        );
    }
    Ok(())
}

fn device_grant(
    paths: &DaemonPaths,
    device_id: &str,
    capabilities: &[String],
) -> Result<(), String> {
    let requested = device::parse_capabilities(capabilities)?;
    if let Some(unsupported) = requested
        .iter()
        .find(|capability| !capability.is_physical())
    {
        return Err(format!(
            "'{unsupported:?}' is not a physical device capability; grant it at pairing time \
             instead"
        ));
    }
    let mut store = RemoteStore::open(&paths.root)?;
    let device = store
        .device(device_id)?
        .ok_or_else(|| format!("Unknown remote device '{device_id}'"))?;
    // Only the physical half is replaced. The run-facing grants were frozen at
    // pairing and this command must not be a way to widen them.
    let mut next = device
        .capabilities
        .iter()
        .copied()
        .filter(|capability| !capability.is_physical())
        .collect::<BTreeSet<_>>();
    next.extend(requested);
    let stored = store.set_device_capabilities(device_id, &next, now_ms()?)?;
    println!(
        "{device_id} now grants: {}",
        stored
            .iter()
            .map(|capability| format!("{capability:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "A grant is only half the answer — the device must also advertise the capability and hold \
         the matching operating-system permission before it becomes effective."
    );
    Ok(())
}

async fn device_action(paths: &DaemonPaths, args: &RemoteDeviceActionArgs) -> Result<(), String> {
    let capability = device::capability_for_action(&args.action)?;
    let mut arguments = serde_json::Map::new();
    for (key, value) in [
        ("position", args.position.clone()),
        ("accuracy", args.accuracy.clone()),
        ("title", args.title.clone()),
        ("body", args.body.clone()),
        ("text", args.text.clone()),
        ("run_id", args.run_id.clone()),
        ("artifact_id", args.artifact_id.clone()),
    ] {
        if let Some(value) = value {
            arguments.insert(key.to_string(), serde_json::Value::String(value));
        }
    }
    if let Some(duration_ms) = args.duration_ms {
        arguments.insert("duration_ms".to_string(), serde_json::json!(duration_ms));
    }
    let record = device::dispatch(
        paths,
        &device::DeviceActionRequest {
            device_id: args.device_id.clone(),
            capability,
            arguments: serde_json::Value::Object(arguments),
            wait_ms: args.wait_ms,
            source_run_id: None,
            source_session_id: None,
            source_tool_call_id: args.invocation_id.clone(),
            // Supplied only by a caller that has a durable invocation to name —
            // the desktop passes its turn and tool-call ids. An operator typing
            // this command twice means it twice.
            invocation_id: args.invocation_id.clone(),
        },
        now_ms()?,
    )
    .await?;
    if args.json {
        return print_json(device::result_json(&record));
    }
    println!("{} {}", record.command_id, record.state.as_str());
    if let Some(error) = &record.error {
        println!("  {error}");
    }
    if let Some(result) = &record.result {
        println!("  {result}");
    }
    if let Some(artifact) = &record.artifact {
        println!(
            "  artifact: {} bytes of {} (sha256 {})",
            artifact.bytes, artifact.media_type, artifact.sha256
        );
    }
    Ok(())
}

/// Exercises a genuinely paired device and says whether it works.
///
/// The one thing the automated suite cannot cover: real hardware, over the real
/// signed transport, on somebody's actual phone. It needs no credentials, no
/// account and no project — the pairing the operator already made is the whole
/// setup — and it is never part of a normal test run, because a photograph is
/// not something CI should take.
///
/// Safe checks run by default and touch no sensor. `--dangerous` adds the
/// physical ones, each of which is skipped unless the device says it is
/// effective, so the check reports "not ready" rather than hanging on a
/// capability the phone cannot serve.
async fn device_check(
    paths: &DaemonPaths,
    device_id: Option<&str>,
    dangerous: bool,
    wait_ms: u64,
    json: bool,
) -> Result<(), String> {
    let rows = device_rows(paths)?;
    let row = match device_id {
        Some(device_id) => rows
            .iter()
            .find(|row| row["device_id"].as_str() == Some(device_id))
            .ok_or_else(|| format!("No paired device '{device_id}'"))?,
        None => match rows.len() {
            0 => return Err("No device is paired with this runner.".to_string()),
            1 => &rows[0],
            _ => {
                return Err(format!(
                    "{} devices are paired — name one with --device-id",
                    rows.len()
                ))
            }
        },
    };
    let device_id = row["device_id"].as_str().unwrap_or_default().to_string();
    let physical = row["physical"].as_array().cloned().unwrap_or_default();
    let effective = |capability: &str| {
        physical.iter().any(|entry| {
            entry["capability"].as_str() == Some(capability)
                && entry["effective"] == serde_json::json!(true)
        })
    };

    if !json {
        println!(
            "Checking {} ({})",
            row["device_name"].as_str().unwrap_or_default(),
            device_id
        );
        for entry in &physical {
            println!(
                "  {:<20} granted={} supported={} permission={} readiness={} effective={}{}",
                entry["capability"].as_str().unwrap_or_default(),
                entry["granted"],
                entry["supported"],
                entry["permission"].as_str().unwrap_or("not reported"),
                entry["readiness"].as_str().unwrap_or("not reported"),
                entry["effective"],
                match entry["reason"].as_str() {
                    Some(reason) => format!("\n      {reason}"),
                    None => String::new(),
                },
            );
        }
    }

    // Safe first, then the physical ones only when asked for. `device_info`
    // reads the device's own name and nothing else, which is why it is the
    // default check: it proves the whole path — queue, wake, lease, start,
    // result — without touching a sensor.
    let mut plan: Vec<(&str, serde_json::Value)> = vec![("device_info", serde_json::json!({}))];
    if dangerous {
        plan.extend([
            ("camera_capture", serde_json::json!({ "position": "back" })),
            (
                "microphone_capture",
                serde_json::json!({ "duration_ms": 2_000 }),
            ),
            ("screen_capture", serde_json::json!({})),
            ("location_read", serde_json::json!({ "accuracy": "coarse" })),
            (
                "notification_post",
                serde_json::json!({ "title": "Little Monkey", "body": "Device check" }),
            ),
            (
                "audio_playback",
                serde_json::json!({ "text": "Device check" }),
            ),
        ]);
    }

    let mut results = Vec::new();
    let mut failures = 0usize;
    for (action, arguments) in plan {
        if !effective(action) {
            results.push(serde_json::json!({
                "action": action,
                "status": "skipped",
                "detail": "not effective on this device",
            }));
            if !json {
                println!("  {action}: skipped (not effective)");
            }
            continue;
        }
        let capability = device::capability_for_action(action)?;
        let record = device::dispatch(
            paths,
            &device::DeviceActionRequest {
                device_id: Some(device_id.clone()),
                capability,
                arguments,
                wait_ms,
                source_run_id: None,
                source_session_id: None,
                source_tool_call_id: None,
                // Each check is its own ask, so none of them dedupes against
                // another.
                invocation_id: None,
            },
            now_ms()?,
        )
        .await?;
        // The shape, not merely the state: a success carrying neither a result
        // nor an artifact has not proven anything.
        let succeeded = record.state == protocol::DeviceCommandState::Succeeded;
        let has_payload = record.result.is_some() || record.artifact.is_some();
        let artifact_ok = record.artifact.as_ref().is_none_or(|artifact| {
            artifact.bytes > 0
                && artifact.sha256.len() == 64
                && !artifact.media_type.trim().is_empty()
        });
        if !(succeeded && has_payload && artifact_ok) {
            failures += 1;
        }
        results.push(serde_json::json!({
            "action": action,
            "status": if succeeded && has_payload && artifact_ok { "ok" } else { "failed" },
            "state": record.state.as_str(),
            "command_id": record.command_id,
            "artifact": record.artifact.as_ref().map(|artifact| serde_json::json!({
                "sha256": artifact.sha256,
                "bytes": artifact.bytes,
                "media_type": artifact.media_type,
            })),
            "error": record.error,
        }));
        if !json {
            println!(
                "  {action}: {} ({}){}",
                if succeeded && has_payload && artifact_ok {
                    "ok"
                } else {
                    "FAILED"
                },
                record.state.as_str(),
                match &record.error {
                    Some(error) => format!(" — {error}"),
                    None => String::new(),
                }
            );
        }
    }

    if json {
        print_json(serde_json::json!({
            "device_id": device_id,
            "physical": physical,
            "checks": results,
            "failures": failures,
        }))?;
    }
    if failures > 0 {
        return Err(format!("{failures} device check(s) failed"));
    }
    Ok(())
}

fn push_status(paths: &DaemonPaths, json: bool) -> Result<(), String> {
    let config = push::load_config(paths)?;
    let registrations = RemoteStore::open(&paths.root)?.push_registrations()?;
    let value = serde_json::json!({
        "configured": config.is_some(),
        "enabled": config.as_ref().is_some_and(|config| config.enabled),
        "backend": config.as_ref().map(|config| config.backend.clone()),
        "project_id": config.as_ref().map(|config| config.project_id.clone()),
        // The public half only. The VAPID private key never leaves the keychain
        // and the device tokens are addresses, not diagnostics.
        "application_server_key": push::application_server_key(paths, &KeyringRemoteSecrets)
            .ok()
            .flatten(),
        "include_detail": config.as_ref().is_some_and(|config| config.include_detail),
        // Never the token itself: it is the device's address, and printing it
        // into a terminal or a support log helps nobody.
        "registered_devices": registrations
            .iter()
            .map(|(device_id, backend, _)| serde_json::json!({
                "device_id": device_id,
                "backend": backend,
            }))
            .collect::<Vec<_>>(),
    });
    if json {
        return print_json(value);
    }
    match config {
        None => println!(
            "Push is not configured. It needs your own Firebase project — Little Monkey ships none."
        ),
        Some(config) => println!(
            "Push backend {} for project {} ({}){}.\n{} device(s) registered.",
            config.backend,
            config.project_id,
            if config.enabled {
                "enabled"
            } else {
                "disabled"
            },
            if config.include_detail {
                ", detail included"
            } else {
                ", detail withheld"
            },
            registrations.len()
        ),
    }
    Ok(())
}

fn device_commands(
    paths: &DaemonPaths,
    device_id: &str,
    limit: u32,
    json: bool,
) -> Result<(), String> {
    let store = RemoteStore::open(&paths.root)?;
    let commands = store.device_commands(device_id, limit)?;
    if json {
        return print_json(serde_json::json!({
            "commands": commands.iter().map(command_json).collect::<Vec<_>>(),
        }));
    }
    for record in &commands {
        println!(
            "{}  {:<10} {:?}{}",
            record.command_id,
            record.state.as_str(),
            record.capability,
            record
                .error
                .as_ref()
                .map(|error| format!("  — {error}"))
                .unwrap_or_default()
        );
    }
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

/// Moves a frozen process image to a paired node (roadmap K18).
///
/// The order is the whole design, so it is worth stating:
///
/// 1. **Preflight first, and it transfers nothing.** The target answers from the
///    header alone, so a refusal costs a round trip instead of a workspace.
/// 2. **The departure is appended locally *before* the image is sent**, because
///    the arrival on the far side has to name the origin's chain tip — and the
///    tip is that departure. Doing it the other way round would leave the target
///    naming an event that did not exist when it named it.
/// 3. **A departure that is then refused stays in the origin's history.** It is
///    not terminal and changes no status, so this run carries on here; an audit
///    that only recorded successful moves would be an audit of nothing.
async fn migrate_run(
    paths: &DaemonPaths,
    alias: &str,
    checkpoint_id: &str,
    required_residency: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    use little_monkey_lib::run_ledger::RunLedger;
    use little_monkey_lib::run_protocol::{
        ClientIdentity, ClientKind, RunEvent, RunEventEnvelope, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    let app_data_dir = crate::app_data_dir()
        .ok_or_else(|| "Could not resolve the app data directory".to_string())?;
    let host = enabled_host(paths)?;
    if let Some(residency) = required_residency {
        little_monkey_lib::node_placement::validate_residency(residency)?;
    }

    // The run this checkpoint's process belongs to. Read from the process row
    // rather than guessed: a checkpoint records a turn, and only the process
    // table knows which durable run that turn was charged to.
    let manifest_dir = migrate::checkpoints_dir(&app_data_dir).join(checkpoint_id);
    little_monkey_lib::checkpoints::validate_checkpoint_id(checkpoint_id)?;
    if !manifest_dir.is_dir() {
        return Err(format!("No checkpoint '{checkpoint_id}' on this node"));
    }
    let mut ledger = RunLedger::open(&paths.ledger_db).map_err(|error| error.to_string())?;
    let frozen_process_id = {
        let raw = std::fs::read_to_string(manifest_dir.join("manifest.json"))
            .map_err(|error| format!("Could not read the checkpoint manifest: {error}"))?;
        let manifest: little_monkey_lib::checkpoints::CheckpointManifest =
            serde_json::from_str(&raw)
                .map_err(|error| format!("Checkpoint manifest is invalid: {error}"))?;
        manifest
            .resume
            .map(|resume| resume.process_id)
            .ok_or_else(|| {
                format!("Checkpoint '{checkpoint_id}' is a turn snapshot, not a frozen process")
            })?
    };
    let run_id = ledger
        .process_table()
        .get(&frozen_process_id)
        .map_err(|error| error.to_string())?
        .and_then(|record| record.run_id)
        .ok_or_else(|| {
            format!("Frozen process '{frozen_process_id}' is not charged to a durable run, so there is no chain to hand over")
        })?;
    let spec = ledger
        .load_run(&run_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("Unknown durable run '{run_id}'"))?
        .spec;

    let mut image = migrate::build_image(
        &app_data_dir,
        &host.runner_id,
        checkpoint_id,
        &spec,
        // Placeholders. The real tip is only knowable after the departure is
        // appended, which only happens once the preflight has passed.
        1,
        &"0".repeat(64),
        required_residency.map(str::to_string),
    )?;

    let preflight = client::call(
        paths,
        alias,
        Method::POST,
        "/v1/remote/node/migration/preflight",
        serde_json::to_vec(&protocol::MigrationPreflightRequest {
            protocol_version: protocol::REMOTE_PROTOCOL_VERSION,
            header: image.header.clone(),
        })
        .map_err(|error| error.to_string())?,
        now_ms()?,
    )
    .await?;
    let acceptable = preflight
        .get("verdict")
        .and_then(|verdict| verdict.get("state"))
        .and_then(serde_json::Value::as_str)
        == Some("acceptable");
    if !acceptable || dry_run {
        print_json(preflight)?;
        if !acceptable {
            return Err("The target node refused this image; nothing was transferred.".to_string());
        }
        println!("Dry run: the target would accept this image. Nothing was transferred.");
        return Ok(());
    }

    let departure_event = RunEvent::MigrationDeparted {
        target_node_id: preflight
            .get("node")
            .and_then(|node| node.get("runner_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "The target did not identify itself".to_string())?
            .to_string(),
        payload_sha256: image.header.payload_sha256.clone(),
        checkpoint_id: checkpoint_id.to_string(),
    };
    let now = now_ms()?;
    let sequence = ledger
        .load_run(&run_id)
        .map_err(|error| error.to_string())?
        .map(|run| run.last_sequence + 1)
        .ok_or_else(|| format!("Unknown durable run '{run_id}'"))?;
    ledger
        .append_event(&RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: format!("evt-depart-{}", &image.header.payload_sha256[..24]),
            run_id: run_id.clone(),
            sequence,
            occurred_at_ms: now,
            actor_id: None,
            emitter: ClientIdentity {
                client_id: host.runner_id.clone(),
                instance_id: host.runner_id.clone(),
                kind: ClientKind::Cli,
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            event: departure_event,
        })
        .map_err(|error| error.to_string())?;
    let departure = ledger
        .migration_departure(&run_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "The departure did not chain on this node".to_string())?;
    image.origin_last_sequence = departure.sequence;
    image.origin_last_event_hash = departure.event_hash.clone();

    let receipt = client::call(
        paths,
        alias,
        Method::POST,
        "/v1/remote/node/migration/accept",
        serde_json::to_vec(&protocol::MigrationAcceptRequest {
            protocol_version: protocol::REMOTE_PROTOCOL_VERSION,
            image,
        })
        .map_err(|error| error.to_string())?,
        now_ms()?,
    )
    .await?;
    print_json(receipt)?;
    println!(
        "Handed run {run_id} to '{alias}'. The origin's chain ends at sequence {} ({}), which the target's first event names.",
        departure.sequence, departure.event_hash
    );
    Ok(())
}

/// The advertised transport, for callers outside this module that need to
/// describe it — the security audit asks whether a phone holding a camera grant
/// is talking over a pinned connection, and there must be one answer to that
/// rather than a second reader of the same file.
pub(crate) fn host_config(
    paths: &DaemonPaths,
) -> Result<Option<protocol::RemoteHostConfig>, String> {
    server::load_host_config(paths)
}

pub(crate) fn enabled_host(paths: &DaemonPaths) -> Result<protocol::RemoteHostConfig, String> {
    let config = server::load_host_config(paths)?
        .ok_or_else(|| "Remote host is not configured".to_string())?;
    if !config.enabled {
        return Err("Remote host is disabled".to_string());
    }
    Ok(config)
}

pub(crate) fn protected_json(path: &Path, value: &impl serde::Serialize) -> Result<(), String> {
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

/// Milliseconds since the epoch, for the peer surface and everything else here.
pub(crate) fn now_ms_public() -> Result<u64, String> {
    now_ms()
}

/// Offer another installation peer standing on this one, and nothing else.
///
/// The scope is deliberately empty: no action, no run id, no workspace. A peer
/// invitation carries the three peer grants the operator chose and nothing that
/// would let the far side read a run or approve anything here.
pub(crate) fn create_peer_invitation(
    paths: &DaemonPaths,
    label: &str,
    grants: &BTreeSet<protocol::DeviceCapability>,
    expires_minutes: u64,
) -> Result<PairingInvitation, String> {
    let config = enabled_host(paths)?;
    let scopes = RemoteScopes {
        actions: BTreeSet::new(),
        run_ids: BTreeSet::new(),
        workspace_ids: BTreeSet::new(),
        max_artifact_bytes: protocol::MAX_REMOTE_ARTIFACT_BYTES,
    };
    let now = now_ms()?;
    let expires_at_ms = now
        .checked_add(expires_minutes.saturating_mul(60_000))
        .ok_or_else(|| "Pairing expiry overflow".to_string())?;
    let invitation = RemoteStore::open(&paths.root)?.create_invitation_with_capabilities(
        &scopes,
        grants,
        now,
        expires_at_ms,
    )?;
    let certificate = std::fs::read_to_string(&config.certificate_path)
        .map_err(|error| format!("Could not read remote certificate: {error}"))?;
    // The label travels as the device name the far side proposes back, so it is
    // not part of the invitation itself; it is what this side will call the
    // peer once it accepts.
    let _ = label;
    Ok(PairingInvitation {
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
    })
}

/// Write an invitation where only this user can read it.
pub(crate) fn write_invitation_file(
    path: &Path,
    invitation: &PairingInvitation,
) -> Result<(), String> {
    protected_json(path, invitation)
}

/// Take up a peer's invitation, so this installation can talk to it.
pub(crate) async fn accept_peer_invitation(
    paths: &DaemonPaths,
    invitation: &Path,
    alias: &str,
) -> Result<protocol::ControllerProfile, String> {
    client::accept_invitation(paths, invitation, alias, &peer_device_name(), now_ms()?).await
}

/// How this installation introduces itself when accepting a peer invitation.
fn peer_device_name() -> String {
    format!(
        "little-monkey-peer-{}",
        hostname_label().unwrap_or_else(|| "unnamed".to_string())
    )
}

fn hostname_label() -> Option<String> {
    let raw = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::env::var("COMPUTERNAME").ok())?;
    let label: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(32)
        .collect();
    (!label.is_empty()).then_some(label)
}

/// One signed call to a peer, by the alias it was accepted under.
pub(crate) async fn peer_call(
    paths: &DaemonPaths,
    alias: &str,
    method: Method,
    path_and_query: &str,
    body: Vec<u8>,
) -> Result<serde_json::Value, String> {
    client::call(paths, alias, method, path_and_query, body, now_ms()?).await
}

/// Take up the replacement key a peer produced when it rotated this pairing.
///
/// The same bundle format and the same verification a controller rotation
/// uses — a peer credential is a device credential, and giving it a second
/// path would mean a second place for the scope checks to be forgotten.
pub(crate) fn accept_peer_rotation(
    paths: &DaemonPaths,
    alias: &str,
    bundle: &Path,
) -> Result<protocol::ControllerProfile, String> {
    client::accept_rotation(paths, alias, bundle, now_ms()?)
}

/// Introduce this installation to a peer and record what came back.
///
/// The one call that both proves a peer is reachable and refreshes what each
/// side knows about the other. `last_seen_at_ms` is written only on success —
/// a failed probe must never read as contact.
pub(crate) async fn peer_hello(
    paths: &DaemonPaths,
    alias: &str,
    requested: &BTreeSet<DeviceCapability>,
) -> Result<protocol::PeerHelloResponse, String> {
    let request = protocol::PeerHelloRequest {
        protocol_version: protocol::REMOTE_PROTOCOL_VERSION,
        instance_id: local_instance_id(paths, alias)?,
        advertised: protocol::all_peer_capabilities(),
        requested: requested.clone(),
    };
    request.validate()?;
    let value = peer_call(
        paths,
        alias,
        Method::POST,
        "/v1/remote/peer/hello",
        serde_json::to_vec(&request).map_err(|error| error.to_string())?,
    )
    .await?;
    let response: protocol::PeerHelloResponse = serde_json::from_value(value)
        .map_err(|error| format!("Peer hello response is invalid: {error}"))?;
    response.validate()?;

    let now = now_ms()?;
    let mut store = RemoteStore::open(&paths.root)?;
    if let Some(mut profile) = store.controller(alias)? {
        profile.last_seen_at_ms = Some(now);
        profile.peer_advertised = response.advertised.clone();
        profile.peer_requested = request.requested.clone();
        // The far side is authoritative about what it grants; recording its
        // answer is how a revocation over there shows up over here without
        // this installation guessing.
        profile.capabilities = protocol::peer_capabilities_of(&response.granted);
        let secret = RemoteStore::controller_secret(&profile, &KeyringRemoteSecrets)?;
        store.save_controller(&profile, &secret, now, &KeyringRemoteSecrets)?;
    }
    Ok(response)
}

/// Hand one artifact's bytes to a peer before referencing it in an envelope.
///
/// Push, not pull: the receiver holds no outbound pairing back here, so it
/// could not fetch even if it wanted to. Returns what the receiver stored,
/// which is what the envelope must then name.
pub(crate) async fn peer_put_artifact(
    paths: &DaemonPaths,
    alias: &str,
    bytes: &[u8],
    filename: Option<&str>,
    media_type: Option<&str>,
) -> Result<protocol::PeerArtifactStored, String> {
    use base64::Engine as _;
    let upload = protocol::PeerArtifactUpload {
        protocol_version: protocol::REMOTE_PROTOCOL_VERSION,
        sha256: protocol::sha256_hex(bytes),
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        filename: filename.map(str::to_string),
        media_type: media_type.map(str::to_string),
    };
    upload.validate()?;
    let value = peer_call(
        paths,
        alias,
        Method::POST,
        "/v1/remote/peer/artifacts",
        serde_json::to_vec(&upload).map_err(|error| error.to_string())?,
    )
    .await?;
    let stored: protocol::PeerArtifactStored = serde_json::from_value(value)
        .map_err(|error| format!("Peer artifact response is invalid: {error}"))?;
    if stored.sha256 != upload.sha256 || stored.artifact_id != upload.sha256 {
        return Err("The peer stored different content than was sent".to_string());
    }
    Ok(stored)
}

/// What this installation calls itself in an envelope's origin chain.
///
/// The runner id when this machine also hosts, because that is the identity
/// every peer already knows it by. Otherwise the device id the peer issued at
/// pairing time: unique, stable, and already meaningful to the receiver — which
/// is all the loop check and the dedupe key need.
pub(crate) fn local_instance_id(paths: &DaemonPaths, alias: &str) -> Result<String, String> {
    if let Some(config) = server::load_host_config(paths)? {
        return Ok(config.runner_id);
    }
    let profile = RemoteStore::open(&paths.root)?
        .controller(alias)?
        .ok_or_else(|| format!("Unknown peer '{alias}'"))?;
    Ok(profile.device_id)
}

fn now_ms() -> Result<u64, String> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())
        .and_then(|duration| {
            u64::try_from(duration.as_millis()).map_err(|_| "timestamp overflow".to_string())
        })
}

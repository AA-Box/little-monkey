//! How a provider's callback reaches a listener that only binds loopback.
//!
//! Four messaging providers and three carriers deliver by posting to a URL, and
//! this process binds `127.0.0.1`. Something has to stand between them. Until
//! now that something was entirely the operator's problem: run a tunnel in
//! another terminal, keep it up, notice when it stops, and paste whatever URL it
//! produced into a provider console. Little Monkey knew nothing about it, which
//! meant a dead tunnel looked exactly like a provider that had gone quiet.
//!
//! This module makes the exposure a thing the daemon owns, without making it a
//! thing this project operates.
//!
//! # What is emphatically not happening here
//!
//! There is no Little Monkey relay, no shared domain, no hosted endpoint, no
//! account belonging to anybody but the operator. A managed tunnel is the
//! operator's own tunnel: their provider account, their credential, their
//! hostname, a binary they installed. What the daemon adds is lifecycle — start
//! it, watch it, restart it with a bound, say what it is doing — which is
//! exactly the part a person cannot do while asleep.
//!
//! # The trust boundary
//!
//! A tunnel is **transport**. It terminates at the same loopback listener a
//! curl from this machine would reach, and everything past that point is
//! unchanged: the provider's own signature over the exact bytes, the replay
//! window, the durable event row, the access policy. Nothing about a request
//! arriving through a tunnel makes it more trusted than one that did not.
//!
//! In particular the public base URL is **configuration, never observation**.
//! Providers that sign the callback URL are verified against the base this
//! module resolves, which comes from what the operator configured — never from
//! `Host`, `X-Forwarded-Host`, `X-Forwarded-Proto` or `Forwarded`, all of which
//! a tunnel sets and an attacker can forge. That is why
//! [`ExposureConfig::public_base`] is derived from the *configured* hostname and
//! not from anything the running tunnel reports: a verification URL that moved
//! when a process restarted would be a verification URL an attacker could move.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::channel_adapter::ChannelSecrets;
use super::store::{DaemonPaths, DaemonStore};

/// `daemon_meta` key holding what the operator configured.
pub(crate) const EXPOSURE_CONFIG_KEY: &str = "channels.exposure_config";
/// `daemon_meta` key holding what the supervisor last observed.
///
/// In the database rather than in memory because the reader is a different
/// process: `monkey channels exposure` and the desktop bridge both shell the
/// CLI, which never shares an address space with the daemon.
pub(crate) const EXPOSURE_STATE_KEY: &str = "channels.exposure_state";
/// `daemon_meta` key holding the pid of the tunnel this daemon started.
///
/// Written so a daemon that was killed rather than stopped does not leave a
/// second tunnel behind on the next start. A pid alone is not proof of identity
/// — pids are reused — so it is only ever used to *stop* something, and only
/// when this daemon is the one that wrote it.
const EXPOSURE_PID_KEY: &str = "channels.exposure_pid";

/// The keychain entry a managed tunnel's credential lives under.
///
/// One entry, like the public base is one value: the operator runs one tunnel
/// in front of one listener. It is a keychain account name in the same
/// namespace every channel credential uses, so the database holds the name and
/// never the secret.
pub(crate) const TUNNEL_CREDENTIAL_REF: &str = "channel-exposure:tunnel-token";

/// Backoff floor and ceiling for a tunnel that will not stay up.
const BACKOFF_MIN_MS: u64 = 2_000;
const BACKOFF_MAX_MS: u64 = 60_000;
/// How long after a spawn the process is given to report itself ready before
/// the supervisor calls it degraded. `cloudflared` dials four edge connections;
/// on a slow link the first one can take a few seconds.
const READY_GRACE_MS: u64 = 20_000;
/// The most stderr kept from a failed tunnel. Enough for the one line that says
/// what was wrong, and short enough that it is an error message rather than a
/// log.
const MAX_TUNNEL_ERROR_CHARS: usize = 400;

/// Where a provider's callback is expected to arrive from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExposureMode {
    /// The operator publishes the URL themselves — a reverse proxy, their own
    /// domain, a tunnel they run outside this app. The original behaviour, and
    /// still the default: nothing about this feature turns anything on.
    #[default]
    Manual,
    /// The daemon runs the operator's own tunnel client and supervises it.
    ManagedTunnel,
}

/// Which tunnel client. An enum rather than a free-form command, so React
/// cannot ask this process to run something of its choosing — the argv is built
/// here from a fixed template and the only operator-supplied part is a
/// validated absolute path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TunnelProvider {
    /// `cloudflared` running a *named*, remotely-managed tunnel.
    ///
    /// Named specifically. A quick tunnel (`cloudflared tunnel --url ...`)
    /// mints a fresh random `trycloudflare.com` hostname on every start, which
    /// cannot be pasted into a provider console and would break every callback
    /// signature the moment the process restarted. A backend that cannot hold a
    /// stable URL is not webhook exposure, so this one is not offered.
    Cloudflared,
}

impl TunnelProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            TunnelProvider::Cloudflared => "cloudflared",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "cloudflared" => Some(TunnelProvider::Cloudflared),
            _ => None,
        }
    }

    /// What the operator has to have done in their own provider's console
    /// before this can work. Shown in the UI and in the CLI's error, because
    /// "connected but nothing arrives" is the failure this prevents.
    pub fn prerequisite(self) -> &'static str {
        match self {
            TunnelProvider::Cloudflared => {
                "In your own Cloudflare Zero Trust dashboard, create a tunnel, add a public \
                 hostname for it, and point that hostname's service at this machine's webhook \
                 listener (http://localhost:<port>). Then paste the tunnel's token here."
            }
        }
    }
}

/// What the operator configured. Never a secret: the credential is a keychain
/// entry name, exactly like every channel account's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExposureConfig {
    #[serde(default)]
    pub mode: ExposureMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<TunnelProvider>,
    /// The hostname the operator configured in their tunnel provider's console,
    /// e.g. `monkey.example.com`. This — not anything the process reports — is
    /// what the public callback base is built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Absolute path to the tunnel client the operator installed. Absolute and
    /// validated for the same reason `signal-cli`'s is: a relative name is
    /// resolved against a `PATH` this process does not control, so it names
    /// whichever binary happens to be first today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// Loopback port the tunnel client serves its own readiness on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics_port: Option<u16>,
}

impl ExposureConfig {
    /// The public base every provider composes its callback URL from.
    ///
    /// Derived from the configured hostname and always `https`. Deliberately
    /// not read from the running tunnel: a base that came from the process
    /// would change when the process did, and it is the value callback
    /// signatures are verified against.
    pub fn public_base(&self, manual: Option<&str>) -> Option<String> {
        match self.mode {
            ExposureMode::Manual => manual.map(str::to_string),
            ExposureMode::ManagedTunnel => self
                .hostname
                .as_deref()
                .map(|hostname| format!("https://{hostname}")),
        }
    }

    /// Everything that has to be true before a tunnel may be started, or the
    /// state that says which piece is missing.
    pub fn readiness(&self) -> Result<StartPlan, ExposureState> {
        if self.mode != ExposureMode::ManagedTunnel {
            return Err(ExposureState::NotConfigured);
        }
        let Some(provider) = self.provider else {
            return Err(ExposureState::NotConfigured);
        };
        if self.hostname.as_deref().unwrap_or_default().is_empty() {
            return Err(ExposureState::PublicUrlUnavailable);
        }
        let Some(executable) = self.executable.as_deref() else {
            return Err(ExposureState::HelperMissing);
        };
        let path = PathBuf::from(executable);
        // Absolute before existing: a relative path that happens to resolve is
        // the more dangerous of the two, because it silently names a different
        // binary depending on where this process was started.
        if !path.is_absolute() {
            return Err(ExposureState::HelperMissing);
        }
        if !path.is_file() {
            return Err(ExposureState::HelperMissing);
        }
        Ok(StartPlan {
            provider,
            executable: path,
            metrics_port: self.metrics_port.unwrap_or(DEFAULT_METRICS_PORT),
        })
    }
}

/// `cloudflared`'s own default range starts here, and the daemon picks the
/// bottom of it unless told otherwise.
pub(crate) const DEFAULT_METRICS_PORT: u16 = 20_241;

/// A configuration that has everything it needs, resolved into what to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPlan {
    pub provider: TunnelProvider,
    pub executable: PathBuf,
    pub metrics_port: u16,
}

/// What the supervisor last observed. Deterministic, and each value maps to one
/// thing an operator can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExposureState {
    /// No managed exposure is set up. Not a fault, and the default: a state
    /// that could not be read must not be a state that claims to be working.
    #[default]
    NotConfigured,
    /// Configured, but the executable is absent, relative, or not a file.
    HelperMissing,
    /// Configured, but the keychain has no token under the entry name.
    CredentialMissing,
    /// Started; no readiness yet, and still inside the grace period.
    Connecting,
    /// The tunnel client reports an active connection to its provider's edge.
    Connected,
    /// Running past the grace period without reporting ready.
    Degraded,
    /// It exited and is being started again after a bounded wait.
    Reconnecting,
    /// It exited in a way that names the credential as the cause.
    AuthenticationFailed,
    /// Configured for a managed tunnel with no hostname to build a URL from.
    PublicUrlUnavailable,
    /// The operator turned it off.
    Stopped,
}

impl ExposureState {
    pub fn as_str(self) -> &'static str {
        match self {
            ExposureState::NotConfigured => "not_configured",
            ExposureState::HelperMissing => "helper_missing",
            ExposureState::CredentialMissing => "credential_missing",
            ExposureState::Connecting => "connecting",
            ExposureState::Connected => "connected",
            ExposureState::Degraded => "degraded",
            ExposureState::Reconnecting => "reconnecting",
            ExposureState::AuthenticationFailed => "authentication_failed",
            ExposureState::PublicUrlUnavailable => "public_url_unavailable",
            ExposureState::Stopped => "stopped",
        }
    }

    /// Whether a provider posting to the public URL right now would reach this
    /// machine. Only one value means yes, and "configured" is not it.
    pub fn is_reachable(self) -> bool {
        matches!(self, ExposureState::Connected)
    }
}

/// The whole of what a front end is told. No token, no argv, no pid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExposureStatus {
    pub mode: ExposureMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<TunnelProvider>,
    pub state: ExposureState,
    /// The base every callback URL is composed from, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public_base: Option<String>,
    /// Whether a credential is stored — never what it is.
    pub credential_stored: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
    /// A short, redacted excerpt of why it last failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub restarts: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<i64>,
}

/// The half of the status the supervisor writes, so a reader in another process
/// sees what the daemon last observed rather than guessing from the config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct StoredState {
    #[serde(default)]
    state: ExposureState,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    restarts: u32,
    #[serde(default)]
    since_ms: Option<i64>,
}

/// Read what the operator configured. An unreadable or absent value is
/// `Manual`, which is the behaviour every existing installation already has.
pub(crate) fn read_config(store: &DaemonStore) -> ExposureConfig {
    store
        .get_meta(EXPOSURE_CONFIG_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

pub(crate) fn write_config(store: &mut DaemonStore, config: &ExposureConfig) -> Result<(), String> {
    let encoded = serde_json::to_string(config).map_err(|error| error.to_string())?;
    store.set_meta(EXPOSURE_CONFIG_KEY, &encoded)
}

fn read_state(store: &DaemonStore) -> StoredState {
    store
        .get_meta(EXPOSURE_STATE_KEY)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_state(store: &mut DaemonStore, state: &StoredState) {
    if let Ok(encoded) = serde_json::to_string(state) {
        let _ = store.set_meta(EXPOSURE_STATE_KEY, &encoded);
    }
}

/// Everything a front end shows, assembled from the config, the supervisor's
/// last observation, and whether a credential exists.
///
/// The state is *corrected* against the configuration rather than taken as
/// read: a daemon that is not running leaves whatever it last wrote, and
/// reporting "connected" for a tunnel that died with its supervisor would be
/// exactly the fake status this project refuses to show. So a configuration
/// problem — no hostname, no helper, no credential — always wins over a stale
/// success.
pub(crate) fn status(store: &DaemonStore, secrets: &dyn ChannelSecrets) -> ExposureStatus {
    let config = read_config(store);
    let manual = store.channel_public_base_url_manual().ok().flatten();
    let stored = read_state(store);
    let credential_stored = secrets
        .get(TUNNEL_CREDENTIAL_REF)
        .map(|token| !token.is_empty())
        .unwrap_or(false);
    let state = match config.readiness() {
        Err(blocked) => blocked,
        Ok(_) if !credential_stored => ExposureState::CredentialMissing,
        Ok(_) => stored.state,
    };
    ExposureStatus {
        mode: config.mode,
        provider: config.provider,
        state,
        public_base: config.public_base(manual.as_deref()),
        credential_stored,
        executable: config.executable.clone(),
        last_error: stored.last_error,
        restarts: stored.restarts,
        since_ms: stored.since_ms,
    }
}

/// Trim and redact one line of a tunnel client's stderr.
///
/// The token is removed by *value*: a client that echoed its own credential
/// into a diagnostic — none of the supported ones do, but a future version
/// might — must not have it end up in `daemon_meta`, a support bundle, or a
/// settings panel. Substituted rather than dropped, so the shape of the message
/// survives and a reader can see something was removed.
pub(crate) fn redact_tunnel_error(raw: &str, token: &str) -> String {
    let mut text = raw.trim().to_string();
    if !token.is_empty() {
        text = text.replace(token, "[redacted]");
    }
    let mut bounded: String = text.chars().take(MAX_TUNNEL_ERROR_CHARS).collect();
    if text.chars().count() > MAX_TUNNEL_ERROR_CHARS {
        bounded.push('…');
    }
    bounded
}

/// Whether this failure names the credential.
///
/// Matched on the words a client uses when a token is rejected, so the operator
/// is told to check their token rather than being told the tunnel "failed".
/// Deliberately conservative: an unrecognised failure stays a generic one,
/// because telling somebody their credential is wrong when it is not sends them
/// to rotate a working secret.
pub(crate) fn looks_like_bad_credential(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    [
        "unauthorized",
        "401",
        "invalid tunnel",
        "token is invalid",
        "failed to parse token",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

/// The argv one tunnel client is run with.
///
/// A fixed template. The only operator-supplied value in it is the metrics
/// port; the executable is a validated absolute path and the credential does
/// **not** appear here at all — it goes in the environment, which is what the
/// provider's own documentation recommends precisely so it stays out of
/// `ps`. Nothing is passed through a shell, so there is no interpolation to get
/// wrong.
pub(crate) fn tunnel_argv(plan: &StartPlan) -> Vec<String> {
    match plan.provider {
        TunnelProvider::Cloudflared => vec![
            "tunnel".to_string(),
            // The daemon owns this process's lifetime. A client that replaced
            // its own binary underneath us would be a process we did not start.
            "--no-autoupdate".to_string(),
            "--metrics".to_string(),
            format!("127.0.0.1:{}", plan.metrics_port),
            "run".to_string(),
        ],
    }
}

/// The environment variable the credential travels in, per the provider's own
/// documented contract.
pub(crate) fn tunnel_token_env(provider: TunnelProvider) -> &'static str {
    match provider {
        TunnelProvider::Cloudflared => "TUNNEL_TOKEN",
    }
}

/// The loopback URL whose 200 means "connected to the provider's edge".
pub(crate) fn readiness_url(plan: &StartPlan) -> String {
    match plan.provider {
        TunnelProvider::Cloudflared => format!("http://127.0.0.1:{}/ready", plan.metrics_port),
    }
}

/// Bounded exponential backoff, so a tunnel that cannot start is not restarted
/// in a tight loop for the rest of the day.
pub(crate) fn backoff_ms(restarts: u32) -> u64 {
    BACKOFF_MIN_MS
        .saturating_mul(2u64.saturating_pow(restarts.min(6)))
        .min(BACKOFF_MAX_MS)
}

/// Kill a tunnel this daemon started and did not stop.
///
/// A daemon that was killed rather than asked to stop leaves its child running,
/// and the next start would put a second tunnel in front of the same listener.
/// The pid is only ever used to stop something, and only when this process
/// wrote it — a reused pid could name anything, so the worst case has to be
/// bounded by never using it to *decide* that a tunnel is healthy.
fn reap_orphan(store: &mut DaemonStore) {
    let Some(recorded) = store.get_meta(EXPOSURE_PID_KEY).ok().flatten() else {
        return;
    };
    let _ = store.set_meta(EXPOSURE_PID_KEY, "");
    let Ok(pid) = recorded.trim().parse::<u32>() else {
        return;
    };
    if pid == 0 {
        return;
    }
    kill_pid(pid);
}

#[cfg(unix)]
fn kill_pid(pid: u32) {
    // SAFETY: `kill` with a pid and a signal number is the documented POSIX
    // interface and cannot invalidate memory in this process. A pid that no
    // longer exists returns ESRCH, which is the outcome we want anyway.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(windows)]
fn kill_pid(pid: u32) {
    // No signals; `taskkill` is the supported way to end another process tree,
    // and a pid that no longer exists is a non-zero exit we ignore.
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Run the operator's tunnel for as long as the daemon lives.
///
/// One task, one child at a time. Everything it decides is written back to
/// `daemon_meta`, because the things that read it — the CLI, Security Doctor,
/// the desktop bridge — are other processes.
pub(crate) fn spawn_supervisor(paths: DaemonPaths) {
    tokio::spawn(async move {
        // Before anything else: a tunnel left behind by a daemon that did not
        // exit cleanly.
        if let Ok(mut store) = DaemonStore::open(&paths) {
            reap_orphan(&mut store);
        }
        let secrets = super::channel_adapter::KeyringChannelSecrets;
        let mut restarts = 0u32;
        loop {
            let Ok(mut store) = DaemonStore::open(&paths) else {
                tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MAX_MS)).await;
                continue;
            };
            let config = read_config(&store);
            let plan = match config.readiness() {
                Ok(plan) => plan,
                Err(blocked) => {
                    // Manual exposure is the normal case and is not a fault, so
                    // it does not carry an error or count as a restart.
                    write_state(
                        &mut store,
                        &StoredState {
                            state: blocked,
                            last_error: None,
                            restarts: 0,
                            since_ms: None,
                        },
                    );
                    drop(store);
                    tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MAX_MS)).await;
                    continue;
                }
            };
            let token = match secrets.get(TUNNEL_CREDENTIAL_REF) {
                Ok(token) if !token.is_empty() => token,
                _ => {
                    write_state(
                        &mut store,
                        &StoredState {
                            state: ExposureState::CredentialMissing,
                            last_error: None,
                            restarts,
                            since_ms: None,
                        },
                    );
                    drop(store);
                    tokio::time::sleep(std::time::Duration::from_millis(BACKOFF_MAX_MS)).await;
                    continue;
                }
            };
            drop(store);

            let outcome = run_once(&paths, &plan, &token, restarts).await;
            restarts = restarts.saturating_add(1);
            if let Ok(mut store) = DaemonStore::open(&paths) {
                let state = if looks_like_bad_credential(&outcome) {
                    ExposureState::AuthenticationFailed
                } else {
                    ExposureState::Reconnecting
                };
                write_state(
                    &mut store,
                    &StoredState {
                        state,
                        last_error: Some(outcome),
                        restarts,
                        since_ms: super::now_ms().ok().and_then(|ms| i64::try_from(ms).ok()),
                    },
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms(restarts))).await;
        }
    });
}

/// Start the tunnel, watch it, and return the reason it stopped.
///
/// Returns only when the child is gone: the supervisor's whole job while it
/// lives is to keep the observed state honest.
async fn run_once(paths: &DaemonPaths, plan: &StartPlan, token: &str, restarts: u32) -> String {
    let mut command = tokio::process::Command::new(&plan.executable);
    command
        .args(tunnel_argv(plan))
        // The credential goes here and nowhere else. On the command line it
        // would be visible to every process on the machine through `ps`.
        .env(tunnel_token_env(plan.provider), token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        // A daemon that is dropped takes its tunnel with it. The pid recorded
        // below is what covers the case where it is killed instead.
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return format!("The tunnel client could not be started: {error}"),
    };
    if let (Ok(mut store), Some(pid)) = (DaemonStore::open(paths), child.id()) {
        let _ = store.set_meta(EXPOSURE_PID_KEY, &pid.to_string());
        write_state(
            &mut store,
            &StoredState {
                state: ExposureState::Connecting,
                last_error: None,
                restarts,
                since_ms: super::now_ms().ok().and_then(|ms| i64::try_from(ms).ok()),
            },
        );
    }
    let stderr = child.stderr.take();
    let readiness = readiness_url(plan);
    let started = std::time::Instant::now();
    let mut reported_ready = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let detail = drain_stderr(stderr, token).await;
                if let Ok(mut store) = DaemonStore::open(paths) {
                    let _ = store.set_meta(EXPOSURE_PID_KEY, "");
                }
                return if detail.is_empty() {
                    format!("The tunnel client exited ({status}).")
                } else {
                    detail
                };
            }
            Ok(None) => {}
            Err(error) => return format!("The tunnel client could not be watched: {error}"),
        }
        let ready = probe_ready(&readiness).await;
        let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let observed = match (ready, elapsed >= READY_GRACE_MS) {
            (true, _) => ExposureState::Connected,
            (false, false) => ExposureState::Connecting,
            (false, true) => ExposureState::Degraded,
        };
        if observed == ExposureState::Connected {
            reported_ready = true;
        }
        if let Ok(mut store) = DaemonStore::open(paths) {
            write_state(
                &mut store,
                &StoredState {
                    state: observed,
                    last_error: None,
                    // A tunnel that has been up counts as a fresh start, so a
                    // client that is merely reconnecting occasionally does not
                    // back off as if it had never worked.
                    restarts: if reported_ready { 0 } else { restarts },
                    since_ms: super::now_ms().ok().and_then(|ms| i64::try_from(ms).ok()),
                },
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;
    }
}

/// Ask the tunnel client's own loopback endpoint whether it has a live
/// connection.
///
/// A local, unauthenticated GET to a port this daemon told the child to open.
/// `reqwest` rather than the hardened egress client on purpose: that one exists
/// to stop a *remote* URL reaching somewhere it should not, and refuses
/// loopback for exactly that reason.
async fn probe_ready(url: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    else {
        return false;
    };
    matches!(client.get(url).send().await, Ok(response) if response.status().is_success())
}

/// Everything the child wrote to stderr, bounded and with the token removed.
async fn drain_stderr(stderr: Option<tokio::process::ChildStderr>, token: &str) -> String {
    use tokio::io::AsyncReadExt;
    let Some(mut stderr) = stderr else {
        return String::new();
    };
    let mut buffer = Vec::new();
    // Bounded read: a client that logs continuously must not be able to grow
    // this process's memory through its own diagnostics.
    let _ = tokio::io::AsyncReadExt::take(&mut stderr, 64 * 1024)
        .read_to_end(&mut buffer)
        .await;
    let text = String::from_utf8_lossy(&buffer);
    // The last few lines are the ones that say why it stopped.
    let tail: Vec<&str> = text.lines().rev().take(4).collect();
    redact_tunnel_error(&tail.into_iter().rev().collect::<Vec<_>>().join(" "), token)
}

/// Validate an operator-supplied hostname.
///
/// A bare host, no scheme, no path, no port: the base is composed as
/// `https://<hostname>` and a value carrying any of those would produce a URL
/// that does not match what the provider signs.
pub(crate) fn validate_hostname(hostname: &str) -> Result<String, String> {
    let trimmed = hostname.trim().trim_end_matches('.');
    if trimmed.is_empty() || trimmed.len() > 253 {
        return Err("A tunnel hostname must be between 1 and 253 characters.".to_string());
    }
    if trimmed.contains("://") || trimmed.contains('/') || trimmed.contains(':') {
        return Err(
            "Give the hostname on its own — no scheme, no port and no path. For example \
             monkey.example.com."
                .to_string(),
        );
    }
    if !trimmed.contains('.') {
        return Err(
            "A tunnel hostname must be fully qualified, like monkey.example.com.".to_string(),
        );
    }
    if !trimmed
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.'))
    {
        return Err("A tunnel hostname may contain only letters, digits, '-' and '.'.".to_string());
    }
    Ok(trimmed.to_ascii_lowercase())
}

/// Validate an operator-supplied path to a tunnel client.
pub(crate) fn validate_executable(path: &str) -> Result<String, String> {
    let candidate = Path::new(path.trim());
    if !candidate.is_absolute() {
        return Err(
            "Give the full path to the tunnel client, starting from the root of the filesystem. \
             A bare name is resolved against a PATH this app does not control, so it could name a \
             different program tomorrow."
                .to_string(),
        );
    }
    if !candidate.is_file() {
        return Err(format!(
            "There is no file at '{}'. Install the tunnel client and point this at it.",
            candidate.display()
        ));
    }
    Ok(candidate.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn managed(hostname: &str, executable: &str) -> ExposureConfig {
        ExposureConfig {
            mode: ExposureMode::ManagedTunnel,
            provider: Some(TunnelProvider::Cloudflared),
            hostname: Some(hostname.to_string()),
            executable: Some(executable.to_string()),
            metrics_port: None,
        }
    }

    /// A file that exists and is absolute, derived from the platform's own
    /// temp directory: a Unix-shaped literal is *relative* on Windows, and a
    /// path that is merely missing is creatable there.
    fn helper_fixture(_name: &str) -> PathBuf {
        let path = std::env::current_exe().expect("a test binary has a path");
        assert!(path.is_absolute() && path.is_file());
        path
    }

    #[test]
    fn manual_is_the_default_and_keeps_the_operators_own_url() {
        let config = ExposureConfig::default();
        assert_eq!(config.mode, ExposureMode::Manual);
        assert_eq!(
            config.public_base(Some("https://hooks.example.com")),
            Some("https://hooks.example.com".to_string())
        );
        // And nothing is started for it.
        assert_eq!(
            config.readiness().unwrap_err(),
            ExposureState::NotConfigured
        );
    }

    #[test]
    fn a_managed_base_comes_from_the_configured_hostname_and_never_from_the_process() {
        let config = managed("monkey.example.com", "/usr/local/bin/cloudflared");
        // The manual value is ignored in managed mode, and the base is https by
        // construction: a provider that signs the callback URL is verified
        // against this, so it may not depend on anything observable.
        assert_eq!(
            config.public_base(Some("https://stale.example.com")),
            Some("https://monkey.example.com".to_string())
        );
    }

    #[test]
    fn each_missing_piece_names_itself() {
        let helper = helper_fixture("ok");
        let mut config = managed("monkey.example.com", &helper.to_string_lossy());
        assert!(config.readiness().is_ok());

        config.hostname = None;
        assert_eq!(
            config.readiness().unwrap_err(),
            ExposureState::PublicUrlUnavailable
        );

        let mut config = managed("monkey.example.com", &helper.to_string_lossy());
        config.executable = None;
        assert_eq!(
            config.readiness().unwrap_err(),
            ExposureState::HelperMissing
        );

        // A relative path is refused even when a file of that name exists in
        // the working directory: it names whichever binary a PATH lookup finds.
        let mut config = managed("monkey.example.com", "cloudflared");
        config.metrics_port = Some(20_242);
        assert_eq!(
            config.readiness().unwrap_err(),
            ExposureState::HelperMissing
        );

        let absent = std::env::temp_dir().join(format!(
            "little-monkey-tunnel-absent-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        assert!(absent.is_absolute());
        let config = managed("monkey.example.com", &absent.to_string_lossy());
        assert_eq!(
            config.readiness().unwrap_err(),
            ExposureState::HelperMissing
        );
    }

    #[test]
    fn the_credential_never_reaches_the_command_line() {
        let helper = helper_fixture("argv");
        let plan = managed("monkey.example.com", &helper.to_string_lossy())
            .readiness()
            .expect("ready");
        let argv = tunnel_argv(&plan);
        assert_eq!(
            argv,
            vec![
                "tunnel".to_string(),
                "--no-autoupdate".to_string(),
                "--metrics".to_string(),
                format!("127.0.0.1:{DEFAULT_METRICS_PORT}"),
                "run".to_string(),
            ]
        );
        // The token's home is the environment, which is what the provider's own
        // documentation recommends so it stays out of `ps`.
        assert_eq!(tunnel_token_env(plan.provider), "TUNNEL_TOKEN");
        assert!(
            !argv.iter().any(|argument| argument.contains("token")),
            "no argv slot exists for a credential to be put in by mistake"
        );
    }

    #[test]
    fn a_tunnels_own_output_never_carries_its_token_onwards() {
        let redacted = redact_tunnel_error(
            "  failed to connect with token eyJhIjoic2VjcmV0In0 to the edge  ",
            "eyJhIjoic2VjcmV0In0",
        );
        assert!(!redacted.contains("eyJhIjoic2VjcmV0In0"), "{redacted}");
        assert!(redacted.contains("[redacted]"), "{redacted}");
        // Bounded, so a client that writes a novel does not put one in the
        // database.
        let long = redact_tunnel_error(&"x".repeat(5_000), "");
        assert!(long.chars().count() <= MAX_TUNNEL_ERROR_CHARS + 1);
    }

    #[test]
    fn only_a_failure_that_names_the_credential_is_reported_as_one() {
        assert!(looks_like_bad_credential(
            "Failed to parse token: invalid base64"
        ));
        assert!(looks_like_bad_credential("error 401 Unauthorized"));
        // A network problem is not somebody's token being wrong, and telling
        // them it is sends them to rotate a working secret.
        assert!(!looks_like_bad_credential(
            "dial tcp: lookup region1.example: no such host"
        ));
    }

    #[test]
    fn backoff_is_bounded_at_both_ends() {
        assert_eq!(backoff_ms(0), BACKOFF_MIN_MS);
        assert!(backoff_ms(1) > backoff_ms(0));
        assert_eq!(backoff_ms(99), BACKOFF_MAX_MS);
    }

    #[test]
    fn a_hostname_is_a_hostname_and_not_a_url() {
        assert_eq!(
            validate_hostname("  Monkey.Example.COM. "),
            Ok("monkey.example.com".to_string())
        );
        for bad in [
            "https://monkey.example.com",
            "monkey.example.com/hooks",
            "monkey.example.com:8443",
            "localhost",
            "",
        ] {
            assert!(validate_hostname(bad).is_err(), "accepted '{bad}'");
        }
    }

    #[test]
    fn readiness_is_asked_of_the_client_on_loopback() {
        let helper = helper_fixture("ready");
        let plan = managed("monkey.example.com", &helper.to_string_lossy())
            .readiness()
            .expect("ready");
        assert_eq!(
            readiness_url(&plan),
            format!("http://127.0.0.1:{DEFAULT_METRICS_PORT}/ready")
        );
    }

    /// A tunnel client that really exists and really runs: this test binary.
    ///
    /// Deliberately not a shell script in a temp directory. That needs an
    /// executable bit, a shebang, a per-platform spelling, and a `/tmp` that is
    /// not mounted `noexec` — four ways for the *fixture* to decide the result
    /// of a test about production code. The test executable is absolute,
    /// present and executable on every platform this ships to, and handed the
    /// production argv it exits non-zero without doing anything.
    ///
    /// What is under test is the spawn path, not the child: that the configured
    /// executable is what runs, that its failure comes back as a reason, and
    /// that the credential does not travel into anything durable.
    fn real_client() -> PathBuf {
        let path = std::env::current_exe().expect("a test binary has a path");
        assert!(path.is_absolute());
        path
    }

    fn test_paths() -> (PathBuf, DaemonPaths) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-exposure-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("root");
        let paths = DaemonPaths::under(&root);
        (root, paths)
    }

    /// The real spawn path, against a real child, proving the three things a
    /// unit test on a pure function cannot: that the configured executable is
    /// what runs, that its failure is captured, and that its credential does
    /// not survive into anything durable.
    #[tokio::test]
    async fn a_tunnel_that_fails_is_reported_with_its_own_reason_and_not_its_token() {
        let (root, paths) = test_paths();
        let client = real_client();
        let plan = managed("monkey.example.com", &client.to_string_lossy())
            .readiness()
            .expect("ready");

        let reason = run_once(&paths, &plan, "s3cret-tunnel-token", 0).await;
        assert!(
            !reason.is_empty(),
            "a tunnel that stops must come back as something an operator can read"
        );
        // The one thing that must never survive: the credential. It went to the
        // child in its environment, and the only text kept from the child is
        // its own output with the token substituted out.
        assert!(!reason.contains("s3cret-tunnel-token"), "{reason}");

        // And nothing durable kept the pid of a process that has gone.
        let store = DaemonStore::open(&paths).expect("store");
        assert_eq!(
            store.get_meta(EXPOSURE_PID_KEY).expect("meta"),
            Some(String::new())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Readiness is a real HTTP question asked of a real socket.
    ///
    /// The tunnel client's `/ready` is the only thing that turns "we started a
    /// process" into "a provider would reach this machine", so the probe has to
    /// be exercised against something that answers and something that does not.
    #[tokio::test]
    async fn readiness_is_true_only_when_something_answers_on_loopback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        // A single-shot server, deliberately not the hardened egress client's
        // idea of a safe host: loopback is exactly where this one must work.
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 1024];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
                let _ = stream.flush();
            }
        });
        assert!(probe_ready(&format!("http://127.0.0.1:{port}/ready")).await);

        // A port nobody is listening on is the state a dead tunnel leaves, and
        // it must not read as connected.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let dead_port = closed.local_addr().expect("addr").port();
        drop(closed);
        assert!(!probe_ready(&format!("http://127.0.0.1:{dead_port}/ready")).await);
    }

    /// What survives a restart: the configuration, and therefore the URL.
    ///
    /// The tunnel process does not survive one and is not supposed to — what
    /// must survive is the public base a provider was given and a signature
    /// will be verified against. Written through a real store and read back
    /// through a reopened one, because "the daemon restarted" is precisely a
    /// second `DaemonStore::open`.
    #[test]
    fn a_restart_keeps_the_configured_public_base_and_forgets_the_process() {
        let (root, paths) = test_paths();
        let client = real_client();
        {
            let mut store = DaemonStore::open(&paths).expect("store");
            write_config(
                &mut store,
                &managed("monkey.example.com", &client.to_string_lossy()),
            )
            .expect("config");
            // A daemon that was killed leaves this behind.
            store.set_meta(EXPOSURE_PID_KEY, "999999").expect("pid");
        }

        let mut store = DaemonStore::open(&paths).expect("reopen");
        assert_eq!(
            read_config(&store).public_base(None),
            Some("https://monkey.example.com".to_string()),
            "the URL a provider console holds does not depend on a process being up"
        );
        // And the orphan is cleared exactly once, so the next start does not
        // try to kill a pid that has since been reused by something else.
        reap_orphan(&mut store);
        assert_eq!(
            store.get_meta(EXPOSURE_PID_KEY).expect("meta"),
            Some(String::new())
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A configuration problem always beats a stale success.
    ///
    /// The daemon writes what it last saw; a daemon that is not running writes
    /// nothing. Reporting the last thing it wrote would mean a settings page
    /// showing "connected" for a tunnel whose supervisor died with it, which is
    /// the fake status this project refuses to render.
    #[test]
    fn a_status_read_never_reports_a_success_the_configuration_contradicts() {
        let (root, paths) = test_paths();
        let mut store = DaemonStore::open(&paths).expect("store");
        let secrets = super::super::channel_adapter::MemoryChannelSecrets::default();

        write_config(&mut store, &managed("monkey.example.com", "cloudflared")).expect("config");
        write_state(
            &mut store,
            &StoredState {
                state: ExposureState::Connected,
                last_error: None,
                restarts: 0,
                since_ms: Some(1),
            },
        );
        // The executable is relative, so it can never have been what ran.
        assert_eq!(
            status(&store, &secrets).state,
            ExposureState::HelperMissing,
            "a stored 'connected' must not outlive the configuration that made it possible"
        );

        // With a real helper but no credential, the missing token wins over the
        // stale success for the same reason.
        let client = real_client();
        write_config(
            &mut store,
            &managed("monkey.example.com", &client.to_string_lossy()),
        )
        .expect("config");
        let reported = status(&store, &secrets);
        assert_eq!(reported.state, ExposureState::CredentialMissing);
        assert!(!reported.credential_stored);
        // The status a front end receives never carries the secret, and there
        // is nowhere on it for one to go.
        let encoded = serde_json::to_string(&reported).expect("encode");
        assert!(!encoded.contains("token"), "{encoded}");

        secrets.put(TUNNEL_CREDENTIAL_REF, "s3cret").expect("store");
        assert_eq!(status(&store, &secrets).state, ExposureState::Connected);
        assert!(status(&store, &secrets).credential_stored);
        let encoded = serde_json::to_string(&status(&store, &secrets)).expect("encode");
        assert!(!encoded.contains("s3cret"), "{encoded}");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Switching to a managed tunnel and back does not lose what was typed.
    #[test]
    fn the_manual_url_is_kept_while_a_tunnel_is_in_force_and_comes_back_after() {
        let (root, paths) = test_paths();
        let mut store = DaemonStore::open(&paths).expect("store");
        store
            .set_channel_public_base_url(Some("https://hooks.example.com"))
            .expect("manual");
        assert_eq!(
            store
                .channel_public_base_url()
                .expect("resolved")
                .as_deref(),
            Some("https://hooks.example.com")
        );

        write_config(&mut store, &managed("monkey.example.com", "cloudflared")).expect("config");
        assert_eq!(
            store
                .channel_public_base_url()
                .expect("resolved")
                .as_deref(),
            Some("https://monkey.example.com"),
            "the tunnel's hostname is what providers are given while it is selected"
        );
        assert_eq!(
            store
                .channel_public_base_url_manual()
                .expect("manual")
                .as_deref(),
            Some("https://hooks.example.com"),
            "and what the operator typed is still there"
        );

        let mut config = read_config(&store);
        config.mode = ExposureMode::Manual;
        write_config(&mut store, &config).expect("config");
        assert_eq!(
            store
                .channel_public_base_url()
                .expect("resolved")
                .as_deref(),
            Some("https://hooks.example.com")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_connected_means_a_provider_would_reach_this_machine() {
        assert!(ExposureState::Connected.is_reachable());
        for state in [
            ExposureState::NotConfigured,
            ExposureState::HelperMissing,
            ExposureState::CredentialMissing,
            ExposureState::Connecting,
            ExposureState::Degraded,
            ExposureState::Reconnecting,
            ExposureState::AuthenticationFailed,
            ExposureState::PublicUrlUnavailable,
            ExposureState::Stopped,
        ] {
            assert!(!state.is_reachable(), "{}", state.as_str());
        }
    }
}

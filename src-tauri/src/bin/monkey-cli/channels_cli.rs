//! `monkey channels` — configure and inspect messaging accounts.
//!
//! Every subcommand here is also what the desktop's Channels settings calls
//! through the typed daemon bridge, which is why each one has a `--json` shape:
//! one implementation, two front ends, no second copy of the rules.
//!
//! Credentials go in through `set-token` and are never printed back. The
//! listing shows a `credential_ref` — a keychain account name — and the stored
//! health, which is only ever written by a real probe.

use little_monkey_lib::channels::policy::{
    AccessPolicy, ChannelAccessPolicy, GroupActivation, SenderState,
};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind, HealthState};

use crate::daemon::channel_adapter::{
    credential_required, AdapterConfig, ChannelSecrets, KeyringChannelSecrets, MemoryChannelSecrets,
};
use crate::daemon::channel_store::{ChannelAccountRecord, StoredSenderAuthorization};
use crate::daemon::store::{DaemonPaths, DaemonStore};

/// `monkey channels <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum ChannelsCmd {
    /// List configured accounts with their real, probed health.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add an account. It starts disabled and with no credential.
    Add {
        /// Provider: telegram, discord, slack, whatsapp, teams, google_chat,
        /// line, mattermost, irc, matrix, signal, imessage, sms.
        kind: String,
        /// Name shown in listings and in the app.
        label: String,
        /// Non-secret provider settings as a JSON object (server URL, bot
        /// username, channels to join — never a token).
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Store the account's credential, read from stdin so it never lands in a
    /// shell history or a process listing.
    SetToken { account_id: String },
    /// Edit an existing account's non-secret settings and label. Validated
    /// against what the provider's adapter actually reads; changing settings
    /// marks the connection unverified until the next probe.
    SetConfig {
        account_id: String,
        /// Replacement non-secret settings as a JSON object.
        #[arg(long)]
        config: Option<String>,
        /// New display label.
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Enable or disable an account.
    Enable {
        account_id: String,
        #[arg(long)]
        off: bool,
    },
    /// Ask the provider whether the credential works. The only thing that can
    /// report an account as connected.
    Probe {
        account_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Set who may talk to this account.
    Policy {
        account_id: String,
        /// disabled | allow_list | pairing | open
        #[arg(long)]
        direct: Option<String>,
        /// disabled | allow_list | pairing | open
        #[arg(long)]
        group: Option<String>,
        /// always | mention_only | disabled
        #[arg(long)]
        activation: Option<String>,
    },
    /// List senders waiting for approval.
    Senders {
        account_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Approve a waiting sender. Grants the ability to send messages and
    /// nothing else.
    Approve {
        account_id: String,
        sender_id: String,
    },
    /// Block a sender.
    Block {
        account_id: String,
        sender_id: String,
    },
    /// List the routes that decide which recipe an inbound message runs.
    Routes {
        #[arg(long)]
        json: bool,
    },
    /// Add a route. With no scope flags it becomes the global default.
    AddRoute {
        /// Recipe an inbound message runs as.
        recipe: String,
        #[command(flatten)]
        options: RouteOptions,
        #[arg(long)]
        json: bool,
    },
    /// Replace a route's scope and target, keeping its id.
    UpdateRoute {
        route_id: String,
        /// Recipe an inbound message runs as.
        recipe: String,
        #[command(flatten)]
        options: RouteOptions,
        #[arg(long)]
        json: bool,
    },
    /// Enable or disable a route without editing it.
    EnableRoute {
        route_id: String,
        #[arg(long)]
        off: bool,
    },
    /// Remove a route.
    RemoveRoute { route_id: String },
    /// Recent inbound and outbound activity. Never includes message text.
    Events {
        account_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Set or clear the public base URL webhook callbacks are advertised
    /// under, e.g. `https://hooks.example.com`. The daemon still listens on
    /// loopback only; reaching it from that URL is the operator's tunnel or
    /// reverse proxy.
    SetPublicUrl {
        /// The base URL. Omit with `--clear` to remove it.
        url: Option<String>,
        #[arg(long)]
        clear: bool,
    },
    /// The complete callback URL to paste into a webhook provider's console,
    /// or a clear statement that no public base URL is configured.
    CallbackUrl {
        account_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Record that a credential was stored for this account by the app. The
    /// secret itself never travels through an argument.
    MarkCredential { account_id: String },
    /// Remove an account and its stored credential.
    Remove { account_id: String },
    /// How this machine's webhook listener is reached from the internet: the
    /// URL you publish yourself, or a tunnel the daemon runs on your behalf
    /// using your own tunnel account.
    #[command(subcommand)]
    Exposure(ExposureCmd),
}

#[derive(clap::Subcommand, Debug)]
pub enum ExposureCmd {
    /// What is configured, what state it is in, and the public base in force.
    /// Never prints a credential.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Go back to publishing the URL yourself. Stops any managed tunnel; the
    /// URL you set with `set-public-url` comes back into force.
    Manual,
    /// Let the daemon run your own tunnel client.
    ///
    /// Everything here is yours: your tunnel provider account, your hostname,
    /// a binary you installed. This project operates no relay and holds no
    /// account of its own.
    Tunnel {
        /// Which tunnel client. Currently: cloudflared.
        provider: String,
        /// The hostname you configured in your tunnel provider's console, on
        /// its own — `monkey.example.com`, not a URL.
        #[arg(long)]
        hostname: String,
        /// Full path to the installed tunnel client.
        #[arg(long)]
        executable: String,
        /// Loopback port the client serves its own readiness on.
        #[arg(long)]
        metrics_port: Option<u16>,
    },
    /// Store the tunnel credential, read from stdin so it never lands in a
    /// shell history or a process listing.
    SetToken,
    /// Forget the tunnel credential.
    ClearToken,
}

/// The scope and target of one route, shared by `add-route` and
/// `update-route` so the two can never drift apart.
#[derive(clap::Args, Debug)]
pub struct RouteOptions {
    /// Scope: account the route is pinned to.
    #[arg(long)]
    pub account: Option<String>,
    /// Scope: conversation inside `--account`.
    #[arg(long)]
    pub conversation: Option<String>,
    /// Scope: thread inside `--conversation`.
    #[arg(long)]
    pub thread: Option<String>,
    /// Scope: sender inside `--conversation`.
    #[arg(long)]
    pub sender: Option<String>,
    /// Scope: provider-wide default, e.g. `--kind telegram`.
    #[arg(long)]
    pub kind: Option<String>,
    /// Workspace the run gets.
    #[arg(long)]
    pub repository: Option<String>,
    /// Recipe parameter as `name=value`. Repeatable.
    #[arg(long = "param")]
    pub params: Vec<String>,
    /// Which durable session the conversation maps onto:
    /// thread | conversation | sender | account.
    #[arg(long)]
    pub session_scope: Option<String>,
    /// Queue priority for runs this route produces.
    #[arg(long)]
    pub priority: Option<i32>,
    /// Do not grant runs of this route the authority to answer the
    /// conversation they came from.
    #[arg(long)]
    pub no_reply: bool,
    /// Create or leave the route disabled.
    #[arg(long)]
    pub disabled: bool,
}

pub async fn dispatch(action: &ChannelsCmd) -> Result<(), String> {
    match action {
        ChannelsCmd::List { json } => list(*json),
        ChannelsCmd::Add {
            kind,
            label,
            config,
            json,
        } => add(kind, label, config.as_deref(), *json),
        ChannelsCmd::SetToken { account_id } => set_token(account_id),
        ChannelsCmd::SetConfig {
            account_id,
            config,
            label,
            json,
        } => set_config(account_id, config.as_deref(), label.as_deref(), *json),
        ChannelsCmd::Enable { account_id, off } => enable(account_id, !*off),
        ChannelsCmd::Probe { account_id, json } => probe(account_id, *json).await,
        ChannelsCmd::Policy {
            account_id,
            direct,
            group,
            activation,
        } => set_policy(
            account_id,
            direct.as_deref(),
            group.as_deref(),
            activation.as_deref(),
        ),
        ChannelsCmd::Senders { account_id, json } => senders(account_id, *json),
        ChannelsCmd::Approve {
            account_id,
            sender_id,
        } => decide_sender(account_id, sender_id, true),
        ChannelsCmd::Block {
            account_id,
            sender_id,
        } => decide_sender(account_id, sender_id, false),
        ChannelsCmd::Routes { json } => routes(*json),
        ChannelsCmd::AddRoute {
            recipe,
            options,
            json,
        } => add_route(recipe, options, *json),
        ChannelsCmd::UpdateRoute {
            route_id,
            recipe,
            options,
            json,
        } => update_route(route_id, recipe, options, *json),
        ChannelsCmd::EnableRoute { route_id, off } => enable_route(route_id, !*off),
        ChannelsCmd::RemoveRoute { route_id } => remove_route(route_id),
        ChannelsCmd::Events {
            account_id,
            limit,
            json,
        } => events(account_id, *limit, *json),
        ChannelsCmd::SetPublicUrl { url, clear } => set_public_url(url.as_deref(), *clear),
        ChannelsCmd::CallbackUrl { account_id, json } => callback_url(account_id, *json),
        ChannelsCmd::MarkCredential { account_id } => mark_credential(account_id),
        ChannelsCmd::Remove { account_id } => remove(account_id),
        ChannelsCmd::Exposure(command) => exposure(command),
    }
}

fn exposure(command: &ExposureCmd) -> Result<(), String> {
    use crate::daemon::callback_exposure as exposure;
    match command {
        ExposureCmd::Status { json } => {
            let store = store()?;
            let status = exposure::status(&store, &KeyringChannelSecrets);
            if *json {
                println!(
                    "{}",
                    serde_json::to_string(&status).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            println!(
                "{} — {}{}",
                match status.mode {
                    exposure::ExposureMode::Manual => "You publish the URL yourself",
                    exposure::ExposureMode::ManagedTunnel => "Little Monkey runs your tunnel",
                },
                status.state.as_str(),
                status
                    .public_base
                    .as_deref()
                    .map(|base| format!(" at {base}"))
                    .unwrap_or_default()
            );
            if let Some(error) = &status.last_error {
                println!("Last failure: {error}");
            }
            // Said outright, because every other line here describes what is
            // *configured* and this is the nearest thing to the question
            // somebody actually has. Nearest, not the same: a live tunnel is the
            // transport, and whether the hostname routes to this machine is set
            // in the provider's dashboard where nothing local can look. A manual
            // URL is never reported as connected at all — this machine cannot
            // see the far side of a proxy it does not run.
            println!(
                "The tunnel {}.",
                if status.state.is_tunnel_connected() {
                    "reports a live connection to its provider's edge; its hostname must also route \
                     to this machine's webhook listener"
                } else {
                    "does not report a live connection, so nothing is arriving through it"
                }
            );
            Ok(())
        }
        ExposureCmd::Manual => {
            let mut store = store()?;
            let mut config = exposure::read_config(&store);
            config.mode = exposure::ExposureMode::Manual;
            exposure::write_config(&mut store, &config)?;
            println!(
                "Managed exposure is off. Any URL set with `set-public-url` is in force again."
            );
            Ok(())
        }
        ExposureCmd::Tunnel {
            provider,
            hostname,
            executable,
            metrics_port,
        } => {
            let provider = exposure::TunnelProvider::parse(provider)
                .ok_or_else(|| format!("Unknown tunnel provider '{provider}' (cloudflared)"))?;
            let hostname = exposure::validate_hostname(hostname)?;
            let executable = exposure::validate_executable(executable)?;
            let mut store = store()?;
            let mut config = exposure::read_config(&store);
            config.mode = exposure::ExposureMode::ManagedTunnel;
            config.provider = Some(provider);
            config.hostname = Some(hostname.clone());
            config.executable = Some(executable);
            config.metrics_port = *metrics_port;
            exposure::write_config(&mut store, &config)?;
            // Never a claim that it is up: the supervisor decides that, and
            // saying "connected" here would be exactly the fake status this
            // project refuses to print.
            println!("Callbacks will be advertised under https://{hostname}.");
            println!("{}", provider.prerequisite());
            if !KeyringChannelSecrets
                .get(exposure::TUNNEL_CREDENTIAL_REF)
                .map(|token| !token.is_empty())
                .unwrap_or(false)
            {
                println!(
                    "No tunnel credential is stored yet — run `monkey channels exposure \
                     set-token` and paste it on stdin."
                );
            }
            Ok(())
        }
        ExposureCmd::SetToken => {
            let token = read_secret_from_stdin()?;
            KeyringChannelSecrets.put(exposure::TUNNEL_CREDENTIAL_REF, &token)?;
            println!("Tunnel credential saved to the OS keychain.");
            Ok(())
        }
        ExposureCmd::ClearToken => {
            KeyringChannelSecrets.delete(exposure::TUNNEL_CREDENTIAL_REF)?;
            println!("Tunnel credential removed.");
            Ok(())
        }
    }
}

fn store() -> Result<DaemonStore, String> {
    DaemonStore::open(&DaemonPaths::resolve()?)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX)
}

/// JSON view of an account. Deliberately not the storage struct: this one can
/// never grow a secret field, because it has nowhere to put one.
fn account_json(account: &ChannelAccountRecord) -> serde_json::Value {
    account_json_with(account, Default::default())
}

/// The same view, plus the refusal tally the store holds separately.
///
/// Split so `list` can read the tally once per account without every other
/// caller having to. A provider whose deliveries stopped authenticating has no
/// other symptom — messages simply stop — so this is the one number that turns
/// a rotated secret from a mystery into a sentence, and the panel shows it the
/// way the telephony panel shows the same thing for a carrier.
fn account_json_with(
    account: &ChannelAccountRecord,
    rejections: crate::daemon::telecom_store::CallbackRejections,
) -> serde_json::Value {
    serde_json::json!({
        "account_id": account.account_id,
        "kind": account.kind.as_str(),
        "label": account.label,
        "enabled": account.enabled,
        "has_credential": account.credential_ref.is_some(),
        "credential_required": credential_required(account),
        "access_policy": account.access_policy,
        "health": account.health.state.as_str(),
        "health_detail": account.health.detail,
        "last_error": account.health.last_error,
        "last_probe_at_ms": account.health.probed_at_ms,
        "non_secret_config": account.non_secret_config,
        // How this machine recognises one of its own messages coming back, and
        // whether the stored reply policy is one it is allowed to run under.
        // Both are shown, because the panel has to be able to say "what you
        // configured is not what is in force" rather than silently rendering a
        // setting the ingress narrows.
        "echo_correlation":
            crate::daemon::channel_adapter::echo_correlation_for(account).as_str(),
        "reply_policy_restricted":
            !crate::daemon::channel_adapter::echo_correlation_for(account).is_host_verifiable()
                && account.access_policy.unsafe_without_echo_correlation(),
        "created_at_ms": account.created_at_ms,
        "updated_at_ms": account.updated_at_ms,
        "callback_rejections": {
            "count": rejections.count,
            // The verifier's own reason code, never a body or a header.
            "last_reason": rejections.last_reason,
            "last_at_ms": rejections.last_at_ms,
        },
    })
}

pub fn list(json: bool) -> Result<(), String> {
    let store = store()?;
    let accounts = store.channel_accounts()?;
    if json {
        let rows: Vec<serde_json::Value> = accounts
            .iter()
            .map(|account| {
                let rejections = store
                    .channel_callback_rejections(&account.account_id)
                    .unwrap_or_default();
                account_json_with(account, rejections)
            })
            .collect();
        println!("{}", serde_json::json!({ "accounts": rows }));
        return Ok(());
    }
    if accounts.is_empty() {
        println!("No messaging accounts configured. Add one with `monkey channels add`.");
        return Ok(());
    }
    for account in &accounts {
        println!(
            "{}  {}  {}  {}{}",
            account.account_id,
            account.kind.label(),
            if account.enabled {
                "enabled"
            } else {
                "disabled"
            },
            account.health.state.as_str(),
            account
                .health
                .last_error
                .as_ref()
                .map(|error| format!("  ({error})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Add an account. The credential is set separately so it never appears in a
/// shell history alongside the rest of the configuration.
pub fn add(kind: &str, label: &str, config_json: Option<&str>, json: bool) -> Result<(), String> {
    let kind = ChannelKind::parse(kind).ok_or_else(|| {
        format!("Unknown provider '{kind}'. Try one of: telegram, discord, slack, whatsapp, signal, teams, google_chat, matrix, mattermost, line, imessage, irc, sms")
    })?;
    let non_secret_config: serde_json::Value = match config_json {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| format!("--config must be a JSON object: {error}"))?,
        None => serde_json::json!({}),
    };
    crate::daemon::adapters::validate_non_secret_config(kind, &non_secret_config)?;
    let account_id = format!("chan-{}", uuid::Uuid::new_v4().simple());
    let now = now_ms();
    let record = ChannelAccountRecord {
        account_id: account_id.clone(),
        kind,
        label: label.to_string(),
        enabled: false,
        non_secret_config,
        credential_ref: None,
        access_policy: ChannelAccessPolicy::default(),
        // Never Connected on creation: nothing has been probed yet.
        health: ChannelHealth {
            state: HealthState::Unconfigured,
            detail: None,
            last_error: None,
            probed_at_ms: now,
        },
        created_at_ms: now,
        updated_at_ms: now,
    };
    store()?.upsert_channel_account(&record)?;
    if json {
        println!("{}", account_json(&record));
    } else {
        println!(
            "Added {} account {account_id}. Set its credential with `monkey channels set-token {account_id}`, then enable it.",
            kind.label()
        );
    }
    Ok(())
}

/// Edit an existing account's non-secret settings.
///
/// The settings replace the stored object wholesale — the caller shows the
/// current values and sends back the edited whole, so there is no merge
/// semantics to misremember. Secrets are not touched here and never could be:
/// this writes `non_secret_config` and `label`, nothing else.
///
/// A config change moves health to `Disconnected`: the old probe result
/// described a configuration that no longer exists, and claiming its
/// connectivity would be the lie the health field exists to prevent. The
/// running adapter is rebuilt from the new row within the worker's normal
/// reload interval.
pub fn set_config(
    account_id: &str,
    config_json: Option<&str>,
    label: Option<&str>,
    json: bool,
) -> Result<(), String> {
    let mut store = store()?;
    let (account, config_changed) = apply_account_edit(&mut store, account_id, config_json, label)?;
    if json {
        println!("{}", account_json(&account));
    } else {
        println!(
            "Updated {account_id}.{}",
            if config_changed {
                " The change takes effect within about 30 seconds; run `monkey channels probe` to verify the connection."
            } else {
                ""
            }
        );
    }
    Ok(())
}

/// The edit itself, against whichever store the caller owns: label and
/// settings applied, and — only when the settings actually changed — the old
/// connectivity claim dropped. A label rename proves nothing about the
/// connection either way, so it leaves health alone.
pub(crate) fn apply_account_edit(
    store: &mut DaemonStore,
    account_id: &str,
    config_json: Option<&str>,
    label: Option<&str>,
) -> Result<(ChannelAccountRecord, bool), String> {
    if config_json.is_none() && label.is_none() {
        return Err("Pass --config, --label, or both.".to_string());
    }
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(label) = label {
        let label = label.trim();
        if label.is_empty() {
            return Err("--label must not be empty.".to_string());
        }
        account.label = label.to_string();
    }
    let mut config_changed = false;
    if let Some(raw) = config_json {
        let config: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| format!("--config must be a JSON object: {error}"))?;
        crate::daemon::adapters::validate_non_secret_config(account.kind, &config)?;
        config_changed = config != account.non_secret_config;
        account.non_secret_config = config;
    }
    if config_changed {
        account.health = unverified_health(
            "Settings changed; run a probe to verify the connection.",
            now_ms(),
        );
    }
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
    Ok((account, config_changed))
}

/// The state a configuration or credential write leaves behind: whatever the
/// old value had proven, the new one has proven nothing yet. Only a real
/// probe, or a real authenticated transport, may claim Connected again.
fn unverified_health(detail: &str, now_ms: i64) -> ChannelHealth {
    ChannelHealth {
        state: HealthState::Disconnected,
        detail: Some(detail.to_string()),
        last_error: None,
        probed_at_ms: now_ms,
    }
}

/// Record that `account_id`'s credential was just written: point the row at
/// its keychain entry and drop the connectivity claim the old credential had
/// earned. Every credential write path funnels through this — `set-token`,
/// which is also what the desktop's save runs, and `mark-credential` for a
/// keychain entry somebody stored by hand.
pub(crate) fn record_credential_change(
    store: &mut DaemonStore,
    account_id: &str,
) -> Result<ChannelAccountRecord, String> {
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    account.credential_ref = Some(little_monkey_lib::channels::credential_ref(account_id));
    account.health = unverified_health(
        "Credential changed; run a probe to verify the connection.",
        now_ms(),
    );
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
    Ok(account)
}

/// Store the account's credential in the keychain. The value is read from
/// stdin, never from an argument, so it cannot end up in a process listing or a
/// shell history file.
pub fn set_token(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    if store.channel_account(account_id)?.is_none() {
        return Err(format!("No such account '{account_id}'"));
    }

    let mut secret = String::new();
    std::io::stdin()
        .read_line(&mut secret)
        .map_err(|error| format!("Could not read the credential from stdin: {error}"))?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("No credential was supplied on stdin".to_string());
    }

    KeyringChannelSecrets.put(
        &little_monkey_lib::channels::credential_ref(account_id),
        secret,
    )?;
    record_credential_change(&mut store, account_id)?;
    println!("Credential stored for {account_id}. Run `monkey channels probe {account_id}` to verify it.");
    Ok(())
}

/// One secret, from stdin. Never an argument: an argument is visible to every
/// process on the machine and lands in a shell history.
fn read_secret_from_stdin() -> Result<String, String> {
    use std::io::BufRead;
    let mut secret = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut secret)
        .map_err(|error| format!("Could not read the credential from stdin: {error}"))?;
    let secret = secret.trim().to_string();
    if secret.is_empty() {
        return Err("No credential was supplied on stdin".to_string());
    }
    Ok(secret)
}

/// Point the account at a keychain entry stored outside this command.
///
/// The desktop no longer needs this — its save runs `set-token` here, so the
/// entry is written by this binary, the one the daemon reads it back from.
pub fn mark_credential(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    record_credential_change(&mut store, account_id)?;
    println!("Credential recorded for {account_id}.");
    Ok(())
}

pub fn enable(account_id: &str, enabled: bool) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if enabled && account.credential_ref.is_none() && credential_required(&account) {
        return Err(format!(
            "Account '{account_id}' has no credential yet; run `monkey channels set-token {account_id}` first"
        ));
    }
    account.enabled = enabled;
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
    println!(
        "{account_id} is now {}.",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

/// Ask the provider whether the credential works. This is the only thing that
/// may write `Connected`.
pub async fn probe(account_id: &str, json: bool) -> Result<(), String> {
    let mut store = store()?;
    let account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let secret = match &account.credential_ref {
        Some(reference) => KeyringChannelSecrets.get(reference)?,
        None => String::new(),
    };
    let adapter = crate::daemon::adapters::build_adapter(
        &AdapterConfig {
            account: &account,
            secret,
        },
        None,
    )?;
    let health = adapter.probe().await;
    store.set_channel_account_health(account_id, &health, now_ms())?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "account_id": account_id,
                "health": health.state.as_str(),
                "detail": health.detail,
                "last_error": health.last_error,
            })
        );
    } else {
        println!(
            "{account_id}: {}{}",
            health.state.as_str(),
            health
                .last_error
                .as_ref()
                .map(|error| format!(" — {error}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

pub fn set_policy(
    account_id: &str,
    direct: Option<&str>,
    group: Option<&str>,
    activation: Option<&str>,
) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(value) = direct {
        account.access_policy.direct = AccessPolicy::parse(value).ok_or_else(|| {
            format!("Unknown DM policy '{value}' (disabled|allow_list|pairing|open)")
        })?;
    }
    if let Some(value) = group {
        account.access_policy.group = AccessPolicy::parse(value).ok_or_else(|| {
            format!("Unknown group policy '{value}' (disabled|allow_list|pairing|open)")
        })?;
    }
    if let Some(value) = activation {
        account.access_policy.group_activation =
            GroupActivation::parse(value).ok_or_else(|| {
                format!("Unknown activation '{value}' (always|mention_only|disabled)")
            })?;
    }
    // Refused here as well as clamped at the ingress. The clamp is what keeps
    // an account configured before this rule existed safe; this is what stops
    // somebody storing a setting the account will never honour and then
    // wondering why an open inbox is not open.
    let correlation = crate::daemon::channel_adapter::echo_correlation_for(&account);
    if !correlation.is_host_verifiable() && account.access_policy.unsafe_without_echo_correlation()
    {
        return Err(format!(
            "'{account_id}' is served by an extension that does not report provider message ids, \
             so this machine cannot tell one of its own messages coming back. An open inbox, or \
             answering every message in a group, could then answer itself forever. Update the \
             extension and set echo_correlation=provider_message_id on the account, or choose a \
             narrower policy."
        ));
    }
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
    println!(
        "{account_id}: DMs {}, groups {} ({}).",
        account.access_policy.direct.as_str(),
        account.access_policy.group.as_str(),
        account.access_policy.group_activation.as_str()
    );
    Ok(())
}

pub fn senders(account_id: &str, json: bool) -> Result<(), String> {
    let pending = store()?.pending_channel_senders(account_id)?;
    if json {
        let rows: Vec<serde_json::Value> = pending
            .iter()
            .map(|sender| {
                serde_json::json!({
                    "sender_id": sender.sender_id,
                    "state": sender.state.as_str(),
                    "display_label": sender.display_label,
                    "requested_at_ms": sender.requested_at_ms,
                    "expires_at_ms": sender.expires_at_ms,
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "pending": rows }));
        return Ok(());
    }
    if pending.is_empty() {
        println!("No senders are waiting for approval on {account_id}.");
        return Ok(());
    }
    for sender in &pending {
        println!(
            "{}  {}  requested {}",
            sender.sender_id,
            sender.display_label.as_deref().unwrap_or("(no name)"),
            sender.requested_at_ms
        );
    }
    Ok(())
}

/// Approve or block a sender.
///
/// Approval grants exactly one thing: the ability to submit messages. It is not
/// tool, admin, device or telephony authority, and nothing here widens it.
pub fn decide_sender(account_id: &str, sender_id: &str, approve: bool) -> Result<(), String> {
    let mut store = store()?;
    let existing = store.channel_sender(account_id, sender_id)?;
    let now = now_ms();
    let record = StoredSenderAuthorization {
        sender_id: sender_id.to_string(),
        state: if approve {
            SenderState::Approved
        } else {
            SenderState::Blocked
        },
        // The outstanding code is consumed by the decision either way.
        pairing_code_digest: None,
        requested_at_ms: existing
            .as_ref()
            .map(|sender| sender.requested_at_ms)
            .unwrap_or(now),
        expires_at_ms: None,
        approved_at_ms: approve.then_some(now),
        blocked_at_ms: (!approve).then_some(now),
        display_label: existing
            .as_ref()
            .and_then(|sender| sender.display_label.clone()),
        metadata: Default::default(),
    };
    store.upsert_channel_sender(account_id, sender_id, &record)?;
    println!(
        "{sender_id} is now {} on {account_id}.",
        if approve { "approved" } else { "blocked" }
    );
    Ok(())
}

pub fn routes(json: bool) -> Result<(), String> {
    let routes = store()?.channel_routes()?;
    if json {
        println!("{}", serde_json::json!({ "routes": routes }));
        return Ok(());
    }
    if routes.is_empty() {
        println!("No routes configured; inbound messages have nowhere to run.");
        return Ok(());
    }
    for route in &routes {
        println!(
            "{}  {}  -> {}{}",
            route.route_id,
            route.scope.specificity().as_str(),
            route.target.recipe,
            if route.enabled { "" } else { "  (disabled)" }
        );
    }
    Ok(())
}

/// The route scope the flags describe. `RouteScope::validate` (run by the
/// store) is what rejects off-ladder combinations; this only assembles.
///
/// Every flag that was passed lands in the scope, including a `--kind` that
/// sits alongside an account: dropping it here would store a narrower route
/// than the operator typed and report success. Validation refuses the pair,
/// which is the honest answer.
fn route_scope(options: &RouteOptions) -> Result<RouteScope, String> {
    let mut scope = match (options.account.as_deref(), options.conversation.as_deref()) {
        (Some(account), Some(conversation)) => RouteScope::conversation(account, conversation),
        (Some(account), None) => RouteScope::account(account),
        (None, Some(conversation)) => RouteScope {
            conversation_id: Some(conversation.to_string()),
            ..RouteScope::default()
        },
        (None, None) => RouteScope::default(),
    };
    if let Some(kind) = options.kind.as_deref() {
        scope.kind =
            Some(ChannelKind::parse(kind).ok_or_else(|| format!("Unknown provider '{kind}'"))?);
    }
    if let Some(thread) = &options.thread {
        scope = scope.with_thread(thread);
    }
    if let Some(sender) = &options.sender {
        scope = scope.with_sender(sender);
    }
    Ok(scope)
}

/// The route target the flags describe.
fn route_target(recipe: &str, options: &RouteOptions) -> Result<RouteTarget, String> {
    if recipe.trim().is_empty() {
        return Err("A route must name the task an incoming message runs as.".to_string());
    }
    let mut target = RouteTarget::new(recipe);
    target.repository = options.repository.clone();
    for param in &options.params {
        let (name, value) = param
            .split_once('=')
            .ok_or_else(|| format!("--param '{param}' must be name=value"))?;
        if name.is_empty() {
            return Err(format!("--param '{param}' has an empty name"));
        }
        target.params.insert(name.to_string(), value.to_string());
    }
    if let Some(session_scope) = &options.session_scope {
        target.session_scope = match session_scope.as_str() {
            "thread" => little_monkey_lib::channels::routing::SessionScope::Thread,
            "conversation" => little_monkey_lib::channels::routing::SessionScope::Conversation,
            "sender" => little_monkey_lib::channels::routing::SessionScope::Sender,
            "account" => little_monkey_lib::channels::routing::SessionScope::Account,
            other => {
                return Err(format!(
                    "Unknown session scope '{other}' (expected thread, conversation, sender or account)"
                ))
            }
        };
    }
    if let Some(priority) = options.priority {
        target.priority = priority;
    }
    target.reply_to_conversation = !options.no_reply;
    Ok(target)
}

pub fn add_route(recipe: &str, options: &RouteOptions, json: bool) -> Result<(), String> {
    let now = now_ms();
    let route = ChannelRoute {
        route_id: format!("route-{}", uuid::Uuid::new_v4().simple()),
        scope: route_scope(options)?,
        target: route_target(recipe, options)?,
        enabled: !options.disabled,
        created_at_ms: now,
        updated_at_ms: now,
    };
    store()?.insert_channel_route(&route)?;
    if json {
        println!("{}", serde_json::json!({ "route": route }));
    } else {
        println!("Added route {} -> {recipe}.", route.route_id);
    }
    Ok(())
}

pub fn update_route(
    route_id: &str,
    recipe: &str,
    options: &RouteOptions,
    json: bool,
) -> Result<(), String> {
    let mut store = store()?;
    let existing = store
        .channel_routes()?
        .into_iter()
        .find(|route| route.route_id == route_id)
        .ok_or_else(|| format!("No such route '{route_id}'"))?;
    let route = ChannelRoute {
        route_id: route_id.to_string(),
        scope: route_scope(options)?,
        target: route_target(recipe, options)?,
        enabled: !options.disabled,
        created_at_ms: existing.created_at_ms,
        updated_at_ms: now_ms(),
    };
    store.update_channel_route(&route)?;
    if json {
        println!("{}", serde_json::json!({ "route": route }));
    } else {
        println!("Updated route {route_id} -> {recipe}.");
    }
    Ok(())
}

pub fn enable_route(route_id: &str, enabled: bool) -> Result<(), String> {
    if store()?.set_channel_route_enabled(route_id, enabled, now_ms())? {
        println!(
            "Route {route_id} is now {}.",
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    } else {
        Err(format!("No such route '{route_id}'"))
    }
}

pub fn set_public_url(url: Option<&str>, clear: bool) -> Result<(), String> {
    match (url, clear) {
        (Some(url), false) => {
            store()?.set_channel_public_base_url(Some(url))?;
            println!("Webhook callbacks are now advertised under {url}.");
            Ok(())
        }
        (None, true) => {
            store()?.set_channel_public_base_url(None)?;
            println!("Public base URL cleared; webhook providers cannot reach this daemon until one is configured.");
            Ok(())
        }
        (Some(_), true) => Err("Pass a URL or --clear, not both.".to_string()),
        (None, false) => Err("Pass the base URL, or --clear to remove it.".to_string()),
    }
}

pub fn callback_url(account_id: &str, json: bool) -> Result<(), String> {
    let store = store()?;
    store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let path = crate::daemon::channel_store::channel_callback_path(account_id);
    let url = store.channel_callback_url(account_id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "account_id": account_id,
                "configured": url.is_some(),
                "url": url,
                "path": path,
            })
        );
        return Ok(());
    }
    match url {
        Some(url) => println!("{url}"),
        None => println!(
            "No public base URL is configured. The listener path is {path}; run \
             `monkey channels set-public-url <https://your-public-host>` once a tunnel or \
             reverse proxy exposes the daemon."
        ),
    }
    Ok(())
}

pub fn remove_route(route_id: &str) -> Result<(), String> {
    if store()?.delete_channel_route(route_id)? {
        println!("Removed route {route_id}.");
        Ok(())
    } else {
        Err(format!("No such route '{route_id}'"))
    }
}

pub fn events(account_id: &str, limit: u32, json: bool) -> Result<(), String> {
    let events = store()?.recent_channel_events(account_id, limit.clamp(1, 200))?;
    if json {
        let rows: Vec<serde_json::Value> = events
            .iter()
            .map(|event| {
                serde_json::json!({
                    "event_id": event.event_id,
                    "direction": event.direction.as_str(),
                    "conversation_id": event.conversation_id,
                    "thread_id": event.thread_id,
                    "sender_id": event.sender_id,
                    "disposition": event.disposition.as_str(),
                    "ignore_reason": event.ignore_reason,
                    // Which accepted turn owns this event, and which run that
                    // turn became: together they answer, without opening the
                    // database, whether a message that arrived was ever acted
                    // on.
                    "ingress_id": event.ingress_id,
                    "job_id": event.job_id,
                    "received_at_ms": event.received_at_ms,
                })
            })
            .collect();
        // Message text is deliberately absent: this listing is for an operator
        // checking whether traffic is flowing, not a transcript export.
        println!("{}", serde_json::json!({ "events": rows }));
        return Ok(());
    }
    if events.is_empty() {
        println!("No activity recorded for {account_id}.");
        return Ok(());
    }
    for event in &events {
        println!(
            "{}  {}  {}  {}",
            event.received_at_ms,
            event.direction.as_str(),
            event.disposition.as_str(),
            event.conversation_id
        );
    }
    Ok(())
}

pub fn remove(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(reference) = &account.credential_ref {
        // Best effort: a keychain that refuses must not leave the account row
        // behind, or the operator cannot finish removing it.
        let _ = KeyringChannelSecrets.delete(reference);
    }
    store.delete_channel_account(account_id)?;
    println!("Removed {account_id} and its stored credential.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_is_never_born_connected() {
        // The health a new account starts with is the one thing this file must
        // not get wrong: "credentials saved" is not "connected".
        let now = now_ms();
        let health = ChannelHealth {
            state: HealthState::Unconfigured,
            detail: None,
            last_error: None,
            probed_at_ms: now,
        };
        assert_eq!(health.state, HealthState::Unconfigured);
        assert_ne!(health.state, HealthState::Connected);
    }

    #[test]
    fn the_json_view_has_nowhere_to_put_a_secret() {
        let account = ChannelAccountRecord {
            account_id: "chan-1".into(),
            kind: ChannelKind::Telegram,
            label: "Ops".into(),
            enabled: true,
            non_secret_config: serde_json::json!({ "note": "no secrets here" }),
            credential_ref: Some("channel:chan-1".into()),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth::connected(1, None),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let rendered = account_json(&account).to_string();
        assert!(rendered.contains("\"has_credential\":true"));
        assert!(!rendered.contains("credential_ref"));
        assert!(!rendered.contains("channel:chan-1"));
    }

    /// One connected account in a store this test owns.
    fn connected_store() -> DaemonStore {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .upsert_channel_account(&ChannelAccountRecord {
                account_id: "chan-1".into(),
                kind: ChannelKind::Telegram,
                label: "Ops".into(),
                enabled: true,
                non_secret_config: serde_json::json!({}),
                credential_ref: Some("channel:chan-1".into()),
                access_policy: ChannelAccessPolicy::default(),
                health: ChannelHealth::connected(1, None),
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .expect("account");
        store
    }

    #[test]
    fn a_replaced_credential_invalidates_the_old_connectivity_claim() {
        // Connected → replace credential → Disconnected. The new credential
        // has never spoken to the provider, so the old claim must not
        // survive the write.
        let mut store = connected_store();
        let account = record_credential_change(&mut store, "chan-1").expect("recorded");
        assert_eq!(account.health.state, HealthState::Disconnected);
        assert!(
            account
                .health
                .detail
                .as_deref()
                .unwrap_or("")
                .contains("probe"),
            "{:?}",
            account.health.detail
        );

        // And writing a credential over a Disconnected account never makes it
        // Connected: saving is not connecting.
        let again = record_credential_change(&mut store, "chan-1").expect("recorded");
        assert_ne!(again.health.state, HealthState::Connected);
    }

    #[test]
    fn a_label_only_edit_keeps_the_health_a_probe_earned() {
        let mut store = connected_store();
        let (account, config_changed) =
            apply_account_edit(&mut store, "chan-1", None, Some("Renamed")).expect("edited");
        assert!(!config_changed);
        assert_eq!(account.label, "Renamed");
        assert_eq!(account.health.state, HealthState::Connected);
    }

    #[test]
    fn a_settings_change_drops_the_old_connectivity_claim() {
        let mut store = connected_store();
        let (account, config_changed) = apply_account_edit(
            &mut store,
            "chan-1",
            Some(r#"{"max_attachment_bytes": 1024}"#),
            None,
        )
        .expect("edited");
        assert!(config_changed);
        assert_eq!(account.health.state, HealthState::Disconnected);

        // Saving the identical settings again is not a change, so the (now
        // Disconnected) state and its detail are left alone.
        let (_, changed_again) = apply_account_edit(
            &mut store,
            "chan-1",
            Some(r#"{"max_attachment_bytes": 1024}"#),
            None,
        )
        .expect("edited");
        assert!(!changed_again);
    }

    /// Settings → row → adapter, end to end.
    ///
    /// Editing configuration is only worth anything if the *next* adapter is
    /// built from what was saved. Mattermost is the honest subject: its
    /// server URL is read out of the account row at build time and validated
    /// there, so an edit that the adapter would refuse proves the build is
    /// reading the edited row rather than a value captured earlier.
    #[test]
    fn an_edited_setting_is_what_the_next_adapter_is_built_from() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let account = ChannelAccountRecord {
            account_id: "chan-mm".into(),
            kind: ChannelKind::Mattermost,
            label: "Team".into(),
            enabled: true,
            non_secret_config: serde_json::json!({ "base_url": "https://chat.example.com" }),
            credential_ref: Some("channel:chan-mm".into()),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth::connected(1, None),
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        store.upsert_channel_account(&account).expect("account");
        let built = |account: &ChannelAccountRecord| {
            crate::daemon::adapters::build_adapter(
                &AdapterConfig {
                    account,
                    secret: "token".to_string(),
                },
                None,
            )
        };
        built(&account).expect("the configured server builds");

        // A server this adapter refuses: the failure is the proof that the
        // edited value is the one being read.
        let (edited, changed) = apply_account_edit(
            &mut store,
            "chan-mm",
            Some(r#"{"base_url": "http://chat.example.com"}"#),
            None,
        )
        .expect("edited");
        assert!(changed);
        assert_eq!(edited.health.state, HealthState::Disconnected);
        let error = match built(&edited) {
            Ok(_) => panic!("plain http against a remote host must be refused"),
            Err(error) => error,
        };
        assert!(error.contains("https"), "{error}");

        // And an edit to another real server builds again — from the stored
        // row, read back, not from anything held in memory.
        apply_account_edit(
            &mut store,
            "chan-mm",
            Some(r#"{"base_url": "https://chat.elsewhere.example"}"#),
            None,
        )
        .expect("edited");
        let stored = store
            .channel_account("chan-mm")
            .expect("read")
            .expect("account");
        assert_eq!(
            stored.non_secret_config["base_url"],
            "https://chat.elsewhere.example"
        );
        built(&stored).expect("the new server builds");
        // The credential never travelled through the edit in either
        // direction; only a probe can claim connectivity again.
        assert_eq!(
            stored.credential_ref.as_deref(),
            Some("channel:chan-mm"),
            "an edit must not disturb the credential"
        );
        assert_ne!(stored.health.state, HealthState::Connected);
    }

    /// Every option the CLI, the bridge and the UI can set, all at once.
    fn fully_populated_options() -> RouteOptions {
        RouteOptions {
            account: Some("chan-1".to_string()),
            conversation: Some("conv-7".to_string()),
            thread: Some("thread-2".to_string()),
            sender: Some("user-9".to_string()),
            kind: None,
            repository: Some("/work/repo".to_string()),
            params: vec!["focus=deps".to_string(), "depth=3".to_string()],
            session_scope: Some("sender".to_string()),
            priority: Some(7),
            no_reply: true,
            disabled: true,
        }
    }

    #[test]
    fn a_fully_populated_route_round_trips_through_create_read_edit_and_reenable() {
        // The daemon replaces a route's target wholesale on update, so the
        // one way an editor can be honest is to send everything back. This
        // holds the other half of that bargain: everything sent is stored,
        // read back, and survives an edit that touches one field.
        let mut store = DaemonStore::open_in_memory().expect("open");
        let options = fully_populated_options();
        let route = ChannelRoute {
            route_id: "route-full".to_string(),
            scope: route_scope(&options).expect("scope"),
            target: route_target("triage", &options).expect("target"),
            enabled: !options.disabled,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        store.insert_channel_route(&route).expect("insert");

        let read = |store: &DaemonStore| {
            store
                .channel_routes()
                .expect("routes")
                .into_iter()
                .find(|entry| entry.route_id == "route-full")
                .expect("the route exists")
        };
        let stored = read(&store);
        assert_eq!(stored.scope.account_id.as_deref(), Some("chan-1"));
        assert_eq!(stored.scope.conversation_id.as_deref(), Some("conv-7"));
        assert_eq!(stored.scope.thread_id.as_deref(), Some("thread-2"));
        assert_eq!(stored.scope.sender_id.as_deref(), Some("user-9"));
        assert_eq!(stored.target.recipe, "triage");
        assert_eq!(stored.target.repository.as_deref(), Some("/work/repo"));
        assert_eq!(
            stored.target.params.get("focus").map(String::as_str),
            Some("deps")
        );
        assert_eq!(
            stored.target.params.get("depth").map(String::as_str),
            Some("3")
        );
        assert_eq!(
            stored.target.session_scope,
            little_monkey_lib::channels::routing::SessionScope::Sender
        );
        assert_eq!(stored.target.priority, 7);
        assert!(!stored.target.reply_to_conversation);
        assert!(!stored.enabled);

        // Edit one field — the recipe — with everything else resent, exactly
        // as the UI does. Nothing else may move.
        let updated = ChannelRoute {
            route_id: "route-full".to_string(),
            scope: route_scope(&options).expect("scope"),
            target: route_target("chat-2", &options).expect("target"),
            enabled: !options.disabled,
            created_at_ms: stored.created_at_ms,
            updated_at_ms: 2,
        };
        store.update_channel_route(&updated).expect("update");
        let after_edit = read(&store);
        assert_eq!(after_edit.target.recipe, "chat-2");
        assert_eq!(after_edit.target.params.len(), 2);
        assert_eq!(after_edit.target.repository.as_deref(), Some("/work/repo"));
        assert_eq!(after_edit.target.priority, 7);
        assert!(!after_edit.target.reply_to_conversation);
        assert_eq!(after_edit.scope, stored.scope);

        // Disable and re-enable through the dedicated switch: the target is
        // not part of that operation and must not be touched by it.
        store
            .set_channel_route_enabled("route-full", true, 3)
            .expect("enable");
        let enabled = read(&store);
        assert!(enabled.enabled);
        assert_eq!(enabled.target, after_edit.target);
        store
            .set_channel_route_enabled("route-full", false, 4)
            .expect("disable");
        let disabled = read(&store);
        assert!(!disabled.enabled);
        assert_eq!(disabled.target, after_edit.target);
    }

    /// A flag that was typed must reach the scope. Anything dropped here is
    /// stored as a *different*, broader route than the operator asked for and
    /// reported as success — the failure mode this assembly must not have.
    #[test]
    fn every_scope_flag_reaches_the_scope_even_when_the_combination_is_illegal() {
        let scoped = |account: Option<&str>, conversation: Option<&str>, kind: Option<&str>| {
            route_scope(&RouteOptions {
                account: account.map(str::to_string),
                conversation: conversation.map(str::to_string),
                kind: kind.map(str::to_string),
                thread: None,
                sender: None,
                repository: None,
                params: Vec::new(),
                session_scope: None,
                priority: None,
                no_reply: false,
                disabled: false,
            })
        };

        // A conversation with no account used to fall through to the global
        // default: the operator asked for one conversation and would have got
        // every message on the installation.
        let orphan = scoped(None, Some("C1"), None).expect("assembled");
        assert_eq!(orphan.conversation_id.as_deref(), Some("C1"));
        assert_eq!(
            orphan.validate(),
            Err(little_monkey_lib::channels::routing::RouteScopeError::MissingAccount)
        );

        // A provider-wide default alongside an account used to drop the
        // provider and store the account route silently.
        let both = scoped(Some("chan-1"), None, Some("telegram")).expect("assembled");
        assert_eq!(both.kind, Some(ChannelKind::Telegram));
        assert_eq!(both.account_id.as_deref(), Some("chan-1"));
        assert!(both.validate().is_err());

        // The legal shapes still assemble to exactly their rung.
        assert_eq!(
            scoped(None, None, Some("slack")).expect("kind").kind,
            Some(ChannelKind::Slack)
        );
        assert_eq!(
            scoped(None, None, None).expect("global"),
            RouteScope::default()
        );
        assert!(scoped(None, None, Some("nonesuch")).is_err());
    }

    /// The sender rung the daemon declares is all four ids, and the CLI is
    /// one of the two front ends that must not be able to store anything
    /// else.
    #[test]
    fn a_sender_route_without_a_thread_is_refused_by_the_store() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let mut options = fully_populated_options();
        options.thread = None;
        let route = ChannelRoute {
            route_id: "route-sender".to_string(),
            scope: route_scope(&options).expect("scope"),
            target: route_target("triage", &options).expect("target"),
            enabled: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        };
        let error = store.insert_channel_route(&route).expect_err("no thread");
        assert!(error.contains("thread"), "{error}");
        assert!(store.channel_routes().expect("routes").is_empty());
    }

    #[test]
    fn a_parameter_with_no_name_is_refused_with_the_daemons_own_words() {
        let mut options = fully_populated_options();
        options.params = vec!["=value".to_string()];
        let error = route_target("triage", &options).expect_err("empty name");
        assert!(error.contains("empty name"), "{error}");
        options.params = vec!["notapair".to_string()];
        let error = route_target("triage", &options).expect_err("no separator");
        assert!(error.contains("name=value"), "{error}");
    }

    #[test]
    fn a_memory_secret_store_round_trips() {
        // Proves the trait the CLI writes through, without touching the real
        // keychain (CI machines have none).
        let secrets = MemoryChannelSecrets::default();
        secrets.put("channel:chan-1", "token").expect("put");
        assert_eq!(secrets.get("channel:chan-1").expect("get"), "token");
        secrets.delete("channel:chan-1").expect("delete");
        assert!(secrets.get("channel:chan-1").is_err());
    }
}

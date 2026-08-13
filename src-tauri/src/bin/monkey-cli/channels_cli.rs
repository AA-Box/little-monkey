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
        "created_at_ms": account.created_at_ms,
        "updated_at_ms": account.updated_at_ms,
    })
}

pub fn list(json: bool) -> Result<(), String> {
    let accounts = store()?.channel_accounts()?;
    if json {
        let rows: Vec<serde_json::Value> = accounts.iter().map(account_json).collect();
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
    if config_json.is_none() && label.is_none() {
        return Err("Pass --config, --label, or both.".to_string());
    }
    let mut store = store()?;
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
        account.health = ChannelHealth {
            state: HealthState::Disconnected,
            detail: Some("Settings changed; run a probe to verify the connection.".to_string()),
            last_error: None,
            probed_at_ms: now_ms(),
        };
    }
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
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

/// Store the account's credential in the keychain. The value is read from
/// stdin, never from an argument, so it cannot end up in a process listing or a
/// shell history file.
pub fn set_token(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;

    let mut secret = String::new();
    std::io::stdin()
        .read_line(&mut secret)
        .map_err(|error| format!("Could not read the credential from stdin: {error}"))?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("No credential was supplied on stdin".to_string());
    }

    let credential_ref = little_monkey_lib::channels::credential_ref(account_id);
    KeyringChannelSecrets.put(&credential_ref, secret)?;
    account.credential_ref = Some(credential_ref);
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
    println!("Credential stored for {account_id}.");
    Ok(())
}

/// Point the account at its keychain entry after the app stored one there.
pub fn mark_credential(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .channel_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    account.credential_ref = Some(little_monkey_lib::channels::credential_ref(account_id));
    account.updated_at_ms = now_ms();
    store.upsert_channel_account(&account)?;
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
    let adapter = crate::daemon::adapters::build_adapter(&AdapterConfig {
        account: &account,
        secret,
    })?;
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
fn route_scope(options: &RouteOptions) -> Result<RouteScope, String> {
    let mut scope = match (options.account.as_deref(), options.conversation.as_deref()) {
        (Some(account), Some(conversation)) => RouteScope::conversation(account, conversation),
        (Some(account), None) => RouteScope::account(account),
        (None, _) => match options.kind.as_deref() {
            Some(kind) => RouteScope::channel_default(
                ChannelKind::parse(kind).ok_or_else(|| format!("Unknown provider '{kind}'"))?,
            ),
            None => RouteScope::global_default(),
        },
    };
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

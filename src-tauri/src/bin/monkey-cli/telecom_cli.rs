//! `monkey telecom` — configure the operator's own carrier accounts.
//!
//! The same arrangement `monkey channels` uses, and for the same reason: the
//! desktop's Telephony settings calls exactly these subcommands through the
//! typed bridge, so there is one implementation of the rules and two front ends
//! to it.
//!
//! Two things this surface never does. It never accepts a carrier credential as
//! an argument — `set-token` reads stdin, so a secret cannot appear in a
//! process listing or a shell history. And it never reports an account as
//! connected because it was configured: only `probe`, which asks the carrier,
//! can write that.
//!
//! Every account here can spend the operator's money at their carrier, so the
//! defaults are the cautious ones: disabled, calls rejected, dialing out never,
//! one call at a time, recording off.

use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

use crate::daemon::channel_adapter::{ChannelSecrets, KeyringChannelSecrets};
use crate::daemon::store::{DaemonPaths, DaemonStore};
use crate::daemon::telecom_store::{
    CallLimits, InboundCallPolicy, OutboundCallApproval, TelecomAccountRecord, TelecomMessageRecord,
};
use crate::daemon::telephony::{provider_for_account, TelecomKind};

/// `monkey telecom <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum TelecomCmd {
    /// List carrier accounts with their real, probed health.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a carrier account. It starts disabled, with no credential, and with
    /// calls refused in both directions.
    Add {
        /// Carrier: twilio, telnyx, plivo, mock.
        kind: String,
        /// Name shown in listings and in the app.
        label: String,
        /// The identifier the carrier issues (Twilio Account SID, Telnyx API
        /// user, Plivo Auth ID). Not a secret.
        carrier_account_id: String,
        /// The number the operator owns, in E.164.
        from_number: String,
        /// The operator's own canonical public URL, which is the only value
        /// ever used to rebuild a signed callback URL.
        #[arg(long)]
        public_url: Option<String>,
        /// Non-secret carrier settings as a JSON object — Telnyx's published
        /// webhook public key goes here.
        #[arg(long)]
        config: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Store the carrier credential, read from stdin.
    SetToken { account_id: String },
    /// Record that the app stored a credential in the keychain for this
    /// account. The secret itself never travels through an argument.
    MarkCredential { account_id: String },
    /// Enable or disable an account.
    Enable {
        account_id: String,
        #[arg(long)]
        off: bool,
    },
    /// Ask the carrier whether the credential works. The only thing that can
    /// report an account as connected.
    Probe {
        account_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Set what this number does with calls.
    Policy {
        account_id: String,
        /// reject | voicemail | answer
        #[arg(long)]
        inbound: Option<String>,
        /// never | approval | allow — whether the agent may dial out at all.
        #[arg(long)]
        outbound: Option<String>,
    },
    /// Set the limits that bound what a call can cost.
    Limits {
        account_id: String,
        #[arg(long)]
        max_concurrent: Option<u32>,
        #[arg(long)]
        ring_timeout_s: Option<u32>,
        #[arg(long)]
        max_duration_s: Option<u32>,
        /// Record calls. Off unless the operator turns it on, and their own
        /// jurisdiction decides whether they may.
        #[arg(long)]
        recording: Option<bool>,
    },
    /// What this number says when a call connects. Without one, a caller who
    /// is answered hears silence until they speak first.
    Greeting {
        account_id: String,
        /// The words to say. Empty clears it.
        text: Vec<String>,
    },
    /// Recent calls on this number.
    Calls {
        account_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Change where this account's carrier reaches it, or its non-secret
    /// settings. A tunnel that moved is the usual reason.
    SetUrl {
        account_id: String,
        /// The operator's own canonical public URL. Pass `--clear` to remove it.
        #[arg(long)]
        url: Option<String>,
        /// Non-secret carrier settings as a JSON object, merged into what is
        /// stored — Telnyx's rotated webhook public key goes here.
        #[arg(long)]
        config: Option<String>,
        /// Remove the public URL, leaving the account with nowhere for its
        /// carrier to deliver.
        #[arg(long)]
        clear: bool,
    },
    /// Recent texts on this number, both directions, with their delivery state.
    Messages {
        account_id: String,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// The callback URL this account's carrier should post to.
    CallbackUrl {
        account_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove an account, its calls and its stored credential.
    Remove { account_id: String },
}

pub async fn dispatch(action: &TelecomCmd) -> Result<(), String> {
    match action {
        TelecomCmd::List { json } => list(*json),
        TelecomCmd::Add {
            kind,
            label,
            carrier_account_id,
            from_number,
            public_url,
            config,
            json,
        } => add(
            kind,
            label,
            carrier_account_id,
            from_number,
            public_url.as_deref(),
            config.as_deref(),
            *json,
        ),
        TelecomCmd::SetToken { account_id } => set_token(account_id),
        TelecomCmd::MarkCredential { account_id } => mark_credential(account_id),
        TelecomCmd::Enable { account_id, off } => enable(account_id, !*off),
        TelecomCmd::Probe { account_id, json } => probe(account_id, *json).await,
        TelecomCmd::Policy {
            account_id,
            inbound,
            outbound,
        } => set_policy(account_id, inbound.as_deref(), outbound.as_deref()),
        TelecomCmd::Limits {
            account_id,
            max_concurrent,
            ring_timeout_s,
            max_duration_s,
            recording,
        } => set_limits(
            account_id,
            *max_concurrent,
            *ring_timeout_s,
            *max_duration_s,
            *recording,
        ),
        TelecomCmd::Greeting { account_id, text } => set_greeting(account_id, &text.join(" ")),
        TelecomCmd::Calls {
            account_id,
            limit,
            json,
        } => calls(account_id, *limit, *json),
        TelecomCmd::SetUrl {
            account_id,
            url,
            config,
            clear,
        } => set_public_url(account_id, url.as_deref(), config.as_deref(), *clear),
        TelecomCmd::Messages {
            account_id,
            limit,
            json,
        } => messages(account_id, *limit, *json),
        TelecomCmd::CallbackUrl { account_id, json } => callback_url(account_id, *json),
        TelecomCmd::Remove { account_id } => remove(account_id),
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

/// JSON view of an account. Deliberately not the storage struct: this one has
/// nowhere to put a secret, so it can never grow one.
pub(crate) fn account_json(account: &TelecomAccountRecord) -> serde_json::Value {
    serde_json::json!({
        "account_id": account.account_id,
        "kind": account.kind.as_str(),
        "kind_label": account.kind.label(),
        "label": account.label,
        "enabled": account.enabled,
        "carrier_account_id": account.carrier_account_id,
        "from_number": account.from_number,
        "has_credential": account.credential_ref.is_some(),
        "public_base_url": account.public_base_url,
        "greeting": account
            .non_secret_config
            .get("greeting")
            .and_then(|value| value.as_str()),
        // Plivo records with an XML element that cannot run alongside the
        // stream a conversation needs, so the UI can say so rather than
        // offering a switch that turns into a refused call.
        "supports_recording": !matches!(account.kind, TelecomKind::Plivo),
        "inbound_policy": account.inbound_policy.as_str(),
        "outbound_approval": account.outbound_approval.as_str(),
        "limits": {
            "max_concurrent_calls": account.limits.max_concurrent_calls,
            "ring_timeout_s": account.limits.ring_timeout_s,
            "max_duration_s": account.limits.max_duration_s,
            "recording_enabled": account.limits.recording_enabled,
        },
        "health": {
            "state": account.health.state.as_str(),
            "detail": account.health.detail,
            "last_error": account.health.last_error,
            "probed_at_ms": account.health.probed_at_ms,
        },
        "updated_at_ms": account.updated_at_ms,
    })
}

/// The same view plus what only the store can answer: how many callbacks this
/// account has refused since one last verified. Separate from
/// [`account_json`] because that function is handed a record and this one needs
/// the store the record came from.
pub(crate) fn account_json_with_store(
    store: &DaemonStore,
    account: &TelecomAccountRecord,
) -> serde_json::Value {
    let mut value = account_json(account);
    let rejections = store
        .callback_rejections(&account.account_id)
        .unwrap_or_default();
    value["callback_rejections"] = serde_json::json!({
        "count": rejections.count,
        "last_reason": rejections.last_reason,
        "last_at_ms": rejections.last_at_ms,
    });
    value
}

/// JSON view of one recent text. No credential can reach this: it is built
/// from a message row and nothing else.
pub(crate) fn message_json(message: &TelecomMessageRecord) -> serde_json::Value {
    serde_json::json!({
        "direction": message.direction.as_str(),
        "peer_number": message.peer_number,
        "text": message.text,
        "state": message.state,
        "delivery_state": message.delivery_state,
        "error": message.error,
        "at_ms": message.at_ms,
    })
}

fn list(json: bool) -> Result<(), String> {
    let store = store()?;
    let accounts = store.telecom_accounts()?;
    if json {
        println!(
            "{}",
            serde_json::Value::Array(
                accounts
                    .iter()
                    .map(|account| account_json_with_store(&store, account))
                    .collect()
            )
        );
        return Ok(());
    }
    if accounts.is_empty() {
        println!("No carrier accounts yet. Add one with `monkey telecom add <carrier> <label> <carrier-account-id> <from-number>`.");
        return Ok(());
    }
    for account in &accounts {
        println!(
            "{}  {}  {}  {}  [{}]{}",
            account.account_id,
            account.kind.label(),
            account.from_number,
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

fn add(
    kind: &str,
    label: &str,
    carrier_account_id: &str,
    from_number: &str,
    public_url: Option<&str>,
    config_json: Option<&str>,
    json: bool,
) -> Result<(), String> {
    let kind = TelecomKind::parse(kind)
        .ok_or_else(|| format!("Unknown carrier '{kind}'. Try one of: twilio, telnyx, plivo"))?;
    if !from_number.starts_with('+') || from_number.len() < 8 {
        return Err("The number must be in international format, like +15551234567".to_string());
    }
    if let Some(url) = public_url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("The public URL must start with https://".to_string());
        }
    }
    let non_secret_config: serde_json::Value = match config_json {
        Some(raw) => serde_json::from_str(raw)
            .map_err(|error| format!("--config must be a JSON object: {error}"))?,
        None => serde_json::json!({}),
    };
    if !non_secret_config.is_object() {
        return Err("--config must be a JSON object".to_string());
    }
    let account_id = format!("tel-{}", uuid::Uuid::new_v4().simple());
    let now = now_ms();
    let record = TelecomAccountRecord {
        account_id: account_id.clone(),
        kind,
        label: label.to_string(),
        enabled: false,
        carrier_account_id: carrier_account_id.to_string(),
        from_number: from_number.to_string(),
        credential_ref: None,
        public_base_url: public_url.map(str::to_string),
        non_secret_config,
        // Both directions refused until the operator says otherwise. A number
        // that answered or dialed on the strength of being configured would be
        // spending money nobody agreed to.
        inbound_policy: InboundCallPolicy::Reject,
        outbound_approval: OutboundCallApproval::Never,
        limits: CallLimits::default(),
        health: ChannelHealth {
            state: HealthState::Unconfigured,
            detail: None,
            last_error: None,
            probed_at_ms: now,
        },
        created_at_ms: now,
        updated_at_ms: now,
    };
    let mut store = store()?;
    store.upsert_telecom_account(&record)?;
    // The messaging side gets its account now rather than on the first text,
    // so an operator can authorize senders and set up routing for this number
    // before anyone texts it.
    crate::daemon::telecom_worker::ensure_sms_channel_account(&mut store, &record, now)?;
    if json {
        println!("{}", account_json(&record));
    } else {
        println!(
            "Added {} account {account_id}. Store its credential with `monkey telecom set-token {account_id}`, then enable it.\nCalls and texts cost money at your carrier.",
            kind.label()
        );
    }
    Ok(())
}

fn set_token(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let mut secret = String::new();
    std::io::stdin()
        .read_line(&mut secret)
        .map_err(|error| format!("Could not read the credential from stdin: {error}"))?;
    let secret = secret.trim();
    if secret.is_empty() {
        return Err("No credential was supplied on stdin".to_string());
    }
    let reference = little_monkey_lib::channels::telecom_credential_ref(account_id);
    KeyringChannelSecrets.put(&reference, secret)?;
    account.credential_ref = Some(reference);
    record_credential_change(&mut store, account)?;
    println!(
        "Credential stored for {account_id}. Run `monkey telecom probe {account_id}` to verify it."
    );
    Ok(())
}

fn mark_credential(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    account.credential_ref = Some(little_monkey_lib::channels::telecom_credential_ref(
        account_id,
    ));
    record_credential_change(&mut store, account)?;
    println!("Credential recorded for {account_id}.");
    Ok(())
}

/// A carrier credential was just written: whatever the old one had proven
/// about connectivity is unverified now, on this row and on the SMS channel
/// account that shadows it — texts go out through this same credential.
fn record_credential_change(
    store: &mut DaemonStore,
    mut account: TelecomAccountRecord,
) -> Result<(), String> {
    let unverified = ChannelHealth {
        state: HealthState::Disconnected,
        detail: Some("Credential changed; run a probe to verify the connection.".to_string()),
        last_error: None,
        probed_at_ms: now_ms(),
    };
    let account_id = account.account_id.clone();
    account.health = unverified.clone();
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)?;
    if let Some(mut channel) = store.channel_account(&account_id)? {
        channel.health = unverified;
        channel.updated_at_ms = now_ms();
        store.upsert_channel_account(&channel)?;
    }
    Ok(())
}

fn enable(account_id: &str, enabled: bool) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if enabled && account.credential_ref.is_none() {
        return Err(format!(
            "Account '{account_id}' has no credential yet; run `monkey telecom set-token {account_id}` first"
        ));
    }
    account.enabled = enabled;
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)?;
    println!(
        "{account_id} is now {}.",
        if enabled { "enabled" } else { "disabled" }
    );
    Ok(())
}

async fn probe(account_id: &str, json: bool) -> Result<(), String> {
    let mut store = store()?;
    let account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let secret = match &account.credential_ref {
        Some(reference) => KeyringChannelSecrets.get(reference)?,
        None => String::new(),
    };
    let base = store.telecom_callback_base(&account);
    let provider = provider_for_account(&account, secret, base)?;
    let health = provider.probe().await;
    store.set_telecom_account_health(account_id, &health, now_ms())?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "account_id": account_id,
                "state": health.state.as_str(),
                "detail": health.detail,
                "last_error": health.last_error,
            })
        );
    } else {
        println!(
            "{account_id}: {}{}",
            health.state.as_str(),
            health
                .detail
                .as_ref()
                .map(|detail| format!(" — {detail}"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn set_policy(
    account_id: &str,
    inbound: Option<&str>,
    outbound: Option<&str>,
) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(value) = inbound {
        account.inbound_policy = InboundCallPolicy::parse(value).ok_or_else(|| {
            format!("--inbound must be reject, voicemail or answer, not '{value}'")
        })?;
    }
    if let Some(value) = outbound {
        account.outbound_approval = OutboundCallApproval::parse(value)
            .ok_or_else(|| format!("--outbound must be never, approval or allow, not '{value}'"))?;
    }
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)?;
    println!(
        "{account_id}: inbound {}, outbound {}.",
        account.inbound_policy.as_str(),
        account.outbound_approval.as_str()
    );
    Ok(())
}

fn set_limits(
    account_id: &str,
    max_concurrent: Option<u32>,
    ring_timeout_s: Option<u32>,
    max_duration_s: Option<u32>,
    recording: Option<bool>,
) -> Result<(), String> {
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let mut limits = account.limits;
    if let Some(value) = max_concurrent {
        limits.max_concurrent_calls = value;
    }
    if let Some(value) = ring_timeout_s {
        limits.ring_timeout_s = value;
    }
    if let Some(value) = max_duration_s {
        limits.max_duration_s = value;
    }
    if let Some(value) = recording {
        limits.recording_enabled = value;
    }
    account.limits = limits.sanitized();
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)?;
    println!(
        "{account_id}: at most {} concurrent call(s), ring {}s, at most {}s per call, recording {}.",
        account.limits.max_concurrent_calls,
        account.limits.ring_timeout_s,
        account.limits.max_duration_s,
        if account.limits.recording_enabled {
            "on"
        } else {
            "off"
        }
    );
    Ok(())
}

/// Store the line the number opens an answered call with.
///
/// Lives in the account's non-secret config rather than in a column of its own:
/// it is operator-authored text about this number, like its label, and nothing
/// in the daemon branches on it.
fn set_greeting(account_id: &str, text: &str) -> Result<(), String> {
    if text.chars().count() > 600 {
        return Err("That greeting is too long to say on a phone call.".to_string());
    }
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    let mut config = account.non_secret_config.clone();
    let Some(object) = config.as_object_mut() else {
        return Err("This account's settings are not a JSON object".to_string());
    };
    if text.trim().is_empty() {
        object.remove("greeting");
        println!("{account_id} will answer without saying anything first.");
    } else {
        object.insert(
            "greeting".to_string(),
            serde_json::Value::String(text.trim().to_string()),
        );
        println!(
            "{account_id} will open an answered call with: {}",
            text.trim()
        );
    }
    account.non_secret_config = config;
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)
}

fn calls(account_id: &str, limit: u32, json: bool) -> Result<(), String> {
    let store = store()?;
    let calls = store.recent_calls(account_id, limit)?;
    if json {
        let rows: Vec<serde_json::Value> = calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "call_id": call.call_id,
                    "direction": call.direction.as_str(),
                    // The other party's number is the whole record of who was
                    // on the call; nothing that was said is stored.
                    "peer_number": call.peer_number,
                    "state": call.state.as_str(),
                    "last_error": call.last_error,
                    "started_at_ms": call.started_at_ms,
                    "ended_at_ms": call.ended_at_ms,
                    "created_at_ms": call.created_at_ms,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(rows));
        return Ok(());
    }
    if calls.is_empty() {
        println!("No calls recorded for {account_id}.");
        return Ok(());
    }
    for call in &calls {
        println!(
            "{}  {}  {}  [{}]{}",
            call.call_id,
            call.direction.as_str(),
            call.peer_number,
            call.state.as_str(),
            call.last_error
                .as_ref()
                .map(|error| format!("  ({error})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

/// Point this account's carrier at a different place, or update its non-secret
/// settings.
///
/// The public URL is the one value every signature check depends on: Twilio and
/// Plivo sign the URL their callback was posted to, so a base that no longer
/// matches the carrier console rejects every genuine callback. An operator
/// whose tunnel moved needs to be able to fix it without deleting the number
/// and its call history.
fn set_public_url(
    account_id: &str,
    url: Option<&str>,
    config_json: Option<&str>,
    clear: bool,
) -> Result<(), String> {
    if url.is_none() && config_json.is_none() && !clear {
        return Err("Pass --url, --config or --clear.".to_string());
    }
    let mut store = store()?;
    let mut account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(url) = url {
        if !url.starts_with("https://") && !url.starts_with("http://") {
            return Err("The public URL must start with https://".to_string());
        }
        account.public_base_url = Some(url.trim_end_matches('/').to_string());
    } else if clear {
        account.public_base_url = None;
    }
    if let Some(raw) = config_json {
        let patch: serde_json::Value = serde_json::from_str(raw)
            .map_err(|error| format!("--config must be a JSON object: {error}"))?;
        let patch = patch
            .as_object()
            .ok_or_else(|| "--config must be a JSON object".to_string())?;
        let mut merged = account
            .non_secret_config
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (key, value) in patch {
            merged.insert(key.clone(), value.clone());
        }
        account.non_secret_config = serde_json::Value::Object(merged);
    }
    // Where a carrier reaches this number just changed, so what the last probe
    // proved about it no longer holds.
    account.health = ChannelHealth {
        state: HealthState::Disconnected,
        detail: Some(
            "Callback settings changed; run a probe to verify the connection.".to_string(),
        ),
        last_error: None,
        probed_at_ms: now_ms(),
    };
    account.updated_at_ms = now_ms();
    store.upsert_telecom_account(&account)?;
    // The old rejections were about the old URL.
    store.clear_callback_rejections(account_id)?;
    match store.telecom_callback_base(&account) {
        Some(base) => println!(
            "{account_id} now expects its carrier at {}",
            crate::daemon::telephony::callback_url(&base, account_id)
        ),
        None => println!("{account_id} has no public URL, so its carrier has nowhere to post to."),
    }
    Ok(())
}

fn messages(account_id: &str, limit: u32, json: bool) -> Result<(), String> {
    let store = store()?;
    if store.telecom_account(account_id)?.is_none() {
        return Err(format!("No such account '{account_id}'"));
    }
    let messages = store.recent_telecom_messages(account_id, limit)?;
    if json {
        println!(
            "{}",
            serde_json::Value::Array(messages.iter().map(message_json).collect())
        );
        return Ok(());
    }
    if messages.is_empty() {
        println!("No texts on {account_id} yet.");
        return Ok(());
    }
    for message in &messages {
        println!(
            "{}  {}  {}  {}{}",
            message.direction.as_str(),
            message.peer_number,
            message
                .delivery_state
                .as_deref()
                .unwrap_or(message.state.as_str()),
            message.text,
            message
                .error
                .as_ref()
                .map(|error| format!("  ({error})"))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn callback_url(account_id: &str, json: bool) -> Result<(), String> {
    let store = store()?;
    let account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    // Resolved, not stored: a number with no URL of its own uses the machine's
    // exposure, and this is the string an operator pastes into a carrier
    // console -- so it has to be the one the verifier will reconstruct.
    let url = store
        .telecom_callback_base(&account)
        .map(|base| crate::daemon::telephony::callback_url(&base, account_id));
    if json {
        println!(
            "{}",
            serde_json::json!({ "account_id": account_id, "callback_url": url })
        );
        return Ok(());
    }
    match url {
        Some(url) => println!("{url}"),
        None => println!(
            "{account_id} has no public URL configured, so its carrier has nowhere to post to."
        ),
    }
    Ok(())
}

fn remove(account_id: &str) -> Result<(), String> {
    let mut store = store()?;
    let account = store
        .telecom_account(account_id)?
        .ok_or_else(|| format!("No such account '{account_id}'"))?;
    if let Some(reference) = &account.credential_ref {
        // Best effort: an account the operator asked to remove must go even if
        // the keychain entry is already gone.
        let _ = KeyringChannelSecrets.delete(reference);
    }
    store.delete_telecom_account(account_id)?;
    println!("Removed {account_id}.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> TelecomAccountRecord {
        TelecomAccountRecord {
            account_id: "tel-1".into(),
            kind: TelecomKind::Twilio,
            label: "Support line".into(),
            enabled: true,
            carrier_account_id: "AC123".into(),
            from_number: "+15550000000".into(),
            credential_ref: Some("telecom:tel-1".into()),
            public_base_url: Some("https://calls.example.test".into()),
            non_secret_config: serde_json::json!({ "webhook_public_key": "abc" }),
            inbound_policy: InboundCallPolicy::Answer,
            outbound_approval: OutboundCallApproval::Approval,
            limits: CallLimits::default(),
            health: ChannelHealth {
                state: HealthState::Connected,
                detail: None,
                last_error: None,
                probed_at_ms: 1_700_000_000_000,
            },
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn the_json_view_reports_that_a_credential_exists_and_never_what_it_is() {
        let value = account_json(&account());

        assert_eq!(value["has_credential"], true);
        let rendered = value.to_string();
        assert!(
            !rendered.contains("telecom:tel-1"),
            "not even the keychain entry name is published: {rendered}"
        );
        assert_eq!(value["limits"]["max_concurrent_calls"], 1);
        assert_eq!(value["limits"]["recording_enabled"], false);
    }

    #[test]
    fn a_number_that_is_not_international_is_refused_before_anything_is_stored() {
        let error = add(
            "twilio",
            "Support",
            "AC123",
            "5551234567",
            None,
            None,
            false,
        )
        .expect_err("refused");
        assert!(error.contains("international format"));
    }

    #[test]
    fn an_unknown_carrier_names_the_ones_that_exist() {
        let error = add(
            "carrier-pigeon",
            "Support",
            "AC1",
            "+15550000000",
            None,
            None,
            false,
        )
        .expect_err("refused");
        assert!(error.contains("twilio"));
    }

    #[test]
    fn a_public_url_has_to_be_a_url() {
        let error = add(
            "twilio",
            "Support",
            "AC1",
            "+15550000000",
            Some("calls.example.test"),
            None,
            false,
        )
        .expect_err("refused");
        assert!(error.contains("https://"));
    }
}

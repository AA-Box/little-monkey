//! Opt-in live tests against real provider accounts.
//!
//! Nothing here runs in CI: every test resolves its credentials from the
//! environment and passes silently when they are absent. An operator who
//! wants to prove a provider end to end supplies their OWN test account and
//! an explicitly configured test destination:
//!
//! ```text
//! LM_TEST_TELEGRAM_BOT_TOKEN=...   LM_TEST_TELEGRAM_CHAT_ID=...
//! LM_TEST_DISCORD_BOT_TOKEN=...    LM_TEST_DISCORD_CHANNEL_ID=...
//! LM_TEST_SLACK_BOT_TOKEN=xoxb-... LM_TEST_SLACK_APP_TOKEN=xapp-... LM_TEST_SLACK_CHANNEL_ID=...
//!
//! cargo test --bin monkey-cli -- daemon::live_smoke --nocapture
//! ```
//!
//! What is here is the **outbound transport smoke**, and it is deliberately
//! one narrow question. With a token set, the test probes (a read-only
//! identity call). With the matching destination variable also set, it queues
//! one message through the same `plan_send`/`queue_send` seam the
//! `send_message` tool uses and drains it through the production
//! `drain_outbox_once` — the same path every normal reply takes — asserting
//! the provider returned a message id. Nothing calls the adapter send
//! primitive directly, and nothing reconstructs an outbox row by hand.
//!
//! The inbound half, and the agent behind it, are **not** here. They used to
//! be, in an interactive test whose run queue recorded submissions rather than
//! executing them and whose reply was written by the test rather than by a
//! model. That proved less than its name suggested, so it is gone:
//! [`super::live_agent_e2e`] now runs the whole path against the same real
//! accounts — real inbound, real `DaemonChannelQueue`, a real daemon child,
//! the real agent loop, and a reply the provider itself is asked to confirm.
//!
//! The tests fail closed: no destination, no message, ever. No credential is
//! bundled, defaulted, or read from any file; the environment is the single
//! source.

use std::sync::Arc;

use super::adapters::discord::DiscordAdapter;
use super::adapters::slack::SlackAdapter;
use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_restart_tests::{seeded_store, temp_daemon_paths};
use super::channel_store::{ChannelAccountRecord, EventDirection};
use super::channel_tool::{
    plan_send, queue_send, ChannelSendRequest, SendAuthority, SendInvocation,
};
use super::channel_worker::drain_outbox_once;
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind, HealthState};

const ACCOUNT_ID: &str = "live-smoke";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn account(kind: ChannelKind) -> ChannelAccountRecord {
    ChannelAccountRecord {
        account_id: ACCOUNT_ID.into(),
        kind,
        label: "Live smoke".into(),
        enabled: true,
        non_secret_config: serde_json::json!({}),
        credential_ref: Some(ACCOUNT_ID.into()),
        access_policy: Default::default(),
        health: ChannelHealth::error(0, "unused"),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

/// A run-queue double that only records what was submitted. The run itself is
/// this test; everything around it is production code.
/// Level A: queue one message through the same `plan_send`/`queue_send` seam
/// the `send_message` tool uses — an ad-hoc send under an explicit account
/// grant, since no run is behind it — and drain it through the production
/// `drain_outbox_once` with the real adapter. Panics unless the provider
/// answered with a message id.
async fn outbox_smoke(kind: ChannelKind, adapter: Arc<dyn ChannelAdapter>, destination: &str) {
    let mut store = seeded_store(ACCOUNT_ID, kind);
    let paths = temp_daemon_paths();
    let now = now_ms();
    let request = ChannelSendRequest {
        account_id: Some(ACCOUNT_ID.into()),
        conversation_id: Some(destination.to_string()),
        text: "Little Monkey live smoke test: this message proves the outbound path.".into(),
        ..Default::default()
    };
    // No run, no origin: the authority names the account explicitly, the
    // same shape a cross-account grant takes.
    let authority = SendAuthority {
        reply: false,
        cross_conversation: false,
        accounts: vec![ACCOUNT_ID.into()],
    };
    let plan = plan_send(&request, &authority, None).expect("the smoke send is allowed");
    // No run and no tool call behind this send, so it carries no durable
    // invocation: nothing will ever legitimately resubmit it.
    queue_send(
        &mut store,
        &paths,
        &request,
        &plan,
        None,
        &SendInvocation::default(),
        now,
    )
    .expect("enqueue");
    let mut adapters: std::collections::BTreeMap<String, Arc<dyn ChannelAdapter>> =
        std::collections::BTreeMap::new();
    adapters.insert(ACCOUNT_ID.to_string(), adapter);
    let report = drain_outbox_once(&mut store, &adapters, now)
        .await
        .expect("drain");
    assert_eq!(
        report.sent,
        1,
        "{} live smoke: the outbox drain did not deliver the message: {report:?}",
        kind.as_str()
    );
    let provider_id = store
        .recent_channel_events(ACCOUNT_ID, 10)
        .expect("events")
        .into_iter()
        .find(|event| event.direction == EventDirection::Outbound)
        .expect("an outbound event")
        .provider_event_id;
    assert!(
        !provider_id.starts_with("local:"),
        "{} accepted the message but returned no id",
        kind.as_str()
    );
    eprintln!(
        "{} live smoke: delivered via the outbox, provider id {provider_id}",
        kind.as_str()
    );
}

// ---------------------------------------------------------------------------
// Telegram
// ---------------------------------------------------------------------------

fn telegram_adapter() -> Option<Arc<dyn ChannelAdapter>> {
    let token = env("LM_TEST_TELEGRAM_BOT_TOKEN")?;
    let record = account(ChannelKind::Telegram);
    Some(Arc::new(
        TelegramAdapter::new(&AdapterConfig {
            account: &record,
            secret: token,
        })
        .expect("adapter"),
    ))
}

#[tokio::test]
async fn telegram_live_outbox_smoke() {
    let Some(adapter) = telegram_adapter() else {
        eprintln!("telegram live smoke: skipped (LM_TEST_TELEGRAM_BOT_TOKEN not set)");
        return;
    };
    let health = adapter.probe().await;
    assert_eq!(
        health.state,
        HealthState::Connected,
        "telegram probe did not connect: {:?} / {:?}",
        health.detail,
        health.last_error
    );
    // Fail closed: without an explicit destination nothing is ever sent.
    let Some(chat_id) = env("LM_TEST_TELEGRAM_CHAT_ID") else {
        eprintln!("telegram live smoke: probe ok; no LM_TEST_TELEGRAM_CHAT_ID, not sending");
        return;
    };
    outbox_smoke(ChannelKind::Telegram, adapter, &chat_id).await;
}

// ---------------------------------------------------------------------------
// Discord
// ---------------------------------------------------------------------------

fn discord_adapter() -> Option<Arc<dyn ChannelAdapter>> {
    let token = env("LM_TEST_DISCORD_BOT_TOKEN")?;
    let record = account(ChannelKind::Discord);
    Some(Arc::new(
        DiscordAdapter::new(&AdapterConfig {
            account: &record,
            secret: token,
        })
        .expect("adapter"),
    ))
}

#[tokio::test]
async fn discord_live_outbox_smoke() {
    let Some(adapter) = discord_adapter() else {
        eprintln!("discord live smoke: skipped (LM_TEST_DISCORD_BOT_TOKEN not set)");
        return;
    };
    // The REST probe alone: Connected requires a live gateway session, which
    // this test never starts, so the honest answers are Disconnected (never
    // started) — anything else means the credential itself failed.
    let health = adapter.probe().await;
    assert!(
        matches!(
            health.state,
            HealthState::Connected | HealthState::Degraded | HealthState::Disconnected
        ),
        "discord probe failed: {:?} / {:?}",
        health.detail,
        health.last_error
    );
    let Some(channel_id) = env("LM_TEST_DISCORD_CHANNEL_ID") else {
        eprintln!("discord live smoke: probe ok; no LM_TEST_DISCORD_CHANNEL_ID, not sending");
        return;
    };
    outbox_smoke(ChannelKind::Discord, adapter, &channel_id).await;
}

// ---------------------------------------------------------------------------
// Slack
// ---------------------------------------------------------------------------

fn slack_adapter() -> Option<Arc<dyn ChannelAdapter>> {
    let (bot_token, app_token) = (
        env("LM_TEST_SLACK_BOT_TOKEN")?,
        env("LM_TEST_SLACK_APP_TOKEN")?,
    );
    let record = account(ChannelKind::Slack);
    let secret = serde_json::json!({ "bot_token": bot_token, "app_token": app_token }).to_string();
    Some(Arc::new(
        SlackAdapter::new(&AdapterConfig {
            account: &record,
            secret,
        })
        .expect("adapter"),
    ))
}

#[tokio::test]
async fn slack_live_outbox_smoke() {
    let Some(adapter) = slack_adapter() else {
        eprintln!(
            "slack live smoke: skipped (LM_TEST_SLACK_BOT_TOKEN / LM_TEST_SLACK_APP_TOKEN not set)"
        );
        return;
    };
    // auth.test alone cannot be Connected any more — Socket Mode has not
    // started. Disconnected is the honest pre-start answer.
    let health = adapter.probe().await;
    assert!(
        matches!(
            health.state,
            HealthState::Connected | HealthState::Degraded | HealthState::Disconnected
        ),
        "slack probe failed: {:?} / {:?}",
        health.detail,
        health.last_error
    );
    let Some(channel_id) = env("LM_TEST_SLACK_CHANNEL_ID") else {
        eprintln!("slack live smoke: probe ok; no LM_TEST_SLACK_CHANNEL_ID, not sending");
        return;
    };
    outbox_smoke(ChannelKind::Slack, adapter, &channel_id).await;
}

//! Opt-in live smoke tests against real provider accounts.
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
//! With only the token set, a test probes (a read-only identity call) and
//! sends nothing. Sending requires the matching `*_CHAT_ID`/`*_CHANNEL_ID`
//! variable naming the destination — the tests fail closed: no destination,
//! no message, ever. No credential is bundled, defaulted, or read from any
//! file; the environment is the single source.

use super::adapters::discord::DiscordAdapter;
use super::adapters::slack::SlackAdapter;
use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_store::ChannelAccountRecord;
use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, HealthState, OutboundMessage, SendOutcome,
};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn account(kind: ChannelKind) -> ChannelAccountRecord {
    ChannelAccountRecord {
        account_id: "live-smoke".into(),
        kind,
        label: "Live smoke".into(),
        enabled: true,
        non_secret_config: serde_json::json!({}),
        credential_ref: Some("live-smoke".into()),
        access_policy: Default::default(),
        health: ChannelHealth::error(0, "unused"),
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn message(kind: ChannelKind, destination: &str) -> OutboundMessage {
    OutboundMessage {
        account_id: "live-smoke".into(),
        kind,
        conversation_id: destination.to_string(),
        thread_id: None,
        text: "Little Monkey live smoke test: this message proves the outbound path.".into(),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        idempotency_key: format!("live-smoke-{}", std::process::id()),
    }
}

fn assert_connected(kind: &str, health: &ChannelHealth) {
    assert_eq!(
        health.state,
        HealthState::Connected,
        "{kind} probe did not connect: {:?} / {:?}",
        health.detail,
        health.last_error
    );
}

fn assert_sent(kind: &str, outcome: &SendOutcome) {
    match outcome {
        SendOutcome::Sent {
            provider_message_id,
        } => {
            eprintln!("{kind} live smoke: sent, provider id {provider_message_id:?}");
            assert!(
                provider_message_id.is_some(),
                "{kind} accepted the message but returned no id"
            );
        }
        other => panic!("{kind} live smoke send failed: {other:?}"),
    }
}

#[tokio::test]
async fn telegram_live_roundtrip() {
    let Some(token) = env("LM_TEST_TELEGRAM_BOT_TOKEN") else {
        eprintln!("telegram live smoke: skipped (LM_TEST_TELEGRAM_BOT_TOKEN not set)");
        return;
    };
    let record = account(ChannelKind::Telegram);
    let adapter = TelegramAdapter::new(&AdapterConfig {
        account: &record,
        secret: token,
    })
    .expect("adapter");
    assert_connected("telegram", &adapter.probe().await);
    // Fail closed: without an explicit destination nothing is ever sent.
    let Some(chat_id) = env("LM_TEST_TELEGRAM_CHAT_ID") else {
        eprintln!("telegram live smoke: probe ok; no LM_TEST_TELEGRAM_CHAT_ID, not sending");
        return;
    };
    let outcome = adapter
        .send(&message(ChannelKind::Telegram, &chat_id))
        .await;
    assert_sent("telegram", &outcome);
}

#[tokio::test]
async fn discord_live_roundtrip() {
    let Some(token) = env("LM_TEST_DISCORD_BOT_TOKEN") else {
        eprintln!("discord live smoke: skipped (LM_TEST_DISCORD_BOT_TOKEN not set)");
        return;
    };
    let record = account(ChannelKind::Discord);
    let adapter = DiscordAdapter::new(&AdapterConfig {
        account: &record,
        secret: token,
    })
    .expect("adapter");
    // The REST probe alone: Connected requires a live gateway too, which this
    // adapter has not started, so accept Connected or the honest Degraded.
    let health = adapter.probe().await;
    assert!(
        matches!(health.state, HealthState::Connected | HealthState::Degraded),
        "discord probe failed: {:?} / {:?}",
        health.detail,
        health.last_error
    );
    let Some(channel_id) = env("LM_TEST_DISCORD_CHANNEL_ID") else {
        eprintln!("discord live smoke: probe ok; no LM_TEST_DISCORD_CHANNEL_ID, not sending");
        return;
    };
    let outcome = adapter
        .send(&message(ChannelKind::Discord, &channel_id))
        .await;
    assert_sent("discord", &outcome);
}

#[tokio::test]
async fn slack_live_roundtrip() {
    let (Some(bot_token), Some(app_token)) = (
        env("LM_TEST_SLACK_BOT_TOKEN"),
        env("LM_TEST_SLACK_APP_TOKEN"),
    ) else {
        eprintln!(
            "slack live smoke: skipped (LM_TEST_SLACK_BOT_TOKEN / LM_TEST_SLACK_APP_TOKEN not set)"
        );
        return;
    };
    let record = account(ChannelKind::Slack);
    let secret = serde_json::json!({ "bot_token": bot_token, "app_token": app_token }).to_string();
    let adapter = SlackAdapter::new(&AdapterConfig {
        account: &record,
        secret,
    })
    .expect("adapter");
    let health = adapter.probe().await;
    assert!(
        matches!(health.state, HealthState::Connected | HealthState::Degraded),
        "slack probe failed: {:?} / {:?}",
        health.detail,
        health.last_error
    );
    let Some(channel_id) = env("LM_TEST_SLACK_CHANNEL_ID") else {
        eprintln!("slack live smoke: probe ok; no LM_TEST_SLACK_CHANNEL_ID, not sending");
        return;
    };
    let outcome = adapter
        .send(&message(ChannelKind::Slack, &channel_id))
        .await;
    assert_sent("slack", &outcome);
}

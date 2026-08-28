//! Live-account acceptance: the whole path, against a real provider.
//!
//! [`super::channel_agent_e2e`] proves the architecture against protocol
//! fixtures. [`super::live_smoke`] proves a real account's outbound transport.
//! Neither one proves the two together, and this module is the join:
//!
//! ```text
//! a person sends a message to the operator's own real account
//!   → production adapter (poll), holding the real credential
//!   → durable channel event
//!   → production channel ingress → durable turn → frozen execution context
//!   → real DaemonChannelQueue::submit
//!   → real DaemonEngine tick → real task-run child process
//!   → real agent loop → model transport → the agent dispatches the tool call
//!   → production channel_tool::send_message → durable outbox row
//!   → production drain_outbox_once → production adapter
//!   → the real provider accepts it
//!   → the provider is asked, over its own API, whether it holds that reply
//! ```
//!
//! Exactly one thing here is not real: the model's HTTP transport, which is a
//! deterministic loopback origin named through `target.local_url` — the same
//! seam a recipe uses to reach llama.cpp or LM Studio. It cannot send a
//! message; the reply exists only because the production agent loop parsed the
//! tool call and dispatched it. Everything else — account, credential,
//! transport, daemon, agent process, outbox, provider — is production.
//!
//! Nothing runs without credentials in the environment. Every test here returns
//! before it does anything when its variables are absent, so CI and every
//! contributor's `cargo test` skip them. No token, account, channel or
//! destination is bundled or defaulted anywhere in this tree.
//!
//! ```text
//! LM_LIVE_TELEGRAM_BOT_TOKEN=…
//!   cargo test --bin monkey-cli daemon::live_agent_e2e::telegram -- --nocapture
//!
//! LM_LIVE_DISCORD_BOT_TOKEN=…
//!   cargo test --bin monkey-cli daemon::live_agent_e2e::discord -- --nocapture
//!
//! LM_LIVE_SLACK_BOT_TOKEN=xoxb-… LM_LIVE_SLACK_APP_TOKEN=xapp-…
//!   cargo test --bin monkey-cli daemon::live_agent_e2e::slack -- --nocapture
//! ```
//!
//! The run prints a nonce and waits for the operator to send it to the
//! account. The reply the agent produces carries that nonce back, which is what
//! makes the provider-side check meaningful: the text the provider holds is
//! the text the model asked for, in the conversation the message came from.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use little_monkey_lib::channels::types::ChannelKind;

use super::adapters::discord::DiscordAdapter;
use super::adapters::slack::SlackAdapter;
use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_agent_e2e::{
    account_record, execute_turn_through_the_daemon, in_isolated_process, isolation_is_real,
    now_ms, seed_channel, sse_response, write_recipe, HttpFixture, ACCOUNT_ID, SKIPPED,
};
use super::channel_store::EventDirection;
use super::channel_worker::poll_account_once;
use super::store::{DaemonConfig, DaemonPaths, DaemonStore, JobState};
use super::DaemonChannelQueue;

/// How long the operator has to send the nonce before the run gives up. Long
/// because a person has to read the instruction, switch to a client and type;
/// bounded because a test that waits forever is a test nobody runs twice.
const INBOUND_WAIT: Duration = Duration::from_secs(240);

/// How long to wait for a typed acknowledgement when the provider cannot be
/// asked about the reply itself.
const ACK_WAIT: Duration = Duration::from_secs(180);

/// What the deterministic model asks `send_message` to say, ahead of the
/// nonce. Finding this on the provider's own side is the proof.
const REPLY_PREFIX: &str = "little-monkey live acceptance reply";

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

// ---------------------------------------------------------------------------
// What a provider has to supply
// ---------------------------------------------------------------------------

/// The provider-specific half of a live run. Everything else in this module is
/// the same for every provider, which is the point of the split.
struct LiveAccount {
    kind: ChannelKind,
    /// The production adapter, holding the operator's own credential.
    adapter: Arc<dyn ChannelAdapter>,
    /// The credential again, for the provider-side observation call. The
    /// adapter does not hand its own secret back out, and asking the provider
    /// what it holds is a second, separate call.
    secret: String,
    /// Printed to the operator: where this account can be reached.
    where_to_send: String,
}

fn telegram_account() -> Option<LiveAccount> {
    let token = env("LM_LIVE_TELEGRAM_BOT_TOKEN")?;
    let record = account_record(ChannelKind::Telegram, now_ms());
    let adapter = TelegramAdapter::new(&AdapterConfig {
        account: &record,
        secret: token.clone(),
    })
    .expect("telegram adapter");
    Some(LiveAccount {
        kind: ChannelKind::Telegram,
        adapter: Arc::new(adapter),
        secret: token,
        where_to_send: "your bot's Telegram chat".to_string(),
    })
}

fn discord_account() -> Option<LiveAccount> {
    let token = env("LM_LIVE_DISCORD_BOT_TOKEN")?;
    let record = account_record(ChannelKind::Discord, now_ms());
    let adapter = DiscordAdapter::new(&AdapterConfig {
        account: &record,
        secret: token.clone(),
    })
    .expect("discord adapter");
    Some(LiveAccount {
        kind: ChannelKind::Discord,
        adapter: Arc::new(adapter),
        secret: token,
        where_to_send: "a channel your bot can read".to_string(),
    })
}

fn slack_account() -> Option<LiveAccount> {
    let (bot_token, app_token) = (
        env("LM_LIVE_SLACK_BOT_TOKEN")?,
        env("LM_LIVE_SLACK_APP_TOKEN")?,
    );
    let record = account_record(ChannelKind::Slack, now_ms());
    let secret = serde_json::json!({ "bot_token": bot_token, "app_token": app_token }).to_string();
    let adapter = SlackAdapter::new(&AdapterConfig {
        account: &record,
        secret: secret.clone(),
    })
    .expect("slack adapter");
    Some(LiveAccount {
        kind: ChannelKind::Slack,
        adapter: Arc::new(adapter),
        secret,
        where_to_send: "a channel your app is in".to_string(),
    })
}

// ---------------------------------------------------------------------------
// The deterministic model
// ---------------------------------------------------------------------------

/// An OpenAI-compatible origin that answers with one `send_message` tool call
/// carrying the nonce it was asked about.
///
/// Deterministic rather than a real model because a live acceptance run must
/// not depend on a model account, and because a reply nobody can predict
/// cannot be looked for on the provider's side. It is still the production
/// agent loop that decides to call the tool, dispatches it and writes the
/// outbox row — this only chooses the words.
fn live_model_fixture(nonce: &str) -> HttpFixture {
    let nonce = nonce.to_string();
    HttpFixture::spawn(move |head, body, _index| {
        if !head.contains("/chat/completions") {
            return super::channel_agent_e2e::json_response(
                r#"{"error":"unexpected model route"}"#,
            );
        }
        // The second call carries the tool result, and answering it with
        // another tool call would loop forever.
        if body.contains("\"role\":\"tool\"") {
            return sse_response(&[
                serde_json::json!({
                    "choices": [{ "index": 0, "delta": { "content": "sent" } }]
                }),
                serde_json::json!({
                    "choices": [{ "index": 0, "delta": {}, "finish_reason": "stop" }]
                }),
            ]);
        }
        let arguments =
            serde_json::json!({ "text": format!("{REPLY_PREFIX} {nonce}") }).to_string();
        sse_response(&[
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [{
                        "index": 0,
                        "id": "call_live_1",
                        "type": "function",
                        "function": { "name": "send_message", "arguments": arguments },
                    }] },
                }]
            }),
            serde_json::json!({
                "choices": [{ "index": 0, "delta": {}, "finish_reason": "tool_calls" }]
            }),
        ])
    })
    .expect("bind the model origin")
}

// ---------------------------------------------------------------------------
// Asking the provider what it holds
// ---------------------------------------------------------------------------

/// What the provider said when asked about the reply.
enum Observed {
    /// The provider returned the message, and this is the text it holds.
    Confirmed(String),
    /// The provider cannot be asked with this credential. Carries the reason,
    /// which is shown to the operator before the manual acknowledgement.
    Unsupported(String),
}

/// The client every provider-side read is made with.
///
/// `egress::hardened()` rather than a bare client for the same reason the
/// adapters use it: these calls carry the account's own credential to a third
/// party, so they want the connect and read budgets and the redirect policy
/// that will not hand that credential to a host the response picked.
fn hardened_client() -> reqwest::Client {
    little_monkey_lib::egress::hardened()
        .build()
        .expect("a hardened client builds")
}

/// Ask the provider, over its own API, whether it holds the reply this run
/// sent — content included.
///
/// This is the difference between "our outbox says we sent it" and "the
/// provider says it has it". A local provider id proves the API accepted a
/// call; only this proves the message exists on the far side.
async fn observe_reply(
    account: &LiveAccount,
    conversation_id: &str,
    provider_message_id: &str,
) -> Observed {
    match account.kind {
        ChannelKind::Telegram => {
            observe_telegram(&account.secret, conversation_id, provider_message_id).await
        }
        ChannelKind::Discord => {
            observe_discord(&account.secret, conversation_id, provider_message_id).await
        }
        ChannelKind::Slack => {
            observe_slack(&account.secret, conversation_id, provider_message_id).await
        }
        other => Observed::Unsupported(format!(
            "no provider-side read is implemented for {}",
            other.label()
        )),
    }
}

/// Telegram: forward the reply back into the same chat and read the copy.
///
/// The Bot API has no "read this message" call — a bot cannot fetch chat
/// history — but `forwardMessage` returns the forwarded message *including the
/// original text*, which is exactly the question being asked. The copy is
/// deleted afterwards so the operator's chat is left as it was found.
async fn observe_telegram(token: &str, chat_id: &str, message_id: &str) -> Observed {
    let client = hardened_client();
    let forwarded = client
        .post(format!(
            "https://api.telegram.org/bot{token}/forwardMessage"
        ))
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "from_chat_id": chat_id,
            "message_id": message_id.parse::<i64>().unwrap_or_default(),
        }))
        .send()
        .await;
    let payload = match forwarded {
        Ok(response) => response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_default(),
        Err(error) => return Observed::Unsupported(format!("forwardMessage failed: {error}")),
    };
    if payload["ok"].as_bool() != Some(true) {
        return Observed::Unsupported(format!(
            "Telegram refused forwardMessage: {}",
            payload["description"].as_str().unwrap_or("no description")
        ));
    }
    let text = payload["result"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // Tidy up the copy this check made. Best effort: a failure here says
    // nothing about the reply, and the assertion has what it needs already.
    if let Some(copy_id) = payload["result"]["message_id"].as_i64() {
        let _ = client
            .post(format!("https://api.telegram.org/bot{token}/deleteMessage"))
            .json(&serde_json::json!({ "chat_id": chat_id, "message_id": copy_id }))
            .send()
            .await;
    }
    Observed::Confirmed(text)
}

/// Discord: read the message straight back out of the channel.
async fn observe_discord(token: &str, channel_id: &str, message_id: &str) -> Observed {
    let response = hardened_client()
        .get(format!(
            "https://discord.com/api/v10/channels/{channel_id}/messages/{message_id}"
        ))
        .header("Authorization", format!("Bot {token}"))
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            let payload = response
                .json::<serde_json::Value>()
                .await
                .unwrap_or_default();
            Observed::Confirmed(payload["content"].as_str().unwrap_or_default().to_string())
        }
        Ok(response) => Observed::Unsupported(format!(
            "Discord answered {} — the bot may lack Read Message History here",
            response.status()
        )),
        Err(error) => Observed::Unsupported(format!("Discord read failed: {error}")),
    }
}

/// Slack: read the one message at that timestamp out of the conversation.
async fn observe_slack(secret: &str, channel_id: &str, ts: &str) -> Observed {
    let bot_token = serde_json::from_str::<serde_json::Value>(secret)
        .ok()
        .and_then(|value| value["bot_token"].as_str().map(str::to_string))
        .unwrap_or_default();
    let response = hardened_client()
        .get("https://slack.com/api/conversations.history")
        .header("Authorization", format!("Bearer {bot_token}"))
        .query(&[
            ("channel", channel_id),
            ("latest", ts),
            ("oldest", ts),
            ("inclusive", "true"),
            ("limit", "1"),
        ])
        .send()
        .await;
    let payload = match response {
        Ok(response) => response
            .json::<serde_json::Value>()
            .await
            .unwrap_or_default(),
        Err(error) => return Observed::Unsupported(format!("Slack read failed: {error}")),
    };
    if payload["ok"].as_bool() != Some(true) {
        // `missing_scope` is the ordinary case: reading history is a scope an
        // operator may not have granted, and the run still has to end honestly.
        return Observed::Unsupported(format!(
            "Slack refused conversations.history: {}",
            payload["error"].as_str().unwrap_or("no error given")
        ));
    }
    let text = payload["messages"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Observed::Confirmed(text)
}

/// The fallback when the provider cannot be asked: the operator says whether
/// the reply arrived. Blocks on a typed answer rather than assuming one.
fn ask_the_operator(nonce: &str, reason: &str) {
    eprintln!("==============================================================");
    eprintln!("  The provider could not be asked about the reply: {reason}");
    eprintln!("  Look at the conversation. Did a reply containing");
    eprintln!("      {REPLY_PREFIX} {nonce}");
    eprintln!(
        "  arrive? Type 'yes' and press return within {}s.",
        ACK_WAIT.as_secs()
    );
    eprintln!("==============================================================");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let _ = sender.send(line);
        }
    });
    let answer = receiver
        .recv_timeout(ACK_WAIT)
        .unwrap_or_else(|_| String::new());
    assert!(
        answer.trim().eq_ignore_ascii_case("yes"),
        "the reply was not confirmed as delivered (answer: {:?})",
        answer.trim()
    );
}

// ---------------------------------------------------------------------------
// The provider-independent run
// ---------------------------------------------------------------------------

async fn run_live_acceptance(root: &Path, account: LiveAccount) {
    if !isolation_is_real(root) {
        println!(
            "{SKIPPED} on this platform: the app-data directory is not resolved from the \
             environment, so the run could not be kept out of the real profile"
        );
        return;
    }

    let nonce = format!("lm-live-{}-{}", std::process::id(), now_ms());
    let model = live_model_fixture(&nonce);

    // ---- an isolated profile with one route, pointed at the deterministic
    // model origin. Identical to the fixture acceptance test's setup.
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots().expect("config roots");
    write_recipe(&roots.authored, &workspace, &model.base);
    let paths = DaemonPaths::under(&roots.legacy);
    paths.ensure().expect("daemon paths");
    let config = DaemonConfig::default();
    config.save(&paths).expect("daemon config");

    let mut store = DaemonStore::open(&paths).expect("daemon store");
    seed_channel(&mut store, account.kind, now_ms());

    eprintln!("==============================================================");
    eprintln!("  {} live acceptance", account.kind.label());
    eprintln!("  Send a message containing exactly this nonce to");
    eprintln!("  {} now:", account.where_to_send);
    eprintln!();
    eprintln!("      {nonce}");
    eprintln!();
    eprintln!("  Waiting up to {}s ...", INBOUND_WAIT.as_secs());
    eprintln!("==============================================================");

    // ---- inbound: the production adapter against the real provider, the
    // production ingress, and the real queue that registers a durable run
    // through an actual monkey-cli child.
    let queue = DaemonChannelQueue::new(paths.clone());
    let deadline = Instant::now() + INBOUND_WAIT;
    let (job_id, conversation_id) = loop {
        assert!(
            Instant::now() < deadline,
            "{} live acceptance timed out: no message containing '{nonce}' arrived",
            account.kind.as_str()
        );
        poll_account_once(
            &mut store,
            &queue,
            ACCOUNT_ID,
            account.adapter.as_ref(),
            now_ms(),
        )
        .await
        .expect("poll");
        let matched = store
            .recent_channel_events(ACCOUNT_ID, 50)
            .expect("events")
            .into_iter()
            .find(|event| {
                event.direction == EventDirection::Inbound
                    && event.envelope_json.contains(&nonce)
                    && event.job_id.is_some()
            });
        if let Some(event) = matched {
            // The invariant the inbound path exists to keep, checked against a
            // real provider: an accepted event owns the turn it became.
            let ingress_id = event
                .ingress_id
                .clone()
                .expect("an accepted event with no durable turn behind it");
            let conversation = serde_json::from_str::<serde_json::Value>(&event.envelope_json)
                .ok()
                .and_then(|envelope| {
                    envelope["conversation"]["conversation_id"]
                        .as_str()
                        .map(str::to_string)
                })
                .expect("the envelope names its conversation");
            eprintln!(
                "{}: durable event {} accepted as turn {ingress_id}",
                account.kind.as_str(),
                event.provider_event_id
            );
            break (event.job_id.expect("job id"), conversation);
        }
    };

    // ---- the durable turn is real, and the run behind it is the queue's.
    let job = store
        .get_job(&job_id)
        .expect("job read")
        .expect("the daemon queue has the job");
    let run_id = job.run_id.clone().expect("a durable run id");
    assert_eq!(job.state, JobState::Queued, "the job was not queued");
    assert_eq!(
        store.ingress_reply_grant_for_job(&job_id).expect("grant"),
        Some(true),
        "the frozen route did not grant this turn a reply"
    );
    assert!(
        store
            .channel_origin_for_job(&job_id)
            .expect("origin read")
            .is_some(),
        "the job has no channel origin for send_message to answer"
    );

    // ---- the shared middle: real daemon, real agent, real outbox drain
    // through the real adapter. Nothing is injected here.
    let proof = execute_turn_through_the_daemon(
        &paths,
        &config,
        ACCOUNT_ID,
        &job_id,
        &run_id,
        &account.adapter,
        &model,
    )
    .await;

    // The model was asked about *this* turn — the operator's own message
    // reached the agent rather than some other text.
    assert!(
        model
            .requests()
            .iter()
            .any(|request| request.contains(&nonce)),
        "the agent never sent the inbound message to the model"
    );
    assert!(
        !proof.provider_message_id.starts_with("local:"),
        "the provider accepted the reply but returned no id of its own"
    );

    // ---- and now the provider's own answer about what it holds.
    match observe_reply(&account, &conversation_id, &proof.provider_message_id).await {
        Observed::Confirmed(text) => {
            assert!(
                text.contains(&nonce) && text.contains(REPLY_PREFIX),
                "the provider holds a different message at {}: {text:?}",
                proof.provider_message_id
            );
            eprintln!(
                "{}: the provider confirms it holds the agent's reply {} — {text:?}",
                account.kind.as_str(),
                proof.provider_message_id
            );
        }
        Observed::Unsupported(reason) => ask_the_operator(&nonce, &reason),
    }

    // ---- one of everything: one message in, one run, one reply out.
    let store = DaemonStore::open(&paths).expect("store reopen");
    let all = store.recent_channel_events(ACCOUNT_ID, 50).expect("events");
    let inbound = all
        .iter()
        .filter(|event| event.direction == EventDirection::Inbound && event.job_id.is_some())
        .count();
    let outbound = all
        .iter()
        .filter(|event| event.direction == EventDirection::Outbound)
        .count();
    assert_eq!(inbound, 1, "more than one inbound turn was accepted");
    assert_eq!(outbound, 1, "the agent's one reply became {outbound} sends");
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

/// A real Telegram message becomes a real daemon run, and Telegram itself
/// confirms it holds the reply the agent produced.
#[test]
fn telegram_message_becomes_an_agent_reply_on_the_real_provider() {
    if env("LM_LIVE_TELEGRAM_BOT_TOKEN").is_none() {
        eprintln!("telegram live acceptance: skipped (LM_LIVE_TELEGRAM_BOT_TOKEN not set)");
        return;
    }
    in_isolated_process(
        "live_agent_e2e",
        "telegram_message_becomes_an_agent_reply_on_the_real_provider",
        |root| {
            Box::pin(async move {
                let account = telegram_account().expect("telegram credentials");
                run_live_acceptance(&root, account).await;
            })
        },
    );
}

/// The same path over the real Discord Gateway, with the reply read back out
/// of the channel it was sent to.
#[test]
fn discord_message_becomes_an_agent_reply_on_the_real_provider() {
    if env("LM_LIVE_DISCORD_BOT_TOKEN").is_none() {
        eprintln!("discord live acceptance: skipped (LM_LIVE_DISCORD_BOT_TOKEN not set)");
        return;
    }
    in_isolated_process(
        "live_agent_e2e",
        "discord_message_becomes_an_agent_reply_on_the_real_provider",
        |root| {
            Box::pin(async move {
                let account = discord_account().expect("discord credentials");
                run_live_acceptance(&root, account).await;
            })
        },
    );
}

/// The same path over real Slack Socket Mode. Reading the reply back needs a
/// history scope; without it the run ends on an explicit acknowledgement
/// rather than a claim.
#[test]
fn slack_message_becomes_an_agent_reply_on_the_real_provider() {
    if env("LM_LIVE_SLACK_BOT_TOKEN").is_none() || env("LM_LIVE_SLACK_APP_TOKEN").is_none() {
        eprintln!(
            "slack live acceptance: skipped (LM_LIVE_SLACK_BOT_TOKEN / LM_LIVE_SLACK_APP_TOKEN \
             not set)"
        );
        return;
    }
    in_isolated_process(
        "live_agent_e2e",
        "slack_message_becomes_an_agent_reply_on_the_real_provider",
        |root| {
            Box::pin(async move {
                let account = slack_account().expect("slack credentials");
                run_live_acceptance(&root, account).await;
            })
        },
    );
}

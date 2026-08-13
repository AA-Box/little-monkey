//! Restart and durability tests for the real provider adapters.
//!
//! Every test here drives the SAME adapter structs release builds ship —
//! `TelegramAdapter`, `DiscordAdapter`, `SlackAdapter` — against loopback
//! servers speaking each provider's wire protocol, through the same
//! `poll_account_once`/`ingest_batch` code the daemon runs. Nothing in this
//! file is a stand-in for an adapter; the only fakes are the provider
//! endpoints themselves and the run queue behind the ingress gate.

use std::sync::{Arc, Mutex as StdMutex};

use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::adapters::discord::DiscordAdapter;
use super::adapters::slack::SlackAdapter;
use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_store::ChannelAccountRecord;
use super::channel_worker::{ingest_batch, poll_account_once, RunQueue};
use super::store::DaemonStore;
use little_monkey_lib::channels::ingress::{ConversationIngress, FrozenExecutionContext};
use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind};

const NOW: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FakeQueue {
    submitted: StdMutex<Vec<String>>,
}

impl RunQueue for FakeQueue {
    fn freeze_execution(
        &self,
        ingress: &ConversationIngress,
    ) -> Result<FrozenExecutionContext, String> {
        Ok(super::channel_worker::test_frozen_execution(ingress))
    }

    fn submit(
        &self,
        ingress: &ConversationIngress,
        _params: Vec<String>,
    ) -> Result<String, String> {
        let job_id = ingress.deterministic_job_id();
        self.submitted.lock().unwrap().push(job_id.clone());
        Ok(job_id)
    }
}

/// An in-memory store with one enabled account and an open route to a chat
/// recipe — the minimum for `plan_channel_ingress` to accept a stranger.
fn seeded_store(account_id: &str, kind: ChannelKind) -> DaemonStore {
    let mut store = DaemonStore::open_in_memory().expect("open in-memory store");
    store
        .upsert_channel_account(&ChannelAccountRecord {
            account_id: account_id.into(),
            kind,
            label: "Restart test".into(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some(format!("test:{account_id}")),
            access_policy: ChannelAccessPolicy {
                direct: AccessPolicy::Open,
                group: AccessPolicy::Open,
                group_activation: GroupActivation::Always,
            },
            health: ChannelHealth::connected(NOW, None),
            created_at_ms: NOW,
            updated_at_ms: NOW,
        })
        .expect("account");
    store
        .insert_channel_route(&ChannelRoute {
            route_id: format!("route-{account_id}"),
            scope: RouteScope::account(account_id),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        })
        .expect("route");
    store
}

fn account_record(account_id: &str, kind: ChannelKind) -> ChannelAccountRecord {
    ChannelAccountRecord {
        account_id: account_id.into(),
        kind,
        label: "Restart test".into(),
        enabled: true,
        non_secret_config: serde_json::json!({}),
        credential_ref: Some(format!("test:{account_id}")),
        access_policy: ChannelAccessPolicy::default(),
        health: ChannelHealth::connected(NOW, None),
        created_at_ms: NOW,
        updated_at_ms: NOW,
    }
}

/// A loopback WebSocket server for gateway/socket-mode tests.
///
/// On each accepted connection it immediately sends `greetings[i]`, then
/// relays: every text frame the client sends is logged as `(connection, frame)`,
/// and every frame pushed through `inject` goes to the *current* connection.
struct WsFixture {
    url: String,
    received: Arc<StdMutex<Vec<(usize, Value)>>>,
    inject: mpsc::UnboundedSender<String>,
}

fn spawn_ws_fixture(greetings: Vec<Vec<String>>) -> WsFixture {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ws fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let received: Arc<StdMutex<Vec<(usize, Value)>>> = Arc::default();
    let (inject_tx, inject_rx) = mpsc::unbounded_channel::<String>();
    let inject_rx = Arc::new(tokio::sync::Mutex::new(inject_rx));
    let log = received.clone();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        for (index, greeting) in greetings.into_iter().enumerate() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            for frame in greeting {
                let _ = ws.send(Message::Text(frame.into())).await;
            }
            let mut inject = inject_rx.lock().await;
            loop {
                tokio::select! {
                    frame = ws.next() => match frame {
                        Some(Ok(Message::Text(text))) => {
                            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                                log.lock().unwrap().push((index, value));
                            }
                        }
                        Some(Ok(Message::Ping(payload))) => {
                            let _ = ws.send(Message::Pong(payload)).await;
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                    frame = inject.recv() => match frame {
                        Some(text) => { let _ = ws.send(Message::Text(text.into())).await; }
                        None => break,
                    },
                }
            }
        }
    });
    WsFixture {
        url: format!("ws://127.0.0.1:{port}"),
        received,
        inject: inject_tx,
    }
}

/// Wait until a logged frame satisfies `predicate`, or panic after `seconds`.
async fn wait_for_frame(
    received: &Arc<StdMutex<Vec<(usize, Value)>>>,
    seconds: u64,
    what: &str,
    predicate: impl Fn(usize, &Value) -> bool,
) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);
    loop {
        {
            let frames = received.lock().unwrap();
            if let Some((_, frame)) = frames
                .iter()
                .find(|(connection, frame)| predicate(*connection, frame))
            {
                return frame.clone();
            }
        }
        if std::time::Instant::now() > deadline {
            let frames = received.lock().unwrap();
            panic!("timed out waiting for {what}; saw: {frames:?}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn frame_op(frame: &Value) -> i64 {
    frame.get("op").and_then(Value::as_i64).unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Telegram: crash before cursor commit
// ---------------------------------------------------------------------------

const TELEGRAM_UPDATE: &str = r#"{
    "ok": true,
    "result": [{
        "update_id": 500,
        "message": {
            "message_id": 71,
            "date": 1700000000,
            "chat": {"id": 555, "type": "private"},
            "from": {"id": 555, "is_bot": false, "first_name": "Ada", "username": "ada"},
            "text": "ship it"
        }
    }]
}"#;

/// Telegram's `getMe` answer.
///
/// The adapter resolves its own identity once before its first poll — without
/// it `is_self` and `mentions_self` are false for everything — so every
/// Telegram fixture below serves this as its first response.
const TELEGRAM_GET_ME: &str = r#"{"ok":true,"result":{"id":42,"is_bot":true,"first_name":"Monkey","username":"little_monkey_bot"}}"#;

fn telegram_adapter(base: &str) -> TelegramAdapter {
    let account = account_record("acct-tg", ChannelKind::Telegram);
    TelegramAdapter::new(&AdapterConfig {
        account: &account,
        secret: "bot-token".into(),
    })
    .expect("adapter")
    .with_base_url(base)
}

/// receive update → crash before cursor commit → restart → same update
/// redelivered → one durable event, one run.
#[tokio::test]
async fn telegram_crash_before_cursor_commit_dedupes_the_redelivery() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    // First life of the daemon: the update arrives and is durably ingested,
    // but the process dies before the cursor is written.
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let mut batch = adapter.poll(None).await.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.cursor.as_deref(), Some("500"));
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-tg".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);
    // CRASH: the cursor was never persisted.
    assert_eq!(store.channel_cursor("acct-tg", "inbound").unwrap(), None);
    drop(adapter);

    // Second life: with no cursor, getUpdates is asked from the beginning and
    // Telegram redelivers the same update. This time the full production path
    // runs, and the event log collapses the duplicate.
    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll after restart");
    assert_eq!(report.duplicates, 1);
    assert_eq!(report.accepted, 0);

    // The redelivery poll carried no offset (nothing was committed), which is
    // what makes Telegram resend at all.
    let _get_me = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("getMe request");
    let request = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("getUpdates request");
    let request = String::from_utf8_lossy(&request);
    assert!(!request.contains("offset="), "{request}");

    // Exactly one durable event, one run, and the cursor is now committed.
    let events = store.recent_channel_events("acct-tg", 10).unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);
    assert_eq!(
        store.channel_cursor("acct-tg", "inbound").unwrap(),
        Some("500".to_string())
    );
}

/// The committed cursor is confirmed back to Telegram as `offset=cursor+1`.
#[tokio::test]
async fn telegram_committed_cursor_confirms_the_update_on_the_next_poll() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
        (200, r#"{"ok":true,"result":[]}"#.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("first poll");
    poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW + 1)
        .await
        .expect("second poll");
    let _get_me = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let _first = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let second = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();
    let second = String::from_utf8_lossy(&second);
    assert!(second.contains("offset=501"), "{second}");
}

/// The reply queued for a Telegram turn anchors to the chat-scoped message_id,
/// not the poll-stream update_id — Telegram cannot find the latter.
#[tokio::test]
async fn telegram_replies_anchor_to_the_message_id_not_the_update_id() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");

    let job_id = queue.submitted.lock().unwrap()[0].clone();
    let origin = store
        .channel_origin_for_job(&job_id)
        .expect("origin lookup")
        .expect("origin");
    assert_eq!(origin.provider_event_id, "71", "must be the message_id");
    assert_eq!(origin.conversation_id, "555");
}

/// A group message addressed to the bot is recognized as a mention.
///
/// The bot's own `@username` is the only thing that can decide this, and it
/// comes from `getMe` — which nothing but the adapter itself calls in the
/// daemon. Without it a group set to answer on mention would never answer.
#[tokio::test]
async fn telegram_resolves_its_own_identity_before_deciding_a_mention() {
    const MENTION_UPDATE: &str = r#"{
        "ok": true,
        "result": [{
            "update_id": 600,
            "message": {
                "message_id": 9,
                "message_thread_id": 42,
                "date": 1700000000,
                "chat": {"id": -999, "type": "supergroup", "title": "Ops"},
                "from": {"id": 777, "is_bot": false, "first_name": "Bo"},
                "text": "/deploy@little_monkey_bot now",
                "entities": [{"type": "bot_command", "offset": 0, "length": 25}]
            }
        }]
    }"#;
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, MENTION_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let batch = adapter.poll(None).await.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    assert!(
        batch.envelopes[0].mentions_self,
        "the bot did not recognize a command addressed to it"
    );
    assert_eq!(
        batch.envelopes[0].conversation.thread_id.as_deref(),
        Some("42")
    );
}

// ---------------------------------------------------------------------------
// Discord: gateway resume survives restart
// ---------------------------------------------------------------------------

fn discord_hello() -> String {
    serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 45_000 } }).to_string()
}

fn discord_adapter(api_base: &str) -> DiscordAdapter {
    let account = account_record("acct-dc", ChannelKind::Discord);
    DiscordAdapter::new(&AdapterConfig {
        account: &account,
        secret: "bot-token".into(),
    })
    .expect("adapter")
    .with_base_url(api_base)
}

/// receive event with seq N → persist state → restart → RESUME with the
/// stored session and sequence → continue from N.
#[tokio::test(flavor = "multi_thread")]
async fn discord_resume_state_survives_a_daemon_restart() {
    let mut store = seeded_store("acct-dc", ChannelKind::Discord);
    let queue = FakeQueue::default();

    // Both of the daemon's lives share one gateway fixture: connection 0 is
    // the fresh IDENTIFY session, connection 1 must be the RESUME.
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) =
        super::channel_adapter::test_http::serve(vec![(200, format!(r#"{{"url":"{}"}}"#, ws.url))]);

    // First life: IDENTIFY, READY (naming the resume URL), one message at seq 5.
    let adapter = discord_adapter(&api_base);
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    wait_for_frame(&ws.received, 20, "IDENTIFY", |connection, frame| {
        connection == 0 && frame_op(frame) == 2
    })
    .await;
    ws.inject
        .send(
            serde_json::json!({
                "op": 0, "t": "READY", "s": 1,
                "d": {
                    "session_id": "sess-77",
                    "resume_gateway_url": ws.url,
                    "user": { "id": "bot-1" },
                }
            })
            .to_string(),
        )
        .unwrap();
    ws.inject
        .send(
            serde_json::json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 5,
                "d": {
                    "id": "msg-1", "channel_id": "chan-1", "guild_id": "guild-1",
                    "content": "first life",
                    "author": { "id": "user-1", "username": "ada", "bot": false },
                }
            })
            .to_string(),
        )
        .unwrap();
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("first poll");
    assert_eq!(batch.envelopes.len(), 1);
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-dc".to_string();
    }
    // The production path: ingest, then persist the cursor (the resume state).
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);
    let cursor = batch.cursor.expect("resume state snapshot");
    store
        .set_channel_cursor("acct-dc", "inbound", &cursor, NOW)
        .expect("persist cursor");
    let persisted: Value = serde_json::from_str(&cursor).expect("cursor is JSON");
    assert_eq!(persisted["session_id"], "sess-77");
    // The snapshot was taken before the drain, so the persisted sequence may
    // lag the message's own seq — it must never lead it.
    assert!(persisted["seq"].as_u64().unwrap_or(0) <= 5);

    // RESTART: the adapter is dropped with its in-memory state.
    drop(adapter);

    // Second life: seeded from the persisted cursor, the adapter must RESUME.
    let stored = store
        .channel_cursor("acct-dc", "inbound")
        .unwrap()
        .expect("persisted resume state");
    let adapter = discord_adapter(&api_base);
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(Some(&stored)).await;
        (adapter, batch)
    });
    let resume = wait_for_frame(&ws.received, 20, "RESUME", |connection, frame| {
        connection == 1 && frame_op(frame) == 6
    })
    .await;
    assert_eq!(resume["d"]["session_id"], "sess-77");
    assert_eq!(resume["d"]["seq"], persisted["seq"]);
    // The gateway accepts and replays what the stored sequence had not seen.
    ws.inject
        .send(serde_json::json!({ "op": 0, "t": "RESUMED", "s": 5 }).to_string())
        .unwrap();
    ws.inject
        .send(
            serde_json::json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 6,
                "d": {
                    "id": "msg-2", "channel_id": "chan-1", "guild_id": "guild-1",
                    "content": "second life",
                    "author": { "id": "user-1", "username": "ada", "bot": false },
                }
            })
            .to_string(),
        )
        .unwrap();
    let (_adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("second poll");
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.envelopes[0].provider_event_id, "msg-2");
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-dc".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW + 1);
    assert_eq!(report.accepted, 1);
    assert_eq!(queue.submitted.lock().unwrap().len(), 2);
}

/// An INVALID_SESSION (resumable=false) answer to the RESUME falls back to a
/// fresh IDENTIFY instead of looping on the dead session.
#[tokio::test(flavor = "multi_thread")]
async fn discord_invalid_session_falls_back_to_identify() {
    // Connection 0 receives the RESUME and invalidates it; connection 1 must
    // then be a fresh IDENTIFY.
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) =
        super::channel_adapter::test_http::serve(vec![(200, format!(r#"{{"url":"{}"}}"#, ws.url))]);
    let stored = serde_json::json!({
        "session_id": "sess-dead",
        "resume_gateway_url": ws.url,
        "seq": 41,
    })
    .to_string();

    let adapter = discord_adapter(&api_base);
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(Some(&stored)).await;
        (adapter, batch)
    });
    let resume = wait_for_frame(&ws.received, 20, "RESUME", |connection, frame| {
        connection == 0 && frame_op(frame) == 6
    })
    .await;
    assert_eq!(resume["d"]["session_id"], "sess-dead");
    // The session is dead; Discord asks for a fresh start.
    ws.inject
        .send(serde_json::json!({ "op": 9, "d": false }).to_string())
        .unwrap();
    // The fallback IDENTIFY arrives on the next connection, after the
    // randomized invalid-session wait (1–5s).
    let identify = wait_for_frame(
        &ws.received,
        30,
        "fallback IDENTIFY",
        |connection, frame| connection == 1 && frame_op(frame) == 2,
    )
    .await;
    assert_eq!(identify["d"]["token"], "bot-token");
    let (_adapter, batch) = poll.await.expect("poll task");
    // No message was dispatched; the poll returns empty (or timed out early).
    assert!(batch.expect("poll").envelopes.is_empty());
}

// ---------------------------------------------------------------------------
// Slack: the ACK waits for durable receipt
// ---------------------------------------------------------------------------

fn slack_event_envelope(envelope_id: &str, ts: &str, text: &str) -> String {
    serde_json::json!({
        "envelope_id": envelope_id,
        "type": "events_api",
        "payload": {
            "event": {
                "type": "message",
                "channel": "C1",
                "channel_type": "channel",
                "user": "U1",
                "text": text,
                "ts": ts,
            }
        }
    })
    .to_string()
}

fn slack_adapter(api_base: &str) -> SlackAdapter {
    let account = account_record("acct-sl", ChannelKind::Slack);
    SlackAdapter::new(&AdapterConfig {
        account: &account,
        secret: r#"{"bot_token":"xoxb-test","app_token":"xapp-test"}"#.into(),
    })
    .expect("adapter")
    .with_base_url(api_base)
}

/// receive envelope → prove no ACK yet → durable insert → ACK goes out.
/// A duplicate delivery of the same event is deduplicated and still ACKed.
#[tokio::test(flavor = "multi_thread")]
async fn slack_acks_only_after_durable_receipt_and_dedupes_redelivery() {
    let mut store = seeded_store("acct-sl", ChannelKind::Slack);
    let queue = FakeQueue::default();

    let ws = spawn_ws_fixture(vec![vec![
        serde_json::json!({ "type": "hello", "num_connections": 1 }).to_string(),
    ]]);
    // auth.test for the socket task, then apps.connections.open.
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#.to_string(),
        ),
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
    ]);
    let adapter = slack_adapter(&api_base);

    // The envelope arrives while the adapter is polling.
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    // Give the socket a moment to come up, then deliver.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ws.inject
        .send(slack_event_envelope("env-1", "1000.001", "ship it"))
        .unwrap();
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);

    // The message is in hand but NOT yet durable: no ACK may exist.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        ws.received
            .lock()
            .unwrap()
            .iter()
            .all(|(_, frame)| frame.get("envelope_id").is_none()),
        "an ACK was sent before durable receipt: {:?}",
        ws.received.lock().unwrap()
    );

    // Durable insert, then the commit releases the ACK.
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-sl".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);
    adapter.commit_batch(&batch.envelopes).await;
    wait_for_frame(&ws.received, 10, "ACK for env-1", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-1")
    })
    .await;

    // Slack redelivers the same event under a fresh envelope id (as it does
    // when an ACK is lost). The event log collapses it; the ACK still goes
    // out, which is what stops the redelivery loop.
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    ws.inject
        .send(slack_event_envelope("env-2", "1000.001", "ship it"))
        .unwrap();
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-sl".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW + 1);
    assert_eq!(report.duplicates, 1);
    assert_eq!(report.accepted, 0);
    adapter.commit_batch(&batch.envelopes).await;
    wait_for_frame(
        &ws.received,
        10,
        "ACK for the duplicate env-2",
        |_, frame| frame.get("envelope_id").and_then(Value::as_str) == Some("env-2"),
    )
    .await;

    // One durable event, one run, despite two deliveries.
    assert_eq!(store.recent_channel_events("acct-sl", 10).unwrap().len(), 1);
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);
}

/// The full production path — `poll_account_once` — performs the same
/// handshake end to end: ingest, cursor, then commit.
#[tokio::test(flavor = "multi_thread")]
async fn slack_poll_account_once_releases_the_ack_after_ingest() {
    let mut store = seeded_store("acct-sl", ChannelKind::Slack);
    let queue = FakeQueue::default();
    let ws = spawn_ws_fixture(vec![vec![
        serde_json::json!({ "type": "hello", "num_connections": 1 }).to_string(),
    ]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#.to_string(),
        ),
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
    ]);
    let adapter = slack_adapter(&api_base);

    let inject = ws.inject.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = inject.send(slack_event_envelope("env-9", "2000.001", "end to end"));
    });
    let report = poll_account_once(&mut store, &queue, "acct-sl", &adapter, NOW)
        .await
        .expect("poll_account_once");
    assert_eq!(report.accepted, 1);
    wait_for_frame(&ws.received, 10, "ACK for env-9", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-9")
    })
    .await;
    assert_eq!(store.recent_channel_events("acct-sl", 10).unwrap().len(), 1);
}

// ---------------------------------------------------------------------------
// The worker stamps ownership
// ---------------------------------------------------------------------------

/// Whatever an adapter writes in `account_id`, the worker overrides it with
/// the account it actually polled — Telegram's normalizer leaves the field
/// blank, and a malfunctioning adapter must not write into another account's
/// event log.
#[tokio::test]
async fn the_worker_stamps_the_polled_account_onto_every_envelope() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    // Telegram's own normalizer leaves account_id empty; poll_account_once
    // must still land the event under the polled account.
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");
    assert_eq!(report.accepted, 1);
    let events = store.recent_channel_events("acct-tg", 10).unwrap();
    assert_eq!(events.len(), 1);
}

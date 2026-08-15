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

use std::collections::BTreeMap;

use super::adapters::discord::DiscordAdapter;
use super::adapters::slack::SlackAdapter;
use super::adapters::telegram::TelegramAdapter;
use super::channel_adapter::{AdapterConfig, ChannelAdapter};
use super::channel_store::{ChannelAccountRecord, EventDirection};
use super::channel_worker::{drain_outbox_once, ingest_batch, poll_account_once, RunQueue};
use super::store::DaemonStore;
use little_monkey_lib::channels::ingress::{ConversationIngress, FrozenExecutionContext};
use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind};

pub(crate) const NOW: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

#[derive(Default)]
pub(crate) struct FakeQueue {
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
/// `pub(crate)` because the opt-in live tests build the same minimal world
/// around a real provider account.
pub(crate) fn seeded_store(account_id: &str, kind: ChannelKind) -> DaemonStore {
    let mut store = DaemonStore::open_in_memory().expect("open in-memory store");
    seed_account_and_route(&mut store, account_id, kind);
    store
}

/// The same minimal world in a store the caller already owns — a file-backed
/// one, for the tests that close it and open it again.
pub(crate) fn seed_account_and_route(store: &mut DaemonStore, account_id: &str, kind: ChannelKind) {
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
pub(crate) struct WsFixture {
    pub(crate) url: String,
    pub(crate) received: Arc<StdMutex<Vec<(usize, Value)>>>,
    pub(crate) inject: mpsc::UnboundedSender<String>,
    /// Connections opened so far, so a test can prove the client did NOT come
    /// back after a close it should have treated as final.
    pub(crate) connections: Arc<std::sync::atomic::AtomicUsize>,
}

/// A close code injected as a real WebSocket close frame. `inject` carries
/// text frames; a close is a different frame type, so it needs its own door.
const CLOSE_PREFIX: &str = "__close__:";

impl WsFixture {
    pub(crate) fn close_with(&self, code: u16) {
        self.inject
            .send(format!("{CLOSE_PREFIX}{code}"))
            .expect("inject close");
    }

    pub(crate) fn connections(&self) -> usize {
        self.connections.load(std::sync::atomic::Ordering::SeqCst)
    }
}

pub(crate) fn spawn_ws_fixture(greetings: Vec<Vec<String>>) -> WsFixture {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ws fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let received: Arc<StdMutex<Vec<(usize, Value)>>> = Arc::default();
    let (inject_tx, inject_rx) = mpsc::unbounded_channel::<String>();
    let inject_rx = Arc::new(tokio::sync::Mutex::new(inject_rx));
    let log = received.clone();
    let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let opened = connections.clone();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        for (index, greeting) in greetings.into_iter().enumerate() {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                return;
            };
            opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                        Some(text) => match text.strip_prefix(CLOSE_PREFIX) {
                            Some(code) => {
                                let code = code.parse::<u16>().expect("close code");
                                let _ = ws
                                    .send(Message::Close(Some(
                                        tokio_tungstenite::tungstenite::protocol::CloseFrame {
                                            code: code.into(),
                                            reason: Default::default(),
                                        },
                                    )))
                                    .await;
                                break;
                            }
                            None => {
                                let _ = ws.send(Message::Text(text.into())).await;
                            }
                        },
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
        connections,
    }
}

/// Wait until a logged frame satisfies `predicate`, or panic after `seconds`.
pub(crate) async fn wait_for_frame(
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

pub(crate) fn frame_op(frame: &Value) -> i64 {
    frame.get("op").and_then(Value::as_i64).unwrap_or(-1)
}

/// The runs a queue was actually asked to create, counted by identity rather
/// than by call: a recovery pass that races the original submission calls the
/// queue twice with the same deterministic job id, and that is one run.
pub(crate) fn distinct_runs(queue: &FakeQueue) -> usize {
    let submitted = queue.submitted.lock().unwrap();
    let unique: std::collections::BTreeSet<&String> = submitted.iter().collect();
    unique.len()
}

/// The invariant every crash test ends on: one provider event, one durable
/// turn that the event points at, and one run.
/// The durable row a crash *before* the queue took the turn leaves behind.
///
/// Asserted column by column rather than trusted, because the injected failure
/// reaches its caller as an `Err` and a caller's error handling writes — the
/// event's `ignore_reason`, the account's health. None of that is state a real
/// crash produces. What makes the recovery claim hold anyway is that recovery
/// reads only `ingress_turns`, so this pins the row to exactly what a bare
/// commit leaves: never submitted, never charged an attempt, no error recorded.
fn assert_turn_awaits_its_first_submission(store: &DaemonStore) {
    let pending = store.pending_ingress_turns(10).unwrap();
    assert_eq!(pending.len(), 1, "expected one turn awaiting submission");
    assert_eq!(
        pending[0].attempts, 0,
        "a crash spent a submission attempt the process never made"
    );
    let turns = store.recent_ingress_turns(10).unwrap();
    assert_eq!(turns[0].state, super::ingress_store::IngressState::Accepted);
    assert_eq!(turns[0].job_id, None, "no run was submitted");
    assert_eq!(
        turns[0].last_error, None,
        "the injected failure must not be recorded as the turn's own"
    );
}

pub(crate) fn assert_one_of_everything(store: &DaemonStore, queue: &FakeQueue, account_id: &str) {
    let events = store.recent_channel_events(account_id, 10).unwrap();
    assert_eq!(events.len(), 1, "expected one durable event: {events:?}");
    let turns = store.recent_ingress_turns(10).unwrap();
    assert_eq!(turns.len(), 1, "expected one durable turn");
    assert_eq!(
        events[0].ingress_id.as_deref(),
        Some(turns[0].ingress_id.as_str()),
        "the event must name the turn it became"
    );
    assert_eq!(distinct_runs(queue), 1, "expected exactly one run");
    assert!(
        store
            .accepted_events_awaiting_processing(10)
            .unwrap()
            .is_empty(),
        "an accepted event with no turn behind it"
    );
}

/// Arm one durable boundary for the next time this thread reaches it.
fn arm(point: super::fail_points::FailPoint) {
    super::fail_points::arm(point);
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

/// Crash A: the update is in hand and the process dies before anything is
/// written. Telegram still owns it, because nothing advanced the offset.
#[tokio::test]
async fn telegram_crash_before_any_durable_write_loses_nothing() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let batch = adapter.poll(None).await.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    // CRASH: the batch is in memory only.
    drop(adapter);
    assert!(store
        .recent_channel_events("acct-tg", 10)
        .unwrap()
        .is_empty());
    assert!(store
        .channel_cursor("acct-tg", "inbound")
        .unwrap()
        .is_none());

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll after restart");

    assert_eq!(report.accepted, 1);
    assert_one_of_everything(&store, &queue, "acct-tg");
    assert_eq!(
        store.channel_cursor("acct-tg", "inbound").unwrap(),
        Some("500".to_string())
    );
}

/// Crash B, the case the old design could not survive: the provider event is
/// written and the process dies before the turn is. The whole acceptance rolls
/// back, so the offset holds and the redelivery is a first delivery.
#[tokio::test]
async fn telegram_crash_between_the_event_and_the_turn_keeps_the_message() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    arm(super::fail_points::FailPoint::AfterEventInsert);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");

    assert!(
        super::fail_points::fired(),
        "the boundary was never reached"
    );
    assert_eq!(report.unrecorded, 1);
    assert_eq!(report.accepted, 0);
    // Nothing at all: no event to suppress the redelivery, no turn, and — the
    // part that makes the message recoverable — no offset.
    assert!(store
        .recent_channel_events("acct-tg", 10)
        .unwrap()
        .is_empty());
    assert!(store.recent_ingress_turns(10).unwrap().is_empty());
    assert!(store
        .channel_cursor("acct-tg", "inbound")
        .unwrap()
        .is_none());
    drop(adapter);

    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW + 1)
        .await
        .expect("poll after restart");

    assert_eq!(report.accepted, 1);
    assert_one_of_everything(&store, &queue, "acct-tg");
    let _get_me = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("getMe request");
    let request = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("getUpdates request");
    assert!(
        !String::from_utf8_lossy(&request).contains("offset="),
        "the second life must not confirm an update it never accepted"
    );
}

/// Crash C: the acceptance is committed and the process dies before the run
/// reaches the queue. Recovery submits it, exactly once.
#[tokio::test]
async fn telegram_crash_before_the_queue_submit_recovers_exactly_one_run() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    arm(super::fail_points::FailPoint::BeforeQueueSubmit);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");

    assert!(super::fail_points::fired());
    assert_eq!(report.deferred, 1);
    assert!(queue.submitted.lock().unwrap().is_empty());
    // Durably accepted, so the offset may advance: the message is this
    // installation's problem now, not Telegram's.
    let events = store.recent_channel_events("acct-tg", 10).unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].ingress_id.is_some());
    assert_turn_awaits_its_first_submission(&store);
    assert_eq!(
        store.channel_cursor("acct-tg", "inbound").unwrap(),
        Some("500".to_string())
    );

    // RESTART: the startup sweep finishes what the crash interrupted.
    let recovery = super::channel_ingress::recover_pending_ingress(&mut store, &queue, NOW + 1)
        .expect("recover");
    assert_eq!(recovery.resubmitted, 1);
    assert_one_of_everything(&store, &queue, "acct-tg");
    assert_eq!(
        store.recent_ingress_turns(10).unwrap()[0].state,
        super::ingress_store::IngressState::Queued
    );
}

/// Crash D: the queue took the run and the process dies before the turn is
/// marked queued. The recovery pass resubmits under the same deterministic job
/// id, and the queue collapses it onto the run it already has.
#[tokio::test]
async fn telegram_crash_before_the_queued_state_does_not_duplicate_the_run() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    arm(super::fail_points::FailPoint::BeforeQueuedState);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");

    assert!(super::fail_points::fired());
    assert_eq!(report.deferred, 1);
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);
    // The run exists and the row does not know it, which is the only state
    // recovery may not be able to tell from "never submitted" — and it must
    // not be able to tell, because the two have to be finished the same way.
    assert_turn_awaits_its_first_submission(&store);

    let recovery = super::channel_ingress::recover_pending_ingress(&mut store, &queue, NOW + 1)
        .expect("recover");
    assert_eq!(recovery.resubmitted, 1);
    assert_eq!(
        queue.submitted.lock().unwrap().len(),
        2,
        "the queue was asked twice"
    );
    assert_one_of_everything(&store, &queue, "acct-tg");
}

/// Crash E: every local write is durable and the process dies before the
/// offset is confirmed. Telegram redelivers, the event log collapses it, and
/// the offset advances on the next pass.
#[tokio::test]
async fn telegram_crash_before_the_cursor_commit_dedupes_and_then_advances() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    arm(super::fail_points::FailPoint::BeforeCursorCommit);
    let interrupted = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW).await;

    assert!(interrupted.is_err(), "{interrupted:?}");
    assert!(super::fail_points::fired());
    assert_one_of_everything(&store, &queue, "acct-tg");
    // The mirror of crash C and D, and the reason this boundary is last: the
    // turn is *completely* durable — queued, with the run it owns named on the
    // row — and the only thing the crash cost is Telegram's offset. That is the
    // cursor invariant from the durable side: an offset may only ever advance
    // over messages already in this state, so losing it costs a redelivery and
    // nothing else.
    let turns = store.recent_ingress_turns(10).unwrap();
    assert_eq!(turns[0].state, super::ingress_store::IngressState::Queued);
    assert_eq!(
        turns[0].job_id.as_deref(),
        Some(queue.submitted.lock().unwrap()[0].as_str()),
        "the queued row must name the run the queue took"
    );
    assert_eq!(turns[0].attempts, 1, "exactly one submission was made");
    assert!(store
        .channel_cursor("acct-tg", "inbound")
        .unwrap()
        .is_none());
    drop(adapter);

    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW + 1)
        .await
        .expect("poll after restart");

    assert_eq!(report.duplicates, 1);
    assert_eq!(report.accepted, 0);
    assert_one_of_everything(&store, &queue, "acct-tg");
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
    // the fresh IDENTIFY session, connection 1 must be the RESUME. The two
    // extra HTTP responses answer each life's one REST lookup for the
    // never-before-seen guild channel (it is not a thread).
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, r#"{"id":"chan-1","type":0}"#.to_string()),
        (200, r#"{"id":"chan-1","type":0}"#.to_string()),
    ]);

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

/// The persisted gateway sequence may never lead a message that did not cross
/// the durable acceptance boundary. A RESUME from the sequence that *was*
/// persisted replays it, and it runs exactly once.
#[tokio::test(flavor = "multi_thread")]
async fn discord_sequence_never_outruns_the_durable_acceptance() {
    let mut store = seeded_store("acct-dc", ChannelKind::Discord);
    let queue = FakeQueue::default();

    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, r#"{"id":"chan-1","type":0}"#.to_string()),
        (200, r#"{"id":"chan-1","type":0}"#.to_string()),
    ]);
    // An earlier life of the daemon got as far as sequence 4.
    let stored = serde_json::json!({
        "session_id": "sess-77",
        "resume_gateway_url": ws.url,
        "seq": 4,
    })
    .to_string();
    store
        .set_channel_cursor("acct-dc", "inbound", &stored, NOW)
        .expect("seed the resume state");

    // This life RESUMEs, is dispatched the message at sequence 5, and its
    // acceptance is interrupted between the event and the turn.
    let message = serde_json::json!({
        "op": 0, "t": "MESSAGE_CREATE", "s": 5,
        "d": {
            "id": "msg-1", "channel_id": "chan-1", "guild_id": "guild-1",
            "content": "did this survive?",
            "author": { "id": "user-1", "username": "ada", "bot": false },
        }
    })
    .to_string();
    let received = ws.received.clone();
    let inject = ws.inject.clone();
    let replay = message.clone();
    tokio::spawn(async move {
        wait_for_frame(&received, 20, "RESUME", |connection, frame| {
            connection == 0 && frame_op(frame) == 6
        })
        .await;
        let _ = inject.send(serde_json::json!({ "op": 0, "t": "RESUMED", "s": 4 }).to_string());
        let _ = inject.send(replay);
    });
    let adapter = discord_adapter(&api_base);
    arm(super::fail_points::FailPoint::AfterEventInsert);
    let report = poll_account_once(&mut store, &queue, "acct-dc", &adapter, NOW)
        .await
        .expect("poll");

    assert!(super::fail_points::fired());
    assert_eq!(report.unrecorded, 1);
    assert!(store
        .recent_channel_events("acct-dc", 10)
        .unwrap()
        .is_empty());
    let held: Value =
        serde_json::from_str(&store.channel_cursor("acct-dc", "inbound").unwrap().unwrap())
            .expect("resume state");
    assert_eq!(
        held["seq"].as_u64(),
        Some(4),
        "the sequence advanced past a message that was never accepted"
    );
    drop(adapter);

    // RESTART: the RESUME asks from 4, so Discord replays sequence 5.
    let received = ws.received.clone();
    let inject = ws.inject.clone();
    tokio::spawn(async move {
        wait_for_frame(&received, 20, "second RESUME", |connection, frame| {
            connection == 1 && frame_op(frame) == 6
        })
        .await;
        let _ = inject.send(serde_json::json!({ "op": 0, "t": "RESUMED", "s": 4 }).to_string());
        let _ = inject.send(message);
    });
    let adapter = discord_adapter(&api_base);
    let report = poll_account_once(&mut store, &queue, "acct-dc", &adapter, NOW + 1)
        .await
        .expect("poll after restart");

    assert_eq!(report.accepted, 1);
    assert_one_of_everything(&store, &queue, "acct-dc");
    let advanced: Value =
        serde_json::from_str(&store.channel_cursor("acct-dc", "inbound").unwrap().unwrap())
            .expect("resume state");
    assert_eq!(advanced["session_id"], "sess-77");
    assert!(advanced["seq"].as_u64().unwrap_or(0) >= 4);
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

/// A close code that discards the session must not be answered with a RESUME
/// for it. Driven through the real socket so what is asserted is the frame
/// that actually went out, not a flag.
async fn discord_close_starts_a_fresh_session(code: u16) {
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
    ]);
    let stored = serde_json::json!({
        "session_id": "sess-stale",
        "resume_gateway_url": ws.url,
        "seq": 41,
    })
    .to_string();

    let adapter = discord_adapter(&api_base);
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(Some(&stored)).await;
        (adapter, batch)
    });
    // First connection resumes the stored session; the provider refuses it.
    wait_for_frame(&ws.received, 20, "RESUME", |connection, frame| {
        connection == 0 && frame_op(frame) == 6
    })
    .await;
    ws.close_with(code);

    let identify = wait_for_frame(&ws.received, 30, "fresh IDENTIFY", |connection, frame| {
        connection == 1 && frame_op(frame) == 2
    })
    .await;
    assert_eq!(identify["d"]["token"], "bot-token");
    // The negative half: the second connection must not have asked to resume
    // the session the provider just discarded.
    let frames = ws.received.lock().unwrap().clone();
    assert!(
        !frames
            .iter()
            .any(|(connection, frame)| *connection == 1 && frame_op(frame) == 6),
        "close {code} resumed a session the provider discarded: {frames:?}"
    );
    let (_adapter, batch) = poll.await.expect("poll task");
    assert!(batch.expect("poll").envelopes.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_invalid_sequence_close_starts_a_fresh_session() {
    discord_close_starts_a_fresh_session(4007).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_timed_out_session_close_starts_a_fresh_session() {
    discord_close_starts_a_fresh_session(4009).await;
}

/// A close code naming an account/configuration problem must stop the gateway
/// for good: no second connection, and health that names the cause.
async fn discord_close_stops_the_gateway(code: u16, expected: &str) {
    // Two greetings are offered on purpose — the fixture will accept a second
    // connection if the adapter makes one, which is exactly what must not
    // happen.
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, r#"{"id":"bot-1","username":"monkey"}"#.to_string()),
    ]);

    let adapter = Arc::new(discord_adapter(&api_base));
    let polling = adapter.clone();
    let poll = tokio::spawn(async move { polling.poll(None).await });
    wait_for_frame(&ws.received, 20, "IDENTIFY", |connection, frame| {
        connection == 0 && frame_op(frame) == 2
    })
    .await;
    ws.close_with(code);
    assert!(poll.await.expect("poll task").is_ok());

    // Health is an actionable error, not "reconnecting".
    let health = adapter.probe().await;
    assert_eq!(
        health.state,
        little_monkey_lib::channels::types::HealthState::Error,
        "close {code} left health at {:?}",
        health.state
    );
    let reported = format!(
        "{} {}",
        health.detail.unwrap_or_default(),
        health.last_error.unwrap_or_default()
    );
    assert!(
        reported.contains(expected),
        "close {code} health does not say what to do: {reported}"
    );
    // Long enough for the shortest backoff plus jitter to have fired.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    assert_eq!(
        ws.connections(),
        1,
        "close {code} reconnected after a failure backoff cannot fix"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_rejected_token_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4004, "bot token").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_invalid_shard_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4010, "shard").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_sharding_required_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4011, "shard").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_invalid_api_version_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4012, "API version").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_invalid_intents_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4013, "intents").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn discord_disallowed_intents_close_stops_the_gateway() {
    discord_close_stops_the_gateway(4014, "Message Content").await;
}

/// The cleared session must not come back after a restart: what the adapter
/// hands the durable cursor after a session-discarding close is a state that
/// identifies fresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_discarded_discord_session_is_not_resurrected_by_the_cursor() {
    let mut store = seeded_store("acct-dc", ChannelKind::Discord);
    let ws = spawn_ws_fixture(vec![vec![discord_hello()], vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
    ]);
    let stored = serde_json::json!({
        "session_id": "sess-stale",
        "resume_gateway_url": ws.url,
        "seq": 41,
    })
    .to_string();

    let adapter = discord_adapter(&api_base);
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(Some(&stored)).await;
        (adapter, batch)
    });
    wait_for_frame(&ws.received, 20, "RESUME", |connection, frame| {
        connection == 0 && frame_op(frame) == 6
    })
    .await;
    ws.close_with(4009);
    wait_for_frame(&ws.received, 30, "fresh IDENTIFY", |connection, frame| {
        connection == 1 && frame_op(frame) == 2
    })
    .await;
    let (_adapter, batch) = poll.await.expect("poll task");
    let cursor = batch.expect("poll").cursor.expect("a resume snapshot");
    store
        .set_channel_cursor("acct-dc", "inbound", &cursor, NOW)
        .expect("persist cursor");

    let persisted: Value = serde_json::from_str(
        &store
            .channel_cursor("acct-dc", "inbound")
            .unwrap()
            .expect("cursor"),
    )
    .expect("cursor is JSON");
    assert!(
        persisted.get("session_id").is_none(),
        "the discarded session survived into the durable cursor: {persisted}"
    );
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

/// The ACK means durable acceptance, not "the insert started". An acceptance
/// that rolls back leaves the envelope unacknowledged, and Slack's redelivery
/// is what makes the message run.
#[tokio::test(flavor = "multi_thread")]
async fn slack_withholds_the_ack_when_the_acceptance_rolls_back() {
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
        let _ = inject.send(slack_event_envelope("env-1", "3000.001", "ship it"));
    });
    arm(super::fail_points::FailPoint::AfterEventInsert);
    let report = poll_account_once(&mut store, &queue, "acct-sl", &adapter, NOW)
        .await
        .expect("poll");

    assert!(super::fail_points::fired());
    assert_eq!(report.unrecorded, 1);
    assert!(report.ack_safe.is_empty());
    assert!(store
        .recent_channel_events("acct-sl", 10)
        .unwrap()
        .is_empty());
    // Give a wrong ACK time to appear before ruling it out.
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        ws.received
            .lock()
            .unwrap()
            .iter()
            .all(|(_, frame)| frame.get("envelope_id").is_none()),
        "an unaccepted envelope was acknowledged: {:?}",
        ws.received.lock().unwrap()
    );

    // Slack redelivers it — under a fresh envelope id, as it does when an ACK
    // never came — and this time the acceptance commits.
    let inject = ws.inject.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let _ = inject.send(slack_event_envelope("env-2", "3000.001", "ship it"));
    });
    let report = poll_account_once(&mut store, &queue, "acct-sl", &adapter, NOW + 1)
        .await
        .expect("poll after redelivery");

    assert_eq!(report.accepted, 1);
    wait_for_frame(&ws.received, 10, "ACK for env-2", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-2")
    })
    .await;
    // Both deliveries of the one message are acknowledged now, and only now:
    // the ACK is parked per provider message, so accepting it releases every
    // delivery of it — which is what stops Slack from redelivering forever.
    wait_for_frame(&ws.received, 10, "ACK for env-1", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-1")
    })
    .await;
    assert_one_of_everything(&store, &queue, "acct-sl");
}

// ---------------------------------------------------------------------------
// Full wire-to-wire paths: provider fixture → durable event → run → reply →
// durable outbox → drain → provider fixture
// ---------------------------------------------------------------------------

/// Daemon paths rooted in a throwaway directory, for the parts of the send
/// seam that touch the filesystem (the content store holding outbound
/// artifacts). Nothing here reads the operator's real data directory.
pub(crate) fn temp_daemon_paths() -> super::store::DaemonPaths {
    let root = std::env::temp_dir()
        .join(format!("lm-restart-{}", uuid::Uuid::new_v4().simple()))
        .join("daemon");
    std::fs::create_dir_all(&root).expect("temp daemon root");
    super::store::DaemonPaths {
        config: root.join("config.json"),
        state_db: root.join("state.db"),
        ledger_db: root.join("ledger.db"),
        snapshots: root.join("snapshots"),
        logs: root.join("logs"),
        worktrees: root.join("worktrees"),
        lock: root.join("daemon.lock"),
        root,
    }
}

/// Send from the run `job_id` through the actual production tool seam: the
/// same authority derivation, the same [`plan_send`] admission and the same
/// [`queue_send`] durable write the `send_message` tool performs — against
/// the test-owned store. Only the model deciding to call the tool is this
/// test; nothing about the outbox row is reconstructed here.
pub(crate) fn send_via_tool_seam(
    store: &mut DaemonStore,
    paths: &super::store::DaemonPaths,
    job_id: &str,
    tool_call_id: &str,
    request: &super::channel_tool::ChannelSendRequest,
    policy: Option<&little_monkey_lib::run_protocol::ChannelSendPolicy>,
) -> Result<serde_json::Value, String> {
    let authority = super::channel_tool::send_authority_for_job(store, job_id, false, policy);
    let origin = store.channel_origin_for_job(job_id).expect("origin lookup");
    let plan = super::channel_tool::plan_send(request, &authority, origin.as_ref())?;
    super::channel_tool::queue_send(
        store,
        paths,
        request,
        &plan,
        origin.as_ref(),
        // The durable identity the agent loop supplies: the job and the
        // runtime's tool-call id, never anything derived from the message.
        &super::channel_tool::SendInvocation {
            job_id: Some(job_id.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
        },
        NOW,
    )
}

/// An origin reply through the tool seam, as every wire-to-wire test sends it.
pub(crate) fn queue_reply_for_job(store: &mut DaemonStore, job_id: &str, text: &str) {
    let paths = temp_daemon_paths();
    let request = super::channel_tool::ChannelSendRequest {
        text: text.to_string(),
        ..Default::default()
    };
    send_via_tool_seam(store, &paths, job_id, "call-reply-1", &request, None)
        .expect("the reply is queued");
}

pub(crate) fn adapters_map(
    account_id: &str,
    adapter: Arc<dyn ChannelAdapter>,
) -> BTreeMap<String, Arc<dyn ChannelAdapter>> {
    let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
    adapters.insert(account_id.to_string(), adapter);
    adapters
}

/// The one outbound event the drain recorded for `account_id`.
pub(crate) fn outbound_event_id(store: &mut DaemonStore, account_id: &str) -> String {
    store
        .recent_channel_events(account_id, 10)
        .expect("events")
        .into_iter()
        .find(|event| event.direction == EventDirection::Outbound)
        .expect("an outbound event")
        .provider_event_id
}

/// getUpdates → durable inbound event (with the photo's stored artifact) →
/// one run → reply row in the durable outbox → drain → sendMessage on the
/// wire with the right chat, forum topic and reply target → the provider's
/// message id stored on the outbound event.
#[tokio::test]
async fn telegram_full_path_from_wire_to_wire() {
    const THREADED_PHOTO_UPDATE: &str = r#"{
        "ok": true,
        "result": [{
            "update_id": 700,
            "message": {
                "message_id": 9,
                "message_thread_id": 42,
                "date": 1700000000,
                "chat": {"id": -999, "type": "supergroup", "title": "Ops"},
                "from": {"id": 777, "is_bot": false, "first_name": "Bo"},
                "text": "look at this",
                "photo": [{"file_id": "PHOTO1", "width": 100, "height": 100, "file_size": 7}]
            }
        }]
    }"#;
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, THREADED_PHOTO_UPDATE.to_string()),
        (
            200,
            r#"{"ok":true,"result":{"file_id":"PHOTO1","file_path":"photos/p.png"}}"#.to_string(),
        ),
        (200, "PNGDATA".to_string()),
        (
            200,
            r#"{"ok":true,"result":{"message_id":99,"chat":{"id":-999,"type":"supergroup"}}}"#
                .to_string(),
        ),
    ]);
    let adapter = telegram_adapter(&base);

    // Inbound: poll, hydrate the photo's real bytes into the (injected) blob
    // store, then ingest — the same steps poll_account_once runs, with the
    // daemon's on-disk store swapped for the fixture sink.
    let mut batch = adapter.poll(None).await.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-tg".to_string();
    }
    super::channel_adapter::hydrate_attachments(
        &adapter,
        &super::channel_adapter::test_http::FixtureBlobs(Vec::new()),
        Default::default(),
        &mut batch.envelopes,
    )
    .await;
    assert_eq!(
        batch.envelopes[0].attachments[0]
            .stored_artifact_id
            .as_deref(),
        Some("fixture-blob"),
        "the photo's bytes must be stored before the turn becomes durable"
    );
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);

    // The durable record the run reads carries the artifact reference.
    let events = store.recent_channel_events("acct-tg", 10).expect("events");
    assert!(
        events[0].envelope_json.contains("fixture-blob"),
        "durable event lost the artifact id: {}",
        events[0].envelope_json
    );

    // The run replies through the production reply seam.
    let job_id = queue.submitted.lock().unwrap()[0].clone();
    queue_reply_for_job(&mut store, &job_id, "on it");
    let drained = drain_outbox_once(&mut store, &adapters_map("acct-tg", Arc::new(adapter)), NOW)
        .await
        .expect("drain");
    assert_eq!(drained.sent, 1);

    // What actually went on the wire.
    for _ in 0..4 {
        requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("inbound-side request");
    }
    let reply = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the reply request");
    let reply = String::from_utf8_lossy(&reply);
    assert!(
        reply.starts_with("POST /botbot-token/sendMessage"),
        "{reply}"
    );
    assert!(reply.contains(r#""chat_id":"-999""#), "{reply}");
    assert!(reply.contains(r#""message_thread_id":42"#), "{reply}");
    // The current reply contract, and only it: a topic and a reply target
    // travel together in one request.
    assert!(
        reply.contains(r#""reply_parameters":{"message_id":9}"#),
        "{reply}"
    );
    assert!(
        !reply.contains("reply_to_message_id"),
        "the retired reply field is still on the wire: {reply}"
    );
    assert!(reply.contains("on it"), "{reply}");

    // The provider's message id is stored on the outbound event.
    assert_eq!(outbound_event_id(&mut store, "acct-tg"), "99");
}

/// Gateway MESSAGE_CREATE in a thread this process has never seen → REST
/// thread resolution → durable event → one run → durable reply → REST post
/// to the thread with a message_reference → provider id captured.
#[tokio::test(flavor = "multi_thread")]
async fn discord_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-dc", ChannelKind::Discord);
    let queue = FakeQueue::default();
    let ws = spawn_ws_fixture(vec![vec![discord_hello()]]);
    let (api_base, requests) = super::channel_adapter::test_http::serve(vec![
        (200, format!(r#"{{"url":"{}"}}"#, ws.url)),
        (
            200,
            r#"{"id":"thread-9","type":11,"parent_id":"chan-1"}"#.to_string(),
        ),
        (200, r#"{"id":"reply-1"}"#.to_string()),
    ]);
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
                "d": { "session_id": "sess-e2e", "resume_gateway_url": ws.url, "user": { "id": "bot-1" } }
            })
            .to_string(),
        )
        .unwrap();
    // The message arrives in a thread that existed before this process: no
    // THREAD_CREATE was ever dispatched, only the REST lookup can place it.
    ws.inject
        .send(
            serde_json::json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 5,
                "d": {
                    "id": "msg-1", "channel_id": "thread-9", "guild_id": "guild-1",
                    "content": "old thread, new process",
                    "author": { "id": "user-1", "username": "ada", "bot": false },
                }
            })
            .to_string(),
        )
        .unwrap();
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    assert_eq!(batch.envelopes[0].conversation.conversation_id, "chan-1");
    assert_eq!(
        batch.envelopes[0].conversation.thread_id.as_deref(),
        Some("thread-9")
    );
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-dc".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);

    let job_id = queue.submitted.lock().unwrap()[0].clone();
    queue_reply_for_job(&mut store, &job_id, "answered in the thread");
    let drained = drain_outbox_once(&mut store, &adapters_map("acct-dc", Arc::new(adapter)), NOW)
        .await
        .expect("drain");
    assert_eq!(drained.sent, 1);

    // Requests: gateway lookup, channel lookup, then the reply.
    for _ in 0..2 {
        requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("inbound-side request");
    }
    let reply = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the reply request");
    let reply = String::from_utf8_lossy(&reply);
    assert!(
        reply.starts_with("POST /channels/thread-9/messages"),
        "the reply must land in the thread, not the parent channel: {reply}"
    );
    assert!(reply.contains("message_reference"), "{reply}");
    assert!(reply.contains("msg-1"), "{reply}");
    assert!(reply.contains("answered in the thread"), "{reply}");

    assert_eq!(outbound_event_id(&mut store, "acct-dc"), "reply-1");
}

/// Socket Mode envelope (in a thread) → no early ACK → durable event → ACK →
/// one run → durable reply → chat.postMessage with the right thread_ts →
/// provider ts captured.
#[tokio::test(flavor = "multi_thread")]
async fn slack_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-sl", ChannelKind::Slack);
    let queue = FakeQueue::default();
    let ws = spawn_ws_fixture(vec![vec![
        serde_json::json!({ "type": "hello", "num_connections": 1 }).to_string(),
    ]]);
    let (api_base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#.to_string(),
        ),
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
        (200, r#"{"ok":true,"ts":"77.88"}"#.to_string()),
    ]);
    let adapter = slack_adapter(&api_base);

    let inject = ws.inject.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let _ = inject.send(
            serde_json::json!({
                "envelope_id": "env-e2e",
                "type": "events_api",
                "payload": { "event": {
                    "type": "message", "channel": "C1", "channel_type": "channel",
                    "user": "U1", "text": "threaded ask",
                    "ts": "3000.002", "thread_ts": "999.000",
                }}
            })
            .to_string(),
        );
    });
    // The full production inbound path: ingest, cursor, deferred ACK.
    let report = poll_account_once(&mut store, &queue, "acct-sl", &adapter, NOW)
        .await
        .expect("poll_account_once");
    assert_eq!(report.accepted, 1);
    wait_for_frame(&ws.received, 10, "ACK for env-e2e", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-e2e")
    })
    .await;

    let job_id = queue.submitted.lock().unwrap()[0].clone();
    queue_reply_for_job(&mut store, &job_id, "answered in the thread");
    let drained = drain_outbox_once(&mut store, &adapters_map("acct-sl", Arc::new(adapter)), NOW)
        .await
        .expect("drain");
    assert_eq!(drained.sent, 1);

    for _ in 0..2 {
        requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("inbound-side request");
    }
    let reply = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the reply request");
    let reply = String::from_utf8_lossy(&reply);
    assert!(reply.starts_with("POST /chat.postMessage"), "{reply}");
    assert!(reply.contains(r#""channel":"C1""#), "{reply}");
    assert!(reply.contains(r#""thread_ts":"999.000""#), "{reply}");
    assert!(reply.contains("answered in the thread"), "{reply}");

    assert_eq!(outbound_event_id(&mut store, "acct-sl"), "77.88");
}

// ---------------------------------------------------------------------------
// Provider echoes of our own messages never start a run
// ---------------------------------------------------------------------------

/// Telegram redelivers the bot's own outbound message through getUpdates.
/// The identity from getMe flags it; ingress records and drops it.
#[tokio::test]
async fn telegram_echo_of_our_own_message_never_starts_a_run() {
    const OWN_MESSAGE: &str = r#"{
        "ok": true,
        "result": [{
            "update_id": 800,
            "message": {
                "message_id": 12,
                "date": 1700000000,
                "chat": {"id": 555, "type": "private"},
                "from": {"id": 42, "is_bot": true, "first_name": "Monkey", "username": "little_monkey_bot"},
                "text": "our own reply, echoed back"
            }
        }]
    }"#;
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, OWN_MESSAGE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(&mut store, &queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");
    assert_eq!(report.ignored, 1);
    assert_eq!(report.accepted, 0);
    assert!(queue.submitted.lock().unwrap().is_empty(), "an echo ran");
}

/// A RESUMEd Discord session sees no second READY, so the bot's identity
/// comes only from the persisted cursor — and its own echoed message must
/// still be recognized and dropped without a run.
#[tokio::test(flavor = "multi_thread")]
async fn discord_echo_after_resume_never_starts_a_run() {
    let mut store = seeded_store("acct-dc", ChannelKind::Discord);
    let queue = FakeQueue::default();
    let ws = spawn_ws_fixture(vec![vec![discord_hello()]]);
    let stored = serde_json::json!({
        "session_id": "sess-echo",
        "resume_gateway_url": ws.url,
        "seq": 41,
        "bot_user_id": "bot-9",
    })
    .to_string();
    let adapter = discord_adapter("http://127.0.0.1:9");
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(Some(&stored)).await;
        (adapter, batch)
    });
    wait_for_frame(&ws.received, 20, "RESUME", |connection, frame| {
        connection == 0 && frame_op(frame) == 6
    })
    .await;
    ws.inject
        .send(serde_json::json!({ "op": 0, "t": "RESUMED", "s": 41 }).to_string())
        .unwrap();
    // A DM from ourselves: Discord dispatches a bot its own messages.
    ws.inject
        .send(
            serde_json::json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 42,
                "d": {
                    "id": "own-1", "channel_id": "dm-1",
                    "content": "our own reply",
                    "author": { "id": "bot-9", "username": "little-monkey", "bot": true },
                }
            })
            .to_string(),
        )
        .unwrap();
    let (_adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    assert!(batch.envelopes[0].sender.is_self);
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-dc".to_string();
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.ignored, 1);
    assert_eq!(report.accepted, 0);
    assert!(queue.submitted.lock().unwrap().is_empty(), "an echo ran");
}

/// Slack delivers the bot's own message as a bot_message with our bot_id.
/// It is recorded, dropped, and still ACKed so Slack stops redelivering it.
#[tokio::test(flavor = "multi_thread")]
async fn slack_echo_of_our_own_bot_message_never_starts_a_run() {
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
        let _ = inject.send(
            serde_json::json!({
                "envelope_id": "env-own",
                "type": "events_api",
                "payload": { "event": {
                    "type": "message", "subtype": "bot_message",
                    "channel": "C1", "channel_type": "channel",
                    "bot_id": "B1", "text": "our own reply", "ts": "4000.001",
                }}
            })
            .to_string(),
        );
    });
    let report = poll_account_once(&mut store, &queue, "acct-sl", &adapter, NOW)
        .await
        .expect("poll_account_once");
    assert_eq!(report.ignored, 1);
    assert_eq!(report.accepted, 0);
    assert!(queue.submitted.lock().unwrap().is_empty(), "an echo ran");
    // The echo is still acknowledged — Slack must stop redelivering it.
    wait_for_frame(&ws.received, 10, "ACK for env-own", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-own")
    })
    .await;
}

// ---------------------------------------------------------------------------
// Discord liveness and admission at the wire level
// ---------------------------------------------------------------------------

/// A connection whose heartbeats stop being acknowledged is a zombie: the
/// adapter must hang up and RESUME on a fresh socket.
#[tokio::test(flavor = "multi_thread")]
async fn discord_missed_heartbeat_ack_reconnects_and_resumes() {
    // A 150ms heartbeat interval, and a fixture that never answers op 1.
    let fast_hello =
        serde_json::json!({ "op": 10, "d": { "heartbeat_interval": 150 } }).to_string();
    let ws = spawn_ws_fixture(vec![vec![fast_hello], vec![discord_hello()]]);
    let (api_base, _requests) =
        super::channel_adapter::test_http::serve(vec![(200, format!(r#"{{"url":"{}"}}"#, ws.url))]);
    let adapter = discord_adapter(&api_base);
    let _poll = tokio::spawn(async move {
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
                "d": { "session_id": "sess-hb", "resume_gateway_url": ws.url, "user": { "id": "bot-1" } }
            })
            .to_string(),
        )
        .unwrap();
    // First heartbeat goes unanswered; the next tick closes the socket and
    // the session is resumed on connection 1.
    let resume = wait_for_frame(
        &ws.received,
        20,
        "RESUME after missed heartbeat ACK",
        |connection, frame| connection == 1 && frame_op(frame) == 6,
    )
    .await;
    assert_eq!(resume["d"]["session_id"], "sess-hb");
}

/// `session_start_limit.remaining == 0` means no IDENTIFY until the reset
/// Discord named — the adapter must wait it out, not spend a session start
/// it does not have.
#[tokio::test(flavor = "multi_thread")]
async fn discord_exhausted_session_budget_delays_identify() {
    let ws = spawn_ws_fixture(vec![vec![discord_hello()]]);
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        format!(
            r#"{{"url":"{}","session_start_limit":{{"total":1000,"remaining":0,"reset_after":1500,"max_concurrency":1}}}}"#,
            ws.url
        ),
    )]);
    let started = std::time::Instant::now();
    let adapter = discord_adapter(&api_base);
    let _poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    // Too early: the allowance has not reset, so no IDENTIFY may exist yet.
    tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
    assert!(
        !ws.received
            .lock()
            .unwrap()
            .iter()
            .any(|(_, frame)| frame_op(frame) == 2),
        "an IDENTIFY was sent before the session-start reset"
    );
    wait_for_frame(&ws.received, 20, "IDENTIFY after the reset", |_, frame| {
        frame_op(frame) == 2
    })
    .await;
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(1_400),
        "IDENTIFY came before Discord's reset_after: {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// Architecture: the outbox drain is the only production sender
// ---------------------------------------------------------------------------

/// Every normal reply must reach a provider through the durable outbox drain.
/// This scan pins the invariant: no daemon source file outside the drain, the
/// adapters' own internals, and the test files may call the adapter send
/// primitive.
#[test]
fn the_outbox_drain_is_the_only_production_caller_of_adapter_send() {
    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                out.push(path);
            }
        }
    }
    let daemon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/monkey-cli/daemon");
    let mut files = Vec::new();
    rust_files(&daemon, &mut files);
    assert!(files.len() > 10, "the scan found too few files to be real");

    // The drain itself, the adapters (whose internals may chunk through their
    // own send), and the channel test files that exercise the primitive.
    let allowed = |path: &std::path::Path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        path.components()
            .any(|component| component.as_os_str() == "adapters")
            || name == "channel_worker.rs"
            || name == "channel_restart_tests.rs"
            // The webhook providers' own suite, for the same reason this file
            // is here: it drives `send` directly to pin down how each adapter
            // classifies a refused connection against an ambiguous one. It has
            // no production callers in it — this scan is about those.
            || name == "channel_webhook_tests.rs"
            || name == "live_smoke.rs"
    };
    // The outbox enqueue has exactly three legitimate writers: the store
    // itself (definition), the send tool's `queue_send`, and the ingress
    // pairing challenge. Everything else — tests included — must go through
    // `plan_send`/`queue_send`, so a second hand-rolled send path cannot
    // creep in behind the tool's authority checks.
    let enqueue_allowed = |path: &std::path::Path| {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        name == "channel_store.rs"
            || name == "channel_tool.rs"
            || name == "channel_ingress.rs"
            || name == "channel_worker.rs"
    };
    let mut offenders = Vec::new();
    let mut enqueue_offenders = Vec::new();
    let mut drain_seen = false;
    let mut tool_enqueue_seen = false;
    // Assembled at runtime so this file's own scan code does not match it.
    let enqueue_needle = format!("enqueue_channel_message{}", "(");
    for path in files {
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        if source.contains("adapter.send(") {
            if path.file_name().and_then(|n| n.to_str()) == Some("channel_worker.rs") {
                drain_seen = true;
            }
            if !allowed(&path) {
                offenders.push(path.clone());
            }
        }
        if source.contains(&enqueue_needle) {
            if path.file_name().and_then(|n| n.to_str()) == Some("channel_tool.rs") {
                tool_enqueue_seen = true;
            }
            if !enqueue_allowed(&path) {
                enqueue_offenders.push(path);
            }
        }
    }
    assert!(
        drain_seen,
        "the drain call in channel_worker.rs moved; update this scan so it keeps guarding the invariant"
    );
    assert!(
        offenders.is_empty(),
        "adapter.send is called outside the outbox drain: {offenders:?}"
    );
    assert!(
        tool_enqueue_seen,
        "queue_send's enqueue in channel_tool.rs moved; update this scan so it keeps guarding the invariant"
    );
    assert!(
        enqueue_offenders.is_empty(),
        "the durable outbox is written outside the sanctioned producers: {enqueue_offenders:?}"
    );
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

// ---------------------------------------------------------------------------
// Send authority through the real tool seam
// ---------------------------------------------------------------------------

/// One accepted Telegram turn, ready to send from: the run's job id and the
/// store that accepted it.
async fn accepted_telegram_turn(store: &mut DaemonStore, queue: &FakeQueue) -> String {
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, TELEGRAM_UPDATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let report = poll_account_once(store, queue, "acct-tg", &adapter, NOW)
        .await
        .expect("poll");
    assert_eq!(report.accepted, 1);
    queue.submitted.lock().unwrap()[0].clone()
}

/// Without the cross-conversation grant the send is refused and nothing —
/// not even a parked row — reaches the durable outbox.
#[tokio::test]
async fn a_cross_conversation_send_without_the_grant_leaves_no_outbox_row() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let job_id = accepted_telegram_turn(&mut store, &queue).await;

    let paths = temp_daemon_paths();
    let request = super::channel_tool::ChannelSendRequest {
        conversation_id: Some("777777".into()),
        text: "psst".into(),
        ..Default::default()
    };
    let refused = send_via_tool_seam(&mut store, &paths, &job_id, "call-1", &request, None);
    assert!(refused.is_err(), "the send must be refused: {refused:?}");
    assert!(
        store
            .claim_outbox_batch(NOW + 60_000, 10)
            .unwrap()
            .is_empty(),
        "a refused send must not leave an outbox row"
    );
}

/// With the frozen snapshot's cross-conversation grant the same send is
/// queued, and the drain delivers it to the conversation that was named —
/// not the origin.
#[tokio::test]
async fn a_granted_cross_conversation_send_reaches_the_named_conversation() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let job_id = accepted_telegram_turn(&mut store, &queue).await;

    let paths = temp_daemon_paths();
    let policy = little_monkey_lib::run_protocol::ChannelSendPolicy {
        cross_conversation: true,
        accounts: Vec::new(),
    };
    let request = super::channel_tool::ChannelSendRequest {
        conversation_id: Some("777777".into()),
        text: "heads up".into(),
        ..Default::default()
    };
    send_via_tool_seam(
        &mut store,
        &paths,
        &job_id,
        "call-2",
        &request,
        Some(&policy),
    )
    .expect("the granted send is queued");

    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"ok":true,"result":{"message_id":55,"chat":{"id":777777,"type":"private"}}}"#
            .to_string(),
    )]);
    let adapter = telegram_adapter(&base);
    let drained = drain_outbox_once(&mut store, &adapters_map("acct-tg", Arc::new(adapter)), NOW)
        .await
        .expect("drain");
    assert_eq!(drained.sent, 1);
    let wire = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the send request");
    let wire = String::from_utf8_lossy(&wire);
    assert!(wire.contains(r#""chat_id":"777777""#), "{wire}");
    // An explicit destination inherits nothing from the origin message.
    assert!(!wire.contains("reply_parameters"), "{wire}");
    assert!(!wire.contains("reply_to_message_id"), "{wire}");
}

/// Cross-account: refused without the account grant, queued onto the named
/// account with it, and delivered through that account's adapter.
#[tokio::test]
async fn a_cross_account_send_needs_the_account_grant() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    store
        .upsert_channel_account(&account_record("acct-tg2", ChannelKind::Telegram))
        .expect("second account");
    let queue = FakeQueue::default();
    let job_id = accepted_telegram_turn(&mut store, &queue).await;

    let paths = temp_daemon_paths();
    let request = super::channel_tool::ChannelSendRequest {
        account_id: Some("acct-tg2".into()),
        conversation_id: Some("888888".into()),
        text: "over here".into(),
        ..Default::default()
    };
    let refused = send_via_tool_seam(&mut store, &paths, &job_id, "call-1", &request, None);
    assert!(refused.is_err(), "the send must be refused: {refused:?}");
    assert!(
        store
            .claim_outbox_batch(NOW + 60_000, 10)
            .unwrap()
            .is_empty(),
        "a refused cross-account send must not leave an outbox row"
    );

    let policy = little_monkey_lib::run_protocol::ChannelSendPolicy {
        cross_conversation: false,
        accounts: vec!["acct-tg2".into()],
    };
    send_via_tool_seam(
        &mut store,
        &paths,
        &job_id,
        "call-2",
        &request,
        Some(&policy),
    )
    .expect("the granted send is queued");

    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"ok":true,"result":{"message_id":56,"chat":{"id":888888,"type":"private"}}}"#
            .to_string(),
    )]);
    let adapter = telegram_adapter(&base);
    let drained = drain_outbox_once(
        &mut store,
        &adapters_map("acct-tg2", Arc::new(adapter)),
        NOW,
    )
    .await
    .expect("drain");
    assert_eq!(drained.sent, 1, "the row must drain through acct-tg2");
    let wire = requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the send request");
    let wire = String::from_utf8_lossy(&wire);
    assert!(wire.contains(r#""chat_id":"888888""#), "{wire}");
}

/// An inbound file becomes a durable artifact; the run forwards the artifact
/// id through the tool seam; the provider receives the artifact's actual
/// bytes as a real multipart upload. At no point does a filesystem path cross
/// the tool boundary.
#[tokio::test]
async fn a_forwarded_artifact_travels_by_id_and_lands_as_a_real_upload() {
    let mut store = seeded_store("acct-tg", ChannelKind::Telegram);
    let queue = FakeQueue::default();
    let paths = temp_daemon_paths();

    // The durable artifact, in the same content store `queue_send` resolves
    // ids against.
    let app_data = paths.root.parent().unwrap();
    let blob = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        super::channel_adapter::MAX_ATTACHMENT_BYTES,
    )
    .expect("content store")
    .put(b"PNGDATA")
    .expect("blob");

    // The inbound turn whose message carried the file: the durable envelope
    // records the artifact id and the name/type it arrived with, which is
    // where a forwarded id gets its filename back from.
    const PHOTO_UPDATE_TEMPLATE: &str = r#"{
        "ok": true,
        "result": [{
            "update_id": 900,
            "message": {
                "message_id": 31,
                "date": 1700000000,
                "chat": {"id": 606, "type": "private"},
                "from": {"id": 606, "is_bot": false, "first_name": "Cy"},
                "text": "keep this",
                "photo": [{"file_id": "PH2", "width": 4, "height": 4, "file_size": 7}]
            }
        }]
    }"#;
    let (base, _requests) = super::channel_adapter::test_http::serve(vec![
        (200, TELEGRAM_GET_ME.to_string()),
        (200, PHOTO_UPDATE_TEMPLATE.to_string()),
    ]);
    let adapter = telegram_adapter(&base);
    let mut batch = adapter.poll(None).await.expect("poll");
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-tg".to_string();
        for attachment in &mut envelope.attachments {
            // What hydration records once the bytes are stored.
            attachment.stored_artifact_id = Some(blob.id.clone());
            attachment.filename = Some("photo.png".to_string());
            attachment.mime_type = Some("image/png".to_string());
        }
    }
    let report = ingest_batch(&mut store, &queue, &batch.envelopes, NOW);
    assert_eq!(report.accepted, 1);
    let job_id = queue.submitted.lock().unwrap()[0].clone();

    // The run forwards the artifact by id — no path, no bytes.
    let request = super::channel_tool::ChannelSendRequest {
        text: "as requested".into(),
        artifact_ids: vec![blob.id.clone()],
        ..Default::default()
    };
    send_via_tool_seam(&mut store, &paths, &job_id, "call-1", &request, None)
        .expect("the reply with the artifact is queued");

    // The provider sees a real multipart upload carrying the stored bytes:
    // the text lands first as its own message, then the photo uploads.
    let (send_base, send_requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"result":{"message_id":57,"chat":{"id":606,"type":"private"}}}"#
                .to_string(),
        ),
        (
            200,
            r#"{"ok":true,"result":{"message_id":58,"chat":{"id":606,"type":"private"}}}"#
                .to_string(),
        ),
    ]);
    let account = account_record("acct-tg", ChannelKind::Telegram);
    let adapter = TelegramAdapter::new(&AdapterConfig {
        account: &account,
        secret: "bot-token".into(),
    })
    .expect("adapter")
    .with_base_url(&send_base)
    .with_blobs(Arc::new(super::channel_adapter::test_http::FixtureBlobs(
        b"PNGDATA".to_vec(),
    )));
    let drained = drain_outbox_once(&mut store, &adapters_map("acct-tg", Arc::new(adapter)), NOW)
        .await
        .expect("drain");
    assert_eq!(drained.sent, 1);
    let text_leg = send_requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the text request");
    assert!(
        String::from_utf8_lossy(&text_leg).contains("as requested"),
        "the text leg must land first"
    );
    let wire = send_requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the upload request");
    let wire = String::from_utf8_lossy(&wire);
    assert!(wire.contains("/sendPhoto"), "{wire}");
    assert!(wire.contains("PNGDATA"), "{wire}");
    assert!(wire.contains("photo.png"), "{wire}");
}

// ---------------------------------------------------------------------------
// Slack: provider-directed reconnect handoff
// ---------------------------------------------------------------------------

/// Like [`spawn_ws_fixture`] but accepting connections CONCURRENTLY, so a
/// replacement socket can complete its handshake while the previous one is
/// still open — the overlap Slack's connection refresh performs. Every
/// connection is greeted with `hello`; frames are logged and injected per
/// connection index.
struct HandoffWsFixture {
    url: String,
    received: Arc<StdMutex<Vec<(usize, Value)>>>,
    senders: Arc<StdMutex<Vec<mpsc::UnboundedSender<String>>>>,
    closed: Arc<StdMutex<Vec<usize>>>,
}

impl HandoffWsFixture {
    fn inject(&self, connection: usize, text: String) {
        let senders = self.senders.lock().unwrap();
        senders[connection].send(text).expect("inject");
    }

    async fn wait_connections(&self, count: usize, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while self.senders.lock().unwrap().len() < count {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn wait_closed(&self, connection: usize, what: &str) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !self.closed.lock().unwrap().contains(&connection) {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {what}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }
}

fn spawn_handoff_ws_fixture() -> HandoffWsFixture {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind ws fixture");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let received: Arc<StdMutex<Vec<(usize, Value)>>> = Arc::default();
    let senders: Arc<StdMutex<Vec<mpsc::UnboundedSender<String>>>> = Arc::default();
    let closed: Arc<StdMutex<Vec<usize>>> = Arc::default();
    let (log, injectors, ended) = (received.clone(), senders.clone(), closed.clone());
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
        let mut index = 0usize;
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<String>();
            injectors.lock().unwrap().push(inject_tx);
            let connection = index;
            index += 1;
            let log = log.clone();
            let ended = ended.clone();
            tokio::spawn(async move {
                let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
                    ended.lock().unwrap().push(connection);
                    return;
                };
                let hello =
                    serde_json::json!({ "type": "hello", "num_connections": 1 }).to_string();
                let _ = ws.send(Message::Text(hello.into())).await;
                loop {
                    tokio::select! {
                        frame = ws.next() => match frame {
                            Some(Ok(Message::Text(text))) => {
                                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                                    log.lock().unwrap().push((connection, value));
                                }
                            }
                            Some(Ok(Message::Ping(payload))) => {
                                let _ = ws.send(Message::Pong(payload)).await;
                            }
                            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                            Some(Ok(_)) => {}
                        },
                        frame = inject_rx.recv() => match frame {
                            Some(text) => { let _ = ws.send(Message::Text(text.into())).await; }
                            None => break,
                        },
                    }
                }
                ended.lock().unwrap().push(connection);
            });
        }
    });
    HandoffWsFixture {
        url: format!("ws://127.0.0.1:{port}"),
        received,
        senders,
        closed,
    }
}

/// Slack names a replacement URL in its disconnect frame → the adapter
/// connects it while the old socket is still open, retires the old socket
/// only once the replacement is live, keeps consuming on the replacement —
/// and never spends a second `apps.connections.open` call. A redelivery of
/// an already-durable event across the handoff is deduplicated, not re-run.
#[tokio::test(flavor = "multi_thread")]
async fn slack_provider_directed_reconnect_hands_off_without_a_receive_gap() {
    let mut store = seeded_store("acct-sl", ChannelKind::Slack);
    let queue = FakeQueue::default();
    let ws = spawn_handoff_ws_fixture();
    // auth.test, then exactly ONE apps.connections.open: the replacement must
    // come from the disconnect frame's URL, not a second open.
    let (api_base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#.to_string(),
        ),
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
    ]);
    let adapter = slack_adapter(&api_base);

    // First connection up; one message ingested and ACKed on it.
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    ws.wait_connections(1, "the first socket").await;
    ws.inject(
        0,
        slack_event_envelope("env-1", "3000.001", "before the refresh"),
    );
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1);
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-sl".to_string();
    }
    assert_eq!(
        ingest_batch(&mut store, &queue, &batch.envelopes, NOW).accepted,
        1
    );
    adapter.commit_batch(&batch.envelopes).await;
    wait_for_frame(&ws.received, 10, "ACK for env-1", |_, frame| {
        frame.get("envelope_id").and_then(Value::as_str) == Some("env-1")
    })
    .await;

    // The provider-directed refresh, naming the replacement URL.
    let disconnect = serde_json::json!({
        "type": "disconnect",
        "reason": "refresh_requested",
        "payload": { "connection_url": ws.url },
    })
    .to_string();
    ws.inject(0, disconnect);
    ws.wait_connections(2, "the replacement socket").await;
    ws.wait_closed(0, "the old socket to be retired").await;

    // The replacement carries traffic; a redelivery of the event that is
    // already durable collapses to a duplicate and still gets its ACK.
    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    ws.inject(
        1,
        slack_event_envelope("env-1b", "3000.001", "before the refresh"),
    );
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
        "ACK for env-1b on the replacement",
        |connection, frame| {
            connection == 1 && frame.get("envelope_id").and_then(Value::as_str) == Some("env-1b")
        },
    )
    .await;

    // One run despite two sockets and two deliveries; and the fixture saw
    // exactly the two HTTP calls above — no second connections.open.
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);
    requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("auth.test");
    requests
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("apps.connections.open");
    assert!(
        requests
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err(),
        "the handoff must not mint a second apps.connections.open URL"
    );
}

/// A disconnect frame that names no replacement URL falls back to a fresh
/// `apps.connections.open`, and the account keeps receiving.
#[tokio::test(flavor = "multi_thread")]
async fn slack_reconnect_without_a_url_falls_back_to_a_fresh_open() {
    let mut store = seeded_store("acct-sl", ChannelKind::Slack);
    let queue = FakeQueue::default();
    let ws = spawn_handoff_ws_fixture();
    let (api_base, _requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"ok":true,"user_id":"UBOT","bot_id":"B1"}"#.to_string(),
        ),
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
        // The fallback open minted by the URL-less disconnect.
        (200, format!(r#"{{"ok":true,"url":"{}"}}"#, ws.url)),
    ]);
    let adapter = slack_adapter(&api_base);

    let poll = tokio::spawn(async move {
        let batch = adapter.poll(None).await;
        (adapter, batch)
    });
    ws.wait_connections(1, "the first socket").await;
    ws.inject(
        0,
        serde_json::json!({ "type": "disconnect", "reason": "refresh_requested" }).to_string(),
    );
    ws.wait_connections(2, "the fallback socket").await;
    ws.inject(
        1,
        slack_event_envelope("env-7", "4000.001", "after the fallback"),
    );
    let (adapter, batch) = poll.await.expect("poll task");
    let mut batch = batch.expect("poll");
    assert_eq!(batch.envelopes.len(), 1, "the fallback socket must deliver");
    for envelope in &mut batch.envelopes {
        envelope.account_id = "acct-sl".to_string();
    }
    assert_eq!(
        ingest_batch(&mut store, &queue, &batch.envelopes, NOW).accepted,
        1
    );
    adapter.commit_batch(&batch.envelopes).await;
    wait_for_frame(&ws.received, 10, "ACK for env-7", |connection, frame| {
        connection == 1 && frame.get("envelope_id").and_then(Value::as_str) == Some("env-7")
    })
    .await;
}

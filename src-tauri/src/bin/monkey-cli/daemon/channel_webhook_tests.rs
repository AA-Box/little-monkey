//! Provider acknowledgement semantics for the four delivered-to channels.
//!
//! Every test here drives the SAME adapter structs release builds ship —
//! `WhatsAppAdapter`, `LineAdapter`, `TeamsAdapter`, `GoogleChatAdapter` —
//! through `webhook::accept_webhook_delivery`, which is the function the HTTP
//! route calls once it has parsed headers and read the body. Nothing in this
//! file re-implements a verifier, a normalizer or a send: the only fakes are
//! the provider endpoints themselves and the run queue behind the ingress gate.
//!
//! Four questions are asked of every provider, because getting any of them
//! wrong loses or duplicates somebody's message:
//!
//! 1. **Invalid authentication** leaves no durable event and is not
//!    acknowledged.
//! 2. **A valid new event** is durable *before* the success answer is composed.
//! 3. **A duplicate** is acknowledged, and produces one event and one run.
//! 4. **A restart** — the store closed and reopened from disk — still has the
//!    event that was acknowledged.
//!
//! A fifth test per provider walks the whole path the task names: authenticated
//! webhook → production verifier → normalized envelope → durable event → route
//! → run → durable outbox → production outbound adapter on the wire.

use std::sync::Arc;

use little_monkey_lib::channels::types::ChannelKind;

use super::adapters::google_chat::GoogleChatAdapter;
use super::adapters::line::LineAdapter;
use super::adapters::teams::TeamsAdapter;
use super::adapters::whatsapp::WhatsAppAdapter;
use super::channel_adapter::{
    AdapterConfig, AttachmentLimits, BlobSource, ChannelAdapter, ConversationReferences,
    MemoryConversationReferences, WebhookChannelAdapter,
};
use super::channel_restart_tests::{
    adapters_map, assert_one_of_everything, distinct_runs, outbound_event_id, queue_reply_for_job,
    seed_account_and_route, seeded_store, FakeQueue, NOW,
};
use super::channel_store::EventDirection;
use super::channel_worker::drain_outbox_once;
use super::store::DaemonStore;
use super::webhook::{accept_webhook_delivery, DeliveryOutcome, WebhookDelivery};

/// A blob sink that keeps nothing: these tests assert on events and requests,
/// never on stored bytes, and the daemon's real content store belongs to the
/// operator.
struct NoBlobs;

impl BlobSource for NoBlobs {
    fn read(&self, _artifact_id: &str) -> Result<Vec<u8>, String> {
        Ok(Vec::new())
    }

    fn write(&self, _bytes: &[u8]) -> Result<String, String> {
        Ok("test-blob".to_string())
    }
}

/// One delivery through the production acceptance path.
///
/// `fetcher` is `None` for every test but the wire-to-wire ones: attachment
/// hydration is the polled path's own concern and is covered there, and a
/// fixture that has to answer a media lookup as well obscures what these tests
/// are about.
async fn deliver(
    store: &mut DaemonStore,
    queue: &FakeQueue,
    adapter: &dyn WebhookChannelAdapter,
    headers: &[(String, String)],
    body: &[u8],
) -> DeliveryOutcome {
    deliver_with_fetcher(store, queue, adapter, None, headers, body, TEST_BUDGET).await
}

/// A budget short enough that a test proving one exists costs no wall clock.
const TEST_BUDGET: std::time::Duration = std::time::Duration::from_millis(200);

#[allow(clippy::too_many_arguments)]
async fn deliver_with_fetcher(
    store: &mut DaemonStore,
    queue: &FakeQueue,
    adapter: &dyn WebhookChannelAdapter,
    fetcher: Option<&dyn ChannelAdapter>,
    headers: &[(String, String)],
    body: &[u8],
    attachment_budget: std::time::Duration,
) -> DeliveryOutcome {
    accept_webhook_delivery(
        store,
        queue,
        adapter,
        fetcher,
        &NoBlobs,
        &WebhookDelivery {
            headers,
            body,
            public_base_url: Some("https://monkey.example.test"),
            limits: AttachmentLimits::default(),
            attachment_budget,
            now_ms: NOW,
        },
    )
    .await
}

/// How many inbound events the durable log holds for this account.
fn inbound_events(store: &DaemonStore, account_id: &str) -> usize {
    store
        .recent_channel_events(account_id, 50)
        .expect("events")
        .into_iter()
        .filter(|event| event.direction == EventDirection::Inbound)
        .count()
}

/// A file-backed daemon store, so a test can close it and open it again — the
/// only honest way to ask whether an acknowledged event survives a restart.
fn restartable_store(
    account_id: &str,
    kind: ChannelKind,
) -> (super::store::DaemonPaths, DaemonStore) {
    let paths = super::channel_restart_tests::temp_daemon_paths();
    let mut store = DaemonStore::open(&paths).expect("open store on disk");
    seed_account_and_route(&mut store, account_id, kind);
    (paths, store)
}

// ---------------------------------------------------------------------------
// WhatsApp Cloud API
// ---------------------------------------------------------------------------

const WA_APP_SECRET: &str = "app-secret-value";
const WA_ACCESS_TOKEN: &str = "token-value";

fn whatsapp_account(account_id: &str) -> super::channel_store::ChannelAccountRecord {
    let mut account = super::adapters::whatsapp::tests::test_account(
        serde_json::json!({ "phone_number_id": "1234567890" }),
    );
    account.account_id = account_id.to_string();
    account
}

fn whatsapp_adapter(account_id: &str) -> WhatsAppAdapter {
    let account = whatsapp_account(account_id);
    WhatsAppAdapter::new(&AdapterConfig {
        account: &account,
        secret: serde_json::json!({
            "app_secret": WA_APP_SECRET,
            "access_token": WA_ACCESS_TOKEN,
            "verify_token": "operator-chosen-token",
        })
        .to_string(),
    })
    .expect("adapter builds")
}

/// Meta signs the exact bytes it sent with the app secret.
fn whatsapp_signature(body: &[u8]) -> Vec<(String, String)> {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, WA_APP_SECRET.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    let hex: String = tag
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    vec![("x-hub-signature-256".to_string(), format!("sha256={hex}"))]
}

fn whatsapp_body(message_id: &str) -> Vec<u8> {
    serde_json::json!({
        "entry": [{
            "id": "waba-1",
            "changes": [{
                "field": "messages",
                "value": {
                    "messaging_product": "whatsapp",
                    "contacts": [{"profile": {"name": "Ada"}, "wa_id": "15550001111"}],
                    "messages": [{
                        "from": "15550001111",
                        "id": message_id,
                        "timestamp": "1700000000",
                        "type": "text",
                        "text": {"body": "hello there"}
                    }]
                }
            }]
        }]
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn whatsapp_refuses_a_body_whose_signature_does_not_match_and_records_nothing() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");

    // The right shape of header over the wrong secret — the case a forged
    // delivery actually looks like.
    let forged = {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"not-the-app-secret");
        let tag = ring::hmac::sign(&key, &body);
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        vec![("x-hub-signature-256".to_string(), format!("sha256={hex}"))]
    };

    let outcome = deliver(&mut store, &queue, &adapter, &forged, &body).await;
    assert_eq!(outcome, DeliveryOutcome::Rejected);
    assert!(!outcome.is_success(), "a forged delivery must not be ACKed");
    assert_eq!(inbound_events(&store, "acct-wa"), 0);
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn whatsapp_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");

    let outcome = deliver(
        &mut store,
        &queue,
        &adapter,
        &whatsapp_signature(&body),
        &body,
    )
    .await;

    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-wa");
    // The provider's own id is the dedupe identity, not a digest of the body.
    let events = store.recent_channel_events("acct-wa", 10).unwrap();
    assert_eq!(events[0].provider_event_id, "wamid.NEW");
}

#[tokio::test]
async fn whatsapp_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");
    let headers = whatsapp_signature(&body);

    deliver(&mut store, &queue, &adapter, &headers, &body).await;
    let second = deliver(&mut store, &queue, &adapter, &headers, &body).await;

    assert!(
        second.is_success(),
        "a redelivery must be acknowledged or the provider keeps sending it: {second:?}"
    );
    assert_eq!(
        second,
        DeliveryOutcome::Accepted {
            accepted: 0,
            duplicates: 1
        }
    );
    assert_one_of_everything(&store, &queue, "acct-wa");
}

#[tokio::test]
async fn whatsapp_keeps_an_acknowledged_event_across_a_restart() {
    let (paths, mut store) = restartable_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");
    let headers = whatsapp_signature(&body);

    assert!(deliver(&mut store, &queue, &adapter, &headers, &body)
        .await
        .is_success());
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-wa"), 1);
    // And the provider redelivering after the restart still collapses.
    let after = deliver(&mut reopened, &queue, &adapter, &headers, &body).await;
    assert!(after.is_success());
    assert_eq!(inbound_events(&reopened, "acct-wa"), 1);
    assert_eq!(distinct_runs(&queue), 1);
}

#[tokio::test]
async fn whatsapp_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"messages":[{"id":"wamid.OUT1"}]}"#.to_string(),
    )]);
    let inbound = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.IN1");

    let outcome = deliver(
        &mut store,
        &queue,
        &inbound,
        &whatsapp_signature(&body),
        &body,
    )
    .await;
    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-wa");

    // The run answers through the production tool seam, and the outbox drains
    // through the production adapter onto the fixture's socket.
    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    let outbound: Arc<dyn ChannelAdapter> =
        Arc::new(whatsapp_adapter("acct-wa").with_base_url(&base));
    let report = drain_outbox_once(&mut store, &adapters_map("acct-wa", outbound), NOW)
        .await
        .expect("drain");
    assert_eq!(report.sent, 1, "{report:?}");

    let request = String::from_utf8_lossy(&requests.recv().expect("a request")).to_string();
    assert!(
        request.starts_with("POST /1234567890/messages"),
        "the operator's own phone number id addresses the send: {request}"
    );
    assert!(
        request.contains(WA_ACCESS_TOKEN),
        "the configured user token is what authorizes it"
    );
    assert!(
        request.contains("15550001111"),
        "addressed back to the sender"
    );
    assert!(
        request.contains(r#""context":{"message_id":"wamid.IN1"}"#),
        "the answer quotes the message it answers: {request}"
    );
    assert_eq!(outbound_event_id(&mut store, "acct-wa"), "wamid.OUT1");
}

fn whatsapp_image_body(message_id: &str) -> Vec<u8> {
    serde_json::json!({
        "entry": [{
            "id": "waba-1",
            "changes": [{
                "field": "messages",
                "value": {
                    "messaging_product": "whatsapp",
                    "contacts": [{"profile": {"name": "Ada"}, "wa_id": "15550001111"}],
                    "messages": [{
                        "from": "15550001111",
                        "id": message_id,
                        "timestamp": "1700000000",
                        "type": "image",
                        "image": {"id": "media-1", "mime_type": "image/jpeg"}
                    }]
                }
            }]
        }]
    })
    .to_string()
    .into_bytes()
}

/// A media host that accepts the connection and then goes quiet — the reason
/// the download budget exists.
///
/// The download client's own read budget is ten minutes. Spending it here
/// spends it on the provider's socket, and every one of these four gives up
/// and redelivers long before that, so a single stalled file would turn one
/// message into an endless redelivery loop. The message must land anyway.
#[tokio::test]
async fn a_stalled_media_host_cannot_hold_the_provider_delivery_open() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let inbound = whatsapp_adapter("acct-wa");
    // The production adapter is what fetches, pointed at a host that answers
    // nothing: no separate stub stands in for the download path.
    let fetcher =
        whatsapp_adapter("acct-wa").with_base_url(&super::channel_adapter::test_http::stall());
    let body = whatsapp_image_body("wamid.IMG");

    let started = std::time::Instant::now();
    let outcome = deliver_with_fetcher(
        &mut store,
        &queue,
        &inbound,
        Some(&fetcher),
        &whatsapp_signature(&body),
        &body,
        TEST_BUDGET,
    )
    .await;

    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the provider was held waiting on a download: {:?}",
        started.elapsed()
    );
    assert!(
        outcome.is_success(),
        "the message itself is not lost: {outcome:?}"
    );
    assert_one_of_everything(&store, &queue, "acct-wa");
    // And the agent is told which file it did not get, rather than being handed
    // an envelope that reads as though nothing was attached.
    let events = store.recent_channel_events("acct-wa", 10).unwrap();
    assert!(
        events[0]
            .envelope_json
            .contains("before this file finished"),
        "the unfetched attachment carries no reason: {}",
        events[0].envelope_json
    );
}

// ---------------------------------------------------------------------------
// LINE Messaging API
// ---------------------------------------------------------------------------

const LINE_SECRET: &str = "line-channel-secret";
const LINE_TOKEN: &str = "line-access-token";

fn line_adapter(account_id: &str) -> LineAdapter {
    let mut account = super::adapters::line::tests::test_account();
    account.account_id = account_id.to_string();
    LineAdapter::new(&AdapterConfig {
        account: &account,
        secret: serde_json::json!({
            "channel_secret": LINE_SECRET,
            "channel_access_token": LINE_TOKEN,
        })
        .to_string(),
    })
    .expect("adapter builds")
    .with_references(Arc::new(MemoryConversationReferences::default()))
}

fn line_signature(body: &[u8]) -> Vec<(String, String)> {
    use base64::Engine as _;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, LINE_SECRET.as_bytes());
    let tag = ring::hmac::sign(&key, body);
    vec![(
        "x-line-signature".to_string(),
        base64::engine::general_purpose::STANDARD.encode(tag.as_ref()),
    )]
}

fn line_body(webhook_event_id: &str, redelivery: bool) -> Vec<u8> {
    serde_json::json!({
        "destination": "Ubot",
        "events": [{
            "type": "message",
            "webhookEventId": webhook_event_id,
            "replyToken": "reply-token-1",
            "deliveryContext": {"isRedelivery": redelivery},
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "user", "userId": "U-ada"},
            "message": {"id": "m1", "type": "text", "text": "hello there"}
        }]
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn line_refuses_a_body_whose_signature_does_not_match_and_records_nothing() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    // Signed with the wrong channel secret, which is what a forged delivery
    // to a leaked callback URL looks like.
    let forged = {
        use base64::Engine as _;
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"wrong-secret");
        let tag = ring::hmac::sign(&key, &body);
        vec![(
            "x-line-signature".to_string(),
            base64::engine::general_purpose::STANDARD.encode(tag.as_ref()),
        )]
    };

    let outcome = deliver(&mut store, &queue, &adapter, &forged, &body).await;
    assert_eq!(outcome, DeliveryOutcome::Rejected);
    assert_eq!(inbound_events(&store, "acct-line"), 0);
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn line_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    let outcome = deliver(&mut store, &queue, &adapter, &line_signature(&body), &body).await;

    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-line");
    let events = store.recent_channel_events("acct-line", 10).unwrap();
    assert_eq!(
        events[0].provider_event_id, "01NEW",
        "the webhook event id is the dedupe identity"
    );
}

#[tokio::test]
async fn line_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let first = line_body("01SAME", false);
    // LINE marks its own retry, and the body differs from the first by that
    // flag alone — so nothing but the event id can collapse the two.
    let retry = line_body("01SAME", true);

    deliver(
        &mut store,
        &queue,
        &adapter,
        &line_signature(&first),
        &first,
    )
    .await;
    let second = deliver(
        &mut store,
        &queue,
        &adapter,
        &line_signature(&retry),
        &retry,
    )
    .await;

    assert!(second.is_success(), "{second:?}");
    assert_one_of_everything(&store, &queue, "acct-line");
}

#[tokio::test]
async fn line_keeps_an_acknowledged_event_across_a_restart() {
    let (paths, mut store) = restartable_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    assert!(
        deliver(&mut store, &queue, &adapter, &line_signature(&body), &body)
            .await
            .is_success()
    );
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-line"), 1);
    let retry = line_body("01NEW", true);
    assert!(deliver(
        &mut reopened,
        &queue,
        &adapter,
        &line_signature(&retry),
        &retry
    )
    .await
    .is_success());
    assert_eq!(inbound_events(&reopened, "acct-line"), 1);
    assert_eq!(distinct_runs(&queue), 1);
}

#[tokio::test]
async fn line_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"sentMessages":[{"id":"461230966842064897"}]}"#.to_string(),
    )]);
    // One reference store across both halves, exactly as the daemon has one
    // database: what the inbound adapter records is what the outbound one
    // reads.
    let references = Arc::new(MemoryConversationReferences::default());
    let inbound = line_adapter("acct-line").with_references(references.clone());
    let body = line_body("01IN", false);

    let outcome = deliver(&mut store, &queue, &inbound, &line_signature(&body), &body).await;
    assert!(outcome.is_success(), "{outcome:?}");

    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    let outbound: Arc<dyn ChannelAdapter> = Arc::new(
        line_adapter("acct-line")
            .with_base_url(&base)
            .with_references(references.clone()),
    );
    let report = drain_outbox_once(&mut store, &adapters_map("acct-line", outbound), NOW)
        .await
        .expect("drain");
    assert_eq!(report.sent, 1, "{report:?}");

    let request = String::from_utf8_lossy(&requests.recv().expect("a request")).to_string();
    assert!(
        request.contains("/v2/bot/message/"),
        "the real Messaging API endpoint: {request}"
    );
    assert!(
        request.contains(LINE_TOKEN),
        "authorized by the configured channel token"
    );
    assert_eq!(
        outbound_event_id(&mut store, "acct-line"),
        "461230966842064897"
    );
}

// ---------------------------------------------------------------------------
// Microsoft Teams / Bot Framework
// ---------------------------------------------------------------------------

fn teams_adapter(account_id: &str, references: Arc<MemoryConversationReferences>) -> TeamsAdapter {
    let mut account = super::adapters::teams::tests::test_account();
    account.account_id = account_id.to_string();
    let adapter = TeamsAdapter::new(&AdapterConfig {
        account: &account,
        secret: serde_json::json!({ "app_password": "pw-value" }).to_string(),
    })
    .expect("adapter builds")
    .with_references(references);
    adapter.seed_jwks_for_test();
    adapter
}

/// The `Authorization: Bearer` a genuine Bot Framework delivery carries,
/// signed with the test key the adapter's JWKS cache was seeded with.
fn teams_authorization() -> Vec<(String, String)> {
    let jwt = super::adapters::teams::tests::sign_test_jwt(
        &super::adapters::teams::tests::valid_claims(NOW / 1000),
        "test-key-1",
        super::adapters::teams::tests::TEST_PRIVATE_KEY_PEM,
    );
    vec![("authorization".to_string(), format!("Bearer {jwt}"))]
}

fn teams_body(activity_id: &str) -> Vec<u8> {
    serde_json::json!({
        "type": "message",
        "id": activity_id,
        "timestamp": "2024-01-01T00:00:00.000Z",
        "serviceUrl": "https://smba.trafficmanager.net/amer/",
        "channelId": "msteams",
        "channelData": {"tenant": {"id": "tenant-1"}},
        "conversation": {"id": "19:conv1", "conversationType": "personal"},
        "from": {"id": "29:user1", "name": "Ada"},
        "recipient": {"id": "28:bot-id", "name": "Monkey"},
        "text": "hello bot"
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn teams_refuses_an_activity_with_no_valid_token_and_records_nothing() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references = Arc::new(MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references.clone());
    let body = teams_body("activity-1");

    for headers in [
        Vec::new(),
        vec![(
            "authorization".to_string(),
            "Bearer not.a.token".to_string(),
        )],
    ] {
        let outcome = deliver(&mut store, &queue, &adapter, &headers, &body).await;
        assert_eq!(outcome, DeliveryOutcome::Rejected);
    }
    assert_eq!(inbound_events(&store, "acct-teams"), 0);
    assert_eq!(distinct_runs(&queue), 0);
    // The serviceUrl in an unauthenticated body must never become an outbound
    // destination — that is the whole reason the write lives behind the JWT
    // check rather than beside it.
    assert!(
        references.get("acct-teams", "19:conv1").is_none(),
        "an unauthenticated request planted a reply address"
    );
}

#[tokio::test]
async fn teams_makes_a_new_activity_durable_before_it_answers() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references = Arc::new(MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references.clone());
    let body = teams_body("activity-1");

    let outcome = deliver(&mut store, &queue, &adapter, &teams_authorization(), &body).await;

    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-teams");
    let stored = references
        .get("acct-teams", "19:conv1")
        .expect("the verified activity's reply address is durable");
    assert_eq!(
        stored.get("service_url").and_then(|v| v.as_str()),
        Some("https://smba.trafficmanager.net/amer/")
    );
    assert_eq!(
        stored.get("tenant_id").and_then(|v| v.as_str()),
        Some("tenant-1")
    );
}

#[tokio::test]
async fn teams_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references = Arc::new(MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references);
    let body = teams_body("activity-1");
    let headers = teams_authorization();

    deliver(&mut store, &queue, &adapter, &headers, &body).await;
    let second = deliver(&mut store, &queue, &adapter, &headers, &body).await;

    assert!(second.is_success(), "{second:?}");
    assert_one_of_everything(&store, &queue, "acct-teams");
}

#[tokio::test]
async fn teams_keeps_an_acknowledged_event_and_its_reply_address_across_a_restart() {
    let (paths, mut store) = restartable_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references = Arc::new(MemoryConversationReferences::default());
    let body = teams_body("activity-1");

    {
        let adapter = teams_adapter("acct-teams", references.clone());
        assert!(
            deliver(&mut store, &queue, &adapter, &teams_authorization(), &body)
                .await
                .is_success()
        );
    }
    drop(store);

    let reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-teams"), 1);
    // The reply address is the part a process-local cache used to lose. A
    // durable turn with nowhere to answer is the defect this whole table
    // exists to close.
    assert!(
        references.get("acct-teams", "19:conv1").is_some(),
        "the serviceUrl did not survive the restart"
    );
}

/// The reference table itself, exercised through the real daemon store rather
/// than the in-memory stand-in the adapter tests use.
#[test]
fn a_teams_reply_address_written_to_the_daemon_store_is_there_after_reopening_it() {
    let (paths, mut store) = restartable_store("acct-teams", ChannelKind::Teams);
    store
        .set_channel_conversation_ref(
            "acct-teams",
            "19:conv1",
            &serde_json::json!({
                "service_url": "https://smba.trafficmanager.net/amer/",
                "tenant_id": "tenant-1",
                "last_updated_at_ms": NOW,
            }),
            NOW,
        )
        .expect("store the reference");
    drop(store);

    let reopened = DaemonStore::open(&paths).expect("reopen");
    let stored = reopened
        .channel_conversation_ref("acct-teams", "19:conv1")
        .expect("read")
        .expect("the row is still there");
    assert_eq!(
        stored.get("service_url").and_then(|v| v.as_str()),
        Some("https://smba.trafficmanager.net/amer/")
    );
}

#[tokio::test]
async fn teams_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references = Arc::new(MemoryConversationReferences::default());
    let body = teams_body("activity-1");

    let inbound = teams_adapter("acct-teams", references.clone());
    let outcome = deliver(&mut store, &queue, &inbound, &teams_authorization(), &body).await;
    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-teams");

    // Point the stored address at the fixture, keeping every other field the
    // verified activity produced. This is the one thing a loopback test must
    // rewrite: everything else about the addressing is the adapter's own.
    let (send_base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (200, r#"{"id":"activity-out-1"}"#.to_string()),
    ]);
    let mut stored = references.get("acct-teams", "19:conv1").expect("stored");
    stored["service_url"] = serde_json::Value::from(send_base.clone());
    references.put("acct-teams", "19:conv1", &stored).unwrap();

    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    // A different adapter instance entirely, as the outbox drain always has:
    // it never received the activity and knows nothing but what is stored.
    let outbound: Arc<dyn ChannelAdapter> =
        Arc::new(teams_adapter("acct-teams", references.clone()).with_login_base(&send_base));
    let report = drain_outbox_once(&mut store, &adapters_map("acct-teams", outbound), NOW)
        .await
        .expect("drain");
    assert_eq!(report.sent, 1, "{report:?}");

    let _token_request = requests.recv().expect("the token exchange");
    let activity_request =
        String::from_utf8_lossy(&requests.recv().expect("the activity POST")).to_string();
    assert!(
        activity_request.starts_with("POST /v3/conversations/19:conv1/activities/activity-1"),
        "the reply is addressed to the stored conversation and hangs from the \
         activity that prompted it: {activity_request}"
    );
    assert!(
        activity_request.contains(r#""replyToId":"activity-1""#),
        "{activity_request}"
    );
    assert_eq!(
        outbound_event_id(&mut store, "acct-teams"),
        "activity-out-1"
    );
}

// ---------------------------------------------------------------------------
// Google Chat
// ---------------------------------------------------------------------------

fn google_chat_adapter(account_id: &str) -> GoogleChatAdapter {
    let mut account = super::adapters::google_chat::tests::test_account();
    account.account_id = account_id.to_string();
    let adapter = GoogleChatAdapter::new(&AdapterConfig {
        account: &account,
        secret: serde_json::json!({
            "client_email": "bot@test-project.iam.gserviceaccount.com",
            "private_key": super::adapters::google_chat::tests::TEST_PRIVATE_KEY_PEM,
        })
        .to_string(),
    })
    .expect("adapter builds");
    adapter.seed_jwks_for_test();
    adapter
}

fn google_chat_authorization() -> Vec<(String, String)> {
    let jwt = super::adapters::google_chat::tests::sign_test_jwt(
        &super::adapters::google_chat::tests::valid_claims(NOW / 1000),
        "test-key-1",
        super::adapters::google_chat::tests::TEST_PRIVATE_KEY_PEM,
    );
    vec![("authorization".to_string(), format!("Bearer {jwt}"))]
}

fn google_chat_body(message_name: &str) -> Vec<u8> {
    serde_json::json!({
        "type": "MESSAGE",
        "eventTime": "2024-01-01T00:00:00.000Z",
        "message": {
            "name": message_name,
            "sender": {"name": "users/111", "displayName": "Ada", "type": "HUMAN"},
            "text": "hello there",
            "argumentText": "hello there",
            "thread": {"name": "spaces/AAAA/threads/TTTT"},
            "space": {"name": "spaces/AAAA", "type": "DM"}
        },
        "space": {"name": "spaces/AAAA", "type": "DM"}
    })
    .to_string()
    .into_bytes()
}

#[tokio::test]
async fn google_chat_refuses_an_unsigned_claim_of_identity_and_records_nothing() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");

    // A body that names Google's own space and sender, with no signed token
    // behind it. Trusting the payload's claim of identity rather than the JWT
    // is exactly the mistake this asserts against.
    for headers in [
        Vec::new(),
        vec![("authorization".to_string(), "Bearer nope".to_string())],
    ] {
        let outcome = deliver(&mut store, &queue, &adapter, &headers, &body).await;
        assert_eq!(outcome, DeliveryOutcome::Rejected);
    }
    assert_eq!(inbound_events(&store, "acct-gchat"), 0);
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn google_chat_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");

    let outcome = deliver(
        &mut store,
        &queue,
        &adapter,
        &google_chat_authorization(),
        &body,
    )
    .await;

    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-gchat");
    let events = store.recent_channel_events("acct-gchat", 10).unwrap();
    assert_eq!(
        events[0].provider_event_id, "spaces/AAAA/messages/BBBB",
        "the Chat message resource name is the dedupe identity"
    );
    assert_eq!(events[0].conversation_id, "spaces/AAAA");
    assert_eq!(
        events[0].thread_id.as_deref(),
        Some("spaces/AAAA/threads/TTTT")
    );
}

#[tokio::test]
async fn google_chat_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");
    let headers = google_chat_authorization();

    deliver(&mut store, &queue, &adapter, &headers, &body).await;
    let second = deliver(&mut store, &queue, &adapter, &headers, &body).await;

    assert!(second.is_success(), "{second:?}");
    assert_one_of_everything(&store, &queue, "acct-gchat");
}

#[tokio::test]
async fn google_chat_keeps_an_acknowledged_event_across_a_restart() {
    let (paths, mut store) = restartable_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");
    let headers = google_chat_authorization();

    assert!(deliver(&mut store, &queue, &adapter, &headers, &body)
        .await
        .is_success());
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-gchat"), 1);
    assert!(deliver(&mut reopened, &queue, &adapter, &headers, &body)
        .await
        .is_success());
    assert_eq!(inbound_events(&reopened, "acct-gchat"), 1);
    assert_eq!(distinct_runs(&queue), 1);
}

#[tokio::test]
async fn google_chat_full_path_from_wire_to_wire() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let inbound = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");

    let outcome = deliver(
        &mut store,
        &queue,
        &inbound,
        &google_chat_authorization(),
        &body,
    )
    .await;
    assert!(outcome.is_success(), "{outcome:?}");
    assert_one_of_everything(&store, &queue, "acct-gchat");

    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (200, r#"{"name":"spaces/AAAA/messages/OUT1"}"#.to_string()),
    ]);
    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    let outbound: Arc<dyn ChannelAdapter> =
        Arc::new(google_chat_adapter("acct-gchat").with_bases(&base, &base));
    let report = drain_outbox_once(&mut store, &adapters_map("acct-gchat", outbound), NOW)
        .await
        .expect("drain");
    assert_eq!(report.sent, 1, "{report:?}");

    let _token_request = requests.recv().expect("the service-account token exchange");
    let message_request =
        String::from_utf8_lossy(&requests.recv().expect("the message POST")).to_string();
    assert!(
        message_request.starts_with("POST /v1/spaces/AAAA/messages"),
        "the real Chat API endpoint for the space the message came from: {message_request}"
    );
    assert!(
        message_request.contains("spaces/AAAA/threads/TTTT")
            && message_request.contains("messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD"),
        "naming the thread is not replying in it without the option: {message_request}"
    );
    assert!(
        message_request.contains("requestId="),
        "Chat's own idempotency is what stops a retried row posting twice: {message_request}"
    );
    assert_eq!(
        outbound_event_id(&mut store, "acct-gchat"),
        "spaces/AAAA/messages/OUT1"
    );
}

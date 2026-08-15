//! The whole delivered-to path for the four webhook providers, end to end.
//!
//! Every test here drives the SAME code a release build ships. The adapters are
//! `WhatsAppAdapter`, `LineAdapter`, `TeamsAdapter` and `GoogleChatAdapter`; the
//! HTTP route is `webhook::handle`, served by the same hyper server the daemon's
//! listener uses; the asynchronous half is
//! `channel_worker::process_pending_channel_ingress`, the same function the
//! daemon's supervisor calls on every tick. Nothing in this file re-implements a
//! verifier, a parser, a decision or a send: the only fakes are the provider's
//! own network endpoint and the run queue behind the ingress gate.
//!
//! What the tests are shaped around is the acknowledgement boundary:
//!
//! ```text
//! HTTP request → authenticate → normalize → durable event → PROVIDER ACK
//!                                                              │
//!            (a different process, possibly after a restart) ───┘
//!                     → hydrate attachments → route → freeze → run
//!                     → durable outbox → production adapter → provider
//! ```
//!
//! So each provider is asked: does an invalid delivery leave nothing? Does a
//! valid one become durable *before* the answer, with no download, no routing
//! and no run in the request? Is the answer the exact thing that provider
//! requires? Does a redelivery collapse? And does a crash at each boundary
//! still produce exactly one run and one reply?

use std::collections::BTreeMap;
use std::sync::Arc;

use little_monkey_lib::channels::ingress::{ConversationIngress, FrozenExecutionContext};
use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind};
use serde_json::Value as JsonValue;

use super::adapters::google_chat::GoogleChatAdapter;
use super::adapters::line::LineAdapter;
use super::adapters::teams::TeamsAdapter;
use super::adapters::whatsapp::WhatsAppAdapter;
use super::channel_adapter::{
    test_secrets, AdapterConfig, BlobSource, ChannelAdapter, ConversationReferences,
    DaemonConversationReferences, WebhookChannelAdapter,
};
use super::channel_restart_tests::{
    adapters_map, assert_one_of_everything, distinct_runs, outbound_event_id, queue_reply_for_job,
    seed_account_and_route, seeded_store, temp_daemon_paths, FakeQueue, NOW,
};
use super::channel_store::{ChannelAccountRecord, EventDirection};
use super::channel_worker::{
    drain_outbox_once, process_pending_channel_ingress, PendingIngressReport, RunQueue,
};
use super::store::{DaemonPaths, DaemonStore};
use super::webhook::{accept_webhook_delivery, test_route, DeliveryOutcome, WebhookDelivery};

/// A blob sink that keeps nothing: these tests assert on events, requests and
/// runs, never on stored bytes, and the daemon's real content store belongs to
/// the operator.
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
/// Takes no queue, no fetcher and no blob store, because the shipped path takes
/// none either: everything those are for happens after the provider has been
/// answered.
fn deliver(
    store: &mut DaemonStore,
    adapter: &dyn WebhookChannelAdapter,
    headers: &[(String, String)],
    body: &[u8],
) -> DeliveryOutcome {
    accept_webhook_delivery(
        store,
        adapter,
        &WebhookDelivery {
            headers,
            body,
            public_base_url: Some(PUBLIC_BASE),
            now_ms: NOW,
        },
    )
}

/// The operator's advertised base, as the account has it configured.
const PUBLIC_BASE: &str = "https://monkey.example.test";

/// One pass of the asynchronous half, with no provider adapters loaded — the
/// shape for every message that references no file.
async fn process(store: &mut DaemonStore, queue: &dyn RunQueue) -> PendingIngressReport {
    process_with(store, queue, &BTreeMap::new()).await
}

async fn process_with(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    fetchers: &BTreeMap<String, Arc<dyn ChannelAdapter>>,
) -> PendingIngressReport {
    process_pending_channel_ingress(store, queue, fetchers, &NoBlobs, NOW)
        .await
        .expect("a pass over the accepted events")
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

/// How many accepted events are still waiting for the worker.
fn awaiting(store: &DaemonStore) -> usize {
    store
        .accepted_events_awaiting_processing(50)
        .expect("pending")
        .len()
}

/// A file-backed daemon store, so a test can close it and open it again — the
/// only honest way to ask whether an acknowledged event survives a restart.
fn restartable_store(account_id: &str, kind: ChannelKind) -> (DaemonPaths, DaemonStore) {
    let paths = temp_daemon_paths();
    let mut store = DaemonStore::open(&paths).expect("open store on disk");
    seed_account_and_route(&mut store, account_id, kind);
    (paths, store)
}

/// A whole daemon the production HTTP route can be pointed at: an enabled
/// account carrying this provider's own settings, an open route to a chat
/// recipe, and the account's credential where the route looks it up.
fn route_world(
    account_id: &str,
    kind: ChannelKind,
    config: JsonValue,
    secret: JsonValue,
) -> (DaemonPaths, DaemonStore) {
    let (paths, mut store) = restartable_store(account_id, kind);
    store
        .upsert_channel_account(&ChannelAccountRecord {
            account_id: account_id.to_string(),
            kind,
            label: "Webhook test".to_string(),
            enabled: true,
            non_secret_config: config,
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
        .expect("provider account");
    store
        .set_channel_public_base_url(Some(PUBLIC_BASE))
        .expect("public base");
    test_secrets::put(&format!("test:{account_id}"), &secret.to_string());
    (paths, store)
}

/// A queue that resolves what a turn will run as and then cannot take it.
///
/// The realistic temporary failure: the daemon is up, the route is fine, the
/// turn is durable, and the submission does not land. The provider has already
/// been answered, so nothing about this may lose the message.
#[derive(Default)]
struct RefusingQueue;

impl RunQueue for RefusingQueue {
    fn freeze_execution(
        &self,
        ingress: &ConversationIngress,
    ) -> Result<FrozenExecutionContext, String> {
        Ok(super::channel_worker::test_frozen_execution(ingress))
    }

    fn submit(
        &self,
        _ingress: &ConversationIngress,
        _params: Vec<String>,
    ) -> Result<String, String> {
        Err("the run queue is not accepting work".to_string())
    }
}

// ---------------------------------------------------------------------------
// WhatsApp Cloud API
// ---------------------------------------------------------------------------

const WA_APP_SECRET: &str = "app-secret-value";
const WA_ACCESS_TOKEN: &str = "token-value";
const WA_VERIFY_TOKEN: &str = "operator-chosen-token";

fn whatsapp_config() -> JsonValue {
    serde_json::json!({ "phone_number_id": "1234567890" })
}

fn whatsapp_secret() -> JsonValue {
    serde_json::json!({
        "app_secret": WA_APP_SECRET,
        "access_token": WA_ACCESS_TOKEN,
        "verify_token": WA_VERIFY_TOKEN,
    })
}

fn whatsapp_account(account_id: &str) -> ChannelAccountRecord {
    let mut account = super::adapters::whatsapp::tests::test_account(whatsapp_config());
    account.account_id = account_id.to_string();
    account
}

fn whatsapp_adapter(account_id: &str) -> WhatsAppAdapter {
    let account = whatsapp_account(account_id);
    WhatsAppAdapter::new(&AdapterConfig {
        account: &account,
        secret: whatsapp_secret().to_string(),
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

    let outcome = deliver(&mut store, &adapter, &forged, &body);
    assert_eq!(outcome, DeliveryOutcome::Rejected);
    assert!(!outcome.is_success(), "a forged delivery must not be ACKed");
    assert_eq!(inbound_events(&store, "acct-wa"), 0);
    process(&mut store, &queue).await;
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn whatsapp_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");

    let outcome = deliver(&mut store, &adapter, &whatsapp_signature(&body), &body);

    assert!(outcome.is_success(), "{outcome:?}");
    // Durable, and nothing more: no route was resolved and no run submitted
    // inside the request.
    assert_eq!(inbound_events(&store, "acct-wa"), 1);
    assert_eq!(awaiting(&store), 1);
    assert_eq!(distinct_runs(&queue), 0);
    // The provider's own id is the dedupe identity, not a digest of the body.
    let events = store.recent_channel_events("acct-wa", 10).unwrap();
    assert_eq!(events[0].provider_event_id, "wamid.NEW");

    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-wa");
}

#[tokio::test]
async fn whatsapp_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");
    let headers = whatsapp_signature(&body);

    deliver(&mut store, &adapter, &headers, &body);
    let second = deliver(&mut store, &adapter, &headers, &body);

    assert_eq!(
        second,
        DeliveryOutcome::Accepted {
            accepted: 0,
            duplicates: 1
        },
        "a redelivery must be acknowledged or the provider keeps sending it"
    );
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-wa");
    // And a third, arriving after the run exists, still collapses.
    assert!(deliver(&mut store, &adapter, &headers, &body).is_success());
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-wa");
}

/// The crash the acknowledgement contract is a promise about: the provider has
/// been told yes, and this process dies before doing anything else with it.
#[tokio::test]
async fn whatsapp_finishes_an_acknowledged_message_after_a_restart() {
    let (paths, mut store) = restartable_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");
    let headers = whatsapp_signature(&body);

    assert!(deliver(&mut store, &adapter, &headers, &body).is_success());
    // The daemon dies here: nothing has been downloaded, routed or run.
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-wa"), 1);
    assert_eq!(awaiting(&reopened), 1, "the restart must find work to do");
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-wa");

    // And the provider redelivering after all that still collapses.
    assert!(deliver(&mut reopened, &adapter, &headers, &body).is_success());
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-wa");
}

/// The rule with teeth: no Graph API request may happen inside the provider's
/// own delivery. Meta's media host is not on this machine, and a message whose
/// photo cannot be fetched must still be acknowledged and still run.
#[tokio::test]
async fn whatsapp_fetches_media_only_after_the_delivery_is_acknowledged() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    // Two hops, as the real Graph API has: the media id is exchanged for a
    // short-lived URL, and the bytes come from there.
    let (blob_base, blob_requests) =
        super::channel_adapter::test_http::serve(vec![(200, "the-photo-bytes".to_string())]);
    let (graph_base, graph_requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        serde_json::json!({ "url": format!("{blob_base}/asset") }).to_string(),
    )]);
    let inbound = whatsapp_adapter("acct-wa");
    let body = whatsapp_image_body("wamid.IMG");

    let outcome = deliver(&mut store, &inbound, &whatsapp_signature(&body), &body);
    assert!(outcome.is_success(), "{outcome:?}");
    assert!(
        graph_requests.try_recv().is_err() && blob_requests.try_recv().is_err(),
        "the provider was made to wait on a media download"
    );

    // The worker, later, with the production adapter doing the fetching.
    let fetcher: Arc<dyn ChannelAdapter> =
        Arc::new(whatsapp_adapter("acct-wa").with_base_url(&graph_base));
    process_with(&mut store, &queue, &adapters_map("acct-wa", fetcher)).await;

    let lookup =
        String::from_utf8_lossy(&graph_requests.recv().expect("the media lookup")).to_string();
    assert!(
        lookup.starts_with("GET /media-1") && lookup.contains(WA_ACCESS_TOKEN),
        "the media id is exchanged with the account's own token: {lookup}"
    );
    blob_requests.recv().expect("the authenticated download");
    assert_one_of_everything(&store, &queue, "acct-wa");
    let events = store.recent_channel_events("acct-wa", 10).unwrap();
    assert!(
        events[0].envelope_json.contains("test-blob"),
        "the stored envelope must carry what was fetched: {}",
        events[0].envelope_json
    );
}

/// A media host that is down costs the attachment and nothing else. The
/// provider was answered long before, the message still runs, and the agent is
/// told which file it did not get rather than being handed an envelope that
/// reads as though nothing was attached.
#[tokio::test]
async fn whatsapp_keeps_a_message_whose_media_could_not_be_fetched() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let inbound = whatsapp_adapter("acct-wa");
    let refused = super::channel_adapter::test_http::refused();
    let fetcher: Arc<dyn ChannelAdapter> =
        Arc::new(whatsapp_adapter("acct-wa").with_base_url(&refused));
    let body = whatsapp_image_body("wamid.IMG");

    assert!(deliver(&mut store, &inbound, &whatsapp_signature(&body), &body).is_success());
    process_with(&mut store, &queue, &adapters_map("acct-wa", fetcher)).await;

    assert_one_of_everything(&store, &queue, "acct-wa");
    let events = store.recent_channel_events("acct-wa", 10).unwrap();
    assert!(
        events[0].envelope_json.contains("fetch_error"),
        "a failed hydration must be visible: {}",
        events[0].envelope_json
    );
}

#[tokio::test]
async fn whatsapp_full_path_from_wire_to_wire() {
    let (paths, store) = route_world(
        "acct-wa",
        ChannelKind::WhatsApp,
        whatsapp_config(),
        whatsapp_secret(),
    );
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"messages":[{"id":"wamid.OUT1"}]}"#.to_string(),
    )]);
    let body = whatsapp_body("wamid.IN1");

    // The real HTTP route, over the real hyper server, with the real verifier.
    let response = test_route::post(
        &paths,
        "/v1/channels/acct-wa",
        &whatsapp_signature(&body),
        &body,
    )
    .await;
    assert_eq!(
        response.status, 200,
        "Meta reads the status and retries anything else: {response:?}"
    );
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/plain; charset=utf-8"),
        "Meta reads nothing but the status"
    );
    assert!(response.body.is_empty(), "{response:?}");

    // Reopened, as a restart would leave it, and only then processed.
    drop(store);
    let mut store = DaemonStore::open(&paths).expect("reopen");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-wa");

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

/// Meta will not save a callback URL at all until the endpoint echoes its
/// challenge, byte for byte, as `text/plain`.
#[tokio::test]
async fn whatsapp_answers_the_subscription_handshake_through_the_real_route() {
    let (paths, _store) = route_world(
        "acct-wa",
        ChannelKind::WhatsApp,
        whatsapp_config(),
        whatsapp_secret(),
    );

    let response = test_route::get(
        &paths,
        &format!(
            "/v1/channels/acct-wa?hub.mode=subscribe&hub.verify_token={WA_VERIFY_TOKEN}&hub.challenge=1158201444"
        ),
    )
    .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.body, "1158201444");
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/plain; charset=utf-8"),
        "Meta compares the body byte for byte"
    );

    // The operator's token is the whole gate, and a wrong one reveals nothing.
    let refused = test_route::get(
        &paths,
        "/v1/channels/acct-wa?hub.mode=subscribe&hub.verify_token=guess&hub.challenge=1158201444",
    )
    .await;
    assert_eq!(refused.status, 403);
    assert!(!refused.body.contains("1158201444"));
}

#[tokio::test]
async fn whatsapp_route_rejects_a_forged_signature_and_writes_nothing() {
    let (paths, _store) = route_world(
        "acct-wa",
        ChannelKind::WhatsApp,
        whatsapp_config(),
        whatsapp_secret(),
    );
    let body = whatsapp_body("wamid.NEW");
    let forged = {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, b"not-the-app-secret");
        let tag = ring::hmac::sign(&key, &body);
        let hex: String = tag.as_ref().iter().map(|b| format!("{b:02x}")).collect();
        vec![("x-hub-signature-256".to_string(), format!("sha256={hex}"))]
    };

    let response = test_route::post(&paths, "/v1/channels/acct-wa", &forged, &body).await;

    assert_eq!(response.status, 401);
    let store = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&store, "acct-wa"), 0);
}

// ---------------------------------------------------------------------------
// LINE Messaging API
// ---------------------------------------------------------------------------

const LINE_SECRET: &str = "line-channel-secret";
const LINE_TOKEN: &str = "line-access-token";

fn line_secret() -> JsonValue {
    serde_json::json!({
        "channel_secret": LINE_SECRET,
        "channel_access_token": LINE_TOKEN,
    })
}

fn line_adapter(account_id: &str) -> LineAdapter {
    let mut account = super::adapters::line::tests::test_account();
    account.account_id = account_id.to_string();
    LineAdapter::new(&AdapterConfig {
        account: &account,
        secret: line_secret().to_string(),
    })
    .expect("adapter builds")
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

    let outcome = deliver(&mut store, &adapter, &forged, &body);
    assert_eq!(outcome, DeliveryOutcome::Rejected);
    assert_eq!(inbound_events(&store, "acct-line"), 0);
    process(&mut store, &queue).await;
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn line_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    let outcome = deliver(&mut store, &adapter, &line_signature(&body), &body);

    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(awaiting(&store), 1);
    assert_eq!(distinct_runs(&queue), 0, "nothing runs inside the request");
    let events = store.recent_channel_events("acct-line", 10).unwrap();
    assert_eq!(
        events[0].provider_event_id, "01NEW",
        "the webhook event id is the dedupe identity"
    );
    assert!(
        !events[0].envelope_json.contains("reply-token-1"),
        "a reply token must never become durable state: {}",
        events[0].envelope_json
    );

    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-line");
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

    deliver(&mut store, &adapter, &line_signature(&first), &first);
    let second = deliver(&mut store, &adapter, &line_signature(&retry), &retry);

    assert!(second.is_success(), "{second:?}");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-line");
}

#[tokio::test]
async fn line_finishes_an_acknowledged_message_after_a_restart() {
    let (paths, mut store) = restartable_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    assert!(deliver(&mut store, &adapter, &line_signature(&body), &body).is_success());
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-line");

    let retry = line_body("01NEW", true);
    assert!(deliver(&mut reopened, &adapter, &line_signature(&retry), &retry).is_success());
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-line");
}

#[tokio::test]
async fn line_full_path_from_wire_to_wire() {
    let (paths, store) = route_world(
        "acct-line",
        ChannelKind::Line,
        serde_json::json!({}),
        line_secret(),
    );
    let queue = FakeQueue::default();
    let (base, requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"sentMessages":[{"id":"461230966842064897"}]}"#.to_string(),
    )]);
    let body = line_body("01IN", false);

    let response = test_route::post(
        &paths,
        "/v1/channels/acct-line",
        &line_signature(&body),
        &body,
    )
    .await;
    assert_eq!(response.status, 200, "LINE requires a 200: {response:?}");
    assert!(
        response.body.is_empty(),
        "LINE reads only the status: {response:?}"
    );

    drop(store);
    let mut store = DaemonStore::open(&paths).expect("reopen");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-line");

    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    let outbound: Arc<dyn ChannelAdapter> =
        Arc::new(line_adapter("acct-line").with_base_url(&base));
    let report = drain_outbox_once(&mut store, &adapters_map("acct-line", outbound), NOW)
        .await
        .expect("drain");
    assert_eq!(report.sent, 1, "{report:?}");

    let request = String::from_utf8_lossy(&requests.recv().expect("a request")).to_string();
    assert!(
        request.starts_with("POST /v2/bot/message/push"),
        "a durable answer must not depend on a reply token: {request}"
    );
    assert!(
        request.to_ascii_lowercase().contains("x-line-retry-key:"),
        "the outbox row's own idempotency key rides along: {request}"
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

/// LINE marks a quoted message with the id of the message being quoted, which
/// is the same fact every other provider calls a reply.
#[tokio::test]
async fn line_normalizes_a_quoted_message_into_the_common_reply_field() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let adapter = line_adapter("acct-line");
    let body = serde_json::json!({
        "destination": "Ubot",
        "events": [{
            "type": "message",
            "webhookEventId": "01QUOTE",
            "replyToken": "reply-token-1",
            "timestamp": 1_700_000_000_000i64,
            "source": {"type": "user", "userId": "U-ada"},
            "message": {
                "id": "m2",
                "type": "text",
                "text": "yes, that one",
                "quotedMessageId": "m1"
            }
        }]
    })
    .to_string()
    .into_bytes();

    assert!(deliver(&mut store, &adapter, &line_signature(&body), &body).is_success());

    let events = store.recent_channel_events("acct-line", 10).unwrap();
    let envelope: JsonValue = serde_json::from_str(&events[0].envelope_json).unwrap();
    assert_eq!(
        envelope
            .get("reply_to_provider_id")
            .and_then(JsonValue::as_str),
        Some("m1")
    );
}

/// Several events arriving in one delivery each become their own durable
/// event. Nothing here is keyed by conversation, so nothing can be overwritten
/// by a sibling.
#[tokio::test]
async fn line_keeps_every_event_of_one_delivery_from_the_same_conversation() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = serde_json::json!({
        "destination": "Ubot",
        "events": [
            {
                "type": "message",
                "webhookEventId": "01A",
                "replyToken": "token-a",
                "timestamp": 1_700_000_000_000i64,
                "source": {"type": "user", "userId": "U-ada"},
                "message": {"id": "m1", "type": "text", "text": "first"}
            },
            {
                "type": "message",
                "webhookEventId": "01B",
                "replyToken": "token-b",
                "timestamp": 1_700_000_000_500i64,
                "source": {"type": "user", "userId": "U-ada"},
                "message": {"id": "m2", "type": "text", "text": "second"}
            }
        ]
    })
    .to_string()
    .into_bytes();

    let outcome = deliver(&mut store, &adapter, &line_signature(&body), &body);
    assert_eq!(
        outcome,
        DeliveryOutcome::Accepted {
            accepted: 2,
            duplicates: 0
        }
    );
    process(&mut store, &queue).await;
    assert_eq!(inbound_events(&store, "acct-line"), 2);
    assert_eq!(distinct_runs(&queue), 2);
}

// ---------------------------------------------------------------------------
// Microsoft Teams / Bot Framework
// ---------------------------------------------------------------------------

fn teams_config() -> JsonValue {
    serde_json::json!({ "app_id": "app-id-1", "tenant_id": "tenant-1" })
}

fn teams_secret() -> JsonValue {
    serde_json::json!({ "app_password": "pw-value" })
}

fn teams_adapter(account_id: &str, references: Arc<dyn ConversationReferences>) -> TeamsAdapter {
    publish_teams_key();
    let mut account = super::adapters::teams::tests::test_account();
    account.account_id = account_id.to_string();
    let adapter = TeamsAdapter::new(&AdapterConfig {
        account: &account,
        secret: teams_secret().to_string(),
    })
    .expect("adapter builds")
    .with_references(references);
    adapter
}

/// Publish the Bot Framework's (test) signing key, so an adapter the
/// production route builds for itself can verify a delivery signed with it.
fn publish_teams_key() {
    super::adapters::teams::tests::publish_route_jwk();
}

/// The `Authorization: Bearer` a genuine Bot Framework delivery carries,
/// signed with the test key the adapter's JWKS cache was seeded with and
/// naming the endpoint the activity came from.
fn teams_authorization_for(service_url: &str) -> Vec<(String, String)> {
    publish_teams_key();
    let jwt = super::adapters::teams::tests::sign_test_jwt(
        &super::adapters::teams::tests::long_lived_claims(service_url),
        super::adapters::teams::tests::ROUTE_KID,
        super::adapters::teams::tests::TEST_PRIVATE_KEY_PEM,
    );
    vec![("authorization".to_string(), format!("Bearer {jwt}"))]
}

fn teams_authorization() -> Vec<(String, String)> {
    teams_authorization_for(super::adapters::teams::tests::TEST_SERVICE_URL)
}

fn teams_body_at(activity_id: &str, service_url: &str) -> Vec<u8> {
    serde_json::json!({
        "type": "message",
        "id": activity_id,
        "timestamp": "2024-01-01T00:00:00.000Z",
        "serviceUrl": service_url,
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

fn teams_body(activity_id: &str) -> Vec<u8> {
    teams_body_at(activity_id, super::adapters::teams::tests::TEST_SERVICE_URL)
}

#[tokio::test]
async fn teams_refuses_an_activity_with_no_valid_token_and_records_nothing() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references: Arc<dyn ConversationReferences> =
        Arc::new(super::channel_adapter::MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references.clone());
    let body = teams_body("activity-1");

    for headers in [
        Vec::new(),
        vec![(
            "authorization".to_string(),
            "Bearer not.a.token".to_string(),
        )],
    ] {
        let outcome = deliver(&mut store, &adapter, &headers, &body);
        assert_eq!(outcome, DeliveryOutcome::Rejected);
    }
    assert_eq!(inbound_events(&store, "acct-teams"), 0);
    process(&mut store, &queue).await;
    assert_eq!(distinct_runs(&queue), 0);
    // The serviceUrl in an unauthenticated body must never become an outbound
    // destination — that is the whole reason the write lives behind the JWT
    // check rather than beside it.
    assert!(
        references.get("acct-teams", "19:conv1").is_none(),
        "an unauthenticated request planted a reply address"
    );
}

/// A token that is real, current and minted for this bot — but issued for a
/// different endpoint than the body claims. Accepting it would let a replay
/// choose where this process later POSTs the bot's own bearer token.
#[tokio::test]
async fn teams_refuses_an_activity_the_token_does_not_vouch_for() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let references: Arc<dyn ConversationReferences> =
        Arc::new(super::channel_adapter::MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references.clone());

    let outcome = deliver(
        &mut store,
        &adapter,
        &teams_authorization_for("https://smba.trafficmanager.net/emea/"),
        &teams_body("activity-1"),
    );

    assert_eq!(outcome, DeliveryOutcome::Rejected);
    assert_eq!(inbound_events(&store, "acct-teams"), 0);
    assert!(
        references.get("acct-teams", "19:conv1").is_none(),
        "a mismatched token planted a reply address"
    );
}

#[tokio::test]
async fn teams_makes_a_new_activity_durable_before_it_answers() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references: Arc<dyn ConversationReferences> =
        Arc::new(super::channel_adapter::MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references.clone());
    let body = teams_body("activity-1");

    let outcome = deliver(&mut store, &adapter, &teams_authorization(), &body);

    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(awaiting(&store), 1);
    assert_eq!(distinct_runs(&queue), 0);
    let stored = references
        .get("acct-teams", "19:conv1")
        .expect("the verified activity's reply address is durable");
    assert_eq!(
        stored.get("service_url").and_then(|v| v.as_str()),
        Some(super::adapters::teams::tests::TEST_SERVICE_URL)
    );
    assert_eq!(
        stored.get("tenant_id").and_then(|v| v.as_str()),
        Some("tenant-1")
    );

    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-teams");
}

#[tokio::test]
async fn teams_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-teams", ChannelKind::Teams);
    let queue = FakeQueue::default();
    let references: Arc<dyn ConversationReferences> =
        Arc::new(super::channel_adapter::MemoryConversationReferences::default());
    let adapter = teams_adapter("acct-teams", references);
    let body = teams_body("activity-1");
    let headers = teams_authorization();

    deliver(&mut store, &adapter, &headers, &body);
    let second = deliver(&mut store, &adapter, &headers, &body);

    assert!(second.is_success(), "{second:?}");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-teams");
}

/// The reply address, through the production store, across a real close and
/// reopen — with two adapters that share no memory at all.
///
/// The first adapter is dropped, its store connection is closed, the daemon
/// state is opened afresh, and a second adapter built from nothing but the
/// account row has to find where to answer. The activity's own `serviceUrl` is
/// the fixture, so what the second adapter POSTs to is exactly what the first
/// one stored — nothing is rewritten in between.
#[tokio::test]
async fn a_teams_reply_address_survives_a_real_restart_through_the_production_store() {
    let (send_base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (200, r#"{"id":"activity-out-1"}"#.to_string()),
    ]);
    let (paths, mut store) = restartable_store("acct-teams", ChannelKind::Teams);
    let body = teams_body_at("activity-1", &send_base);

    {
        // The production reference store, pointed at this test's own daemon.
        let receiver = teams_adapter(
            "acct-teams",
            Arc::new(DaemonConversationReferences::at(paths.clone())),
        );
        assert!(deliver(
            &mut store,
            &receiver,
            &teams_authorization_for(&send_base),
            &body
        )
        .is_success());
    }
    // Everything the receiving process held is gone.
    drop(store);

    let reopened = DaemonStore::open(&paths).expect("reopen");
    assert_eq!(inbound_events(&reopened, "acct-teams"), 1);
    drop(reopened);

    let sender = teams_adapter(
        "acct-teams",
        Arc::new(DaemonConversationReferences::at(paths.clone())),
    )
    .with_login_base(&send_base);
    let outcome = sender
        .send(&little_monkey_lib::channels::types::OutboundMessage {
            account_id: "acct-teams".to_string(),
            kind: ChannelKind::Teams,
            conversation_id: "19:conv1".to_string(),
            thread_id: None,
            text: "on it".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: Some("activity-1".to_string()),
            idempotency_key: "idem-1".to_string(),
        })
        .await;

    assert!(
        matches!(
            outcome,
            little_monkey_lib::channels::types::SendOutcome::Sent { .. }
        ),
        "a restart must not strand a durable reply: {outcome:?}"
    );
    let _token_request = requests.recv().expect("the token exchange");
    let activity_request =
        String::from_utf8_lossy(&requests.recv().expect("the activity POST")).to_string();
    assert!(
        activity_request.starts_with("POST /v3/conversations/19:conv1/activities/activity-1"),
        "addressed from the row the first adapter wrote: {activity_request}"
    );
}

/// Addressing, and nothing that authorizes anything, in the durable row.
#[tokio::test]
async fn a_teams_reply_address_row_holds_no_credential() {
    let (paths, mut store) = restartable_store("acct-teams", ChannelKind::Teams);
    let adapter = teams_adapter(
        "acct-teams",
        Arc::new(DaemonConversationReferences::at(paths.clone())),
    );
    assert!(deliver(
        &mut store,
        &adapter,
        &teams_authorization(),
        &teams_body("activity-1")
    )
    .is_success());

    let stored = DaemonStore::open(&paths)
        .expect("reopen")
        .channel_conversation_ref("acct-teams", "19:conv1")
        .expect("read")
        .expect("the row is there")
        .to_string();
    for forbidden in ["pw-value", "Bearer", "access_token", "app_password"] {
        assert!(
            !stored.contains(forbidden),
            "'{forbidden}' reached the reference table: {stored}"
        );
    }
}

#[tokio::test]
async fn teams_full_path_from_wire_to_wire() {
    let (send_base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (200, r#"{"id":"activity-out-1"}"#.to_string()),
    ]);
    let (paths, store) = route_world(
        "acct-teams",
        ChannelKind::Teams,
        teams_config(),
        teams_secret(),
    );
    let queue = FakeQueue::default();
    let body = teams_body_at("activity-1", &send_base);

    let response = test_route::post(
        &paths,
        "/v1/channels/acct-teams",
        &teams_authorization_for(&send_base),
        &body,
    )
    .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/json"),
        "the Bot Framework reads the body as an optional response activity"
    );
    assert_eq!(response.body, "{}");

    drop(store);
    let mut store = DaemonStore::open(&paths).expect("reopen");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-teams");

    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("the accepted turn was queued");
    queue_reply_for_job(&mut store, &job_id, "on it");
    // A different adapter instance entirely, as the outbox drain always has:
    // it never received the activity and knows nothing but what is stored.
    let outbound: Arc<dyn ChannelAdapter> = Arc::new(
        teams_adapter(
            "acct-teams",
            Arc::new(DaemonConversationReferences::at(paths.clone())),
        )
        .with_login_base(&send_base),
    );
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

fn google_chat_config() -> JsonValue {
    serde_json::json!({ "project_number": "123456789" })
}

fn google_chat_secret() -> JsonValue {
    serde_json::json!({
        "client_email": "bot@test-project.iam.gserviceaccount.com",
        "private_key": super::adapters::google_chat::tests::TEST_PRIVATE_KEY_PEM,
    })
}

fn google_chat_adapter(account_id: &str) -> GoogleChatAdapter {
    super::adapters::google_chat::tests::publish_route_jwk();
    let mut account = super::adapters::google_chat::tests::test_account();
    account.account_id = account_id.to_string();
    let adapter = GoogleChatAdapter::new(&AdapterConfig {
        account: &account,
        secret: google_chat_secret().to_string(),
    })
    .expect("adapter builds");
    adapter
}

fn google_chat_authorization() -> Vec<(String, String)> {
    super::adapters::google_chat::tests::publish_route_jwk();
    let jwt = super::adapters::google_chat::tests::sign_test_jwt(
        &super::adapters::google_chat::tests::long_lived_claims(),
        super::adapters::google_chat::tests::ROUTE_KID,
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
        let outcome = deliver(&mut store, &adapter, &headers, &body);
        assert_eq!(outcome, DeliveryOutcome::Rejected);
    }
    assert_eq!(inbound_events(&store, "acct-gchat"), 0);
    process(&mut store, &queue).await;
    assert_eq!(distinct_runs(&queue), 0);
}

#[tokio::test]
async fn google_chat_makes_a_new_message_durable_before_it_answers() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");

    let outcome = deliver(&mut store, &adapter, &google_chat_authorization(), &body);

    assert!(outcome.is_success(), "{outcome:?}");
    assert_eq!(awaiting(&store), 1);
    assert_eq!(distinct_runs(&queue), 0);
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

    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-gchat");
}

#[tokio::test]
async fn google_chat_answers_a_redelivery_but_runs_it_once() {
    let mut store = seeded_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");
    let headers = google_chat_authorization();

    deliver(&mut store, &adapter, &headers, &body);
    let second = deliver(&mut store, &adapter, &headers, &body);

    assert!(second.is_success(), "{second:?}");
    process(&mut store, &queue).await;
    assert_one_of_everything(&store, &queue, "acct-gchat");
}

#[tokio::test]
async fn google_chat_finishes_an_acknowledged_message_after_a_restart() {
    let (paths, mut store) = restartable_store("acct-gchat", ChannelKind::GoogleChat);
    let queue = FakeQueue::default();
    let adapter = google_chat_adapter("acct-gchat");
    let body = google_chat_body("spaces/AAAA/messages/BBBB");
    let headers = google_chat_authorization();

    assert!(deliver(&mut store, &adapter, &headers, &body).is_success());
    drop(store);

    let mut reopened = DaemonStore::open(&paths).expect("reopen");
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-gchat");

    assert!(deliver(&mut reopened, &adapter, &headers, &body).is_success());
    process(&mut reopened, &queue).await;
    assert_one_of_everything(&reopened, &queue, "acct-gchat");
}

#[tokio::test]
async fn google_chat_full_path_from_wire_to_wire() {
    let (paths, store) = route_world(
        "acct-gchat",
        ChannelKind::GoogleChat,
        google_chat_config(),
        google_chat_secret(),
    );
    let queue = FakeQueue::default();
    let body = google_chat_body("spaces/AAAA/messages/BBBB");

    let response = test_route::post(
        &paths,
        "/v1/channels/acct-gchat",
        &google_chat_authorization(),
        &body,
    )
    .await;
    assert_eq!(response.status, 200, "{response:?}");
    assert_eq!(
        response.content_type.as_deref(),
        Some("application/json"),
        "Google Chat reads a 200's body as an optional immediate reply"
    );
    assert_eq!(response.body, "{}");

    drop(store);
    let mut store = DaemonStore::open(&paths).expect("reopen");
    process(&mut store, &queue).await;
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

/// Chat's own idempotency only works if the id is a property of the outbox row
/// rather than of the attempt.
#[tokio::test]
async fn google_chat_sends_the_same_request_id_on_every_attempt() {
    let (base, requests) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (500, r#"{"error":"try again"}"#.to_string()),
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (200, r#"{"name":"spaces/AAAA/messages/OUT1"}"#.to_string()),
    ]);
    let adapter = google_chat_adapter("acct-gchat").with_bases(&base, &base);
    let message = little_monkey_lib::channels::types::OutboundMessage {
        account_id: "acct-gchat".to_string(),
        kind: ChannelKind::GoogleChat,
        conversation_id: "spaces/AAAA".to_string(),
        thread_id: None,
        text: "on it".to_string(),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        idempotency_key: "outbox-row-7".to_string(),
    };

    adapter.send(&message).await;
    // A fresh adapter, as a restart between attempts would produce.
    let retried = google_chat_adapter("acct-gchat").with_bases(&base, &base);
    retried.send(&message).await;

    let mut request_ids = Vec::new();
    for _ in 0..2 {
        let _token = requests.recv().expect("a token exchange");
        let sent = String::from_utf8_lossy(&requests.recv().expect("a message POST")).to_string();
        let line = sent.lines().next().unwrap_or_default().to_string();
        request_ids.push(
            line.split("requestId=")
                .nth(1)
                .unwrap_or_default()
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string(),
        );
    }
    assert!(!request_ids[0].is_empty(), "{request_ids:?}");
    assert_eq!(
        request_ids[0], request_ids[1],
        "a request id that changed per attempt would collapse nothing"
    );
}

// ---------------------------------------------------------------------------
// The boundary itself, provider by provider
// ---------------------------------------------------------------------------

/// The regression test that makes it hard to put run submission back into the
/// webhook request: the queue cannot take anything at all, and the provider is
/// answered anyway, because the message is already ours.
#[tokio::test]
async fn a_queue_that_cannot_take_a_run_never_costs_the_provider_its_acknowledgement() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");
    let refusing = RefusingQueue;

    let outcome = deliver(&mut store, &adapter, &whatsapp_signature(&body), &body);
    assert!(
        outcome.is_success(),
        "the queue has nothing to do with whether the message arrived: {outcome:?}"
    );

    // The worker runs and cannot finish the job.
    let report = process(&mut store, &refusing).await;
    assert_eq!(report.deferred, 1, "{report:?}");
    assert_eq!(report.queued, 0);
    assert_eq!(
        store.recent_ingress_turns(10).unwrap().len(),
        1,
        "the turn is durable even though nothing took it"
    );

    // The queue comes back. Nobody has to redeliver anything.
    let queue = FakeQueue::default();
    let recovery = super::channel_ingress::recover_pending_ingress(&mut store, &queue, NOW + 1)
        .expect("recovery");
    assert_eq!(recovery.resubmitted, 1, "{recovery:?}");
    assert_one_of_everything(&store, &queue, "acct-wa");
}

/// A crash between storing what was downloaded and deciding the message. The
/// files are already on disk, so the restart does not fetch them again, and the
/// message still becomes exactly one run.
#[tokio::test]
async fn a_crash_between_hydration_and_routing_costs_neither_the_files_nor_the_run() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let (blob_base, _blob_requests) =
        super::channel_adapter::test_http::serve(vec![(200, "the-photo-bytes".to_string())]);
    // Exactly one media lookup is served. A second attempt would find nothing,
    // which is what makes "the files are not fetched again" an assertion rather
    // than a hope.
    let (graph_base, graph_requests) = super::channel_adapter::test_http::serve(vec![(
        200,
        serde_json::json!({ "url": format!("{blob_base}/asset") }).to_string(),
    )]);
    let fetcher: Arc<dyn ChannelAdapter> =
        Arc::new(whatsapp_adapter("acct-wa").with_base_url(&graph_base));
    let fetchers = adapters_map("acct-wa", fetcher);
    let body = whatsapp_image_body("wamid.IMG");

    assert!(deliver(
        &mut store,
        &whatsapp_adapter("acct-wa"),
        &whatsapp_signature(&body),
        &body
    )
    .is_success());

    super::fail_points::arm(super::fail_points::FailPoint::AfterAttachmentHydration);
    let interrupted =
        process_pending_channel_ingress(&mut store, &queue, &fetchers, &NoBlobs, NOW).await;
    assert!(interrupted.is_err(), "{interrupted:?}");
    assert!(super::fail_points::fired());
    assert_eq!(distinct_runs(&queue), 0, "nothing was decided");
    assert_eq!(awaiting(&store), 1, "the message is still owed a run");
    graph_requests.recv().expect("the one media lookup");

    // The restart. The stored envelope already names the artifact, so no second
    // fetch is attempted, and the decision happens for the first time.
    process_with(&mut store, &queue, &fetchers).await;
    assert_one_of_everything(&store, &queue, "acct-wa");
    assert!(
        graph_requests.try_recv().is_err(),
        "an attachment already on disk was downloaded twice"
    );
}

/// A crash after the turn is durable and before it reaches the queue. This is
/// the boundary the turn-level recovery owns, and the event must not be
/// selected again by the message-level one.
#[tokio::test]
async fn a_crash_before_the_queue_submission_leaves_the_turn_for_recovery() {
    let mut store = seeded_store("acct-line", ChannelKind::Line);
    let queue = FakeQueue::default();
    let adapter = line_adapter("acct-line");
    let body = line_body("01NEW", false);

    assert!(deliver(&mut store, &adapter, &line_signature(&body), &body).is_success());

    super::fail_points::arm(super::fail_points::FailPoint::BeforeQueueSubmit);
    let report = process(&mut store, &queue).await;
    assert!(super::fail_points::fired());
    assert_eq!(report.deferred, 1, "{report:?}");
    assert_eq!(distinct_runs(&queue), 0);
    assert_eq!(
        awaiting(&store),
        0,
        "the event owns a turn now, so the turn's own recovery has it"
    );

    let recovery = super::channel_ingress::recover_pending_ingress(&mut store, &queue, NOW + 1)
        .expect("recovery");
    assert_eq!(recovery.resubmitted, 1, "{recovery:?}");
    assert_one_of_everything(&store, &queue, "acct-line");
}

/// Nothing about an unauthenticated delivery is durable, so there is nothing
/// for the worker to find and nothing for an operator to clean up.
#[tokio::test]
async fn a_refused_delivery_leaves_no_work_behind_for_any_provider() {
    for (account_id, kind) in [
        ("acct-wa", ChannelKind::WhatsApp),
        ("acct-line", ChannelKind::Line),
        ("acct-teams", ChannelKind::Teams),
        ("acct-gchat", ChannelKind::GoogleChat),
    ] {
        let mut store = seeded_store(account_id, kind);
        let queue = FakeQueue::default();
        let bad = vec![("authorization".to_string(), "Bearer nope".to_string())];
        let outcome = match kind {
            ChannelKind::WhatsApp => deliver(
                &mut store,
                &whatsapp_adapter(account_id),
                &bad,
                &whatsapp_body("wamid.X"),
            ),
            ChannelKind::Line => deliver(
                &mut store,
                &line_adapter(account_id),
                &bad,
                &line_body("01X", false),
            ),
            ChannelKind::Teams => deliver(
                &mut store,
                &teams_adapter(
                    account_id,
                    Arc::new(super::channel_adapter::MemoryConversationReferences::default()),
                ),
                &bad,
                &teams_body("activity-x"),
            ),
            _ => deliver(
                &mut store,
                &google_chat_adapter(account_id),
                &bad,
                &google_chat_body("spaces/AAAA/messages/X"),
            ),
        };
        assert_eq!(outcome, DeliveryOutcome::Rejected, "{account_id}");
        assert_eq!(awaiting(&store), 0, "{account_id}");
        process(&mut store, &queue).await;
        assert_eq!(distinct_runs(&queue), 0, "{account_id}");
    }
}

/// What the classification is actually for: the durable outbox row.
///
/// A send that may have reached the provider must leave the row needing
/// reconciliation rather than queued for another attempt, because the retry is
/// how one answer becomes two.
#[tokio::test]
async fn an_ambiguous_send_parks_the_outbox_row_instead_of_retrying_it() {
    let mut store = seeded_store("acct-wa", ChannelKind::WhatsApp);
    let queue = FakeQueue::default();
    let adapter = whatsapp_adapter("acct-wa");
    let body = whatsapp_body("wamid.NEW");

    assert!(deliver(&mut store, &adapter, &whatsapp_signature(&body), &body).is_success());
    process(&mut store, &queue).await;
    let job_id = store.recent_ingress_turns(1).unwrap()[0]
        .job_id
        .clone()
        .expect("queued");
    queue_reply_for_job(&mut store, &job_id, "on it");

    let hangup = super::channel_adapter::test_http::accept_then_hangup();
    let outbound: Arc<dyn ChannelAdapter> =
        Arc::new(whatsapp_adapter("acct-wa").with_base_url(&hangup));
    let report = drain_outbox_once(&mut store, &adapters_map("acct-wa", outbound), NOW)
        .await
        .expect("drain");

    assert_eq!(report.needs_reconciliation, 1, "{report:?}");
    assert_eq!(report.retrying, 0, "{report:?}");
    // And nothing is claimable afterwards, so no later drain repeats it.
    assert!(store
        .claim_outbox_batch(NOW + 60 * 60 * 1000, 10)
        .expect("claim")
        .is_empty());
}

/// Every provider, both directions of the ambiguity question.
///
/// A refused connection proves the request never left, so the outbox may retry
/// it. A connection that took the bytes and then went away proves nothing, and
/// a blind retry there is how one answer becomes two.
#[tokio::test]
async fn an_ambiguous_send_is_never_retried_blindly_and_a_refused_one_always_is() {
    use little_monkey_lib::channels::types::{OutboundMessage, SendOutcome};

    let refused = super::channel_adapter::test_http::refused();
    let hangup = super::channel_adapter::test_http::accept_then_hangup();
    let message = |kind: ChannelKind, conversation_id: &str| OutboundMessage {
        account_id: "acct-x".to_string(),
        kind,
        conversation_id: conversation_id.to_string(),
        thread_id: None,
        text: "on it".to_string(),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        idempotency_key: "idem-1".to_string(),
    };

    // WhatsApp and LINE post the message itself with no token exchange first,
    // so both failures land on the send.
    for (label, retryable, ambiguous) in [
        (
            "whatsapp",
            whatsapp_adapter("acct-x")
                .with_base_url(&refused)
                .send(&message(ChannelKind::WhatsApp, "15550001111"))
                .await,
            whatsapp_adapter("acct-x")
                .with_base_url(&hangup)
                .send(&message(ChannelKind::WhatsApp, "15550001111"))
                .await,
        ),
        (
            "line",
            line_adapter("acct-x")
                .with_base_url(&refused)
                .send(&message(ChannelKind::Line, "U-ada"))
                .await,
            line_adapter("acct-x")
                .with_base_url(&hangup)
                .send(&message(ChannelKind::Line, "U-ada"))
                .await,
        ),
    ] {
        assert!(
            matches!(retryable, SendOutcome::RetryableFailure { .. }),
            "{label}: a refused connection is safe to retry, got {retryable:?}"
        );
        assert!(
            matches!(ambiguous, SendOutcome::NeedsReconciliation { .. }),
            "{label}: a send that may have landed must be reconciled, got {ambiguous:?}"
        );
    }

    // Google Chat and Teams buy a token first, so the ambiguity has to be
    // produced on the request that carries the message.
    let (token_base, _tokens) = super::channel_adapter::test_http::serve(vec![
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
        (
            200,
            r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
        ),
    ]);
    let chat_ambiguous = google_chat_adapter("acct-x")
        .with_bases(&hangup, &token_base)
        .send(&message(ChannelKind::GoogleChat, "spaces/AAAA"))
        .await;
    assert!(
        matches!(chat_ambiguous, SendOutcome::NeedsReconciliation { .. }),
        "google chat: {chat_ambiguous:?}"
    );
    let chat_refused = google_chat_adapter("acct-x")
        .with_bases(&refused, &token_base)
        .send(&message(ChannelKind::GoogleChat, "spaces/AAAA"))
        .await;
    assert!(
        matches!(chat_refused, SendOutcome::RetryableFailure { .. }),
        "google chat: {chat_refused:?}"
    );

    let references: Arc<dyn ConversationReferences> =
        Arc::new(super::channel_adapter::MemoryConversationReferences::default());
    references
        .put(
            "acct-x",
            "19:conv1",
            &serde_json::json!({ "service_url": hangup, "bot_id": "28:bot" }),
        )
        .expect("seed");
    let (teams_token_base, _teams_tokens) = super::channel_adapter::test_http::serve(vec![(
        200,
        r#"{"access_token":"tok","expires_in":3600}"#.to_string(),
    )]);
    let teams_ambiguous = teams_adapter("acct-x", references.clone())
        .with_login_base(&teams_token_base)
        .send(&message(ChannelKind::Teams, "19:conv1"))
        .await;
    assert!(
        matches!(teams_ambiguous, SendOutcome::NeedsReconciliation { .. }),
        "teams: {teams_ambiguous:?}"
    );
}

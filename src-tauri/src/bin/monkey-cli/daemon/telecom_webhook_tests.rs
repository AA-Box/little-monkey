//! The carrier callback path, end to end through the production route.
//!
//! Every test here posts to `webhook::handle` — the same function the daemon's
//! listener serves — and asserts on what a carrier would actually see and on
//! what the daemon durably kept. Nothing re-implements a verifier, a
//! normalizer, a policy or a store write.
//!
//! The boundary these are shaped around is the one a carrier retries against:
//!
//! ```text
//! POST → verify signature → dedupe on the carrier's event id → durable row
//!      → CARRIER ANSWER (a TwiML-shaped document for a call to answer,
//!                        an acknowledgement for everything else)
//! ```
//!
//! So each test asks: does an unverified body leave nothing behind? Does a ring
//! reach the answering policy rather than being dropped as progress for a call
//! nobody placed? Does a redelivery collapse instead of ringing twice? Does a
//! delivery receipt land on the message it is about? And does an operator find
//! out when their carrier's callbacks stop verifying at all?

use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

use super::channel_restart_tests::temp_daemon_paths;
use super::store::{DaemonPaths, DaemonStore};
use super::telecom_store::{
    CallDirection, CallLimits, InboundCallPolicy, OutboundCallApproval, TelecomAccountRecord,
};
use super::telephony::mock::MockProvider;
use super::telephony::{TelecomConfig, TelecomKind};
use super::webhook::test_route;

const NOW: i64 = 1_700_000_000_000;
const ACCOUNT: &str = "tel-wire";
const NUMBER: &str = "+15550001111";

/// A telephony account with no stored credential.
///
/// The route resolves the carrier secret from the keychain and falls back to an
/// empty one, so a test signs with the same empty secret the route will verify
/// with — real verification, no keychain entry, nothing left on the machine.
fn account(inbound_policy: InboundCallPolicy) -> TelecomAccountRecord {
    TelecomAccountRecord {
        account_id: ACCOUNT.to_string(),
        kind: TelecomKind::Mock,
        label: "Wire test line".to_string(),
        enabled: true,
        carrier_account_id: "carrier-1".to_string(),
        from_number: NUMBER.to_string(),
        credential_ref: None,
        public_base_url: Some("https://ops.example.test".to_string()),
        non_secret_config: serde_json::json!({}),
        inbound_policy,
        outbound_approval: OutboundCallApproval::Never,
        limits: CallLimits::default(),
        health: ChannelHealth {
            state: HealthState::Connected,
            detail: None,
            last_error: None,
            probed_at_ms: NOW,
        },
        created_at_ms: NOW,
        updated_at_ms: NOW,
    }
}

fn seeded(inbound_policy: InboundCallPolicy) -> (DaemonPaths, TelecomAccountRecord) {
    let paths = temp_daemon_paths();
    let record = account(inbound_policy);
    let mut store = DaemonStore::open(&paths).expect("open store");
    store.upsert_telecom_account(&record).expect("seed account");
    // The messaging side of the number, as `telecom add` creates it.
    super::telecom_worker::ensure_sms_channel_account(&mut store, &record, NOW)
        .expect("sms account");
    (paths, record)
}

/// The carrier a test signs with: the same construction the route builds.
fn carrier() -> MockProvider {
    MockProvider::new(TelecomConfig {
        account_id: ACCOUNT.to_string(),
        kind: TelecomKind::Mock,
        carrier_account_id: "carrier-1".to_string(),
        from_number: NUMBER.to_string(),
        secret: String::new(),
        public_base_url: Some("https://ops.example.test".to_string()),
        webhook_public_key: None,
    })
}

fn path() -> String {
    super::telephony::callback_path(ACCOUNT)
}

fn store(paths: &DaemonPaths) -> DaemonStore {
    DaemonStore::open(paths).expect("reopen store")
}

/// A default route, so an accepted text has somewhere to run. Without one the
/// gate has nothing to hand the message to and records it as failed.
fn add_default_route(paths: &DaemonPaths) {
    use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
    store(paths)
        .insert_channel_route(&ChannelRoute {
            route_id: "route-wire".to_string(),
            scope: RouteScope::global_default(),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        })
        .expect("route");
}

fn approve_sender(paths: &DaemonPaths, sender_id: &str) {
    let mut store = store(paths);
    store
        .upsert_channel_sender(
            ACCOUNT,
            sender_id,
            &super::channel_store::StoredSenderAuthorization {
                sender_id: sender_id.to_string(),
                state: little_monkey_lib::channels::policy::SenderState::Approved,
                pairing_code_digest: None,
                requested_at_ms: NOW,
                expires_at_ms: None,
                approved_at_ms: Some(NOW),
                blocked_at_ms: None,
                display_label: None,
                metadata: Default::default(),
            },
        )
        .expect("approve");
}

/// One already-sent text on a number, as the outbox drain would have left it.
///
/// Lives here rather than beside the code under test because the outbox has
/// exactly four sanctioned production producers, and a ratchet in
/// `channel_restart_tests` enforces that by scanning source. A fixture staged
/// from a file that only exists under `cfg(test)` is not a fifth producer; the
/// same lines inside a production module would be indistinguishable from one.
pub(crate) fn stage_sent_text(
    store: &mut DaemonStore,
    account_id: &str,
    text: &str,
    provider_message_id: &str,
) -> String {
    let enqueued = store
        .enqueue_channel_message(&super::channel_store::NewOutboxMessage {
            account_id: account_id.to_string(),
            conversation_id: "+15551234567".to_string(),
            thread_id: None,
            reply_to_provider_id: None,
            payload_json: serde_json::json!({ "text": text }).to_string(),
            payload_digest: format!("digest-{provider_message_id}"),
            idempotency_key: format!("outbox-{provider_message_id}"),
            invocation_id: None,
            max_attempts: 3,
            job_id: None,
            created_at_ms: NOW,
        })
        .expect("enqueue");
    let outbox_id = match enqueued {
        super::channel_store::OutboxEnqueue::Queued { outbox_id }
        | super::channel_store::OutboxEnqueue::AlreadyQueued { outbox_id } => outbox_id,
    };
    store
        .complete_outbox_send(
            &outbox_id,
            &little_monkey_lib::channels::types::SendOutcome::Sent {
                provider_message_id: Some(provider_message_id.to_string()),
            },
            NOW,
        )
        .expect("sent");
    outbox_id
}

#[tokio::test]
async fn a_forged_callback_leaves_nothing_and_is_counted_against_the_account() {
    let (paths, _) = seeded(InboundCallPolicy::Answer);
    let carrier = carrier();
    let (_, body) = carrier.sign_inbound_sms("+15551234567", NUMBER, "let me in");
    let forged = vec![(
        "X-Mock-Signature".to_string(),
        "not-a-signature".to_string(),
    )];

    let response = test_route::post(&paths, &path(), &forged, &body).await;

    assert_eq!(response.status, 401);
    let store = store(&paths);
    assert_eq!(
        store.recent_telecom_messages(ACCOUNT, 10).expect("recent"),
        Vec::new(),
        "an unverified body must not become a message"
    );
    assert_eq!(
        store.recent_calls(ACCOUNT, 10).expect("calls").len(),
        0,
        "nor a call"
    );
    // The body is not kept — it is attacker-supplied — but the fact that this
    // account is refusing callbacks is the only signal an operator whose
    // callback URL no longer matches will ever get.
    let rejections = store.callback_rejections(ACCOUNT).expect("rejections");
    assert_eq!(rejections.count, 1);
    assert!(rejections.last_reason.is_some());
}

#[tokio::test]
async fn a_callback_that_verifies_clears_the_rejections_before_it() {
    let (paths, _) = seeded(InboundCallPolicy::Answer);
    let carrier = carrier();
    let (_, body) = carrier.sign_inbound_sms("+15551234567", NUMBER, "hello");
    let forged = vec![("X-Mock-Signature".to_string(), "nope".to_string())];
    for _ in 0..3 {
        test_route::post(&paths, &path(), &forged, &body).await;
    }
    assert_eq!(store(&paths).callback_rejections(ACCOUNT).unwrap().count, 3);

    let (headers, body) = carrier.sign_inbound_sms("+15551234567", NUMBER, "hello again");
    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    assert_eq!(
        store(&paths).callback_rejections(ACCOUNT).unwrap().count,
        0,
        "the counter reads 'since it last worked', so a working callback resets it"
    );
}

#[tokio::test]
async fn a_ringing_line_is_answered_with_the_carrier_s_own_document() {
    let (paths, _) = seeded(InboundCallPolicy::Answer);
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_call("mock-call-1", "+15551234567");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 200);
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/xml; charset=utf-8"),
        "a carrier asking what to do with a ringing line needs a document, not an ack: {}",
        response.body
    );
    assert!(
        response.body.contains("wss://ops.example.test/v1/telecom/"),
        "the answer points the carrier at this call's media socket: {}",
        response.body
    );
    let calls = store(&paths).recent_calls(ACCOUNT, 10).expect("calls");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].direction, CallDirection::Inbound);
    assert_eq!(calls[0].peer_number, "+15551234567");
    assert!(
        calls[0].session_key.is_some(),
        "an answered call has the session its conversation runs in"
    );
}

#[tokio::test]
async fn a_number_set_to_reject_records_the_ring_and_answers_nothing() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_call("mock-call-2", "+15551234567");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202, "{}", response.body);
    assert_ne!(
        response.content_type.as_deref(),
        Some("text/xml; charset=utf-8"),
        "nothing connects a caller to a number set to refuse them"
    );
    let calls = store(&paths).recent_calls(ACCOUNT, 10).expect("calls");
    assert_eq!(calls.len(), 1, "the operator still sees that it rang");
    assert!(calls[0].session_key.is_none());
}

#[tokio::test]
async fn a_redelivered_ring_answers_again_without_ringing_twice() {
    let (paths, _) = seeded(InboundCallPolicy::Answer);
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_call("mock-call-3", "+15551234567");
    let first = test_route::post(&paths, &path(), &headers, &body).await;
    assert_eq!(first.status, 200);

    // Carriers retry an answer request they did not see a response to.
    let second = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(second.status, 200);
    assert_eq!(
        store(&paths)
            .recent_calls(ACCOUNT, 10)
            .expect("calls")
            .len(),
        1,
        "a redelivery must not become a second call against the concurrency limit"
    );
}

#[tokio::test]
async fn an_inbound_text_is_durable_before_the_carrier_is_answered() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    add_default_route(&paths);
    approve_sender(&paths, "+15551234567");
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_sms("+15551234567", NUMBER, "are you there");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    let messages = store(&paths)
        .recent_telecom_messages(ACCOUNT, 10)
        .expect("recent");
    let inbound = messages
        .iter()
        .find(|message| matches!(message.direction, CallDirection::Inbound))
        .expect("the text");
    assert_eq!(inbound.text, "are you there");
    assert_eq!(inbound.peer_number, "+15551234567");
    // Whatever the gate then decides, the message crossed the durable boundary
    // before the carrier heard anything — which is what makes the carrier's
    // acknowledgement safe to give. (What the queue does with it afterwards
    // depends on a running daemon, which is deliberately not part of the
    // request.)
    assert!(
        !inbound.state.is_empty(),
        "an acknowledged text has a disposition an operator can see"
    );
}

#[tokio::test]
async fn a_redelivered_text_is_acknowledged_once_and_kept_once() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    add_default_route(&paths);
    approve_sender(&paths, "+15551234567");
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_sms("+15551234567", NUMBER, "twice");

    let first = test_route::post(&paths, &path(), &headers, &body).await;
    let second = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!((first.status, second.status), (202, 202));
    let inbound = store(&paths)
        .recent_telecom_messages(ACCOUNT, 10)
        .expect("recent")
        .into_iter()
        .filter(|message| matches!(message.direction, CallDirection::Inbound))
        .count();
    assert_eq!(
        inbound, 1,
        "one text arrived, however many times it was posted"
    );
}

#[tokio::test]
async fn a_delivery_receipt_reaches_the_message_it_is_about() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    // A text this machine sent, as the outbox drain would have left it.
    stage_sent_text(&mut store(&paths), ACCOUNT, "on my way", "mock-msg-9");
    let carrier = carrier();
    let (headers, body) = carrier.sign_sms_status("mock-msg-9", false, Some("handset unreachable"));

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    let sent = store(&paths)
        .recent_telecom_messages(ACCOUNT, 10)
        .expect("recent")
        .into_iter()
        .find(|message| matches!(message.direction, CallDirection::Outbound))
        .expect("the text we sent");
    assert_eq!(sent.delivery_state.as_deref(), Some("undelivered"));
    assert_eq!(sent.error.as_deref(), Some("handset unreachable"));
    assert_eq!(
        sent.state, "sent",
        "the send still succeeded; a receipt must not push the row back into retries"
    );
}

/// A call this machine placed, in the state the dial leaves it: a durable row
/// carrying the id the carrier accepted, waiting to be picked up.
fn dialed_call(paths: &DaemonPaths, provider_call_id: &str) -> String {
    let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
    store(paths)
        .start_call(&super::telecom_store::TelecomCallRecord {
            call_id: call_id.clone(),
            account_id: ACCOUNT.to_string(),
            provider_call_id: Some(provider_call_id.to_string()),
            direction: CallDirection::Outbound,
            peer_number: "+15551234567".to_string(),
            state: super::telephony::CallState::Queued,
            session_key: Some("call:tel-wire:+15551234567".to_string()),
            job_id: None,
            idempotency_key: format!("outbound:tool:{provider_call_id}"),
            opening_line: Some("hello, this is a test".to_string()),
            last_error: None,
            started_at_ms: None,
            ended_at_ms: None,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        })
        .expect("dial recorded");
    call_id
}

#[tokio::test]
async fn a_call_we_placed_is_connected_when_the_far_end_picks_up() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    let call_id = dialed_call(&paths, "mock-call-out-1");
    let carrier = carrier();
    let (headers, body) = carrier.sign_outbound_answered("mock-call-out-1", "+15551234567");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    // Without this the phone rings, somebody says hello, and nothing is ever
    // connected to the audio: the carrier gets a bare acknowledgement instead
    // of the markup that opens the socket.
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(
        response.content_type.as_deref(),
        Some("text/xml; charset=utf-8"),
        "{}",
        response.body
    );
    assert!(
        response.body.contains(&format!("call={call_id}")),
        "the document points at this call's own socket: {}",
        response.body
    );
    let call = store(&paths)
        .telecom_call(&call_id)
        .expect("query")
        .expect("row");
    assert_eq!(call.state, super::telephony::CallState::InProgress);
    // An inbound policy of "reject" governs who may call *in*. It must not
    // refuse a call this machine placed and an operator approved.
    assert_eq!(call.direction, CallDirection::Outbound);
}

#[tokio::test]
async fn an_answer_for_a_call_nobody_placed_connects_nothing() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    let carrier = carrier();
    let (headers, body) = carrier.sign_outbound_answered("not-ours", "+15551234567");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    assert_ne!(
        response.content_type.as_deref(),
        Some("text/xml; charset=utf-8"),
        "a carrier must not be able to open a media socket by naming a call that does not exist"
    );
    assert_eq!(
        store(&paths)
            .recent_calls(ACCOUNT, 10)
            .expect("calls")
            .len(),
        0
    );
}

#[tokio::test]
async fn a_carrier_that_renames_a_call_when_it_goes_live_is_followed() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    // Plivo accepts a dial with a `RequestUUID` and identifies the live call by
    // `CallUUID`. The row holds the first.
    let call_id = dialed_call(&paths, "req-uuid-1");
    let carrier = carrier();
    let (headers, body) =
        carrier.sign_outbound_answered_with_request_id("live-call-uuid-1", "req-uuid-1");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 200, "{}", response.body);
    let call = store(&paths)
        .telecom_call(&call_id)
        .expect("query")
        .expect("row");
    assert_eq!(
        call.provider_call_id.as_deref(),
        Some("live-call-uuid-1"),
        "every later progress, hangup and reconciliation addresses the id the \
         carrier now uses; keeping the dial-time one loses the call"
    );
    assert!(store(&paths)
        .call_by_provider_id(ACCOUNT, "live-call-uuid-1")
        .expect("query")
        .is_some());
}

#[tokio::test]
async fn a_redelivered_hangup_still_closes_a_call_a_crash_left_open() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    let call_id = dialed_call(&paths, "mock-call-out-2");
    let carrier = carrier();
    let (headers, body) =
        carrier.sign_call_progress("mock-call-out-2", super::telephony::CallState::Completed);
    // The crash this repairs: the event row commits, then the process dies
    // before the call is closed. Recording it by hand is exactly that state.
    store(&paths)
        .record_telecom_event(
            ACCOUNT,
            "progress:mock-call-out-2:completed",
            "call_progress",
            None,
            "digest",
            NOW,
        )
        .expect("event row");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    let call = store(&paths)
        .telecom_call(&call_id)
        .expect("query")
        .expect("row");
    assert_eq!(
        call.state,
        super::telephony::CallState::Completed,
        "the carrier's retry is the only thing that can repair the gap; \
         answering it 'duplicate' without acting leaves the call open forever"
    );
}

#[tokio::test]
async fn a_redelivered_receipt_still_lands_on_its_message() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);
    stage_sent_text(&mut store(&paths), ACCOUNT, "on my way", "mock-msg-11");
    let carrier = carrier();
    let (headers, body) = carrier.sign_sms_status("mock-msg-11", false, Some("unreachable"));
    store(&paths)
        .record_telecom_event(
            ACCOUNT,
            "status:mock-msg-11:false",
            "sms_status",
            None,
            "digest",
            NOW,
        )
        .expect("event row");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 202);
    let sent = store(&paths)
        .recent_telecom_messages(ACCOUNT, 10)
        .expect("recent")
        .into_iter()
        .find(|message| matches!(message.direction, CallDirection::Outbound))
        .expect("the text we sent");
    assert_eq!(sent.delivery_state.as_deref(), Some("undelivered"));
}

#[tokio::test]
async fn a_disabled_number_is_not_reachable_at_all() {
    let (paths, mut record) = seeded(InboundCallPolicy::Answer);
    {
        let mut store = store(&paths);
        record.enabled = false;
        store.upsert_telecom_account(&record).expect("disable");
    }
    let carrier = carrier();
    let (headers, body) = carrier.sign_inbound_call("mock-call-4", "+15551234567");

    let response = test_route::post(&paths, &path(), &headers, &body).await;

    assert_eq!(response.status, 404);
    assert_eq!(
        store(&paths)
            .recent_calls(ACCOUNT, 10)
            .expect("calls")
            .len(),
        0
    );
}

#[tokio::test]
async fn the_sms_side_of_a_number_carries_the_messaging_defaults() {
    let (paths, _) = seeded(InboundCallPolicy::Reject);

    let account = store(&paths)
        .channel_account(ACCOUNT)
        .expect("query")
        .expect("the messaging half exists before the first text");

    assert_eq!(
        account.access_policy,
        ChannelAccessPolicy {
            direct: AccessPolicy::Pairing,
            group: AccessPolicy::Disabled,
            group_activation: GroupActivation::Disabled,
        },
        "a text from a stranger starts a pairing handshake, exactly as on every other channel"
    );
}

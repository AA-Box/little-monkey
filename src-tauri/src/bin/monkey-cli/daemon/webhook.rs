use std::convert::Infallible;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use super::ledger::SharedLedger;
use super::store::{DaemonPaths, DaemonStore};
use super::telecom_store::CallDirection;
use super::trigger::{
    ingest_signed_delivery, IngestOutcome, KeyringSecretStore, SignedDelivery, MAX_WEBHOOK_BYTES,
};

pub async fn spawn_local_listener(paths: DaemonPaths, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|error| format!("Failed to bind daemon webhook listener: {error}"))?;
    tokio::spawn(async move {
        loop {
            let Ok((stream, _address)) = listener.accept().await else {
                continue;
            };
            let io = TokioIo::new(stream);
            let paths = paths.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let paths = paths.clone();
                    async move { Ok::<_, Infallible>(handle(paths, request).await) }
                });
                let _ = http1::Builder::new().serve_connection(io, service).await;
            });
        }
    });
    Ok(())
}

async fn handle(paths: DaemonPaths, request: Request<Incoming>) -> Response<Full<Bytes>> {
    // A carrier media stream arrives as a GET that upgrades, so it is matched
    // before the method gates below — it is neither a provider handshake nor a
    // delivery.
    if let Some(account_id) = request
        .uri()
        .path()
        .strip_prefix("/v1/telecom/")
        .and_then(|rest| rest.strip_suffix("/media"))
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string)
    {
        return super::call_socket::handle_media_upgrade(paths, account_id, request).await;
    }
    let channel_account = request
        .uri()
        .path()
        .strip_prefix("/v1/channels/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string);
    // A GET reaches exactly one thing: the subscription handshake a provider
    // performs before it will save the callback URL at all. Everything else
    // stays POST-only.
    if request.method() == Method::GET {
        return match channel_account {
            Some(account_id) => {
                let query = request.uri().query().unwrap_or_default().to_string();
                handle_channel_verification(paths, account_id, &query)
            }
            None => response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed"),
        };
    }
    // An outbound MMS attachment: the carrier fetches it with the signed URL the
    // SMS adapter minted. Matched before the method gates for the same reason
    // the media socket is — it is a GET, and it is neither a handshake nor a
    // delivery.
    if let Some(account_id) = request
        .uri()
        .path()
        .strip_prefix("/v1/telecom/")
        .and_then(|rest| rest.strip_suffix("/file"))
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string)
    {
        return serve_signed_attachment(
            paths,
            account_id,
            request.uri().query().unwrap_or_default(),
        );
    }
    if request.method() != Method::POST {
        return response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    }
    if let Some(account_id) = channel_account {
        return handle_channel_delivery(paths, account_id, request).await;
    }
    // Two carrier paths, and the difference is the whole reply. `/v1/telecom/<id>`
    // is where a carrier asks what to do with a live call, and the answer is
    // the markup that connects it; `/v1/telecom/<id>/status` is where it
    // reports what became of a message or a call, and the answer is an
    // acknowledgement. Both are signed over the URL they arrived at, so the
    // path travels to the verifier rather than being assumed there.
    if let Some(rest) = request.uri().path().strip_prefix("/v1/telecom/") {
        let (account_id, on_status_path) = match rest.strip_suffix("/status") {
            Some(account_id) => (account_id, true),
            None => (rest, false),
        };
        if !account_id.is_empty() && !account_id.contains('/') {
            let account_id = account_id.to_string();
            let path = if on_status_path {
                super::telephony::status_callback_path(&account_id)
            } else {
                super::telephony::callback_path(&account_id)
            };
            return handle_carrier_callback(paths, account_id, path, request).await;
        }
    }
    let Some(trigger_id) = request
        .uri()
        .path()
        .strip_prefix("/v1/triggers/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string)
    else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let headers = request.headers();
    let delivery_id = header(headers, "x-little-monkey-delivery-id");
    let timestamp =
        header(headers, "x-little-monkey-timestamp-ms").and_then(|value| value.parse::<u64>().ok());
    let nonce = header(headers, "x-little-monkey-nonce");
    let signature = header(headers, "x-little-monkey-signature")
        .or_else(|| header(headers, "x-hub-signature-256"));
    let event = header(headers, "x-github-event");
    let (Some(delivery_id), Some(timestamp_ms), Some(nonce), Some(signature)) =
        (delivery_id, timestamp, nonce, signature)
    else {
        return response(StatusCode::BAD_REQUEST, "missing_signature_headers");
    };
    let collected = match Limited::new(request.into_body(), MAX_WEBHOOK_BYTES)
        .collect()
        .await
    {
        Ok(value) => value.to_bytes(),
        Err(_) => return response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let now_ms = match super::now_ms() {
        Ok(value) => value,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "clock_error"),
    };
    let outcome = (|| {
        let mut shared = SharedLedger::open(&paths.ledger_db)?;
        let mut state = DaemonStore::open(&paths)?;
        ingest_signed_delivery(
            &mut shared,
            &mut state,
            &KeyringSecretStore,
            &SignedDelivery {
                trigger_id: &trigger_id,
                delivery_id: &delivery_id,
                timestamp_ms,
                nonce: &nonce,
                signature: &signature,
                event_name: event.as_deref(),
                payload: &collected,
            },
            now_ms,
        )
    })();
    match outcome {
        Ok(IngestOutcome::Accepted) => response(StatusCode::ACCEPTED, "accepted"),
        Ok(IngestOutcome::Duplicate) => response(StatusCode::OK, "duplicate"),
        Ok(IngestOutcome::Rejected) => response(StatusCode::UNAUTHORIZED, "rejected"),
        Err(error) => json_error(StatusCode::BAD_REQUEST, &error),
    }
}

/// One delivery from a messaging provider that posts rather than being polled.
///
/// The signature is checked by the provider's own adapter, over the exact bytes
/// received, before anything is parsed or stored. Nothing here reads `Host` or
/// any `X-Forwarded-*` header: those are attacker-controlled, and a provider
/// whose signature covers its callback URL is given the operator's own
/// configured value from the account instead.
///
/// The listener still binds loopback only. Reaching it from the internet is the
/// operator's own tunnel or reverse proxy, which is the same posture the
/// existing trigger route has.
async fn handle_channel_delivery(
    paths: DaemonPaths,
    account_id: String,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    let body = match Limited::new(request.into_body(), MAX_WEBHOOK_BYTES)
        .collect()
        .await
    {
        Ok(value) => value.to_bytes(),
        Err(_) => return response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let now_ms = match super::now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    {
        Ok(value) => value,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "clock_error"),
    };

    let (mut store, adapter) = match open_webhook_adapter(&paths, &account_id) {
        Ok(pair) => pair,
        Err(refusal) => return *refusal,
    };

    // The operator's advertised base, for providers whose signatures cover
    // the full callback URL. Absent is fine: adapters that need nothing
    // ignore it, and one that requires it refuses its delivery itself.
    let public_base_url = store.channel_public_base_url().ok().flatten();
    let outcome = accept_webhook_delivery(
        &mut store,
        adapter.as_ref(),
        &WebhookDelivery {
            headers: &headers,
            body: &body,
            public_base_url: public_base_url.as_deref(),
            now_ms,
        },
    );
    outcome.into_response(adapter.ack())
}

/// One delivery as it reached the listener, before any of it is trusted.
pub(crate) struct WebhookDelivery<'a> {
    /// Lowercase-keyed, as [`WebhookChannelAdapter::verify_and_normalize`]
    /// requires.
    pub headers: &'a [(String, String)],
    /// The exact bytes received. Never re-serialized: every one of these
    /// providers signs the body it sent, not a normalization of it.
    pub body: &'a [u8],
    pub public_base_url: Option<&'a str>,
    pub now_ms: i64,
}

/// What one delivery did, before it becomes a status code.
///
/// Separate from the HTTP shell because this — not the header parsing around
/// it — is the contract with the provider: what may be acknowledged, and what
/// must be left for redelivery. Tests drive it with the production adapters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    /// Nothing proved the request came from the provider. No durable trace is
    /// left, deliberately: an unverified body has not earned a row.
    Rejected,
    /// Authenticated, and carrying no message — a status-only body, or an
    /// event type this provider maps to nothing.
    Nothing { receipts: usize },
    /// Every message in it crossed the durable acceptance boundary. Safe to
    /// acknowledge: a redelivery from here collapses onto the rows already
    /// committed.
    Accepted { accepted: u32, duplicates: u32 },
    /// At least one message left no durable trace. Must NOT be acknowledged —
    /// only the provider's redelivery can bring it back.
    NotAccepted,
}

impl DeliveryOutcome {
    fn into_response(self, ack: super::channel_adapter::WebhookAck) -> Response<Full<Bytes>> {
        match self {
            DeliveryOutcome::Rejected => response(StatusCode::UNAUTHORIZED, "rejected"),
            // A body that authenticated and carried nothing this provider maps
            // to a message is still finished with, so it gets the provider's
            // own success rather than a different status it would read as a
            // reason to send it again.
            DeliveryOutcome::Nothing { .. } | DeliveryOutcome::Accepted { .. } => ack_response(ack),
            DeliveryOutcome::NotAccepted => {
                response(StatusCode::INTERNAL_SERVER_ERROR, "not_accepted")
            }
        }
    }

    /// Whether the provider may be told this delivery is done with.
    ///
    /// The shipped path answers with [`Self::into_response`] instead; this is
    /// how the acknowledgement tests ask the same question without asserting on
    /// a status code, which is a detail of the HTTP shell rather than of what
    /// was durably accepted.
    #[cfg(test)]
    pub(crate) fn is_success(&self) -> bool {
        matches!(
            self,
            DeliveryOutcome::Nothing { .. } | DeliveryOutcome::Accepted { .. }
        )
    }
}

/// Answer with exactly what this provider asked for.
fn ack_response(ack: super::channel_adapter::WebhookAck) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(ack.status).unwrap_or(StatusCode::OK))
        .header(CONTENT_TYPE, ack.content_type)
        .body(Full::new(Bytes::from(ack.body)))
        .unwrap_or_else(|_| response(StatusCode::INTERNAL_SERVER_ERROR, "response_failed"))
}

/// Authenticate one delivery, then durably accept whatever it carries.
///
/// **This function is the whole of what happens before a provider is told
/// "yes".** The order is the point, and it is the same for all four:
///
/// 1. the provider's own adapter verifies the signature over the exact bytes
///    received — nothing is parsed before that, and a failure returns
///    [`DeliveryOutcome::Rejected`] having written nothing;
/// 2. the verified body is normalized into envelopes *and* whatever reply
///    addressing this provider requires, as one result: an adapter that
///    authenticated a message it could not produce an address for returns
///    `Err` here, which is refused exactly like a bad signature, because a
///    message nobody can answer must be redelivered rather than accepted;
/// 3. any reply address the delivery established is committed, because a
///    message that is acknowledged and then cannot be answered is worse than
///    one that is redelivered — see [`record_durable_addressing`];
/// 4. each envelope is committed to `channel_events`, whose
///    `UNIQUE(source, account_id, direction, provider_event_id)` is what makes
///    the provider's own event id the durable dedupe identity;
/// 5. only then may the caller answer with this provider's success.
///
/// What is deliberately *not* here: no file is downloaded, no media endpoint is
/// asked anything, no blob is written, no route is resolved, no execution
/// context is frozen, no run is submitted and no agent runs. Every one of those
/// can fail for reasons that have nothing to do with whether the provider's
/// message reached us, and a provider that is not acknowledged sends the
/// message again. They belong to
/// [`channel_worker::process_pending_channel_ingress`], which continues each
/// accepted row asynchronously and picks up wherever a restart left off.
///
/// The committed row is enough to finish from a cold start: it carries the
/// envelope, and the account it arrived on.
pub(crate) fn accept_webhook_delivery(
    store: &mut DaemonStore,
    adapter: &dyn super::channel_adapter::WebhookChannelAdapter,
    delivery: &WebhookDelivery<'_>,
) -> DeliveryOutcome {
    let verified = match adapter.verify_and_normalize(
        delivery.headers,
        delivery.body,
        delivery.public_base_url,
        delivery.now_ms,
    ) {
        Ok(verified) => verified,
        // Deliberately opaque, and deliberately not recorded. Covers both a
        // delivery that did not authenticate and one that did but could not
        // produce the addressing its own message requires.
        Err(_) => return DeliveryOutcome::Rejected,
    };
    let super::channel_adapter::VerifiedWebhookDelivery {
        envelopes,
        durable_addressing,
    } = verified;
    // Where this conversation's answer goes, before anything says it arrived.
    // A provider that saw success for a message whose only reply address was
    // lost is owed an answer nothing can ever send, and it will not redeliver.
    if !record_durable_addressing(store, durable_addressing, delivery.now_ms) {
        return DeliveryOutcome::NotAccepted;
    }
    // What the provider says happened to messages we already sent. Recorded
    // before the inbound work because it is cheap and must survive even a
    // delivery that carries nothing else — a status-only body is the normal
    // shape of a failure report.
    let receipts = record_delivery_receipts(
        store,
        adapter.delivery_receipts(delivery.body, delivery.now_ms),
    );

    if envelopes.is_empty() {
        return DeliveryOutcome::Nothing { receipts };
    }

    let mut accepted = 0;
    let mut duplicates = 0;
    for envelope in &envelopes {
        match record_accepted_event(store, envelope) {
            Ok(super::channel_store::EventRecording::Recorded { .. }) => accepted += 1,
            Ok(super::channel_store::EventRecording::Duplicate { .. }) => duplicates += 1,
            // Nothing was committed for this message, so the provider must be
            // left to redeliver the whole thing. The siblings that did commit
            // collapse when it arrives.
            Err(_) => return DeliveryOutcome::NotAccepted,
        }
    }
    DeliveryOutcome::Accepted {
        accepted,
        duplicates,
    }
}

/// Commit the reply addresses a verified delivery established, before the
/// event that will need them.
///
/// Order is the whole of the crash-safety argument, and it is the reverse of
/// the intuitive one. Address first, event second: a crash in between leaves an
/// address for a conversation with no event, which costs nothing — the provider
/// never saw success, so it redelivers, and the address is simply already
/// correct when it does. The other order leaves the state that cannot be
/// repaired: an accepted message, acknowledged, with nowhere to answer.
///
/// `false` means one of them did not commit, and the caller must withhold the
/// acknowledgement so the provider sends the whole delivery again.
fn record_durable_addressing(
    store: &mut DaemonStore,
    addressing: Vec<super::channel_adapter::DurableAddressing>,
    now_ms: i64,
) -> bool {
    if !addressing.is_empty()
        && super::fail_points::fire(super::fail_points::FailPoint::BeforeAddressingCommit).is_err()
    {
        return false;
    }
    addressing.into_iter().all(|entry| {
        store
            .set_channel_conversation_ref(
                &entry.account_id,
                &entry.conversation_id,
                &entry.reference,
                now_ms.max(1),
            )
            .is_ok()
    })
}

/// Commit one authenticated envelope as accepted-and-unprocessed.
///
/// `accepted` with no turn behind it is exactly the state
/// `DaemonStore::accepted_events_awaiting_processing` selects, and the worker
/// makes the real decision — run, ignore, challenge or refuse — from the
/// envelope stored here. Nothing about the sender's access, the route or the
/// recipe is consulted at this point: those are reads of operator
/// configuration, and none of them change whether this message arrived.
fn record_accepted_event(
    store: &mut DaemonStore,
    envelope: &little_monkey_lib::channels::types::ChannelEnvelope,
) -> Result<super::channel_store::EventRecording, String> {
    use super::channel_store::{EventDirection, EventDisposition, NewChannelEvent};

    store.record_channel_event(&NewChannelEvent {
        account_id: envelope.account_id.clone(),
        source: little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
        direction: EventDirection::Inbound,
        provider_event_id: envelope.provider_event_id.clone(),
        conversation_id: envelope.conversation.conversation_id.clone(),
        thread_id: envelope.conversation.thread_id.clone(),
        sender_id: Some(envelope.sender.sender_id.clone()),
        envelope_json: serde_json::to_string(envelope).map_err(|error| error.to_string())?,
        disposition: EventDisposition::Accepted,
        received_at_ms: envelope.received_at_ms.max(1),
    })
}

/// An open store plus the adapter for the account the request named.
type OpenedWebhookAccount = (
    DaemonStore,
    Box<dyn super::channel_adapter::WebhookChannelAdapter>,
);

/// Open the store and build one account's webhook adapter, or the response
/// that says why not.
///
/// An unknown account, a disabled one, and a provider that is not delivered to
/// all answer the same flat 404: a stranger probing the endpoint learns
/// nothing about what exists.
fn open_webhook_adapter(
    paths: &DaemonPaths,
    account_id: &str,
) -> Result<OpenedWebhookAccount, Box<Response<Full<Bytes>>>> {
    let refuse = |status, text| Box::new(response(status, text));
    let store = DaemonStore::open(paths)
        .map_err(|_| refuse(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"))?;
    let account = match store.channel_account(account_id) {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => return Err(refuse(StatusCode::NOT_FOUND, "not_found")),
        Err(_) => {
            return Err(refuse(
                StatusCode::INTERNAL_SERVER_ERROR,
                "state_unavailable",
            ))
        }
    };
    let secret = match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    let config = super::channel_adapter::AdapterConfig {
        account: &account,
        secret,
    };
    let adapter = super::adapters::build_webhook_adapter(&config, Some(paths))
        .map_err(|_| refuse(StatusCode::NOT_FOUND, "not_found"))?;
    Ok((store, adapter))
}

/// Record what a provider says happened to messages we already sent.
///
/// Each transition is its own row, keyed `status:<message id>:<state>`, for
/// two reasons: the send itself already owns the row keyed on the bare message
/// id, and a provider that redelivers the same status must collapse onto the
/// row it wrote the first time rather than adding another. Returns how many
/// were new.
///
/// A receipt is deliberately not a turn. Nobody is speaking, so nothing is
/// queued and nothing runs — the operator sees it in the account's activity
/// list, which is where a message that quietly failed to arrive becomes
/// visible.
fn record_delivery_receipts(
    store: &mut DaemonStore,
    receipts: Vec<little_monkey_lib::channels::types::DeliveryReceipt>,
) -> usize {
    use super::channel_store::{EventDirection, EventDisposition, NewChannelEvent};
    use little_monkey_lib::channels::types::DeliveryState;

    let mut recorded = 0;
    for receipt in receipts {
        let disposition = match receipt.state {
            DeliveryState::Failed => EventDisposition::Failed,
            _ => EventDisposition::Accepted,
        };
        let envelope_json = serde_json::to_string(&receipt).unwrap_or_default();
        let outcome = store.record_channel_event(&NewChannelEvent {
            account_id: receipt.account_id.clone(),
            source: little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
            direction: EventDirection::Outbound,
            provider_event_id: format!(
                "status:{}:{}",
                receipt.provider_message_id,
                receipt.state.as_str()
            ),
            // A receipt names a message, not a conversation: the provider does
            // not repeat which thread it was in, and inventing one would put
            // the row under a conversation that may not exist.
            conversation_id: format!("message:{}", receipt.provider_message_id),
            thread_id: None,
            sender_id: None,
            envelope_json,
            disposition,
            received_at_ms: receipt.observed_at_ms,
        });
        if matches!(
            outcome,
            Ok(super::channel_store::EventRecording::Recorded { .. })
        ) {
            recorded += 1;
        }
    }
    recorded
}

/// The subscription handshake a provider performs before it will accept the
/// callback URL.
///
/// The adapter answers, because only it knows what its provider asks and what
/// shared secret proves the asker is that provider. A refusal is a flat 403
/// with no detail — the alternative tells whoever is probing which half of the
/// handshake they got right.
///
/// The challenge is echoed as `text/plain`, which is what Meta requires: it
/// compares the body byte for byte and a JSON wrapper fails the comparison.
fn handle_channel_verification(
    paths: DaemonPaths,
    account_id: String,
    query: &str,
) -> Response<Full<Bytes>> {
    let (_store, adapter) = match open_webhook_adapter(&paths, &account_id) {
        Ok(pair) => pair,
        Err(refusal) => return *refusal,
    };
    match adapter.verification_challenge(query) {
        Some(challenge) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Full::new(Bytes::from(challenge)))
            .expect("challenge response is valid"),
        None => response(StatusCode::FORBIDDEN, "rejected"),
    }
}

/// One callback from a carrier.
///
/// Same posture as the messaging route: the carrier's own provider verifies
/// the signature over the exact bytes received before anything is parsed, an
/// unverified body is answered opaquely and recorded nowhere, and an unknown
/// account is a flat 404 so probing reveals nothing.
///
/// The event is deduplicated before it is acted on, because carriers retry:
/// a redelivered "call answered" must not answer a second time, and a
/// redelivered text must not run twice.
async fn handle_carrier_callback(
    paths: DaemonPaths,
    account_id: String,
    path: String,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_string()))
        })
        .collect();
    let body = match Limited::new(request.into_body(), MAX_WEBHOOK_BYTES)
        .collect()
        .await
    {
        Ok(value) => value.to_bytes(),
        Err(_) => return response(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large"),
    };
    let now_ms = match super::now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    {
        Ok(value) => value,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "clock_error"),
    };

    let mut store = match DaemonStore::open(&paths) {
        Ok(store) => store,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"),
    };
    let account = match store.telecom_account(&account_id) {
        Ok(Some(account)) if account.enabled => account,
        Ok(_) => return response(StatusCode::NOT_FOUND, "not_found"),
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"),
    };
    let secret = match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    let provider = match super::telephony::provider_for_account(&account, secret.clone()) {
        Ok(provider) => provider,
        Err(_) => return response(StatusCode::NOT_FOUND, "not_found"),
    };

    let event = match provider.verify_webhook(&path, &headers, &body, now_ms) {
        Ok(event) => {
            // It verified, so whatever was wrong before is fixed. The counter
            // reads "since it last worked", which is the only form of it an
            // operator can act on.
            let _ = store.clear_callback_rejections(&account.account_id);
            event
        }
        Err(reason) => {
            // The body earns no durable row — it is unauthenticated — but the
            // fact that this account is refusing callbacks does, because a
            // carrier posting to a URL that never verifies is invisible
            // otherwise. Only the verifier's own reason is kept; see
            // `record_callback_rejection`.
            let _ = store.record_callback_rejection(&account.account_id, &reason, now_ms);
            return response(StatusCode::UNAUTHORIZED, "rejected");
        }
    };

    let digest = super::trigger::sha256_hex(&body);
    let provider_event_id = carrier_event_id(&event, &digest);
    // This row records that the callback was *seen*. It is never proof that
    // what the callback asked for happened: the effect is committed after it,
    // and a crash in between would otherwise turn the carrier's retry — the
    // only thing that can repair the gap — into an immediate "duplicate" and
    // lose the call, the receipt or the message for good.
    //
    // So a duplicate is handled again rather than short-circuited. Every effect
    // below is idempotent by its own identity: a call row deduplicates on its
    // idempotency key, `advance_call` ignores a transition it has already made
    // or has moved past, a delivery receipt is an overwrite of the same
    // columns, and a message deduplicates on its provider event id.
    if store
        .record_telecom_event(
            &account.account_id,
            &provider_event_id,
            carrier_event_kind(&event),
            None,
            &digest,
            now_ms,
        )
        .is_err()
    {
        return response(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable");
    }

    let queue = super::DaemonChannelQueue::new(paths.clone());
    match super::telecom_worker::handle_carrier_event(&mut store, &queue, &account, event, now_ms) {
        // An answered call is the one case where the carrier needs more than an
        // acknowledgement: it is asking what to do with the line, and the answer
        // is "stream the audio here". A call the policy or the concurrency limit
        // refused gets the plain acknowledgement, which leaves the carrier to
        // its own no-answer handling rather than connecting anything.
        Ok(super::telecom_worker::CarrierOutcome::Call {
            call_id,
            answered: true,
        }) => match media_socket_url(&provider, &account, &secret, &call_id, now_ms) {
            None => response(StatusCode::ACCEPTED, "accepted"),
            Some(media_url) => match provider.answer_instructions(&media_url) {
                // A carrier that answers with markup reads this response body.
                Some(document) => xml_response(document),
                // A carrier driven by commands (Telnyx) ignores it: the call is
                // answered, or the stream started on it, by a REST call made
                // here. A failure is reported as a failure rather than as a
                // cheerful acknowledgement of a call nobody can hear.
                None => {
                    let answered_already = matches!(
                        store.telecom_call(&call_id),
                        Ok(Some(ref call)) if call.direction == CallDirection::Outbound
                    );
                    match provider
                        .connect_media(
                            &provider_call_id_for(&store, &call_id),
                            &media_url,
                            answered_already,
                            account.limits.recording_enabled,
                        )
                        .await
                    {
                        Ok(()) => response(StatusCode::OK, "accepted"),
                        Err(_) => response(StatusCode::INTERNAL_SERVER_ERROR, "not_connected"),
                    }
                }
            },
        },
        Ok(_) => response(StatusCode::ACCEPTED, "accepted"),
        Err(_) => response(StatusCode::INTERNAL_SERVER_ERROR, "not_handled"),
    }
}

/// How long a media-stream token is good for.
///
/// Long enough for a carrier to answer and dial the socket, short enough that a
/// URL captured from a log is worthless by the time anyone reads it. The token
/// is bound to one call as well, so this only bounds the window on that call.
const MEDIA_TOKEN_TTL_MS: i64 = 120_000;

/// Build the carrier's answer document, pointing at this call's media socket.
///
/// `None` when the account has no public URL configured or the carrier has no
/// media stream: without both there is nowhere for the audio to go, and a
/// document that connects a caller to silence is worse than not answering.
/// The carrier's own id for a call this daemon knows, for a command addressed
/// to it. Empty when the row has none, which a command-driven carrier reports
/// as a failure rather than acting on.
fn provider_call_id_for(store: &DaemonStore, call_id: &str) -> String {
    store
        .telecom_call(call_id)
        .ok()
        .flatten()
        .and_then(|call| call.provider_call_id)
        .unwrap_or_default()
}

fn media_socket_url(
    provider: &std::sync::Arc<dyn super::telephony::TelecomProvider>,
    account: &super::telecom_store::TelecomAccountRecord,
    secret: &str,
    call_id: &str,
    now_ms: i64,
) -> Option<String> {
    provider.media_stream()?;
    let base = account.public_base_url.as_deref()?.trim_end_matches('/');
    let socket_base = base
        .strip_prefix("https://")
        .map(|rest| format!("wss://{rest}"))
        .or_else(|| {
            base.strip_prefix("http://")
                .map(|rest| format!("ws://{rest}"))
        })?;
    let expires_at_ms = now_ms + MEDIA_TOKEN_TTL_MS;
    let token =
        super::telephony::media_stream_token(secret, &account.account_id, call_id, expires_at_ms);
    Some(format!(
        "{socket_base}/v1/telecom/{}/media?call={call_id}&exp={expires_at_ms}&sig={token}",
        account.account_id
    ))
}

fn xml_response(document: super::telephony::AnswerDocument) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, document.content_type)
        .body(Full::new(Bytes::from(document.body)))
        .unwrap_or_else(|_| response(StatusCode::INTERNAL_SERVER_ERROR, "response_failed"))
}

/// The carrier's own identifier for this event, which is what dedupe hangs on.
/// A carrier that supplies none falls back to the body digest — deterministic,
/// so a redelivery of the identical body still collapses.
fn carrier_event_id(event: &super::telephony::TelecomEvent, digest: &str) -> String {
    use super::telephony::TelecomEvent;
    match event {
        TelecomEvent::InboundSms(envelope) => format!("sms:{}", envelope.provider_event_id),
        TelecomEvent::AnswerRequest {
            provider_call_id,
            direction,
            ..
        } => format!("answer:{}:{provider_call_id}", direction.as_str()),
        TelecomEvent::CallProgress {
            provider_call_id,
            state,
            ..
        } => format!("progress:{provider_call_id}:{}", state.as_str()),
        TelecomEvent::SmsStatus {
            provider_message_id,
            delivered,
            ..
        } => format!("status:{provider_message_id}:{delivered}"),
        TelecomEvent::Ignored => format!("other:{digest}"),
    }
}

fn carrier_event_kind(event: &super::telephony::TelecomEvent) -> &'static str {
    use super::telephony::TelecomEvent;
    match event {
        TelecomEvent::InboundSms(_) => "inbound_sms",
        TelecomEvent::AnswerRequest { .. } => "answer_request",
        TelecomEvent::CallProgress { .. } => "call_progress",
        TelecomEvent::SmsStatus { .. } => "sms_status",
        TelecomEvent::Ignored => "ignored",
    }
}

fn header(headers: &hyper::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn response(status: StatusCode, status_text: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "status": status_text }).to_string();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("static webhook response is valid")
}

fn json_error(status: StatusCode, message: &str) -> Response<Full<Bytes>> {
    let body = serde_json::json!({ "status": "error", "message": message }).to_string();
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .expect("static webhook error response is valid")
}

/// Drive the production HTTP route the way a provider does.
///
/// The service under test is [`handle`] itself, served by the same hyper
/// `http1` server the listener uses, over an in-memory duplex instead of a
/// socket. Real request bytes are parsed by hyper, the real router picks the
/// path, the real adapter verifies, and the real status line and headers come
/// back — which is the only way to assert on what a provider is actually told.
/// Binding a TCP port per test would prove nothing more and would cost every
/// test in the binary on a loaded CI machine.
#[cfg(test)]
pub(crate) mod test_route {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// What the route answered, as the provider would see it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct RouteResponse {
        pub status: u16,
        pub content_type: Option<String>,
        pub body: String,
    }

    /// One POST through the production route.
    pub(crate) async fn post(
        paths: &DaemonPaths,
        path: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> RouteResponse {
        let mut raw = format!(
            "POST {path} HTTP/1.1\r\nhost: monkey.test\r\ncontent-length: {}\r\nconnection: close\r\n",
            body.len()
        )
        .into_bytes();
        for (name, value) in headers {
            raw.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        raw.extend_from_slice(b"\r\n");
        raw.extend_from_slice(body);
        send_raw(paths, &raw).await
    }

    /// One GET through the production route, for a provider handshake.
    pub(crate) async fn get(paths: &DaemonPaths, path_and_query: &str) -> RouteResponse {
        let raw = format!(
            "GET {path_and_query} HTTP/1.1\r\nhost: monkey.test\r\nconnection: close\r\n\r\n"
        )
        .into_bytes();
        send_raw(paths, &raw).await
    }

    async fn send_raw(paths: &DaemonPaths, raw: &[u8]) -> RouteResponse {
        let (mut client, server) = tokio::io::duplex(1024 * 1024);
        let served = paths.clone();
        let connection = tokio::spawn(async move {
            let service = service_fn(move |request| {
                let paths = served.clone();
                async move { Ok::<_, Infallible>(handle(paths, request).await) }
            });
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(server), service)
                .await;
        });
        client.write_all(raw).await.expect("write request");
        client.flush().await.expect("flush request");
        let mut received = Vec::new();
        client
            .read_to_end(&mut received)
            .await
            .expect("read response");
        let _ = connection.await;
        parse(&received)
    }

    fn parse(raw: &[u8]) -> RouteResponse {
        let text = String::from_utf8_lossy(raw).to_string();
        let (head, body) = text
            .split_once("\r\n\r\n")
            .unwrap_or_else(|| panic!("not an HTTP response: {text:?}"));
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|code| code.parse::<u16>().ok())
            .unwrap_or_else(|| panic!("no status line in {head:?}"));
        let content_type = lines
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.trim().to_string());
        RouteResponse {
            status,
            content_type,
            body: body.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::types::{DeliveryReceipt, DeliveryState};

    /// A store holding the account the receipts belong to — in production the
    /// route has already looked it up before any of this runs.
    fn seeded_store() -> DaemonStore {
        use little_monkey_lib::channels::policy::ChannelAccessPolicy;
        use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind, HealthState};

        let mut store = DaemonStore::open_in_memory().expect("open store");
        store
            .upsert_channel_account(&super::super::channel_store::ChannelAccountRecord {
                account_id: "acct-wa".to_string(),
                kind: ChannelKind::WhatsApp,
                label: "Test WhatsApp".to_string(),
                enabled: true,
                non_secret_config: serde_json::json!({ "phone_number_id": "1234567890" }),
                credential_ref: Some("wa-cred".to_string()),
                access_policy: ChannelAccessPolicy::default(),
                health: ChannelHealth {
                    state: HealthState::Unconfigured,
                    detail: None,
                    last_error: None,
                    probed_at_ms: 0,
                },
                created_at_ms: 1_000,
                updated_at_ms: 1_000,
            })
            .expect("seed account");
        store
    }

    fn receipt(state: DeliveryState) -> DeliveryReceipt {
        DeliveryReceipt {
            account_id: "acct-wa".to_string(),
            provider_message_id: "wamid.OUT1".to_string(),
            state,
            error: None,
            observed_at_ms: 1_700_000_000,
        }
    }

    #[test]
    fn a_redelivered_status_is_recorded_once_and_each_transition_separately() {
        let mut store = seeded_store();
        assert_eq!(
            record_delivery_receipts(&mut store, vec![receipt(DeliveryState::Sent)]),
            1
        );
        // Providers retry. The same status arriving twice must not become two
        // rows in the operator's activity list.
        assert_eq!(
            record_delivery_receipts(&mut store, vec![receipt(DeliveryState::Sent)]),
            0
        );
        // A later transition on the same message is genuinely new.
        assert_eq!(
            record_delivery_receipts(&mut store, vec![receipt(DeliveryState::Delivered)]),
            1
        );
    }

    #[test]
    fn route_rejects_nested_trigger_paths() {
        let path = "/v1/triggers/a/b";
        assert!(path
            .strip_prefix("/v1/triggers/")
            .filter(|value| !value.contains('/'))
            .is_none());
    }
}

/// The largest attachment served to a carrier. The per-type caps in
/// `adapters::sms` are the real limit; this only bounds what is read from disk
/// before those are applied.
const MAX_ATTACHMENT_BYTES: u64 = 5 * 1024 * 1024;

/// Hand a carrier one attachment, if it presents a valid signed URL.
///
/// This is the only path by which a stored artifact leaves this machine
/// unauthenticated, so it is narrow on purpose: one artifact named in the
/// signature, an expiry, the account's own credential as the key, and no
/// directory, listing or range semantics. Every refusal is the same "not
/// found": the endpoint is public, and a specific error is a hint.
fn serve_signed_attachment(
    paths: DaemonPaths,
    account_id: String,
    query: &str,
) -> Response<Full<Bytes>> {
    let params: std::collections::BTreeMap<&str, &str> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .collect();
    let (Some(artifact_id), Some(expires_at_ms), Some(token)) = (
        params.get("artifact").copied(),
        params
            .get("exp")
            .and_then(|value| value.parse::<i64>().ok()),
        params.get("sig").copied(),
    ) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let Ok(now_ms) = super::now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let Ok(store) = DaemonStore::open(&paths) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let Ok(Some(account)) = store.telecom_account(&account_id) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    if !account.enabled {
        return response(StatusCode::NOT_FOUND, "not_found");
    }
    let secret = match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    if super::telephony::verify_media_file_token(
        &secret,
        &account_id,
        artifact_id,
        expires_at_ms,
        token,
        now_ms,
    )
    .is_err()
    {
        return response(StatusCode::NOT_FOUND, "not_found");
    }
    let Some(app_data) = paths.root.parent() else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let Ok(artifacts) = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
        app_data.join("content-v1"),
        MAX_ATTACHMENT_BYTES,
    ) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    let Ok(bytes) = artifacts.read(artifact_id) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    // The type is read from the bytes themselves rather than from anything the
    // request or the artifact metadata claims. A carrier cannot deliver an
    // attachment it cannot identify, and this process will not name a type it
    // has not looked at.
    let Some(media_type) = super::adapters::sms::sniff_media_type(&bytes) else {
        return response(StatusCode::NOT_FOUND, "not_found");
    };
    if super::adapters::sms::media_limit(media_type).is_none_or(|limit| bytes.len() > limit) {
        return response(StatusCode::NOT_FOUND, "not_found");
    }
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, media_type)
        // A carrier fetches this once; nothing downstream should keep it.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(bytes)))
        .unwrap_or_else(|_| response(StatusCode::NOT_FOUND, "not_found"))
}

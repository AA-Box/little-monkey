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
    if let Some(account_id) = request
        .uri()
        .path()
        .strip_prefix("/v1/telecom/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string)
    {
        return handle_carrier_callback(paths, account_id, request).await;
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

    let (mut store, adapter, fetcher) = match open_webhook_adapter(&paths, &account_id) {
        Ok(pair) => pair,
        Err(refusal) => return *refusal,
    };
    // What this account allows an attachment to cost, which the operator may
    // have tuned. Read once per delivery rather than per file.
    let limits = store
        .channel_account(&account_id)
        .ok()
        .flatten()
        .map(|account| {
            super::channel_adapter::AttachmentLimits::for_account(&account.non_secret_config)
        })
        .unwrap_or_default();

    // The operator's advertised base, for providers whose signatures cover
    // the full callback URL. Absent is fine: adapters that need nothing
    // ignore it, and one that requires it refuses its delivery itself.
    let public_base_url = store.channel_public_base_url().ok().flatten();
    let mut envelopes =
        match adapter.verify_and_normalize(&headers, &body, public_base_url.as_deref(), now_ms) {
            Ok(envelopes) => envelopes,
            // Deliberately opaque, and deliberately not recorded: an unverified
            // body has not earned a row in the durable event log.
            Err(_) => return response(StatusCode::UNAUTHORIZED, "rejected"),
        };
    // What the provider says happened to messages we already sent. Recorded
    // before the inbound work because it is cheap and must survive even a
    // delivery that carries nothing else — a status-only body is the normal
    // shape of a failure report.
    let receipts = record_delivery_receipts(&mut store, adapter.delivery_receipts(&body, now_ms));

    if !envelopes.is_empty() {
        // Same as the polled path: the bytes are fetched before the turn is
        // durable, so what is stored is what the agent will be shown.
        if let Some(fetcher) = fetcher.as_deref() {
            super::channel_adapter::hydrate_attachments(
                fetcher,
                &super::channel_adapter::DaemonBlobs,
                limits,
                &mut envelopes,
            )
            .await;
        }
    }

    if envelopes.is_empty() {
        return response(
            StatusCode::OK,
            if receipts > 0 { "recorded" } else { "ignored" },
        );
    }

    let queue = super::DaemonChannelQueue::new(paths.clone());
    let report = super::channel_worker::ingest_batch(&mut store, &queue, &envelopes, now_ms);
    if report.failed > 0 && report.accepted == 0 {
        return response(StatusCode::INTERNAL_SERVER_ERROR, "not_queued");
    }
    response(StatusCode::ACCEPTED, "accepted")
}

/// An open store plus the adapter for the account the request named.
type OpenedWebhookAccount = (
    DaemonStore,
    Box<dyn super::channel_adapter::WebhookChannelAdapter>,
    // The same provider as a polling adapter, when it is also one. Only used
    // to download attachments — the two halves are the same struct for every
    // provider that is delivered to.
    Option<std::sync::Arc<dyn super::channel_adapter::ChannelAdapter>>,
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
    let adapter = super::adapters::build_webhook_adapter(&config)
        .map_err(|_| refuse(StatusCode::NOT_FOUND, "not_found"))?;
    let fetcher = super::adapters::build_adapter(&config).ok();
    Ok((store, adapter, fetcher))
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
    let (_store, adapter, _fetcher) = match open_webhook_adapter(&paths, &account_id) {
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

    let event = match provider.verify_webhook(&headers, &body, now_ms) {
        Ok(event) => event,
        Err(_) => return response(StatusCode::UNAUTHORIZED, "rejected"),
    };

    let digest = super::trigger::sha256_hex(&body);
    let provider_event_id = carrier_event_id(&event, &digest);
    match store.record_telecom_event(
        &account.account_id,
        &provider_event_id,
        carrier_event_kind(&event),
        None,
        &digest,
        now_ms,
    ) {
        Ok(super::telecom_store::TelecomEventRecording::Duplicate { .. }) => {
            return response(StatusCode::OK, "duplicate")
        }
        Ok(super::telecom_store::TelecomEventRecording::Recorded { .. }) => {}
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"),
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
        }) => match answer_document(&provider, &account, &secret, &call_id, now_ms) {
            Some(document) => xml_response(document),
            None => response(StatusCode::ACCEPTED, "accepted"),
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
fn answer_document(
    provider: &std::sync::Arc<dyn super::telephony::TelecomProvider>,
    account: &super::telecom_store::TelecomAccountRecord,
    secret: &str,
    call_id: &str,
    now_ms: i64,
) -> Option<super::telephony::AnswerDocument> {
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
    let url = format!(
        "{socket_base}/v1/telecom/{}/media?call={call_id}&exp={expires_at_ms}&sig={token}",
        account.account_id
    );
    provider.answer_instructions(&url)
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
        TelecomEvent::InboundCall {
            provider_call_id, ..
        } => format!("call:{provider_call_id}"),
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
        TelecomEvent::InboundCall { .. } => "inbound_call",
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

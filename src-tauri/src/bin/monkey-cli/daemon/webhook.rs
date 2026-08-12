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
    if request.method() != Method::POST {
        return response(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    }
    if let Some(account_id) = request
        .uri()
        .path()
        .strip_prefix("/v1/channels/")
        .filter(|value| !value.is_empty() && !value.contains('/'))
        .map(str::to_string)
    {
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

    let mut store = match DaemonStore::open(&paths) {
        Ok(store) => store,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "state_unavailable"),
    };
    let account = match store.channel_account(&account_id) {
        Ok(Some(account)) if account.enabled => account,
        // An unknown or disabled account is a 404 rather than an explanation:
        // a stranger probing the endpoint learns nothing about what exists.
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
    let config = super::channel_adapter::AdapterConfig {
        account: &account,
        secret,
    };
    let adapter = match super::adapters::build_webhook_adapter(&config) {
        Ok(adapter) => adapter,
        Err(_) => return response(StatusCode::NOT_FOUND, "not_found"),
    };

    let envelopes = match adapter.verify_and_normalize(&headers, &body, None, now_ms) {
        Ok(envelopes) => envelopes,
        // Deliberately opaque, and deliberately not recorded: an unverified
        // body has not earned a row in the durable event log.
        Err(_) => return response(StatusCode::UNAUTHORIZED, "rejected"),
    };
    if envelopes.is_empty() {
        return response(StatusCode::OK, "ignored");
    }

    let queue = super::DaemonChannelQueue::new(paths.clone());
    let report = super::channel_worker::ingest_batch(&mut store, &queue, &envelopes, now_ms);
    if report.failed > 0 && report.accepted == 0 {
        return response(StatusCode::INTERNAL_SERVER_ERROR, "not_queued");
    }
    response(StatusCode::ACCEPTED, "accepted")
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
    let provider = match super::telephony::build_provider(super::telephony::TelecomConfig {
        account_id: account.account_id.clone(),
        kind: account.kind,
        carrier_account_id: account.carrier_account_id.clone(),
        from_number: account.from_number.clone(),
        secret,
        public_base_url: account.public_base_url.clone(),
        webhook_public_key: account
            .non_secret_config
            .get("webhook_public_key")
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }) {
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
        Ok(_) => response(StatusCode::ACCEPTED, "accepted"),
        Err(_) => response(StatusCode::INTERNAL_SERVER_ERROR, "not_handled"),
    }
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
    #[test]
    fn route_rejects_nested_trigger_paths() {
        let path = "/v1/triggers/a/b";
        assert!(path
            .strip_prefix("/v1/triggers/")
            .filter(|value| !value.contains('/'))
            .is_none());
    }
}

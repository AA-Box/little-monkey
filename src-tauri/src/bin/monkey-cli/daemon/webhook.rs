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

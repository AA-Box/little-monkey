//! The carrier media socket: everything between "a carrier is connecting" and
//! "a conversation is running".
//!
//! Kept apart from [`super::call_media`], which is the conversation itself and
//! has no idea what a socket is, and from [`super::webhook`], which is HTTP.
//! What lives here is the upgrade, the token check, and the one place a live
//! call is looked up and its pieces assembled.

use std::sync::Mutex;

use async_trait::async_trait;
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{CONNECTION, SEC_WEBSOCKET_ACCEPT, SEC_WEBSOCKET_KEY, UPGRADE};
use hyper::{Request, Response, StatusCode};
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;

use super::call_media::{
    run_media_session, CallIdentity, ConfiguredSpeech, MediaSocket, QueuedCallTurns,
};
use super::store::{DaemonPaths, DaemonStore};

/// Verify a carrier's media connection and, if it checks out, hand the socket to
/// the conversation loop.
///
/// Every refusal here is deliberately silent about why: this endpoint is
/// reachable by whoever can reach the operator's public URL, and a helpful
/// error message is a hint about how to get in.
pub(crate) async fn handle_media_upgrade(
    paths: DaemonPaths,
    account_id: String,
    request: Request<Incoming>,
) -> Response<Full<Bytes>> {
    let query = request.uri().query().unwrap_or_default().to_string();
    let params = query_params(&query);
    let (Some(call_id), Some(expires_at_ms), Some(token)) = (
        params.get("call").cloned(),
        params
            .get("exp")
            .and_then(|value| value.parse::<i64>().ok()),
        params.get("sig").cloned(),
    ) else {
        return refuse();
    };
    let Ok(now_ms) = super::now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    else {
        return refuse();
    };

    let Ok(store) = DaemonStore::open(&paths) else {
        return refuse();
    };
    let Ok(Some(account)) = store.telecom_account(&account_id) else {
        return refuse();
    };
    if !account.enabled {
        return refuse();
    }
    let secret = match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    };
    if super::telephony::verify_media_stream_token(
        &secret,
        &account_id,
        &call_id,
        expires_at_ms,
        &token,
        now_ms,
    )
    .is_err()
    {
        return refuse();
    }
    // The call has to be one this machine knows about and still has open. A
    // token for a call that already ended opens nothing.
    let Ok(Some(call)) = store.telecom_call(&call_id) else {
        return refuse();
    };
    if call.account_id != account_id || call.state.is_terminal() {
        return refuse();
    }
    let Ok(provider) = super::telephony::provider_for_account(&account, secret) else {
        return refuse();
    };
    let Some(format) = provider.media_stream() else {
        return refuse();
    };
    let Some(session_key) = call.session_key.clone() else {
        return refuse();
    };
    let Some(target) = route_target_for(&store, &account_id) else {
        return refuse();
    };

    let Some(accept) = request
        .headers()
        .get(SEC_WEBSOCKET_KEY)
        .and_then(|value| value.to_str().ok())
        .map(websocket_accept)
    else {
        return refuse();
    };

    let identity = CallIdentity {
        account_id,
        call_id,
        peer_number: call.peer_number.clone(),
        session_key,
    };
    tokio::spawn(async move {
        let upgraded = match hyper::upgrade::on(request).await {
            Ok(upgraded) => upgraded,
            Err(error) => {
                eprintln!("monkey daemon: a carrier media socket failed to upgrade: {error}");
                return;
            }
        };
        let stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            Role::Server,
            None,
        )
        .await;
        let mut socket = TungsteniteSocket { stream };
        let store = Mutex::new(match DaemonStore::open(&paths) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("monkey daemon: a call could not open its state: {error}");
                return;
            }
        });
        let queue = super::DaemonChannelQueue::new(paths.clone());
        let sink = QueuedCallTurns {
            store: &store,
            queue: &queue,
            target,
        };
        let speech = ConfiguredSpeech {
            app_data_dir: paths.root.clone(),
        };
        let report = run_media_session(&mut socket, &speech, &sink, format, identity).await;
        eprintln!(
            "monkey daemon: a call ended after {} turn(s)",
            report.turns_submitted
        );
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| refuse())
}

/// The route a call's turns run under.
///
/// The same routes the number's texts use: an operator who pointed this line at
/// a recipe meant the line, not one medium of it.
fn route_target_for(
    store: &DaemonStore,
    account_id: &str,
) -> Option<little_monkey_lib::channels::routing::RouteTarget> {
    let routes = store.channel_routes().ok()?;
    routes
        .iter()
        .filter(|route| route.enabled)
        .find(|route| route.scope.account_id.as_deref() == Some(account_id))
        .or_else(|| {
            routes
                .iter()
                .find(|route| route.enabled && route.scope.account_id.is_none())
        })
        .map(|route| route.target.clone())
}

struct TungsteniteSocket {
    stream: tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>,
}

#[async_trait]
impl MediaSocket for TungsteniteSocket {
    async fn recv(&mut self) -> Option<String> {
        use futures_util::StreamExt;
        loop {
            match self.stream.next().await? {
                Ok(Message::Text(text)) => return Some(text.to_string()),
                // Carriers send text frames; a binary or control frame is not
                // audio and is not worth guessing at.
                Ok(Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    async fn send(&mut self, frame: String) -> Result<(), String> {
        use futures_util::SinkExt;
        self.stream
            .send(Message::Text(frame.into()))
            .await
            .map_err(|error| error.to_string())
    }
}

/// The RFC 6455 handshake response value: the client's key, the fixed GUID,
/// SHA-1, base64.
fn websocket_accept(key: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = ring::digest::digest(
        &ring::digest::SHA1_FOR_LEGACY_USE_ONLY,
        format!("{key}{WEBSOCKET_GUID}").as_bytes(),
    );
    STANDARD.encode(digest.as_ref())
}

fn query_params(query: &str) -> std::collections::BTreeMap<String, String> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

/// One answer for every reason to say no.
fn refuse() -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Full::new(Bytes::from_static(b"not_found")))
        .expect("a static response always builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_handshake_answer_matches_the_rfc_example() {
        // RFC 6455 §1.3's worked example.
        assert_eq!(
            websocket_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn a_query_string_becomes_its_parameters() {
        let params = query_params("call=call-1&exp=17&sig=abc");
        assert_eq!(params.get("call").map(String::as_str), Some("call-1"));
        assert_eq!(params.get("exp").map(String::as_str), Some("17"));
        assert_eq!(params.get("sig").map(String::as_str), Some("abc"));
    }
}

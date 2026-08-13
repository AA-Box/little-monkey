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
use super::telecom_store::{CallDirection, InboundCallPolicy};

/// How many carrier media sockets may be open at once, across every account.
///
/// This endpoint is reachable by anyone who can reach the operator's public
/// URL. The token check below is what stops them talking to a call, but the
/// socket itself is allocated before any of that, so the count is the bound on
/// what an unauthenticated caller can make this process hold.
const MAX_MEDIA_SOCKETS: usize = 16;

/// How long a dropped media stream has to come back before the call it was
/// carrying is hung up.
///
/// A carrier's socket can close mid-call and be re-established seconds later —
/// a proxy recycling a connection, a media server handing the stream to
/// another node — and hanging up on the first close would drop somebody who is
/// still holding the phone to their ear. Waiting is not free either: with no
/// stream there is nothing to hear and nothing to say, so every extra second is
/// a caller listening to silence on a line the carrier is still billing. Two
/// seconds is long enough for a reconnect and short enough that a stream which
/// is really gone is not paid for.
const STREAM_DISCONNECT_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

fn open_sockets() -> &'static std::sync::atomic::AtomicUsize {
    static OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    &OPEN
}

/// Decrements the open-socket count however the session ends — normally, by
/// error, or by panic.
struct SocketSlot;

impl Drop for SocketSlot {
    fn drop(&mut self) {
        open_sockets().fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

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
    let secret = account_secret(&account);
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
    if provider.media_stream().is_none() {
        // A carrier that cannot stream audio can ring and text, but it cannot
        // hold a conversation, and pretending otherwise connects a caller to
        // silence.
        return refuse();
    }
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

    // What is said when the line opens. An outbound call carries the sentence
    // the operator approved; an inbound one gets the number's greeting, if the
    // operator wrote one. Voicemail takes one message and hangs up.
    let single_turn = matches!(account.inbound_policy, InboundCallPolicy::Voicemail)
        && matches!(call.direction, CallDirection::Inbound);
    let opening_line = call.opening_line.clone().or_else(|| {
        account
            .non_secret_config
            .get("greeting")
            .and_then(|value| value.as_str())
            .filter(|greeting| !greeting.trim().is_empty())
            .map(str::to_string)
    });
    if open_sockets().fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= MAX_MEDIA_SOCKETS {
        open_sockets().fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        return refuse();
    }
    let slot = SocketSlot;

    let disconnected = (account_id.clone(), call_id.clone());
    let identity = CallIdentity {
        account_id,
        call_id,
        peer_number: call.peer_number.clone(),
        session_key,
        opening_line,
        single_turn,
    };
    tokio::spawn(async move {
        let _slot = slot;
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
        let report =
            run_media_session(&mut socket, &speech, &sink, provider.as_ref(), identity).await;
        eprintln!(
            "monkey daemon: a call ended after {} turn(s)",
            report.turns_submitted
        );
        if report.stream_dropped {
            let (account_id, call_id) = disconnected;
            end_after_disconnect_grace(paths, account_id, call_id).await;
        }
    });

    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(CONNECTION, "Upgrade")
        .header(UPGRADE, "websocket")
        .header(SEC_WEBSOCKET_ACCEPT, accept)
        .body(Full::new(Bytes::new()))
        .unwrap_or_else(|_| refuse())
}

/// Hang up a call whose media stream dropped — unless the carrier comes back
/// for it first.
///
/// A closed socket is not the same fact as an ended call. The carrier decides
/// when a call is over and says so with a `stop` event and a status callback; a
/// socket that simply stops is a transport failure, and the call it was
/// carrying is still up, still billing, and now silent in both directions. So
/// the close starts a clock rather than a hangup: if a new media socket
/// registers this call inside [`STREAM_DISCONNECT_GRACE`] the conversation
/// carries on untouched, and only a stream that never came back gets the line
/// hung up and the row closed.
///
/// Doing nothing was the old behaviour, and it left the caller on a dead line
/// until the max-duration sweep noticed minutes later.
async fn end_after_disconnect_grace(paths: DaemonPaths, account_id: String, call_id: String) {
    tokio::time::sleep(STREAM_DISCONNECT_GRACE).await;
    end_dropped_call(paths, account_id, call_id).await;
}

/// What the grace decides once it is up. Split from the wait so a test can ask
/// the question without one.
async fn end_dropped_call(paths: DaemonPaths, account_id: String, call_id: String) {
    // A reconnect re-registers the call as live. That is a conversation in
    // progress, not a dead line, and it must not be hung up.
    if super::call_media::is_on_the_line(&call_id) {
        return;
    }
    let Ok(mut store) = DaemonStore::open(&paths) else {
        return;
    };
    let Ok(Some(call)) = store.telecom_call(&call_id) else {
        return;
    };
    // The carrier's own status callback may have landed during the grace. A
    // call it already closed needs nothing from us.
    if call.state.is_terminal() {
        return;
    }
    let carrier = store
        .telecom_account(&account_id)
        .ok()
        .flatten()
        .and_then(|account| {
            let secret = account_secret(&account);
            super::telephony::provider_for_account(&account, secret).ok()
        });
    let Ok(now_ms) = super::now_ms()
        .and_then(|value| i64::try_from(value).map_err(|_| "clock is beyond bounds".to_string()))
    else {
        return;
    };
    match super::telecom_worker::hang_up_and_close(
        &mut store,
        carrier.as_ref(),
        &call_id,
        call.provider_call_id.as_deref(),
        super::telephony::CallState::Completed,
        "The carrier's media stream disconnected and did not reconnect",
        now_ms,
    )
    .await
    {
        Ok(true) => eprintln!("monkey daemon: hung up {call_id} after its media stream dropped"),
        Ok(false) => eprintln!(
            "monkey daemon: {call_id} lost its media stream and the carrier would not hang it up"
        ),
        Err(error) => eprintln!("monkey daemon: could not close {call_id}: {error}"),
    }
}

/// The carrier credential for one account, resolved from the keychain.
fn account_secret(account: &super::telecom_store::TelecomAccountRecord) -> String {
    match &account.credential_ref {
        Some(reference) => super::channel_adapter::ChannelSecrets::get(
            &super::channel_adapter::KeyringChannelSecrets,
            reference,
        )
        .unwrap_or_default(),
        None => String::new(),
    }
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

    /// A telephony account and one call in progress on it, in a store of their
    /// own. No credential reference, so nothing reaches for the keychain.
    fn call_in_progress(label: &str) -> (DaemonPaths, String) {
        use super::super::telecom_store::{
            CallLimits, OutboundCallApproval, TelecomAccountRecord, TelecomCallRecord,
        };
        use super::super::telephony::{CallState, TelecomKind};
        use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

        const NOW: i64 = 1_700_000_000_000;
        let root = std::env::temp_dir().join(format!(
            "little-monkey-call-grace-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).expect("temp root");
        let paths = DaemonPaths::under(&root);
        paths.ensure().expect("paths");
        let mut store = DaemonStore::open(&paths).expect("store");
        store
            .upsert_telecom_account(&TelecomAccountRecord {
                account_id: "tel-1".into(),
                kind: TelecomKind::Mock,
                label: "Support line".into(),
                enabled: true,
                carrier_account_id: "carrier-1".into(),
                from_number: "+15550000000".into(),
                credential_ref: None,
                public_base_url: None,
                non_secret_config: serde_json::json!({}),
                inbound_policy: InboundCallPolicy::Answer,
                outbound_approval: OutboundCallApproval::Approval,
                limits: CallLimits::default(),
                health: ChannelHealth {
                    state: HealthState::Connected,
                    detail: None,
                    last_error: None,
                    probed_at_ms: NOW,
                },
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("account");
        let call_id = format!("call-{label}");
        store
            .start_call(&TelecomCallRecord {
                call_id: call_id.clone(),
                account_id: "tel-1".into(),
                provider_call_id: Some(format!("carrier-{label}")),
                direction: CallDirection::Inbound,
                peer_number: "+15551234567".into(),
                state: CallState::InProgress,
                session_key: Some(format!("call:tel-1:{call_id}")),
                job_id: None,
                idempotency_key: format!("inbound:carrier-{label}"),
                opening_line: None,
                last_error: None,
                started_at_ms: Some(NOW),
                ended_at_ms: None,
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("call");
        (paths, call_id)
    }

    fn state_of(paths: &DaemonPaths, call_id: &str) -> super::super::telephony::CallState {
        DaemonStore::open(paths)
            .expect("store")
            .telecom_call(call_id)
            .expect("query")
            .expect("row")
            .state
    }

    #[test]
    fn a_dropped_stream_is_given_two_seconds_to_come_back() {
        assert_eq!(STREAM_DISCONNECT_GRACE.as_millis(), 2000);
    }

    /// The stream is gone and stays gone: the caller is on a line nobody can
    /// hear, so the carrier is told to hang it up.
    #[tokio::test]
    async fn a_stream_that_never_comes_back_ends_the_call() {
        let (paths, call_id) = call_in_progress("dropped");

        end_dropped_call(paths.clone(), "tel-1".to_string(), call_id.clone()).await;

        assert_eq!(
            state_of(&paths, &call_id),
            super::super::telephony::CallState::Completed
        );
    }

    /// The same drop, but the carrier reconnects inside the grace. Somebody is
    /// mid-conversation, and hanging up on them is the failure this avoids.
    #[tokio::test]
    async fn a_stream_that_reconnects_inside_the_grace_keeps_the_call() {
        let (paths, call_id) = call_in_progress("reconnected");
        let _reconnected = super::super::call_media::register_reconnected_call(&call_id);

        end_dropped_call(paths.clone(), "tel-1".to_string(), call_id.clone()).await;

        assert_eq!(
            state_of(&paths, &call_id),
            super::super::telephony::CallState::InProgress,
            "a reconnected stream must not have its call hung up"
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

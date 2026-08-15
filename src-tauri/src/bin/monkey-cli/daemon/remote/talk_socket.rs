//! The wire under a Talk session: one upgraded WebSocket, the operator's own
//! speech backends, and the durable queue every other turn goes through.
//!
//! Kept apart from [`super::talk`] on purpose. That module is the conversation
//! and is driven by scripted sockets in tests; this one is the plumbing that
//! cannot be — a real TLS connection, a real synthesizer, a real run ledger.
//! The seam between them is three traits, so the conversation's behaviour is
//! provable without any of it.

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use super::api::{RemoteApi, TalkSessionTurns, TalkSocketAuthorization};
use super::protocol::MAX_TALK_FRAME_BYTES;
use super::talk::{run_talk_session, TalkIdentity, TalkSocket, TalkSpeech};

/// How long a Talk socket may stay open. A conversation that outlives this is
/// reopened with a fresh ticket; a tab left open overnight is not a microphone
/// left open overnight.
const MAX_SESSION_MS: u64 = 60 * 60 * 1_000;
/// How long a socket may carry nothing from the device before it is closed. A
/// long answer is not idle — the runner is talking — but a phone that connected
/// and then went quiet is holding a microphone open for no one.
const MAX_IDLE_MS: u64 = 15 * 60 * 1_000;

type Upgraded = TokioIo<hyper::upgrade::Upgraded>;

struct WebSocketTalkSocket {
    inner: WebSocketStream<Upgraded>,
    /// Absolute, so the bound survives `try_recv` dropping a read mid-poll
    /// every hundred milliseconds for the length of an answer.
    session_deadline: tokio::time::Instant,
    idle_deadline: tokio::time::Instant,
    /// What ended the socket, when it was not the device closing it politely.
    violation: Option<&'static str>,
}

#[async_trait]
impl TalkSocket for WebSocketTalkSocket {
    async fn recv(&mut self) -> Option<String> {
        loop {
            let deadline = self.session_deadline.min(self.idle_deadline);
            let next = match tokio::time::timeout_at(deadline, self.inner.next()).await {
                Ok(next) => next?,
                Err(_) => {
                    self.violation = Some("timeout");
                    return None;
                }
            };
            match next {
                Ok(Message::Text(text)) => {
                    self.idle_deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_millis(MAX_IDLE_MS);
                    return Some(text.to_string());
                }
                // Binary frames are not part of this protocol: audio rides
                // base64 inside a versioned JSON envelope, so a binary frame is
                // either a different protocol or a probe. Either way the socket
                // ends here rather than becoming a session nobody is driving.
                Ok(Message::Binary(_)) => {
                    self.violation = Some("binary frame");
                    return None;
                }
                Ok(Message::Close(_)) => return None,
                Ok(Message::Ping(payload)) => {
                    if self.inner.send(Message::Pong(payload)).await.is_err() {
                        return None;
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    self.violation = Some("transport error");
                    return None;
                }
            }
        }
    }

    async fn send(&mut self, frame: String) -> Result<(), String> {
        self.inner
            .send(Message::Text(frame.into()))
            .await
            .map_err(|error| format!("Talk socket send failed: {error}"))
    }
}

/// The operator's own configured speech stack, reached through the shared
/// companion state so a Talk session cannot use a different provider than the
/// desktop does. The same rule, and the same seam, as a phone call's.
struct ConfiguredTalkSpeech {
    app_data_dir: std::path::PathBuf,
}

#[async_trait]
impl TalkSpeech for ConfiguredTalkSpeech {
    async fn transcribe(&self, audio: Vec<u8>, media_type: &str) -> Result<String, String> {
        little_monkey_lib::m7_companion::transcribe_audio_bytes(
            &self.app_data_dir,
            &audio,
            media_type,
        )
        .await
    }

    async fn synthesize(&self, text: &str) -> Result<(Vec<u8>, String), String> {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-talk-{}.wav",
            uuid::Uuid::new_v4().simple()
        ));
        let result = little_monkey_lib::m7_companion::synthesize_speech_to_wav(
            &self.app_data_dir,
            text,
            &path,
        )
        .await
        .and_then(|()| std::fs::read(&path).map_err(|error| error.to_string()));
        // Always: synthesized speech of somebody's conversation must not be
        // left in a temporary directory.
        let _ = std::fs::remove_file(&path);
        result.map(|bytes| (bytes, "audio/wav".to_string()))
    }
}

/// Runs one admitted Talk socket to completion.
pub(crate) async fn serve(
    api: RemoteApi,
    authorization: TalkSocketAuthorization,
    connection: Upgraded,
) {
    let mut config = WebSocketConfig::default();
    // The protocol's own frame ceiling, enforced by the transport as well: a
    // client that ignores it is disconnected rather than allocated for.
    config.max_message_size = Some(MAX_TALK_FRAME_BYTES);
    config.max_frame_size = Some(MAX_TALK_FRAME_BYTES);
    let started = tokio::time::Instant::now();
    let started_ms = super::now_ms_public().unwrap_or_default();
    let mut socket = WebSocketTalkSocket {
        inner: WebSocketStream::from_raw_socket(connection, Role::Server, Some(config)).await,
        session_deadline: started + std::time::Duration::from_millis(MAX_SESSION_MS),
        idle_deadline: started + std::time::Duration::from_millis(MAX_IDLE_MS),
        violation: None,
    };
    let configured = ConfiguredTalkSpeech {
        app_data_dir: api.app_data_dir_for_talk(),
    };
    let injected = api.talk_speech();
    let speech: &dyn TalkSpeech = match injected.as_deref() {
        Some(speech) => speech,
        None => &configured,
    };
    let identity = TalkIdentity {
        device_id: authorization.device_id.clone(),
        session_id: authorization.session_id.clone(),
        session_generation: authorization.session_generation.clone(),
    };
    // Registered before the first frame and closed after the last, so "this
    // device is capturing right now" is true exactly while it is true.
    let capture = api.open_talk_capture(
        &authorization.device_id,
        &authorization.session_id,
        started_ms.saturating_add(MAX_SESSION_MS),
    );
    let turns = TalkSessionTurns::new(api.clone(), &authorization);
    // The session's bound is the socket's own deadline rather than a timeout
    // wrapped around the conversation: cancelling the future here would drop the
    // report with it, and a session that ran for an hour would be audited as
    // though nothing had happened.
    let mut report = run_talk_session(&mut socket, speech, &turns, identity).await;
    if socket.violation.is_some() {
        report.stream_dropped = true;
    }
    if let Some(command_id) = capture {
        let ended = if report.grant_revoked {
            Some("The voice_stream grant was withdrawn while the socket was open.")
        } else {
            socket.violation.map(|violation| match violation {
                "timeout" => "The Talk socket reached its deadline.",
                "binary frame" => "The device sent a frame this protocol does not carry.",
                _ => "The Talk socket stopped carrying frames.",
            })
        };
        api.close_talk_capture(&authorization.device_id, &command_id, ended);
    }
    // Counters only. What was said, and the audio it was said in, stop at this
    // function — see `talk.rs`'s header.
    api.record_talk_session(&authorization.device_id, &report);
    let _ = socket.inner.close(None).await;
}

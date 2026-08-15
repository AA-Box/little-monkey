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

type Upgraded = TokioIo<hyper::upgrade::Upgraded>;

struct WebSocketTalkSocket {
    inner: WebSocketStream<Upgraded>,
}

#[async_trait]
impl TalkSocket for WebSocketTalkSocket {
    async fn recv(&mut self) -> Option<String> {
        loop {
            match self.inner.next().await? {
                Ok(Message::Text(text)) => return Some(text.to_string()),
                // Binary frames are not part of this protocol: audio rides
                // base64 inside a versioned JSON envelope, so a binary frame is
                // either a different protocol or a probe.
                Ok(Message::Binary(_)) => return None,
                Ok(Message::Close(_)) => return None,
                Ok(Message::Ping(payload)) => {
                    if self.inner.send(Message::Pong(payload)).await.is_err() {
                        return None;
                    }
                }
                Ok(_) => {}
                Err(_) => return None,
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
    let mut socket = WebSocketTalkSocket {
        inner: WebSocketStream::from_raw_socket(connection, Role::Server, Some(config)).await,
    };
    let speech = ConfiguredTalkSpeech {
        app_data_dir: api.app_data_dir_for_talk(),
    };
    let identity = TalkIdentity {
        device_id: authorization.device_id.clone(),
        session_id: authorization.session_id.clone(),
        session_generation: authorization.session_generation.clone(),
    };
    let turns = TalkSessionTurns::new(api.clone(), &authorization);
    let report = match tokio::time::timeout(
        std::time::Duration::from_millis(MAX_SESSION_MS),
        run_talk_session(&mut socket, &speech, &turns, identity),
    )
    .await
    {
        Ok(report) => report,
        Err(_) => Default::default(),
    };
    // Counters only. What was said, and the audio it was said in, stop at this
    // function — see `talk.rs`'s header.
    api.record_talk_session(&authorization.device_id, &report);
    let _ = socket.inner.close(None).await;
}

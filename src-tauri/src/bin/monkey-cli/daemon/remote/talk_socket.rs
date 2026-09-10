//! The wire under a Talk session: one upgraded WebSocket, the operator's own
//! speech backends, and the durable queue every other turn goes through.
//!
//! Kept apart from [`super::talk`] on purpose. That module is the conversation
//! and is driven by scripted sockets in tests; this one is the plumbing that
//! cannot be — a real TLS connection, a real synthesizer, a real run ledger.
//! The seam between them is three traits, so the conversation's behaviour is
//! provable without any of it.

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use futures_util::{SinkExt, StreamExt};
use std::collections::VecDeque;
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use super::api::{RemoteApi, TalkSessionTurns, TalkSocketAuthorization};
use super::protocol::{
    TalkClientFrame, TalkClientFrameKind, TalkSequenceTracker, TalkServerFrame,
    TalkServerFrameKind, TalkState, MAX_TALK_FRAME_BYTES, TALK_PROTOCOL_VERSION,
};
use super::talk::{
    bounded_output_audio_chunks, run_talk_session, TalkIdentity, TalkSessionReport, TalkSocket, TalkSpeech,
};

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


async fn send_output_frame(
    socket: &mut dyn TalkSocket,
    authorization: &TalkSocketAuthorization,
    sequence: &mut u64,
    kind: TalkServerFrameKind,
) -> Result<(), String> {
    *sequence = sequence.saturating_add(1);
    let frame = TalkServerFrame {
        protocol_version: TALK_PROTOCOL_VERSION,
        session_id: authorization.session_id.clone(),
        session_generation: authorization.session_generation.clone(),
        frame_sequence: *sequence,
        kind,
    };
    frame.validate()?;
    socket.send(serde_json::to_string(&frame).map_err(|error| error.to_string())?).await
}

fn parse_output_client_frame(
    raw: &str,
    authorization: &TalkSocketAuthorization,
    tracker: &mut TalkSequenceTracker,
) -> Result<TalkClientFrame, String> {
    let frame: TalkClientFrame = serde_json::from_str(raw)
        .map_err(|error| format!("Invalid Talk frame: {error}"))?;
    frame.validate()?;
    if frame.session_id != authorization.session_id
        || frame.session_generation != authorization.session_generation
    {
        return Err("Talk frame belongs to a different session generation".to_string());
    }
    tracker.accept(frame.frame_sequence, frame.audio_sequence())?;
    Ok(frame)
}

async fn output_wait_for_ack(
    socket: &mut dyn TalkSocket,
    api: &RemoteApi,
    authorization: &TalkSocketAuthorization,
    tracker: &mut TalkSequenceTracker,
    outbound_sequence: &mut u64,
    audio_sequence: u64,
    cursor: &mut u64,
    backlog: &mut VecDeque<super::store::VoiceRouteEventRecord>,
) -> Result<bool, String> {
    let generation = authorization.route_generation.ok_or_else(|| "Output route has no generation".to_string())?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if !api.talk_output_route_live(authorization) {
            return Err("VoiceRoute output authority was revoked or moved".to_string());
        }
        for event in api.talk_route_events(&authorization.session_id, *cursor)? {
            *cursor = (*cursor).max(event.event_id);
            if event.generation != generation { continue; }
            if event.kind == "output_stop" {
                send_output_frame(
                    socket,
                    authorization,
                    outbound_sequence,
                    TalkServerFrameKind::State { state: TalkState::Interrupted },
                ).await?;
                return Ok(false);
            }
            backlog.push_back(event);
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("Timed out waiting for paired speaker playback acknowledgement".to_string());
        }
        match tokio::time::timeout(std::time::Duration::from_millis(100), socket.recv()).await {
            Ok(Some(raw)) => {
                let frame = parse_output_client_frame(&raw, authorization, tracker)?;
                match frame.kind {
                    TalkClientFrameKind::PlaybackAck { audio_sequence: ack, played } if ack == audio_sequence => {
                        return Ok(played);
                    }
                    TalkClientFrameKind::Interrupt { .. } => return Ok(false),
                    TalkClientFrameKind::State { .. } | TalkClientFrameKind::PlaybackAck { .. } => {}
                    _ => return Err("An output-only Talk socket received a microphone frame".to_string()),
                }
            }
            Ok(None) => return Err("Paired speaker Talk socket closed".to_string()),
            Err(_) => {}
        }
    }
}

async fn run_output_route_session(
    socket: &mut dyn TalkSocket,
    speech: &dyn TalkSpeech,
    api: &RemoteApi,
    authorization: &TalkSocketAuthorization,
) -> TalkSessionReport {
    let mut report = TalkSessionReport::default();
    let mut outbound_sequence = 0u64;
    let mut outbound_audio_sequence = 0u64;
    let mut inbound = TalkSequenceTracker::default();
    let mut cursor = authorization.route_event_cursor;
    let mut backlog = VecDeque::new();
    let generation = match authorization.route_generation {
        Some(value) => value,
        None => { report.errors += 1; return report; }
    };

    if send_output_frame(socket, authorization, &mut outbound_sequence, TalkServerFrameKind::Ready).await.is_err() {
        report.stream_dropped = true;
        return report;
    }
    // The first client frame remains the normal Talk hello. Output-only roles
    // carry no microphone bytes after it.
    let Some(raw) = socket.recv().await else { report.stream_dropped = true; return report; };
    match parse_output_client_frame(&raw, authorization, &mut inbound) {
        Ok(TalkClientFrame { kind: TalkClientFrameKind::Hello { .. }, .. }) => {}
        _ => {
            report.errors += 1;
            let _ = send_output_frame(
                socket,
                authorization,
                &mut outbound_sequence,
                TalkServerFrameKind::Error {
                    code: "hello_required".into(),
                    message: "An output-only Talk socket must start with hello.".into(),
                    retryable: false,
                },
            ).await;
            return report;
        }
    }
    let _ = send_output_frame(
        socket,
        authorization,
        &mut outbound_sequence,
        TalkServerFrameKind::State { state: TalkState::Idle },
    ).await;
    if let Some(command_id) = api.talk_route_command_id(authorization) {
        let _ = api.append_talk_route_event(
            &authorization.session_id,
            generation,
            "output_ready",
            &serde_json::json!({ "command_id": command_id, "device_id": authorization.device_id }),
        );
    }

    loop {
        if !api.talk_output_route_live(authorization) {
            report.grant_revoked = true;
            break;
        }
        if backlog.is_empty() {
            match api.talk_route_events(&authorization.session_id, cursor) {
                Ok(events) => {
                    for event in events {
                        cursor = cursor.max(event.event_id);
                        if event.generation == generation { backlog.push_back(event); }
                    }
                }
                Err(_) => { report.errors += 1; break; }
            }
        }
        let Some(event) = backlog.pop_front() else {
            match tokio::time::timeout(std::time::Duration::from_millis(120), socket.recv()).await {
                Ok(Some(raw)) => {
                    match parse_output_client_frame(&raw, authorization, &mut inbound) {
                        Ok(TalkClientFrame { kind: TalkClientFrameKind::State { .. } | TalkClientFrameKind::PlaybackAck { .. }, .. }) => {}
                        Ok(TalkClientFrame { kind: TalkClientFrameKind::Interrupt { .. }, .. }) => {
                            let _ = send_output_frame(socket, authorization, &mut outbound_sequence, TalkServerFrameKind::State { state: TalkState::Interrupted }).await;
                        }
                        Ok(_) | Err(_) => { report.errors += 1; break; }
                    }
                }
                Ok(None) => break,
                Err(_) => {}
            }
            continue;
        };
        match event.kind.as_str() {
            "output_stop" => {
                let _ = send_output_frame(socket, authorization, &mut outbound_sequence, TalkServerFrameKind::State { state: TalkState::Interrupted }).await;
            }
            "speak_text" => {
                let Some(text) = event.payload.get("text").and_then(serde_json::Value::as_str) else { continue; };
                let job_id = event.payload.get("job_id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let turn_id = event.payload.get("turn_id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let (bytes, media_type) = match speech.synthesize(text).await {
                    Ok(value) => value,
                    Err(error) => {
                        report.errors += 1;
                        let _ = api.append_talk_route_event(&authorization.session_id, generation, "output_failed", &serde_json::json!({"job_id": job_id, "turn_id": turn_id, "error": error}));
                        continue;
                    }
                };
                let chunks = match bounded_output_audio_chunks(bytes, &media_type) {
                    Ok(chunks) => chunks,
                    Err(error) => {
                        report.errors += 1;
                        let _ = api.append_talk_route_event(
                            &authorization.session_id,
                            generation,
                            "output_failed",
                            &serde_json::json!({"job_id": job_id, "turn_id": turn_id, "error": error}),
                        );
                        continue;
                    }
                };
                let chunk_count = match u32::try_from(chunks.len()) {
                    Ok(value) if value > 0 => value,
                    _ => { report.errors += 1; continue; }
                };
                let response_id = format!("route-{generation}-{}", event.event_id);
                if send_output_frame(
                    socket,
                    authorization,
                    &mut outbound_sequence,
                    TalkServerFrameKind::State { state: TalkState::Speaking },
                ).await.is_err() {
                    report.stream_dropped = true;
                    break;
                }
                let mut playback_error: Option<String> = None;
                for (chunk_index, bytes) in chunks.into_iter().enumerate() {
                    outbound_audio_sequence = outbound_audio_sequence.saturating_add(1);
                    let audio_sequence = outbound_audio_sequence;
                    if send_output_frame(
                        socket,
                        authorization,
                        &mut outbound_sequence,
                        TalkServerFrameKind::OutputAudio {
                            audio_sequence,
                            response_id: response_id.clone(),
                            chunk_index: u32::try_from(chunk_index).unwrap_or(u32::MAX),
                            chunk_count,
                            route_generation: Some(generation),
                            media_type: media_type.clone(),
                            audio_base64: STANDARD.encode(bytes),
                        },
                    ).await.is_err() {
                        report.stream_dropped = true;
                        playback_error = Some("Paired speaker Talk socket closed while streaming output".to_string());
                        break;
                    }
                    // Backpressure is per playable chunk: a slow phone can hold
                    // at most the current bounded frame, never an unbounded TTS
                    // response queued in host memory.
                    match output_wait_for_ack(
                        socket, api, authorization, &mut inbound, &mut outbound_sequence,
                        audio_sequence, &mut cursor, &mut backlog,
                    ).await {
                        Ok(true) => {}
                        Ok(false) => {
                            playback_error = Some("Playback was interrupted".to_string());
                            break;
                        }
                        Err(error) => {
                            report.stream_dropped = true;
                            playback_error = Some(error);
                            break;
                        }
                    }
                }
                if let Some(error) = playback_error {
                    let _ = api.append_talk_route_event(
                        &authorization.session_id, generation, "output_failed",
                        &serde_json::json!({"job_id": job_id, "turn_id": turn_id, "error": error}),
                    );
                    if report.stream_dropped { break; }
                } else {
                    report.spoken_chunks = report.spoken_chunks.saturating_add(1);
                    let _ = api.append_talk_route_event(
                        &authorization.session_id, generation, "output_played",
                        &serde_json::json!({
                            "job_id": job_id, "turn_id": turn_id,
                            "audio_sequence": outbound_audio_sequence, "chunk_count": chunk_count,
                        }),
                    );
                }
                let _ = send_output_frame(
                    socket, authorization, &mut outbound_sequence,
                    TalkServerFrameKind::State { state: TalkState::Idle },
                ).await;
            }
            _ => {}
        }
    }
    report
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
    let output_only = authorization.route_role.as_deref() == Some("output");
    let mut report = if output_only {
        run_output_route_session(&mut socket, speech, &api, &authorization).await
    } else {
        let identity = TalkIdentity {
            device_id: authorization.device_id.clone(),
            session_id: authorization.session_id.clone(),
            session_generation: authorization.session_generation.clone(),
            route_generation: authorization.route_generation,
        };
        // Only input/duplex roles register microphone capture. Output-only
        // routes never request or report microphone ownership.
        let capture = api.open_talk_capture(
            &authorization.device_id,
            &authorization.session_id,
            started_ms.saturating_add(MAX_SESSION_MS),
        );
        if let (Some(generation), Some(command_id)) =
            (authorization.route_generation, api.talk_route_command_id(&authorization))
        {
            let _ = api.append_talk_route_event(
                &authorization.session_id,
                generation,
                "input_ready",
                &serde_json::json!({ "command_id": command_id, "device_id": authorization.device_id }),
            );
        }
        let turns = TalkSessionTurns::new(api.clone(), &authorization);
        let report = run_talk_session(&mut socket, speech, &turns, identity).await;
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
        report
    };
    if socket.violation.is_some() {
        report.stream_dropped = true;
    }
    // Counters only. What was said, and the audio it was said in, stop at this
    // function — see `talk.rs`'s header.
    api.record_talk_session(&authorization.device_id, &report);
    let _ = socket.inner.close(None).await;
}

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
use std::collections::{HashMap, HashSet, VecDeque};
use hyper_util::rt::TokioIo;
use tokio_tungstenite::tungstenite::protocol::{Message, Role, WebSocketConfig};
use tokio_tungstenite::WebSocketStream;

use super::api::{RemoteApi, TalkSessionTurns, TalkSocketAuthorization};
use super::protocol::{
    TalkClientFrame, TalkClientFrameKind, TalkSequenceTracker, TalkServerFrame,
    TalkServerFrameKind, TalkState, MAX_TALK_FRAME_BYTES, TALK_PROTOCOL_VERSION,
};
use super::realtime_bridge::REALTIME_PCM_MEDIA_TYPE;
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


/// A span in whole milliseconds, saturating rather than wrapping — a socket
/// left open for an hour must not report a negative or truncated duration.
fn elapsed_ms(since: tokio::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
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

/// What a device is told when the authority under its socket goes away.
///
/// The input half already does this: a withdrawn `voice_stream` grant reaches
/// the phone as a non-retryable `capability_revoked` error before the socket
/// closes, and the device's own rule is that a non-retryable error ends Talk
/// instead of reconnecting. A speaker was told nothing at all, so revocation
/// and a flaky network looked identical from the device and it retried a route
/// it is no longer allowed to hold. `Interrupted` leads, because the device is
/// still holding whatever it has not acknowledged and must drop it rather than
/// play a conversation the grant no longer covers.
async fn report_route_revocation(
    socket: &mut dyn TalkSocket,
    authorization: &TalkSocketAuthorization,
    sequence: &mut u64,
    stop_playback: bool,
    message: &str,
) {
    if stop_playback {
        let _ = send_output_frame(
            socket,
            authorization,
            sequence,
            TalkServerFrameKind::State { state: TalkState::Interrupted },
        )
        .await;
    }
    let _ = send_output_frame(
        socket,
        authorization,
        sequence,
        TalkServerFrameKind::Error {
            code: "capability_revoked".into(),
            message: message.to_string(),
            retryable: false,
        },
    )
    .await;
    let _ = send_output_frame(
        socket,
        authorization,
        sequence,
        TalkServerFrameKind::State { state: TalkState::Idle },
    )
    .await;
}

/// Both halves of the same sentence: the route no longer names this device,
/// either because the capability was withdrawn or because the conversation was
/// pointed somewhere else. The device cannot act on the difference — both mean
/// stop — so it is told one thing.
const OUTPUT_REVOKED: &str = "This Talk route no longer grants audio_playback to this device.";
const INPUT_REVOKED: &str = "This Talk route no longer grants voice_stream to this device.";

/// What the wait for one chunk's acknowledgement ended on.
///
/// Four outcomes rather than a bool because the ones that stop a response are
/// not the same event: an interruption is somebody speaking over the answer, a
/// failure is a speaker that could not play it, and a revocation is an operator
/// taking the speaker away. They are counted apart — and only the last of them
/// ends the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AckOutcome {
    Played,
    Failed,
    Interrupted,
    Revoked,
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
) -> Result<AckOutcome, String> {
    let generation = authorization.route_generation.ok_or_else(|| "Output route has no generation".to_string())?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    loop {
        if !api.talk_output_route_live(authorization) {
            return Ok(AckOutcome::Revoked);
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
                return Ok(AckOutcome::Interrupted);
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
                        return Ok(if played { AckOutcome::Played } else { AckOutcome::Failed });
                    }
                    TalkClientFrameKind::Interrupt { .. } => return Ok(AckOutcome::Interrupted),
                    TalkClientFrameKind::State { .. }
                    | TalkClientFrameKind::PlaybackAck { .. }
                    | TalkClientFrameKind::InputGateAck { .. } => {}
                    _ => return Err("An output-only Talk socket received a microphone frame".to_string()),
                }
            }
            Ok(None) => return Err("Paired speaker Talk socket closed".to_string()),
            Err(_) => {}
        }
    }
}

async fn run_realtime_route_session(
    socket: &mut dyn TalkSocket,
    api: &RemoteApi,
    authorization: &TalkSocketAuthorization,
) -> TalkSessionReport {
    const MAX_IN_FLIGHT_OUTPUT: usize = 8;
    let mut report = TalkSessionReport::default();
    let mut outbound_sequence = 0u64;
    let mut outbound_audio_sequence = 0u64;
    let mut inbound = TalkSequenceTracker::default();
    let mut pending_output_acks = HashSet::<u64>::new();
    let mut pending_input_gates = HashMap::<u64, bool>::new();
    let mut route_cursor = authorization.route_event_cursor;
    let generation = match authorization.route_generation {
        Some(value) => value,
        None => {
            report.errors = report.errors.saturating_add(1);
            return report;
        }
    };
    let input_role = matches!(authorization.route_role.as_deref(), Some("input" | "duplex"));
    let output_role = matches!(authorization.route_role.as_deref(), Some("output" | "duplex"));

    if send_output_frame(socket, authorization, &mut outbound_sequence, TalkServerFrameKind::Ready)
        .await
        .is_err()
    {
        report.stream_dropped = true;
        return report;
    }
    let Some(raw) = socket.recv().await else {
        report.stream_dropped = true;
        return report;
    };
    let hello_ok = matches!(
        parse_output_client_frame(&raw, authorization, &mut inbound),
        Ok(TalkClientFrame {
            kind: TalkClientFrameKind::Hello {
                ref media_type,
                sample_rate_hz: 24_000,
                channels: 1,
            },
            ..
        }) if media_type == REALTIME_PCM_MEDIA_TYPE
    );
    if !hello_ok {
        report.errors = report.errors.saturating_add(1);
        let _ = send_output_frame(
            socket,
            authorization,
            &mut outbound_sequence,
            TalkServerFrameKind::Error {
                code: "realtime_pcm_required".into(),
                message: "Paired Realtime Talk requires mono 24 kHz PCM16 audio.".into(),
                retryable: false,
            },
        )
        .await;
        return report;
    }

    let _ = send_output_frame(
        socket,
        authorization,
        &mut outbound_sequence,
        TalkServerFrameKind::State {
            state: TalkState::Listening,
        },
    )
    .await;
    if let Some(command_id) = api.talk_route_command_id(authorization) {
        if input_role {
            let _ = api.append_talk_route_event(
                &authorization.session_id,
                generation,
                "input_ready",
                &serde_json::json!({ "command_id": command_id, "device_id": authorization.device_id }),
            );
        }
        if output_role {
            let _ = api.append_talk_route_event(
                &authorization.session_id,
                generation,
                "output_ready",
                &serde_json::json!({ "command_id": command_id, "device_id": authorization.device_id }),
            );
        }
    }

    loop {
        if input_role && !api.talk_input_route_live(authorization) {
            report.grant_revoked = true;
            report_route_revocation(
                socket, authorization, &mut outbound_sequence, output_role, INPUT_REVOKED,
            ).await;
            break;
        }
        if output_role && !api.talk_output_route_live(authorization) {
            report.grant_revoked = true;
            report_route_revocation(
                socket, authorization, &mut outbound_sequence, true, OUTPUT_REVOKED,
            ).await;
            break;
        }

        // Only the drain's own failures end the socket. Counted errors must not
        // be the test: a chunk the speaker could not play raises `errors` too,
        // and that ends one answer rather than the conversation.
        let mut drain_failed = false;
        for event in api.talk_route_events(&authorization.session_id, route_cursor).unwrap_or_default() {
            route_cursor = route_cursor.max(event.event_id);
            if event.generation != generation { continue; }
            match event.kind.as_str() {
                "output_stop" if output_role => {
                    report.interruptions = report.interruptions.saturating_add(1);
                    let _ = api.clear_realtime_output_from_host(&authorization.session_id, generation);
                    pending_output_acks.clear();
                    let _ = send_output_frame(
                        socket,
                        authorization,
                        &mut outbound_sequence,
                        TalkServerFrameKind::State { state: TalkState::Interrupted },
                    ).await;
                }
                "input_gate" if input_role => {
                    let Some(open) = event.payload.get("open").and_then(serde_json::Value::as_bool) else { continue; };
                    if pending_input_gates.len() >= 8 {
                        report.errors = report.errors.saturating_add(1);
                        drain_failed = true;
                        break;
                    }
                    if send_output_frame(
                        socket,
                        authorization,
                        &mut outbound_sequence,
                        TalkServerFrameKind::InputGate { gate_sequence: event.event_id, open },
                    ).await.is_err() {
                        report.stream_dropped = true;
                        break;
                    }
                    pending_input_gates.insert(event.event_id, open);
                }
                _ => {}
            }
        }
        if report.stream_dropped || drain_failed { break; }

        if output_role {
            while pending_output_acks.len() < MAX_IN_FLIGHT_OUTPUT {
                let chunk = match api.take_realtime_output_for_device(authorization) {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => {
                        report.grant_revoked = true;
                        break;
                    }
                };
                outbound_audio_sequence = outbound_audio_sequence.saturating_add(1);
                let audio_sequence = outbound_audio_sequence;
                if send_output_frame(
                    socket,
                    authorization,
                    &mut outbound_sequence,
                    TalkServerFrameKind::OutputAudio {
                        audio_sequence,
                        response_id: format!("realtime-{generation}-{}", chunk.sequence),
                        chunk_index: 0,
                        chunk_count: 1,
                        route_generation: Some(generation),
                        media_type: REALTIME_PCM_MEDIA_TYPE.to_string(),
                        audio_base64: STANDARD.encode(chunk.bytes),
                    },
                )
                .await
                .is_err()
                {
                    report.stream_dropped = true;
                    break;
                }
                pending_output_acks.insert(audio_sequence);
                report.spoken_chunks = report.spoken_chunks.saturating_add(1);
            }
            if report.grant_revoked {
                report_route_revocation(
                    socket, authorization, &mut outbound_sequence, true, OUTPUT_REVOKED,
                ).await;
            }
            if report.stream_dropped || report.grant_revoked {
                break;
            }
        }

        match tokio::time::timeout(std::time::Duration::from_millis(20), socket.recv()).await {
            Ok(Some(raw)) => {
                let frame = match parse_output_client_frame(&raw, authorization, &mut inbound) {
                    Ok(frame) => frame,
                    Err(error) => {
                        report.errors = report.errors.saturating_add(1);
                        let _ = send_output_frame(
                            socket,
                            authorization,
                            &mut outbound_sequence,
                            TalkServerFrameKind::Error {
                                code: "invalid_realtime_frame".into(),
                                message: error,
                                retryable: false,
                            },
                        )
                        .await;
                        break;
                    }
                };
                match frame.kind {
                    TalkClientFrameKind::Audio {
                        media_type,
                        audio_base64,
                        last,
                        ..
                    } if input_role => {
                        if media_type != REALTIME_PCM_MEDIA_TYPE || last {
                            report.errors = report.errors.saturating_add(1);
                            let _ = send_output_frame(
                                socket,
                                authorization,
                                &mut outbound_sequence,
                                TalkServerFrameKind::Error {
                                    code: "invalid_realtime_audio".into(),
                                    message: "Realtime PCM is a continuous stream and cannot contain a closing utterance frame.".into(),
                                    retryable: false,
                                },
                            )
                            .await;
                            break;
                        }
                        let bytes = match STANDARD.decode(audio_base64) {
                            Ok(bytes) => bytes,
                            Err(_) => {
                                report.errors = report.errors.saturating_add(1);
                                break;
                            }
                        };
                        if let Err(error) = api.push_realtime_input_from_device(authorization, bytes) {
                            report.errors = report.errors.saturating_add(1);
                            let _ = send_output_frame(
                                socket,
                                authorization,
                                &mut outbound_sequence,
                                TalkServerFrameKind::Error {
                                    code: "realtime_backpressure".into(),
                                    message: error,
                                    retryable: true,
                                },
                            )
                            .await;
                            break;
                        }
                    }
                    TalkClientFrameKind::PlaybackAck { audio_sequence, played } if output_role => {
                        if pending_output_acks.remove(&audio_sequence) {
                            let (kind, payload) = if played {
                                ("output_played", serde_json::json!({ "audio_sequence": audio_sequence }))
                            } else {
                                report.errors = report.errors.saturating_add(1);
                                report.fallbacks = report.fallbacks.saturating_add(1);
                                ("output_failed", serde_json::json!({
                                    "audio_sequence": audio_sequence,
                                    "error": "paired Realtime speaker could not play PCM",
                                }))
                            };
                            let _ = api.append_talk_route_event(
                                &authorization.session_id, generation, kind, &payload,
                            );
                            // A speaker that failed one chunk ends the answer it
                            // was playing, not the conversation — the same rule
                            // the pipeline path follows. What is already queued
                            // is dropped rather than played into a device that
                            // just said it cannot play, and the acks in flight
                            // are forgotten so a late one for the abandoned
                            // response cannot be recorded as played.
                            if !played {
                                pending_output_acks.clear();
                                let _ = api.clear_realtime_output_from_host(
                                    &authorization.session_id,
                                    generation,
                                );
                            }
                        }
                    }
                    TalkClientFrameKind::InputGateAck { gate_sequence, open } if input_role => {
                        if pending_input_gates.remove(&gate_sequence).is_some_and(|expected| expected == open) {
                            let _ = api.append_talk_route_event(
                                &authorization.session_id,
                                generation,
                                "input_gate_ack",
                                &serde_json::json!({ "gate_sequence": gate_sequence, "open": open }),
                            );
                        }
                    }
                    TalkClientFrameKind::Interrupt { reason } => {
                        report.interruptions = report.interruptions.saturating_add(1);
                        let _ = api.append_talk_route_event(
                            &authorization.session_id,
                            generation,
                            "interrupt",
                            &serde_json::json!({ "reason": reason.unwrap_or_else(|| "paired_device".to_string()) }),
                        );
                        let _ = send_output_frame(
                            socket,
                            authorization,
                            &mut outbound_sequence,
                            TalkServerFrameKind::State {
                                state: TalkState::Interrupted,
                            },
                        )
                        .await;
                    }
                    TalkClientFrameKind::State { .. }
                    | TalkClientFrameKind::Metrics { .. }
                    | TalkClientFrameKind::PlaybackAck { .. }
                    | TalkClientFrameKind::InputGateAck { .. } => {}
                    _ => {
                        report.errors = report.errors.saturating_add(1);
                        break;
                    }
                }
            }
            Ok(None) => break,
            Err(_) => {}
        }
    }

    api.discard_realtime_media(&authorization.session_id, generation);
    report
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
            report_route_revocation(
                socket, authorization, &mut outbound_sequence, true, OUTPUT_REVOKED,
            ).await;
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
                        Ok(TalkClientFrame { kind: TalkClientFrameKind::State { .. } | TalkClientFrameKind::PlaybackAck { .. } | TalkClientFrameKind::InputGateAck { .. }, .. }) => {}
                        Ok(TalkClientFrame { kind: TalkClientFrameKind::Interrupt { .. }, .. }) => {
                            report.interruptions = report.interruptions.saturating_add(1);
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
                report.interruptions = report.interruptions.saturating_add(1);
                let _ = send_output_frame(socket, authorization, &mut outbound_sequence, TalkServerFrameKind::State { state: TalkState::Interrupted }).await;
            }
            "speak_text" => {
                let Some(text) = event.payload.get("text").and_then(serde_json::Value::as_str) else { continue; };
                let job_id = event.payload.get("job_id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                let turn_id = event.payload.get("turn_id").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
                // Time to first audio is measured from the moment the answer is
                // handed to this socket, so synthesis is inside it: a slow
                // synthesizer and a slow speaker are the same silence to
                // whoever is waiting in the room.
                let began = tokio::time::Instant::now();
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
                    // Per chunk, not per answer: an operator can withdraw the
                    // grant between two chunks of the same sentence, and the
                    // ack wait below only notices after the chunk has already
                    // left. One more chunk of somebody's conversation reaching
                    // a speaker that was just taken away is the whole point of
                    // the revocation.
                    if !api.talk_output_route_live(authorization) {
                        report.grant_revoked = true;
                        report_route_revocation(
                            socket, authorization, &mut outbound_sequence, true, OUTPUT_REVOKED,
                        ).await;
                        playback_error = Some(OUTPUT_REVOKED.to_string());
                        break;
                    }
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
                    if chunk_index == 0 {
                        report.latency.tts_first_audio.observe(elapsed_ms(began));
                    }
                    // Backpressure is per playable chunk: a slow phone can hold
                    // at most the current bounded frame, never an unbounded TTS
                    // response queued in host memory.
                    match output_wait_for_ack(
                        socket, api, authorization, &mut inbound, &mut outbound_sequence,
                        audio_sequence, &mut cursor, &mut backlog,
                    ).await {
                        Ok(AckOutcome::Played) => {}
                        Ok(AckOutcome::Interrupted) => {
                            report.interruptions = report.interruptions.saturating_add(1);
                            playback_error = Some("Playback was interrupted".to_string());
                            break;
                        }
                        Ok(AckOutcome::Failed) => {
                            report.errors = report.errors.saturating_add(1);
                            playback_error =
                                Some("The paired speaker could not play this chunk".to_string());
                            break;
                        }
                        Ok(AckOutcome::Revoked) => {
                            report.grant_revoked = true;
                            report_route_revocation(
                                socket, authorization, &mut outbound_sequence, true, OUTPUT_REVOKED,
                            ).await;
                            playback_error = Some(OUTPUT_REVOKED.to_string());
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
                    // The answer existed as text and was not spoken, which is
                    // exactly what `fallbacks` counts on the desktop half — the
                    // routed speaker failing is the same outcome to the person
                    // in the room as a synthesizer that could not run.
                    report.fallbacks = report.fallbacks.saturating_add(1);
                    let _ = api.append_talk_route_event(
                        &authorization.session_id, generation, "output_failed",
                        &serde_json::json!({"job_id": job_id, "turn_id": turn_id, "error": error}),
                    );
                    // A revoked speaker leaves the loop here rather than at the
                    // next iteration: the device has already been told, and the
                    // ack it may still send for the abandoned chunk is never
                    // read, so it cannot be recorded as played.
                    if report.stream_dropped || report.grant_revoked { break; }
                } else {
                    report.latency.end_to_end.observe(elapsed_ms(began));
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
    let realtime_route = authorization.route_engine.as_deref() == Some("realtime");
    let output_only = authorization.route_role.as_deref() == Some("output");
    let configured = ConfiguredTalkSpeech {
        app_data_dir: api.app_data_dir_for_talk(),
    };
    let injected = api.talk_speech();
    let speech: &dyn TalkSpeech = match injected.as_deref() {
        Some(speech) => speech,
        None => &configured,
    };
    let mut report = if realtime_route {
        let input_role = matches!(authorization.route_role.as_deref(), Some("input" | "duplex"));
        let capture = if input_role {
            api.open_talk_capture(
                &authorization.device_id,
                &authorization.session_id,
                started_ms.saturating_add(MAX_SESSION_MS),
            )
        } else {
            None
        };
        let report = run_realtime_route_session(&mut socket, &api, &authorization).await;
        if let Some(command_id) = capture {
            let ended = if report.grant_revoked {
                Some("The routed Realtime microphone authority was withdrawn while the socket was open.")
            } else if report.stream_dropped {
                Some("The routed Realtime Talk socket stopped carrying PCM.")
            } else {
                None
            };
            api.close_talk_capture(&authorization.device_id, &command_id, ended);
        }
        report
    } else if output_only {
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

/// The routed halves of this module, driven through their own seams.
///
/// `serve` above needs a real upgraded TLS connection and is exercised from the
/// socket tests in `api.rs`; everything *inside* it — the two loops that carry
/// all routed audio — needs only a scripted device and the real store, and that
/// is what this exercises. The store is real on purpose: route liveness, route
/// generations and the coordination ledger are the authority these loops obey,
/// and a fake of them would prove nothing about the rules they enforce.
#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::daemon::remote::protocol::{
        random_token, DeviceCapability, DeviceConstraints, DeviceReadiness, DeviceSurface,
        OsPermission, RemoteAction, RemoteHostConfig, RemoteScopes, MAX_TALK_AUDIO_BYTES,
        REMOTE_PROTOCOL_VERSION,
    };
    use crate::daemon::remote::store::{RemoteSecretStore, RemoteStore};
    use crate::daemon::store::DaemonPaths;

    const SESSION_ID: &str = "talk-session-one";

    #[derive(Default)]
    struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl RemoteSecretStore for FakeSecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing secret".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    struct Fixture {
        root: PathBuf,
        api: RemoteApi,
        device_id: String,
        route_id: String,
        generation: u64,
        session_generation: String,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn authorization(&self, role: &str, engine: &str) -> TalkSocketAuthorization {
            TalkSocketAuthorization {
                device_id: self.device_id.clone(),
                signed_request_sha256: "a".repeat(64),
                session_id: SESSION_ID.to_string(),
                session_generation: self.session_generation.clone(),
                route_id: Some(self.route_id.clone()),
                route_generation: Some(self.generation),
                route_role: Some(role.to_string()),
                route_engine: Some(engine.to_string()),
                route_output_to_socket: true,
                route_event_cursor: 0,
            }
        }

        fn append(&self, kind: &str, payload: serde_json::Value) {
            self.api
                .append_talk_route_event(SESSION_ID, self.generation, kind, &payload)
                .expect("the fixture's own coordination event must be accepted");
        }

        fn events(&self) -> Vec<super::super::store::VoiceRouteEventRecord> {
            self.api.talk_route_events(SESSION_ID, 0).unwrap()
        }

        fn kinds(&self) -> Vec<String> {
            self.events().into_iter().map(|event| event.kind).collect()
        }
    }

    fn fixture(engine: &str) -> Fixture {
        let root =
            std::env::temp_dir().join(format!("little-monkey-talk-socket-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 64 * 1024,
        };
        let capabilities = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DeviceInfo,
            DeviceCapability::MicrophoneCapture,
            DeviceCapability::VoiceStream,
            DeviceCapability::AudioPlayback,
        ]);
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
            .unwrap();
        let device_id = store
            .accept_invitation(
                &invite.pairing_id,
                &invite.token,
                "phone",
                "runner-one",
                1_100,
                secrets.as_ref(),
            )
            .unwrap()
            .device_id;
        let physical = BTreeSet::from([
            DeviceCapability::DeviceInfo,
            DeviceCapability::MicrophoneCapture,
            DeviceCapability::VoiceStream,
            DeviceCapability::AudioPlayback,
        ]);
        let surface = DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: "android".into(),
            platform_version: "15".into(),
            app_version: "1.3.0".into(),
            device_model: "Pixel".into(),
            capabilities: physical.clone(),
            permissions: BTreeMap::from([
                (DeviceCapability::DeviceInfo, OsPermission::NotRequired),
                (DeviceCapability::AudioPlayback, OsPermission::NotRequired),
                (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
                (DeviceCapability::VoiceStream, OsPermission::Granted),
            ]),
            readiness: physical
                .iter()
                .map(|capability| (*capability, DeviceReadiness::Ready))
                .collect(),
            constraints: DeviceConstraints::default(),
            reported_at_ms: 1_200,
        };
        store.save_device_surface(&device_id, &surface, 1_200).unwrap();
        let route = store
            .replace_voice_route(
                SESSION_ID,
                engine,
                &format!("paired:{device_id}:input"),
                &format!("paired:{device_id}:output"),
                Some("dcmd-input"),
                Some("dcmd-output"),
                1_300,
            )
            .unwrap();
        Fixture {
            root,
            route_id: route.route_id.clone(),
            generation: route.generation,
            device_id,
            session_generation: random_token(18).unwrap(),
            api: RemoteApi::injected(paths, host, store, secrets),
        }
    }

    /// A synthesizer that always returns the same WAV, so a test decides how
    /// many bounded chunks one answer becomes.
    struct FixedWavSpeech {
        wav: Vec<u8>,
    }

    #[async_trait]
    impl TalkSpeech for FixedWavSpeech {
        async fn transcribe(&self, _audio: Vec<u8>, _media_type: &str) -> Result<String, String> {
            Err("an output-only route never transcribes".to_string())
        }
        async fn synthesize(&self, _text: &str) -> Result<(Vec<u8>, String), String> {
            Ok((self.wav.clone(), "audio/wav".to_string()))
        }
    }

    /// Mono 16-bit PCM at 24 kHz with `data_bytes` of payload. Real enough for
    /// the splitter, which reads `fmt `, `data` and the block alignment.
    fn wav(data_bytes: usize) -> Vec<u8> {
        let mut wav = Vec::with_capacity(44 + data_bytes);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&u32::try_from(36 + data_bytes).unwrap().to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&24_000u32.to_le_bytes());
        wav.extend_from_slice(&48_000u32.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&u32::try_from(data_bytes).unwrap().to_le_bytes());
        wav.resize(44 + data_bytes, 0x11);
        wav
    }

    /// How the scripted device answers the audio it is sent.
    enum Ack {
        /// Every chunk played.
        Played,
        /// Nothing is ever acknowledged — a phone that went silent.
        Silent,
        /// This chunk failed; every other chunk played.
        Failed(u64),
    }

    /// The paired speaker, scripted.
    ///
    /// It answers output audio the way a device does — one playback ack per
    /// chunk — and keeps the exact order in which frames crossed the socket,
    /// which is what the backpressure claims below are actually about.
    struct FakeDevice {
        inbound: VecDeque<(String, String)>,
        sent: Vec<TalkServerFrame>,
        trace: Vec<String>,
        session_id: String,
        session_generation: String,
        next_client_sequence: u64,
        ack: Ack,
        /// Idle polls left before the device hangs up, so no test can hang.
        idle_budget: u32,
        /// Runs once, when the first output audio frame arrives: how a test
        /// makes the world change mid-stream.
        on_first_audio: Option<Box<dyn FnMut() + Send>>,
        /// An acknowledgement this device only sends once it has been told to
        /// stop — the race a late ack for an abandoned answer really is.
        late_ack: Option<u64>,
    }

    impl FakeDevice {
        fn new(authorization: &TalkSocketAuthorization, ack: Ack) -> Self {
            Self {
                inbound: VecDeque::new(),
                sent: Vec::new(),
                trace: Vec::new(),
                session_id: authorization.session_id.clone(),
                session_generation: authorization.session_generation.clone(),
                next_client_sequence: 0,
                ack,
                idle_budget: 20,
                on_first_audio: None,
                late_ack: None,
            }
        }

        fn encode(&mut self, kind: TalkClientFrameKind) -> String {
            self.next_client_sequence = self.next_client_sequence.saturating_add(1);
            serde_json::to_string(&TalkClientFrame {
                protocol_version: TALK_PROTOCOL_VERSION,
                session_id: self.session_id.clone(),
                session_generation: self.session_generation.clone(),
                frame_sequence: self.next_client_sequence,
                kind,
            })
            .unwrap()
        }

        fn queue(&mut self, label: &str, kind: TalkClientFrameKind) {
            let raw = self.encode(kind);
            self.inbound.push_back((label.to_string(), raw));
        }

        fn queue_raw(&mut self, label: &str, raw: String) {
            self.inbound.push_back((label.to_string(), raw));
        }

        fn hello(&mut self, media_type: &str, sample_rate_hz: u32) {
            self.queue(
                "hello",
                TalkClientFrameKind::Hello {
                    media_type: media_type.to_string(),
                    sample_rate_hz,
                    channels: 1,
                },
            );
        }

        fn output_audio(&self) -> Vec<&TalkServerFrameKind> {
            self.sent
                .iter()
                .map(|frame| &frame.kind)
                .filter(|kind| matches!(kind, TalkServerFrameKind::OutputAudio { .. }))
                .collect()
        }

        fn states(&self) -> Vec<TalkState> {
            self.sent
                .iter()
                .filter_map(|frame| match frame.kind {
                    TalkServerFrameKind::State { state } => Some(state),
                    _ => None,
                })
                .collect()
        }

        /// Only the audio and the acks, in the order they crossed the wire.
        fn audio_trace(&self) -> Vec<String> {
            self.trace
                .iter()
                .filter(|entry| entry.starts_with("audio:") || entry.starts_with("ack:"))
                .cloned()
                .collect()
        }
    }

    #[async_trait]
    impl TalkSocket for FakeDevice {
        async fn recv(&mut self) -> Option<String> {
            loop {
                if let Some((label, raw)) = self.inbound.pop_front() {
                    self.trace.push(label);
                    return Some(raw);
                }
                if self.idle_budget == 0 {
                    return None;
                }
                self.idle_budget -= 1;
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        }

        async fn send(&mut self, frame: String) -> Result<(), String> {
            let frame: TalkServerFrame = serde_json::from_str(&frame)
                .map_err(|error| format!("the runner sent an unparsable frame: {error}"))?;
            frame
                .validate()
                .map_err(|error| format!("the runner sent an invalid frame: {error}"))?;
            if let TalkServerFrameKind::OutputAudio { audio_sequence, .. } = frame.kind {
                self.trace.push(format!("audio:{audio_sequence}"));
                if let Some(mut hook) = self.on_first_audio.take() {
                    hook();
                }
                let played = match self.ack {
                    Ack::Played => Some(true),
                    Ack::Failed(sequence) => Some(sequence != audio_sequence),
                    Ack::Silent => None,
                };
                if let Some(played) = played {
                    self.queue(
                        &format!("ack:{audio_sequence}"),
                        TalkClientFrameKind::PlaybackAck {
                            audio_sequence,
                            played,
                        },
                    );
                }
            }
            if let (TalkServerFrameKind::State { state: TalkState::Interrupted }, Some(audio_sequence)) =
                (&frame.kind, self.late_ack.take())
            {
                self.queue(
                    &format!("ack:{audio_sequence}"),
                    TalkClientFrameKind::PlaybackAck {
                        audio_sequence,
                        played: true,
                    },
                );
            }
            self.sent.push(frame);
            Ok(())
        }
    }

    /// **One synthesized answer reaches a paired speaker as ordered, bounded,
    /// individually acknowledged chunks — and the next one never leaves before
    /// the last one landed.**
    ///
    /// The defects this would have caught are the ones that make routed
    /// playback silently wrong rather than broken: a chunk index or count
    /// recomputed per frame (so the device cannot tell where a response ends),
    /// a response id minted per chunk (so a device cannot discard a superseded
    /// answer), a missing route generation (so audio from a moved route plays),
    /// and — the expensive one — sending the whole response ahead of the acks,
    /// which puts an unbounded TTS answer in a phone's memory.
    #[tokio::test]
    async fn a_routed_answer_is_streamed_as_ordered_bounded_chunks_one_acknowledgement_at_a_time() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let speak_event_id = fixture.events().last().unwrap().event_id;
        let mut device = FakeDevice::new(&authorization, Ack::Played);
        device.hello("audio/wav", 24_000);
        // Two full frames and a remainder: enough that chunk_index, chunk_count
        // and the ordering all have something to be wrong about.
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        let mut seen = Vec::new();
        for kind in device.output_audio() {
            let TalkServerFrameKind::OutputAudio {
                audio_sequence,
                response_id,
                chunk_index,
                chunk_count,
                route_generation,
                media_type,
                audio_base64,
            } = kind
            else {
                unreachable!("filtered to output audio above")
            };
            let bytes = STANDARD.decode(audio_base64).expect("audio is base64");
            assert!(
                bytes.len() <= MAX_TALK_AUDIO_BYTES && bytes.starts_with(b"RIFF"),
                "every chunk must be a playable bounded container, got {} bytes",
                bytes.len(),
            );
            assert_eq!(media_type, "audio/wav");
            assert_eq!(*route_generation, Some(fixture.generation));
            assert_eq!(*chunk_count, 3);
            assert_eq!(
                response_id,
                &format!("route-{}-{speak_event_id}", fixture.generation),
                "every chunk of one answer carries the same response id",
            );
            seen.push((*audio_sequence, *chunk_index));
        }
        assert_eq!(seen, vec![(1, 0), (2, 1), (3, 2)]);
        assert_eq!(
            device.audio_trace(),
            vec!["audio:1", "ack:1", "audio:2", "ack:2", "audio:3", "ack:3"],
            "a chunk may only leave after the previous one was acknowledged",
        );
        assert!(
            device.states().contains(&TalkState::Speaking)
                && device.states().last() == Some(&TalkState::Idle),
            "the device is told when the answer starts and when it is over: {:?}",
            device.states(),
        );
        assert!(
            fixture.kinds().contains(&"output_played".to_string())
                && !fixture.kinds().contains(&"output_failed".to_string()),
            "a fully acknowledged answer is recorded as played: {:?}",
            fixture.kinds(),
        );
        assert_eq!(
            (report.spoken_chunks, report.errors, report.stream_dropped),
            (1, 0, false),
        );
    }

    /// **A chunk that is never acknowledged stops the stream instead of
    /// emptying the rest of the answer into the socket.**
    ///
    /// The failure this guards is the one backpressure exists for: without the
    /// per-chunk wait, a silent phone still receives every megabyte of the
    /// response, and the host learns nothing about whether any of it played.
    #[tokio::test]
    async fn an_unacknowledged_chunk_stops_the_answer_rather_than_draining_it_into_the_socket() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let mut device = FakeDevice::new(&authorization, Ack::Silent);
        device.hello("audio/wav", 24_000);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        assert_eq!(
            device.output_audio().len(),
            1,
            "only the first chunk may be in flight while it is unacknowledged",
        );
        assert!(report.stream_dropped, "a device that stopped answering ended the stream");
        assert_eq!(report.fallbacks, 1, "the answer existed and was not spoken");
        assert!(
            !fixture.kinds().contains(&"output_played".to_string()),
            "nothing may be recorded as played: {:?}",
            fixture.kinds(),
        );
    }

    /// **A speaker that reports a chunk as failed ends that answer, records
    /// `output_failed`, and leaves the conversation open.**
    ///
    /// Two defects in one claim. Hanging on a negative ack would hold the
    /// socket for the full 90-second ack deadline with nothing to wait for; and
    /// continuing to stream into a speaker that just said it cannot play is how
    /// a whole answer is lost silently.
    #[tokio::test]
    async fn a_negative_playback_ack_ends_the_answer_and_is_recorded_without_ending_the_session() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let mut device = FakeDevice::new(&authorization, Ack::Failed(1));
        device.hello("audio/wav", 24_000);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        assert_eq!(device.output_audio().len(), 1, "the stream stops at the failed chunk");
        assert!(
            !report.stream_dropped,
            "one chunk the speaker could not play is not a dead socket",
        );
        assert_eq!((report.errors, report.fallbacks), (1, 1));
        let failure = fixture
            .events()
            .into_iter()
            .find(|event| event.kind == "output_failed")
            .expect("a failed answer is recorded for the desktop half");
        assert_eq!(failure.payload.get("turn_id").unwrap(), "turn-1");
        assert!(
            failure
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|error| error.contains("could not play")),
            "the recorded reason must say what happened: {:?}",
            failure.payload,
        );
    }

    /// **The Realtime path answers a negative playback ack the same way the
    /// pipeline path does: the answer stops, the queue is dropped, the socket
    /// stays open.**
    ///
    /// It did not. A `played: false` ack tore the whole Realtime socket down,
    /// so one chunk a speaker could not play ended the conversation and the
    /// phone had to reconnect — while the identical event on the pipeline route
    /// merely ended that answer. This test fails against that behaviour: the
    /// interrupt queued behind the negative ack is never seen by a loop that
    /// broke out on the ack.
    #[tokio::test]
    async fn a_negative_realtime_playback_ack_ends_the_answer_but_not_the_realtime_socket() {
        let fixture = fixture("realtime");
        let authorization = fixture.authorization("output", "realtime");
        fixture
            .api
            .push_realtime_output_from_host(SESSION_ID, fixture.generation, vec![0x01, 0x02, 0x03, 0x04])
            .unwrap();
        let mut device = FakeDevice::new(&authorization, Ack::Silent);
        device.hello(REALTIME_PCM_MEDIA_TYPE, 24_000);
        device.queue(
            "nack",
            TalkClientFrameKind::PlaybackAck {
                audio_sequence: 1,
                played: false,
            },
        );
        device.queue(
            "interrupt",
            TalkClientFrameKind::Interrupt {
                reason: Some("still here".to_string()),
            },
        );

        let report = run_realtime_route_session(&mut device, &fixture.api, &authorization).await;

        assert_eq!(device.output_audio().len(), 1);
        assert_eq!(
            (report.errors, report.fallbacks, report.interruptions),
            (1, 1, 1),
            "the failed chunk is counted, and the socket lived long enough to see the interrupt",
        );
        let kinds = fixture.kinds();
        assert!(
            kinds.contains(&"output_failed".to_string()) && kinds.contains(&"interrupt".to_string()),
            "the failed chunk and the later interrupt are both recorded: {kinds:?}",
        );
        assert!(
            !kinds.contains(&"output_played".to_string()),
            "a chunk the speaker refused is never recorded as played: {kinds:?}",
        );
    }

    /// **An `output_stop` drops the audio already queued for the old response,
    /// tells the device it was interrupted, and refuses the acks that arrive
    /// afterwards for it.**
    ///
    /// Barge-in is only real if the queued audio dies with it. Without the
    /// clear, the phone keeps playing the sentence the user spoke over; without
    /// forgetting the in-flight acks, a late ack for the abandoned response is
    /// recorded as played and the desktop believes the interrupted answer was
    /// heard.
    #[tokio::test]
    async fn an_output_stop_clears_queued_realtime_audio_and_refuses_the_stopped_response_acks() {
        let fixture = fixture("realtime");
        let authorization = fixture.authorization("output", "realtime");
        // One more chunk than the socket will hold in flight, so there is
        // always audio still queued host-side when the interruption lands.
        for _ in 0..9 {
            fixture
                .api
                .push_realtime_output_from_host(SESSION_ID, fixture.generation, vec![0x01, 0x02])
                .unwrap();
        }
        let mut device = FakeDevice::new(&authorization, Ack::Silent);
        device.hello(REALTIME_PCM_MEDIA_TYPE, 24_000);
        device.late_ack = Some(1);
        let api = fixture.api.clone();
        let generation = fixture.generation;
        device.on_first_audio = Some(Box::new(move || {
            api.append_talk_route_event(SESSION_ID, generation, "output_stop", &serde_json::json!({}))
                .unwrap();
        }));

        let report = run_realtime_route_session(&mut device, &fixture.api, &authorization).await;

        assert_eq!(
            device.output_audio().len(),
            8,
            "in-flight output is bounded, and the chunk still queued behind the interruption \
             must never be sent",
        );
        assert!(
            device.states().contains(&TalkState::Interrupted),
            "the device is told to stop playing: {:?}",
            device.states(),
        );
        assert_eq!(report.interruptions, 1);
        assert!(
            !fixture.kinds().contains(&"output_played".to_string()),
            "an ack that arrives after the stop belongs to an abandoned answer: {:?}",
            fixture.kinds(),
        );
    }

    /// **A routed socket refuses a replayed frame sequence, a rewound audio
    /// sequence, and a frame naming another session — each on its own.**
    ///
    /// These three are the whole inbound discipline of the routed paths, and
    /// each one is a distinct attack: replay a frame, reorder audio, or steer a
    /// frame at a conversation this socket was never admitted to.
    #[test]
    fn routed_frames_are_refused_when_replayed_rewound_or_addressed_to_another_session() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        let frame = |sequence: u64, kind: TalkClientFrameKind| TalkClientFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: authorization.session_id.clone(),
            session_generation: authorization.session_generation.clone(),
            frame_sequence: sequence,
            kind,
        };
        let audio = |sequence: u64| TalkClientFrameKind::Audio {
            audio_sequence: sequence,
            media_type: "audio/wav".to_string(),
            audio_base64: STANDARD.encode([0x11, 0x22]),
            last: false,
            utterance_id: None,
        };
        let mut tracker = TalkSequenceTracker::default();
        let raw = |frame: &TalkClientFrame| serde_json::to_string(frame).unwrap();

        parse_output_client_frame(&raw(&frame(1, audio(1))), &authorization, &mut tracker)
            .expect("the first frame is accepted");
        let replayed =
            parse_output_client_frame(&raw(&frame(1, audio(2))), &authorization, &mut tracker)
                .expect_err("a replayed frame sequence is refused");
        assert!(replayed.contains("frame sequence"), "{replayed}");
        let rewound =
            parse_output_client_frame(&raw(&frame(2, audio(1))), &authorization, &mut tracker)
                .expect_err("an audio sequence that does not advance is refused");
        assert!(rewound.contains("audio sequence"), "{rewound}");

        let mut foreign = frame(3, audio(2));
        foreign.session_id = "talk-session-two".to_string();
        let refused =
            parse_output_client_frame(&raw(&foreign), &authorization, &mut tracker)
                .expect_err("a frame naming another session is refused");
        assert!(refused.contains("different session"), "{refused}");
        let mut moved = frame(3, audio(2));
        moved.session_generation = random_token(18).unwrap();
        assert!(
            parse_output_client_frame(&raw(&moved), &authorization, &mut tracker).is_err(),
            "a frame from an earlier session generation is refused",
        );
        // And the tracker was not advanced by any refused frame.
        parse_output_client_frame(&raw(&frame(2, audio(2))), &authorization, &mut tracker)
            .expect("the next legitimate frame still fits the tracker");
    }

    /// **A refused inbound frame ends the routed socket with a typed error
    /// rather than being ignored.**
    ///
    /// The branch above is only worth having if the loop actually reaches it:
    /// a parse failure that fell through to the catch-all would leave a device
    /// replaying frames at a socket that answers nothing.
    #[tokio::test]
    async fn a_replayed_frame_ends_the_realtime_socket_with_a_typed_error() {
        let fixture = fixture("realtime");
        let authorization = fixture.authorization("output", "realtime");
        let mut device = FakeDevice::new(&authorization, Ack::Played);
        device.hello(REALTIME_PCM_MEDIA_TYPE, 24_000);
        // The hello was frame one; this replays that sequence number.
        let replay = serde_json::to_string(&TalkClientFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: authorization.session_id.clone(),
            session_generation: authorization.session_generation.clone(),
            frame_sequence: 1,
            kind: TalkClientFrameKind::State {
                state: TalkState::Idle,
            },
        })
        .unwrap();
        device.queue_raw("replay", replay);

        let report = run_realtime_route_session(&mut device, &fixture.api, &authorization).await;

        assert_eq!(report.errors, 1);
        let error = device
            .sent
            .iter()
            .find_map(|frame| match &frame.kind {
                TalkServerFrameKind::Error { code, message, .. } => Some((code.clone(), message.clone())),
                _ => None,
            })
            .expect("the device is told why its frame was refused");
        assert_eq!(error.0, "invalid_realtime_frame");
        assert!(error.1.contains("frame sequence"), "{}", error.1);
    }

    /// **A route generation that moves while the socket is streaming ends the
    /// session instead of finishing the answer into a speaker that is no longer
    /// the route's.**
    ///
    /// The authority is host-side and per generation: once the conversation is
    /// pointed at a different speaker, the chunks already synthesized for the
    /// old one must not be delivered. A liveness check made only at admission
    /// would send all of them.
    #[tokio::test]
    async fn a_route_generation_that_moves_mid_answer_ends_the_established_socket() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let mut device = FakeDevice::new(&authorization, Ack::Played);
        device.hello("audio/wav", 24_000);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };
        let api = fixture.api.clone();
        let device_id = fixture.device_id.clone();
        device.on_first_audio = Some(Box::new(move || {
            let store = api.store_for_tests();
            let mut store = store.lock().unwrap();
            store
                .replace_voice_route(
                    SESSION_ID,
                    "pipeline",
                    &format!("paired:{device_id}:input"),
                    "local:output:default",
                    Some("dcmd-input"),
                    Some("dcmd-output"),
                    9_000,
                )
                .unwrap();
        }));

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        assert_eq!(
            device.output_audio().len(),
            1,
            "the rest of the answer belongs to a route this socket no longer holds",
        );
        assert!(
            report.grant_revoked,
            "a route that moved took this socket's authority with it — that is not a dead socket",
        );
        assert!(
            device.sent.iter().any(|frame| matches!(
                &frame.kind,
                TalkServerFrameKind::Error { code, retryable: false, .. } if code == "capability_revoked"
            )),
            "and the speaker is told, rather than left to retry a route it no longer holds",
        );
        assert!(
            !fixture.kinds().contains(&"output_played".to_string()),
            "nothing from the stale generation may be recorded as played: {:?}",
            fixture.kinds(),
        );
    }

    /// **The counters a finished route session leaves behind describe the
    /// session that actually happened.**
    ///
    /// These are what reaches the audit row — the only trace of a routed
    /// conversation, since nothing said aloud may be written down. An
    /// interruption that is never counted, or a time-to-first-audio nobody
    /// measured, makes a routed session invisible exactly when it is going
    /// wrong.
    #[tokio::test]
    async fn a_finished_route_session_reports_the_interruptions_and_latency_it_actually_had() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let mut device = FakeDevice::new(&authorization, Ack::Played);
        device.hello("audio/wav", 24_000);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };
        let api = fixture.api.clone();
        let generation = fixture.generation;
        device.on_first_audio = Some(Box::new(move || {
            api.append_talk_route_event(SESSION_ID, generation, "output_stop", &serde_json::json!({}))
                .unwrap();
        }));

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        assert_eq!(
            (report.interruptions, report.fallbacks, report.spoken_chunks),
            (1, 1, 0),
            "one answer was interrupted and none was spoken through",
        );
        assert_eq!(
            report.latency.tts_first_audio.samples, 1,
            "time to first routed audio is measured once per answer",
        );
        assert_eq!(
            report.latency.end_to_end.samples, 0,
            "an answer that never finished has no end-to-end span",
        );
        assert!(
            device.states().contains(&TalkState::Interrupted),
            "{:?}",
            device.states(),
        );
    }

    /// The capability set this fixture's device keeps once `audio_playback` is
    /// taken away — everything it was paired with, minus the speaker.
    fn without_audio_playback() -> BTreeSet<DeviceCapability> {
        BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DeviceInfo,
            DeviceCapability::MicrophoneCapture,
            DeviceCapability::VoiceStream,
        ])
    }

    /// **Withdrawing `audio_playback` while a routed answer is streaming tells
    /// the paired speaker why, stops the stream at the chunk in flight, and
    /// never records the abandoned answer as played.**
    ///
    /// The asymmetry this catches: a withdrawn `voice_stream` grant reaches a
    /// microphone as a non-retryable `capability_revoked` error before the
    /// socket closes, while a withdrawn `audio_playback` grant merely broke the
    /// loop and let `serve` close the socket. A speaker could not tell an
    /// operator's decision from a dropped connection, so it reconnected and
    /// retried a route it is no longer allowed to hold — and it kept the chunk
    /// it had not acknowledged, whose late ack would then have been counted as
    /// the interrupted answer having been heard.
    #[tokio::test]
    async fn revoking_audio_playback_mid_answer_tells_the_paired_speaker_and_stops_the_stream() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        // Silent until it is told to stop, then one acknowledgement for the
        // chunk it was holding: exactly the late ack a revoked answer races.
        let mut device = FakeDevice::new(&authorization, Ack::Silent);
        device.hello("audio/wav", 24_000);
        device.late_ack = Some(1);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };
        let api = fixture.api.clone();
        let device_id = fixture.device_id.clone();
        device.on_first_audio = Some(Box::new(move || {
            let store = api.store_for_tests();
            let mut store = store.lock().unwrap();
            store
                .set_device_capabilities(&device_id, &without_audio_playback(), 9_000)
                .unwrap();
        }));

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        assert_eq!(
            device.output_audio().len(),
            1,
            "the rest of the answer belongs to a speaker the operator just took away",
        );
        let error = device
            .sent
            .iter()
            .find_map(|frame| match &frame.kind {
                TalkServerFrameKind::Error { code, message, retryable } => {
                    Some((code.clone(), message.clone(), *retryable))
                }
                _ => None,
            })
            .expect("a revoked speaker is told why its socket is ending");
        assert_eq!(error.0, "capability_revoked");
        assert!(
            error.1.contains("audio_playback") && !error.2,
            "the reason names the withdrawn grant and must not invite a retry: {error:?}",
        );
        assert!(
            device.states().contains(&TalkState::Interrupted)
                && device.states().last() == Some(&TalkState::Idle),
            "the device is told to drop what it is holding and then to stand down: {:?}",
            device.states(),
        );
        assert_eq!(
            (report.grant_revoked, report.stream_dropped, report.spoken_chunks, report.fallbacks),
            (true, false, 0, 1),
            "a revoked grant is not a dropped socket, and the answer was not spoken",
        );
        assert!(
            !fixture.kinds().contains(&"output_played".to_string()),
            "the ack that arrives after the revocation belongs to an abandoned answer: {:?}",
            fixture.kinds(),
        );
        let failure = fixture
            .events()
            .into_iter()
            .find(|event| event.kind == "output_failed")
            .expect("the desktop half learns the routed answer was never spoken");
        assert!(
            failure
                .payload
                .get("error")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|error| error.contains("audio_playback")),
            "the recorded reason must say the grant went away: {:?}",
            failure.payload,
        );
    }

    /// **A Realtime speaker whose `audio_playback` grant is withdrawn is told
    /// the same thing, and the PCM still queued for it is dropped rather than
    /// handed to the next socket.**
    ///
    /// Realtime output is a host-side queue, so silence is not enough: a device
    /// that reconnects after a revocation must not be able to collect the
    /// seconds of conversation that were waiting for it, and the chunk it was
    /// holding when the grant went must not come back as an acknowledgement the
    /// host records as played.
    #[tokio::test]
    async fn revoking_audio_playback_under_a_realtime_speaker_tells_it_and_drops_the_queued_pcm() {
        let fixture = fixture("realtime");
        let authorization = fixture.authorization("output", "realtime");
        for _ in 0..9 {
            fixture
                .api
                .push_realtime_output_from_host(SESSION_ID, fixture.generation, vec![0x01, 0x02])
                .unwrap();
        }
        let mut device = FakeDevice::new(&authorization, Ack::Silent);
        device.hello(REALTIME_PCM_MEDIA_TYPE, 24_000);
        device.late_ack = Some(1);
        let api = fixture.api.clone();
        let device_id = fixture.device_id.clone();
        device.on_first_audio = Some(Box::new(move || {
            let store = api.store_for_tests();
            let mut store = store.lock().unwrap();
            store
                .set_device_capabilities(&device_id, &without_audio_playback(), 9_000)
                .unwrap();
        }));

        let report = run_realtime_route_session(&mut device, &fixture.api, &authorization).await;

        assert_eq!(
            device.output_audio().len(),
            1,
            "not one chunk more may be taken from the queue once the grant is gone",
        );
        assert!(report.grant_revoked);
        let error = device
            .sent
            .iter()
            .find_map(|frame| match &frame.kind {
                TalkServerFrameKind::Error { code, retryable, .. } => Some((code.clone(), *retryable)),
                _ => None,
            })
            .expect("a revoked Realtime speaker is told why its socket is ending");
        assert_eq!(error, ("capability_revoked".to_string(), false));
        assert!(
            device.states().contains(&TalkState::Interrupted),
            "the device is told to drop the PCM it is holding: {:?}",
            device.states(),
        );
        assert!(
            !fixture.kinds().contains(&"output_played".to_string()),
            "an ack from the revoked generation is not progress: {:?}",
            fixture.kinds(),
        );
        // Grant it back, which is the only way to ask the host what is left in
        // the queue: nothing, because the revocation dropped it.
        {
            let store = fixture.api.store_for_tests();
            let mut store = store.lock().unwrap();
            let mut restored = without_audio_playback();
            restored.insert(DeviceCapability::AudioPlayback);
            store
                .set_device_capabilities(&fixture.device_id, &restored, 9_100)
                .unwrap();
        }
        assert!(
            fixture
                .api
                .take_realtime_output_for_device(&authorization)
                .expect("the route is live again")
                .is_none(),
            "audio queued under a grant that was withdrawn must never reach the next socket",
        );
    }

    /// **A barge-in from the routed microphone stops the paired speaker
    /// mid-answer, and the conversation goes on to speak the next one.**
    ///
    /// This is the whole remote-to-remote path: the microphone's socket records
    /// `interrupt`, the desktop turns that into the `output_stop` this speaker
    /// obeys. Two defects it would catch — a speaker that plays out the
    /// sentence the user spoke over because it only watches its own socket for
    /// interruptions, and a speaker that treats the interruption as the end of
    /// the conversation and has to be reconnected before the next answer.
    #[tokio::test]
    async fn a_remote_microphone_barge_in_stops_the_answer_and_the_next_one_is_still_spoken() {
        let fixture = fixture("pipeline");
        let authorization = fixture.authorization("output", "pipeline");
        fixture.append(
            "speak_text",
            serde_json::json!({"text": "hello there", "job_id": "job-1", "turn_id": "turn-1"}),
        );
        let mut device = FakeDevice::new(&authorization, Ack::Played);
        device.hello("audio/wav", 24_000);
        let speech = FixedWavSpeech {
            wav: wav(MAX_TALK_AUDIO_BYTES * 2 + 1_000),
        };
        let api = fixture.api.clone();
        let generation = fixture.generation;
        device.on_first_audio = Some(Box::new(move || {
            // What the routed microphone's own socket writes when somebody
            // talks over the answer, and what the desktop relays back.
            api.append_talk_route_event(
                SESSION_ID,
                generation,
                "interrupt",
                &serde_json::json!({"turn_id": "turn-1", "reason": "device_barge_in"}),
            )
            .unwrap();
            api.append_talk_route_event(
                SESSION_ID,
                generation,
                "output_stop",
                &serde_json::json!({"reason": "host_interrupt"}),
            )
            .unwrap();
            api.append_talk_route_event(
                SESSION_ID,
                generation,
                "speak_text",
                &serde_json::json!({"text": "go on then", "job_id": "job-2", "turn_id": "turn-2"}),
            )
            .unwrap();
        }));

        let report =
            run_output_route_session(&mut device, &speech, &fixture.api, &authorization).await;

        let responses = device
            .output_audio()
            .into_iter()
            .filter_map(|kind| match kind {
                TalkServerFrameKind::OutputAudio { response_id, .. } => Some(response_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            responses.len(),
            4,
            "one chunk of the answer that was spoken over, then all three of the next: {responses:?}",
        );
        assert!(
            responses[0] != responses[1]
                && responses[1] == responses[2]
                && responses[2] == responses[3],
            "the interrupted answer stopped and a new response id followed it: {responses:?}",
        );
        assert!(
            device.states().contains(&TalkState::Interrupted),
            "the speaker is told to stop playing what the user spoke over: {:?}",
            device.states(),
        );
        assert_eq!(
            (report.interruptions, report.fallbacks, report.spoken_chunks, report.stream_dropped),
            (1, 1, 1, false),
            "one answer was cut off, the next was spoken through, and the socket lived",
        );
        let played = fixture
            .events()
            .into_iter()
            .find(|event| event.kind == "output_played")
            .expect("the answer after the barge-in is recorded as spoken");
        assert_eq!(played.payload.get("turn_id").unwrap(), "turn-2");
        let failed = fixture
            .events()
            .into_iter()
            .find(|event| event.kind == "output_failed")
            .expect("the answer that was spoken over is recorded as not spoken");
        assert_eq!(failed.payload.get("turn_id").unwrap(), "turn-1");
    }
}

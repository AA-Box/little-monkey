//! The phone conversation itself: carrier audio in, an ordinary Little Monkey
//! turn in the middle, synthesized speech back out.
//!
//! ```text
//! carrier media stream -> µ-law frames -> utterance -> STT
//!   -> durable ingress record -> normal run -> send_message
//!   -> this session -> TTS -> µ-law frames -> carrier
//! ```
//!
//! Two things are deliberately *not* here. There is no phone-only agent: a call
//! turn goes through the same queue, the same route and the same session as a
//! message, and the run answering it cannot tell it is on the phone beyond the
//! text saying so. And there is no phone-only speech stack: transcription and
//! synthesis are the operator's own configured backends, reached through
//! `little_monkey_lib::m7_companion`, so a call cannot quietly use a different
//! provider than the desktop does.
//!
//! # Who may connect
//!
//! A carrier media stream carries no signature, so the URL handed to the
//! carrier carries a token instead: an HMAC over the account and call, keyed by
//! the account's own credential. A socket that cannot present one is closed
//! before a single frame is read, which is what stops anyone who learns the
//! callback URL from streaming audio into a live call.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use tokio::sync::mpsc;

use super::call_audio::{
    decode_mulaw, encode_mulaw, read_wav_as_call_audio, write_wav, UtteranceDetector,
    UtteranceProgress, CALL_SAMPLE_RATE,
};

/// Silence that ends a caller's turn.
const HANGOVER_MS: u32 = 700;
/// How much continuous speech counts as the caller interrupting.
///
/// One loud frame is a door, a cough, or the caller's own audio echoing back
/// off a speakerphone. Requiring it to persist is what tells an interruption
/// apart from a room — and it is the honest version of this without a voice
/// activity detector, which is a model dependency this does not carry.
const BARGE_IN_MS: u32 = 240;
/// The longest single utterance transcribed. A caller who talks past this is
/// answered rather than left talking into a machine that stopped listening.
const MAX_UTTERANCE_MS: u32 = 20_000;

/// What one carrier's media stream is actually shaped like.
///
/// The three carriers are close enough to look interchangeable and different
/// enough that treating them as such produces silence on two of them. What
/// differs is captured here; what differs *more* than a field name — how an
/// outbound frame is spelled — is the carrier's own
/// [`TelecomProvider::encode_media_frame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct MediaStreamFormat {
    /// Where the stream identifier lives in an inbound frame. Plivo nests it
    /// under `start`, the other two put it at the top level, and reading the
    /// wrong one means echoing an empty id back at a carrier that requires it.
    pub stream_id_path: &'static [&'static str],
    /// How much audio goes in one outbound frame. Twilio and Plivo take the
    /// carrier's own 20 ms frame; Telnyx accepts at most one payload per
    /// second, so anything smaller is dropped on the floor.
    pub outbound_chunk_ms: u32,
}

/// A carrier media socket, abstracted so the conversation loop can be driven by
/// a test with no network and no carrier.
#[async_trait]
pub(crate) trait MediaSocket: Send {
    /// Next text frame from the carrier, or `None` when the socket closes.
    async fn recv(&mut self) -> Option<String>;
    async fn send(&mut self, frame: String) -> Result<(), String>;
}

/// The operator's configured speech backends.
#[async_trait]
pub(crate) trait CallSpeech: Send + Sync {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String, String>;
    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String>;
    /// Keep the recording of what was said, returning its artifact id.
    ///
    /// A voicemail is the audio: a transcript of a bad line, or of a name being
    /// spelled out, is not what the operator needs to hear back. The bytes go
    /// to the same content-addressed store every other attachment uses, under
    /// the same size limits.
    async fn keep_audio(&self, wav: Vec<u8>) -> Result<String, String>;
}

/// Where a transcribed caller turn goes.
pub(crate) trait CallTurnSink: Send + Sync {
    /// Hand one caller turn to the agent. Returns the job id it became.
    fn submit_turn(&self, turn: CallTurn<'_>) -> Result<String, String>;
}

/// One thing the caller said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallTurn<'a> {
    pub account_id: &'a str,
    pub call_id: &'a str,
    pub peer_number: &'a str,
    pub session_key: &'a str,
    pub text: &'a str,
    /// Artifact holding the audio of this turn, when it was kept. Voicemail
    /// keeps it; a live conversation does not, because nobody asked for every
    /// call to be recorded.
    pub audio_artifact_id: Option<&'a str>,
    /// Turn number within this call, which is what makes the ingress record's
    /// event id deterministic and therefore deduplicable.
    pub index: u32,
}

/// Live calls that can be spoken to, by call id.
///
/// In-process on purpose: speech is only deliverable while the socket is open,
/// so a durable queue of things to say would mostly hold things that can never
/// be said. The outbox row stays durable; what this map answers is whether the
/// line is still up.
fn speakers() -> &'static Mutex<BTreeMap<String, mpsc::UnboundedSender<String>>> {
    static SPEAKERS: OnceLock<Mutex<BTreeMap<String, mpsc::UnboundedSender<String>>>> =
        OnceLock::new();
    SPEAKERS.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Say something on a live call. `Err` when that call is not on the line — the
/// outbox turns that into a permanent failure rather than retrying, because a
/// call that has ended does not become live again.
pub(crate) fn speak_on_call(call_id: &str, text: &str) -> Result<(), String> {
    let speakers = speakers().lock().map_err(|_| "speaker registry poisoned")?;
    let sender = speakers
        .get(call_id)
        .ok_or_else(|| format!("Call {call_id} is no longer on the line"))?;
    sender
        .send(text.to_string())
        .map_err(|_| format!("Call {call_id} is no longer on the line"))
}

/// Whether a media socket for this call is open right now.
///
/// The same registry [`speak_on_call`] delivers through, asked the other
/// question: not "can I say something" but "is anyone still carrying this
/// call's audio". A carrier that reconnects re-registers here, which is what
/// tells a dropped stream apart from one that came back.
pub(crate) fn is_on_the_line(call_id: &str) -> bool {
    speakers()
        .lock()
        .map(|speakers| speakers.contains_key(call_id))
        .unwrap_or(false)
}

fn register_speaker(call_id: &str) -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    if let Ok(mut speakers) = speakers().lock() {
        speakers.insert(call_id.to_string(), sender);
    }
    receiver
}

/// Register a live call by hand, standing in for the media session a carrier
/// that reconnected would be running.
#[cfg(test)]
pub(crate) fn register_reconnected_call(call_id: &str) -> mpsc::UnboundedReceiver<String> {
    register_speaker(call_id)
}

fn unregister_speaker(call_id: &str) {
    if let Ok(mut speakers) = speakers().lock() {
        speakers.remove(call_id);
    }
}

/// What one finished media session did, for the caller's log and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MediaSessionReport {
    pub turns_submitted: u32,
    pub utterances_transcribed: u32,
    pub frames_spoken: u32,
    /// Times the caller talked over the agent and the rest was dropped.
    pub interruptions: u32,
    /// Voicemail recordings written to the artifact store.
    pub recordings_kept: u32,
    /// The socket broke rather than being closed on purpose: no `stop` event,
    /// no voicemail that had said its piece, just a stream that stopped
    /// carrying audio. It is the difference between a call that ended and a
    /// call nobody can hear any more, and only the second one needs somebody
    /// to go and hang the line up.
    pub stream_dropped: bool,
}

/// Run one call's audio until the carrier hangs up.
///
/// Errors from transcription or synthesis end that turn, not the call: a
/// speech backend that is misconfigured should not drop a line that a person is
/// on. They are reported to the caller's log by the session's owner.
pub(crate) async fn run_media_session(
    socket: &mut dyn MediaSocket,
    speech: &dyn CallSpeech,
    sink: &dyn CallTurnSink,
    carrier: &dyn MediaFrameCodec,
    call: CallIdentity,
) -> MediaSessionReport {
    let format = carrier.format();
    let mut report = MediaSessionReport::default();
    let mut detector = UtteranceDetector::new(HANGOVER_MS, MAX_UTTERANCE_MS);
    let mut to_speak = register_speaker(&call.call_id);
    let mut stream_id = String::new();
    let mut turn_index = 0;
    let mut spoke_opening = call.opening_line.is_none();
    // Consecutive milliseconds of caller speech heard while the agent is
    // talking. Reset by any quiet frame, so only sustained speech interrupts.
    let mut speech_over_us_ms = 0_u32;
    // The greeting is not interrupted: it is who is calling and why, and a
    // caller who talks over "hello" has not been told anything yet.
    let mut greeting_playing = false;
    // Audio waiting to go out, one carrier frame each. Held here rather than
    // written in a loop so the caller can interrupt: a sentence already handed
    // to the socket cannot be taken back.
    let mut pending: std::collections::VecDeque<String> = std::collections::VecDeque::new();

    loop {
        let frame = tokio::select! {
            biased;
            Some(text) = to_speak.recv() => {
                match speech.synthesize(&text).await {
                    Ok(samples) => queue_audio(&mut pending, &samples, carrier, format, &stream_id),
                    Err(error) => eprintln!(
                        "monkey daemon: could not speak on {}: {error}",
                        call.call_id
                    ),
                }
                continue;
            }
            // Speaking and listening happen together. One frame goes out per
            // pass, and the loop comes straight back for whatever the caller
            // is saying while it plays.
            () = tokio::time::sleep(std::time::Duration::from_millis(
                u64::from(format.outbound_chunk_ms).saturating_sub(5).max(1),
            )), if !pending.is_empty() => {
                if let Some(frame) = pending.pop_front() {
                    if socket.send(frame).await.is_err() {
                        report.stream_dropped = true;
                        break;
                    }
                    report.frames_spoken += 1;
                }
                if pending.is_empty() {
                    greeting_playing = false;
                }
                continue;
            }
            frame = socket.recv() => frame,
        };
        let Some(frame) = frame else {
            // The carrier closes with a `stop` event when the call is over.
            // Reaching the end of the socket without one means the stream went
            // away while the call was still live.
            report.stream_dropped = true;
            break;
        };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&frame) else {
            continue;
        };
        if let Some(id) = read_path(&value, format.stream_id_path) {
            stream_id = id;
        }
        match value.get("event").and_then(serde_json::Value::as_str) {
            Some("stop") => break,
            Some("media") => {}
            // start, mark, connected, clear: nothing to hear.
            _ => continue,
        }
        // The greeting goes out on the first audio frame rather than on the
        // start event, because that is the first moment the carrier is
        // demonstrably carrying audio in both directions. A call that opens
        // with silence sounds broken to whoever picked up.
        if !spoke_opening {
            spoke_opening = true;
            if let Some(line) = call.opening_line.as_deref() {
                match speech.synthesize(line).await {
                    Ok(samples) => {
                        greeting_playing = true;
                        queue_audio(&mut pending, &samples, carrier, format, &stream_id);
                    }
                    Err(error) => eprintln!(
                        "monkey daemon: could not speak the opening line on {}: {error}",
                        call.call_id
                    ),
                }
            }
        }
        let Some(payload) = value
            .get("media")
            .and_then(|media| media.get("payload"))
            .and_then(serde_json::Value::as_str)
        else {
            continue;
        };
        let Ok(bytes) = STANDARD.decode(payload) else {
            continue;
        };
        let samples: Vec<i16> = bytes.iter().copied().map(decode_mulaw).collect();
        // Barge-in: somebody talking over the agent means they stopped
        // listening, so the rest of the sentence is dropped here and cleared at
        // the carrier, which is still holding what was already sent. Sustained
        // speech only, and never over the greeting.
        if pending.is_empty() || greeting_playing {
            speech_over_us_ms = 0;
        } else if super::call_audio::contains_speech(&samples) {
            speech_over_us_ms += frame_ms(samples.len());
            if speech_over_us_ms >= BARGE_IN_MS {
                speech_over_us_ms = 0;
                pending.clear();
                report.interruptions += 1;
                if socket
                    .send(carrier.encode_clear_frame(&stream_id))
                    .await
                    .is_err()
                {
                    report.stream_dropped = true;
                    break;
                }
            }
        } else {
            speech_over_us_ms = 0;
        }
        let UtteranceProgress::Complete(utterance) = detector.push(&samples) else {
            continue;
        };
        report.utterances_transcribed += 1;
        let wav = write_wav(&utterance, CALL_SAMPLE_RATE);
        // Voicemail keeps the recording; a conversation does not. Kept before
        // transcription so a message survives even when transcription fails —
        // an unintelligible voicemail is still a voicemail.
        let audio_artifact_id = if call.single_turn {
            match speech.keep_audio(wav.clone()).await {
                Ok(id) => {
                    report.recordings_kept += 1;
                    Some(id)
                }
                Err(error) => {
                    eprintln!(
                        "monkey daemon: could not keep the recording for {}: {error}",
                        call.call_id
                    );
                    None
                }
            }
        } else {
            None
        };
        let text = match speech.transcribe(wav).await {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "monkey daemon: could not transcribe a turn of {}: {error}",
                    call.call_id
                );
                // A voicemail whose audio was kept is still worth handing over,
                // even with no words to go with it.
                if audio_artifact_id.is_none() {
                    continue;
                }
                String::new()
            }
        };
        if text.trim().is_empty() && audio_artifact_id.is_none() {
            continue;
        }
        turn_index += 1;
        match sink.submit_turn(CallTurn {
            account_id: &call.account_id,
            call_id: &call.call_id,
            peer_number: &call.peer_number,
            session_key: &call.session_key,
            text: text.trim(),
            audio_artifact_id: audio_artifact_id.as_deref(),
            index: turn_index,
        }) {
            Ok(_) => report.turns_submitted += 1,
            Err(error) => eprintln!(
                "monkey daemon: a turn of {} could not be queued: {error}",
                call.call_id
            ),
        }
        if call.single_turn {
            // Voicemail: the caller leaves one message and the line closes.
            // Staying open would be a conversation, which is the other policy.
            break;
        }
    }

    unregister_speaker(&call.call_id);
    report
}

/// Which call a media socket belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CallIdentity {
    pub account_id: String,
    pub call_id: String,
    pub peer_number: String,
    pub session_key: String,
    /// Spoken as soon as audio is flowing: the number's greeting on an inbound
    /// call, or what the agent asked to say on an outbound one. `None` means
    /// the line opens silently, which only makes sense when the other end
    /// called us and is already talking.
    pub opening_line: Option<String>,
    /// Voicemail: take one message and hang up, rather than hold a
    /// conversation.
    pub single_turn: bool,
}

/// The half of a media stream that no two carriers spell the same way.
///
/// Implemented by each carrier rather than switched on here: Twilio wants its
/// `streamSid` echoed on every frame, Plivo wants a `playAudio` event carrying
/// the content type and sample rate, and Telnyx wants `media` under an RTP
/// bidirectional stream and drops anything faster than one payload a second.
pub(crate) trait MediaFrameCodec: Send + Sync {
    fn format(&self) -> MediaStreamFormat;
    /// One outbound audio frame, already base64 µ-law.
    fn encode_media_frame(&self, payload_b64: &str, stream_id: &str) -> String;
    /// Discard whatever this side has already queued at the carrier.
    ///
    /// Barge-in needs both halves: stop sending, and throw away what the
    /// carrier is still holding. Without the second one the caller interrupts
    /// and then listens to the rest of the sentence anyway.
    fn encode_clear_frame(&self, stream_id: &str) -> String;
}

/// Read a nested string out of an inbound frame, e.g. `["start", "streamId"]`.
fn read_path(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(str::to_string)
}

/// How long a batch of samples lasts, in milliseconds.
fn frame_ms(samples: usize) -> u32 {
    u32::try_from(samples * 1_000 / CALL_SAMPLE_RATE as usize).unwrap_or(u32::MAX)
}

/// Cut synthesized speech into this carrier's frames and queue them.
///
/// Queued rather than sent, because a sentence written straight to the socket
/// cannot be interrupted — and the carrier's own pacing limit (Telnyx allows
/// one payload a second) is honoured by the loop that drains this.
fn queue_audio(
    pending: &mut std::collections::VecDeque<String>,
    samples: &[i16],
    carrier: &dyn MediaFrameCodec,
    format: MediaStreamFormat,
    stream_id: &str,
) {
    let chunk_samples =
        (CALL_SAMPLE_RATE as usize / 1_000) * format.outbound_chunk_ms.max(20) as usize;
    for chunk in samples.chunks(chunk_samples) {
        let payload: Vec<u8> = chunk.iter().copied().map(encode_mulaw).collect();
        pending.push_back(carrier.encode_media_frame(&STANDARD.encode(payload), stream_id));
    }
}

/// Hands each caller turn to the daemon's own queue, as an ordinary run.
///
/// The turn is recorded as a channel event on the number's account before it is
/// queued, so a call leaves the same audit trail a text does — who said it, on
/// which line, and which job answered. Access is not re-decided here: the call
/// was answered because the account's inbound policy said to answer, and asking
/// a caller to complete a pairing handshake by voice is not a thing.
pub(crate) struct QueuedCallTurns<'a> {
    pub store: &'a std::sync::Mutex<super::store::DaemonStore>,
    pub queue: &'a dyn super::channel_worker::RunQueue,
    pub target: little_monkey_lib::channels::routing::RouteTarget,
}

impl CallTurnSink for QueuedCallTurns<'_> {
    fn submit_turn(&self, turn: CallTurn<'_>) -> Result<String, String> {
        use little_monkey_lib::channels::ingress::{ConversationIngress, ConversationSource};

        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| "System clock is before the Unix epoch".to_string())?
                .as_millis(),
        )
        .map_err(|_| "System clock is beyond the supported range".to_string())?;
        // Deterministic, so a turn cannot be recorded twice under a different
        // id if this path is ever retried.
        let source_event_id = format!("{}:turn:{}", turn.call_id, turn.index);
        {
            let mut store = self
                .store
                .lock()
                .map_err(|_| "call store lock poisoned".to_string())?;
            let recording = store.record_channel_event(&super::channel_store::NewChannelEvent {
                account_id: turn.account_id.to_string(),
                source: ConversationSource::Telephone,
                direction: super::channel_store::EventDirection::Inbound,
                provider_event_id: source_event_id.clone(),
                conversation_id: format!("call:{}", turn.call_id),
                thread_id: None,
                sender_id: Some(turn.peer_number.to_string()),
                // The transcript is the envelope: there is no provider payload
                // behind a phone call, only what was said.
                envelope_json: serde_json::json!({
                    "transcript": turn.text,
                    "audio_artifact_id": turn.audio_artifact_id,
                })
                .to_string(),
                disposition: super::channel_store::EventDisposition::Accepted,
                received_at_ms: now_ms,
            })?;
            if let super::channel_store::EventRecording::Duplicate { .. } = recording {
                return Err("This turn was already recorded".to_string());
            }
        }
        let mut ingress = ConversationIngress::direct(
            ConversationSource::Telephone,
            turn.account_id,
            source_event_id,
            turn.session_key,
            turn.text,
            self.target.clone(),
            now_ms,
        );
        if let Some(artifact_id) = turn.audio_artifact_id {
            // The recording rides along as an attachment, so the run can play
            // or forward it rather than only read what it was heard as.
            ingress
                .attachments
                .push(little_monkey_lib::channels::types::ChannelAttachment {
                    provider_id: None,
                    kind: little_monkey_lib::channels::types::AttachmentKind::Audio,
                    filename: Some(format!("{}.wav", turn.call_id)),
                    mime_type: Some("audio/wav".to_string()),
                    declared_size_bytes: None,
                    source: little_monkey_lib::channels::types::AttachmentSource::ProviderHandle {
                        handle: artifact_id.to_string(),
                    },
                });
        }
        let params = super::channel_ingress::run_params_for(&self.target, &ingress);
        self.queue.submit(&ingress, params)
    }
}

/// The operator's own speech backends, reached through the shared companion
/// state so a call uses exactly what the desktop uses.
pub(crate) struct ConfiguredSpeech {
    pub app_data_dir: std::path::PathBuf,
}

#[async_trait]
impl CallSpeech for ConfiguredSpeech {
    async fn transcribe(&self, wav: Vec<u8>) -> Result<String, String> {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-call-{}.wav",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&path, wav).map_err(|error| error.to_string())?;
        let result =
            little_monkey_lib::m7_companion::transcribe_call_audio(&self.app_data_dir, &path).await;
        let _ = std::fs::remove_file(&path);
        result
    }

    async fn keep_audio(&self, wav: Vec<u8>) -> Result<String, String> {
        little_monkey_lib::artifact_store::ArtifactStore::new(self.app_data_dir.join("content-v1"))
            .map_err(|error| error.to_string())?
            .put(&wav)
            .map(|blob| blob.id)
            .map_err(|error| error.to_string())
    }

    async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String> {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-say-{}.wav",
            uuid::Uuid::new_v4().simple()
        ));
        let result = little_monkey_lib::m7_companion::synthesize_speech_to_wav(
            &self.app_data_dir,
            text,
            &path,
        )
        .await
        .and_then(|()| std::fs::read(&path).map_err(|error| error.to_string()))
        .and_then(|bytes| read_wav_as_call_audio(&bytes));
        let _ = std::fs::remove_file(&path);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A carrier shaped like Twilio: stream id at the top level, echoed back
    /// on every frame, 20 ms per frame.
    struct FakeCarrier;

    impl MediaFrameCodec for FakeCarrier {
        fn format(&self) -> MediaStreamFormat {
            MediaStreamFormat {
                stream_id_path: &["streamSid"],
                outbound_chunk_ms: 20,
            }
        }

        fn encode_media_frame(&self, payload_b64: &str, stream_id: &str) -> String {
            serde_json::json!({
                "event": "media",
                "streamSid": stream_id,
                "media": { "payload": payload_b64 },
            })
            .to_string()
        }

        fn encode_clear_frame(&self, stream_id: &str) -> String {
            serde_json::json!({ "event": "clear", "streamSid": stream_id }).to_string()
        }
    }

    struct ScriptedSocket {
        /// Held shut until the test says the agent is talking, so "the caller
        /// interrupts" is a fact about ordering rather than a race.
        gate: Option<tokio::sync::oneshot::Receiver<()>>,
        inbound: Vec<String>,
        sent: Arc<Mutex<Vec<String>>>,
        /// Held open until the test's speaking is done, so the session does not
        /// end before a reply can be spoken.
        linger: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    #[async_trait]
    impl MediaSocket for ScriptedSocket {
        async fn recv(&mut self) -> Option<String> {
            if let Some(gate) = self.gate.as_mut() {
                let _ = gate.await;
                self.gate = None;
            }
            if let Some(frame) = self.inbound.pop() {
                return Some(frame);
            }
            // Borrowed rather than taken: this future is cancelled every time
            // the session picks another branch, and a lingering line has to
            // still be open on the next pass.
            if let Some(linger) = self.linger.as_mut() {
                let _ = linger.await;
                self.linger = None;
            }
            None
        }

        async fn send(&mut self, frame: String) -> Result<(), String> {
            self.sent.lock().unwrap().push(frame);
            Ok(())
        }
    }

    #[derive(Default)]
    struct KeptAudio(Mutex<Vec<usize>>);

    struct FakeSpeech {
        heard: &'static str,
        kept: Arc<KeptAudio>,
    }

    impl FakeSpeech {
        fn new(heard: &'static str) -> Self {
            Self {
                heard,
                kept: Arc::new(KeptAudio::default()),
            }
        }
    }

    #[async_trait]
    impl CallSpeech for FakeSpeech {
        async fn transcribe(&self, _wav: Vec<u8>) -> Result<String, String> {
            Ok(self.heard.to_string())
        }

        async fn keep_audio(&self, wav: Vec<u8>) -> Result<String, String> {
            self.kept.0.lock().unwrap().push(wav.len());
            Ok("artifact-1".to_string())
        }

        async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String> {
            // One sample per character: enough to prove the bytes came from
            // this text without pulling a synthesizer into a unit test.
            Ok(text.bytes().map(|byte| i16::from(byte) * 100).collect())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        turns: Mutex<Vec<(String, u32, Option<String>)>>,
    }

    impl CallTurnSink for RecordingSink {
        fn submit_turn(&self, turn: CallTurn<'_>) -> Result<String, String> {
            self.turns.lock().unwrap().push((
                turn.text.to_string(),
                turn.index,
                turn.audio_artifact_id.map(str::to_string),
            ));
            Ok(format!("job-{}", turn.index))
        }
    }

    fn media_frame(samples: &[i16]) -> String {
        let payload: Vec<u8> = samples.iter().copied().map(encode_mulaw).collect();
        serde_json::json!({
            "event": "media",
            "streamSid": "MZ1",
            "media": { "payload": STANDARD.encode(payload) },
        })
        .to_string()
    }

    /// A distinct call id per test: the speaker registry is process-global, the
    /// way live calls are, so two tests sharing an id would unregister each
    /// other's line.
    fn identity_for(call_id: &str) -> CallIdentity {
        CallIdentity {
            account_id: "tel-1".into(),
            call_id: call_id.into(),
            peer_number: "+15551234567".into(),
            session_key: format!("call:tel-1:{call_id}"),
            opening_line: None,
            single_turn: false,
        }
    }

    fn speech_samples(count: usize) -> Vec<i16> {
        (0..count)
            .map(|index| if index % 2 == 0 { 9_000 } else { -9_000 })
            .collect()
    }

    #[tokio::test]
    async fn what_the_caller_says_becomes_one_turn() {
        // `recv` pops from the back, so the script is written in reverse.
        let mut socket = ScriptedSocket {
            inbound: vec![
                serde_json::json!({ "event": "stop", "streamSid": "MZ1" }).to_string(),
                media_frame(&vec![0; 8_000]),
                media_frame(&speech_samples(8_000)),
                serde_json::json!({ "event": "start", "streamSid": "MZ1" }).to_string(),
            ],
            sent: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            linger: None,
        };
        let sink = RecordingSink::default();

        let report = run_media_session(
            &mut socket,
            &FakeSpeech::new("what is the deploy status"),
            &sink,
            &FakeCarrier,
            identity_for("call-turn"),
        )
        .await;

        assert_eq!(report.utterances_transcribed, 1);
        assert_eq!(report.turns_submitted, 1);
        assert_eq!(
            sink.turns.lock().unwrap().as_slice(),
            [("what is the deploy status".to_string(), 1, None)]
        );
    }

    #[tokio::test]
    async fn a_silent_line_is_never_transcribed_or_queued() {
        let mut socket = ScriptedSocket {
            inbound: vec![
                serde_json::json!({ "event": "stop", "streamSid": "MZ1" }).to_string(),
                media_frame(&vec![0; 16_000]),
            ],
            sent: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            linger: None,
        };
        let sink = RecordingSink::default();

        let report = run_media_session(
            &mut socket,
            &FakeSpeech::new("unused"),
            &sink,
            &FakeCarrier,
            identity_for("call-silent"),
        )
        .await;

        assert_eq!(report, MediaSessionReport::default());
        assert!(sink.turns.lock().unwrap().is_empty());
    }

    /// The two ways a session ends have to be told apart: the carrier saying
    /// the call is over, and the stream going away while it is still up. Only
    /// the second one leaves a line for somebody to hang up.
    #[tokio::test]
    async fn a_socket_that_stops_short_of_a_stop_event_reports_a_dropped_stream() {
        let ended = ScriptedSocket {
            inbound: vec![serde_json::json!({ "event": "stop", "streamSid": "MZ1" }).to_string()],
            sent: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            linger: None,
        };
        let dropped = ScriptedSocket {
            inbound: Vec::new(),
            sent: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            linger: None,
        };

        for (mut socket, call_id, expected) in [
            (ended, "call-stopped", false),
            (dropped, "call-dropped", true),
        ] {
            let report = run_media_session(
                &mut socket,
                &FakeSpeech::new("unused"),
                &RecordingSink::default(),
                &FakeCarrier,
                identity_for(call_id),
            )
            .await;

            assert_eq!(report.stream_dropped, expected, "{call_id}");
        }
    }

    #[tokio::test]
    async fn the_agent_s_reply_is_spoken_back_to_the_carrier() {
        let (release, linger) = tokio::sync::oneshot::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = ScriptedSocket {
            inbound: Vec::new(),
            sent: sent.clone(),
            gate: None,
            linger: Some(linger),
        };
        let session = async {
            run_media_session(
                &mut socket,
                &FakeSpeech::new("unused"),
                &RecordingSink::default(),
                &FakeCarrier,
                identity_for("call-reply"),
            )
            .await
        };
        let speaking = async {
            // The session registers itself as it starts; retry briefly rather
            // than racing it.
            for _ in 0..50 {
                if speak_on_call("call-reply", "deploy finished ten minutes ago").is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            // Long enough for the queued frames to drain at 20 ms each.
            tokio::time::sleep(std::time::Duration::from_millis(600)).await;
            let _ = release.send(());
        };
        let (report, ()) = tokio::join!(session, speaking);

        assert!(report.frames_spoken > 0, "the reply reached the carrier");
        let frames = sent.lock().unwrap().clone();
        let first: serde_json::Value = serde_json::from_str(&frames[0]).expect("json");
        assert_eq!(first["event"], "media");
        assert!(
            first["media"]["payload"].as_str().is_some(),
            "audio rides in the payload the carrier expects"
        );
    }

    #[tokio::test]
    async fn the_line_opens_with_the_greeting_rather_than_with_silence() {
        let (release, linger) = tokio::sync::oneshot::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = ScriptedSocket {
            inbound: vec![media_frame(&vec![0; 800])],
            sent: sent.clone(),
            // The line stays open while the greeting plays, the way a carrier's
            // does; the session ends when the socket closes.
            gate: None,
            linger: Some(linger),
        };
        let mut call = identity_for("call-greeting");
        call.opening_line = Some("Hello, this is the support line.".into());

        let speech = FakeSpeech::new("unused");
        let sink = RecordingSink::default();
        let session = run_media_session(&mut socket, &speech, &sink, &FakeCarrier, call);
        let hang_up = async {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            let _ = release.send(());
        };
        let (report, ()) = tokio::join!(session, hang_up);

        assert!(report.frames_spoken > 0, "the greeting was spoken");
        let first: serde_json::Value =
            serde_json::from_str(&sent.lock().unwrap()[0]).expect("json");
        assert_eq!(
            first["streamSid"], "MZ1",
            "frames carry the carrier's own id"
        );
    }

    /// One 20 ms frame of somebody talking.
    fn talking_frame() -> String {
        media_frame(&speech_samples(160))
    }

    /// Drive a call whose agent is mid-sentence while the caller talks over it
    /// for `frames` × 20 ms, and report what the session did.
    async fn interrupted_after(
        frames: usize,
        call: CallIdentity,
    ) -> (MediaSessionReport, Vec<String>) {
        let (open_gate, gate) = tokio::sync::oneshot::channel();
        let (release, linger) = tokio::sync::oneshot::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let call_id = call.call_id.clone();
        let speaking_first = call.opening_line.is_none();
        let mut socket = ScriptedSocket {
            gate: Some(gate),
            // `recv` pops from the back.
            inbound: std::iter::repeat_with(talking_frame).take(frames).collect(),
            sent: sent.clone(),
            linger: Some(linger),
        };
        let speech = FakeSpeech::new("heard");
        let sink = RecordingSink::default();
        let session = run_media_session(&mut socket, &speech, &sink, &FakeCarrier, call);
        let caller = async {
            if speaking_first {
                for _ in 0..50 {
                    if speak_on_call(&call_id, &"x".repeat(8_000)).is_ok() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(60)).await;
            let _ = open_gate.send(());
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            let _ = release.send(());
        };
        let (report, ()) = tokio::join!(session, caller);
        let frames = sent.lock().unwrap().clone();
        (report, frames)
    }

    fn cleared(frames: &[String]) -> bool {
        frames
            .iter()
            .filter_map(|frame| serde_json::from_str::<serde_json::Value>(frame).ok())
            .any(|frame| frame["event"] == "clear")
    }

    #[tokio::test]
    async fn a_caller_talking_over_the_agent_cuts_it_off() {
        // 12 frames is 240 ms: sustained talking, not a noise.
        let (report, frames) = interrupted_after(12, identity_for("call-bargein")).await;

        assert_eq!(report.interruptions, 1, "the caller interrupted once");
        assert!(
            cleared(&frames),
            "the carrier is told to drop what it is still holding"
        );
        assert!(
            report.frames_spoken < 40,
            "the rest of the sentence was dropped, sent {} of 50 frames",
            report.frames_spoken
        );
    }

    #[tokio::test]
    async fn a_noise_in_the_room_does_not_cut_the_agent_off() {
        // Four frames is 80 ms — a door, a cough, or the caller's own audio
        // echoing back off a speakerphone.
        let (report, frames) = interrupted_after(4, identity_for("call-noise")).await;

        assert_eq!(report.interruptions, 0);
        assert!(!cleared(&frames));
    }

    #[tokio::test]
    async fn the_greeting_itself_is_never_interrupted() {
        // A caller who talks over "hello, this is…" has not been told who is
        // calling yet, so the greeting finishes.
        let mut call = identity_for("call-greeting-bargein");
        call.opening_line = Some("x".repeat(8_000));

        let (report, frames) = interrupted_after(30, call).await;

        assert_eq!(report.interruptions, 0);
        assert!(!cleared(&frames));
    }

    #[tokio::test]
    async fn voicemail_takes_one_message_and_stops() {
        let mut socket = ScriptedSocket {
            inbound: vec![
                // More audio after the first message, which must not be read:
                // the line is over once the message is taken.
                media_frame(&speech_samples(8_000)),
                media_frame(&vec![0; 8_000]),
                media_frame(&speech_samples(8_000)),
            ],
            sent: Arc::new(Mutex::new(Vec::new())),
            gate: None,
            linger: None,
        };
        let mut call = identity_for("call-voicemail");
        call.single_turn = true;
        let sink = RecordingSink::default();
        let speech = FakeSpeech::new("please call me back");

        let report = run_media_session(&mut socket, &speech, &sink, &FakeCarrier, call).await;

        assert_eq!(report.turns_submitted, 1);
        assert_eq!(sink.turns.lock().unwrap().len(), 1);
        assert_eq!(report.recordings_kept, 1, "a voicemail is the audio");
        assert!(
            !speech.kept.0.lock().unwrap().is_empty(),
            "the recording reached the store"
        );
        assert_eq!(
            sink.turns.lock().unwrap()[0].2.as_deref(),
            Some("artifact-1"),
            "and the turn names it"
        );
    }

    #[tokio::test]
    async fn speaking_to_a_call_that_ended_is_refused_rather_than_queued() {
        let error = speak_on_call("call-that-never-existed", "hello").expect_err("refused");
        assert!(error.contains("no longer on the line"));
    }
}

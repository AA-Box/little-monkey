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

fn register_speaker(call_id: &str) -> mpsc::UnboundedReceiver<String> {
    let (sender, receiver) = mpsc::unbounded_channel();
    if let Ok(mut speakers) = speakers().lock() {
        speakers.insert(call_id.to_string(), sender);
    }
    receiver
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

    loop {
        let frame = tokio::select! {
            // Anything the agent said is spoken before more audio is read, so a
            // reply is not queued behind a caller who keeps talking.
            Some(text) = to_speak.recv() => {
                match speech.synthesize(&text).await {
                    Ok(samples) => {
                        report.frames_spoken +=
                            send_audio(socket, &samples, carrier, format, &stream_id).await;
                    }
                    Err(error) => eprintln!("monkey daemon: could not speak on {}: {error}", call.call_id),
                }
                continue;
            }
            frame = socket.recv() => frame,
        };
        let Some(frame) = frame else { break };
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
                        report.frames_spoken +=
                            send_audio(socket, &samples, carrier, format, &stream_id).await;
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
        let UtteranceProgress::Complete(utterance) = detector.push(&samples) else {
            continue;
        };
        report.utterances_transcribed += 1;
        let wav = write_wav(&utterance, CALL_SAMPLE_RATE);
        let text = match speech.transcribe(wav).await {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "monkey daemon: could not transcribe a turn of {}: {error}",
                    call.call_id
                );
                continue;
            }
        };
        if text.trim().is_empty() {
            continue;
        }
        turn_index += 1;
        match sink.submit_turn(CallTurn {
            account_id: &call.account_id,
            call_id: &call.call_id,
            peer_number: &call.peer_number,
            session_key: &call.session_key,
            text: text.trim(),
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
}

/// Read a nested string out of an inbound frame, e.g. `["start", "streamId"]`.
fn read_path(value: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(str::to_string)
}

async fn send_audio(
    socket: &mut dyn MediaSocket,
    samples: &[i16],
    carrier: &dyn MediaFrameCodec,
    format: MediaStreamFormat,
    stream_id: &str,
) -> u32 {
    let mut sent = 0;
    let chunk_samples =
        (CALL_SAMPLE_RATE as usize / 1_000) * format.outbound_chunk_ms.max(20) as usize;
    for chunk in samples.chunks(chunk_samples) {
        let payload: Vec<u8> = chunk.iter().copied().map(encode_mulaw).collect();
        let frame = carrier.encode_media_frame(&STANDARD.encode(payload), stream_id);
        if socket.send(frame).await.is_err() {
            break;
        }
        sent += 1;
        // A carrier that caps payloads per second drops anything faster, so
        // the pacing is part of sending correctly rather than politeness.
        if format.outbound_chunk_ms >= 1_000 {
            tokio::time::sleep(std::time::Duration::from_millis(
                u64::from(format.outbound_chunk_ms) - 50,
            ))
            .await;
        }
    }
    sent
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
                envelope_json: serde_json::json!({ "transcript": turn.text }).to_string(),
                disposition: super::channel_store::EventDisposition::Accepted,
                received_at_ms: now_ms,
            })?;
            if let super::channel_store::EventRecording::Duplicate { .. } = recording {
                return Err("This turn was already recorded".to_string());
            }
        }
        let ingress = ConversationIngress::direct(
            ConversationSource::Telephone,
            turn.account_id,
            source_event_id,
            turn.session_key,
            turn.text,
            self.target.clone(),
            now_ms,
        );
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
    }

    struct ScriptedSocket {
        inbound: Vec<String>,
        sent: Arc<Mutex<Vec<String>>>,
        /// Held open until the test's speaking is done, so the session does not
        /// end before a reply can be spoken.
        linger: Option<tokio::sync::oneshot::Receiver<()>>,
    }

    #[async_trait]
    impl MediaSocket for ScriptedSocket {
        async fn recv(&mut self) -> Option<String> {
            if let Some(frame) = self.inbound.pop() {
                return Some(frame);
            }
            if let Some(linger) = self.linger.take() {
                let _ = linger.await;
            }
            None
        }

        async fn send(&mut self, frame: String) -> Result<(), String> {
            self.sent.lock().unwrap().push(frame);
            Ok(())
        }
    }

    struct FakeSpeech {
        heard: &'static str,
    }

    #[async_trait]
    impl CallSpeech for FakeSpeech {
        async fn transcribe(&self, _wav: Vec<u8>) -> Result<String, String> {
            Ok(self.heard.to_string())
        }

        async fn synthesize(&self, text: &str) -> Result<Vec<i16>, String> {
            // One sample per character: enough to prove the bytes came from
            // this text without pulling a synthesizer into a unit test.
            Ok(text.bytes().map(|byte| i16::from(byte) * 100).collect())
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        turns: Mutex<Vec<(String, u32)>>,
    }

    impl CallTurnSink for RecordingSink {
        fn submit_turn(&self, turn: CallTurn<'_>) -> Result<String, String> {
            self.turns
                .lock()
                .unwrap()
                .push((turn.text.to_string(), turn.index));
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

    fn identity() -> CallIdentity {
        CallIdentity {
            account_id: "tel-1".into(),
            call_id: "call-1".into(),
            peer_number: "+15551234567".into(),
            session_key: "call:tel-1:call-1".into(),
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
            linger: None,
        };
        let sink = RecordingSink::default();

        let report = run_media_session(
            &mut socket,
            &FakeSpeech {
                heard: "what is the deploy status",
            },
            &sink,
            &FakeCarrier,
            identity(),
        )
        .await;

        assert_eq!(report.utterances_transcribed, 1);
        assert_eq!(report.turns_submitted, 1);
        assert_eq!(
            sink.turns.lock().unwrap().as_slice(),
            [("what is the deploy status".to_string(), 1)]
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
            linger: None,
        };
        let sink = RecordingSink::default();

        let report = run_media_session(
            &mut socket,
            &FakeSpeech { heard: "unused" },
            &sink,
            &FakeCarrier,
            identity(),
        )
        .await;

        assert_eq!(report, MediaSessionReport::default());
        assert!(sink.turns.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_agent_s_reply_is_spoken_back_to_the_carrier() {
        let (release, linger) = tokio::sync::oneshot::channel();
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = ScriptedSocket {
            inbound: Vec::new(),
            sent: sent.clone(),
            linger: Some(linger),
        };
        let session = async {
            run_media_session(
                &mut socket,
                &FakeSpeech { heard: "unused" },
                &RecordingSink::default(),
                &FakeCarrier,
                identity(),
            )
            .await
        };
        let speaking = async {
            // The session registers itself as it starts; retry briefly rather
            // than racing it.
            for _ in 0..50 {
                if speak_on_call("call-1", "deploy finished ten minutes ago").is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
        let sent = Arc::new(Mutex::new(Vec::new()));
        let mut socket = ScriptedSocket {
            inbound: vec![
                serde_json::json!({ "event": "stop", "streamSid": "MZ1" }).to_string(),
                media_frame(&vec![0; 800]),
            ],
            sent: sent.clone(),
            linger: None,
        };
        let mut call = identity();
        call.opening_line = Some("Hello, this is the support line.".into());

        let report = run_media_session(
            &mut socket,
            &FakeSpeech { heard: "unused" },
            &RecordingSink::default(),
            &FakeCarrier,
            call,
        )
        .await;

        assert!(report.frames_spoken > 0, "the greeting was spoken");
        let first: serde_json::Value =
            serde_json::from_str(&sent.lock().unwrap()[0]).expect("json");
        assert_eq!(
            first["streamSid"], "MZ1",
            "frames carry the carrier's own id"
        );
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
            linger: None,
        };
        let mut call = identity();
        call.single_turn = true;
        let sink = RecordingSink::default();

        let report = run_media_session(
            &mut socket,
            &FakeSpeech {
                heard: "please call me back",
            },
            &sink,
            &FakeCarrier,
            call,
        )
        .await;

        assert_eq!(report.turns_submitted, 1);
        assert_eq!(sink.turns.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn speaking_to_a_call_that_ended_is_refused_rather_than_queued() {
        let error = speak_on_call("call-that-never-existed", "hello").expect_err("refused");
        assert!(error.contains("no longer on the line"));
    }
}

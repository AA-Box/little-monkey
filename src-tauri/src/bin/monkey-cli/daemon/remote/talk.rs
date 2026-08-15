//! A live conversation with a paired device: microphone in, an ordinary Little
//! Monkey turn in the middle, synthesized speech back out.
//!
//! ```text
//! device microphone -> local VAD -> utterance frames -> STT
//!   -> durable ingress record -> normal run -> assistant deltas
//!   -> sentence boundaries -> TTS -> output audio frames -> device
//! ```
//!
//! **Why this is not the voice-stream capture path.** `voice.rs` records a
//! room: audio goes one way, lands in a file, and the device command that
//! carries it is the whole conversation with the runner. A Talk session is
//! two-way and stateful — what was heard, what is being thought, what is being
//! said — so it needs a socket, not a queue of appends. It reuses the grant
//! (`voice_stream`), the pairing and the TLS; it does not reuse the transport.
//!
//! **Nothing here is a voice agent.** A finalized transcript becomes an
//! ordinary durable turn through the same queue the mobile chat surface uses,
//! answered by the same run with the same tools, memory and approvals. This
//! module owns the microphone and the speaker; it owns no model, no prompt and
//! no session state the rest of the product does not already have.
//!
//! **The audio never lands in a log.** Frames are decoded, held for the length
//! of one utterance and dropped. What survives a session is the transcript, the
//! assistant's text and bounded counters — see [`TalkSessionReport`].

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};

use super::protocol::{
    TalkClientFrame, TalkClientFrameKind, TalkSequenceTracker, TalkServerFrame,
    TalkServerFrameKind, TalkState, MAX_TALK_AUDIO_BYTES, MAX_TALK_LATENCY_MS, MAX_TALK_TEXT_BYTES,
    TALK_PROTOCOL_VERSION,
};

/// The most audio one utterance may hold before the runner refuses to keep
/// buffering. Ninety seconds of Opus is far below this; a device that never
/// sets `last` is what it is for.
pub const MAX_TALK_UTTERANCE_BYTES: usize = 8 * 1024 * 1024;
/// The most assistant text one turn may speak. Beyond this the answer is still
/// delivered as text; only synthesis stops.
pub const MAX_SPOKEN_TEXT_BYTES: usize = 32 * 1024;
/// How often a running turn's durable events are re-read while waiting.
pub const RUN_POLL_INTERVAL_MS: u64 = 120;
/// How long one turn may stay unfinished before the session says so and returns
/// to listening. The run itself is untouched — it is durable, and a long tool
/// call is not an error.
pub const MAX_TURN_WAIT_MS: u64 = 10 * 60 * 1_000;
/// How often the grant is re-read while an answer is streaming. Between frames
/// it is checked on arrival; during an answer nothing may arrive for minutes,
/// and a revoked microphone that keeps working for minutes is not revoked.
pub const GRANT_RECHECK_INTERVAL_MS: u64 = 1_000;

/// One Talk socket, abstracted so the session loop can be driven by a test with
/// no network and no browser.
#[async_trait]
pub trait TalkSocket: Send {
    /// Next text frame from the device, or `None` when the socket closes.
    async fn recv(&mut self) -> Option<String>;
    async fn send(&mut self, frame: String) -> Result<(), String>;
}

/// The operator's own configured speech backends.
///
/// Deliberately the same seam `call_media.rs` uses: a Talk session must not get
/// a speech stack of its own, or an operator who configured local Whisper would
/// find what they say into their phone quietly sent to a hosted provider.
#[async_trait]
pub trait TalkSpeech: Send + Sync {
    async fn transcribe(&self, audio: Vec<u8>, media_type: &str) -> Result<String, String>;
    /// Synthesized speech, as bytes plus the media type they are in.
    async fn synthesize(&self, text: &str) -> Result<(Vec<u8>, String), String>;
}

/// What a running turn has produced so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TalkRunProgress {
    /// Assistant text appended since the last poll.
    pub delta: String,
    /// Where the next poll should resume from.
    pub next_index: u64,
    pub finished: bool,
    /// Set when the run failed or was cancelled. The session reports it to the
    /// device rather than pretending the answer was empty.
    pub error: Option<String>,
}

/// Where a finalized transcript goes, and how its answer is read back.
pub trait TalkTurns: Send + Sync {
    /// Queue one spoken turn as an ordinary durable turn. `client_key` is this
    /// utterance's stable identity, so a retried submission lands on the run the
    /// first attempt made instead of starting a second one.
    fn submit(&self, session_id: &str, client_key: &str, text: &str) -> Result<String, String>;
    fn progress(&self, run_id: &str, from_index: u64) -> Result<TalkRunProgress, String>;
    /// Stop a run the user talked over. Best effort by construction: a tool call
    /// that already reached the world is not undone by this, and the session
    /// never claims it was.
    fn cancel(&self, run_id: &str) -> Result<(), String>;
    /// Whether the device still holds `voice_stream`. Asked between turns so a
    /// grant revoked mid-conversation closes the microphone rather than waiting
    /// for the socket to break.
    fn still_granted(&self, device_id: &str) -> bool;
}

/// Which conversation a socket belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TalkIdentity {
    pub device_id: String,
    pub session_id: String,
    /// Random token minted with the one-use ticket. Every frame in both
    /// directions carries it, so a frame captured from an earlier socket cannot
    /// be replayed into this one.
    pub session_generation: String,
}

/// One latency span across a session: how many samples, their total and the
/// worst one. Three numbers rather than a list, so a long conversation cannot
/// grow the thing that gets written to an audit row.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TalkLatencySpan {
    pub samples: u32,
    pub total_ms: u64,
    pub worst_ms: u64,
}

impl TalkLatencySpan {
    pub fn observe(&mut self, span_ms: u64) {
        let span_ms = span_ms.min(MAX_TALK_LATENCY_MS);
        self.samples = self.samples.saturating_add(1);
        self.total_ms = self.total_ms.saturating_add(span_ms);
        self.worst_ms = self.worst_ms.max(span_ms);
    }

    pub fn mean_ms(&self) -> Option<u64> {
        (self.samples > 0).then(|| self.total_ms / u64::from(self.samples))
    }
}

/// Where a spoken turn spent its time, in the same seven spans the desktop
/// measures — so one diagnostic model covers a phone and a laptop even though
/// the transports have nothing else in common.
///
/// The first three are measured on the device and arrive on a `metrics` frame;
/// the rest are measured here. All of them are durations. None of them can hold
/// a word anybody said.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TalkSessionLatency {
    pub speech_detection: TalkLatencySpan,
    pub capture: TalkLatencySpan,
    pub upload: TalkLatencySpan,
    pub transcription: TalkLatencySpan,
    pub model_first_token: TalkLatencySpan,
    pub tts_first_audio: TalkLatencySpan,
    pub end_to_end: TalkLatencySpan,
}

/// What one finished session did. Bounded counters and nothing said aloud: this
/// is what may reach a log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TalkSessionReport {
    pub utterances: u32,
    pub turns_submitted: u32,
    pub interruptions: u32,
    pub spoken_chunks: u32,
    /// Errors reported to the device. A misconfigured speech backend ends a
    /// turn, never the session.
    pub errors: u32,
    /// Turns whose answer arrived as text but could not be spoken. The counter
    /// the desktop calls `fallback`.
    pub fallbacks: u32,
    /// The socket stopped carrying frames rather than being closed on purpose.
    pub stream_dropped: bool,
    /// The grant was withdrawn while the session was open.
    pub grant_revoked: bool,
    pub latency: TalkSessionLatency,
}

/// Cuts streamed assistant text into sentence- or phrase-sized pieces safe to
/// speak.
///
/// Three things must never reach a synthesizer mid-flight: a fenced code block
/// (nobody wants their braces read out), an unfinished Markdown link, and a URL
/// that has not finished arriving. The first is skipped outright, the second
/// holds the buffer until its closing paren lands, and the third is stripped.
#[derive(Debug, Default)]
pub struct SpeechChunker {
    buffer: String,
    /// A trailing partial fence (`` ` `` or ``` `` ```) held back until the next
    /// delta says whether it opens a block.
    tick_carry: String,
    in_code_fence: bool,
}

impl SpeechChunker {
    pub fn push(&mut self, delta: &str, final_delta: bool) -> Vec<String> {
        let value = format!("{}{delta}", std::mem::take(&mut self.tick_carry));
        let bytes: Vec<char> = value.chars().collect();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index..].starts_with(&['`', '`', '`']) {
                self.in_code_fence = !self.in_code_fence;
                index += 3;
                continue;
            }
            if !final_delta && bytes[index] == '`' && bytes.len() - index < 3 {
                self.tick_carry = bytes[index..].iter().collect();
                break;
            }
            if !self.in_code_fence {
                self.buffer.push(bytes[index]);
            }
            index += 1;
        }
        if final_delta {
            if !self.in_code_fence {
                let carry = std::mem::take(&mut self.tick_carry);
                self.buffer.push_str(&carry);
            }
            self.tick_carry.clear();
            self.in_code_fence = false;
        }
        self.drain(final_delta)
    }

    pub fn reset(&mut self) {
        self.buffer.clear();
        self.tick_carry.clear();
        self.in_code_fence = false;
    }

    fn drain(&mut self, final_delta: bool) -> Vec<String> {
        let mut chunks = Vec::new();
        loop {
            let characters: Vec<char> = self.buffer.chars().collect();
            let scan_limit = incomplete_markdown_at(&characters).unwrap_or(characters.len());
            let mut boundary = None;
            for index in 0..scan_limit {
                let current = characters[index];
                let next = characters.get(index + 1).copied();
                let followed_by_space = next.is_none_or(char::is_whitespace);
                let sentence = matches!(current, '.' | '!' | '?') && followed_by_space;
                let phrase = matches!(current, ';' | ':')
                    && next.is_some_and(char::is_whitespace)
                    && index >= 48;
                let line = current == '\n';
                let clause =
                    current == ',' && next.is_some_and(char::is_whitespace) && index >= 180;
                if sentence || phrase || line || clause {
                    boundary = Some(index + 1);
                }
                if boundary.is_some_and(|value| value >= 320) {
                    break;
                }
            }
            let boundary = match (boundary, final_delta) {
                (Some(value), _) => value,
                (None, true) if scan_limit > 0 => scan_limit,
                _ => break,
            };
            if boundary == 0 {
                break;
            }
            let raw: String = characters[..boundary].iter().collect();
            self.buffer = characters[boundary..]
                .iter()
                .collect::<String>()
                .trim_start()
                .to_string();
            let clean = strip_markdown_for_speech(&raw);
            if !clean.is_empty() {
                chunks.push(clean);
            }
            if self.buffer.is_empty() {
                break;
            }
        }
        chunks
    }
}

/// Where an unfinished Markdown link starts, if the tail of the buffer holds
/// one. Speaking up to there would read half a link out loud.
fn incomplete_markdown_at(characters: &[char]) -> Option<usize> {
    let open = characters.iter().rposition(|character| *character == '[');
    let close = characters.iter().rposition(|character| *character == ']');
    match (open, close) {
        (Some(open), None) => return Some(open),
        (Some(open), Some(close)) if open > close => return Some(open),
        _ => {}
    }
    let link_start = characters
        .windows(2)
        .rposition(|pair| pair == [']', '('])
        .map(|index| index + 1)?;
    if characters[link_start..].contains(&')') {
        return None;
    }
    characters[..link_start]
        .iter()
        .rposition(|character| *character == '[')
}

/// Markdown a synthesizer should not read as characters. URLs go entirely:
/// "h t t p s colon slash slash" is never what anyone wanted to hear.
fn strip_markdown_for_speech(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let characters: Vec<char> = value.chars().collect();
    let mut index = 0;
    while index < characters.len() {
        let rest: String = characters[index..].iter().collect();
        if rest.starts_with("http://") || rest.starts_with("https://") {
            // Stops at the closing paren as well as at whitespace: a URL inside
            // a Markdown link has no space before the `)`, and swallowing it
            // would eat the sentence's own punctuation with it.
            while index < characters.len()
                && !characters[index].is_whitespace()
                && characters[index] != ')'
            {
                index += 1;
            }
            continue;
        }
        let character = characters[index];
        match character {
            '*' | '_' | '~' | '`' | '#' | '>' | '[' | ']' | '(' | ')' | '<' | '|' => out.push(' '),
            '!' if characters.get(index + 1) == Some(&'[') => out.push(' '),
            _ => out.push(character),
        }
        index += 1;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Run one Talk session until the device closes the socket.
///
/// Errors from transcription, the queue or synthesis end that *turn* and are
/// reported to the device as a typed error frame; they never end the session. A
/// speech backend somebody forgot to configure should show up as a sentence on
/// a screen, not as a socket that dies.
pub async fn run_talk_session(
    socket: &mut dyn TalkSocket,
    speech: &dyn TalkSpeech,
    turns: &dyn TalkTurns,
    identity: TalkIdentity,
) -> TalkSessionReport {
    let mut session = Session {
        report: TalkSessionReport::default(),
        identity,
        outbound_sequence: 0,
        outbound_audio_sequence: 0,
        inbound: TalkSequenceTracker::default(),
        utterance: Vec::new(),
        utterance_media_type: None,
        utterance_index: 0,
        greeted: false,
        revoked: false,
        interrupting_utterance_complete: false,
        pending_device_latency: DeviceLatency::default(),
    };
    if session
        .emit(socket, TalkServerFrameKind::Ready)
        .await
        .is_err()
    {
        session.report.stream_dropped = true;
        return session.report;
    }
    let _ = session
        .emit(
            socket,
            TalkServerFrameKind::State {
                state: TalkState::Listening,
            },
        )
        .await;

    while let Some(raw) = socket.recv().await {
        // A grant withdrawn between utterances closes the microphone here
        // rather than at the next network failure.
        if !turns.still_granted(&session.identity.device_id) {
            session.report.grant_revoked = true;
            let _ = session.report_revocation(socket).await;
            break;
        }
        let frame = match session.parse(&raw) {
            Ok(frame) => frame,
            Err(error) => {
                session.report.errors += 1;
                if session
                    .emit(
                        socket,
                        TalkServerFrameKind::Error {
                            code: "invalid_frame".into(),
                            message: error,
                            retryable: false,
                        },
                    )
                    .await
                    .is_err()
                {
                    session.report.stream_dropped = true;
                    break;
                }
                continue;
            }
        };
        match frame.kind {
            TalkClientFrameKind::Hello { media_type, .. } => {
                session.greeted = true;
                session.utterance_media_type = Some(media_type);
            }
            TalkClientFrameKind::State { .. } => {}
            TalkClientFrameKind::Metrics {
                speech_detection_ms,
                capture_ms,
                upload_ms,
            } => session.observe_device_latency(speech_detection_ms, capture_ms, upload_ms),
            TalkClientFrameKind::Interrupt { .. } => {
                // Nothing is playing between turns; an interrupt that arrives
                // here only clears whatever was half-buffered.
                session.report.interruptions += 1;
                session.utterance.clear();
                let _ = session
                    .emit(
                        socket,
                        TalkServerFrameKind::State {
                            state: TalkState::Listening,
                        },
                    )
                    .await;
            }
            TalkClientFrameKind::Audio {
                media_type,
                audio_base64,
                last,
                ..
            } => {
                let Ok(bytes) = STANDARD.decode(&audio_base64) else {
                    session.report.errors += 1;
                    let _ = session
                        .emit(
                            socket,
                            TalkServerFrameKind::Error {
                                code: "invalid_audio".into(),
                                message: "The audio payload was not valid base64.".into(),
                                retryable: false,
                            },
                        )
                        .await;
                    continue;
                };
                if session.utterance.len() + bytes.len() > MAX_TALK_UTTERANCE_BYTES {
                    session.utterance.clear();
                    session.report.errors += 1;
                    let _ = session
                        .emit(
                            socket,
                            TalkServerFrameKind::Error {
                                code: "utterance_too_long".into(),
                                message: "That utterance passed this runner's size ceiling.".into(),
                                retryable: true,
                            },
                        )
                        .await;
                    continue;
                }
                session.utterance.extend_from_slice(&bytes);
                if session.utterance_media_type.is_none() {
                    session.utterance_media_type = Some(media_type);
                }
                if !last {
                    continue;
                }
                if session
                    .answer_utterance(socket, speech, turns)
                    .await
                    .is_err()
                {
                    session.report.stream_dropped = true;
                    break;
                }
                // Talking over the answer sends audio, and that audio is a
                // complete utterance of its own. Answer it here rather than
                // waiting for a frame the device has no reason to send.
                while session.interrupting_utterance_complete && !session.revoked {
                    session.interrupting_utterance_complete = false;
                    if session
                        .answer_utterance(socket, speech, turns)
                        .await
                        .is_err()
                    {
                        session.report.stream_dropped = true;
                        break;
                    }
                }
                // A grant withdrawn *during* the answer is noticed by the
                // streaming loop, which stops speaking immediately; the session
                // itself ends here, without waiting for a frame that a silent
                // device is never going to send.
                //
                // The check is repeated once per turn as well as on the
                // streaming loop's timer, because a short answer can finish
                // before that timer comes round — and a device that then goes
                // quiet would otherwise hold an open microphone on a grant it
                // no longer has.
                if !session.revoked
                    && !session.report.stream_dropped
                    && !turns.still_granted(&session.identity.device_id)
                {
                    session.report.grant_revoked = true;
                    session.revoked = true;
                    let _ = session.report_revocation(socket).await;
                }
                if session.revoked || session.report.stream_dropped {
                    break;
                }
            }
        }
    }
    session.report
}

/// The three spans the device measured for the utterance it just sent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct DeviceLatency {
    speech_detection_ms: Option<u64>,
    capture_ms: Option<u64>,
    upload_ms: Option<u64>,
}

impl DeviceLatency {
    /// Everything the device spent before the runner heard anything, which is
    /// the head of the end-to-end span the runner completes itself.
    fn device_side_ms(&self) -> u64 {
        self.capture_ms
            .unwrap_or(0)
            .saturating_add(self.upload_ms.unwrap_or(0))
    }
}

struct Session {
    report: TalkSessionReport,
    identity: TalkIdentity,
    outbound_sequence: u64,
    outbound_audio_sequence: u64,
    inbound: TalkSequenceTracker,
    utterance: Vec<u8>,
    utterance_media_type: Option<String>,
    utterance_index: u32,
    greeted: bool,
    /// Set when the grant went away mid-answer. The socket is finished; the run
    /// it was narrating is not, and nothing here pretends otherwise.
    revoked: bool,
    /// The audio that interrupted an answer arrived complete, so it is already a
    /// whole utterance and must be answered without waiting for another frame.
    interrupting_utterance_complete: bool,
    pending_device_latency: DeviceLatency,
}

impl Session {
    fn parse(&mut self, raw: &str) -> Result<TalkClientFrame, String> {
        let frame: TalkClientFrame =
            serde_json::from_str(raw).map_err(|error| format!("Unreadable Talk frame: {error}"))?;
        frame.validate()?;
        if frame.session_id != self.identity.session_id {
            return Err("That frame names another Talk session".to_string());
        }
        // The generation is what binds a frame to *this* socket. A frame
        // captured from an earlier session cannot be replayed into this one
        // even with a valid signature on the ticket that opened it.
        if frame.session_generation != self.identity.session_generation {
            return Err("That frame belongs to an earlier Talk session".to_string());
        }
        if !self.greeted && !matches!(frame.kind, TalkClientFrameKind::Hello { .. }) {
            return Err("The first Talk frame must be a hello".to_string());
        }
        self.inbound
            .accept(frame.frame_sequence, frame.audio_sequence())?;
        Ok(frame)
    }

    async fn emit(
        &mut self,
        socket: &mut dyn TalkSocket,
        kind: TalkServerFrameKind,
    ) -> Result<(), String> {
        self.outbound_sequence += 1;
        let frame = TalkServerFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: self.identity.session_id.clone(),
            session_generation: self.identity.session_generation.clone(),
            frame_sequence: self.outbound_sequence,
            kind,
        };
        frame.validate()?;
        socket
            .send(
                serde_json::to_string(&frame)
                    .map_err(|error| format!("Talk frame cannot be serialized: {error}"))?,
            )
            .await
    }

    /// Transcribe what was heard, run it as an ordinary turn, and speak the
    /// answer back — stopping the moment the device says it is talking again.
    ///
    /// `Err` here means the *socket* broke, not that the turn failed: a failed
    /// turn is an error frame and the session stays open.
    async fn answer_utterance(
        &mut self,
        socket: &mut dyn TalkSocket,
        speech: &dyn TalkSpeech,
        turns: &dyn TalkTurns,
    ) -> Result<(), String> {
        let audio = std::mem::take(&mut self.utterance);
        let media_type = self
            .utterance_media_type
            .clone()
            .unwrap_or_else(|| "audio/webm".to_string());
        if audio.is_empty() {
            return Ok(());
        }
        self.report.utterances += 1;
        self.utterance_index += 1;
        let device_latency = std::mem::take(&mut self.pending_device_latency);
        // The clock the whole turn is measured against. It starts where the
        // device's own spans end, so `end_to_end` means what it says: the first
        // word spoken to the last thing said back.
        let turn_started = std::time::Instant::now();
        self.emit(
            socket,
            TalkServerFrameKind::State {
                state: TalkState::Transcribing,
            },
        )
        .await?;
        let text = match speech.transcribe(audio, &media_type).await {
            Ok(text) => text.trim().to_string(),
            Err(error) => {
                self.report.fallbacks += 1;
                return self
                    .fail_turn(socket, "transcription_failed", &error, true)
                    .await;
            }
        };
        self.report
            .latency
            .transcription
            .observe(elapsed_ms(turn_started));
        // A grant can be withdrawn while a long transcription runs. Noticing it
        // here means the words are dropped rather than becoming a turn.
        if !turns.still_granted(&self.identity.device_id) {
            self.report.grant_revoked = true;
            self.revoked = true;
            return self.report_revocation(socket).await;
        }
        if text.is_empty() {
            // Silence is not an error and must not become a turn.
            return self
                .emit(
                    socket,
                    TalkServerFrameKind::State {
                        state: TalkState::Listening,
                    },
                )
                .await;
        }
        self.emit(
            socket,
            TalkServerFrameKind::Transcript {
                text: bounded(&text, MAX_TALK_TEXT_BYTES),
                is_final: true,
            },
        )
        .await?;
        self.emit(
            socket,
            TalkServerFrameKind::State {
                state: TalkState::Thinking,
            },
        )
        .await?;

        // The utterance's stable identity, so a resubmitted turn collapses onto
        // the run the first attempt made.
        let client_key = format!(
            "talk-{}-{}",
            &self.identity.session_generation[..16.min(self.identity.session_generation.len())],
            self.utterance_index
        );
        let run_id = match turns.submit(&self.identity.session_id, &client_key, &text) {
            Ok(run_id) => run_id,
            Err(error) => {
                self.report.fallbacks += 1;
                return self.fail_turn(socket, "turn_refused", &error, true).await;
            }
        };
        let outcome = self
            .stream_answer(socket, speech, turns, &run_id, turn_started)
            .await;
        if let Some(span) = device_latency.speech_detection_ms {
            self.report.latency.speech_detection.observe(span);
        }
        if let Some(span) = device_latency.capture_ms {
            self.report.latency.capture.observe(span);
        }
        if let Some(span) = device_latency.upload_ms {
            self.report.latency.upload.observe(span);
        }
        self.report.latency.end_to_end.observe(
            device_latency
                .device_side_ms()
                .saturating_add(elapsed_ms(turn_started)),
        );
        outcome
    }

    async fn stream_answer(
        &mut self,
        socket: &mut dyn TalkSocket,
        speech: &dyn TalkSpeech,
        turns: &dyn TalkTurns,
        run_id: &str,
        turn_started: std::time::Instant,
    ) -> Result<(), String> {
        let mut chunker = SpeechChunker::default();
        let mut cursor = 0u64;
        let mut spoken_bytes = 0usize;
        let mut speaking = false;
        let mut waited_ms = 0u64;
        let mut since_grant_check_ms = 0u64;
        let mut first_token_seen = false;
        let mut first_audio_seen = false;
        loop {
            // A grant is withdrawn by an operator, not by the device, so a
            // silent phone must not be able to hold the microphone open for the
            // length of a ten-minute answer. The check is on a timer of its own
            // rather than on frame arrival for exactly that reason.
            if since_grant_check_ms >= GRANT_RECHECK_INTERVAL_MS
                && !turns.still_granted(&self.identity.device_id)
            {
                self.report.grant_revoked = true;
                self.revoked = true;
                // Stop the run as well: the authority to listen and the
                // authority to keep answering into a closed microphone went
                // away at the same moment.
                let _ = turns.cancel(run_id);
                return self.report_revocation(socket).await;
            }
            if since_grant_check_ms >= GRANT_RECHECK_INTERVAL_MS {
                since_grant_check_ms = 0;
            }
            // Barge-in is read *between* polls rather than waited on, so the
            // rest of an answer is dropped as soon as the user starts talking
            // over it. A frame already handed to the socket cannot be recalled;
            // everything after it can.
            if let Some(raw) = try_recv(socket).await {
                match self.classify_during_playback(&raw) {
                    Interjection::Metrics(latency) => {
                        self.pending_device_latency = latency;
                    }
                    Interjection::Interrupt => {
                        self.report.interruptions += 1;
                        // Truthful order: stop speaking, then ask the run to
                        // stop. A tool call that already ran is not undone, and
                        // nothing here says it was.
                        let _ = turns.cancel(run_id);
                        chunker.reset();
                        self.emit(
                            socket,
                            TalkServerFrameKind::State {
                                state: TalkState::Interrupted,
                            },
                        )
                        .await?;
                        return self
                            .emit(
                                socket,
                                TalkServerFrameKind::State {
                                    state: TalkState::Listening,
                                },
                            )
                            .await;
                    }
                    Interjection::Refused(error) => {
                        self.report.errors += 1;
                        self.emit(
                            socket,
                            TalkServerFrameKind::Error {
                                code: "invalid_frame".into(),
                                message: error,
                                retryable: false,
                            },
                        )
                        .await?;
                    }
                    Interjection::Ignored => {}
                }
            }

            let progress = match turns.progress(run_id, cursor) {
                Ok(progress) => progress,
                Err(error) => return self.fail_turn(socket, "run_unreadable", &error, true).await,
            };
            cursor = progress.next_index;
            if !progress.delta.is_empty() {
                if !first_token_seen {
                    first_token_seen = true;
                    self.report
                        .latency
                        .model_first_token
                        .observe(elapsed_ms(turn_started));
                }
                self.emit(
                    socket,
                    TalkServerFrameKind::AssistantDelta {
                        text: bounded(&progress.delta, MAX_TALK_TEXT_BYTES),
                    },
                )
                .await?;
                for chunk in chunker.push(&progress.delta, false) {
                    if spoken_bytes + chunk.len() > MAX_SPOKEN_TEXT_BYTES {
                        break;
                    }
                    spoken_bytes += chunk.len();
                    if !speaking {
                        speaking = true;
                        self.emit(
                            socket,
                            TalkServerFrameKind::State {
                                state: TalkState::Speaking,
                            },
                        )
                        .await?;
                    }
                    let spoken_before = self.report.spoken_chunks;
                    self.speak(socket, speech, &chunk).await?;
                    if !first_audio_seen && self.report.spoken_chunks > spoken_before {
                        first_audio_seen = true;
                        self.report
                            .latency
                            .tts_first_audio
                            .observe(elapsed_ms(turn_started));
                    }
                }
            }
            if progress.finished {
                for chunk in chunker.push("", true) {
                    if spoken_bytes + chunk.len() > MAX_SPOKEN_TEXT_BYTES {
                        break;
                    }
                    spoken_bytes += chunk.len();
                    if !speaking {
                        speaking = true;
                        self.emit(
                            socket,
                            TalkServerFrameKind::State {
                                state: TalkState::Speaking,
                            },
                        )
                        .await?;
                    }
                    self.speak(socket, speech, &chunk).await?;
                }
                if let Some(error) = progress.error {
                    self.report.fallbacks += 1;
                    return self.fail_turn(socket, "run_failed", &error, true).await;
                }
                self.report.turns_submitted += 1;
                return self
                    .emit(
                        socket,
                        TalkServerFrameKind::State {
                            state: TalkState::Listening,
                        },
                    )
                    .await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(RUN_POLL_INTERVAL_MS)).await;
            waited_ms = waited_ms.saturating_add(RUN_POLL_INTERVAL_MS);
            since_grant_check_ms = since_grant_check_ms.saturating_add(RUN_POLL_INTERVAL_MS);
            if waited_ms >= MAX_TURN_WAIT_MS {
                // The run is durable and still going; this session simply stops
                // narrating it. Saying so is more honest than a silent return
                // to listening.
                return self
                    .fail_turn(
                        socket,
                        "turn_still_running",
                        "This turn is still running and can be followed in the session; \
                         Talk stopped waiting for it.",
                        true,
                    )
                    .await;
            }
        }
    }

    async fn speak(
        &mut self,
        socket: &mut dyn TalkSocket,
        speech: &dyn TalkSpeech,
        chunk: &str,
    ) -> Result<(), String> {
        let (bytes, media_type) = match speech.synthesize(chunk).await {
            Ok(value) => value,
            Err(error) => {
                // The answer is on screen either way; only the voice is missing.
                self.report.errors += 1;
                self.report.fallbacks += 1;
                return self
                    .emit(
                        socket,
                        TalkServerFrameKind::Error {
                            code: "synthesis_failed".into(),
                            message: bounded(&error, 1_024),
                            retryable: true,
                        },
                    )
                    .await;
            }
        };
        if bytes.is_empty() || bytes.len() > MAX_TALK_AUDIO_BYTES {
            self.report.errors += 1;
            return self
                .emit(
                    socket,
                    TalkServerFrameKind::Error {
                        code: "synthesis_too_large".into(),
                        message: "Synthesized speech passed this runner's frame ceiling.".into(),
                        retryable: true,
                    },
                )
                .await;
        }
        self.outbound_audio_sequence += 1;
        self.report.spoken_chunks += 1;
        let audio_sequence = self.outbound_audio_sequence;
        self.emit(
            socket,
            TalkServerFrameKind::OutputAudio {
                audio_sequence,
                media_type,
                audio_base64: STANDARD.encode(bytes),
            },
        )
        .await
    }

    async fn fail_turn(
        &mut self,
        socket: &mut dyn TalkSocket,
        code: &str,
        message: &str,
        retryable: bool,
    ) -> Result<(), String> {
        self.report.errors += 1;
        self.emit(
            socket,
            TalkServerFrameKind::Error {
                code: code.to_string(),
                message: bounded(message, 1_024),
                retryable,
            },
        )
        .await?;
        self.emit(
            socket,
            TalkServerFrameKind::State {
                state: TalkState::Listening,
            },
        )
        .await
    }

    /// What an inbound frame means while the assistant is answering.
    fn classify_during_playback(&mut self, raw: &str) -> Interjection {
        match self.parse(raw) {
            Ok(frame) => match frame.kind {
                TalkClientFrameKind::Interrupt { .. } => Interjection::Interrupt,
                // Audio arriving mid-answer *is* the user talking over it. The
                // device's own detector already decided it was speech, so this
                // is the same event spelled differently, and treating it as
                // anything else would leave the assistant talking into it.
                //
                // The bytes are kept rather than dropped: what the user said to
                // interrupt is the beginning of what they want answered next,
                // and making them say it twice is the whole reason barge-in
                // feels broken when it is done the other way.
                TalkClientFrameKind::Audio {
                    media_type,
                    audio_base64,
                    last,
                    ..
                } => {
                    self.retain_interrupting_audio(&media_type, &audio_base64, last);
                    Interjection::Interrupt
                }
                TalkClientFrameKind::Metrics {
                    speech_detection_ms,
                    capture_ms,
                    upload_ms,
                } => Interjection::Metrics(DeviceLatency {
                    speech_detection_ms,
                    capture_ms,
                    upload_ms,
                }),
                _ => Interjection::Ignored,
            },
            Err(error) => Interjection::Refused(error),
        }
    }

    /// Hold the audio that interrupted an answer so the next turn starts with
    /// it. A payload that will not decode, or that would push the buffer past
    /// the ceiling, is dropped — the interruption still stands.
    fn retain_interrupting_audio(&mut self, media_type: &str, audio_base64: &str, last: bool) {
        let Ok(bytes) = STANDARD.decode(audio_base64) else {
            return;
        };
        if self.utterance.len() + bytes.len() > MAX_TALK_UTTERANCE_BYTES {
            return;
        }
        self.utterance.extend_from_slice(&bytes);
        if self.utterance_media_type.is_none() {
            self.utterance_media_type = Some(media_type.to_string());
        }
        self.interrupting_utterance_complete = last;
    }

    /// Say plainly that the authority to listen is gone, then let the caller
    /// close the socket.
    async fn report_revocation(&mut self, socket: &mut dyn TalkSocket) -> Result<(), String> {
        let _ = self
            .emit(
                socket,
                TalkServerFrameKind::Error {
                    code: "capability_revoked".into(),
                    message: "The voice_stream grant was withdrawn.".into(),
                    retryable: false,
                },
            )
            .await;
        self.emit(
            socket,
            TalkServerFrameKind::State {
                state: TalkState::Idle,
            },
        )
        .await
    }

    fn observe_device_latency(
        &mut self,
        speech_detection_ms: Option<u64>,
        capture_ms: Option<u64>,
        upload_ms: Option<u64>,
    ) {
        self.pending_device_latency = DeviceLatency {
            speech_detection_ms,
            capture_ms,
            upload_ms,
        };
    }
}

enum Interjection {
    Interrupt,
    Metrics(DeviceLatency),
    Refused(String),
    Ignored,
}

fn elapsed_ms(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis())
        .unwrap_or(MAX_TALK_LATENCY_MS)
        .min(MAX_TALK_LATENCY_MS)
}

/// One frame if the device already sent one, without waiting for it.
async fn try_recv(socket: &mut dyn TalkSocket) -> Option<String> {
    // A zero-length timeout is the cancellation point: `recv` is polled once,
    // and a socket with nothing buffered yields immediately.
    tokio::time::timeout(std::time::Duration::from_millis(1), socket.recv())
        .await
        .ok()
        .flatten()
}

fn bounded(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct ScriptedSocket {
        inbound: VecDeque<String>,
        sent: Arc<Mutex<Vec<TalkServerFrame>>>,
    }

    impl ScriptedSocket {
        fn new(inbound: Vec<String>) -> Self {
            Self {
                inbound: inbound.into(),
                sent: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl TalkSocket for ScriptedSocket {
        async fn recv(&mut self) -> Option<String> {
            self.inbound.pop_front()
        }

        async fn send(&mut self, frame: String) -> Result<(), String> {
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::from_str(&frame).expect("server frames are valid json"));
            Ok(())
        }
    }

    struct FakeSpeech {
        heard: &'static str,
        /// Every audio payload handed to transcription, so a test can prove the
        /// chunks were reassembled in order.
        transcribed: Arc<Mutex<Vec<Vec<u8>>>>,
        spoken: Arc<Mutex<Vec<String>>>,
        fail: bool,
        /// Flipped to `false` while transcription is in flight, so a test can
        /// put the revocation exactly where a long local Whisper run would put
        /// it rather than only between turns.
        revoke_while_transcribing: Option<Arc<Mutex<bool>>>,
    }

    impl FakeSpeech {
        fn new(heard: &'static str) -> Self {
            Self {
                heard,
                transcribed: Arc::new(Mutex::new(Vec::new())),
                spoken: Arc::new(Mutex::new(Vec::new())),
                fail: false,
                revoke_while_transcribing: None,
            }
        }
    }

    #[async_trait]
    impl TalkSpeech for FakeSpeech {
        async fn transcribe(&self, audio: Vec<u8>, _media_type: &str) -> Result<String, String> {
            self.transcribed.lock().unwrap().push(audio);
            if let Some(granted) = &self.revoke_while_transcribing {
                *granted.lock().unwrap() = false;
            }
            if self.fail {
                return Err("no transcription backend is configured".to_string());
            }
            Ok(self.heard.to_string())
        }

        async fn synthesize(&self, text: &str) -> Result<(Vec<u8>, String), String> {
            self.spoken.lock().unwrap().push(text.to_string());
            Ok((text.as_bytes().to_vec(), "audio/wav".to_string()))
        }
    }

    #[derive(Default)]
    struct FakeTurns {
        submitted: Mutex<Vec<(String, String)>>,
        cancelled: Mutex<Vec<String>>,
        /// Answer delivered one delta per poll, so streaming is exercised.
        deltas: Mutex<VecDeque<String>>,
        granted: Arc<Mutex<bool>>,
        refuse: bool,
        /// Withdraw the grant after this many polls of a running answer, which
        /// is how a revocation lands while the runner is thinking or speaking
        /// rather than between turns.
        revoke_after_polls: Mutex<Option<u32>>,
        polls: Mutex<u32>,
    }

    impl FakeTurns {
        fn answering(parts: &[&str]) -> Self {
            Self {
                deltas: Mutex::new(parts.iter().map(|part| part.to_string()).collect()),
                granted: Arc::new(Mutex::new(true)),
                ..Self::default()
            }
        }
    }

    impl TalkTurns for FakeTurns {
        fn submit(&self, _session: &str, client_key: &str, text: &str) -> Result<String, String> {
            if self.refuse {
                return Err("the queue refused this turn".to_string());
            }
            self.submitted
                .lock()
                .unwrap()
                .push((client_key.to_string(), text.to_string()));
            Ok(format!("run-{client_key}"))
        }

        fn progress(&self, _run_id: &str, from_index: u64) -> Result<TalkRunProgress, String> {
            {
                let mut polls = self.polls.lock().unwrap();
                *polls += 1;
                if let Some(after) = *self.revoke_after_polls.lock().unwrap() {
                    if *polls >= after {
                        *self.granted.lock().unwrap() = false;
                    }
                }
            }
            let mut deltas = self.deltas.lock().unwrap();
            match deltas.pop_front() {
                Some(delta) => Ok(TalkRunProgress {
                    delta,
                    next_index: from_index + 1,
                    finished: deltas.is_empty(),
                    error: None,
                }),
                None => Ok(TalkRunProgress {
                    next_index: from_index,
                    finished: true,
                    ..TalkRunProgress::default()
                }),
            }
        }

        fn cancel(&self, run_id: &str) -> Result<(), String> {
            self.cancelled.lock().unwrap().push(run_id.to_string());
            Ok(())
        }

        fn still_granted(&self, _device_id: &str) -> bool {
            *self.granted.lock().unwrap()
        }
    }

    /// A generation minted the way the ticket route mints it, so the tests
    /// exercise the entropy length the validator actually enforces.
    fn generation() -> String {
        super::super::protocol::TalkTicketResponse::issue("session-one", 1_000, 30_000)
            .unwrap()
            .session_generation
    }

    fn identity() -> TalkIdentity {
        TalkIdentity {
            device_id: "device-one".into(),
            session_id: "session-one".into(),
            session_generation: generation(),
        }
    }

    fn client_frame(identity: &TalkIdentity, sequence: u64, kind: TalkClientFrameKind) -> String {
        serde_json::to_string(&TalkClientFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: identity.session_id.clone(),
            session_generation: identity.session_generation.clone(),
            frame_sequence: sequence,
            kind,
        })
        .unwrap()
    }

    fn hello(identity: &TalkIdentity) -> String {
        client_frame(
            identity,
            1,
            TalkClientFrameKind::Hello {
                media_type: "audio/webm;codecs=opus".into(),
                sample_rate_hz: 48_000,
                channels: 1,
            },
        )
    }

    fn audio(
        identity: &TalkIdentity,
        sequence: u64,
        audio_sequence: u64,
        payload: &[u8],
        last: bool,
    ) -> String {
        client_frame(
            identity,
            sequence,
            TalkClientFrameKind::Audio {
                audio_sequence,
                media_type: "audio/webm;codecs=opus".into(),
                audio_base64: STANDARD.encode(payload),
                last,
            },
        )
    }

    fn states(frames: &[TalkServerFrame]) -> Vec<TalkState> {
        frames
            .iter()
            .filter_map(|frame| match &frame.kind {
                TalkServerFrameKind::State { state } => Some(*state),
                _ => None,
            })
            .collect()
    }

    /// The whole point of the module: what somebody says becomes exactly one
    /// ordinary turn, its answer streams back as text, and that text is spoken.
    #[tokio::test]
    async fn one_utterance_becomes_one_turn_and_its_answer_is_spoken() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"first-", false),
            audio(&identity, 3, 2, b"second", true),
        ]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("what is the deploy status");
        let turns = FakeTurns::answering(&["The deploy finished. ", "Nothing is pending."]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.utterances, 1);
        assert_eq!(report.turns_submitted, 1);
        assert_eq!(
            speech.transcribed.lock().unwrap().as_slice(),
            [b"first-second".to_vec()],
            "chunks are reassembled in order before transcription"
        );
        let submitted = turns.submitted.lock().unwrap().clone();
        assert_eq!(submitted.len(), 1);
        assert_eq!(submitted[0].1, "what is the deploy status");
        let frames = sent.lock().unwrap().clone();
        assert!(matches!(frames[0].kind, TalkServerFrameKind::Ready));
        assert!(frames.iter().any(|frame| matches!(
            &frame.kind,
            TalkServerFrameKind::Transcript { text, is_final: true }
                if text == "what is the deploy status"
        )));
        assert_eq!(
            states(&frames),
            [
                TalkState::Listening,
                TalkState::Transcribing,
                TalkState::Thinking,
                TalkState::Speaking,
                TalkState::Listening,
            ]
        );
        assert!(
            speech
                .spoken
                .lock()
                .unwrap()
                .iter()
                .any(|chunk| chunk == "The deploy finished."),
            "a finished sentence is spoken before the answer is complete"
        );
        assert!(report.spoken_chunks >= 2);
        // Every server frame carries a strictly increasing sequence.
        let sequences: Vec<u64> = frames.iter().map(|frame| frame.frame_sequence).collect();
        assert!(sequences.windows(2).all(|pair| pair[1] > pair[0]));
    }

    /// Silence must never become a turn. The device's detector can be wrong;
    /// an empty transcript is where that stops.
    #[tokio::test]
    async fn an_empty_transcript_never_becomes_a_turn() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"quiet", true),
        ]);
        let speech = FakeSpeech::new("   ");
        let turns = FakeTurns::answering(&["unused"]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.utterances, 1);
        assert_eq!(report.turns_submitted, 0);
        assert!(turns.submitted.lock().unwrap().is_empty());
    }

    /// Barge-in: talking over the answer stops the speech, cancels the run and
    /// returns to listening rather than finishing the sentence.
    #[tokio::test]
    async fn talking_over_the_answer_stops_it_and_cancels_the_run() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"question", true),
            client_frame(
                &identity,
                3,
                TalkClientFrameKind::Interrupt {
                    reason: Some("barge_in".into()),
                },
            ),
        ]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("keep going");
        let turns = FakeTurns::answering(&["one. ", "two. ", "three. ", "four. "]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.interruptions, 1);
        assert_eq!(
            turns.cancelled.lock().unwrap().len(),
            1,
            "the still-running turn is cancelled through the ordinary path"
        );
        assert!(states(&sent.lock().unwrap()).contains(&TalkState::Interrupted));
        assert_eq!(
            report.turns_submitted, 0,
            "an interrupted turn is not reported as completed"
        );
    }

    /// A frame replayed from an earlier socket must not be accepted, even
    /// though it is otherwise perfectly formed.
    #[tokio::test]
    async fn a_frame_from_another_generation_or_a_replayed_sequence_is_refused() {
        let identity = identity();
        let mut stale = identity.clone();
        stale.session_generation = generation();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&stale, 2, 1, b"captured", true),
            // The hello's own sequence, sent again: a frame this socket has
            // already consumed must not be accepted a second time.
            audio(&identity, 1, 1, b"replayed-sequence", true),
        ]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("never heard");
        let turns = FakeTurns::answering(&["unused"]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.utterances, 0, "neither frame reached transcription");
        assert!(speech.transcribed.lock().unwrap().is_empty());
        let refusals: Vec<String> = sent
            .lock()
            .unwrap()
            .iter()
            .filter_map(|frame| match &frame.kind {
                TalkServerFrameKind::Error { code, message, .. } if code == "invalid_frame" => {
                    Some(message.clone())
                }
                _ => None,
            })
            .collect();
        assert_eq!(refusals.len(), 2);
        assert!(refusals[0].contains("earlier Talk session"));
        assert!(refusals[1].contains("sequence"));
    }

    /// Audio before the hello is refused: the session has not agreed a format
    /// yet, and accepting it would mean guessing one.
    #[tokio::test]
    async fn audio_before_the_hello_is_refused() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![audio(&identity, 1, 1, b"early", true)]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("never heard");
        let turns = FakeTurns::answering(&["unused"]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.utterances, 0);
        assert!(sent.lock().unwrap().iter().any(|frame| matches!(
            &frame.kind,
            TalkServerFrameKind::Error { message, .. } if message.contains("hello")
        )));
    }

    /// A speech backend nobody configured ends the turn with a reason on the
    /// screen, not the session.
    #[tokio::test]
    async fn a_failed_transcription_ends_the_turn_and_not_the_session() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"speech", true),
            audio(&identity, 3, 2, b"more speech", true),
        ]);
        let sent = socket.sent.clone();
        let mut speech = FakeSpeech::new("unused");
        speech.fail = true;
        let turns = FakeTurns::answering(&["unused"]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.utterances, 2, "the session kept listening");
        assert_eq!(report.errors, 2);
        let frames = sent.lock().unwrap().clone();
        assert!(frames.iter().any(|frame| matches!(
            &frame.kind,
            TalkServerFrameKind::Error { code, retryable, .. }
                if code == "transcription_failed" && *retryable
        )));
        assert_eq!(
            states(&frames).last(),
            Some(&TalkState::Listening),
            "a failed turn returns to listening"
        );
    }

    /// Withdrawing `voice_stream` mid-conversation closes the microphone.
    #[tokio::test]
    async fn revoking_the_grant_closes_the_session() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"speech", true),
        ]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("never heard");
        let turns = FakeTurns::answering(&["unused"]);
        *turns.granted.lock().unwrap() = false;

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert!(report.grant_revoked);
        assert_eq!(report.utterances, 0);
        assert!(sent.lock().unwrap().iter().any(|frame| matches!(
            &frame.kind,
            TalkServerFrameKind::Error { code, .. } if code == "capability_revoked"
        )));
    }

    /// **Revocation reaches every phase of a turn, not only the gap between
    /// two of them.**
    ///
    /// The grant used to be read in exactly one place: the top of the receive
    /// loop. A device that is silent — which is what a device is for the whole
    /// of a long answer — never went round that loop, so withdrawing
    /// `voice_stream` while the runner was transcribing, thinking or speaking
    /// left the microphone open until the answer finished or the hour ran out.
    /// "Revoked at the next frame the user happens to send" is not revoked.
    #[tokio::test]
    async fn a_withdrawn_grant_ends_the_session_in_whichever_phase_it_lands() {
        // While transcribing: the words are never submitted at all.
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"speech", true),
        ]);
        let turns = FakeTurns::answering(&["An answer nobody asked for."]);
        let mut speech = FakeSpeech::new("what is the deploy status");
        speech.revoke_while_transcribing = Some(Arc::clone(&turns.granted));

        let report = run_talk_session(&mut socket, &speech, &turns, identity.clone()).await;

        assert!(report.grant_revoked);
        assert!(
            turns.submitted.lock().unwrap().is_empty(),
            "a grant withdrawn during transcription must not let the words become a turn"
        );

        // While thinking or speaking: the answer stops and the run is asked to
        // stop with it, without another frame from the device.
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"speech", true),
        ]);
        let sent = socket.sent.clone();
        let speech = FakeSpeech::new("what is the deploy status");
        // Long enough that the streaming loop's own grant timer comes round
        // mid-answer, which is the property under test: the revocation must not
        // wait for the turn to end.
        let answer: Vec<&str> = vec!["Another sentence. "; 24];
        let turns = FakeTurns::answering(&answer);
        *turns.revoke_after_polls.lock().unwrap() = Some(2);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert!(report.grant_revoked);
        assert_eq!(
            turns.submitted.lock().unwrap().len(),
            1,
            "the turn was submitted before the grant went away"
        );
        assert_eq!(
            turns.cancelled.lock().unwrap().len(),
            1,
            "the authority to listen and the authority to keep answering end together"
        );
        assert!(
            speech.spoken.lock().unwrap().len() < answer.len(),
            "speaking stops at the revocation rather than finishing the answer"
        );
        assert!(sent.lock().unwrap().iter().any(|frame| matches!(
            &frame.kind,
            TalkServerFrameKind::Error { code, .. } if code == "capability_revoked"
        )));
    }

    /// The words somebody interrupts with are the next question, and the runner
    /// keeps them rather than making the operator say them twice.
    #[tokio::test]
    async fn audio_that_interrupts_an_answer_becomes_the_next_turn() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            audio(&identity, 2, 1, b"first question", true),
            audio(&identity, 3, 2, b"second question", true),
        ]);
        let speech = FakeSpeech::new("tell me about staging");
        let turns = FakeTurns::answering(&["A long answer. ", "It keeps going. "]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.interruptions, 1);
        assert_eq!(
            turns.submitted.lock().unwrap().len(),
            2,
            "the interrupting utterance becomes a second durable turn"
        );
        let heard = speech.transcribed.lock().unwrap();
        assert_eq!(heard.len(), 2);
        assert_eq!(
            heard[1], b"second question",
            "the interrupting audio is transcribed, not discarded"
        );
    }

    /// Telemetry the device sends is folded into the session's counters, and
    /// there is nowhere in the shape for a word of it to hide.
    #[tokio::test]
    async fn device_latency_is_kept_as_durations_and_nothing_else() {
        let identity = identity();
        let mut socket = ScriptedSocket::new(vec![
            hello(&identity),
            client_frame(
                &identity,
                2,
                TalkClientFrameKind::Metrics {
                    speech_detection_ms: Some(180),
                    capture_ms: Some(1_200),
                    upload_ms: Some(40),
                },
            ),
            audio(&identity, 3, 1, b"speech", true),
        ]);
        let speech = FakeSpeech::new("what is the deploy status");
        let turns = FakeTurns::answering(&["Done."]);

        let report = run_talk_session(&mut socket, &speech, &turns, identity).await;

        assert_eq!(report.latency.speech_detection.samples, 1);
        assert_eq!(report.latency.speech_detection.worst_ms, 180);
        assert_eq!(report.latency.capture.worst_ms, 1_200);
        assert_eq!(report.latency.upload.worst_ms, 40);
        assert_eq!(report.latency.transcription.samples, 1);
        assert_eq!(report.latency.model_first_token.samples, 1);
        assert!(
            report.latency.end_to_end.worst_ms >= 1_240,
            "end to end includes what the device spent before the runner heard anything"
        );
    }

    #[test]
    fn code_blocks_urls_and_half_written_links_never_reach_the_synthesizer() {
        let mut chunker = SpeechChunker::default();
        assert!(
            chunker
                .push("Here is the fix:\n```rust\nfn main() {}\n```\n", false)
                .iter()
                .all(|chunk| !chunk.contains("fn main")),
            "a fenced block is skipped rather than read out"
        );

        let mut chunker = SpeechChunker::default();
        let chunks = chunker.push("See https://example.com/deploy for detail. ", true);
        assert_eq!(chunks, ["See for detail."]);

        // A link that has not finished arriving waits instead of being spoken
        // as punctuation.
        let mut chunker = SpeechChunker::default();
        assert!(chunker.push("Read the [release notes", false).is_empty());
        let finished = chunker.push("](https://example.com). ", false);
        assert_eq!(finished, ["Read the release notes ."]);
    }

    #[test]
    fn speech_is_cut_on_sentence_boundaries_rather_than_on_arrival() {
        let mut chunker = SpeechChunker::default();
        assert!(
            chunker.push("The deploy fin", false).is_empty(),
            "half a sentence waits"
        );
        assert_eq!(
            chunker.push("ished. And then", false),
            ["The deploy finished."]
        );
        assert_eq!(chunker.push("", true), ["And then"]);
    }
}

//! The audio arithmetic a phone call needs, and nothing else.
//!
//! Carriers stream 8 kHz mono G.711 µ-law: one byte per sample, 160 samples per
//! 20 ms frame. Everything speech-related in this app works on 16-bit PCM WAV,
//! so this module is the translation between the two, plus the one decision
//! that cannot be made anywhere else — when the person on the phone has stopped
//! talking.
//!
//! Kept free of IO and of the store so it can be tested as arithmetic, which is
//! what it is.

/// Sample rate every carrier media stream uses.
pub(crate) const CALL_SAMPLE_RATE: u32 = 8_000;

/// Decode one µ-law byte to a 16-bit sample. The inverse of [`encode_mulaw`];
/// both follow the G.711 table exactly rather than approximating it, because a
/// half-right codec sounds like a broken line rather than like a bug.
pub(crate) fn decode_mulaw(byte: u8) -> i16 {
    let value = !byte;
    let sign = value & 0x80;
    let exponent = (value >> 4) & 0x07;
    let mantissa = value & 0x0F;
    let magnitude = ((i32::from(mantissa) << 3) + 0x84) << exponent;
    let sample = magnitude - 0x84;
    let sample = if sign != 0 { -sample } else { sample };
    sample.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// Encode one 16-bit sample as µ-law.
pub(crate) fn encode_mulaw(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32_635;
    let mut sample = i32::from(sample);
    let sign = if sample < 0 {
        sample = -sample;
        0x80
    } else {
        0x00
    };
    let sample = sample.min(CLIP) + BIAS;
    let exponent = (0..8)
        .rev()
        .find(|exponent| sample >= (1 << (exponent + 7)))
        .unwrap_or(0);
    let mantissa = (sample >> (exponent + 3)) & 0x0F;
    !(sign as u8 | ((exponent as u8) << 4) | mantissa as u8)
}

/// A minimal PCM WAV file: 16-bit, mono, `sample_rate`.
///
/// Written by hand rather than with a crate because the header is 44 bytes and
/// every field of it is fixed by the one format this pipeline produces.
pub(crate) fn write_wav(samples: &[i16], sample_rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

/// Read a 16-bit PCM WAV back to mono samples at [`CALL_SAMPLE_RATE`].
///
/// Only what a system speech synthesizer emits is supported: PCM, 16-bit, one
/// or two channels, any rate. Anything else is an error rather than a guess,
/// because guessing wrong produces noise on somebody's phone.
// ponytail: nearest-sample resampling and channel averaging, no filter. Phone
// audio is band-limited to 3.4 kHz anyway; add a proper low-pass if downsampled
// speech ever sounds harsh.
pub(crate) fn read_wav_as_call_audio(bytes: &[u8]) -> Result<Vec<i16>, String> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("Synthesized speech is not a WAV file".to_string());
    }
    let mut cursor = 12;
    let mut format = None;
    let mut data = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes([
            bytes[cursor + 4],
            bytes[cursor + 5],
            bytes[cursor + 6],
            bytes[cursor + 7],
        ]) as usize;
        let body = cursor + 8;
        let end = body.saturating_add(size).min(bytes.len());
        match id {
            b"fmt " if size >= 16 => {
                let channels = u16::from_le_bytes([bytes[body + 2], bytes[body + 3]]);
                let rate = u32::from_le_bytes([
                    bytes[body + 4],
                    bytes[body + 5],
                    bytes[body + 6],
                    bytes[body + 7],
                ]);
                let bits = u16::from_le_bytes([bytes[body + 14], bytes[body + 15]]);
                if bits != 16 || channels == 0 || channels > 2 || rate == 0 {
                    return Err(format!(
                        "Synthesized speech is {bits}-bit with {channels} channel(s), which this pipeline cannot send"
                    ));
                }
                format = Some((channels as usize, rate));
            }
            b"data" => data = Some(&bytes[body..end]),
            _ => {}
        }
        // Chunks are word-aligned.
        cursor = body + size + (size % 2);
    }
    let (channels, rate) = format.ok_or("Synthesized speech has no format chunk")?;
    let data = data.ok_or("Synthesized speech has no audio data")?;
    let frames: Vec<i16> = data
        .chunks_exact(2 * channels)
        .map(|frame| {
            let total: i32 = frame
                .chunks_exact(2)
                .map(|sample| i32::from(i16::from_le_bytes([sample[0], sample[1]])))
                .sum();
            (total / channels as i32) as i16
        })
        .collect();
    if rate == CALL_SAMPLE_RATE {
        return Ok(frames);
    }
    let ratio = f64::from(rate) / f64::from(CALL_SAMPLE_RATE);
    let out_len = (frames.len() as f64 / ratio) as usize;
    Ok((0..out_len)
        .map(|index| frames[((index as f64) * ratio) as usize])
        .collect())
}

/// Decides when the caller has finished saying something.
///
/// A phone call has no "send" button, so the end of a turn is inferred from
/// silence. Everything is counted in samples rather than wall-clock time: the
/// carrier's own frame rate is the clock, which keeps the decision identical in
/// a test and on a live call.
pub(crate) struct UtteranceDetector {
    samples: Vec<i16>,
    trailing_silent_samples: usize,
    heard_speech: bool,
    silence_threshold: i32,
    hangover_samples: usize,
    max_samples: usize,
}

/// What one batch of carrier audio produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UtteranceProgress {
    /// Still listening.
    Listening,
    /// A complete utterance, ready to transcribe.
    Complete(Vec<i16>),
}

impl UtteranceDetector {
    /// `hangover_ms` is how much silence ends a turn, `max_ms` the point at
    /// which a monologue is cut and transcribed anyway so the caller is never
    /// left talking to a machine that has stopped listening.
    pub(crate) fn new(hangover_ms: u32, max_ms: u32) -> Self {
        let per_ms = CALL_SAMPLE_RATE as usize / 1_000;
        Self {
            samples: Vec::new(),
            trailing_silent_samples: 0,
            heard_speech: false,
            // Roughly -40 dBFS: below this a µ-law line is line noise, not
            // speech.
            silence_threshold: 320,
            hangover_samples: hangover_ms as usize * per_ms,
            max_samples: max_ms as usize * per_ms,
        }
    }

    pub(crate) fn push(&mut self, frame: &[i16]) -> UtteranceProgress {
        for &sample in frame {
            let loud = i32::from(sample).abs() > self.silence_threshold;
            if loud {
                self.heard_speech = true;
                self.trailing_silent_samples = 0;
            } else {
                self.trailing_silent_samples += 1;
            }
            // Leading silence is dropped rather than transcribed: it is the
            // gap before somebody speaks, and it is most of a phone call.
            if self.heard_speech {
                self.samples.push(sample);
            }
        }
        let ended = self.heard_speech
            && (self.trailing_silent_samples >= self.hangover_samples
                || self.samples.len() >= self.max_samples);
        if ended {
            UtteranceProgress::Complete(self.take())
        } else {
            UtteranceProgress::Listening
        }
    }

    fn take(&mut self) -> Vec<i16> {
        self.heard_speech = false;
        self.trailing_silent_samples = 0;
        std::mem::take(&mut self.samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mulaw_round_trips_within_its_own_quantization() {
        for sample in [-32_000i16, -8_000, -100, 0, 100, 8_000, 32_000] {
            let decoded = decode_mulaw(encode_mulaw(sample));
            let error = (i32::from(decoded) - i32::from(sample)).abs();
            assert!(
                error <= (i32::from(sample).abs() / 8).max(200),
                "{sample} came back as {decoded}"
            );
        }
    }

    #[test]
    fn a_written_wav_reads_back_as_the_same_audio() {
        let samples: Vec<i16> = (0..800).map(|index| (index * 40) as i16).collect();
        let wav = write_wav(&samples, CALL_SAMPLE_RATE);

        let read = read_wav_as_call_audio(&wav).expect("read");

        assert_eq!(read, samples);
    }

    #[test]
    fn a_higher_rate_wav_is_brought_down_to_the_call_rate() {
        let samples: Vec<i16> = (0..1_600).map(|index| (index % 100) as i16).collect();
        let wav = write_wav(&samples, 16_000);

        let read = read_wav_as_call_audio(&wav).expect("read");

        assert_eq!(read.len(), 800, "16 kHz halves into 8 kHz");
    }

    #[test]
    fn audio_that_is_not_16_bit_pcm_is_refused_rather_than_guessed_at() {
        let mut wav = write_wav(&[0, 1, 2], CALL_SAMPLE_RATE);
        wav[34] = 8; // bits per sample
        assert!(read_wav_as_call_audio(&wav)
            .expect_err("refused")
            .contains("8-bit"));
        assert!(read_wav_as_call_audio(b"not a wav at all")
            .expect_err("refused")
            .contains("not a WAV"));
    }

    fn speech(samples: usize) -> Vec<i16> {
        (0..samples)
            .map(|index| if index % 2 == 0 { 8_000 } else { -8_000 })
            .collect()
    }

    #[test]
    fn silence_after_speech_ends_the_turn() {
        let mut detector = UtteranceDetector::new(500, 15_000);

        assert_eq!(
            detector.push(&speech(4_000)),
            UtteranceProgress::Listening,
            "half a second of talking is not a finished turn"
        );
        // 400 ms of quiet is a pause, 500 ms is the end.
        assert_eq!(detector.push(&vec![0; 3_200]), UtteranceProgress::Listening);
        let UtteranceProgress::Complete(utterance) = detector.push(&vec![0; 1_600]) else {
            panic!("expected the turn to end");
        };
        assert!(utterance.len() >= 4_000);
    }

    #[test]
    fn silence_alone_never_becomes_a_turn() {
        let mut detector = UtteranceDetector::new(500, 15_000);

        // Ten seconds of an open line with nobody talking.
        for _ in 0..10 {
            assert_eq!(
                detector.push(&vec![0; 8_000]),
                UtteranceProgress::Listening,
                "a quiet line must not be transcribed"
            );
        }
    }

    #[test]
    fn a_monologue_is_cut_at_the_maximum_rather_than_never_answered() {
        let mut detector = UtteranceDetector::new(500, 2_000);

        let progress = detector.push(&speech(16_001));

        let UtteranceProgress::Complete(utterance) = progress else {
            panic!("expected the turn to be cut at its maximum");
        };
        assert!(utterance.len() >= 16_000);
    }

    #[test]
    fn the_detector_is_reusable_for_the_next_turn() {
        let mut detector = UtteranceDetector::new(500, 15_000);
        detector.push(&speech(4_000));
        detector.push(&vec![0; 4_000]);

        // Second turn: the caller speaks again after being answered.
        assert_eq!(detector.push(&speech(800)), UtteranceProgress::Listening);
        assert!(matches!(
            detector.push(&vec![0; 4_000]),
            UtteranceProgress::Complete(_)
        ));
    }
}

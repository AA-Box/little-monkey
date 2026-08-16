// --- The Talk wire, with no DOM in it --------------------------------------
//
// The frame envelope the runner accepts, the containers it accepts, and the
// local voice activity detector — lifted out of `app.js` so a test can drive
// them instead of reading them.
//
// That split is not tidiness. This client used to open the socket and send
// `audio` as its first frame; the runner refuses anything that is not a `hello`
// there, with `retryable: false`, and the client's own error handler then tore
// the session down. Mobile Talk could not work at all. A source-scanning test
// looked at `app.js` and saw nothing wrong, because nothing in it *was*
// individually wrong — the defect was the order the frames went out in. Order
// is only visible to something that runs the code, so the builders below own
// the sequence counters and refuse to produce an `audio` frame before a
// `hello`, and `mobileTalkProtocol.test.ts` drives a whole session through
// them.

export const TALK_PROTOCOL_VERSION = 1;

/// Exactly `TALK_MEDIA_TYPES` in `protocol.rs`. A container that is not on this
/// list is refused outright, so guessing one is the same as dropping the
/// utterance.
export const TALK_MEDIA_TYPES = [
  "audio/webm",
  "audio/webm;codecs=opus",
  "audio/ogg",
  "audio/ogg;codecs=opus",
  "audio/mp4",
  "audio/wav",
  "audio/mpeg",
];

// What this client asks a recorder for, best first. Opus in WebM everywhere
// except Safari, which records AAC in MP4 and supports neither of the others.
const TALK_RECORDER_PREFERENCES = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4"];

// The detector's numbers. They are the desktop detector's, unchanged: a
// threshold that floats above the room, 180 ms before a noise counts as speech,
// 800 ms of quiet before an utterance is finished, and a hard cap so a stuck
// microphone cannot record forever.
export const TALK_MIN_SPEECH_MS = 180;
export const TALK_SILENCE_MS = 800;
export const TALK_MAX_UTTERANCE_MS = 90_000;
const TALK_NOISE_FLOOR_START = 0.008;
const TALK_NOISE_FLOOR_MIN = 0.0005;
const TALK_NOISE_FLOOR_MAX = 0.08;
const TALK_FLOOR_THRESHOLD = 0.012;
const TALK_THRESHOLD_FACTOR = 2.8;

// `MAX_TALK_AUDIO_BYTES` in `protocol.rs`, and how much base64 may carry it.
// One audio frame is checked here rather than after the socket has already
// refused it, because that refusal is not retryable and ends the conversation.
//
// The chunk size is a multiple of 4, so every slice but the last is standard
// base64 needing no padding and decoding to whole bytes — the runner decodes
// each frame on its own and concatenates the results, so a slice at any other
// offset would corrupt the utterance. It rounds *down* from 512 KiB rather than
// up: 699_052 characters would decode to 524_289 bytes, one past the ceiling.
export const MAX_TALK_AUDIO_BYTES = 524_288;
export const TALK_AUDIO_CHUNK_BASE64_CHARS = Math.floor(MAX_TALK_AUDIO_BYTES / 3) * 4;
export const MAX_TALK_LATENCY_MS = 600_000;

/**
 * Normalizes whatever a recorder reports into a container the runner accepts,
 * or `""` when it is none of them.
 *
 * Deliberately not a fallback: bytes in an unknown container relabelled as WebM
 * are still not WebM, and the transcriber would be handed a file whose header
 * contradicts its media type. Better to refuse the session with a sentence
 * naming the container than to send mislabelled audio somewhere.
 */
export function normalizeTalkMediaType(recorded) {
  const value = String(recorded || "").replace(/\s+/gu, "");
  if (TALK_MEDIA_TYPES.includes(value)) return value;
  // A recorder is free to spell its codecs however it likes; what matters to
  // the runner is the family the bytes are in.
  for (const family of ["audio/webm", "audio/ogg", "audio/mp4", "audio/wav", "audio/mpeg"]) {
    if (value.startsWith(family)) return family;
  }
  return "";
}

/**
 * The audio frames one utterance is sent as.
 *
 * A 90-second recording is well past what a single frame may carry, so the
 * payload is cut into frame-sized pieces the runner reassembles in order. The
 * cut is on the base64, at a multiple of 4, so each piece decodes on its own.
 */
export function splitTalkAudioBase64(audioBase64) {
  const payload = String(audioBase64 || "");
  const chunks = [];
  for (let at = 0; at < payload.length; at += TALK_AUDIO_CHUNK_BASE64_CHARS) {
    chunks.push(payload.slice(at, at + TALK_AUDIO_CHUNK_BASE64_CHARS));
  }
  return chunks;
}

/**
 * The container this browser will actually record in, decided before the socket
 * exists. Takes `MediaRecorder.isTypeSupported` rather than reading it, so the
 * choice is a pure function of what a browser says it can do.
 *
 * Returns `""` when none of the preferred types are supported — the caller then
 * asks a throwaway recorder what it chose, because a hello that names a
 * container the recorder does not produce is worse than no preference at all.
 */
export function chooseTalkMediaType(isTypeSupported) {
  const supported = typeof isTypeSupported === "function" ? isTypeSupported : () => false;
  for (const candidate of TALK_RECORDER_PREFERENCES) {
    try {
      if (supported(candidate)) return candidate;
    } catch {
      // A browser that throws on a type it does not know does not support it.
    }
  }
  return "";
}

/** The runner accepts 8 kHz to 192 kHz; a browser that reports nothing gets 48. */
export function clampTalkSampleRateHz(value) {
  if (!Number.isFinite(value)) return 48_000;
  return Math.min(192_000, Math.max(8_000, Math.round(value)));
}

/** One channel or two. A browser that reports nothing is mono, which a
 * microphone almost always is. */
export function clampTalkChannels(value) {
  if (!Number.isFinite(value)) return 1;
  return Math.min(2, Math.max(1, Math.round(value)));
}

/** The runner's own bound and character set for an utterance id. */
export const MAX_TALK_UTTERANCE_ID_CHARS = 128;

/**
 * A name for one utterance, in the only alphabet the runner accepts.
 *
 * Alphanumerics, `-` and `_`, bounded. Anything else is replaced rather than
 * refused: this value is generated on this side, so an unusable one is a bug
 * here, and turning it into a refusal would silence a conversation over a
 * character.
 */
export function normalizeUtteranceId(value) {
  const cleaned = String(value || "").replace(/[^A-Za-z0-9_-]/gu, "-");
  const bounded = cleaned.slice(0, MAX_TALK_UTTERANCE_ID_CHARS);
  return bounded || "utterance";
}

/**
 * A fresh name, from the browser's CSPRNG.
 *
 * Random rather than a counter: a counter restarts with the page, and two
 * utterances that collide are two different things somebody said arriving as
 * one turn.
 */
function defaultUtteranceId() {
  const bytes = globalThis.crypto?.getRandomValues?.(new Uint8Array(12));
  if (!bytes) {
    throw new Error("This browser has no secure random source, so Talk cannot name an utterance");
  }
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return `utt-${btoa(binary).replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/u, "")}`;
}

function boundedSpan(value) {
  if (!Number.isFinite(value)) return undefined;
  return Math.min(MAX_TALK_LATENCY_MS, Math.max(0, Math.round(value)));
}

/**
 * The one place a Talk frame is built.
 *
 * Owns both sequences, so they cannot be advanced by accident or restarted by a
 * refactor: `frame_sequence` starts at 1 (the runner refuses 0) and every frame
 * increments it, `audio_sequence` counts only audio frames. The media type is
 * fixed at construction and reused for the hello and for every audio frame —
 * they are the same value by construction rather than by agreement.
 *
 * It owns the utterance id for the same reason. The runner queues a spoken turn
 * under that id and refuses a closing audio frame without one, because nothing
 * on its side survives a restart: the session generation is minted fresh with
 * every ticket and the audio counter restarts with it. So the name for one
 * utterance has to come from here, and it has to be the same name on every
 * attempt to send that utterance — which is why it is minted once, held across
 * the chunks, and only rotated when a closing frame has actually gone out.
 *
 * `randomId` is injected so a test can pin what is generated; production passes
 * a CSPRNG-backed token.
 */
export function createTalkFrames({ sessionId, sessionGeneration, mediaType, sampleRateHz, channels, randomId }) {
  const media = normalizeTalkMediaType(mediaType);
  if (!media) {
    throw new Error(`This browser records in ${String(mediaType || "an unknown container")}, which Talk cannot transcribe`);
  }
  const rate = clampTalkSampleRateHz(sampleRateHz);
  const channelCount = clampTalkChannels(channels);
  let frameSequence = 0;
  let audioSequence = 0;
  let greeted = false;
  // The device's own name for the utterance being sent. Minted lazily on the
  // first chunk of one and cleared after its closing frame, so every chunk of
  // one utterance carries the same name and the next utterance gets its own.
  const mintId = typeof randomId === "function" ? randomId : defaultUtteranceId;
  let utteranceId = null;

  const envelope = (kind) => {
    frameSequence += 1;
    return {
      protocol_version: TALK_PROTOCOL_VERSION,
      session_id: sessionId,
      session_generation: sessionGeneration,
      frame_sequence: frameSequence,
      ...kind,
    };
  };

  const requireGreeted = (what) => {
    if (!greeted) {
      throw new Error(`A Talk ${what} frame cannot be sent before the hello`);
    }
  };

  return {
    mediaType: media,
    sampleRateHz: rate,
    channels: channelCount,
    get frameSequence() {
      return frameSequence;
    },
    get audioSequence() {
      return audioSequence;
    },
    get greeted() {
      return greeted;
    },
    hello() {
      if (greeted) throw new Error("A Talk session sends exactly one hello");
      greeted = true;
      return envelope({
        type: "hello",
        media_type: media,
        sample_rate_hz: rate,
        channels: channelCount,
      });
    },
    /**
     * One chunk of the current utterance.
     *
     * `utteranceId` overrides the generated name, which is what a caller that
     * re-sends a recording the runner never answered must pass: the same
     * recording has to arrive under the same name or it becomes a second turn.
     */
    audio({ audioBase64, last = false, utteranceId: override }) {
      requireGreeted("audio");
      const payload = String(audioBase64 || "");
      if (payload.length === 0) throw new Error("A Talk audio frame carries no audio");
      if (payload.length > TALK_AUDIO_CHUNK_BASE64_CHARS) {
        throw new Error("That audio is larger than one Talk frame may carry — split it first");
      }
      audioSequence += 1;
      if (override) utteranceId = normalizeUtteranceId(override);
      if (!utteranceId) utteranceId = normalizeUtteranceId(mintId());
      const closing = Boolean(last);
      const frame = envelope({
        type: "audio",
        audio_sequence: audioSequence,
        media_type: media,
        audio_base64: payload,
        last: closing,
        // Sent on every chunk rather than only the last: the runner reads it
        // from the closing frame, and a device that loses track of which chunk
        // is last still labels them all consistently.
        utterance_id: utteranceId,
      });
      // Rotated only after the closing frame is built, so the whole utterance
      // shares one name and the next one cannot inherit it.
      if (closing) utteranceId = null;
      return frame;
    },
    interrupt(reason) {
      requireGreeted("interrupt");
      // `reason` is optional on the wire and the runner denies unknown fields,
      // so an absent reason is an absent key rather than a null.
      return envelope(
        reason === undefined || reason === null
          ? { type: "interrupt" }
          : { type: "interrupt", reason: String(reason) },
      );
    },
    /**
     * Durations, and nothing else. No transcript, no audio, no text of any kind
     * can reach this frame — the shape is an utterance number and three optional
     * integers, and every other key is dropped before it is built.
     *
     * `audioSequence` names the utterance these spans measured, and the frame
     * must go out *before* that utterance's audio: the runner answers the
     * instant an utterance closes, so metrics sent afterwards arrive too late to
     * be filed against it and are dropped rather than credited to the next one.
     */
    metrics({ audioSequence, speechDetectionMs, captureMs, uploadMs } = {}) {
      requireGreeted("metrics");
      if (!Number.isInteger(audioSequence) || audioSequence < 1) {
        throw new Error("Talk metrics must name the utterance they measure");
      }
      const frame = { type: "metrics", audio_sequence: audioSequence };
      const spans = {
        speech_detection_ms: boundedSpan(speechDetectionMs),
        capture_ms: boundedSpan(captureMs),
        upload_ms: boundedSpan(uploadMs),
      };
      for (const [name, span] of Object.entries(spans)) {
        if (span !== undefined) frame[name] = span;
      }
      return envelope(frame);
    },
  };
}

/**
 * The local detector, adaptive to the room it is in.
 *
 * It is the phone that decides where an utterance begins and ends: the runner
 * hears only what is uploaded and can never guess from silence it was not sent.
 * `observe` is called once per audio frame and reports what that frame changed,
 * plus the threshold it used — the caller draws the meter with it, because
 * drawing is the one thing this module does not do.
 */
export function createTalkDetector(config = {}) {
  const minSpeechMs = config.minSpeechMs ?? TALK_MIN_SPEECH_MS;
  const silenceMs = config.silenceMs ?? TALK_SILENCE_MS;
  const maxUtteranceMs = config.maxUtteranceMs ?? TALK_MAX_UTTERANCE_MS;
  let noiseFloor = config.noiseFloor ?? TALK_NOISE_FLOOR_START;
  let candidateStartedAt = null;
  let speechStartedAt = null;
  let lastSpeechAt = null;

  const reset = () => {
    candidateStartedAt = null;
    speechStartedAt = null;
    lastSpeechAt = null;
  };

  return {
    reset,
    get noiseFloor() {
      return noiseFloor;
    },
    get speaking() {
      return speechStartedAt !== null;
    },
    observe(rms, nowMs) {
      const threshold = Math.max(TALK_FLOOR_THRESHOLD, noiseFloor * TALK_THRESHOLD_FACTOR);
      const above = rms >= threshold;
      if (speechStartedAt === null) {
        if (above) {
          if (candidateStartedAt === null) candidateStartedAt = nowMs;
          if (nowMs - candidateStartedAt >= minSpeechMs) {
            speechStartedAt = candidateStartedAt;
            lastSpeechAt = nowMs;
            // How long the room took to convince this detector. The one span
            // the runner cannot measure for itself, because it happens before
            // a single byte is uploaded.
            return { event: "speech-start", threshold, speechDetectionMs: nowMs - candidateStartedAt };
          }
        } else {
          candidateStartedAt = null;
          // The floor only learns from quiet, and never while someone is
          // speaking — otherwise a long sentence raises the bar until it stops
          // hearing itself.
          const bounded = Math.min(Math.max(rms, TALK_NOISE_FLOOR_MIN), TALK_NOISE_FLOOR_MAX);
          noiseFloor = noiseFloor * 0.96 + bounded * 0.04;
        }
        return { event: "none", threshold };
      }
      if (above) lastSpeechAt = nowMs;
      if (nowMs - speechStartedAt >= maxUtteranceMs) {
        reset();
        return { event: "max-utterance", threshold };
      }
      if (lastSpeechAt !== null && nowMs - lastSpeechAt >= silenceMs) {
        reset();
        return { event: "utterance-end", threshold };
      }
      return { event: "none", threshold };
    },
  };
}

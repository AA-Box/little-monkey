import { describe, expect, it } from "vitest";

// The module under test is shipped to a browser as plain JavaScript with no
// build step, so it has no declaration file and TypeScript types it as `any`.
// It is imported by path anyway, and deliberately: these are the exact bytes
// `web.rs` serves at `/v1/remote/ui/talkProtocol.js`. A hand-written `.d.ts`
// would be a second copy of the API free to drift from the first, which is the
// defect class this whole file exists to catch.
// @ts-expect-error — untyped ES module served verbatim to the mobile client.
import { MAX_TALK_AUDIO_BASE64_CHARS, TALK_PROTOCOL_VERSION, chooseTalkMediaType, clampTalkChannels, clampTalkSampleRateHz, createTalkDetector, createTalkFrames, normalizeTalkMediaType } from "../../src-tauri/src/bin/monkey-cli/daemon/remote/ui/talkProtocol.js";

/**
 * The mobile Talk client's wire behaviour, driven rather than read.
 *
 * This file exists because the defect it now catches shipped: `app.js` opened
 * the Talk socket and sent `audio` as its first frame. The runner refuses any
 * opening frame that is not a `hello` — `talk.rs`, "The first Talk frame must
 * be a hello" — with `retryable: false`, and the client's own error handler
 * then tore the session down. Foreground mobile Talk could not work at all.
 *
 * A source-scanning test in `web.rs` looked at the same file and saw nothing,
 * because nothing in it was individually wrong: the defect was the *order* the
 * frames went out in, and an ordering is only visible to something that runs
 * the code. So this imports the module the runner actually serves — the same
 * bytes `web.rs` returns for `/v1/remote/ui/talkProtocol.js` — and drives a
 * whole session through it.
 *
 * Every expectation on a frame is `toEqual` on the complete object, never a
 * property check: `TalkClientFrame` is `deny_unknown_fields`, so one extra key
 * is a hard parse failure on the runner and has to fail here too.
 */

const SESSION_ID = "mobile-device-Ab_3-xY";
// 24 URL-safe base64 characters, the shape `TalkTicketResponse::issue` mints.
const SESSION_GENERATION = "aBcDeFgHiJkLmNoPqRsTuVwX";

function newFrames(overrides: Record<string, unknown> = {}) {
  return createTalkFrames({
    sessionId: SESSION_ID,
    sessionGeneration: SESSION_GENERATION,
    mediaType: "audio/webm;codecs=opus",
    sampleRateHz: 48_000,
    channels: 1,
    ...overrides,
  });
}

function envelope(frameSequence: number) {
  return {
    protocol_version: TALK_PROTOCOL_VERSION,
    session_id: SESSION_ID,
    session_generation: SESSION_GENERATION,
    frame_sequence: frameSequence,
  };
}

/** Runs the detector over a constant level and returns the events it produced. */
function feed(
  detector: ReturnType<typeof createTalkDetector>,
  { rms, fromMs, toMs, stepMs = 20 }: { rms: number; fromMs: number; toMs: number; stepMs?: number },
) {
  const events: string[] = [];
  for (let nowMs = fromMs; nowMs <= toMs; nowMs += stepMs) {
    events.push(detector.observe(rms, nowMs).event);
  }
  return events;
}

describe("the Talk frame builder", () => {
  it("opens with a hello and numbers every later frame after it", () => {
    const frames = newFrames();

    // Frame 1 is the hello. `frame_sequence` starting anywhere else — 0 above
    // all, which the runner rejects outright — breaks the session.
    expect(frames.hello()).toEqual({
      ...envelope(1),
      type: "hello",
      media_type: "audio/webm;codecs=opus",
      sample_rate_hz: 48_000,
      channels: 1,
    });

    expect(frames.audio({ audioBase64: "AAECAwQ=", last: true })).toEqual({
      ...envelope(2),
      type: "audio",
      audio_sequence: 1,
      media_type: "audio/webm;codecs=opus",
      audio_base64: "AAECAwQ=",
      last: true,
    });

    // The three spans the runner cannot measure, immediately after the frame
    // that closed the utterance. Durations only — there is nowhere on this
    // frame for a transcript to hide.
    expect(frames.metrics({ speechDetectionMs: 184, captureMs: 1_240, uploadMs: 96 })).toEqual({
      ...envelope(3),
      type: "metrics",
      speech_detection_ms: 184,
      capture_ms: 1_240,
      upload_ms: 96,
    });

    // Barge-in during the answer.
    expect(frames.interrupt("barge_in")).toEqual({
      ...envelope(4),
      type: "interrupt",
      reason: "barge_in",
    });

    // …and the utterance that interrupted it. The audio sequence counts audio
    // frames only, so it is 2 while the frame sequence is 5.
    expect(frames.audio({ audioBase64: "BQYHCA==", last: true })).toEqual({
      ...envelope(5),
      type: "audio",
      audio_sequence: 2,
      media_type: "audio/webm;codecs=opus",
      audio_base64: "BQYHCA==",
      last: true,
    });
  });

  it("refuses to build any frame before the hello", () => {
    // The P0 itself: an `audio` frame as the opening move is what the runner
    // answers with a non-retryable `invalid_frame`.
    expect(() => newFrames().audio({ audioBase64: "AAEC" })).toThrow(/before the hello/u);
    expect(() => newFrames().interrupt("barge_in")).toThrow(/before the hello/u);
    expect(() => newFrames().metrics({ captureMs: 10 })).toThrow(/before the hello/u);

    const frames = newFrames();
    frames.hello();
    expect(() => frames.audio({ audioBase64: "AAEC" })).not.toThrow();
    // And exactly one hello: a second would restart nothing on the runner and
    // would put two greetings in one generation.
    expect(() => frames.hello()).toThrow(/exactly one hello/u);
  });

  it("carries one media type, chosen once, in the hello and in every audio frame", () => {
    // Safari records AAC in MP4 and spells it with a codec parameter the
    // runner's allow-list does not contain. Whatever normalization does, the
    // hello and the audio have to agree — a hello that promises WebM while the
    // blobs are MP4 is a transcription failure on every utterance.
    const frames = newFrames({ mediaType: "audio/mp4;codecs=mp4a.40.2" });
    const hello = frames.hello() as { media_type: string };
    const audio = frames.audio({ audioBase64: "AAEC", last: true }) as { media_type: string };
    expect(hello.media_type).toBe("audio/mp4");
    expect(audio.media_type).toBe(hello.media_type);
  });

  it("keeps the sequences strictly increasing across a long session", () => {
    const frames = newFrames();
    const sent: number[] = [(frames.hello() as { frame_sequence: number }).frame_sequence];
    for (let utterance = 0; utterance < 25; utterance += 1) {
      sent.push((frames.audio({ audioBase64: "AAEC", last: true }) as { frame_sequence: number }).frame_sequence);
      sent.push((frames.metrics({ captureMs: 500 }) as { frame_sequence: number }).frame_sequence);
    }
    expect(sent[0]).toBe(1);
    expect(sent.every((value, index) => index === 0 || value > sent[index - 1])).toBe(true);
    expect(frames.audioSequence).toBe(25);
  });

  it("omits telemetry it does not have and bounds what it does", () => {
    const frames = newFrames();
    frames.hello();
    // Optional means absent, not null: `deny_unknown_fields` is fine with a
    // missing key and the runner's `Option` handles it, but a null would have
    // to be spelled by the sender and never is.
    expect(frames.metrics({ captureMs: 1_000 })).toEqual({
      ...envelope(2),
      type: "metrics",
      capture_ms: 1_000,
    });
    expect(frames.metrics({})).toEqual({ ...envelope(3), type: "metrics" });
    // The runner caps a span at 600 000 ms; a clock jump must not turn into a
    // refused frame in the middle of a working conversation.
    expect(frames.metrics({ speechDetectionMs: -5, captureMs: 9_999_999, uploadMs: 12.6 })).toEqual({
      ...envelope(4),
      type: "metrics",
      speech_detection_ms: 0,
      capture_ms: 600_000,
      upload_ms: 13,
    });
  });

  it("omits an interrupt reason rather than sending a null", () => {
    const frames = newFrames();
    frames.hello();
    expect(frames.interrupt()).toEqual({ ...envelope(2), type: "interrupt" });
  });

  it("refuses an utterance larger than one audio frame may carry", () => {
    const frames = newFrames();
    frames.hello();
    // 512 KiB decoded. The runner refuses this without retry, which ends the
    // conversation — so the client has to notice first.
    const oversize = "A".repeat(MAX_TALK_AUDIO_BASE64_CHARS + 1);
    expect(() => frames.audio({ audioBase64: oversize, last: true })).toThrow(/larger than/u);
    expect(() => frames.audio({ audioBase64: "", last: true })).toThrow(/no audio/u);
  });

  it("keeps the sample rate and channel count inside what the runner accepts", () => {
    // `sample_rate_hz` is 8 000–192 000 and `channels` is 1–2 on the runner.
    // A browser that reports nothing gets an honest default rather than a
    // number invented to look plausible.
    expect(clampTalkSampleRateHz(44_100)).toBe(44_100);
    expect(clampTalkSampleRateHz(0)).toBe(8_000);
    expect(clampTalkSampleRateHz(384_000)).toBe(192_000);
    expect(clampTalkSampleRateHz(undefined)).toBe(48_000);
    expect(clampTalkChannels(2)).toBe(2);
    expect(clampTalkChannels(0)).toBe(1);
    expect(clampTalkChannels(6)).toBe(2);
    expect(clampTalkChannels(undefined)).toBe(1);

    const frames = newFrames({ sampleRateHz: 1_000_000, channels: 0 });
    expect(frames.hello()).toEqual({
      ...envelope(1),
      type: "hello",
      media_type: "audio/webm;codecs=opus",
      sample_rate_hz: 192_000,
      channels: 1,
    });
  });
});

describe("choosing a container", () => {
  it("prefers Opus, falls back to what the browser has, and admits when it has nothing", () => {
    const chrome = chooseTalkMediaType((type: string) => type.startsWith("audio/webm"));
    expect(chrome).toBe("audio/webm;codecs=opus");

    // Safari: no WebM at all, MP4 only.
    const safari = chooseTalkMediaType((type: string) => type === "audio/mp4");
    expect(safari).toBe("audio/mp4");

    // Nothing preferred is supported. An empty answer is what tells the client
    // to ask a real recorder instead of naming a container it cannot produce.
    expect(chooseTalkMediaType(() => false)).toBe("");
    expect(
      chooseTalkMediaType(() => {
        throw new Error("this browser throws on unknown types");
      }),
    ).toBe("");
  });

  it("normalizes whatever a recorder reports onto the runner's allow-list", () => {
    expect(normalizeTalkMediaType("audio/webm;codecs=opus")).toBe("audio/webm;codecs=opus");
    expect(normalizeTalkMediaType("audio/webm; codecs=opus")).toBe("audio/webm;codecs=opus");
    expect(normalizeTalkMediaType("audio/ogg;codecs=vorbis")).toBe("audio/ogg");
    expect(normalizeTalkMediaType("audio/mp4;codecs=mp4a.40.2")).toBe("audio/mp4");
    expect(normalizeTalkMediaType("audio/flac")).toBe("audio/webm");
    expect(normalizeTalkMediaType("")).toBe("audio/webm");
  });
});

describe("the local voice activity detector", () => {
  it("ignores a noise shorter than the confirmation window", () => {
    const detector = createTalkDetector();
    // A door, a cough, a tap on the phone: loud, and over in 160 ms.
    const events = feed(detector, { rms: 0.6, fromMs: 0, toMs: 160 });
    expect(events.every((event) => event === "none")).toBe(true);
    expect(detector.speaking).toBe(false);
    // …and it leaves no half-finished candidate behind: quiet clears it, so the
    // next burst has to earn its own 180 ms rather than completing this one.
    expect(feed(detector, { rms: 0.0001, fromMs: 180, toMs: 400 }).includes("speech-start")).toBe(
      false,
    );
    expect(feed(detector, { rms: 0.6, fromMs: 420, toMs: 560 }).includes("speech-start")).toBe(
      false,
    );
  });

  it("confirms speech at 180 ms and reports how long that took", () => {
    const detector = createTalkDetector();
    const before = feed(detector, { rms: 0.6, fromMs: 0, toMs: 160 });
    expect(before.includes("speech-start")).toBe(false);
    const confirmed = detector.observe(0.6, 180);
    expect(confirmed.event).toBe("speech-start");
    // The one span that happens entirely before any audio is uploaded.
    expect(confirmed.speechDetectionMs).toBe(180);
    expect(detector.speaking).toBe(true);
  });

  it("ends an utterance after 800 ms of quiet and not before", () => {
    const detector = createTalkDetector();
    feed(detector, { rms: 0.6, fromMs: 0, toMs: 180 });
    expect(detector.speaking).toBe(true);

    // A 600 ms pause mid-sentence is a pause, not the end of a turn.
    const pause = feed(detector, { rms: 0.0001, fromMs: 200, toMs: 780 });
    expect(pause.includes("utterance-end")).toBe(false);
    expect(detector.speaking).toBe(true);

    // 900 ms of it is the end of the turn.
    const silence = feed(detector, { rms: 0.0001, fromMs: 800, toMs: 1_080 });
    expect(silence.filter((event) => event === "utterance-end")).toHaveLength(1);
    expect(detector.speaking).toBe(false);
  });

  it("stops an utterance that never ends", () => {
    const detector = createTalkDetector();
    feed(detector, { rms: 0.6, fromMs: 0, toMs: 180 });
    // A microphone in a pocket against a fan: loud forever. The cap is what
    // keeps one upload inside the runner's 512 KiB frame.
    const capped = feed(detector, { rms: 0.6, fromMs: 200, toMs: 90_100, stepMs: 100 });
    expect(capped.filter((event) => event === "max-utterance")).toHaveLength(1);
    expect(detector.speaking).toBe(false);
  });

  it("adapts to a noisy room instead of hearing it as speech", () => {
    const detector = createTalkDetector();
    // A café: a steady 0.02, well above the 0.012 floor the threshold starts
    // from but never speech. The detector learns it.
    const ambient = feed(detector, { rms: 0.02, fromMs: 0, toMs: 4_000 });
    expect(ambient.includes("speech-start")).toBe(false);
    expect(detector.noiseFloor).toBeGreaterThan(0.015);

    // A moderate level that would have been speech in a silent room is not
    // speech in this one — and the floor does not drift up while it is heard.
    const moderate = feed(detector, { rms: 0.03, fromMs: 4_020, toMs: 6_000 });
    expect(moderate.includes("speech-start")).toBe(false);

    // Someone actually talking still is.
    const speech = feed(detector, { rms: 0.4, fromMs: 6_020, toMs: 6_400 });
    expect(speech.includes("speech-start")).toBe(true);
  });
});

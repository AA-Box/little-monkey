import { describe, expect, it } from "vitest";

import { formatSpeakerTranscript, type TranscriptResult } from "./companionClient";

function result(overrides: Partial<TranscriptResult> = {}): TranscriptResult {
  return {
    jobId: "meeting-1",
    text: "fallback",
    segments: [],
    transcript: {
      blob: { id: "a".repeat(64), size: 8 },
      mediaType: "text/plain",
      source: "meeting",
      createdAtMs: 1,
    },
    rawAudio: null,
    backend: "local_whisper",
    ...overrides,
  };
}

describe("meeting transcript formatting", () => {
  it("uses the plain transcript when a backend returns no speaker segments", () => {
    expect(formatSpeakerTranscript(result({ text: "plain words" }))).toBe("plain words");
  });

  it("preserves speaker labels and stable minute-second timestamps", () => {
    expect(formatSpeakerTranscript(result({
      segments: [
        { speaker: "Alice", startMs: 65_000, endMs: 67_000, text: "Ship it", confidence: 0.92 },
        { speaker: "Bob", startMs: null, endMs: null, text: "Agreed", confidence: null },
      ],
    }))).toBe("Alice [01:05]: Ship it\nBob: Agreed");
  });
});

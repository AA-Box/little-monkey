import { describe, expect, it } from "vitest";

import type { VoiceConfig } from "../../lib/companionClient";
import { shouldAutoOpenTalk } from "./ChatWindow";

/**
 * The regression this pins: Talk moved into the composer and the composer
 * gates the hook behind a press, so Always Listening quietly meant "listens
 * once you press Talk". The standalone page had no such gate — it mounted the
 * hook the moment it opened, which is what made the setting's claim true.
 */
const VOICE = {
  backend: "local_whisper",
  wakePhraseEnabled: true,
  wakePhrase: "hey little monkey",
  alwaysListening: true,
  inputDeviceId: null,
  outputDeviceId: null,
} as unknown as VoiceConfig;

describe("Always Listening opens Talk without a press", () => {
  it("opens when the setting and its phrase are both on", () => {
    expect(shouldAutoOpenTalk(VOICE)).toBe(true);
  });

  it("stays shut when the setting is off", () => {
    expect(shouldAutoOpenTalk({ ...VOICE, alwaysListening: false })).toBe(false);
  });

  /**
   * A continuous session with no wake gating is a hot microphone submitting
   * every sentence in the room. The Rust side refuses the combination, and
   * this refuses it again rather than trusting that.
   */
  it("stays shut with no wake phrase to gate on", () => {
    expect(shouldAutoOpenTalk({ ...VOICE, wakePhraseEnabled: false })).toBe(false);
  });

  /** Realtime opens its own microphone, after its own privacy gate. */
  it("never opens the realtime engine from a setting", () => {
    expect(shouldAutoOpenTalk({ ...VOICE, engineKind: "realtime" })).toBe(false);
  });
});

import { describe, expect, it } from "vitest";

import { errorMessage } from "./errors";

describe("errorMessage", () => {
  // The 301 call sites this replaced all relied on exactly these two rules;
  // anything else would silently change what users read in an error banner.
  it("returns an Error's message verbatim", () => {
    expect(errorMessage(new Error("workspace root is no longer attached"))).toBe(
      "workspace root is no longer attached",
    );
  });

  it("returns a thrown string unchanged", () => {
    expect(errorMessage("Permission denied")).toBe("Permission denied");
  });

  // Tauri rejects with plain objects, which `String(value)` renders as the
  // useless "[object Object]" the old inline pattern produced.
  it("recovers the message from an object-shaped rejection instead of [object Object]", () => {
    expect(errorMessage({ message: "command not found" })).toBe("command not found");
    expect(errorMessage({ error: "keychain unavailable" })).toBe("keychain unavailable");
    expect(errorMessage({ reason: "kill switch engaged" })).toBe("kill switch engaged");
  });

  it("serializes an object with no message-like field rather than losing it", () => {
    expect(errorMessage({ code: 429, retryAfterMs: 1_000 })).toBe(
      '{"code":429,"retryAfterMs":1000}',
    );
  });

  it("falls back to String() for primitives, null, and undefined", () => {
    expect(errorMessage(null)).toBe("null");
    expect(errorMessage(undefined)).toBe("undefined");
    expect(errorMessage(42)).toBe("42");
  });

  it("never throws on a circular object", () => {
    const circular: Record<string, unknown> = {};
    circular.self = circular;
    expect(() => errorMessage(circular)).not.toThrow();
  });

  it("prefers a subclass's message over its serialized form", () => {
    class TransportError extends Error {}
    expect(errorMessage(new TransportError("stream closed"))).toBe("stream closed");
  });
});

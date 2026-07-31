import { beforeEach, describe, expect, it, vi } from "vitest";

import { clearPersistedState, hydrateState, persistState } from "./persistedState";

const KEY = "little-monkey-test-envelope";

/** Matches `rateLimitTracker.test.ts`: the suite runs in the `node`
 * environment, so `localStorage` is stubbed rather than provided by jsdom. */
function stubStorage(overrides: Partial<Storage> = {}): Map<string, string> {
  const values = new Map<string, string>();
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => values.get(key) ?? null,
    setItem: (key: string, value: string) => values.set(key, value),
    removeItem: (key: string) => values.delete(key),
    clear: () => values.clear(),
    ...overrides,
  });
  return values;
}

describe("persistedState", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
    stubStorage();
  });

  // The on-disk shape is load-bearing: nine stores already have user data
  // written as a flat `{ version, ...payload }`, so a nested envelope would
  // have silently discarded all of it on first launch after the refactor.
  it("writes the flat versioned shape the existing stores already use", () => {
    persistState(KEY, 3, { runs: [{ id: "run-1" }] });
    expect(JSON.parse(localStorage.getItem(KEY) ?? "null")).toEqual({
      version: 3,
      runs: [{ id: "run-1" }],
    });
  });

  it("round-trips a payload at the matching version", () => {
    persistState(KEY, 2, { cases: ["a", "b"] });
    expect(hydrateState(KEY, 2)).toEqual({ version: 2, cases: ["a", "b"] });
  });

  it("discards a payload written by a different schema version", () => {
    persistState(KEY, 1, { cases: ["stale"] });
    expect(hydrateState(KEY, 2)).toBeNull();
  });

  it("returns null for absent, unparseable, and non-object entries", () => {
    expect(hydrateState(KEY, 1)).toBeNull();
    localStorage.setItem(KEY, "{not json");
    expect(hydrateState(KEY, 1)).toBeNull();
    localStorage.setItem(KEY, JSON.stringify([1, 2, 3]));
    expect(hydrateState(KEY, 1)).toBeNull();
    localStorage.setItem(KEY, JSON.stringify("a string"));
    expect(hydrateState(KEY, 1)).toBeNull();
  });

  // A full or disabled localStorage must never fail the user's actual action
  // — persistence here is a convenience, not a source of truth.
  it("swallows a write failure instead of throwing into the caller", () => {
    stubStorage({
      setItem: () => {
        throw new Error("QuotaExceededError");
      },
    });
    expect(() => persistState(KEY, 1, { anything: true })).not.toThrow();
  });

  it("swallows a read failure and reports 'nothing stored'", () => {
    stubStorage({
      getItem: () => {
        throw new Error("SecurityError");
      },
    });
    expect(hydrateState(KEY, 1)).toBeNull();
  });

  it("clears an entry without throwing when storage is unavailable", () => {
    persistState(KEY, 1, { value: 1 });
    clearPersistedState(KEY);
    expect(localStorage.getItem(KEY)).toBeNull();
    stubStorage({
      removeItem: () => {
        throw new Error("SecurityError");
      },
    });
    expect(() => clearPersistedState(KEY)).not.toThrow();
  });
});

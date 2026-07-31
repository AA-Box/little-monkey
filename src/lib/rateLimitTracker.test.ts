import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  evaluateRateLimit,
  getCountInWindow,
  getCountLastDay,
  getCountLastMinute,
  recordRequest,
} from "./rateLimitTracker";

describe("rateLimitTracker runtime warnings", () => {
  beforeEach(() => {
    const values = new Map<string, string>();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
      clear: () => values.clear(),
    });
  });

  it("counts failed/successful attempts alike within exact rolling windows", () => {
    const now = 2_000_000;
    recordRequest("provider", now - 60_001);
    recordRequest("provider", now - 60_000);
    recordRequest("provider", now - 1);

    expect(getCountLastMinute("provider", now)).toBe(2);
    expect(getCountInWindow("provider", 500, now)).toBe(1);
    expect(getCountLastDay("provider", now)).toBe(3);
  });

  it("warns on the imminent request at eighty percent and then over cap", () => {
    const now = 5_000_000;
    for (let index = 0; index < 7; index += 1) {
      recordRequest("provider", now - index);
    }

    expect(evaluateRateLimit("provider", { rpm: 10 }, now)).toEqual([
      expect.objectContaining({
        window: "minute",
        severity: "approaching",
        currentCount: 7,
        nextCount: 8,
        limit: 10,
        percent: 0.8,
      }),
    ]);

    for (let index = 7; index < 10; index += 1) {
      recordRequest("provider", now - index);
    }
    expect(evaluateRateLimit("provider", { rpm: 10 }, now)).toEqual([
      expect.objectContaining({
        severity: "exceeded",
        currentCount: 10,
        nextCount: 11,
      }),
    ]);
  });

  it("evaluates minute and day caps independently", () => {
    const now = 9_000_000;
    for (let index = 0; index < 4; index += 1) {
      recordRequest("provider", now - 120_000 - index);
    }
    expect(evaluateRateLimit("provider", { rpm: 100, rpd: 5 }, now)).toEqual([
      expect.objectContaining({
        window: "day",
        severity: "approaching",
        nextCount: 5,
      }),
    ]);
  });

  it("does nothing without a valid configured cap", () => {
    recordRequest("provider", 1000);
    expect(evaluateRateLimit("provider", undefined, 1000)).toEqual([]);
    expect(evaluateRateLimit("provider", {}, 1000)).toEqual([]);
    expect(evaluateRateLimit("provider", { rpm: 0, rpd: Number.NaN }, 1000)).toEqual([]);
  });

  it("prunes attempts older than one day", () => {
    const now = 100_000_000;
    recordRequest("provider", now - 86_400_001);
    recordRequest("provider", now);
    expect(getCountLastDay("provider", now)).toBe(1);
  });
});

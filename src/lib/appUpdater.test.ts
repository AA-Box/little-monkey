import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import {
  FOCUS_MIN_INTERVAL_MS,
  POLL_INTERVAL_MS,
  STARTUP_CHECK_DELAY_MS,
  downloadProgress,
  installsWhileRunning,
  shouldCheckNow,
  startUpdateWatcher,
} from "./appUpdater";

vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
}));

describe("shouldCheckNow", () => {
  const now = 1_000_000_000;

  it("never checks while a check, download, or pending relaunch is in flight", () => {
    for (const reason of ["startup", "interval", "focus", "manual"] as const) {
      expect(shouldCheckNow(reason, { now, lastCheckedAt: null, busy: true })).toBe(false);
    }
  });

  it("always checks on launch and on an explicit manual request", () => {
    expect(shouldCheckNow("startup", { now, lastCheckedAt: now - 1, busy: false })).toBe(true);
    expect(shouldCheckNow("manual", { now, lastCheckedAt: now - 1, busy: false })).toBe(true);
  });

  it("throttles refocus checks far tighter than the background poll", () => {
    const halfHourAgo = now - FOCUS_MIN_INTERVAL_MS / 2;
    expect(shouldCheckNow("focus", { now, lastCheckedAt: halfHourAgo, busy: false })).toBe(false);
    expect(
      shouldCheckNow("focus", { now, lastCheckedAt: now - FOCUS_MIN_INTERVAL_MS, busy: false }),
    ).toBe(true);
    // The same elapsed time is not yet enough for the interval poll.
    expect(
      shouldCheckNow("interval", { now, lastCheckedAt: now - FOCUS_MIN_INTERVAL_MS, busy: false }),
    ).toBe(false);
    expect(
      shouldCheckNow("interval", { now, lastCheckedAt: now - POLL_INTERVAL_MS, busy: false }),
    ).toBe(true);
  });

  it("checks when nothing has been checked yet in this window", () => {
    expect(shouldCheckNow("focus", { now, lastCheckedAt: null, busy: false })).toBe(true);
  });
});

describe("installsWhileRunning", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("is false on Windows (installer closes the app) and true elsewhere", () => {
    vi.stubGlobal("navigator", { userAgent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64)" });
    expect(installsWhileRunning()).toBe(false);

    vi.stubGlobal("navigator", { userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)" });
    expect(installsWhileRunning()).toBe(true);

    vi.stubGlobal("navigator", { userAgent: "Mozilla/5.0 (X11; Linux x86_64)" });
    expect(installsWhileRunning()).toBe(true);
  });
});

describe("downloadProgress", () => {
  it("is null without a content length, and clamped otherwise", () => {
    expect(downloadProgress(500, null)).toBeNull();
    expect(downloadProgress(500, 0)).toBeNull();
    expect(downloadProgress(500, 1000)).toBe(0.5);
    expect(downloadProgress(1500, 1000)).toBe(1);
  });
});

/** The suite runs in the `node` environment (see vitest.config.ts), so there
 * is no real `window` — this stands in for one, delegating timers to the
 * globals vitest's fake timers patch. */
function stubWindow() {
  const focusListeners = new Set<() => void>();
  vi.stubGlobal("window", {
    setTimeout: (fn: () => void, ms: number) => globalThis.setTimeout(fn, ms),
    clearTimeout: (id: ReturnType<typeof globalThis.setTimeout>) => globalThis.clearTimeout(id),
    setInterval: (fn: () => void, ms: number) => globalThis.setInterval(fn, ms),
    clearInterval: (id: ReturnType<typeof globalThis.setInterval>) => globalThis.clearInterval(id),
    addEventListener: (type: string, fn: () => void) => {
      if (type === "focus") focusListeners.add(fn);
    },
    removeEventListener: (type: string, fn: () => void) => {
      if (type === "focus") focusListeners.delete(fn);
    },
  });
  return { focus: () => focusListeners.forEach((fn) => fn()) };
}

describe("startUpdateWatcher", () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("fires the delayed launch check, then polls, then reacts to refocus", () => {
    const stubbed = stubWindow();
    const check = vi.fn().mockResolvedValue(undefined);
    const stop = startUpdateWatcher(check);

    expect(check).not.toHaveBeenCalled();
    vi.advanceTimersByTime(STARTUP_CHECK_DELAY_MS);
    expect(check).toHaveBeenCalledWith("startup");

    vi.advanceTimersByTime(POLL_INTERVAL_MS);
    expect(check).toHaveBeenCalledWith("interval");

    stubbed.focus();
    expect(check).toHaveBeenCalledWith("focus");

    stop();
    check.mockClear();
    vi.advanceTimersByTime(POLL_INTERVAL_MS * 2);
    stubbed.focus();
    expect(check).not.toHaveBeenCalled();
  });
});

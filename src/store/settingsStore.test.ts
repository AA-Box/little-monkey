import { beforeEach, describe, expect, it, vi } from "vitest";

import { STORAGE_KEY, useSettingsStore } from "./settingsStore";

describe("settingsStore.checkpointRetention", () => {
  beforeEach(() => {
    useSettingsStore.setState({ checkpointRetention: 20 });
  });

  it("defaults to 20 when nothing is persisted", async () => {
    // Exercises the real default-hydration path (`hydrate()`/`defaults()`)
    // instead of asserting against state this suite's own `beforeEach` just
    // set by hand — a fresh module instance with no persisted blob is the
    // only way to actually cover that code path. Without `resetModules` +
    // a dynamic re-import, this test would pass even if
    // `DEFAULT_CHECKPOINT_RETENTION` were changed to something else,
    // because `beforeEach` would still be forcing the value to 20.
    // (This suite runs under vitest's `node` environment, which has no
    // `localStorage` global at all — guarded rather than assumed, since
    // `hydrate()` itself tolerates that via its own try/catch.)
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().checkpointRetention).toBe(20);
  });

  it("clamps below the 5-checkpoint floor", () => {
    useSettingsStore.getState().setCheckpointRetention(0);
    expect(useSettingsStore.getState().checkpointRetention).toBe(5);
  });

  it("clamps above the 100-checkpoint ceiling", () => {
    useSettingsStore.getState().setCheckpointRetention(500);
    expect(useSettingsStore.getState().checkpointRetention).toBe(100);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setCheckpointRetention(42.6);
    expect(useSettingsStore.getState().checkpointRetention).toBe(43);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setCheckpointRetention(50);
    expect(useSettingsStore.getState().checkpointRetention).toBe(50);
  });
});

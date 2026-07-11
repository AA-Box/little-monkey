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

describe("settingsStore.memoryEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ memoryEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as the
    // checkpointRetention default test above — `beforeEach` forces `true`
    // regardless, so only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setMemoryEnabled(false);
    expect(useSettingsStore.getState().memoryEnabled).toBe(false);
    useSettingsStore.getState().setMemoryEnabled(true);
    expect(useSettingsStore.getState().memoryEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    // Guarded like the sibling tests above/below — this suite runs under
    // vitest's `node` environment, which has no `localStorage` global, so
    // `persist()`'s best-effort write silently no-ops there.
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setMemoryEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ memoryEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.webToolsEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ webToolsEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as memoryEnabled's
    // own default test above — `beforeEach` forces `true` regardless, so
    // only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setWebToolsEnabled(false);
    expect(useSettingsStore.getState().webToolsEnabled).toBe(false);
    useSettingsStore.getState().setWebToolsEnabled(true);
    expect(useSettingsStore.getState().webToolsEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setWebToolsEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ webToolsEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.verifyEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ verifyEnabled: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as memoryEnabled's
    // own default test above — `beforeEach` forces `false` regardless, so
    // only a fresh module import actually covers `defaults()`. Unlike
    // memoryEnabled/webToolsEnabled, this one defaults OFF: running
    // arbitrary configured shell automatically should be opt-in.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setVerifyEnabled(true);
    expect(useSettingsStore.getState().verifyEnabled).toBe(true);
    useSettingsStore.getState().setVerifyEnabled(false);
    expect(useSettingsStore.getState().verifyEnabled).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setVerifyEnabled(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ verifyEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.verifyMaxRounds", () => {
  beforeEach(() => {
    useSettingsStore.setState({ verifyMaxRounds: 1 });
  });

  it("defaults to 1 when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as checkpointRetention's
    // own default test above — `beforeEach` forces `1` regardless, so only a
    // fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(1);
  });

  it("clamps below the 0-round floor", () => {
    useSettingsStore.getState().setVerifyMaxRounds(-1);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(0);
  });

  it("clamps above the 3-round ceiling", () => {
    useSettingsStore.getState().setVerifyMaxRounds(10);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(3);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setVerifyMaxRounds(2.4);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(2);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setVerifyMaxRounds(2);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(2);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setVerifyMaxRounds(3);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(3);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores an out-of-range persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ verifyMaxRounds: 99 }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(1);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.artifactScriptsEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ artifactScriptsEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as webToolsEnabled's
    // own default test above — `beforeEach` forces `true` regardless, so
    // only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setArtifactScriptsEnabled(false);
    expect(useSettingsStore.getState().artifactScriptsEnabled).toBe(false);
    useSettingsStore.getState().setArtifactScriptsEnabled(true);
    expect(useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setArtifactScriptsEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ artifactScriptsEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.artifactAutoPreview", () => {
  beforeEach(() => {
    useSettingsStore.setState({ artifactAutoPreview: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as verifyEnabled's
    // own default test above — `beforeEach` forces `false` regardless, so
    // only a fresh module import actually covers `defaults()`. Defaults OFF:
    // auto-opening the workspace panel on the user's behalf should be
    // opt-in, mirroring verifyEnabled's posture.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setArtifactAutoPreview(true);
    expect(useSettingsStore.getState().artifactAutoPreview).toBe(true);
    useSettingsStore.getState().setArtifactAutoPreview(false);
    expect(useSettingsStore.getState().artifactAutoPreview).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setArtifactAutoPreview(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ artifactAutoPreview: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { usePermissionStore } from "./permissionStore";

describe("permissionStore.lastActMode", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(undefined);
    usePermissionStore.setState({ lastActMode: "acceptEdits" });
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem("little-monkey-last-act-mode");
    }
  });

  it("defaults to acceptEdits when nothing is persisted", async () => {
    // Same "exercise the real hydration path via a fresh module instance"
    // rationale as settingsStore.test.ts's checkpointRetention default test
    // — this repo's vitest config runs under `node`, which has no
    // `localStorage` global at all (guarded, not assumed).
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem("little-monkey-last-act-mode");
    }
    vi.resetModules();
    const fresh = await import("./permissionStore");
    expect(fresh.usePermissionStore.getState().lastActMode).toBe("acceptEdits");
  });

  it("setLastActMode accepts manual/acceptEdits/auto", () => {
    for (const mode of ["manual", "acceptEdits", "auto"] as const) {
      usePermissionStore.getState().setLastActMode(mode);
      expect(usePermissionStore.getState().lastActMode).toBe(mode);
    }
  });

  it("setLastActMode is a no-op for plan — lastActMode never becomes 'plan'", () => {
    usePermissionStore.getState().setLastActMode("auto");
    usePermissionStore.getState().setLastActMode("plan");
    expect(usePermissionStore.getState().lastActMode).toBe("auto");
  });

  it("setLastActMode is a no-op for bypass — lastActMode never becomes 'bypass'", () => {
    usePermissionStore.getState().setLastActMode("manual");
    usePermissionStore.getState().setLastActMode("bypass");
    expect(usePermissionStore.getState().lastActMode).toBe("manual");
  });

  it("persists a valid act mode to localStorage, but never persists plan/bypass", () => {
    if (typeof localStorage === "undefined") return;
    usePermissionStore.getState().setLastActMode("auto");
    expect(localStorage.getItem("little-monkey-last-act-mode")).toBe("auto");

    usePermissionStore.getState().setLastActMode("bypass");
    expect(localStorage.getItem("little-monkey-last-act-mode")).toBe("auto");

    usePermissionStore.getState().setLastActMode("plan");
    expect(localStorage.getItem("little-monkey-last-act-mode")).toBe("auto");
  });

  it("restores a previously persisted valid act mode on a fresh module load", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem("little-monkey-last-act-mode", "manual");
    vi.resetModules();
    const fresh = await import("./permissionStore");
    expect(fresh.usePermissionStore.getState().lastActMode).toBe("manual");
  });

  it("never restores a persisted 'bypass' or 'plan' value (falls back to the default)", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem("little-monkey-last-act-mode", "bypass");
    vi.resetModules();
    const fresh = await import("./permissionStore");
    expect(fresh.usePermissionStore.getState().lastActMode).toBe("acceptEdits");
  });
});

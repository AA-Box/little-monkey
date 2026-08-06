import { beforeEach, describe, expect, it, vi } from "vitest";

import { setPendingUpdateForTests, useUpdateStore } from "./updateStore";

vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
}));

const check = vi.fn();
const relaunch = vi.fn();
const installsWhileRunning = vi.fn(() => true);

vi.mock("@tauri-apps/plugin-updater", () => ({
  check: (...args: unknown[]) => check(...args),
}));

vi.mock("@tauri-apps/plugin-process", () => ({
  relaunch: (...args: unknown[]) => relaunch(...args),
}));

vi.mock("../lib/appUpdater", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/appUpdater")>();
  return { ...actual, installsWhileRunning: () => installsWhileRunning() };
});

type ProgressEvent =
  | { event: "Started"; data: { contentLength?: number } }
  | { event: "Progress"; data: { chunkLength: number } }
  | { event: "Finished" };

function fakeUpdate(chunks: ProgressEvent[] = [{ event: "Finished" }]) {
  const emit = async (onEvent?: (event: ProgressEvent) => void) => {
    for (const chunk of chunks) onEvent?.(chunk);
  };
  return {
    version: "1.24012.11",
    body: "Fixes and improvements",
    download: vi.fn(emit),
    downloadAndInstall: vi.fn(emit),
    install: vi.fn(async () => {}),
  };
}

function reset() {
  useUpdateStore.setState({
    status: "idle",
    version: null,
    notes: null,
    downloadedBytes: 0,
    contentLength: null,
    lastCheckedAt: null,
    lastError: null,
  });
  setPendingUpdateForTests(null);
  check.mockReset();
  relaunch.mockReset();
  installsWhileRunning.mockReturnValue(true);
}

describe("updateStore", () => {
  beforeEach(reset);

  it("stays idle when the endpoint reports no update", async () => {
    check.mockResolvedValue(null);
    await useUpdateStore.getState().check("startup");
    const state = useUpdateStore.getState();
    expect(state.status).toBe("idle");
    expect(state.version).toBeNull();
    expect(state.lastCheckedAt).not.toBeNull();
  });

  it("downloads and installs in the background on macOS/Linux, then parks in `ready`", async () => {
    const update = fakeUpdate([
      { event: "Started", data: { contentLength: 900 } },
      { event: "Progress", data: { chunkLength: 400 } },
      { event: "Progress", data: { chunkLength: 500 } },
      { event: "Finished" },
    ]);
    check.mockResolvedValue(update);

    await useUpdateStore.getState().check("startup");

    const state = useUpdateStore.getState();
    expect(state.status).toBe("ready");
    expect(state.version).toBe("1.24012.11");
    expect(state.notes).toBe("Fixes and improvements");
    expect(state.contentLength).toBe(900);
    expect(state.downloadedBytes).toBe(900);
    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
    expect(update.download).not.toHaveBeenCalled();
  });

  it("only downloads on Windows — the installer would close the app mid-turn", async () => {
    installsWhileRunning.mockReturnValue(false);
    const update = fakeUpdate();
    check.mockResolvedValue(update);

    await useUpdateStore.getState().check("startup");

    expect(update.download).toHaveBeenCalledTimes(1);
    expect(update.downloadAndInstall).not.toHaveBeenCalled();
    expect(update.install).not.toHaveBeenCalled();
    expect(useUpdateStore.getState().status).toBe("ready");
  });

  it("runs the Windows installer only on the card click, never `relaunch`", async () => {
    installsWhileRunning.mockReturnValue(false);
    const update = fakeUpdate();
    check.mockResolvedValue(update);
    await useUpdateStore.getState().check("startup");

    await useUpdateStore.getState().applyUpdate();

    expect(update.install).toHaveBeenCalledTimes(1);
    expect(relaunch).not.toHaveBeenCalled();
  });

  it("swallows a failed check — no user-visible state beyond `lastError`", async () => {
    check.mockRejectedValue(new Error("signature verification failed"));
    await useUpdateStore.getState().check("startup");
    const state = useUpdateStore.getState();
    expect(state.status).toBe("idle");
    expect(state.lastError).toContain("signature");
    expect(state.lastCheckedAt).not.toBeNull();
  });

  it("does not re-check while a staged update is waiting for the card click", async () => {
    check.mockResolvedValue(fakeUpdate());
    await useUpdateStore.getState().check("startup");
    expect(useUpdateStore.getState().status).toBe("ready");

    check.mockClear();
    await useUpdateStore.getState().check("interval");
    await useUpdateStore.getState().check("manual");
    expect(check).not.toHaveBeenCalled();
  });

  it("applies only from `ready`, and falls back to `ready` if the relaunch fails", async () => {
    relaunch.mockResolvedValue(undefined);
    await useUpdateStore.getState().applyUpdate();
    expect(relaunch).not.toHaveBeenCalled();

    useUpdateStore.setState({ status: "ready", version: "1.24012.11" });
    relaunch.mockRejectedValue(new Error("restart blocked"));
    await useUpdateStore.getState().applyUpdate();
    expect(relaunch).toHaveBeenCalledTimes(1);
    expect(useUpdateStore.getState().status).toBe("ready");
    expect(useUpdateStore.getState().lastError).toContain("restart blocked");
  });
});

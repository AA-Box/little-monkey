import { beforeEach, describe, expect, it, vi } from "vitest";

import { setPendingUpdateForTests, useUpdateStore } from "./updateStore";

const invoke = vi.fn();

vi.mock("@tauri-apps/api/core", () => ({
  isTauri: () => true,
  invoke: (...args: unknown[]) => invoke(...args),
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

const snapshot = {
  version: "1.2.0",
  kind: "macBundle" as const,
  installRoot: "/Applications/Little Monkey.app",
  payload: "/data/updates/rollback/payload",
  relaunch: "/Applications/Little Monkey.app/Contents/MacOS/little-monkey",
  createdAtMs: 1_700_000_000_000,
  sizeBytes: 512,
};

function reset() {
  useUpdateStore.setState({
    status: "idle",
    version: null,
    notes: null,
    downloadedBytes: 0,
    contentLength: null,
    lastCheckedAt: null,
    lastError: null,
    rollback: null,
    rollbackError: null,
    rollbackBusy: false,
  });
  setPendingUpdateForTests(null);
  check.mockReset();
  relaunch.mockReset();
  invoke.mockReset();
  invoke.mockResolvedValue(snapshot);
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

  it("snapshots the installed build before it is replaced, and keeps it after", async () => {
    check.mockResolvedValue(fakeUpdate());
    await useUpdateStore.getState().check("startup");

    expect(invoke).toHaveBeenCalledWith("update_snapshot_create");
    expect(useUpdateStore.getState().rollback).toEqual(snapshot);
    expect(useUpdateStore.getState().rollbackError).toBeNull();
  });

  it("takes the Windows snapshot at the click, where the install actually happens", async () => {
    installsWhileRunning.mockReturnValue(false);
    check.mockResolvedValue(fakeUpdate());
    await useUpdateStore.getState().check("startup");
    expect(invoke).not.toHaveBeenCalled();

    await useUpdateStore.getState().applyUpdate();
    expect(invoke).toHaveBeenCalledWith("update_snapshot_create");
  });

  it("installs anyway when the snapshot fails — no rollback beats no update", async () => {
    invoke.mockRejectedValue(new Error("disk full"));
    const update = fakeUpdate();
    check.mockResolvedValue(update);

    await useUpdateStore.getState().check("startup");

    expect(update.downloadAndInstall).toHaveBeenCalledTimes(1);
    expect(useUpdateStore.getState().status).toBe("ready");
    expect(useUpdateStore.getState().rollback).toBeNull();
    expect(useUpdateStore.getState().rollbackError).toContain("disk full");
  });

  it("discards a snapshot through the backend and clears it locally", async () => {
    useUpdateStore.setState({ rollback: snapshot });
    invoke.mockResolvedValue(undefined);

    await useUpdateStore.getState().discardRollback();

    expect(invoke).toHaveBeenCalledWith("update_rollback_discard");
    expect(useUpdateStore.getState().rollback).toBeNull();
    expect(useUpdateStore.getState().rollbackBusy).toBe(false);
  });

  it("reports a rollback that could not start instead of pretending it did", async () => {
    useUpdateStore.setState({ rollback: snapshot });
    invoke.mockRejectedValue(new Error("no snapshot"));

    await useUpdateStore.getState().applyRollback();

    expect(invoke).toHaveBeenCalledWith("update_rollback_apply");
    expect(useUpdateStore.getState().rollbackError).toContain("no snapshot");
    expect(useUpdateStore.getState().rollbackBusy).toBe(false);
  });
});

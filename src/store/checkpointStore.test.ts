import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

import { useCheckpointStore, type CheckpointInfo } from "./checkpointStore";

function makeInfo(overrides: Partial<CheckpointInfo> = {}): CheckpointInfo {
  return {
    id: "id-1",
    createdAtMs: Date.now(),
    sessionId: "session-1",
    anchorIndex: 0,
    label: "did something",
    files: 2,
    shellRan: false,
    reverted: false,
    reapplyable: false,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useCheckpointStore.setState({ bySession: {}, loadingSessions: {}, errorsBySession: {} });
});

describe("checkpointStore.refresh", () => {
  it("calls checkpoint_list with the session id and caches the result", async () => {
    const info = makeInfo();
    invokeMock.mockResolvedValueOnce([info]);

    await useCheckpointStore.getState().refresh("session-1");

    expect(invokeMock).toHaveBeenCalledWith("checkpoint_list", { sessionId: "session-1" });
    expect(useCheckpointStore.getState().bySession["session-1"]).toEqual([info]);
    expect(useCheckpointStore.getState().loadingSessions["session-1"]).toBe(false);
    expect(useCheckpointStore.getState().errorsBySession["session-1"]).toBeNull();
  });

  it("records a failure in errorsBySession instead of throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("disk unavailable"));

    await expect(useCheckpointStore.getState().refresh("session-1")).resolves.toBeUndefined();

    expect(useCheckpointStore.getState().errorsBySession["session-1"]).toBe("disk unavailable");
    expect(useCheckpointStore.getState().loadingSessions["session-1"]).toBe(false);
    // A failed refresh must not fabricate an empty list for a session that
    // may have had a previously cached (valid) one.
    expect(useCheckpointStore.getState().bySession["session-1"]).toBeUndefined();
  });

  it("caches each session's checkpoints independently (split-pane safe)", async () => {
    const infoA = makeInfo({ id: "a", sessionId: "session-a" });
    const infoB = makeInfo({ id: "b", sessionId: "session-b" });
    invokeMock.mockResolvedValueOnce([infoA]).mockResolvedValueOnce([infoB]);

    await useCheckpointStore.getState().refresh("session-a");
    await useCheckpointStore.getState().refresh("session-b");

    expect(useCheckpointStore.getState().bySession["session-a"]).toEqual([infoA]);
    expect(useCheckpointStore.getState().bySession["session-b"]).toEqual([infoB]);
  });

  it("clears a stale error once a later refresh succeeds", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom")).mockResolvedValueOnce([makeInfo()]);

    await useCheckpointStore.getState().refresh("session-1");
    expect(useCheckpointStore.getState().errorsBySession["session-1"]).toBe("boom");

    await useCheckpointStore.getState().refresh("session-1");
    expect(useCheckpointStore.getState().errorsBySession["session-1"]).toBeNull();
    expect(useCheckpointStore.getState().bySession["session-1"]).toHaveLength(1);
  });
});

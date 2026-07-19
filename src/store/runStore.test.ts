import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  listRuns: vi.fn(),
  getRun: vi.fn(),
  loadRunEvents: vi.fn(),
  checkRunLedgerIntegrity: vi.fn(),
  onRunsChanged: vi.fn(),
  archiveRun: vi.fn(),
  unarchiveRun: vi.fn(),
}));
vi.mock("../lib/runProtocol", () => mocks);

import { disposeRunStoreSubscription, initializeRunStore, useRunStore } from "./runStore";
import type { RunRecord } from "../lib/runProtocol";

function run(id: string, created: number): RunRecord {
  return {
    spec: { run_id: id, created_at_ms: created } as RunRecord["spec"],
    status: "queued",
    lastSequence: 0,
    terminalSequence: null,
    updatedAtMs: created,
    archivedAtMs: null,
  };
}

function archivedRun(id: string, created: number, archivedAtMs: number): RunRecord {
  return { ...run(id, created), status: "cancelled", archivedAtMs };
}

describe("runStore", () => {
  beforeEach(() => {
    disposeRunStoreSubscription();
    useRunStore.setState({
      runs: [], selectedRunId: null, eventsByRun: {}, loading: false,
      detailLoading: false, error: null, integrity: null, showArchived: false,
    });
    vi.clearAllMocks();
    mocks.onRunsChanged.mockResolvedValue(() => {});
    mocks.loadRunEvents.mockResolvedValue([]);
  });

  it("loads newest runs and their selected event history", async () => {
    const older = run("run-old", 1);
    const newer = run("run-new", 2);
    mocks.listRuns.mockResolvedValue([newer, older]);
    mocks.getRun.mockResolvedValue(newer);
    await useRunStore.getState().refresh();
    expect(useRunStore.getState().selectedRunId).toBe("run-new");
    expect(mocks.loadRunEvents).toHaveBeenCalledWith("run-new");
  });

  it("installs one listener and refreshes the changed run", async () => {
    const record = run("run-1", 1);
    let changed: ((payload: { runId: string }) => void) | undefined;
    mocks.onRunsChanged.mockImplementation(async (handler) => {
      changed = handler;
      return () => {};
    });
    mocks.listRuns.mockResolvedValue([record]);
    mocks.getRun.mockResolvedValue(record);
    await Promise.all([initializeRunStore(), initializeRunStore()]);
    expect(mocks.onRunsChanged).toHaveBeenCalledTimes(1);
    mocks.getRun.mockClear();
    changed?.({ runId: "run-1" });
    await vi.waitFor(() => expect(mocks.getRun).toHaveBeenCalledWith("run-1"));
  });

  it("surfaces integrity violations without discarding history", async () => {
    mocks.checkRunLedgerIntegrity.mockResolvedValue({ ok: false, violations: ["sequence gap"] });
    await useRunStore.getState().checkIntegrity();
    expect(useRunStore.getState().integrity).toEqual({ ok: false, violations: ["sequence gap"] });
  });

  it("archiving a run drops it from the visible list once showArchived is off", async () => {
    const visible = run("run-visible", 1);
    const target = run("run-target", 2);
    useRunStore.setState({ runs: [target, visible], showArchived: false });
    mocks.archiveRun.mockResolvedValue(archivedRun("run-target", 2, 9_000));
    mocks.getRun.mockResolvedValue(archivedRun("run-target", 2, 9_000));

    await useRunStore.getState().archiveRun("run-target");

    expect(mocks.archiveRun).toHaveBeenCalledWith("run-target");
    const ids = useRunStore.getState().runs.map((entry) => entry.spec.run_id);
    expect(ids).toEqual(["run-visible"]);
  });

  it("showing archived runs keeps an archived run's refresh visible", async () => {
    useRunStore.setState({ runs: [], showArchived: true });
    mocks.getRun.mockResolvedValue(archivedRun("run-target", 2, 9_000));

    await useRunStore.getState().refreshRun("run-target");

    const ids = useRunStore.getState().runs.map((entry) => entry.spec.run_id);
    expect(ids).toEqual(["run-target"]);
  });

  it("setShowArchived toggles the flag and reloads with the archived filter", async () => {
    mocks.listRuns.mockResolvedValue([]);
    await useRunStore.getState().setShowArchived(true);
    expect(useRunStore.getState().showArchived).toBe(true);
    expect(mocks.listRuns).toHaveBeenCalledWith(200, true);
  });
});

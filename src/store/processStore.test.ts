import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  listProcesses: vi.fn(),
  signalProcess: vi.fn(),
  onProcessesChanged: vi.fn(),
}));

vi.mock("../lib/processTable", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/processTable")>();
  return {
    ...actual,
    listProcesses: (...args: unknown[]) => mocks.listProcesses(...args),
    onProcessesChanged: (...args: unknown[]) => mocks.onProcessesChanged(...args),
  };
});
// `signalProcess` and the display derivation live in their own module so they
// stay out of the eager chunk — see `processSignals.ts`.
vi.mock("../lib/processSignals", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/processSignals")>();
  return { ...actual, signalProcess: (...args: unknown[]) => mocks.signalProcess(...args) };
});

import {
  selectLiveProcessCount,
  selectStateCounts,
  subscribeToProcessChanges,
  useProcessStore,
} from "./processStore";
import type { ProcessKind, ProcessRecord, ProcessState } from "../lib/processTable";

function record(overrides: {
  processId: string;
  kind?: ProcessKind;
  state?: ProcessState;
  createdAtMs?: number;
  stopRequested?: boolean;
  suspendRequested?: boolean;
}): ProcessRecord {
  return {
    processId: overrides.processId,
    parentProcessId: null,
    kind: overrides.kind ?? "chat_turn",
    externalId: `external-${overrides.processId}`,
    state: overrides.state ?? "running",
    runId: null,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: {},
    signalIntent: {
      stopRequested: overrides.stopRequested ?? false,
      suspendRequested: overrides.suspendRequested ?? false,
      killRequested: false,
    },
    signalReason: null,
    signalRequestedAtMs: null,
    exit: null,
    createdAtMs: overrides.createdAtMs ?? 1_000,
    updatedAtMs: 1_000,
    startedAtMs: null,
    exitedAtMs: null,
  };
}

beforeEach(() => {
  mocks.listProcesses.mockReset();
  mocks.listProcesses.mockResolvedValue([]);
  mocks.signalProcess.mockReset();
  mocks.onProcessesChanged.mockReset();
  useProcessStore.setState({ records: [], loading: false, error: null, pending: {} });
});

afterEach(() => {
  useProcessStore.setState({ records: [], loading: false, error: null, pending: {} });
});

describe("processStore.refresh", () => {
  it("reads only live processes and sorts them newest first", async () => {
    mocks.listProcesses.mockResolvedValue([
      record({ processId: "old", createdAtMs: 1 }),
      record({ processId: "new", createdAtMs: 9 }),
    ]);

    await useProcessStore.getState().refresh();

    expect(mocks.listProcesses).toHaveBeenCalledWith({ liveOnly: true, limit: 500 });
    expect(useProcessStore.getState().records.map((entry) => entry.processId)).toEqual(["new", "old"]);
    expect(useProcessStore.getState().loading).toBe(false);
  });

  it("surfaces a read failure instead of leaving the panel spinning", async () => {
    mocks.listProcesses.mockRejectedValue(new Error("ledger locked"));

    await useProcessStore.getState().refresh();

    expect(useProcessStore.getState().loading).toBe(false);
    expect(useProcessStore.getState().error).toContain("ledger locked");
  });
});

describe("processStore.applyRecord", () => {
  it("inserts an unseen record in sort order", () => {
    useProcessStore.setState({ records: [record({ processId: "a", createdAtMs: 5 })] });

    useProcessStore.getState().applyRecord(record({ processId: "b", createdAtMs: 7 }));

    expect(useProcessStore.getState().records.map((entry) => entry.processId)).toEqual(["b", "a"]);
  });

  it("replaces rather than duplicates a record it already holds", () => {
    useProcessStore.setState({ records: [record({ processId: "a" })] });

    useProcessStore.getState().applyRecord(record({ processId: "a", suspendRequested: true }));

    const records = useProcessStore.getState().records;
    expect(records).toHaveLength(1);
    expect(records[0].signalIntent.suspendRequested).toBe(true);
  });

  it("drops an exited record from the live listing", () => {
    useProcessStore.setState({ records: [record({ processId: "a" })] });

    useProcessStore.getState().applyRecord(record({ processId: "a", state: "exited" }));

    expect(useProcessStore.getState().records).toEqual([]);
  });
});

describe("processStore.signal", () => {
  it("applies the returned record so the row updates without a refetch", async () => {
    useProcessStore.setState({ records: [record({ processId: "a" })] });
    mocks.signalProcess.mockResolvedValue(record({ processId: "a", suspendRequested: true }));

    await useProcessStore.getState().signal("a", "suspend", "why");

    expect(mocks.signalProcess).toHaveBeenCalledWith("a", "suspend", "why");
    expect(useProcessStore.getState().records[0].signalIntent.suspendRequested).toBe(true);
    expect(useProcessStore.getState().pending).toEqual({});
  });

  it("shows a refusal reason rather than swallowing it", async () => {
    // The point of typed refusals: a kind that cannot honour a signal says
    // why, and a button that silently did nothing would be worse than an error.
    mocks.signalProcess.mockRejectedValue(
      new Error("a workflow node has no independent pause mechanism"),
    );

    await useProcessStore.getState().signal("a", "suspend");

    expect(useProcessStore.getState().error).toContain("no independent pause mechanism");
    expect(useProcessStore.getState().pending).toEqual({});
  });

  it("marks the process pending only while the signal is in flight", async () => {
    let release: (value: ProcessRecord) => void = () => {};
    mocks.signalProcess.mockImplementation(
      () => new Promise<ProcessRecord>((resolve) => { release = resolve; }),
    );

    const call = useProcessStore.getState().signal("a", "stop");
    expect(useProcessStore.getState().pending.a).toBe(true);

    release(record({ processId: "a", stopRequested: true }));
    await call;
    expect(useProcessStore.getState().pending.a).toBeUndefined();
  });
});

describe("selectors", () => {
  it("counts by DERIVED state, so a pause-pending row is not counted as running", () => {
    useProcessStore.setState({
      records: [
        record({ processId: "a" }),
        record({ processId: "b", suspendRequested: true }),
        record({ processId: "c", state: "suspended", suspendRequested: true }),
        record({ processId: "d", stopRequested: true }),
      ],
    });

    const counts = selectStateCounts(useProcessStore.getState().records);
    expect(counts.sort((x, y) => x.state.localeCompare(y.state))).toEqual([
      { state: "pause_pending", count: 1 },
      { state: "running", count: 1 },
      { state: "stopping", count: 1 },
      { state: "suspended", count: 1 },
    ]);
    expect(selectLiveProcessCount(useProcessStore.getState())).toBe(4);
  });
});

describe("subscribeToProcessChanges", () => {
  it("routes an event straight into the store and returns its unsubscribe", async () => {
    const unlisten = vi.fn();
    // Held in an object rather than a `let`: TypeScript cannot see the
    // assignment happening inside the mock's callback and would narrow a bare
    // local to `null` at the call site below.
    const captured: { handler: ((record: ProcessRecord) => void) | null } = { handler: null };
    mocks.onProcessesChanged.mockImplementation(async (fn: (record: ProcessRecord) => void) => {
      captured.handler = fn;
      return unlisten;
    });

    const cleanup = await subscribeToProcessChanges();
    captured.handler?.(record({ processId: "a" }));

    expect(useProcessStore.getState().records.map((entry) => entry.processId)).toEqual(["a"]);
    expect(cleanup).toBe(unlisten);
  });
});

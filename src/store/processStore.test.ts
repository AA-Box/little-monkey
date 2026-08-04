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
  PROCESS_CATCH_UP_INTERVAL_MS,
  selectLiveProcessCount,
  selectStateCounts,
  startProcessCatchUp,
  subscribeToProcessChanges,
  useProcessStore,
} from "./processStore";
import type { ProcessKind, ProcessRecord, ProcessState } from "../lib/processTable";

function record(overrides: {
  processId: string;
  kind?: ProcessKind;
  state?: ProcessState;
  createdAtMs?: number;
  updatedAtMs?: number;
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
    updatedAtMs: overrides.updatedAtMs ?? 1_000,
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

describe("processStore.catchUp", () => {
  // The gap this closes: `monkey processes signal` writes the same SQLite
  // ledger from another OS process and cannot emit `processes://changed` into
  // this one, so the event path alone leaves a CLI-paused row rendering its
  // previous state until the panel is remounted.
  it("picks up a suspend written by another OS process", async () => {
    useProcessStore.setState({ records: [record({ processId: "a" })] });
    mocks.listProcesses.mockResolvedValue([
      record({ processId: "a", suspendRequested: true, updatedAtMs: 2_000 }),
    ]);

    await useProcessStore.getState().catchUp();

    expect(useProcessStore.getState().records[0].signalIntent.suspendRequested).toBe(true);
  });

  it("leaves the records array untouched when nothing changed, so a quiet poll costs no re-render", async () => {
    const before = [record({ processId: "a" }), record({ processId: "b", createdAtMs: 5 })];
    useProcessStore.setState({ records: [...before].sort((x, y) => y.createdAtMs - x.createdAtMs) });
    const identity = useProcessStore.getState().records;
    // Fresh objects with identical field values: a shallow identity check on
    // the records would call this a change, which is the trap being pinned.
    mocks.listProcesses.mockResolvedValue([
      record({ processId: "a" }),
      record({ processId: "b", createdAtMs: 5 }),
    ]);
    const listener = vi.fn();
    const unsubscribe = useProcessStore.subscribe(listener);

    await useProcessStore.getState().catchUp();
    unsubscribe();

    expect(useProcessStore.getState().records).toBe(identity);
    expect(listener).not.toHaveBeenCalled();
  });

  it("notices a state change that leaves the row count and stamp alone", async () => {
    // `updated_at_ms` is written from the signal's own timestamp, so two
    // signals inside one millisecond share a stamp; the comparison has to look
    // at what the row draws, not only at the clock.
    useProcessStore.setState({ records: [record({ processId: "a", suspendRequested: true })] });
    mocks.listProcesses.mockResolvedValue([
      record({ processId: "a", state: "suspended", suspendRequested: true }),
    ]);

    await useProcessStore.getState().catchUp();

    expect(useProcessStore.getState().records[0].state).toBe("suspended");
  });

  it("never toggles loading, and hides a read failure rather than banner-flashing every tick", async () => {
    useProcessStore.setState({ records: [record({ processId: "a" })] });
    mocks.listProcesses.mockRejectedValue(new Error("ledger locked"));

    await useProcessStore.getState().catchUp();

    expect(useProcessStore.getState().loading).toBe(false);
    expect(useProcessStore.getState().error).toBeNull();
    // The last good listing stays on screen instead of emptying out.
    expect(useProcessStore.getState().records).toHaveLength(1);
  });

  it("stands down while a signal is in flight so the row does not flick back", async () => {
    useProcessStore.setState({
      records: [record({ processId: "a", suspendRequested: true })],
      pending: { a: true },
    });
    mocks.listProcesses.mockResolvedValue([record({ processId: "a" })]);

    await useProcessStore.getState().catchUp();

    expect(mocks.listProcesses).not.toHaveBeenCalled();
    expect(useProcessStore.getState().records[0].signalIntent.suspendRequested).toBe(true);
  });
});

describe("startProcessCatchUp", () => {
  it("ticks the clock and reads the ledger on every interval, and stops on cleanup", () => {
    vi.useFakeTimers();
    try {
      const tick = vi.fn();
      const stop = startProcessCatchUp(tick);

      vi.advanceTimersByTime(PROCESS_CATCH_UP_INTERVAL_MS * 2);
      expect(tick).toHaveBeenCalledTimes(2);
      expect(mocks.listProcesses).toHaveBeenCalledTimes(2);

      stop();
      vi.advanceTimersByTime(PROCESS_CATCH_UP_INTERVAL_MS * 3);
      expect(tick).toHaveBeenCalledTimes(2);
      expect(mocks.listProcesses).toHaveBeenCalledTimes(2);
    } finally {
      vi.useRealTimers();
    }
  });

  it("ticks the clock even when the listing is unchanged, so a row's age keeps moving", () => {
    // The age is `Date.now()` at render. A poll that only wrote the store when
    // something changed would leave every age frozen on an idle panel.
    vi.useFakeTimers();
    try {
      const tick = vi.fn();
      useProcessStore.setState({ records: [record({ processId: "a" })] });
      mocks.listProcesses.mockResolvedValue([record({ processId: "a" })]);

      const stop = startProcessCatchUp(tick);
      vi.advanceTimersByTime(PROCESS_CATCH_UP_INTERVAL_MS * 3);
      stop();

      expect(tick).toHaveBeenCalledTimes(3);
    } finally {
      vi.useRealTimers();
    }
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

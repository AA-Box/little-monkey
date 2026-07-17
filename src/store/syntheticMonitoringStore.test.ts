import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => resolveTargetMock(...args),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const runMonitorJourneyMock = vi.fn();
vi.mock("../lib/syntheticMonitoring", async () => {
  const actual = await vi.importActual<typeof import("../lib/syntheticMonitoring")>("../lib/syntheticMonitoring");
  return {
    ...actual,
    runMonitorJourney: (...args: unknown[]) => runMonitorJourneyMock(...args),
  };
});

// vitest's "node" test environment has no `localStorage` global — stub an
// in-memory one so the store's real persistence path is exercised rather
// than skipped (same shim `workflowDraftStore.test.ts` uses).
beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
});

import {
  runSyntheticMonitoringTickForTests,
  useSyntheticMonitoringStore,
} from "./syntheticMonitoringStore";
import type { MonitorRun } from "../lib/syntheticMonitoring";

function demoInput() {
  return {
    name: "Homepage",
    url: "https://example.com/",
    targetEnv: "production" as const,
    intervalMinutes: 5,
    assertion: { type: "textPresent" as const, value: "Welcome" },
  };
}

function passingRun(monitorId: string): MonitorRun {
  return {
    id: "run-1",
    monitorId,
    monitorName: "Homepage",
    url: "https://example.com/",
    targetEnv: "production",
    startedAtMs: 1_000,
    finishedAtMs: 1_200,
    status: "pass",
    latencyMs: 200,
    failureReason: null,
    diagnosis: null,
    evidence: { screenshotArtifactId: "shot-1", domArtifactId: null, consoleArtifactId: null, networkArtifactId: null },
  };
}

beforeEach(() => {
  localStorage.clear();
  resolveTargetMock.mockReset();
  attemptStreamMock.mockReset();
  runMonitorJourneyMock.mockReset();
  useSyntheticMonitoringStore.setState({
    monitors: [],
    runsByMonitor: {},
    runningMonitorIds: {},
    selectedMonitorId: null,
    error: null,
  });
});

describe("syntheticMonitoringStore CRUD", () => {
  it("adds a monitor, selects it, and persists it to localStorage", () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    const state = useSyntheticMonitoringStore.getState();
    expect(state.monitors).toHaveLength(1);
    expect(state.selectedMonitorId).toBe(monitor.id);
    expect(JSON.parse(localStorage.getItem("little-monkey-synthetic-monitors-v1")!).monitors).toHaveLength(1);
  });

  it("rejects an invalid URL rather than silently saving a broken monitor", () => {
    expect(() => useSyntheticMonitoringStore.getState().addMonitor({ ...demoInput(), url: "file:///etc/passwd" })).toThrow();
    expect(useSyntheticMonitoringStore.getState().monitors).toHaveLength(0);
  });

  it("updates a monitor in place, preserving its id/enabled/lastRunAtMs", () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    useSyntheticMonitoringStore.getState().toggleMonitor(monitor.id);
    useSyntheticMonitoringStore.getState().updateMonitor(monitor.id, { ...demoInput(), name: "Renamed", intervalMinutes: 10 });
    const updated = useSyntheticMonitoringStore.getState().monitors[0];
    expect(updated.id).toBe(monitor.id);
    expect(updated.name).toBe("Renamed");
    expect(updated.intervalMinutes).toBe(10);
    expect(updated.enabled).toBe(false);
  });

  it("surfaces an invalid update as a store error instead of throwing out of the caller", () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    useSyntheticMonitoringStore.getState().updateMonitor(monitor.id, { ...demoInput(), url: "not a url" });
    expect(useSyntheticMonitoringStore.getState().error).toBeTruthy();
    // The original monitor is untouched by the rejected update.
    expect(useSyntheticMonitoringStore.getState().monitors[0].url).toBe("https://example.com/");
  });

  it("deletes a monitor and its run history together", () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    useSyntheticMonitoringStore.setState({ runsByMonitor: { [monitor.id]: [passingRun(monitor.id)] } });
    useSyntheticMonitoringStore.getState().deleteMonitor(monitor.id);
    const state = useSyntheticMonitoringStore.getState();
    expect(state.monitors).toHaveLength(0);
    expect(state.runsByMonitor[monitor.id]).toBeUndefined();
  });

  it("toggles enabled on and off", () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    expect(useSyntheticMonitoringStore.getState().monitors[0].enabled).toBe(true);
    useSyntheticMonitoringStore.getState().toggleMonitor(monitor.id);
    expect(useSyntheticMonitoringStore.getState().monitors[0].enabled).toBe(false);
  });
});

describe("runMonitorNow", () => {
  it("records the finished run, caps history, and updates lastRunAtMs", async () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    runMonitorJourneyMock.mockResolvedValue(passingRun(monitor.id));

    await useSyntheticMonitoringStore.getState().runMonitorNow(monitor.id);

    const state = useSyntheticMonitoringStore.getState();
    expect(state.monitors[0].lastRunAtMs).toBe(1_200);
    expect(state.runsByMonitor[monitor.id]).toHaveLength(1);
    expect(state.runningMonitorIds[monitor.id]).toBeUndefined();
    expect(JSON.parse(localStorage.getItem("little-monkey-synthetic-monitor-runs-v1")!).runsByMonitor[monitor.id]).toHaveLength(1);
  });

  it("does not start a second run for a monitor that is already running", async () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    let resolveFirst!: (run: MonitorRun) => void;
    runMonitorJourneyMock.mockReturnValueOnce(new Promise((resolve) => { resolveFirst = resolve; }));

    const first = useSyntheticMonitoringStore.getState().runMonitorNow(monitor.id);
    expect(useSyntheticMonitoringStore.getState().runningMonitorIds[monitor.id]).toBe(true);
    await useSyntheticMonitoringStore.getState().runMonitorNow(monitor.id);
    expect(runMonitorJourneyMock).toHaveBeenCalledTimes(1);

    resolveFirst(passingRun(monitor.id));
    await first;
  });

  it("wires a diagnose callback that resolves the active target lazily and calls attemptStream without recording usage", async () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://127.0.0.1:1", modelLabel: "Local" });
    attemptStreamMock.mockResolvedValue({ content: "Likely a bad deploy.", streamError: null });

    runMonitorJourneyMock.mockImplementation(async (_monitor, options) => {
      const run: MonitorRun = { ...passingRun(monitor.id), status: "fail", failureReason: "boom", diagnosis: null };
      const diagnosis = await options.diagnose(monitor, run, "evidence excerpt");
      return { ...run, diagnosis };
    });

    await useSyntheticMonitoringStore.getState().runMonitorNow(monitor.id);

    expect(resolveTargetMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    const call = attemptStreamMock.mock.calls[0];
    expect(call[5]).toBe(`synthetic-monitor:${monitor.id}`);
    expect(call[7]).toBe(false); // recordUsage
    expect(useSyntheticMonitoringStore.getState().runsByMonitor[monitor.id][0].diagnosis).toBe("Likely a bad deploy.");
  });

  it("caps run history at the retention limit", async () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    for (let i = 0; i < 25; i += 1) {
      runMonitorJourneyMock.mockResolvedValueOnce({ ...passingRun(monitor.id), id: `run-${i}` });
      await useSyntheticMonitoringStore.getState().runMonitorNow(monitor.id);
    }
    expect(useSyntheticMonitoringStore.getState().runsByMonitor[monitor.id]).toHaveLength(20);
  });
});

describe("scheduled tick", () => {
  it("runs at most one due monitor per tick, skipping disabled and not-yet-due monitors", async () => {
    const due = useSyntheticMonitoringStore.getState().addMonitor({ ...demoInput(), name: "Due" });
    const notDue = useSyntheticMonitoringStore.getState().addMonitor({ ...demoInput(), name: "Not due", intervalMinutes: 60 });
    useSyntheticMonitoringStore.setState((state) => ({
      monitors: state.monitors.map((entry) => (entry.id === notDue.id ? { ...entry, lastRunAtMs: Date.now() } : entry)),
    }));
    runMonitorJourneyMock.mockResolvedValue(passingRun(due.id));

    await runSyntheticMonitoringTickForTests();

    expect(runMonitorJourneyMock).toHaveBeenCalledTimes(1);
    expect(useSyntheticMonitoringStore.getState().monitors.find((m) => m.id === due.id)?.lastRunAtMs).not.toBeNull();
  });

  it("does not run a disabled monitor", async () => {
    const monitor = useSyntheticMonitoringStore.getState().addMonitor(demoInput());
    useSyntheticMonitoringStore.getState().toggleMonitor(monitor.id);
    await runSyntheticMonitoringTickForTests();
    expect(runMonitorJourneyMock).not.toHaveBeenCalled();
  });
});

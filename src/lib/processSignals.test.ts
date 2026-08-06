import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

import { canResume, canSuspend, processDisplayState, signalProcess } from "./processSignals";
import type { ProcessRecord } from "./processTable";

beforeEach(() => {
  invokeMock.mockReset();
});

/** A live record with the intent bits under test; everything else is filler
 * the derivation never reads. */
function liveRecord(overrides: Partial<ProcessRecord> = {}): ProcessRecord {
  return {
    processId: "process-1",
    parentProcessId: null,
    kind: "chat_turn",
    externalId: "turn-1",
    state: "running",
    runId: null,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: {},
    signalIntent: { stopRequested: false, suspendRequested: false , killRequested: false,},
    signalReason: null,
    signalRequestedAtMs: null,
    exit: null,
    createdAtMs: 1,
    updatedAtMs: 1,
    startedAtMs: null,
    exitedAtMs: null,
    ...overrides,
  };
}

describe("processDisplayState", () => {
  it("passes through a state with no signal latched", () => {
    expect(processDisplayState(liveRecord())).toBe("running");
    expect(processDisplayState(liveRecord({ state: "admitted" }))).toBe("admitted");
    expect(processDisplayState(liveRecord({ state: "suspended" }))).toBe("suspended");
  });

  it("derives pause_pending for a suspend that has not reached its safe point", () => {
    // The honest middle state: asked for, not arrived. Reporting `suspended`
    // here would claim a park that has not happened, and `running` would hide
    // that the user already asked.
    const record = liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true , killRequested: false,} });
    expect(processDisplayState(record)).toBe("pause_pending");
  });

  it("reports a process that has actually parked as suspended, not pause_pending", () => {
    const record = liveRecord({
      state: "suspended",
      signalIntent: { stopRequested: false, suspendRequested: true , killRequested: false,},
    });
    expect(processDisplayState(record)).toBe("suspended");
  });

  it("lets a pending stop outrank a pending pause", () => {
    // The two latches are independent — `resume` never clears a stop — so a
    // process on its way out must not read as merely pausing.
    const record = liveRecord({ signalIntent: { stopRequested: true, suspendRequested: true , killRequested: false,} });
    expect(processDisplayState(record)).toBe("stopping");
  });

  it("reports an exited process as exited whatever is still latched on it", () => {
    const record = liveRecord({
      state: "exited",
      signalIntent: { stopRequested: true, suspendRequested: true , killRequested: false,},
    });
    expect(processDisplayState(record)).toBe("exited");
  });
});

describe("canSuspend / canResume", () => {
  it("offers suspend only where it would say something new", () => {
    expect(canSuspend(liveRecord())).toBe(true);
    expect(
      canSuspend(liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true , killRequested: false,} })),
    ).toBe(false);
    expect(canSuspend(liveRecord({ state: "exited" }))).toBe(false);
  });

  it("offers resume for a parked process and for one still on its way there", () => {
    const pending = liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true , killRequested: false,} });
    const parked = liveRecord({
      state: "suspended",
      signalIntent: { stopRequested: false, suspendRequested: true , killRequested: false,},
    });
    expect(canResume(pending)).toBe(true);
    expect(canResume(parked)).toBe(true);
    expect(canResume(liveRecord())).toBe(false);
  });

  it("offers resume for a process the OS stopped even with no latch left", () => {
    // Found by hand, not by a test. A background shell suspended in-app and
    // then resumed from `monkey processes signal` ends up `suspended` with the
    // latch already cleared. Keying only on the latch called that "nothing to
    // resume", so the panel rendered Pause on a stopped child and there was no
    // way back short of `kill -CONT` from outside the app.
    const strandedByClearedLatch = liveRecord({
      state: "suspended",
      signalIntent: { stopRequested: false, suspendRequested: false, killRequested: false },
    });
    expect(canResume(strandedByClearedLatch)).toBe(true);
    expect(canSuspend(strandedByClearedLatch)).toBe(false);

    // And an exited row offers neither, whatever the latch says.
    const exited = liveRecord({
      state: "exited",
      signalIntent: { stopRequested: false, suspendRequested: true, killRequested: false },
    });
    expect(canResume(exited)).toBe(false);
    expect(canSuspend(exited)).toBe(false);
  });
});

describe("signalProcess", () => {
  it("passes the signal and reason through to the backend command", async () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue(liveRecord());

    await signalProcess("process-1", "suspend", "Paused from the Processes panel");

    expect(invokeMock).toHaveBeenCalledWith("process_signal", {
      processId: "process-1",
      signal: "suspend",
      reason: "Paused from the Processes panel",
    });
  });

  it("rejects rather than swallowing a refusal, unlike every other call here", async () => {
    // Deliberate asymmetry: the rest of this module is fail-soft bookkeeping,
    // but a refused signal is a direct answer to a direct user action.
    invokeMock.mockReset();
    invokeMock.mockRejectedValue("a workflow node has no independent pause mechanism");

    await expect(signalProcess("process-1", "suspend")).rejects.toBeTruthy();
  });
});

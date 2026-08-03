/**
 * The decision table, and the two properties that make it safe to run on a
 * timer:
 *
 * 1. Stop wins over suspend, and state is the acknowledgement — the same two
 *    rules the daemon's tick already follows, so the two readers of one latch
 *    cannot disagree about what it means.
 * 2. Delivery is exhaustive over `ProcessKind` *at typecheck time* (see
 *    `DELIVERS_HERE`). Adding a tenth kind without deciding who delivers it is a
 *    compile error rather than a signal that silently goes nowhere.
 *
 * `runCancellationRegistry` is deliberately NOT mocked: it is the real registry a
 * chat turn and a crew member register into, so asserting through it proves the
 * fan-out reaches the primitive rather than proving the mock was called. The
 * heavier sibling modules are mocked — their own tests cover that
 * `cancelSubagentRun`/`cancelSideTask` key on the same id these records carry.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const cancelSubagentRunMock = vi.fn<(id: string) => boolean>(() => true);
vi.mock("./subagent", () => ({
  cancelSubagentRun: (id: string) => cancelSubagentRunMock(id),
}));

const sideTaskMock = {
  cancel: vi.fn<(id: string) => void>(),
  pause: vi.fn<(id: string) => void>(),
  resume: vi.fn<(id: string) => void>(),
};
vi.mock("./sideTaskRunner", () => ({
  cancelSideTask: (id: string) => sideTaskMock.cancel(id),
  pauseSideTask: (id: string) => sideTaskMock.pause(id),
  resumeSideTask: (id: string) => sideTaskMock.resume(id),
}));

import {
  DESKTOP_DELIVERABLE_KINDS,
  deliverProcessSignal,
  sweepPendingProcessSignals,
} from "./processSignalDelivery";
import type { ProcessKind, ProcessRecord, ProcessState } from "./processTable";
import {
  clearRunCancellationRegistryForTests,
  registerRunCancellation,
} from "./runCancellationRegistry";
import {
  clearPauseRegistryForTests,
  isPauseRequested,
  setPauseRequested,
} from "./pauseRegistry";

const MAIN = { ownsGlobalKinds: true };
const SECONDARY = { ownsGlobalKinds: false };

function record(overrides: Partial<ProcessRecord> & { kind: ProcessKind }): ProcessRecord {
  return {
    processId: `proc-${overrides.kind}`,
    parentProcessId: null,
    externalId: "ext-1",
    state: "running" as ProcessState,
    runId: null,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: {},
    signalIntent: { stopRequested: false, suspendRequested: false },
    signalReason: null,
    signalRequestedAtMs: null,
    exit: null,
    createdAtMs: 0,
    updatedAtMs: 0,
    startedAtMs: null,
    exitedAtMs: null,
    ...overrides,
  };
}

const stop = { stopRequested: true, suspendRequested: false };
const suspend = { stopRequested: false, suspendRequested: true };

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(true);
  cancelSubagentRunMock.mockReset();
  cancelSubagentRunMock.mockReturnValue(true);
  sideTaskMock.cancel.mockReset();
  sideTaskMock.pause.mockReset();
  sideTaskMock.resume.mockReset();
  clearRunCancellationRegistryForTests();
  clearPauseRegistryForTests();
});

describe("which signal is pending", () => {
  it("reports nothing pending for a clear latch", async () => {
    const outcome = await deliverProcessSignal(record({ kind: "chat_turn" }), MAIN);
    expect(outcome).toBe("nothing-pending");
  });

  it("reports nothing pending once the process has exited", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", state: "exited", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("nothing-pending");
  });

  it("delivers stop, not suspend, when both latches are set", async () => {
    // A suspended loop never reaches its own cancellation branch, so honouring
    // the suspend would park a process that was also asked to wind down.
    const outcome = await deliverProcessSignal(
      record({
        kind: "side_task",
        signalIntent: { stopRequested: true, suspendRequested: true },
      }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(sideTaskMock.cancel).toHaveBeenCalledWith("ext-1");
    expect(sideTaskMock.pause).not.toHaveBeenCalled();
  });

  it("treats a suspended state as the acknowledgement of a suspend", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "side_task", state: "suspended", signalIntent: suspend }),
      MAIN,
    );
    expect(outcome).toBe("nothing-pending");
    expect(sideTaskMock.pause).not.toHaveBeenCalled();
  });

  it("reads a cleared suspend on a suspended row as a pending resume", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "side_task", state: "suspended" }),
      MAIN,
    );
    expect(outcome).toBe("resumed");
    expect(sideTaskMock.resume).toHaveBeenCalledWith("ext-1");
  });

  it("defers a suspend that arrives before the process is running", async () => {
    // `admitted` has no legal transition to `suspended`; delivering here would
    // fail the ledger's own trigger on every sweep, forever.
    const outcome = await deliverProcessSignal(
      record({ kind: "side_task", state: "admitted", signalIntent: suspend }),
      MAIN,
    );
    expect(outcome).toBe("deferred");
    expect(sideTaskMock.pause).not.toHaveBeenCalled();
  });
});

describe("fan-out to each kind's own primitive", () => {
  it("stops a chat turn through the registry its turn id is registered in", async () => {
    const cancel = vi.fn();
    registerRunCancellation("turn-9", cancel);
    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "turn-9", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("stops a crew member through the same registry", async () => {
    const cancel = vi.fn();
    registerRunCancellation("actor-run-3", cancel);
    const outcome = await deliverProcessSignal(
      record({ kind: "crew_member", externalId: "actor-run-3", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(cancel).toHaveBeenCalledTimes(1);
  });

  it("reports no live target when the turn is running in another window", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "turn-elsewhere", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("no-live-target");
  });

  it("stops a subagent by its cancel id", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "subagent", externalId: "task-4", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(cancelSubagentRunMock).toHaveBeenCalledWith("task-4");
  });

  it("kills a background shell through the command that owns the child", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "background_shell", externalId: "shell-2", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(invokeMock).toHaveBeenCalledWith("background_shell_kill", { id: "shell-2" });
  });

  it("keeps a workflow run's absent registry entry visible as a miss", async () => {
    // `m4_workflows_cancel` returning false is the out-of-process hole K2 still
    // lists as open, not a transport failure — reporting it as `stopped` would
    // hide the one surface that genuinely cannot be cancelled from elsewhere.
    invokeMock.mockResolvedValue(false);
    const outcome = await deliverProcessSignal(
      record({ kind: "workflow_run", externalId: "wf-run-1", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("no-live-target");
    expect(invokeMock).toHaveBeenCalledWith("m4_workflows_cancel", { runId: "wf-run-1" });
  });
});

describe("who delivers what", () => {
  /**
   * `true` where the desktop delivers, `false` where another process does or no
   * primitive exists. `satisfies Record<ProcessKind, boolean>` is the point: a
   * new kind cannot be added to the union without a decision recorded here.
   */
  const DELIVERS_HERE = {
    chat_turn: true,
    subagent: true,
    crew_member: true,
    side_task: true,
    background_shell: true,
    workflow_run: true,
    // The daemon reads its own intent once per tick; a remote run is delivered by
    // whichever host owns it.
    daemon_job: false,
    remote_run: false,
    // Cancelling a node means cancelling its run, which is a different request.
    workflow_node: false,
  } satisfies Record<ProcessKind, boolean>;

  it("scopes the sweep to exactly the kinds it can deliver to", () => {
    const expected = Object.entries(DELIVERS_HERE)
      .filter(([, delivers]) => delivers)
      .map(([kind]) => kind)
      .sort();
    expect([...DESKTOP_DELIVERABLE_KINDS].sort()).toEqual(expected);
  });

  it("never claims to deliver a kind it does not own", async () => {
    for (const [kind, delivers] of Object.entries(DELIVERS_HERE)) {
      // Registered so the window-local kinds have a target to hit; the point of
      // this assertion is which kinds refuse regardless.
      registerRunCancellation("ext-1", vi.fn());
      const outcome = await deliverProcessSignal(
        record({ kind: kind as ProcessKind, signalIntent: stop }),
        MAIN,
      );
      if (delivers) expect(outcome, kind).toBe("stopped");
      else expect(outcome, kind).toMatch(/^(delivered-elsewhere|no-primitive)$/);
    }
  });

  it("names a workflow node's missing primitive rather than blaming another process", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "workflow_node", signalIntent: stop }),
      MAIN,
    );
    expect(outcome).toBe("no-primitive");
  });

  it("leaves the Rust-owned kinds to the one window responsible for them", async () => {
    // Two windows both invoking `background_shell_kill` for the same child is a
    // race with a guaranteed loser, so only the main window delivers these.
    const outcome = await deliverProcessSignal(
      record({ kind: "background_shell", signalIntent: stop }),
      SECONDARY,
    );
    expect(outcome).toBe("delivered-elsewhere");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("still delivers window-local kinds from a secondary window", async () => {
    const cancel = vi.fn();
    registerRunCancellation("turn-in-secondary", cancel);
    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "turn-in-secondary", signalIntent: stop }),
      SECONDARY,
    );
    expect(outcome).toBe("stopped");
    expect(cancel).toHaveBeenCalledTimes(1);
  });
});

describe("the catch-up sweep", () => {
  it("asks only for the kinds it can deliver to, and delivers each", async () => {
    const pending = [
      record({ kind: "subagent", externalId: "task-a", signalIntent: stop }),
      record({ kind: "side_task", externalId: "task-b", signalIntent: suspend }),
    ];
    invokeMock.mockImplementation((command: string) =>
      command === "process_pending_signals" ? Promise.resolve(pending) : Promise.resolve(true),
    );

    const outcomes = await sweepPendingProcessSignals(MAIN);

    expect(outcomes).toEqual(["stopped", "suspended"]);
    expect(invokeMock).toHaveBeenCalledWith("process_pending_signals", {
      kinds: [...DESKTOP_DELIVERABLE_KINDS],
    });
    expect(cancelSubagentRunMock).toHaveBeenCalledWith("task-a");
    expect(sideTaskMock.pause).toHaveBeenCalledWith("task-b");
  });

  it("survives an unavailable backend without throwing", async () => {
    invokeMock.mockRejectedValue(new Error("no ledger"));
    await expect(sweepPendingProcessSignals(MAIN)).resolves.toEqual([]);
  });
});

describe("cooperative pause delivery", () => {
  const COOPERATIVE: ProcessKind[] = ["chat_turn", "subagent", "crew_member"];

  it("latches a suspend onto the pause registry, keyed by externalId", async () => {
    for (const kind of COOPERATIVE) {
      const outcome = await deliverProcessSignal(
        record({ kind, externalId: `ext-${kind}`, signalIntent: suspend }),
        MAIN,
      );
      expect(outcome).toBe("suspended");
      expect(isPauseRequested(`ext-${kind}`)).toBe(true);
    }
    // The cooperative kinds must never reach the side task's own latch.
    expect(sideTaskMock.pause).not.toHaveBeenCalled();
  });

  it("clears the latch on a resume that lands BEFORE the loop parked", async () => {
    // The deadlock this guards: `pause_pending` means the record is still
    // `running` while the suspend is latched, so the record's own state cannot
    // be the acknowledgement. Treating "no intent + running" as nothing-pending
    // would leave the registry latched, and the loop would park at its next
    // checkpoint and never wake.
    setPauseRequested("ext-turn", true);

    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "ext-turn", state: "running" }),
      MAIN,
    );

    expect(outcome).toBe("resumed");
    expect(isPauseRequested("ext-turn")).toBe(false);
  });

  it("clears the latch on a resume that lands after the loop parked", async () => {
    setPauseRequested("ext-turn", true);

    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "ext-turn", state: "suspended" }),
      MAIN,
    );

    expect(outcome).toBe("resumed");
    expect(isPauseRequested("ext-turn")).toBe(false);
  });

  it("reports nothing pending for a cooperative kind with no latch either side", async () => {
    const outcome = await deliverProcessSignal(
      record({ kind: "chat_turn", externalId: "ext-quiet" }),
      MAIN,
    );
    expect(outcome).toBe("nothing-pending");
  });

  it("leaves a stop to win over a pending suspend", async () => {
    // Independent latches: honouring the suspend of a process also asked to
    // stop would park it instead of winding it down.
    const cancelled = registerRunCancellation("ext-both", () => {});
    const outcome = await deliverProcessSignal(
      record({
        kind: "chat_turn",
        externalId: "ext-both",
        signalIntent: { stopRequested: true, suspendRequested: true },
      }),
      MAIN,
    );
    expect(outcome).toBe("stopped");
    expect(isPauseRequested("ext-both")).toBe(false);
    cancelled();
  });

  it("defers suspend and resume for the kinds Rust delivers to itself", async () => {
    // A background shell gets a real SIGSTOP inline in `process_signal`, and a
    // workflow run parks at its own level boundary. Nothing for this side to do.
    for (const kind of ["background_shell", "workflow_run"] as ProcessKind[]) {
      const outcome = await deliverProcessSignal(
        record({ kind, externalId: `ext-${kind}`, signalIntent: suspend }),
        MAIN,
      );
      expect(outcome).toBe("delivered-elsewhere");
      expect(isPauseRequested(`ext-${kind}`)).toBe(false);
    }
  });
});

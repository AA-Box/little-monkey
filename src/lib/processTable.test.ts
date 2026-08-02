/**
 * Two things are asserted here, and the second matters more than the first.
 *
 * 1. A real `runAgentTurn` projects itself onto the unified process table with
 *    the right lifecycle and the right exit.
 * 2. A projection failure never breaks the turn. The process table is an
 *    observability and arbitration surface; a turn that refuses to run because
 *    its bookkeeping row could not be written would be a worse product than an
 *    incomplete listing.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

let scriptedRounds: Array<{ content?: string; toolCalls?: unknown[] }> = [];

vi.mock("./llamaClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./llamaClient")>();
  return {
    ...actual,
    streamChat: async function* streamChat() {
      const round = scriptedRounds.shift() ?? { content: "done" };
      if (round.toolCalls && round.toolCalls.length > 0) {
        for (const toolCall of round.toolCalls) yield { type: "tool_call", toolCall };
        yield { type: "done" };
        return;
      }
      yield { type: "delta", content: round.content ?? "done" };
      yield { type: "done" };
    },
  };
});

import { runAgentTurn, stopTurn } from "./agentLoop";
import {
  admitProcess,
  canResume,
  canSuspend,
  exitStatusFor,
  listProcesses,
  processDisplayState,
  signalProcess,
  type ProcessRecord,
} from "./processTable";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useWorkspaceStore } from "../store/workspaceStore";
import { usePermissionStore } from "../store/permissionStore";
import { useModelStore } from "../store/modelStore";

const NO_DAEMON = {
  installed: false,
  serviceRunning: false,
  heartbeatFresh: false,
  killSwitch: false,
};

interface AdmitCall {
  kind: string;
  externalId: string;
  parentExternalId?: string | null;
  workspace?: string | null;
  profile?: string | null;
}

interface TransitionCall {
  processId: string;
  state: string;
  exitStatus?: string | null;
  exitReason?: string | null;
}

let admits: AdmitCall[] = [];
let transitions: TransitionCall[] = [];

function installBackend(options: { failProjection?: boolean } = {}): void {
  invokeMock.mockImplementation(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "daemon_desktop_status") return NO_DAEMON;
    if (command === "process_admit") {
      if (options.failProjection) throw new Error("ledger is locked");
      const args = payload?.args as AdmitCall;
      admits.push(args);
      return { processId: `p-${args.kind}-${admits.length}`, ...args };
    }
    if (command === "process_transition") {
      if (options.failProjection) throw new Error("ledger is locked");
      transitions.push(payload?.args as TransitionCall);
      return {};
    }
    return undefined;
  });
}

function seedSession(sessionId: string, personaId: string | null = null): void {
  const session: ChatSession = {
    id: sessionId,
    title: "Projection",
    messages: [],
    createdAt: 0,
    updatedAt: 0,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
  };
  useSessionStore.setState({
    sessions: [session],
    activeSessionId: sessionId,
    messages: [],
    runningTurns: {},
  });
}

async function drainPersistence(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 425));
}

describe("a chat turn projects itself onto the process table", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    admits = [];
    transitions = [];
    scriptedRounds = [];
    installBackend();
    useWorkspaceStore.setState({
      roots: [{ id: "r0", path: "/tmp/projection-workspace", label: "ws", is_primary: true }],
    });
    usePermissionStore.setState({ mode: "manual" });
    useModelStore.setState({ activeProvider: "ollama", activeOllamaModel: "test-model" });
  });

  it("admits a chat_turn, marks it running, and exits it succeeded", async () => {
    const sessionId = "projection-ok";
    seedSession(sessionId, "persona-7");
    scriptedRounds = [{ content: "all done" }];

    await runAgentTurn(sessionId, "Say hello.");

    expect(admits).toHaveLength(1);
    expect(admits[0].kind).toBe("chat_turn");
    expect(admits[0].externalId).toBeTruthy();
    expect(admits[0].workspace).toBe("/tmp/projection-workspace");
    expect(admits[0].profile).toBe("persona-7");

    const states = transitions.map((call) => call.state);
    expect(states).toEqual(["running", "exited"]);
    expect(transitions[1].exitStatus).toBe("succeeded");

    await drainPersistence();
  });

  it("records a stopped turn as cancelled rather than failed", async () => {
    const sessionId = "projection-stopped";
    seedSession(sessionId);
    // Stop lands while the first round is in flight.
    scriptedRounds = [{ content: "partial" }];
    const running = runAgentTurn(sessionId, "Take your time.");
    stopTurn(sessionId);
    await running.catch(() => undefined);

    const exit = transitions.find((call) => call.state === "exited");
    expect(exit, "a stopped turn must still be exited").toBeDefined();
    expect(exit?.exitStatus).toBe("cancelled");
    expect(exit?.exitStatus).not.toBe("failed");

    await drainPersistence();
  });

  it("completes the turn even when the projection cannot be written", async () => {
    const sessionId = "projection-broken";
    seedSession(sessionId);
    installBackend({ failProjection: true });
    scriptedRounds = [{ content: "still worked" }];

    await expect(runAgentTurn(sessionId, "Carry on.")).resolves.toBeUndefined();

    const messages =
      useSessionStore.getState().sessions.find((s) => s.id === sessionId)?.messages ?? [];
    expect(messages.some((m) => m.role === "assistant")).toBe(true);
    expect(admits).toHaveLength(0);
    expect(transitions).toHaveLength(0);

    await drainPersistence();
  });
});

describe("the fail-soft client contract", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    admits = [];
    transitions = [];
  });

  it("returns null from admit rather than throwing", async () => {
    invokeMock.mockRejectedValue(new Error("no backend"));
    await expect(admitProcess({ kind: "side_task", externalId: "x" })).resolves.toBeNull();
  });

  it("returns an empty list rather than throwing", async () => {
    invokeMock.mockRejectedValue(new Error("no backend"));
    await expect(listProcesses({ liveOnly: true })).resolves.toEqual([]);
  });
});

describe("exitStatusFor", () => {
  it("treats an abort as cancelled even when an error was also thrown", () => {
    // Aborting a turn usually surfaces as an exception. Recording a user's Stop
    // as a failure would make the listing lie about what happened.
    expect(exitStatusFor({ aborted: true, error: new Error("aborted") }).status).toBe("cancelled");
  });

  it("classifies a thrown error as failed and carries its message", () => {
    const outcome = exitStatusFor({ aborted: false, error: new Error("boom") });
    expect(outcome.status).toBe("failed");
    expect(outcome.reason).toBe("boom");
  });

  it("classifies a clean return as succeeded with no reason", () => {
    expect(exitStatusFor({ aborted: false })).toEqual({ status: "succeeded", reason: null });
  });
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
    signalIntent: { stopRequested: false, suspendRequested: false },
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
    const record = liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true } });
    expect(processDisplayState(record)).toBe("pause_pending");
  });

  it("reports a process that has actually parked as suspended, not pause_pending", () => {
    const record = liveRecord({
      state: "suspended",
      signalIntent: { stopRequested: false, suspendRequested: true },
    });
    expect(processDisplayState(record)).toBe("suspended");
  });

  it("lets a pending stop outrank a pending pause", () => {
    // The two latches are independent — `resume` never clears a stop — so a
    // process on its way out must not read as merely pausing.
    const record = liveRecord({ signalIntent: { stopRequested: true, suspendRequested: true } });
    expect(processDisplayState(record)).toBe("stopping");
  });

  it("reports an exited process as exited whatever is still latched on it", () => {
    const record = liveRecord({
      state: "exited",
      signalIntent: { stopRequested: true, suspendRequested: true },
    });
    expect(processDisplayState(record)).toBe("exited");
  });
});

describe("canSuspend / canResume", () => {
  it("offers suspend only where it would say something new", () => {
    expect(canSuspend(liveRecord())).toBe(true);
    expect(
      canSuspend(liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true } })),
    ).toBe(false);
    expect(canSuspend(liveRecord({ state: "exited" }))).toBe(false);
  });

  it("offers resume for a parked process and for one still on its way there", () => {
    const pending = liveRecord({ signalIntent: { stopRequested: false, suspendRequested: true } });
    const parked = liveRecord({
      state: "suspended",
      signalIntent: { stopRequested: false, suspendRequested: true },
    });
    expect(canResume(pending)).toBe(true);
    expect(canResume(parked)).toBe(true);
    expect(canResume(liveRecord())).toBe(false);
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

/**
 * Round-trip cooperative-pause coverage for the chat-turn loop.
 *
 * Deliberately a separate file from `agentLoop.test.ts`: this suite has to
 * mock `./turnEngine`, but `agentLoop.ts` re-exports the *real*
 * `isToolCallAllowed` from that module and `agentLoop.test.ts` asserts on its
 * actual behaviour. A top-level mock there would quietly replace the function
 * under test in those cases, so the provider-mocking harness lives here
 * instead, where nothing else depends on the real implementation.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
  admitProcess: vi.fn(),
  markProcessRunning: vi.fn(),
  markProcessSuspended: vi.fn(),
  exitProcess: vi.fn(),
  reconcileProcess: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("./turnEngine", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./turnEngine")>();
  return {
    ...actual,
    attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
    executeToolCall: (...args: unknown[]) => mocks.executeToolCall(...args),
  };
});
// The turn must take the in-process loop, not the resident runner — that's
// the path with the pause checkpoints.
vi.mock("./daemonDesktopTurn", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./daemonDesktopTurn")>();
  return { ...actual, daemonDesktopRoute: async () => "fallback" as const };
});
// A null recorder is a supported state (`durable.recorder` is optional
// throughout); this keeps the durable ledger out of a pause test.
vi.mock("./durableRun", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./durableRun")>();
  return { ...actual, beginDurableRun: async () => null };
});
// Only the async IPC-backed calls are stubbed. `exitStatusFor` and the rest
// stay real so the turn's own bookkeeping is exercised, and `pauseRegistry`
// resolves `markProcessSuspended`/`markProcessRunning` to these same spies.
vi.mock("./processTable", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./processTable")>();
  return {
    ...actual,
    admitProcess: (...args: unknown[]) => mocks.admitProcess(...args),
    markProcessRunning: (...args: unknown[]) => mocks.markProcessRunning(...args),
    markProcessSuspended: (...args: unknown[]) => mocks.markProcessSuspended(...args),
    exitProcess: (...args: unknown[]) => mocks.exitProcess(...args),
    reconcileProcess: (...args: unknown[]) => mocks.reconcileProcess(...args),
  };
});

import { runAgentTurn } from "./agentLoop";
import { clearPauseRegistryForTests, isPauseRequested, setPauseRequested } from "./pauseRegistry";
import { deliverProcessSignal } from "./processSignalDelivery";
import type { AttemptResult } from "./turnEngine";
import type { ToolCall } from "./llamaClient";
import type { ProcessRecord } from "./processTable";
import { useModelStore } from "../store/modelStore";
import { usePermissionStore } from "../store/permissionStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useSettingsStore } from "../store/settingsStore";
import { useWorkspaceStore } from "../store/workspaceStore";

const SESSION_ID = "pause-session";
const TURN_ID = "pause-turn";
const PROCESS_ID = "process-pause-turn";

function session(): ChatSession {
  return {
    id: SESSION_ID,
    title: "Pause",
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
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
  };
}

function success(content: string, toolCalls: ToolCall[] = []): AttemptResult {
  return {
    content,
    toolCalls,
    streamError: null,
    contentStarted: content.length > 0 || toolCalls.length > 0,
    usage: { promptTokens: 10, completionTokens: 5, totalTokens: 15 },
  };
}

/** The loop renders assistant text from the streamed deltas, not from
 * `AttemptResult.content`, so a mock has to push through `onDelta` (the 7th
 * parameter) for the transcript to reflect anything. */
function stream(args: unknown[], content: string): AttemptResult {
  (args[6] as ((chunk: string) => void) | undefined)?.(content);
  return success(content);
}

/** The record the daemon/CLI's `process_signal` would emit onto
 * `processes://changed` for this turn — used to drive the pause through the
 * real fan-in rather than poking the registry directly. */
function suspendSignal(suspendRequested: boolean): ProcessRecord {
  return {
    processId: PROCESS_ID,
    parentProcessId: null,
    kind: "chat_turn",
    externalId: TURN_ID,
    state: "running",
    runId: null,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: {},
    signalIntent: { stopRequested: false, suspendRequested },
    signalReason: suspendRequested ? "Paused from the CLI" : null,
    signalRequestedAtMs: suspendRequested ? 1 : null,
    exit: null,
    createdAtMs: 0,
    updatedAtMs: 0,
    startedAtMs: null,
    exitedAtMs: null,
  };
}

/** Drives a record through the real delivery path — the same call `App.tsx`
 * makes from `processes://changed` and from the CLI catch-up sweep. */
function deliver(record: ProcessRecord) {
  return deliverProcessSignal(record, { ownsGlobalKinds: true });
}

function assistantText(): string {
  const messages = useSessionStore.getState().sessions.find((entry) => entry.id === SESSION_ID)?.messages ?? [];
  return messages.filter((message) => message.role === "assistant").map((message) => message.content).join("\n");
}

beforeEach(() => {
  vi.useRealTimers();
  mocks.invoke.mockReset();
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "checkpoint_begin") return "checkpoint-1";
    if (command === "checkpoint_end") return null;
    return [];
  });
  mocks.attemptStream.mockReset();
  mocks.executeToolCall.mockReset();
  mocks.executeToolCall.mockResolvedValue("file contents");
  mocks.admitProcess.mockReset();
  mocks.admitProcess.mockResolvedValue(PROCESS_ID);
  mocks.markProcessRunning.mockReset();
  mocks.markProcessRunning.mockResolvedValue(undefined);
  mocks.markProcessSuspended.mockReset();
  mocks.markProcessSuspended.mockResolvedValue(undefined);
  mocks.exitProcess.mockReset();
  mocks.exitProcess.mockResolvedValue(undefined);
  mocks.reconcileProcess.mockReset();
  mocks.reconcileProcess.mockResolvedValue(undefined);

  useSessionStore.setState({
    sessions: [session()],
    groups: [],
    crews: [],
    activeSessionId: SESSION_ID,
    splitSessionId: null,
    renameRequestId: null,
    messages: [],
    runningTurns: {},
    runningSyntheses: {},
    runningCrews: {},
    runningVerifyLabel: {},
    persistError: null,
  });
  useModelStore.setState({
    activeProvider: "ollama",
    activeOllamaModel: "pause-model:latest",
    ollamaReachable: true,
  } as Partial<ReturnType<typeof useModelStore.getState>> as never);
  useWorkspaceStore.setState({ roots: [] });
  usePermissionStore.setState({ mode: "auto" });
  useSettingsStore.setState({ contextTrimEnabled: false, verifyMaxRounds: 0 });
  clearPauseRegistryForTests();
});

afterEach(async () => {
  clearPauseRegistryForTests();
  // Drain sessionStore's debounced persistence so it cannot leak an IPC call
  // into a later test file sharing this worker.
  await new Promise((resolve) => setTimeout(resolve, 425));
  mocks.invoke.mockReset();
});

describe("chat turn cooperative pause", () => {
  it("holds before the first model call while latched, then runs it once resumed", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => stream(args, "ANSWERED"));

    // Latched the way a `process_signal <id> suspend` from the CLI reaches
    // this window: durable intent -> `processes://changed` -> fan-in.
    await deliver(suspendSignal(true));
    expect(isPauseRequested(TURN_ID)).toBe(true);

    const turn = runAgentTurn(SESSION_ID, "Explain this", [], undefined, TURN_ID);

    // The honest part of `pause_pending`: the turn only reports `suspended`
    // once it has actually reached its safe point, not when the signal landed.
    await vi.waitFor(() => expect(mocks.markProcessSuspended).toHaveBeenCalledWith(PROCESS_ID), {
      timeout: 5000,
    });
    expect(mocks.attemptStream).not.toHaveBeenCalled();

    // Still parked after the event loop has had every chance to proceed.
    await new Promise((resolve) => setTimeout(resolve, 20));
    expect(mocks.attemptStream).not.toHaveBeenCalled();

    await deliver(suspendSignal(false));
    await turn;

    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
    expect(assistantText()).toContain("ANSWERED");
    // `markProcessRunning` fires twice: once at admission, once on resume.
    expect(mocks.markProcessRunning.mock.calls.map(([id]) => id)).toEqual([PROCESS_ID, PROCESS_ID]);
    expect(mocks.exitProcess).toHaveBeenCalledWith(PROCESS_ID, "succeeded", null);
    // Teardown drops the latch so a finished turn can't leak one.
    expect(isPauseRequested(TURN_ID)).toBe(false);
  });

  it("holds at the end-of-round checkpoint instead of opening another model round", async () => {
    const readCall: ToolCall = {
      id: "read-1",
      type: "function",
      function: { name: "read_file", arguments: '{"path":"README.md"}' },
    };
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      // First round asks for a tool; the pause lands while that tool runs,
      // which is exactly the window `pause latency is unbounded` describes.
      if (mocks.attemptStream.mock.calls.length === 1) return success("", [readCall]);
      return stream(args, "SECOND_ROUND");
    });
    mocks.executeToolCall.mockImplementation(async () => {
      setPauseRequested(TURN_ID, true);
      return "file contents";
    });

    const turn = runAgentTurn(SESSION_ID, "Read the readme", [], undefined, TURN_ID);

    await vi.waitFor(() => expect(mocks.markProcessSuspended).toHaveBeenCalledWith(PROCESS_ID), {
      timeout: 5000,
    });
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    // The tool round completed, but the next model round never opened.
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);

    setPauseRequested(TURN_ID, false);
    await turn;

    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
    expect(assistantText()).toContain("SECOND_ROUND");
  });

  it("lets Stop win over a pause instead of leaving the turn parked forever", async () => {
    mocks.attemptStream.mockImplementation(async () => success("NEVER_REACHED"));
    setPauseRequested(TURN_ID, true);

    const controller = new AbortController();
    const turn = runAgentTurn(SESSION_ID, "Explain this", [], controller.signal, TURN_ID);
    await vi.waitFor(() => expect(mocks.markProcessSuspended).toHaveBeenCalledWith(PROCESS_ID), {
      timeout: 5000,
    });

    controller.abort();
    await turn;

    expect(mocks.attemptStream).not.toHaveBeenCalled();
    // Aborting out of a park must not claim the process went back to running.
    expect(mocks.markProcessRunning).toHaveBeenCalledTimes(1);
    expect(mocks.exitProcess).toHaveBeenCalledWith(PROCESS_ID, "cancelled", expect.anything());
  });
});

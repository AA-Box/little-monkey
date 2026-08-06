/**
 * Round-trip cooperative-pause coverage for the Crew loop.
 *
 * Separate from `crewRunner.test.ts` because that suite pins `isTauri()` to
 * `false`, which short-circuits `initializeActorRecorders` — and without
 * recorders there are no durable run ids, so `honourActorPause` has no key to
 * check and pause is unobservable. Here `isTauri()` is true and `./durableRun`
 * is mocked to hand back a deterministic recorder id per actor, which is what
 * the pause latch, the process-table record, and `registerRunCancellation` are
 * all keyed by in production.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  resolveReferences: vi.fn(),
  currentSystemPrompt: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
  admitProcess: vi.fn(),
  markProcessRunning: vi.fn(),
  markProcessSuspended: vi.fn(),
  exitProcess: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "crew-pause-test" }) }));
vi.mock("./agentLoop", () => ({
  MENTION_NOTE_PREFIX: "[Mention]",
  attachedStackPromptInfo: () => [],
  formatSourcesNotice: (value: unknown) => `[Sources] ${JSON.stringify(value)}`,
  resolveReferences: (...args: unknown[]) => mocks.resolveReferences(...args),
  toMessageContent: (text: string) => text,
}));
vi.mock("./systemPrompt", () => ({
  currentSystemPrompt: (...args: unknown[]) => mocks.currentSystemPrompt(...args),
}));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
  executeToolCall: (...args: unknown[]) => mocks.executeToolCall(...args),
  isToolCallAllowed: (call: { function: { name: string } }, tools: Array<{ function: { name: string } }>) =>
    tools.some((tool) => tool.function.name === call.function.name),
}));
// A recorder per actor with a deterministic `runId`, so a test can latch a
// pause on exactly the key production would use.
vi.mock("./durableRun", () => ({
  defaultRunBudgets: () => ({}),
  beginDurableRun: async (options: { actorId: string }) => ({
    runId: `run-${options.actorId}`,
    actorId: options.actorId,
    recordModelOutput: () => {},
    recordStatus: () => {},
    recordUsage: () => {},
    recordToolProposed: async () => {},
    recordToolStarted: () => {},
    recordToolFinished: async () => {},
    complete: async () => {},
    fail: async () => {},
    cancel: async () => {},
    flush: async () => {},
  }),
}));
vi.mock("./runProtocol", () => ({ requestRunCancellation: async () => {} }));
vi.mock("./processTable", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./processTable")>();
  return {
    ...actual,
    admitProcess: (...args: unknown[]) => mocks.admitProcess(...args),
    markProcessRunning: (...args: unknown[]) => mocks.markProcessRunning(...args),
    markProcessSuspended: (...args: unknown[]) => mocks.markProcessSuspended(...args),
    exitProcess: (...args: unknown[]) => mocks.exitProcess(...args),
  };
});

import { cancelCrewRun, startCrew } from "./crewRunner";
import { clearPauseRegistryForTests, isPauseRequested } from "./pauseRegistry";
import { deliverProcessSignal } from "./processSignalDelivery";
import type { AttemptResult } from "./turnEngine";
import type { ToolCall } from "./llamaClient";
import type { ProcessRecord } from "./processTable";
import type { CrewDefinition } from "./crewTypes";
import type { ModelTargetSnapshot } from "./modelTargets";
import { useModelStore, type OllamaModelInfo } from "../store/modelStore";
import { usePromptStore, type PromptEntry } from "../store/promptStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useStackStore } from "../store/stackStore";

function ollamaInfo(name: string): OllamaModelInfo {
  return {
    name,
    size_bytes: 1_000,
    is_cloud: false,
    tool_calling: true,
    vision: false,
    modified_at: "2026-07-13T00:00:00Z",
  };
}

function target(name: string): ModelTargetSnapshot {
  return {
    kind: "ollama",
    key: `ollama:${encodeURIComponent(name)}`,
    label: "Ollama",
    displayName: name,
    baseUrl: "http://127.0.0.1:11434",
    model: name,
    isCloud: false,
    estimatedMemoryBytes: 2_000,
    capabilities: {
      toolCalling: { state: "yes", evidence: "test" },
      vision: { state: "no", evidence: "test" },
    },
    availability: { status: "available", evidence: "test daemon" },
  };
}

const PERSONAS: PromptEntry[] = [
  { id: "persona-a", kind: "persona", name: "Analyst", command: "analyst", content: "Analyze.", createdAt: 1, updatedAt: 1 },
  { id: "persona-b", kind: "persona", name: "Critic", command: "critic", content: "Critique.", createdAt: 1, updatedAt: 1 },
];

function definition(): CrewDefinition {
  const targets = [target("crew-a:latest"), target("crew-b:latest")];
  return {
    version: 1,
    id: "crew",
    name: "Test Crew",
    coordinator: {
      id: "coordinator",
      name: "Coordinator",
      role: "synthesize",
      personaId: "persona-a",
      modelTarget: targets[0],
      contextPolicy: "shared_session",
      toolProfile: "read_only",
    },
    members: [
      {
        id: "member-1",
        name: "Member 1",
        role: "perspective 1",
        personaId: "persona-a",
        modelTarget: targets[0],
        contextPolicy: "prompt_only",
        toolProfile: "read_only",
      },
      {
        id: "member-2",
        name: "Member 2",
        role: "perspective 2",
        personaId: "persona-b",
        modelTarget: targets[1],
        contextPolicy: "shared_session",
        toolProfile: "read_only",
      },
    ],
    createdAt: 1,
    updatedAt: 1,
  };
}

function sourceSession(): ChatSession {
  return {
    id: "source",
    title: "Source",
    messages: [{ role: "user", content: "SHARED_HISTORY" }],
    createdAt: 1,
    updatedAt: 1,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    crewRun: null,
    workspacePath: "/workspace",
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

/** `crewRunner.ts` derives an actor id from the per-actor session id it
 * threads into `attemptStream`'s 6th argument. */
function actorIdFromAttempt(args: unknown[]): string {
  const sessionId = String(args[5]);
  return sessionId.slice(sessionId.lastIndexOf(":") + 1);
}

/** What `process_signal <id> suspend|resume` publishes on
 * `processes://changed` for a crew actor — keyed by its durable run id. */
function actorSignal(actorId: string, suspendRequested: boolean): ProcessRecord {
  return {
    processId: `process-${actorId}`,
    parentProcessId: null,
    kind: "crew_member",
    externalId: `run-${actorId}`,
    state: "running",
    runId: `run-${actorId}`,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: {},
    signalIntent: { stopRequested: false, suspendRequested, killRequested: false },
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

function seed(): void {
  const source = sourceSession();
  useSessionStore.setState({
    sessions: [source],
    groups: [],
    crews: [definition()],
    activeSessionId: source.id,
    splitSessionId: null,
    renameRequestId: null,
    messages: source.messages,
    runningTurns: {},
    runningSyntheses: {},
    runningCrews: {},
    runningVerifyLabel: {},
    persistError: null,
  });
  usePromptStore.setState({ entries: PERSONAS, defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: ["crew-a:latest", "crew-b:latest"].map(ollamaInfo),
    ollamaReachable: true,
    providers: [],
    providerModels: {},
  });
  useStackStore.setState({ stacks: [] });
}

beforeEach(() => {
  vi.useRealTimers();
  mocks.invoke.mockReset();
  mocks.invoke.mockImplementation(async () => []);
  mocks.resolveReferences.mockReset();
  mocks.resolveReferences.mockResolvedValue({ textRefs: [], images: [], unresolved: [] });
  mocks.currentSystemPrompt.mockReset();
  mocks.currentSystemPrompt.mockImplementation((personaId: string | null) => `SYSTEM:${personaId ?? "none"}`);
  mocks.attemptStream.mockReset();
  mocks.executeToolCall.mockReset();
  mocks.executeToolCall.mockResolvedValue("read result");
  mocks.admitProcess.mockReset();
  mocks.admitProcess.mockImplementation(async ({ externalId }: { externalId: string }) => `process-${externalId}`);
  mocks.markProcessRunning.mockReset();
  mocks.markProcessRunning.mockResolvedValue(undefined);
  mocks.markProcessSuspended.mockReset();
  mocks.markProcessSuspended.mockResolvedValue(undefined);
  mocks.exitProcess.mockReset();
  mocks.exitProcess.mockResolvedValue(undefined);
  clearPauseRegistryForTests();
  seed();
});

afterEach(() => {
  clearPauseRegistryForTests();
});

function crewRun(sessionId: string) {
  return useSessionStore.getState().sessions.find((session) => session.id === sessionId)?.crewRun;
}

describe("crew cooperative pause", () => {
  it("holds one latched member before its first model call without stalling the others", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      if (actorId === "coordinator") return success(JSON.stringify({ answer: "COMBINED", mutationPlan: [] }));
      return success(JSON.stringify({ report: `REPORT_${actorId}`, proposedMutations: [] }));
    });

    // Latched before the run starts, on the key `initializeActorRecorders`
    // will mint for this actor — the same durable id `registerRunCancellation`
    // and the process-table record use.
    await deliver(actorSignal("member-1", true));
    expect(isPauseRequested("run-member-1")).toBe(true);

    const handle = await startCrew("source", "Solve this", [], "crew");

    // Member 2 is unaffected — a pause is per-actor, not per-run.
    await vi.waitFor(() => {
      expect(mocks.attemptStream.mock.calls.map(actorIdFromAttempt)).toContain("member-2");
    });
    await vi.waitFor(() => {
      expect(mocks.markProcessSuspended).toHaveBeenCalledWith("process-run-member-1");
    });
    expect(mocks.attemptStream.mock.calls.map(actorIdFromAttempt)).not.toContain("member-1");
    // The coordinator waits on every member, so it can't have started either.
    expect(mocks.attemptStream.mock.calls.map(actorIdFromAttempt)).not.toContain("coordinator");

    await deliver(actorSignal("member-1", false));
    await handle.done;

    expect(mocks.attemptStream.mock.calls.map(actorIdFromAttempt)).toContain("member-1");
    expect(mocks.markProcessRunning).toHaveBeenCalledWith("process-run-member-1");
    const run = crewRun(handle.sessionId);
    expect(run).toMatchObject({ status: "completed", finalAnswer: "COMBINED" });
    expect(run?.members.every((member) => member.status === "completed")).toBe(true);
    // Teardown drops the latch so a re-run can't inherit it.
    expect(isPauseRequested("run-member-1")).toBe(false);
  });

  it("holds after a member's model call, before its envelope is forwarded", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      if (actorId === "member-1") {
        // The pause lands while this call is in flight — the honest case the
        // process table's derived `pause_pending` exists for.
        await deliver(actorSignal("member-1", true));
        return success(JSON.stringify({ report: "REPORT_ONE", proposedMutations: [] }));
      }
      if (actorId === "coordinator") return success(JSON.stringify({ answer: "COMBINED", mutationPlan: [] }));
      return success(JSON.stringify({ report: `REPORT_${actorId}`, proposedMutations: [] }));
    });

    const handle = await startCrew("source", "Solve this", [], "crew");

    await vi.waitFor(() => {
      expect(mocks.markProcessSuspended).toHaveBeenCalledWith("process-run-member-1");
    });
    expect(crewRun(handle.sessionId)?.members[0].status).not.toBe("completed");
    expect(mocks.attemptStream.mock.calls.map(actorIdFromAttempt)).not.toContain("coordinator");

    await deliver(actorSignal("member-1", false));
    await handle.done;

    expect(crewRun(handle.sessionId)).toMatchObject({ status: "completed", finalAnswer: "COMBINED" });
  });

  it("lets a cancel win over a pause instead of leaving the actor parked", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      if (actorId === "coordinator") return success(JSON.stringify({ answer: "COMBINED", mutationPlan: [] }));
      return success(JSON.stringify({ report: `REPORT_${actorId}`, proposedMutations: [] }));
    });
    await deliver(actorSignal("member-1", true));

    const handle = await startCrew("source", "Solve this", [], "crew");
    await vi.waitFor(() => {
      expect(mocks.markProcessSuspended).toHaveBeenCalledWith("process-run-member-1");
    });

    cancelCrewRun(handle.sessionId);
    await handle.done;

    const run = crewRun(handle.sessionId);
    expect(run?.status).toBe("cancelled");
    expect(run?.members[0].status).toBe("cancelled");
    expect(mocks.exitProcess).toHaveBeenCalledWith("process-run-member-1", "cancelled", expect.anything());
    // Admission marks it running once; aborting out of a park must not claim
    // it went back to running a second time.
    const runningCalls = mocks.markProcessRunning.mock.calls.filter(([id]) => id === "process-run-member-1");
    expect(runningCalls).toHaveLength(1);
  });
});

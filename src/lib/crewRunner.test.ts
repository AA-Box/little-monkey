import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  resolveReferences: vi.fn(),
  currentSystemPrompt: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "crew-test" }) }));
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

import {
  cancelCrewRun,
  startCrew,
} from "./crewRunner";
import type { AttemptResult } from "./turnEngine";
import type { ChatMessage, ToolCall } from "./llamaClient";
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

function definition(memberCount = 2): CrewDefinition {
  const targets = [target("crew-a:latest"), target("crew-b:latest"), target("crew-c:latest"), target("crew-d:latest")];
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
    members: Array.from({ length: memberCount }, (_, index) => ({
      id: `member-${index + 1}`,
      name: `Member ${index + 1}`,
      role: `perspective ${index + 1}`,
      personaId: index % 2 === 0 ? "persona-a" : "persona-b",
      modelTarget: targets[index],
      contextPolicy: index === 0 ? "prompt_only" as const : "shared_session" as const,
      toolProfile: "read_only" as const,
    })),
    createdAt: 1,
    updatedAt: 1,
  };
}

function sourceSession(): ChatSession {
  return {
    id: "source",
    title: "Source",
    messages: [{ role: "user", content: "SHARED_HISTORY_SECRET" }],
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

function seed(memberCount = 2): void {
  const source = sourceSession();
  const crew = definition(memberCount);
  useSessionStore.setState({
    sessions: [source],
    groups: [],
    crews: [crew],
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
  const infos = ["crew-a:latest", "crew-b:latest", "crew-c:latest", "crew-d:latest"].map(ollamaInfo);
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: infos,
    ollamaReachable: true,
    providers: [],
    providerModels: {},
  });
  useStackStore.setState({ stacks: [] });
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

function actorIdFromAttempt(args: unknown[]): string {
  const sessionId = String(args[5]);
  return sessionId.slice(sessionId.lastIndexOf(":") + 1);
}

beforeEach(() => {
  vi.useRealTimers();
  mocks.invoke.mockReset();
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "rules_read" || command === "memory_list") return [];
    throw new Error(`Unexpected invoke ${command}`);
  });
  mocks.resolveReferences.mockReset();
  mocks.resolveReferences.mockResolvedValue({ textRefs: [], images: [], unresolved: [] });
  mocks.currentSystemPrompt.mockReset();
  mocks.currentSystemPrompt.mockImplementation((personaId: string | null) => `SYSTEM:${personaId ?? "none"}`);
  mocks.attemptStream.mockReset();
  mocks.executeToolCall.mockReset();
  mocks.executeToolCall.mockResolvedValue("read result");
  seed();
});

describe("Crew execution", () => {
  it("completes an all-Ollama Crew while keeping prompt-only and raw member transcripts isolated", async () => {
    const wirePayloads = new Map<string, ChatMessage[]>();
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      wirePayloads.set(actorId, structuredClone(args[1] as ChatMessage[]));
      const onDelta = args[6] as ((content: string) => void) | undefined;
      if (actorId === "member-1") {
        const raw = JSON.stringify({ report: "REPORT_ONE", proposedMutations: [], privateScratch: "PRIVATE_ONE" });
        onDelta?.(raw);
        return success(raw);
      }
      if (actorId === "member-2") {
        const raw = JSON.stringify({ report: "REPORT_TWO", proposedMutations: [], privateScratch: "PRIVATE_TWO" });
        onDelta?.(raw);
        return success(raw);
      }
      const raw = JSON.stringify({ answer: "COMBINED", mutationPlan: [] });
      onDelta?.(raw);
      return success(raw);
    });

    const handle = await startCrew("source", "Solve this", [], "crew");
    await handle.done;

    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;
    expect(run).toMatchObject({ status: "completed", finalAnswer: "COMBINED", round: 1 });
    expect(run?.members.map((member) => member.modelTarget.key)).toEqual([
      "ollama:crew-a%3Alatest",
      "ollama:crew-b%3Alatest",
    ]);
    expect(JSON.stringify(wirePayloads.get("member-1"))).not.toContain("SHARED_HISTORY_SECRET");
    expect(JSON.stringify(wirePayloads.get("member-2"))).toContain("SHARED_HISTORY_SECRET");
    const coordinatorPayload = JSON.stringify(wirePayloads.get("coordinator"));
    expect(coordinatorPayload).toContain("REPORT_ONE");
    expect(coordinatorPayload).toContain("REPORT_TWO");
    expect(coordinatorPayload).not.toContain("PRIVATE_ONE");
    expect(coordinatorPayload).not.toContain("PRIVATE_TWO");
    expect(run?.members[0].transcript[0]).toMatchObject({ actorId: "member-1", kind: "model" });
    expect(run?.budget).toMatchObject({ modelCalls: 3, totalTokens: 45, limitReason: null });
  });

  it("blocks mutation-shaped model output without execution and isolates one member failure", async () => {
    const writeCall: ToolCall = {
      id: "write-1",
      type: "function",
      function: { name: "write_file", arguments: '{"path":"x","content":"bad"}' },
    };
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      if (actorId === "member-1") return success("", [writeCall]);
      if (actorId === "member-2") return success(JSON.stringify({ report: "SAFE_REPORT", proposedMutations: [] }));
      return success(JSON.stringify({ answer: "USED_SAFE_REPORT", mutationPlan: [] }));
    });

    const handle = await startCrew("source", "Check", [], "crew");
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(mocks.executeToolCall).not.toHaveBeenCalled();
    expect(run?.status).toBe("completed");
    expect(run?.members[0]).toMatchObject({ status: "failed", report: null });
    expect(run?.members[0].toolRequests[0]).toMatchObject({
      actorId: "member-1",
      name: "write_file",
      status: "blocked",
      permission: "not_requested_blocked",
    });
    expect(run?.members[1].status).toBe("completed");
    expect(run?.finalAnswer).toBe("USED_SAFE_REPORT");
  });

  it("allows one read-only tool round, then blocks a model from opening another round", async () => {
    const readCall = (id: string): ToolCall => ({
      id,
      type: "function",
      function: { name: "read_file", arguments: '{"path":"README.md"}' },
    });
    const callsByActor = new Map<string, number>();
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      const n = (callsByActor.get(actorId) ?? 0) + 1;
      callsByActor.set(actorId, n);
      if (actorId === "member-1" && n === 1) return success("", [readCall("read-1")]);
      if (actorId === "member-1") return success("", [readCall("read-again")]);
      if (actorId === "member-2") return success(JSON.stringify({ report: "OTHER", proposedMutations: [] }));
      return success(JSON.stringify({ answer: "FINAL", mutationPlan: [] }));
    });

    const handle = await startCrew("source", "Inspect", [], "crew");
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    expect(run?.members[0].status).toBe("failed");
    expect(run?.members[0].error).toContain("additional tool round");
    expect(run?.members[0].toolRequests).toEqual([
      expect.objectContaining({ id: "read-1", actorId: "member-1", status: "completed" }),
      expect.objectContaining({
        id: "read-again",
        actorId: "member-1",
        status: "blocked",
        permission: "not_requested_blocked",
      }),
    ]);
    expect(callsByActor.get("member-1")).toBe(2);
    expect(run?.status).toBe("completed");
    expect(run?.budget.modelCalls).toBe(4);
  });

  it("uses one bounded repair call for a malformed member envelope without forwarding the malformed response", async () => {
    const callsByActor = new Map<string, number>();
    const coordinatorPayloads: ChatMessage[][] = [];
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      const count = (callsByActor.get(actorId) ?? 0) + 1;
      callsByActor.set(actorId, count);
      if (actorId === "member-1" && count === 1) return success("RAW_PRIVATE_INVALID");
      if (actorId === "member-1") return success(JSON.stringify({ report: "REPAIRED_REPORT", proposedMutations: [] }));
      if (actorId === "member-2") return success(JSON.stringify({ report: "OTHER_REPORT", proposedMutations: [] }));
      coordinatorPayloads.push(structuredClone(args[1] as ChatMessage[]));
      return success(JSON.stringify({ answer: "REPAIR_FINAL", mutationPlan: [] }));
    });

    const handle = await startCrew("source", "Repair", [], "crew");
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(run?.status).toBe("completed");
    expect(run?.members[0]).toMatchObject({ status: "completed", modelCalls: 2, report: "REPAIRED_REPORT" });
    expect(run?.members[0].transcript.some((entry) => entry.kind === "notice" && entry.content.includes("envelope-repair"))).toBe(true);
    expect(JSON.stringify(coordinatorPayloads)).toContain("REPAIRED_REPORT");
    expect(JSON.stringify(coordinatorPayloads)).not.toContain("RAW_PRIVATE_INVALID");
    expect(run?.budget.modelCalls).toBe(4);
  });

  it("cancel-all aborts every outstanding member and never starts the coordinator", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const signal = args[3] as AbortSignal;
      return await new Promise<AttemptResult>((resolve) => {
        signal.addEventListener("abort", () => resolve(success("")), { once: true });
      });
    });

    const handle = await startCrew("source", "Wait", [], "crew");
    cancelCrewRun(handle.sessionId);
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(run?.status).toBe("cancelled");
    expect(run?.members.every((member) => member.status === "cancelled")).toBe(true);
    expect(run?.coordinator.status).toBe("idle");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
  });

  it("enforces the completion-token ceiling in code even when model output ignores its prompt", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const onDelta = args[6] as ((content: string) => void) | undefined;
      const huge = "x".repeat(9_000);
      onDelta?.(huge);
      return success(huge);
    });

    const handle = await startCrew("source", "Overflow", [], "crew");
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(run?.status).toBe("failed");
    expect(run?.members.every((member) => member.status === "failed")).toBe(true);
    expect(run?.coordinator.status).toBe("idle");
    expect(run?.budget.limitReason).toBe("tokens");
  });

  it("never exceeds ten model calls with four tool-using members and a tool-using coordinator", async () => {
    seed(4);
    const counts = new Map<string, number>();
    const readCall = (id: string): ToolCall => ({
      id,
      type: "function",
      function: { name: "read_file", arguments: '{"path":"README.md"}' },
    });
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const actorId = actorIdFromAttempt(args);
      const count = (counts.get(actorId) ?? 0) + 1;
      counts.set(actorId, count);
      if (count === 1) return success("", [readCall(`${actorId}-read`)]);
      if (actorId === "coordinator") return success(JSON.stringify({ answer: "TEN_CALL_FINAL", mutationPlan: [] }));
      return success(JSON.stringify({ report: `REPORT_${actorId}`, proposedMutations: [] }));
    });

    const handle = await startCrew("source", "Bounded", [], "crew");
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(mocks.attemptStream).toHaveBeenCalledTimes(10);
    expect(run?.budget.modelCalls).toBe(10);
    expect(run?.status).toBe("completed");
    expect(run?.finalAnswer).toBe("TEN_CALL_FINAL");
  });
});

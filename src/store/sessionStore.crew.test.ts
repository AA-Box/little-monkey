import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "crew-store-test" }) }));

import {
  DEFAULT_CREW_LIMITS,
  emptyCrewUsage,
  type CrewActorRun,
  type CrewDefinition,
  type CrewRun,
} from "../lib/crewTypes";
import type { ModelTargetSnapshot } from "../lib/modelTargets";
import { hydrateSessions, useSessionStore, type ChatSession } from "./sessionStore";

function target(name: string): ModelTargetSnapshot {
  return {
    kind: "ollama",
    key: `ollama:${encodeURIComponent(name)}`,
    label: "Ollama",
    displayName: name,
    baseUrl: "http://127.0.0.1:11434",
    model: name,
    capabilities: {
      toolCalling: { state: "yes", evidence: "test" },
      vision: { state: "no", evidence: "test" },
    },
    availability: { status: "available", evidence: "test" },
  };
}

function definition(): CrewDefinition {
  return {
    version: 1,
    id: "crew",
    name: "Stored Crew",
    coordinator: {
      id: "coordinator",
      name: "Coordinator",
      role: "synthesize",
      personaId: null,
      modelTarget: target("coord:latest"),
      contextPolicy: "shared_session",
      toolProfile: "read_only",
    },
    members: [1, 2].map((index) => ({
      id: `member-${index}`,
      name: `Member ${index}`,
      role: `role ${index}`,
      personaId: `persona-${index}`,
      modelTarget: target(`member-${index}:latest`),
      contextPolicy: "prompt_only" as const,
      toolProfile: "read_only" as const,
    })),
    createdAt: 1,
    updatedAt: 2,
  };
}

function actor(id: string, kind: "coordinator" | "member", status: CrewActorRun["status"]): CrewActorRun {
  return {
    actorId: id,
    kind,
    name: id,
    role: id,
    persona: kind === "member" ? { id: `persona-${id}`, name: id, content: id } : null,
    modelTarget: target(`${id}:latest`),
    contextPolicy: kind === "coordinator" ? "shared_session" : "prompt_only",
    toolProfile: "read_only",
    systemPrompt: `system ${id}`,
    status,
    startedAt: status === "running" ? 10 : null,
    completedAt: null,
    durationMs: null,
    error: null,
    rawOutput: "",
    report: null,
    transcript: [],
    toolRequests: [],
    permissions: [],
    mutationProposals: [],
    usage: emptyCrewUsage(),
    modelCalls: 0,
    estimatedCostUsd: 0,
  };
}

function run(status: CrewRun["status"] = "running"): CrewRun {
  return {
    version: 1,
    id: "run",
    crewId: "crew",
    crewName: "Stored Crew",
    status,
    createdAt: 5,
    startedAt: 10,
    completedAt: null,
    durationMs: null,
    error: null,
    round: 0,
    limits: { ...DEFAULT_CREW_LIMITS },
    budget: { modelCalls: 2, totalTokens: 30, estimatedCostUsd: 0, limitReason: null },
    input: {
      sourceSessionId: "source",
      prompt: "Frozen prompt",
      storedContent: "Frozen prompt",
      wireContent: "Frozen prompt with refs",
      baseMessages: [{ role: "user", content: "old context" }],
      contextMessages: [],
      unresolvedReferences: [],
      createdAt: 5,
    },
    coordinator: actor("coordinator", "coordinator", "idle"),
    members: [actor("member-1", "member", "running"), actor("member-2", "member", "completed")],
    finalAnswer: "",
    mutationProposals: [],
  };
}

function session(id: string, crewRun: CrewRun | null = null): ChatSession {
  return {
    id,
    title: id,
    messages: [],
    createdAt: 1,
    updatedAt: 1,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    crewRun,
    workspacePath: "/workspace",
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
  };
}

function reset(): void {
  const source = session("source");
  useSessionStore.setState({
    sessions: [source],
    groups: [],
    crews: [],
    activeSessionId: "source",
    splitSessionId: null,
    renameRequestId: null,
    messages: [],
    runningTurns: {},
    runningSyntheses: {},
    runningCrews: {},
    runningVerifyLabel: {},
    persistError: null,
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(null);
  reset();
});

afterEach(async () => {
  await vi.runOnlyPendingTimersAsync();
  vi.useRealTimers();
});

describe("Crew persistence", () => {
  it("restores definitions and converts an in-flight run and actor into honest cold-start failures", async () => {
    const persistedRun = run("running");
    invokeMock.mockImplementationOnce(async () => JSON.stringify({
      sessions: [session("crew-session", persistedRun)],
      activeSessionId: "crew-session",
      groups: [],
      crews: [definition()],
    }));

    await hydrateSessions();

    const state = useSessionStore.getState();
    expect(state.crews).toHaveLength(1);
    expect(state.crews[0].members).toHaveLength(2);
    const restored = state.sessions[0].crewRun;
    expect(restored).toMatchObject({
      status: "failed",
      error: expect.stringContaining("Interrupted"),
      budget: { modelCalls: 2, totalTokens: 30 },
    });
    expect(restored?.members[0]).toMatchObject({
      actorId: "member-1",
      status: "failed",
      error: expect.stringContaining("Interrupted"),
    });
    expect(restored?.members[1].status).toBe("completed");
    expect(restored?.members[1].modelTarget.key).toBe("ollama:member-2%3Alatest");
  });

  it("persists saved Crews inside the same durable sessions payload", async () => {
    useSessionStore.getState().saveCrew(definition());
    await vi.advanceTimersByTimeAsync(500);

    const save = invokeMock.mock.calls.find(([command]) => command === "sessions_save");
    expect(save).toBeTruthy();
    const payload = JSON.parse((save?.[1] as { payload: string }).payload);
    expect(payload.crews).toHaveLength(1);
    expect(payload.crews[0]).toMatchObject({ id: "crew", name: "Stored Crew", version: 1 });
  });

  it("promotes coordinator output and structured inert proposals, never private member transcripts", () => {
    const completed = run("completed");
    completed.members[0].transcript = [{
      id: "private",
      actorId: "member-1",
      at: 20,
      kind: "model",
      content: "PRIVATE_MEMBER_CHAIN",
    }];
    completed.finalAnswer = "SAFE_FINAL";
    completed.mutationProposals = [{
      id: "proposal",
      actorId: "coordinator",
      summary: "Edit a file",
      details: "proposed only",
      sourceActorIds: ["member-1"],
      status: "proposed",
    }];
    const crewSession = session("crew-session", completed);
    useSessionStore.setState({ sessions: [crewSession], activeSessionId: crewSession.id, messages: [] });

    const promotedId = useSessionStore.getState().promoteCrewResult("crew-session");
    const promoted = useSessionStore.getState().sessions.find((candidate) => candidate.id === promotedId);

    expect(promoted?.crewRun).toBeNull();
    expect(JSON.stringify(promoted?.messages)).toContain("SAFE_FINAL");
    const mutationNotice = promoted?.messages.find((message) => message.role === "system")?.content;
    expect(mutationNotice).toContain("executed none");
    expect(mutationNotice).toContain('"summary": "Edit a file"');
    expect(mutationNotice).toContain('"sourceActorIds"');
    expect(JSON.stringify(promoted?.messages)).not.toContain("PRIVATE_MEMBER_CHAIN");
  });
});

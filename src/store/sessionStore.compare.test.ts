import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "compare-test" }) }));

import type { ChatMessage } from "../lib/llamaClient";
import type { ModelTargetSnapshot } from "../lib/modelTargets";
import { hydrateSessions, rehydrateFromFile, useSessionStore, type ChatSession, type SessionGroup } from "./sessionStore";

function providerTarget(model: string): ModelTargetSnapshot {
  return {
    kind: "provider",
    key: `provider:test-provider:${model}`,
    label: "Test Provider",
    displayName: model,
    providerId: "test-provider",
    endpoint: "https://test-provider.invalid/v1",
    model,
    credentialRefId: "keychain:com.littlemonkey.app:test-provider",
    capabilities: {
      toolCalling: { state: "unknown", evidence: "metadata" },
      vision: { state: "unknown", evidence: "metadata" },
    },
    availability: { status: "available", evidence: "Connected in test" },
  };
}

function makeSession(id: string, overrides: Partial<ChatSession> = {}): ChatSession {
  return {
    id,
    title: `session ${id}`,
    messages: [],
    createdAt: 10,
    updatedAt: 10,
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
    ...overrides,
  };
}

function seed(source = makeSession("source"), groups: SessionGroup[] = []): void {
  useSessionStore.setState({
    sessions: [source],
    groups,
    activeSessionId: source.id,
    splitSessionId: null,
    renameRequestId: null,
    messages: source.messages,
    runningTurns: {},
    runningSyntheses: {},
    runningVerifyLabel: {},
    persistError: null,
  });
}

beforeEach(() => {
  vi.useFakeTimers();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  seed();
});

afterEach(async () => {
  await vi.runOnlyPendingTimersAsync();
  vi.useRealTimers();
});

describe("comparison persistence migration", () => {
  it("defaults old sessions and groups, and drops a malformed target snapshot", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "old",
            title: "Old session",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            pinned: false,
            unread: false,
            archived: false,
            groupId: "old-folder",
            workspacePath: null,
            modelTarget: { kind: "provider", key: "incomplete" },
          },
        ],
        activeSessionId: "old",
        groups: [{ id: "old-folder", name: "Old folder" }],
      })
    );

    await hydrateSessions();

    const session = useSessionStore.getState().sessions[0];
    expect(session.modelTarget).toBeNull();
    expect(session.comparisonBranch).toBeNull();
    expect(useSessionStore.getState().groups).toEqual([
      { id: "old-folder", name: "Old folder", kind: "folder", createdAt: 0 },
    ]);
  });

  it("preserves a pre-durable provider target using a non-routable migration marker", async () => {
    const legacy = providerTarget("legacy-model") as ModelTargetSnapshot & {
      endpoint?: string;
      credentialRefId?: string;
    };
    delete legacy.endpoint;
    delete legacy.credentialRefId;
    invokeMock.mockImplementationOnce(async () => JSON.stringify({
      sessions: [makeSession("legacy", { modelTarget: legacy as ModelTargetSnapshot })],
      activeSessionId: "legacy",
      groups: [],
    }));

    await hydrateSessions();

    expect(useSessionStore.getState().sessions[0].modelTarget).toMatchObject({
      kind: "provider",
      endpoint: "https://legacy-target.invalid/v1",
      credentialRefId: "keychain:com.littlemonkey.app:test-provider",
    });
  });

  it("fills every retry-snapshot field for an older comparison group", async () => {
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            id: "branch",
            title: "Old branch",
            messages: [],
            createdAt: 1,
            updatedAt: 1,
            groupId: "compare-old",
          },
        ],
        activeSessionId: "branch",
        groups: [
          {
            id: "compare-old",
            name: "Old comparison",
            kind: "comparison",
            createdAt: 4,
            comparison: { sourceSessionId: "source-old", prompt: "Compare this" },
          },
        ],
      })
    );

    await hydrateSessions();

    expect(useSessionStore.getState().groups[0].comparison).toEqual({
      sourceSessionId: "source-old",
      prompt: "Compare this",
      baseMessageCount: 0,
      storedContent: null,
      wireContent: null,
      unresolvedReferences: [],
      effort: null,
      systemPrompt: null,
      contextMessages: [],
      executionPlan: null,
      synthesis: null,
    });
  });

  it("marks a persisted running branch interrupted and falls back from a missing active id", async () => {
    const target = providerTarget("alpha");
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [
          {
            ...makeSession("branch", {
              modelTarget: target,
              comparisonBranch: {
                comparisonId: "compare",
                index: 0,
                status: "running",
                startedAt: 100,
                completedAt: null,
                durationMs: null,
                error: null,
                usage: null,
              },
            }),
          },
        ],
        activeSessionId: "deleted-session",
        groups: [],
      })
    );

    await hydrateSessions();

    const state = useSessionStore.getState();
    expect(state.activeSessionId).toBe("branch");
    expect(state.sessions[0].comparisonBranch).toMatchObject({
      status: "failed",
      error: expect.stringContaining("Interrupted"),
    });
  });

  it("restores valid execution plans and drops malformed persisted plans", async () => {
    const validPlan = {
      version: 1 as const,
      mode: "local_sequential" as const,
      strategy: "memory_queue" as const,
      localTargetKeys: ["ollama:alpha", "ollama:beta"],
      branches: [
        {
          sessionId: "branch-alpha",
          targetKey: "ollama:alpha",
          mode: "queued" as const,
          queuePosition: 0,
          estimatedResidentBytes: 8,
        },
        {
          sessionId: "branch-beta",
          targetKey: "ollama:beta",
          mode: "queued" as const,
          queuePosition: 1,
          estimatedResidentBytes: 7,
        },
      ],
      estimatedLocalBytes: 15,
      availableMemoryBytes: 16,
      budgetMemoryBytes: 12,
      reason: "memory_pressure" as const,
      residentOllamaModels: ["preexisting:latest"],
      cleanupWarnings: [],
    };
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [makeSession("branch")],
        activeSessionId: "branch",
        groups: [
          {
            id: "valid-plan",
            name: "Valid plan",
            kind: "comparison",
            createdAt: 1,
            comparison: { sourceSessionId: "source", prompt: "compare", executionPlan: validPlan },
          },
          {
            id: "invalid-plan",
            name: "Invalid plan",
            kind: "comparison",
            createdAt: 2,
            comparison: {
              sourceSessionId: "source",
              prompt: "compare",
              executionPlan: { ...validPlan, availableMemoryBytes: -1 },
            },
          },
        ],
      })
    );

    await hydrateSessions();

    const groups = useSessionStore.getState().groups;
    expect(groups.find((group) => group.id === "valid-plan")?.comparison?.executionPlan).toEqual(validPlan);
    expect(groups.find((group) => group.id === "invalid-plan")?.comparison?.executionPlan).toBeNull();
  });

  it("interrupts cold-hydrated running and queued branches and syntheses without losing frozen sources", async () => {
    const branch = (id: string, index: number, status: "running" | "queued" | "completed") =>
      makeSession(id, {
        modelTarget: providerTarget(id),
        comparisonBranch: {
          comparisonId: "compare-recovery",
          index,
          status,
          startedAt: status === "queued" ? null : 100 + index,
          completedAt: status === "completed" ? 180 : null,
          durationMs: status === "completed" ? 78 : null,
          error: null,
          usage: null,
        },
      });
    const sourceBranches = [
      { sessionId: "running", label: "Alpha", targetKey: "provider:test-provider:running", content: "Alpha answer" },
      { sessionId: "queued", label: "Beta", targetKey: "provider:test-provider:queued", content: "Beta answer" },
    ];
    const synthesis = (status: "running" | "queued") => ({
      target: providerTarget(`judge-${status}`),
      sourceBranches,
      status,
      content: status === "running" ? "Partial synthesis" : "",
      startedAt: status === "running" ? 200 : null,
      completedAt: null,
      durationMs: null,
      error: null,
      usage: null,
    });
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [branch("running", 0, "running"), branch("queued", 1, "queued"), branch("done", 2, "completed")],
        activeSessionId: "running",
        groups: [
          {
            id: "compare-recovery",
            name: "Running synthesis",
            kind: "comparison",
            createdAt: 1,
            comparison: { sourceSessionId: "source", prompt: "compare", synthesis: synthesis("running") },
          },
          {
            id: "compare-queued-synthesis",
            name: "Queued synthesis",
            kind: "comparison",
            createdAt: 2,
            comparison: { sourceSessionId: "source", prompt: "compare", synthesis: synthesis("queued") },
          },
        ],
      })
    );

    await hydrateSessions();

    const state = useSessionStore.getState();
    expect(state.sessions.find((session) => session.id === "running")?.comparisonBranch).toMatchObject({
      status: "failed",
      error: expect.stringContaining("Interrupted"),
    });
    expect(state.sessions.find((session) => session.id === "queued")?.comparisonBranch).toMatchObject({
      status: "failed",
      error: expect.stringContaining("Interrupted"),
    });
    expect(state.sessions.find((session) => session.id === "done")?.comparisonBranch).toMatchObject({
      status: "completed",
      error: null,
    });
    for (const groupId of ["compare-recovery", "compare-queued-synthesis"]) {
      expect(state.groups.find((group) => group.id === groupId)?.comparison?.synthesis).toMatchObject({
        status: "failed",
        sourceBranches,
        error: expect.stringContaining("Interrupted"),
      });
    }
    expect(state.groups.find((group) => group.id === "compare-recovery")?.comparison?.synthesis?.content).toBe(
      "Partial synthesis",
    );
  });
});

describe("cross-window comparison merge", () => {
  it("preserves a locally running branch and its group when another window saves", async () => {
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "compare", [providerTarget("alpha"), providerTarget("beta")]);
    const runningId = sessionIds[0];
    useSessionStore.getState().addMessage(runningId, { role: "assistant", content: "local partial stream" });
    useSessionStore.getState().updateComparisonBranch(runningId, { status: "running", startedAt: 10 });
    useSessionStore.getState().markTurnRunning(runningId, true);

    const local = useSessionStore.getState();
    const staleSessions = local.sessions.map((session) =>
      session.id === runningId
        ? {
            ...session,
            messages: [],
            comparisonBranch: session.comparisonBranch
              ? { ...session.comparisonBranch, status: "running" as const }
              : null,
          }
        : session
    );
    const external = makeSession("external", { updatedAt: 99 });
    invokeMock.mockImplementationOnce(async () =>
      JSON.stringify({
        sessions: [...staleSessions, external],
        activeSessionId: "external",
        groups: local.groups,
      })
    );

    await rehydrateFromFile();

    const merged = useSessionStore.getState();
    expect(merged.sessions.find((session) => session.id === runningId)?.messages).toEqual([
      { role: "assistant", content: "local partial stream", at: expect.any(Number) },
    ]);
    expect(merged.sessions.find((session) => session.id === runningId)?.comparisonBranch?.status).toBe("running");
    expect(merged.groups.some((group) => group.id === groupId && group.kind === "comparison")).toBe(true);
    expect(merged.sessions.some((session) => session.id === "external")).toBe(true);
  });
});

describe("createComparison", () => {
  it("creates ordered, independent transcript/config clones with immutable targets", () => {
    const messages: ChatMessage[] = [
      {
        role: "user",
        content: [
          { type: "text", text: "Earlier question" },
          { type: "image_url", image_url: { url: "data:image/png;base64,AAAA" } },
        ],
      },
      { role: "assistant", content: "Earlier answer" },
    ];
    const source = makeSession("source", {
      title: "Existing transcript",
      messages,
      workspacePath: "/workspace",
      personaId: "persona-reviewer",
      attachedStackIds: ["stack-a", "stack-b"],
      docChatMode: true,
      subagentRuns: { "task-1": [{ role: "assistant", content: "child result" }] },
    });
    seed(source);
    const targets = [providerTarget("alpha"), providerTarget("beta")];

    const result = useSessionStore.getState().createComparison("source", "Which answer is best?", targets);
    const state = useSessionStore.getState();
    const branches = result.sessionIds.map((id) => state.sessions.find((session) => session.id === id)!);

    expect(result.sessionIds).toHaveLength(2);
    expect(state.activeSessionId).toBe(result.sessionIds[0]);
    expect(state.groups.find((group) => group.id === result.groupId)).toMatchObject({
      kind: "comparison",
      comparison: {
        sourceSessionId: "source",
        prompt: "Which answer is best?",
        baseMessageCount: 2,
        storedContent: null,
        wireContent: null,
        systemPrompt: null,
        contextMessages: [],
      },
    });

    for (const [index, branch] of branches.entries()) {
      expect(branch.title).toBe(`Which answer is best? · ${targets[index].displayName}`);
      expect(branch.messages).toEqual(source.messages);
      expect(branch.messages).not.toBe(source.messages);
      expect(branch.groupId).toBe(result.groupId);
      expect(branch.workspacePath).toBe(source.workspacePath);
      expect(branch.personaId).toBe(source.personaId);
      expect(branch.attachedStackIds).toEqual(source.attachedStackIds);
      expect(branch.attachedStackIds).not.toBe(source.attachedStackIds);
      expect(branch.docChatMode).toBe(true);
      expect(branch.subagentRuns).toEqual(source.subagentRuns);
      expect(branch.subagentRuns).not.toBe(source.subagentRuns);
      expect(branch.modelTarget).toEqual(targets[index]);
      expect(branch.modelTarget).not.toBe(targets[index]);
      expect(branch.comparisonBranch).toEqual({
        comparisonId: result.groupId,
        index,
        status: "idle",
        startedAt: null,
        completedAt: null,
        durationMs: null,
        error: null,
        usage: null,
      });
    }
    expect(branches[0].messages[0].content).not.toBe(branches[1].messages[0].content);
    expect(branches[0].modelTarget).not.toBe(branches[1].modelTarget);

    const originalLabel = branches[0].modelTarget?.label;
    (targets[0] as { label: string }).label = "mutated caller object";
    expect(branches[0].modelTarget?.label).toBe(originalLabel);
  });

  it("rejects fewer than two, more than four, and duplicate targets", () => {
    const one = [providerTarget("one")];
    const five = ["one", "two", "three", "four", "five"].map(providerTarget);
    const duplicate = providerTarget("same");

    expect(() => useSessionStore.getState().createComparison("source", "prompt", one)).toThrow();
    expect(() => useSessionStore.getState().createComparison("source", "prompt", five)).toThrow();
    expect(() => useSessionStore.getState().createComparison("source", "prompt", [duplicate, duplicate])).toThrow();
    expect(useSessionStore.getState().sessions).toHaveLength(1);
    expect(useSessionStore.getState().groups).toHaveLength(0);
  });
});

describe("comparison lifecycle actions", () => {
  it("stores resolved input and branch execution metadata", () => {
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "@answer.md", [providerTarget("alpha"), providerTarget("beta")]);
    const storedContent: ChatMessage["content"] = "@answer.md";
    const wireContent: ChatMessage["content"] = "Resolved answer contents";
    const contextMessages: ChatMessage[] = [{ role: "system", content: "Retrieved source passage" }];

    useSessionStore.getState().setComparisonInput(groupId, {
      storedContent,
      wireContent,
      unresolvedReferences: ["missing.md"],
      effort: "high",
      systemPrompt: "Frozen system prompt",
      contextMessages,
    });
    useSessionStore.getState().updateComparisonBranch(sessionIds[0], {
      status: "completed",
      startedAt: 100,
      completedAt: 175,
      durationMs: 75,
      error: null,
      usage: { promptTokens: 12, completionTokens: 8, totalTokens: 20 },
    });

    const state = useSessionStore.getState();
    expect(state.groups.find((group) => group.id === groupId)?.comparison).toMatchObject({
      storedContent,
      wireContent,
      unresolvedReferences: ["missing.md"],
      effort: "high",
      systemPrompt: "Frozen system prompt",
      contextMessages,
    });
    expect(state.sessions.find((session) => session.id === sessionIds[0])?.comparisonBranch).toMatchObject({
      status: "completed",
      startedAt: 100,
      completedAt: 175,
      durationMs: 75,
      usage: { promptTokens: 12, completionTokens: 8, totalTokens: 20 },
    });
  });

  it("promotes a branch into an ungrouped normal session with independent state", () => {
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "prompt", [providerTarget("alpha"), providerTarget("beta")]);
    useSessionStore.getState().addMessage(sessionIds[1], { role: "assistant", content: "Beta wins" });
    const branch = useSessionStore.getState().sessions.find((session) => session.id === sessionIds[1])!;

    const promotedId = useSessionStore.getState().promoteComparisonBranch(sessionIds[1]);
    const promoted = useSessionStore.getState().sessions.find((session) => session.id === promotedId)!;

    expect(promotedId).not.toBeNull();
    expect(useSessionStore.getState().activeSessionId).toBe(promotedId);
    expect(promoted.messages).toEqual(branch.messages);
    expect(promoted.messages).not.toBe(branch.messages);
    expect(promoted.modelTarget).toEqual(branch.modelTarget);
    expect(promoted.modelTarget).not.toBe(branch.modelTarget);
    expect(promoted.groupId).toBeNull();
    expect(promoted.comparisonBranch).toBeNull();
    expect(useSessionStore.getState().groups.some((group) => group.id === groupId)).toBe(true);
  });

  it("forks a comparison branch as a standalone normal session", () => {
    const { sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "prompt", [providerTarget("alpha"), providerTarget("beta")]);
    const branch = useSessionStore.getState().sessions.find((session) => session.id === sessionIds[0])!;

    useSessionStore.getState().forkSession(branch.id);
    const fork = useSessionStore.getState().sessions.find(
      (session) => session.id === useSessionStore.getState().activeSessionId
    )!;

    expect(fork.groupId).toBeNull();
    expect(fork.comparisonBranch).toBeNull();
    expect(fork.modelTarget).toEqual(branch.modelTarget);
    expect(fork.modelTarget).not.toBe(branch.modelTarget);
  });

  it("dissolves a comparison below two branches, but retains ordinary folders", () => {
    const folder: SessionGroup = { id: "folder", name: "Folder", kind: "folder", createdAt: 1 };
    seed(makeSession("source", { groupId: "folder" }), [folder]);
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "prompt", [providerTarget("alpha"), providerTarget("beta")]);

    useSessionStore.getState().deleteSession(sessionIds[0]);
    expect(useSessionStore.getState().groups.some((group) => group.id === groupId)).toBe(false);
    expect(useSessionStore.getState().sessions.find((session) => session.id === sessionIds[1])).toMatchObject({
      groupId: null,
      comparisonBranch: null,
    });
    useSessionStore.getState().deleteSession(sessionIds[1]);
    expect(useSessionStore.getState().groups.some((group) => group.id === groupId)).toBe(false);

    useSessionStore.getState().deleteSession("source");
    expect(useSessionStore.getState().groups).toContainEqual(folder);
  });
});

describe("persistence payload", () => {
  it("writes comparison groups, frozen inputs, targets, and branch status through the existing groups payload", async () => {
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "persist me", [providerTarget("alpha"), providerTarget("beta")]);
    useSessionStore.getState().setComparisonInput(groupId, {
      storedContent: "persist me",
      wireContent: "persist me resolved",
      unresolvedReferences: [],
      effort: "medium",
      systemPrompt: "frozen rules",
      contextMessages: [{ role: "system", content: "frozen retrieval" }],
    });
    useSessionStore.getState().updateComparisonBranch(sessionIds[0], {
      status: "failed",
      startedAt: 20,
      completedAt: 50,
      durationMs: 30,
      error: "provider unavailable",
    });

    await vi.advanceTimersByTimeAsync(401);

    const saves = invokeMock.mock.calls.filter((call) => call[0] === "sessions_save");
    const save = saves[saves.length - 1];
    expect(save).toBeDefined();
    const payload = JSON.parse((save[1] as { payload: string }).payload) as {
      sessions: ChatSession[];
      groups: SessionGroup[];
      activeSessionId: string;
    };
    const persistedGroup = payload.groups.find((group) => group.id === groupId)!;
    const persistedBranch = payload.sessions.find((session) => session.id === sessionIds[0])!;

    expect(persistedGroup.kind).toBe("comparison");
    expect(persistedGroup.comparison).toMatchObject({
      sourceSessionId: "source",
      baseMessageCount: 0,
      storedContent: "persist me",
      wireContent: "persist me resolved",
      systemPrompt: "frozen rules",
      contextMessages: [{ role: "system", content: "frozen retrieval" }],
    });
    expect(persistedBranch.modelTarget).toEqual(providerTarget("alpha"));
    expect(persistedBranch.comparisonBranch).toMatchObject({
      comparisonId: groupId,
      index: 0,
      status: "failed",
      durationMs: 30,
      error: "provider unavailable",
    });
    expect(payload.activeSessionId).toBe(sessionIds[0]);
  });

  it("persists an immutable synthesis source snapshot through lifecycle updates", async () => {
    const { groupId, sessionIds } = useSessionStore
      .getState()
      .createComparison("source", "persist synthesis", [providerTarget("alpha"), providerTarget("beta")]);
    const sourceBranches = [
      {
        sessionId: sessionIds[0],
        label: "Alpha",
        targetKey: providerTarget("alpha").key,
        content: "Frozen alpha answer",
      },
      {
        sessionId: sessionIds[1],
        label: "Beta",
        targetKey: providerTarget("beta").key,
        content: "Frozen beta answer",
      },
    ];
    const synthesisTarget = providerTarget("judge");
    useSessionStore.getState().setComparisonSynthesis(groupId, {
      target: synthesisTarget,
      sourceBranches,
      status: "running",
      content: "",
      startedAt: 100,
      completedAt: null,
      durationMs: null,
      error: null,
      usage: null,
    });

    sourceBranches[0].content = "caller mutation";
    (synthesisTarget as unknown as { displayName: string }).displayName = "caller mutation";
    useSessionStore.getState().updateComparisonSynthesis(groupId, {
      status: "completed",
      content: "Alpha is more complete.",
      completedAt: 160,
      durationMs: 60,
      usage: { promptTokens: 20, completionTokens: 6, totalTokens: 26 },
    });

    await vi.advanceTimersByTimeAsync(401);

    const saves = invokeMock.mock.calls.filter((call) => call[0] === "sessions_save");
    const save = saves[saves.length - 1];
    expect(save).toBeDefined();
    const payload = JSON.parse((save[1] as { payload: string }).payload) as { groups: SessionGroup[] };
    const persisted = payload.groups.find((group) => group.id === groupId)?.comparison?.synthesis;
    expect(persisted).toMatchObject({
      target: { displayName: "judge" },
      sourceBranches: [
        { sessionId: sessionIds[0], content: "Frozen alpha answer" },
        { sessionId: sessionIds[1], content: "Frozen beta answer" },
      ],
      status: "completed",
      content: "Alpha is more complete.",
      durationMs: 60,
      usage: { promptTokens: 20, completionTokens: 6, totalTokens: 26 },
    });
  });
});

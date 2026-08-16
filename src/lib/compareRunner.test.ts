import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  tauri: false,
  resolveReferences: vi.fn(),
  currentSystemPrompt: vi.fn(),
  attemptStream: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  // Keep the real session store in memory without starting its debounced
  // Tauri persistence timer. Its comparison actions/state are still used.
  isTauri: () => mocks.tauri,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "compare-runner-test" }) }));

vi.mock("./agentLoop", () => ({
  MENTION_NOTE_PREFIX: "[Mention]",
  attachedStackPromptInfo: () => [],
  formatSourcesNotice: (notice: unknown) => `[Sources] ${JSON.stringify(notice)}`,
  resolveReferences: (...args: unknown[]) => mocks.resolveReferences(...args),
  toMessageContent: (
    text: string,
    images: Array<{ path: string; dataUrl: string }>,
  ): string | Array<{ type: "text"; text: string } | { type: "image_url"; image_url: { url: string } }> => {
    if (images.length === 0) return text;
    return [
      { type: "text", text },
      ...images.map((image) => ({ type: "image_url" as const, image_url: { url: image.dataUrl } })),
    ];
  },
}));

vi.mock("./systemPrompt", () => ({
  currentSystemPrompt: (...args: unknown[]) => mocks.currentSystemPrompt(...args),
}));

vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));

import {
  retryComparisonBranch,
  startComparison,
  startComparisonSynthesis,
  stopComparisonBranch,
} from "./compareRunner";
import type { ChatMessage } from "./llamaClient";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { AttemptResult, ResolvedTarget } from "./turnEngine";
import {
  useModelStore,
  type ModelInfo,
  type OllamaModelInfo,
  type ProviderConfig,
} from "../store/modelStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useStackStore } from "../store/stackStore";
import { useUsageStore, type UsageInfo } from "../store/usageStore";
import { useUsageHistoryStore } from "../store/usageHistoryStore";

const PROVIDER_A: ProviderConfig = {
  id: "provider-a",
  label: "Provider A",
  base_url: "https://provider-a.test/v1",
  is_custom: true,
  has_key: true, is_extension: false
};

const PROVIDER_B: ProviderConfig = {
  id: "provider-b",
  label: "Provider B",
  base_url: "https://provider-b.test/v1",
  is_custom: true,
  has_key: true, is_extension: false
};

function providerTarget(provider: ProviderConfig, model: string): ModelTargetSnapshot {
  return {
    kind: "provider",
    key: `provider:${provider.id}:${model}`,
    label: provider.label,
    displayName: model,
    providerId: provider.id,
    endpoint: provider.base_url,
    model,
    credentialRefId: `keychain:com.littlemonkey.app:${provider.id}`,
    capabilities: {
      toolCalling: { state: "unknown", evidence: "test inventory" },
      vision: { state: "unknown", evidence: "test inventory" },
    },
    availability: { status: "available", evidence: "test provider is configured" },
  };
}

const TARGET_A = providerTarget(PROVIDER_A, "alpha");
const TARGET_B = providerTarget(PROVIDER_B, "beta");

const LOCAL_MODEL: ModelInfo = {
  id: "local-alpha",
  name: "Local Alpha",
  repo: "test/local-alpha",
  file: "local-alpha.gguf",
  size_gb: 1,
  tool_calling: false,
  installed: true,
  path: "/models/local-alpha.gguf",
  is_external: false,
  kind: "chat",
};

const LOCAL_TARGET: ModelTargetSnapshot = {
  kind: "local",
  key: "local:local-alpha",
  label: "Local",
  displayName: LOCAL_MODEL.name,
  modelId: LOCAL_MODEL.id,
  modelPath: LOCAL_MODEL.path!,
  capabilities: {
    toolCalling: { state: "no", evidence: "test inventory" },
    vision: { state: "unknown", evidence: "test inventory" },
  },
  availability: { status: "available", evidence: "test llama runtime is ready" },
};

function ollamaModel(name: string): OllamaModelInfo {
  return {
    name,
    size_bytes: 6,
    is_cloud: false,
    tool_calling: false,
    vision: false,
    modified_at: "2026-07-13T00:00:00Z",
  };
}

function ollamaTarget(name: string, estimatedMemoryBytes = 8): ModelTargetSnapshot {
  return {
    kind: "ollama",
    key: `ollama:${encodeURIComponent(name)}`,
    label: "Ollama",
    displayName: name,
    baseUrl: "http://127.0.0.1:11434",
    model: name,
    isCloud: false,
    estimatedMemoryBytes,
    capabilities: {
      toolCalling: { state: "no", evidence: "test inventory" },
      vision: { state: "no", evidence: "test inventory" },
    },
    availability: { status: "available", evidence: "test Ollama daemon is reachable" },
  };
}

const OLLAMA_A = ollamaTarget("local-a:latest");
const OLLAMA_B = ollamaTarget("local-b:latest");

interface ReferenceResolution {
  textRefs: Array<{ path: string; isDir: boolean; content: string }>;
  images: Array<{ path: string; dataUrl: string }>;
  unresolved: string[];
}

function emptyReferences(): ReferenceResolution {
  return { textRefs: [], images: [], unresolved: [] };
}

function makeSession(messages: ChatMessage[] = []): ChatSession {
  return {
    id: "source",
    title: "Source conversation",
    messages,
    createdAt: 1,
    updatedAt: 1,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: "/workspace",
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
  };
}

function seedSession(messages: ChatMessage[] = []): void {
  const source = makeSession(messages);
  useSessionStore.setState({
    sessions: [source],
    groups: [],
    messages: source.messages,
    activeSessionId: source.id,
    splitSessionId: null,
    renameRequestId: null,
    runningTurns: {},
    runningVerifyLabel: {},
    persistError: null,
  });
}

function successfulResult(content = "done", usage?: UsageInfo): AttemptResult {
  return {
    content,
    toolCalls: [],
    streamError: null,
    contentStarted: content.length > 0,
    ...(usage ? { usage } : {}),
  };
}

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (reason?: unknown) => void } {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((resolvePromise, rejectPromise) => {
    resolve = resolvePromise;
    reject = rejectPromise;
  });
  return { promise, resolve, reject };
}

function branch(sessionId: string): ChatSession {
  const session = useSessionStore.getState().sessions.find((candidate) => candidate.id === sessionId);
  if (!session) throw new Error(`Missing branch ${sessionId}`);
  return session;
}

function lastMessage(sessionId: string): ChatMessage | undefined {
  const messages = branch(sessionId).messages;
  return messages[messages.length - 1];
}

beforeEach(() => {
  mocks.tauri = false;
  mocks.invoke.mockReset();
  mocks.invoke.mockImplementation(async (command: string) => {
    if (command === "rules_read" || command === "memory_list") return [];
    if (command === "sessions_save") return null;
    throw new Error(`Unexpected invoke: ${command}`);
  });
  mocks.resolveReferences.mockReset();
  mocks.resolveReferences.mockResolvedValue({ textRefs: [], images: [], unresolved: [] });
  mocks.currentSystemPrompt.mockReset();
  mocks.currentSystemPrompt.mockReturnValue("Frozen system prompt");
  mocks.attemptStream.mockReset();
  mocks.attemptStream.mockResolvedValue(successfulResult());

  seedSession();
  useStackStore.setState({ stacks: [] });
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: [],
    ollamaReachable: false,
    providers: [PROVIDER_A, PROVIDER_B],
    providerModels: {
      [PROVIDER_A.id]: [{ id: "alpha" }],
      [PROVIDER_B.id]: [{ id: "beta" }],
    },
    activeProvider: "provider",
    activeProviderId: PROVIDER_A.id,
    activeProviderModel: "alpha",
    effortByTarget: {},
  });
  useUsageStore.setState({ usageBySession: {}, contextLimit: null });
  useUsageHistoryStore.setState({
    totalPromptTokens: 0,
    totalCompletionTokens: 0,
    totalTokens: 0,
    peakTurnTokens: 0,
    dailyTotals: {},
    byModel: {},
    totalTurns: 0,
    longestTurnMs: 0,
    toolCallsMade: 0,
    subagentTasksRun: 0,
    verifyRuns: 0,
  });
});

describe("startComparison", () => {
  it("fans out two concurrent, explicit no-tools calls from one identical resolved-input snapshot", async () => {
    const pendingA = deferred<AttemptResult>();
    const pendingB = deferred<AttemptResult>();
    mocks.resolveReferences.mockResolvedValue({
      textRefs: [{ path: "facts.md", isDir: false, content: "frozen facts" }],
      images: [],
      unresolved: [],
    });
    mocks.attemptStream.mockImplementation((target: ResolvedTarget) =>
      target.kind === "provider" && target.providerId === PROVIDER_A.id ? pendingA.promise : pendingB.promise,
    );
    const attachments = [{ path: "facts.md", isDir: false }];

    const handle = await startComparison("source", "Compare @facts.md", attachments, [TARGET_A, TARGET_B]);
    await vi.waitFor(() => expect(mocks.attemptStream).toHaveBeenCalledTimes(2));

    // Neither branch had to finish before the sibling request was started.
    expect(useSessionStore.getState().runningTurns).toEqual({
      [handle.sessionIds[0]]: true,
      [handle.sessionIds[1]]: true,
    });
    expect(mocks.resolveReferences).toHaveBeenCalledTimes(1);
    expect(mocks.resolveReferences).toHaveBeenCalledWith("Compare @facts.md", attachments);

    const [callA, callB] = mocks.attemptStream.mock.calls;
    expect(callA[0]).toEqual({ kind: "provider", providerId: PROVIDER_A.id, model: "alpha" });
    expect(callB[0]).toEqual({ kind: "provider", providerId: PROVIDER_B.id, model: "beta" });
    expect(callA[2]).toEqual([]);
    expect(callB[2]).toEqual([]);
    expect(callA[7]).toBe(false);
    expect(callB[7]).toBe(false);
    expect(callA[1]).toEqual(callB[1]);
    expect(callA[1]).not.toBe(callB[1]);

    pendingA.resolve(successfulResult("alpha answer"));
    pendingB.resolve(successfulResult("beta answer"));
    await handle.done;
  });

  it("keeps a failed branch, a successful sibling, and their usage/status records isolated", async () => {
    const usageA = { promptTokens: 11, completionTokens: 3, totalTokens: 14 };
    const usageB = { promptTokens: 17, completionTokens: 5, totalTokens: 22 };
    mocks.attemptStream.mockImplementation(
      async (target: ResolvedTarget, _history: ChatMessage[], _tools: unknown[], _signal: AbortSignal, _effort: string, _sessionId: string, onDelta: (content: string) => void) => {
        if (target.kind === "provider" && target.providerId === PROVIDER_A.id) {
          onDelta("partial alpha");
          return {
            content: "partial alpha",
            toolCalls: [],
            streamError: "provider A disconnected",
            contentStarted: true,
            usage: usageA,
          } satisfies AttemptResult;
        }
        onDelta("complete beta");
        return successfulResult("complete beta", usageB);
      },
    );

    const handle = await startComparison("source", "Which is stronger?", [], [TARGET_A, TARGET_B]);
    const settled = await handle.done;

    // runBranch persists a branch failure instead of rejecting/cancelling the
    // aggregate; the sibling still reaches its own successful terminal state.
    expect(settled.map((result) => result.status)).toEqual(["fulfilled", "fulfilled"]);
    expect(branch(handle.sessionIds[0]).comparisonBranch).toMatchObject({
      status: "failed",
      error: "provider A disconnected",
      usage: usageA,
    });
    expect(branch(handle.sessionIds[1]).comparisonBranch).toMatchObject({
      status: "completed",
      error: null,
      usage: usageB,
    });
    expect(useUsageStore.getState().usageBySession).toEqual({
      [handle.sessionIds[0]]: usageA,
      [handle.sessionIds[1]]: usageB,
    });
    expect(lastMessage(handle.sessionIds[0])?.content).toContain("provider A disconnected");
    expect(lastMessage(handle.sessionIds[1])?.content).toBe("complete beta");
  });

  it("stops one branch without aborting or changing its running sibling", async () => {
    const controls = new Map<
      string,
      {
        signal: AbortSignal;
        onDelta: (content: string) => void;
        result: ReturnType<typeof deferred<AttemptResult>>;
      }
    >();
    mocks.attemptStream.mockImplementation(
      (target: ResolvedTarget, _history: ChatMessage[], _tools: unknown[], signal: AbortSignal, _effort: string, _sessionId: string, onDelta: (content: string) => void) => {
        if (target.kind !== "provider") throw new Error("Expected provider target");
        const result = deferred<AttemptResult>();
        controls.set(target.providerId, { signal, onDelta, result });
        signal.addEventListener("abort", () => result.resolve(successfulResult("")), { once: true });
        return result.promise;
      },
    );

    const handle = await startComparison("source", "Run both", [], [TARGET_A, TARGET_B]);
    await vi.waitFor(() => expect(controls.size).toBe(2));

    stopComparisonBranch(handle.sessionIds[0]);
    await vi.waitFor(() => expect(branch(handle.sessionIds[0]).comparisonBranch?.status).toBe("cancelled"));

    expect(controls.get(PROVIDER_A.id)?.signal.aborted).toBe(true);
    expect(controls.get(PROVIDER_B.id)?.signal.aborted).toBe(false);
    expect(branch(handle.sessionIds[1]).comparisonBranch?.status).toBe("running");
    expect(useSessionStore.getState().runningTurns[handle.sessionIds[1]]).toBe(true);

    const sibling = controls.get(PROVIDER_B.id)!;
    sibling.onDelta("beta survived");
    sibling.result.resolve(successfulResult("beta survived"));
    await handle.done;

    expect(branch(handle.sessionIds[0]).comparisonBranch?.status).toBe("cancelled");
    expect(branch(handle.sessionIds[1]).comparisonBranch?.status).toBe("completed");
    expect(lastMessage(handle.sessionIds[1])?.content).toBe("beta survived");
  });

  it("rejects a known vision-incompatible target when an image exists in the base history", async () => {
    const targetWithoutVision: ModelTargetSnapshot = {
      ...TARGET_A,
      capabilities: {
        ...TARGET_A.capabilities,
        vision: { state: "no", evidence: "provider metadata says text-only" },
      },
    };
    seedSession([
      {
        role: "user",
        content: [
          { type: "text", text: "What is in this image?" },
          { type: "image_url", image_url: { url: "data:image/png;base64,AAAA" } },
        ],
      },
      { role: "assistant", content: "A historical answer" },
    ]);

    await expect(
      startComparison("source", "Reconsider the earlier answer", [], [targetWithoutVision, TARGET_B]),
    ).rejects.toThrow("image history");

    expect(useSessionStore.getState().sessions.map((session) => session.id)).toEqual(["source"]);
    expect(useSessionStore.getState().groups).toEqual([]);
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("catches a provider model disappearing during delayed reference resolution before creating sessions", async () => {
    const pendingReferences = deferred<ReferenceResolution>();
    mocks.resolveReferences.mockReturnValue(pendingReferences.promise);

    const preparing = startComparison("source", "Compare after reading", [], [TARGET_A, TARGET_B]);
    await vi.waitFor(() => expect(mocks.resolveReferences).toHaveBeenCalledTimes(1));

    // The first preflight already passed. Simulate provider B's model list
    // disappearing while file references are still being prepared.
    useModelStore.setState({
      providers: [PROVIDER_A, PROVIDER_B],
      providerModels: {
        [PROVIDER_A.id]: [{ id: "alpha" }],
        [PROVIDER_B.id]: [],
      },
    });
    pendingReferences.resolve(emptyReferences());

    await expect(preparing).rejects.toThrow("no longer available");
    expect(useSessionStore.getState().sessions.map((session) => session.id)).toEqual(["source"]);
    expect(useSessionStore.getState().groups).toEqual([]);
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("catches the loaded local model disappearing during delayed reference resolution before creating sessions", async () => {
    useModelStore.setState({
      installed: [LOCAL_MODEL],
      active: LOCAL_MODEL,
      llamaStatus: "ready",
    });
    const pendingReferences = deferred<ReferenceResolution>();
    mocks.resolveReferences.mockReturnValue(pendingReferences.promise);

    const preparing = startComparison("source", "Compare local and cloud", [], [LOCAL_TARGET, TARGET_A]);
    await vi.waitFor(() => expect(mocks.resolveReferences).toHaveBeenCalledTimes(1));

    // Stop/unload the managed runtime after the first preflight but before
    // the second inventory check.
    useModelStore.setState({ installed: [], active: null, llamaStatus: "stopped" });
    pendingReferences.resolve(emptyReferences());

    await expect(preparing).rejects.toThrow("no longer available");
    expect(useSessionStore.getState().sessions.map((session) => session.id)).toEqual(["source"]);
    expect(useSessionStore.getState().groups).toEqual([]);
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("detaches caller-owned targets before awaits so later mutation cannot change persistence or routing", async () => {
    const pendingReferences = deferred<ReferenceResolution>();
    mocks.resolveReferences.mockReturnValue(pendingReferences.promise);
    const callerTargets = structuredClone([TARGET_A, TARGET_B]);

    const preparing = startComparison("source", "Keep target identity frozen", [], callerTargets);
    await vi.waitFor(() => expect(mocks.resolveReferences).toHaveBeenCalledTimes(1));

    Object.assign(callerTargets[0], {
      key: "provider:mutated-provider:mutated-model",
      label: "Mutated Provider",
      displayName: "mutated-model",
      providerId: "mutated-provider",
      model: "mutated-model",
    });
    (callerTargets[0].capabilities.vision as { state: string; evidence: string }).state = "no";
    pendingReferences.resolve(emptyReferences());

    const handle = await preparing;
    await handle.done;

    expect(branch(handle.sessionIds[0]).modelTarget).toEqual(TARGET_A);
    expect(branch(handle.sessionIds[0]).modelTarget).not.toBe(callerTargets[0]);
    expect(mocks.attemptStream.mock.calls[0][0]).toEqual({
      kind: "provider",
      providerId: PROVIDER_A.id,
      model: "alpha",
    });
    expect(mocks.attemptStream.mock.calls[1][0]).toEqual({
      kind: "provider",
      providerId: PROVIDER_B.id,
      model: "beta",
    });
  });

  it("sequences memory-pressured Ollama branches while remote branches remain concurrent", async () => {
    mocks.tauri = true;
    mocks.invoke.mockImplementation(async (command: string) => {
      if (command === "rules_read" || command === "memory_list") return [];
      if (command === "sessions_save") return null;
      if (command === "system_memory_info") return { totalBytes: 32, availableBytes: 10 };
      if (command === "ollama_list_running_models") {
        return [{ name: "local-a:latest" }, { name: "local-b:latest" }];
      }
      throw new Error(`Unexpected invoke: ${command}`);
    });
    useModelStore.setState({
      ollamaModels: [ollamaModel("local-a:latest"), ollamaModel("local-b:latest")],
      ollamaReachable: true,
    });
    const pendingA = deferred<AttemptResult>();
    const pendingB = deferred<AttemptResult>();
    const pendingRemote = deferred<AttemptResult>();
    mocks.attemptStream.mockImplementation((target: ResolvedTarget) => {
      if (target.kind === "ollama" && target.model === "local-a:latest") return pendingA.promise;
      if (target.kind === "ollama" && target.model === "local-b:latest") return pendingB.promise;
      return pendingRemote.promise;
    });

    const handle = await startComparison(
      "source",
      "Keep remote work moving",
      [],
      [OLLAMA_A, TARGET_A, OLLAMA_B],
    );
    await vi.waitFor(() => expect(mocks.attemptStream).toHaveBeenCalledTimes(2));

    expect(mocks.attemptStream.mock.calls.map((call) => call[0])).toEqual(
      expect.arrayContaining([
        { kind: "ollama", baseUrl: "http://127.0.0.1:11434", model: "local-a:latest" },
        { kind: "provider", providerId: PROVIDER_A.id, model: "alpha" },
      ]),
    );
    expect(
      mocks.attemptStream.mock.calls.some(
        (call) => (call[0] as ResolvedTarget).kind === "ollama" && (call[0] as { model?: string }).model === "local-b:latest",
      ),
    ).toBe(false);
    expect(branch(handle.sessionIds[0]).comparisonBranch?.status).toBe("running");
    expect(branch(handle.sessionIds[1]).comparisonBranch?.status).toBe("running");
    expect(branch(handle.sessionIds[2]).comparisonBranch?.status).toBe("queued");
    expect(
      useSessionStore.getState().groups.find((group) => group.id === handle.groupId)?.comparison?.executionPlan,
    ).toMatchObject({
      version: 1,
      mode: "local_sequential",
      strategy: "memory_queue",
      reason: "memory_pressure",
      branches: [
        { sessionId: handle.sessionIds[0], mode: "queued", queuePosition: 0 },
        { sessionId: handle.sessionIds[1], mode: "concurrent", queuePosition: null },
        { sessionId: handle.sessionIds[2], mode: "queued", queuePosition: 1 },
      ],
    });

    pendingA.resolve(successfulResult("local A"));
    await vi.waitFor(() => expect(mocks.attemptStream).toHaveBeenCalledTimes(3));
    expect(branch(handle.sessionIds[1]).comparisonBranch?.status).toBe("running");
    expect(branch(handle.sessionIds[2]).comparisonBranch?.status).toBe("running");

    pendingB.resolve(successfulResult("local B"));
    pendingRemote.resolve(successfulResult("remote"));
    await handle.done;
  });

  it("unloads only queued Ollama models absent from the exact pre-run residency snapshot", async () => {
    mocks.tauri = true;
    mocks.invoke.mockImplementation(async (command: string) => {
      if (command === "rules_read" || command === "memory_list") return [];
      if (command === "sessions_save") return null;
      if (command === "system_memory_info") return { totalBytes: 32, availableBytes: 10 };
      if (command === "ollama_list_running_models") return [{ name: "local-a:latest" }];
      if (command === "ollama_unload_model") return null;
      throw new Error(`Unexpected invoke: ${command}`);
    });
    useModelStore.setState({
      ollamaModels: [ollamaModel("local-a:latest"), ollamaModel("local-b:latest")],
      ollamaReachable: true,
    });
    mocks.attemptStream.mockImplementation(
      async (
        target: ResolvedTarget,
        _history: ChatMessage[],
        _tools: unknown[],
        _signal: AbortSignal,
        _effort: string,
        _sessionId: string,
        onDelta: (content: string) => void,
      ) => {
        const content =
          target.kind === "provider"
            ? "remote"
            : target.kind === "ollama"
              ? target.model
              : target.modelLabel ?? "local";
        onDelta(content);
        return successfulResult(content);
      },
    );

    const handle = await startComparison(
      "source",
      "Clean up only owned residency",
      [],
      [OLLAMA_A, OLLAMA_B, TARGET_A],
    );
    await handle.done;

    const unloadCalls = mocks.invoke.mock.calls.filter((call) => call[0] === "ollama_unload_model");
    expect(unloadCalls).toEqual([["ollama_unload_model", { model: "local-b:latest" }]]);
    expect(
      useSessionStore.getState().groups.find((group) => group.id === handle.groupId)?.comparison?.executionPlan,
    ).toMatchObject({ residentOllamaModels: ["local-a:latest"], cleanupWarnings: [] });
  });
});

describe("comparison synthesis", () => {
  it("uses no tools and an immutable snapshot of completed branch responses", async () => {
    mocks.attemptStream.mockImplementation(
      async (
        target: ResolvedTarget,
        _history: ChatMessage[],
        _tools: unknown[],
        _signal: AbortSignal,
        _effort: string,
        _sessionId: string,
        onDelta: (content: string) => void,
      ) => {
        if (target.kind !== "provider") throw new Error("Expected provider target");
        const content = target.providerId === PROVIDER_A.id ? "Frozen alpha response" : "Frozen beta response";
        onDelta(content);
        return successfulResult(content);
      },
    );
    const comparison = await startComparison("source", "Synthesize these", [], [TARGET_A, TARGET_B]);
    await comparison.done;

    const pendingSynthesis = deferred<AttemptResult>();
    mocks.attemptStream.mockReset();
    mocks.attemptStream.mockReturnValue(pendingSynthesis.promise);
    const synthesis = startComparisonSynthesis(comparison.groupId, TARGET_A);

    useSessionStore.getState().replaceMessages(comparison.sessionIds[0], [
      { role: "assistant", content: "Mutated alpha response" },
    ]);
    useSessionStore.getState().replaceMessages(comparison.sessionIds[1], [
      { role: "assistant", content: "Mutated beta response" },
    ]);
    await vi.waitFor(() => expect(mocks.attemptStream).toHaveBeenCalledTimes(1));

    const synthesisCall = mocks.attemptStream.mock.calls[0];
    const synthesisWire = JSON.stringify(synthesisCall[1]);
    expect(synthesisCall[2]).toEqual([]);
    expect(synthesisCall[7]).toBe(false);
    expect(synthesisWire).toContain("Frozen alpha response");
    expect(synthesisWire).toContain("Frozen beta response");
    expect(synthesisWire).not.toContain("Mutated alpha response");
    expect(synthesisWire).not.toContain("Mutated beta response");
    expect(
      useSessionStore.getState().groups.find((group) => group.id === comparison.groupId)?.comparison?.synthesis
        ?.sourceBranches,
    ).toMatchObject([
      { sessionId: comparison.sessionIds[0], content: "Frozen alpha response" },
      { sessionId: comparison.sessionIds[1], content: "Frozen beta response" },
    ]);

    pendingSynthesis.resolve(successfulResult("Combined answer"));
    await synthesis.done;
    expect(
      useSessionStore.getState().groups.find((group) => group.id === comparison.groupId)?.comparison?.synthesis,
    ).toMatchObject({ status: "completed", content: "Combined answer" });
  });
});

describe("retryComparisonBranch", () => {
  it("reuses persisted system/wire/context input and target without rereading references or global selection", async () => {
    const baseMessages: ChatMessage[] = [
      { role: "user", content: "Earlier question" },
      { role: "assistant", content: "Earlier answer" },
    ];
    seedSession(baseMessages);
    mocks.resolveReferences.mockResolvedValue({
      textRefs: [{ path: "facts.md", isDir: false, content: "ORIGINAL FILE CONTENT" }],
      images: [],
      unresolved: ["missing.md"],
    });
    mocks.currentSystemPrompt.mockReturnValue("ORIGINAL SYSTEM PROMPT");
    mocks.attemptStream.mockImplementation(
      async (_target: ResolvedTarget, _history: ChatMessage[], _tools: unknown[], _signal: AbortSignal, _effort: string, _sessionId: string, onDelta: (content: string) => void) => {
        onDelta("initial answer");
        return successfulResult("initial answer");
      },
    );

    // Effort is frozen per-model onto the target snapshot itself now (see
    // `modelTargets.ts`'s `providerTarget`) — bake one on here so this test
    // can assert a retry keeps using it even after the live per-model
    // selection changes below.
    const targetAWithEffort: ModelTargetSnapshot = { ...TARGET_A, effort: "high" };
    const handle = await startComparison("source", "Use @facts.md and @missing.md", [], [targetAWithEffort, TARGET_B]);
    await handle.done;
    const metadata = useSessionStore.getState().groups.find((group) => group.id === handle.groupId)?.comparison;
    expect(metadata?.systemPrompt).toContain("ORIGINAL SYSTEM PROMPT");
    expect(metadata?.wireContent).toContain("ORIGINAL FILE CONTENT");
    expect(metadata?.contextMessages).toEqual([
      {
        role: "system",
        content: expect.stringContaining("@missing.md"),
      },
    ]);

    // Everything that would affect a freshly built turn changes after the
    // first fan-out. A retry must ignore it and use the frozen group/session
    // snapshot, including the original effort level.
    mocks.resolveReferences.mockClear();
    mocks.resolveReferences.mockRejectedValue(new Error("changed file must not be read"));
    mocks.currentSystemPrompt.mockClear();
    mocks.currentSystemPrompt.mockReturnValue("MUTATED SYSTEM PROMPT");
    mocks.attemptStream.mockReset();
    mocks.attemptStream.mockImplementation(
      async (_target: ResolvedTarget, _history: ChatMessage[], _tools: unknown[], _signal: AbortSignal, _effort: string, _sessionId: string, onDelta: (content: string) => void) => {
        onDelta("retried answer");
        return successfulResult("retried answer");
      },
    );
    useModelStore.setState({
      activeProvider: "provider",
      activeProviderId: PROVIDER_B.id,
      activeProviderModel: "beta",
    });
    // Live per-model effort changes after the freeze must not leak into a retry.
    useModelStore.getState().setEffortForTarget(targetAWithEffort.key, "low");

    await retryComparisonBranch(handle.sessionIds[0]);

    expect(mocks.resolveReferences).not.toHaveBeenCalled();
    expect(mocks.currentSystemPrompt).not.toHaveBeenCalled();
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
    const retryCall = mocks.attemptStream.mock.calls[0];
    expect(retryCall[0]).toEqual({ kind: "provider", providerId: PROVIDER_A.id, model: "alpha" });
    expect(retryCall[1]).toEqual([
      { role: "system", content: metadata!.systemPrompt },
      ...baseMessages,
      { role: "user", content: metadata!.wireContent },
      ...metadata!.contextMessages,
    ]);
    expect(retryCall[2]).toEqual([]);
    expect(retryCall[4]).toBe("high");
    expect(retryCall[7]).toBe(false);
    expect(branch(handle.sessionIds[0]).comparisonBranch?.status).toBe("completed");
    expect(lastMessage(handle.sessionIds[0])?.content).toBe("retried answer");
  });

  it("rejects an incomplete transcript when baseMessageCount exceeds it without mutating the branch", async () => {
    const baseMessages: ChatMessage[] = [
      { role: "user", content: "First historical message" },
      { role: "assistant", content: "Second historical message" },
    ];
    seedSession(baseMessages);
    const handle = await startComparison("source", "Initial comparison", [], [TARGET_A, TARGET_B]);
    await handle.done;

    const metadata = useSessionStore.getState().groups.find((group) => group.id === handle.groupId)?.comparison;
    expect(metadata?.baseMessageCount).toBe(2);
    useSessionStore.getState().replaceMessages(handle.sessionIds[0], [baseMessages[0]]);
    const beforeRetry = structuredClone(branch(handle.sessionIds[0]));
    mocks.attemptStream.mockClear();

    await expect(retryComparisonBranch(handle.sessionIds[0])).rejects.toThrow("base history is incomplete");

    expect(branch(handle.sessionIds[0])).toEqual(beforeRetry);
    expect(useSessionStore.getState().runningTurns[handle.sessionIds[0]]).toBeUndefined();
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("marks an existing synthesis stale before rerunning a source branch", async () => {
    mocks.attemptStream.mockImplementation(
      async (
        target: ResolvedTarget,
        _history: ChatMessage[],
        _tools: unknown[],
        _signal: AbortSignal,
        _effort: string,
        _sessionId: string,
        onDelta: (content: string) => void,
      ) => {
        if (target.kind !== "provider") throw new Error("Expected provider target");
        const content = target.providerId === PROVIDER_A.id ? "Original alpha" : "Original beta";
        onDelta(content);
        return successfulResult(content);
      },
    );
    const comparison = await startComparison("source", "Keep synthesis provenance", [], [TARGET_A, TARGET_B]);
    await comparison.done;
    const sourceBranches = [
      {
        sessionId: comparison.sessionIds[0],
        label: "Provider A · alpha",
        targetKey: TARGET_A.key,
        content: "Original alpha",
      },
      {
        sessionId: comparison.sessionIds[1],
        label: "Provider B · beta",
        targetKey: TARGET_B.key,
        content: "Original beta",
      },
    ];
    useSessionStore.getState().setComparisonSynthesis(comparison.groupId, {
      target: TARGET_A,
      sourceBranches,
      status: "completed",
      content: "Saved combined answer",
      startedAt: 10,
      completedAt: 20,
      durationMs: 10,
      error: null,
      usage: null,
    });

    const pendingRetry = deferred<AttemptResult>();
    mocks.attemptStream.mockReset();
    mocks.attemptStream.mockReturnValue(pendingRetry.promise);
    const retry = retryComparisonBranch(comparison.sessionIds[0]);

    await vi.waitFor(() => expect(branch(comparison.sessionIds[0]).comparisonBranch?.status).toBe("running"));
    expect(
      useSessionStore.getState().groups.find((group) => group.id === comparison.groupId)?.comparison?.synthesis,
    ).toMatchObject({
      status: "stale",
      content: "Saved combined answer",
      sourceBranches,
    });

    pendingRetry.resolve(successfulResult("Retried alpha"));
    await retry;
    expect(
      useSessionStore.getState().groups.find((group) => group.id === comparison.groupId)?.comparison?.synthesis?.status,
    ).toBe("stale");
  });
});

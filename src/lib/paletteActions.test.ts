import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

const runAgentTurnMock = vi.fn(async (_sessionId: string, _userText: string, _attachments?: unknown[]) => {});
vi.mock("./agentLoop", async () => {
  const actual = await vi.importActual<typeof import("./agentLoop")>("./agentLoop");
  return {
    ...actual,
    runAgentTurn: (sessionId: string, userText: string, attachments?: unknown[]) =>
      runAgentTurnMock(sessionId, userText, attachments),
  };
});

const runRecipeNowMock = vi.fn(async (_recipe: unknown, _overrides: Record<string, string>) => ({
  sessionId: "recipe-session",
  done: Promise.resolve(),
}));
vi.mock("./recipeRunner", () => ({
  runRecipeNow: (recipe: unknown, overrides: Record<string, string>) => runRecipeNowMock(recipe, overrides),
}));

interface FakeRecorder {
  complete: ReturnType<typeof vi.fn>;
  fail: ReturnType<typeof vi.fn>;
}
const beginDurableRunMock = vi.fn<(...args: unknown[]) => Promise<FakeRecorder>>();
vi.mock("./durableRun", () => ({
  beginDurableRun: (...args: unknown[]) => beginDurableRunMock(...args),
}));

import {
  buildQuickActionPrompt,
  cancelSearchKnowledge,
  resolveTargetSessionId,
  runApprovePending,
  runCreateTask,
  runQuickAction,
  runSearchKnowledge,
  runStartWorkflow,
  type CapturedContext,
} from "./paletteActions";
import { useSessionStore } from "../store/sessionStore";
import { useModelStore } from "../store/modelStore";
import { usePermissionStore } from "../store/permissionStore";
import type { Recipe } from "../store/recipeStore";
import { useKnowledgeV2Store, type KnowledgeInspectorResponse } from "../store/knowledgeV2Store";

function makeRecipe(overrides: Partial<Recipe> = {}): Recipe {
  return {
    version: 1,
    name: "nightly-deps-audit",
    target: { ollama: "qwen2.5:14b" },
    permission_mode: "acceptEdits",
    prompt: "Check {{manifest}} for outdated deps.",
    params: { manifest: "package.json" },
    output: { json: false },
    ...overrides,
  };
}

function fakeRecorder(): FakeRecorder {
  return { complete: vi.fn(async () => {}), fail: vi.fn(async () => {}) };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  runAgentTurnMock.mockReset();
  runAgentTurnMock.mockResolvedValue(undefined);
  runRecipeNowMock.mockReset();
  runRecipeNowMock.mockResolvedValue({ sessionId: "recipe-session", done: Promise.resolve() });
  beginDurableRunMock.mockReset();
  beginDurableRunMock.mockResolvedValue(fakeRecorder());
  usePermissionStore.setState({ mode: "manual", pending: null, queue: [] });
});

describe("buildQuickActionPrompt", () => {
  const context: CapturedContext = { source: "clipboard", text: "Hello world", imageDataUrl: null };

  it("wraps captured text as untrusted data for summarize", () => {
    const prompt = buildQuickActionPrompt("summarize", context);
    expect(prompt).toContain("Summarize the following");
    expect(prompt).toContain("BEGIN UNTRUSTED DATA");
    expect(prompt).toContain("Hello world");
  });

  it("notes there's nothing to rewrite when no context was captured", () => {
    const prompt = buildQuickActionPrompt("rewrite", null);
    expect(prompt).toContain("nothing to rewrite");
  });

  it("defaults translate's target language to English when none is given", () => {
    expect(buildQuickActionPrompt("translate", context)).toContain("into English");
  });

  it("uses the given target language for translate", () => {
    expect(buildQuickActionPrompt("translate", context, "French")).toContain("into French");
  });

  it("combines the free-form question with captured context for askModel", () => {
    const prompt = buildQuickActionPrompt("askModel", context, "What does this mean?");
    expect(prompt).toContain("What does this mean?");
    expect(prompt).toContain("Hello world");
  });

  it("askModel with no context and no question is empty", () => {
    expect(buildQuickActionPrompt("askModel", null, "")).toBe("");
  });
});

describe("resolveTargetSessionId", () => {
  it("reuses the active session when no turn is running in it", () => {
    useSessionStore.setState((state) => {
      const [first] = state.sessions;
      return { activeSessionId: first.id, runningTurns: {} };
    });
    const activeId = useSessionStore.getState().activeSessionId;
    expect(resolveTargetSessionId()).toBe(activeId);
  });

  it("creates a fresh session when the active one is mid-turn", () => {
    useSessionStore.setState((state) => {
      const [first] = state.sessions;
      return { activeSessionId: first.id, runningTurns: { [first.id]: true } };
    });
    const busyId = useSessionStore.getState().activeSessionId;
    const resolved = resolveTargetSessionId();
    expect(resolved).not.toBe(busyId);
    expect(useSessionStore.getState().activeSessionId).toBe(resolved);
  });
});

describe("runQuickAction", () => {
  it("throws instead of starting a turn when there's nothing to send", async () => {
    await expect(runQuickAction("askModel", null, "")).rejects.toThrow("Nothing to send");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  it("runs the exact same runAgentTurn the chat composer uses, in the resolved session", async () => {
    useSessionStore.setState((state) => {
      const [first] = state.sessions;
      return { activeSessionId: first.id, runningTurns: {} };
    });
    const sessionId = useSessionStore.getState().activeSessionId;

    const outcome = await runQuickAction("summarize", { source: "clipboard", text: "Some text", imageDataUrl: null });

    expect(outcome.sessionId).toBe(sessionId);
    expect(runAgentTurnMock).toHaveBeenCalledTimes(1);
    const [calledSessionId, calledPrompt, calledAttachments] = runAgentTurnMock.mock.calls[0];
    expect(calledSessionId).toBe(sessionId);
    expect(calledPrompt).toContain("Some text");
    expect(calledAttachments).toEqual([]);
  });

  it("attaches a captured screenshot as a vision attachment instead of inlining it as text", async () => {
    useSessionStore.setState((state) => {
      const [first] = state.sessions;
      return { activeSessionId: first.id, runningTurns: {} };
    });

    await runQuickAction("askModel", { source: "screenshot", text: null, imageDataUrl: "data:image/png;base64,AAA" }, "What is this?");

    const [, , attachments] = runAgentTurnMock.mock.calls[0] as [string, string, { kind?: string; dataUrl?: string }[]];
    expect(attachments).toHaveLength(1);
    expect(attachments[0].kind).toBe("image");
    expect(attachments[0].dataUrl).toBe("data:image/png;base64,AAA");
  });
});

describe("runStartWorkflow", () => {
  it("passes captured text as a param override when the recipe declares a matching free-form param", async () => {
    const recipe = makeRecipe({ params: { context: null } });
    await runStartWorkflow(recipe, { source: "clipboard", text: "captured text", imageDataUrl: null });
    expect(runRecipeNowMock).toHaveBeenCalledWith(recipe, { context: "captured text" });
  });

  it("passes no overrides when the recipe declares no matching param", async () => {
    const recipe = makeRecipe({ params: { manifest: "package.json" } });
    await runStartWorkflow(recipe, { source: "clipboard", text: "captured text", imageDataUrl: null });
    expect(runRecipeNowMock).toHaveBeenCalledWith(recipe, {});
  });

  it("passes no overrides when there is no captured context", async () => {
    const recipe = makeRecipe({ params: { context: null } });
    await runStartWorkflow(recipe, null);
    expect(runRecipeNowMock).toHaveBeenCalledWith(recipe, {});
  });

  it("returns the session id runRecipeNow started", async () => {
    const outcome = await runStartWorkflow(makeRecipe(), null);
    expect(outcome.sessionId).toBe("recipe-session");
  });
});

function selectOllamaModel(tag: string) {
  useModelStore.setState({
    activeProvider: "ollama",
    activeOllamaModel: tag,
    ollamaModels: [{ name: tag, size_bytes: 0, is_cloud: false, tool_calling: true, vision: false, modified_at: "" }],
    ollamaReachable: true,
  });
}

describe("runCreateTask", () => {
  it("rejects a blank name or prompt without touching the recipe store", async () => {
    selectOllamaModel("qwen2.5:14b");
    await expect(runCreateTask("", "do the thing")).rejects.toThrow("name");
    await expect(runCreateTask("my task", "  ")).rejects.toThrow("something to do");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("rejects when the active target is the local runtime (recipes have no local-target field)", async () => {
    useModelStore.setState({ activeProvider: "local", active: null });
    await expect(runCreateTask("my task", "do the thing")).rejects.toThrow("Ollama or cloud-provider");
  });

  it("saves a recipe targeting the active ollama model under a safe permission mode", async () => {
    selectOllamaModel("qwen2.5:14b");
    usePermissionStore.setState({ mode: "bypass" });
    invokeMock.mockImplementation(async (...args: unknown[]) => {
      const command = args[0] as string;
      if (command === "recipes_save") return makeRecipe({ name: "my task" });
      if (command === "recipes_list") return [];
      return null;
    });

    const recipe = await runCreateTask("my task", "Do the thing.");
    expect(recipe.name).toBe("my task");

    const saveCall = invokeMock.mock.calls.find(([cmd]) => cmd === "recipes_save");
    expect(saveCall).toBeDefined();
    const content = (saveCall![1] as { name: string; content: string }).content;
    expect(content).toContain('name: "my task"');
    expect(content).toContain("ollama:");
    // bypass is never allowed for a saved recipe — falls back to manual.
    expect(content).toContain("permission_mode: manual");
    expect(content).toContain("prompt: |");
    expect(content).toContain("  Do the thing.");
  });
});

describe("runSearchKnowledge", () => {
  it("requires a stack and a query", async () => {
    await expect(runSearchKnowledge("", "query")).rejects.toThrow("stack");
    await expect(runSearchKnowledge("stack-1", "  ")).rejects.toThrow("search for");
  });

  it("requires an active model target so the run has something to record against", async () => {
    useModelStore.setState({ activeProvider: "local", active: null });
    await expect(runSearchKnowledge("stack-1", "query")).rejects.toThrow("Select a model");
  });

  it("records a background run and completes it with the hit count on success", async () => {
    selectOllamaModel("qwen2.5:14b");
    const recorder = fakeRecorder();
    beginDurableRunMock.mockResolvedValue(recorder);
    const response: KnowledgeInspectorResponse = {
      query_id: "q1",
      normalized_query: "query",
      excluded_source_ids: [],
      token_budget: 4096,
      estimated_context_tokens: 0,
      final_context: "",
      search: { hits: [], diagnostics: {} as KnowledgeInspectorResponse["search"]["diagnostics"] },
    };
    const queryMock = vi.fn(async () => response);
    useKnowledgeV2Store.setState({ query: queryMock });

    const outcome = await runSearchKnowledge("stack-1", "query");

    expect(queryMock).toHaveBeenCalled();
    expect(outcome.response).toBe(response);
    expect(recorder.complete).toHaveBeenCalledTimes(1);
    expect(recorder.fail).not.toHaveBeenCalled();
  });

  it("fails the run and rethrows when the query itself fails", async () => {
    selectOllamaModel("qwen2.5:14b");
    const recorder = fakeRecorder();
    beginDurableRunMock.mockResolvedValue(recorder);
    const failure = new Error("index unavailable");
    useKnowledgeV2Store.setState({ query: vi.fn(async () => { throw failure; }) });

    await expect(runSearchKnowledge("stack-1", "query")).rejects.toThrow("index unavailable");
    expect(recorder.fail).toHaveBeenCalledWith(failure);
    expect(recorder.complete).not.toHaveBeenCalled();
  });
});

describe("cancelSearchKnowledge", () => {
  it("delegates to the knowledge store's cancelQuery", async () => {
    const cancelQueryMock = vi.fn(async () => true);
    useKnowledgeV2Store.setState({ cancelQuery: cancelQueryMock });
    await expect(cancelSearchKnowledge("run-1")).resolves.toBe(true);
    expect(cancelQueryMock).toHaveBeenCalledWith("run-1");
  });
});

describe("runApprovePending", () => {
  it("throws when there is no pending permission request", async () => {
    usePermissionStore.setState({ pending: null });
    await expect(runApprovePending(true)).rejects.toThrow("no pending permission request");
  });

  it("calls permissionStore.respond with the exact same code path the modal's buttons use", async () => {
    const respondMock = vi.fn(async () => {});
    usePermissionStore.setState({
      pending: { id: "req-1", tool: "run_shell", detail: "echo hi" },
      respond: respondMock,
    });
    await runApprovePending(true);
    expect(respondMock).toHaveBeenCalledWith(true, false);
  });
});

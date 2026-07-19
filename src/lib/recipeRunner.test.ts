import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

const runAgentTurnMock = vi.fn(async (_sessionId: string, _userText: string, _attachments?: unknown[], _signal?: AbortSignal, _turnId?: string) => {});
vi.mock("./agentLoop", async () => {
  const actual = await vi.importActual<typeof import("./agentLoop")>("./agentLoop");
  return {
    ...actual,
    runAgentTurn: (sessionId: string, userText: string, attachments?: unknown[], signal?: AbortSignal, turnId?: string) =>
      runAgentTurnMock(sessionId, userText, attachments, signal, turnId),
  };
});

import { runRecipeNow } from "./recipeRunner";
import { isRecipeNotice, parseRecipeNotice } from "./agentLoop";
import { useModelStore } from "../store/modelStore";
import { useSessionStore } from "../store/sessionStore";
import { usePermissionStore } from "../store/permissionStore";
import type { Recipe } from "../store/recipeStore";

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

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  runAgentTurnMock.mockReset();
  usePermissionStore.setState({ mode: "manual" });
});

describe("runRecipeNow", () => {
  it("renders the recipe, switches to its ollama target, and starts a tagged session", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Check package.json for outdated deps.", system: null });

    const recipe = makeRecipe();
    const { sessionId, done } = await runRecipeNow(recipe);
    await done;

    expect(invokeMock).toHaveBeenCalledWith("recipes_render", { nameOrPath: "nightly-deps-audit", overrides: {} });
    expect(useModelStore.getState().activeProvider).toBe("ollama");
    expect(useModelStore.getState().activeOllamaModel).toBe("qwen2.5:14b");

    const session = useSessionStore.getState().sessions.find((s) => s.id === sessionId);
    expect(session?.title).toBe("nightly-deps-audit");
    expect(session?.messages.some((m) => isRecipeNotice(m) && parseRecipeNotice(m)?.name === "nightly-deps-audit")).toBe(true);

    const setCall = invokeMock.mock.calls.find(([cmd]) => cmd === "set_permission_mode_for_turn");
    const turnId = (setCall?.[1] as { turnId: string } | undefined)?.turnId;
    expect(turnId).toEqual(expect.any(String));
    expect(runAgentTurnMock).toHaveBeenCalledWith(sessionId, "Check package.json for outdated deps.", [], undefined, turnId);
  });

  it("switches to a provider target when the recipe declares provider+model", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ target: { provider: "openrouter", model: "anthropic/claude-sonnet" } });

    await runRecipeNow(recipe);

    expect(useModelStore.getState().activeProvider).toBe("provider");
    expect(useModelStore.getState().activeProviderId).toBe("openrouter");
    expect(useModelStore.getState().activeProviderModel).toBe("anthropic/claude-sonnet");
  });

  it("prepends the recipe's rendered system text to the prompt for this one-shot turn", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: "You are auditing package.json." });
    const recipe = makeRecipe();

    const { sessionId, done } = await runRecipeNow(recipe);
    await done;

    const setCall = invokeMock.mock.calls.find(([cmd]) => cmd === "set_permission_mode_for_turn");
    const turnId = (setCall?.[1] as { turnId: string } | undefined)?.turnId;
    expect(runAgentTurnMock).toHaveBeenCalledWith(sessionId, "You are auditing package.json.\n\nDo the thing.", [], undefined, turnId);
  });

  it("rejects a local_url target with a message pointing at monkey task run, without touching the model store or starting a turn", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ target: { local_url: "http://127.0.0.1:8090" } });

    await expect(runRecipeNow(recipe)).rejects.toThrow("monkey task run");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  it("rejects a provider target missing its model", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ target: { provider: "openrouter" } });

    await expect(runRecipeNow(recipe)).rejects.toThrow("no 'model'");
  });

  it("rejects a recipe with no valid target at all", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ target: {} });

    await expect(runRecipeNow(recipe)).rejects.toThrow("no valid target");
  });

  it("propagates a render failure (e.g. an unresolved param) instead of starting a session", async () => {
    invokeMock.mockRejectedValueOnce(new Error("missing required --param value(s) (no default): manifest"));
    const recipe = makeRecipe();

    await expect(runRecipeNow(recipe)).rejects.toThrow("missing required");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  it("applies the recipe's own permission_mode via a turn-scoped override before the turn starts, clears it once the turn finishes, and never touches the global mode", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ permission_mode: "acceptEdits" });

    const { done } = await runRecipeNow(recipe);

    const setCall = invokeMock.mock.calls.find(([cmd]) => cmd === "set_permission_mode_for_turn");
    expect(setCall).toBeDefined();
    const turnId = (setCall![1] as { turnId: string }).turnId;
    expect(setCall![1]).toEqual({ turnId, mode: "acceptEdits" });
    // The override must already be set by the time `runAgentTurn` is called —
    // not just eventually — since a permission prompt mid-turn depends on it.
    expect(runAgentTurnMock).toHaveBeenCalledWith(expect.any(String), "Do the thing.", [], undefined, turnId);
    // Crucially: the app's single global mode is never touched by a recipe
    // run — this is exactly the split-pane race the turn-scoped override
    // replaces the old global set/restore dance to avoid.
    expect(usePermissionStore.getState().mode).toBe("manual");

    await done;

    expect(invokeMock).toHaveBeenCalledWith("clear_permission_mode_for_turn", { turnId });
    expect(usePermissionStore.getState().mode).toBe("manual");
  });

  it("applies permissionModeOverride instead of the recipe's own mode when given (the scheduler's use)", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ permission_mode: "manual" });

    const { done } = await runRecipeNow(recipe, {}, "acceptEdits");

    const setCall = invokeMock.mock.calls.find(([cmd]) => cmd === "set_permission_mode_for_turn");
    expect(setCall?.[1]).toMatchObject({ mode: "acceptEdits" });

    await done;
  });

  it("rejects an invalid permission_mode before rendering or touching any store", async () => {
    const recipe = makeRecipe({ permission_mode: "yolo" });

    await expect(runRecipeNow(recipe)).rejects.toThrow("not a valid mode");
    expect(invokeMock).not.toHaveBeenCalled();
    expect(usePermissionStore.getState().mode).toBe("manual");
  });

  it("rejects a recipe whose own permission_mode is 'bypass' before rendering or touching any store", async () => {
    const recipe = makeRecipe({ permission_mode: "bypass" });

    await expect(runRecipeNow(recipe)).rejects.toThrow("not allowed");
    expect(invokeMock).not.toHaveBeenCalled();
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  it("rejects a scheduler permissionModeOverride of 'bypass' the same way, even when the recipe's own mode is safe", async () => {
    const recipe = makeRecipe({ permission_mode: "manual" });

    await expect(runRecipeNow(recipe, {}, "bypass")).rejects.toThrow("not allowed");
    expect(invokeMock).not.toHaveBeenCalled();
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });
});

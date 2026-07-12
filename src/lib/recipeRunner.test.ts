import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

const runAgentTurnMock = vi.fn(async (_sessionId: string, _userText: string) => {});
vi.mock("./agentLoop", async () => {
  const actual = await vi.importActual<typeof import("./agentLoop")>("./agentLoop");
  return { ...actual, runAgentTurn: (sessionId: string, userText: string) => runAgentTurnMock(sessionId, userText) };
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

    expect(runAgentTurnMock).toHaveBeenCalledWith(sessionId, "Check package.json for outdated deps.");
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

    expect(runAgentTurnMock).toHaveBeenCalledWith(sessionId, "You are auditing package.json.\n\nDo the thing.");
  });

  it("rejects a local_url target with a message pointing at monkey-cli task run, without touching the model store or starting a turn", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ target: { local_url: "http://127.0.0.1:8090" } });

    await expect(runRecipeNow(recipe)).rejects.toThrow("monkey-cli task run");
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

  it("applies the recipe's own permission_mode before the turn starts, and restores the previous mode once it finishes", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ permission_mode: "bypass" });
    expect(usePermissionStore.getState().mode).toBe("manual");

    const { done } = await runRecipeNow(recipe);
    // The mode must already be applied by the time `runAgentTurn` is called,
    // not just eventually — a permission prompt mid-turn depends on it.
    expect(usePermissionStore.getState().mode).toBe("bypass");

    await done;
    expect(usePermissionStore.getState().mode).toBe("manual");
  });

  it("applies permissionModeOverride instead of the recipe's own mode when given (the scheduler's use)", async () => {
    invokeMock.mockResolvedValueOnce({ prompt: "Do the thing.", system: null });
    const recipe = makeRecipe({ permission_mode: "manual" });

    const { done } = await runRecipeNow(recipe, {}, "acceptEdits");
    expect(usePermissionStore.getState().mode).toBe("acceptEdits");

    await done;
  });

  it("rejects an invalid permission_mode before rendering or touching any store", async () => {
    const recipe = makeRecipe({ permission_mode: "yolo" });

    await expect(runRecipeNow(recipe)).rejects.toThrow("not a valid mode");
    expect(invokeMock).not.toHaveBeenCalled();
    expect(usePermissionStore.getState().mode).toBe("manual");
  });
});

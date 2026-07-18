import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
const runRequestHandlerRef = vi.hoisted(() => ({
  current: null as ((event: { payload: unknown }) => void) | null,
}));
const changedHandlerRef = vi.hoisted(() => ({
  current: null as ((event: { payload: unknown }) => void) | null,
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test-window" }) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (event: { payload: unknown }) => void) => {
    if (name === "local-apps://run-requested") runRequestHandlerRef.current = handler;
    if (name === "local-apps://changed") changedHandlerRef.current = handler;
    return Promise.resolve(() => {});
  },
}));

const runRecipeNowMock = vi.fn(
  async (_recipe: unknown, _paramOverrides?: unknown, _permissionModeOverride?: string, _localAppId?: string) => ({
    sessionId: "session-1",
    done: Promise.resolve(),
  }),
);
vi.mock("../lib/recipeRunner", () => ({
  runRecipeNow: (recipe: unknown, paramOverrides?: unknown, permissionModeOverride?: string, localAppId?: string) =>
    runRecipeNowMock(recipe, paramOverrides, permissionModeOverride, localAppId),
}));

import {
  useLocalAppsStore,
  subscribeToLocalAppsChanges,
  subscribeToLocalAppRunRequests,
  type LocalAppDefinition,
} from "./localAppsStore";

function sampleApp(overrides: Partial<LocalAppDefinition> = {}): LocalAppDefinition {
  return {
    id: "app-1",
    name: "nightly-audit",
    recipe_name: "nightly-audit",
    template: "form",
    param_bindings: {},
    scoped_token_id: "tok-1",
    created_at: 1,
    enabled: true,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  runRecipeNowMock.mockClear();
  useLocalAppsStore.setState({ apps: [], loading: false, error: null });
  runRequestHandlerRef.current = null;
  changedHandlerRef.current = null;
});

describe("localAppsStore", () => {
  it("refreshes the app list from local_apps_list", async () => {
    invokeMock.mockResolvedValueOnce([sampleApp()]);
    await useLocalAppsStore.getState().refresh();
    expect(invokeMock).toHaveBeenCalledWith("local_apps_list");
    expect(useLocalAppsStore.getState().apps).toEqual([sampleApp()]);
    expect(useLocalAppsStore.getState().error).toBeNull();
  });

  it("surfaces a refresh failure without throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    await useLocalAppsStore.getState().refresh();
    expect(useLocalAppsStore.getState().error).toBe("boom");
    expect(useLocalAppsStore.getState().loading).toBe(false);
  });

  it("publishes an app and then refreshes the list", async () => {
    const published = sampleApp();
    invokeMock.mockResolvedValueOnce(published); // local_apps_publish
    invokeMock.mockResolvedValueOnce([published]); // local_apps_list from refresh

    const result = await useLocalAppsStore.getState().publish("nightly-audit", "form", { target: "Target file" });

    expect(invokeMock).toHaveBeenNthCalledWith(1, "local_apps_publish", {
      recipeName: "nightly-audit",
      template: "form",
      paramBindings: { target: "Target file" },
    });
    expect(result).toEqual(published);
    expect(useLocalAppsStore.getState().apps).toEqual([published]);
  });

  it("unpublishes an app and then refreshes the list", async () => {
    invokeMock.mockResolvedValueOnce(undefined); // local_apps_unpublish
    invokeMock.mockResolvedValueOnce([]); // local_apps_list from refresh

    await useLocalAppsStore.getState().unpublish("app-1");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "local_apps_unpublish", { id: "app-1" });
    expect(useLocalAppsStore.getState().apps).toEqual([]);
  });

  it("resolves the open URL via local_apps_open", async () => {
    invokeMock.mockResolvedValueOnce("http://127.0.0.1:1234/local-apps/app-1");
    const url = await useLocalAppsStore.getState().open("app-1");
    expect(invokeMock).toHaveBeenCalledWith("local_apps_open", { id: "app-1" });
    expect(url).toBe("http://127.0.0.1:1234/local-apps/app-1");
  });

  it("refreshes when another window's local-apps://changed event arrives, but ignores its own", async () => {
    await subscribeToLocalAppsChanges();
    invokeMock.mockResolvedValueOnce([sampleApp()]);

    changedHandlerRef.current?.({ payload: "test-window" });
    await Promise.resolve();
    expect(invokeMock).not.toHaveBeenCalled();

    changedHandlerRef.current?.({ payload: "other-window" });
    await Promise.resolve();
    await Promise.resolve();
    expect(invokeMock).toHaveBeenCalledWith("local_apps_list");
  });

  it("resolves the named recipe and runs it tagged with the app id when a run is requested", async () => {
    await subscribeToLocalAppRunRequests();
    const recipe = { name: "nightly-audit", params: { target: null } };
    invokeMock.mockResolvedValueOnce(recipe); // recipes_read

    runRequestHandlerRef.current?.({
      payload: { app_id: "app-1", recipe_name: "nightly-audit", params: { target: "package.json" } },
    });
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    expect(invokeMock).toHaveBeenCalledWith("recipes_read", { nameOrPath: "nightly-audit" });
    expect(runRecipeNowMock).toHaveBeenCalledWith(recipe, { target: "package.json" }, undefined, "app-1");
  });
});

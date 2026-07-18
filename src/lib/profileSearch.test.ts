import { beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));

import { globalProfileSearch } from "./profileSearch";

describe("globalProfileSearch", () => {
  beforeEach(() => invoke.mockReset());

  it("does not query the host for blank input", async () => {
    await expect(globalProfileSearch({
      query: "  ", includeArchived: false, fromMs: null, toMs: null,
      modelKey: null, personaId: null, workspacePath: null, limit: 25,
    })).resolves.toEqual([]);
    expect(invoke).not.toHaveBeenCalled();
  });

  it("passes every bounded filter through one structured request", async () => {
    invoke.mockResolvedValueOnce({ state: "current" }).mockResolvedValueOnce([]);
    await globalProfileSearch({
      query: "  durable runs ", includeArchived: true, fromMs: 10, toMs: 20,
      modelKey: "ollama:qwen", personaId: "reviewer", workspacePath: "/repo", limit: 100,
    });
    expect(invoke).toHaveBeenLastCalledWith("profile_global_search", {
      request: {
        query: "durable runs", includeArchived: true, fromMs: 10, toMs: 20,
        modelKey: "ollama:qwen", personaId: "reviewer", workspacePath: "/repo", limit: 100,
      },
    });
  });
});

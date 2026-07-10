import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { useRulesStore, type RuleFile } from "./rulesStore";

function makeRule(overrides: Partial<RuleFile> = {}): RuleFile {
  return {
    scope: "global",
    label: "global",
    path: "/app-data/MONKEY.md",
    content: "Always write tests.",
    truncated: false,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useRulesStore.setState({ rules: [], facts: [] });
});

describe("rulesStore.refresh", () => {
  it("calls rules_read and caches the result", async () => {
    const rule = makeRule();
    invokeMock.mockResolvedValueOnce([rule]);

    await useRulesStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("rules_read");
    expect(useRulesStore.getState().rules).toEqual([rule]);
  });

  it("falls back to an empty list instead of throwing when the backend errors", async () => {
    invokeMock.mockRejectedValueOnce(new Error("disk unavailable"));

    await expect(useRulesStore.getState().refresh()).resolves.toBeUndefined();

    expect(useRulesStore.getState().rules).toEqual([]);
  });

  it("leaves facts empty — memory.rs lands in a later slice", async () => {
    invokeMock.mockResolvedValueOnce([makeRule()]);

    await useRulesStore.getState().refresh();

    expect(useRulesStore.getState().facts).toEqual([]);
  });
});

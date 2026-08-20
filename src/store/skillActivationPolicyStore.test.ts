import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listen: vi.fn(),
  eventHandler: null as ((event: { payload: string }) => void) | null,
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_event: string, handler: (event: { payload: string }) => void) => {
    mocks.listen(_event, handler);
    mocks.eventHandler = handler;
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: "main" }),
}));

import { useSkillActivationPolicyStore } from "./skillActivationPolicyStore";
import type { SkillActivationEntry } from "../lib/skillActivationClient";

function entry(policy: SkillActivationEntry["policy"]): SkillActivationEntry {
  return { key: "native:global:deploy", policy, pinned: false, updated_at_unix_ms: 1 };
}

describe("skill activation policy cross-window refresh", () => {
  beforeEach(() => {
    mocks.invoke.mockReset();
    mocks.listen.mockReset();
    mocks.eventHandler = null;
    useSkillActivationPolicyStore.setState({
      policies: {},
      hydrated: false,
      hydrating: false,
      error: null,
    });
  });

  it("refreshes a renderer when another window changes the backend policy", async () => {
    let backend = [entry("automatic")];
    mocks.invoke.mockImplementation(async (command: string) => {
      if (command === "skill_activation_list") return backend;
      if (command === "skill_activation_migrate") return backend;
      throw new Error(`unexpected command ${command}`);
    });

    await useSkillActivationPolicyStore.getState().hydrate();
    expect(useSkillActivationPolicyStore.getState().getPolicy("native:global:deploy")).toBe("automatic");

    backend = [entry("manual")];
    mocks.eventHandler?.({ payload: "secondary" });
    await vi.waitFor(() => {
      expect(useSkillActivationPolicyStore.getState().getPolicy("native:global:deploy")).toBe("manual");
    });
    expect(mocks.listen).toHaveBeenCalledWith("skill-activation://changed", expect.any(Function));
  });
});

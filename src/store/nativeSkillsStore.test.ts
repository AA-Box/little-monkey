import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  eventHandler: null as ((event: { payload: string }) => void) | null,
}));

vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(async (_event: string, handler: (event: { payload: string }) => void) => {
    mocks.eventHandler = handler;
    return () => {};
  }),
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: "main" }),
}));

import { subscribeToNativeSkillChanges, useNativeSkillsStore } from "./nativeSkillsStore";

describe("native skill registry invalidation", () => {
  beforeEach(() => {
    mocks.eventHandler = null;
    useNativeSkillsStore.setState({ revision: 0 });
  });

  it("bumps the discovery revision for another window and filesystem changes", async () => {
    await subscribeToNativeSkillChanges();
    expect(useNativeSkillsStore.getState().revision).toBe(0);

    mocks.eventHandler?.({ payload: "secondary" });
    expect(useNativeSkillsStore.getState().revision).toBe(1);

    mocks.eventHandler?.({ payload: "filesystem" });
    expect(useNativeSkillsStore.getState().revision).toBe(2);
    mocks.eventHandler?.({ payload: "main" });
    expect(useNativeSkillsStore.getState().revision).toBe(2);
  });
});

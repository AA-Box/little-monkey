import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  eventHandler: null as ((event: { payload: string }) => void) | null,
  listen: vi.fn(),
  discover: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: mocks.listen,
}));
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: "main" }),
}));
vi.mock("../lib/skillLearningClient", () => ({
  skillLearningClient: { discover: mocks.discover },
}));

import { subscribeToNativeSkillChanges, useNativeSkillsStore } from "./nativeSkillsStore";
import type { NativeSkillDescriptor } from "../lib/nativeSkillsClient";

const descriptor: NativeSkillDescriptor = {
  name: "Review",
  description: "Review code",
  command: "review",
  version: "1.0.0",
  instructions: "Review the change.",
  sha256: "a".repeat(64),
  file_count: 1,
  total_bytes: 20,
  enabled: true,
  managed: true,
  eligibility: {
    eligible: true,
    current_os: "linux",
    unsupported_os: false,
    missing_bins: [],
    missing_env: [],
  },
  supported_os: [],
  requirements: { bins: [], env: [] },
  source: { kind: "global", path: "/skills/review" },
  permissions: [],
  git_repository: null,
  allowed_tools: [],
  resource_files: [],
};

describe("native skill registry invalidation", () => {
  beforeEach(() => {
    mocks.eventHandler = null;
    mocks.listen.mockReset();
    mocks.discover.mockReset().mockResolvedValue([descriptor]);
    useNativeSkillsStore.setState({ descriptors: [], loading: false, error: null, generation: 0 });
  });

  it("retries listener registration and refreshes the shared registry", async () => {
    mocks.listen.mockRejectedValueOnce(new Error("temporary listener failure"));
    await expect(subscribeToNativeSkillChanges()).rejects.toThrow("temporary listener failure");

    mocks.listen.mockImplementationOnce(async (_event: string, handler: (event: { payload: string }) => void) => {
      mocks.eventHandler = handler;
      return () => {};
    });
    await subscribeToNativeSkillChanges();
    expect(useNativeSkillsStore.getState().descriptors).toEqual([descriptor]);
    expect(useNativeSkillsStore.getState().generation).toBe(1);
    expect(mocks.discover).toHaveBeenCalledTimes(1);

    mocks.eventHandler?.({ payload: "secondary" });
    await vi.waitFor(() => expect(mocks.discover).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => expect(useNativeSkillsStore.getState().generation).toBe(2));
    expect(useNativeSkillsStore.getState().generation).toBe(2);

    mocks.eventHandler?.({ payload: "filesystem" });
    await vi.waitFor(() => expect(mocks.discover).toHaveBeenCalledTimes(3));
    expect(useNativeSkillsStore.getState().generation).toBe(3);
    mocks.eventHandler?.({ payload: "main" });
    expect(mocks.discover).toHaveBeenCalledTimes(3);
  });
});

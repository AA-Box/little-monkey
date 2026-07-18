import { describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "ultracode-runner-test" }) }));
// `ultracodeRunner.ts` only needs these two functions/type from `compareRunner`
// — mocking the module itself (rather than importing the real one) skips its
// much larger dependency graph (agentLoop, turnEngine, comparisonPlan, usage
// stores, durableRun, ...), which is irrelevant to the guard-clause behavior
// under test here.
vi.mock("./compareRunner", () => ({
  startComparison: vi.fn(),
  startComparisonSynthesis: vi.fn(),
}));

import { startUltracode } from "./ultracodeRunner";

describe("startUltracode", () => {
  it("throws a clear error instead of starting a comparison when fewer than 2 models are available", async () => {
    // Default `useModelStore`/`useSettingsStore` state has zero installed
    // local models and zero configured providers, so `buildModelTargetInventory`
    // yields zero available targets — this is exactly the state a fresh
    // install (or a dev environment with no provider credentials) starts in.
    await expect(startUltracode("session-1", "hello", [])).rejects.toThrow(
      /Ultracode needs at least 2 available models/,
    );
  });
});

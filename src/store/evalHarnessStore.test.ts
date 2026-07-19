import { beforeEach, describe, expect, it, vi } from "vitest";

import type { EvalRuntime } from "../lib/evalHarness";
import {
  __resetEvalHarnessStoreForTests,
  __setEvalRuntimeFactoryForTests,
  useEvalHarnessStore,
} from "./evalHarnessStore";

class MemoryStorage implements Storage {
  private values = new Map<string, string>();
  get length() { return this.values.size; }
  clear() { this.values.clear(); }
  getItem(key: string) { return this.values.get(key) ?? null; }
  key(index: number) { return [...this.values.keys()][index] ?? null; }
  removeItem(key: string) { this.values.delete(key); }
  setItem(key: string, value: string) { this.values.set(key, String(value)); }
}

function passingRuntime(): EvalRuntime {
  return {
    execute: vi.fn().mockResolvedValue({
      output: "ready",
      toolCalls: [],
      usage: { promptTokens: 2, completionTokens: 1, totalTokens: 3 },
      costMicros: 0,
      executionSucceeded: true,
      targetLabel: "test",
      metadata: { fixture: true },
    }),
    judge: vi.fn().mockResolvedValue({ passed: true, score: 1, evidence: "ok", usage: null }),
  };
}

describe("evalHarnessStore", () => {
  beforeEach(() => {
    Object.defineProperty(globalThis, "localStorage", { value: new MemoryStorage(), configurable: true });
    __resetEvalHarnessStoreForTests();
    __setEvalRuntimeFactoryForTests(passingRuntime);
  });

  it("durably persists suite edits and case definitions", () => {
    const id = useEvalHarnessStore.getState().createSuite("Agent regression");
    const firstCase = useEvalHarnessStore.getState().suites[0].cases[0];
    useEvalHarnessStore.getState().updateSuite(id, { description: "Release checks", releaseGate: true });
    useEvalHarnessStore.getState().updateCase(id, firstCase.id, {
      input: "Say ready",
      allowedTools: ["lookup"],
      expectations: { ...firstCase.expectations, contains: ["ready"] },
    });

    const persisted = JSON.parse(localStorage.getItem("little-monkey-eval-harness-v1") ?? "null");
    expect(persisted.version).toBe(1);
    expect(persisted.suites[0]).toMatchObject({ id, description: "Release checks", releaseGate: true });
    expect(persisted.suites[0].cases[0]).toMatchObject({ input: "Say ready", allowedTools: ["lookup"] });
    expect(persisted.suites[0].revision).toBeGreaterThan(1);
  });

  it("runs the real scoring path, persists history, and opens the release gate", async () => {
    const id = useEvalHarnessStore.getState().createSuite("Gate");
    const firstCase = useEvalHarnessStore.getState().suites[0].cases[0];
    useEvalHarnessStore.getState().updateCase(id, firstCase.id, {
      input: "Return ready",
      expectations: { ...firstCase.expectations, contains: ["ready"] },
    });
    useEvalHarnessStore.getState().updateSuite(id, { releaseGate: true });

    const run = await useEvalHarnessStore.getState().runSuite(id);

    expect(run.status).toBe("passed");
    expect(run.results[0].assertions.find((entry) => entry.id === "contains-0")?.passed).toBe(true);
    expect(useEvalHarnessStore.getState()).toMatchObject({ activeRunId: null, error: null });
    expect(useEvalHarnessStore.getState().runs[0].id).toBe(run.id);
    const persisted = JSON.parse(localStorage.getItem("little-monkey-eval-harness-v1") ?? "null");
    expect(persisted.runs[0]).toMatchObject({ id: run.id, status: "passed" });
  });

  it("surfaces validation errors without invoking the target", async () => {
    const fixture = passingRuntime();
    __setEvalRuntimeFactoryForTests(() => fixture);
    const id = useEvalHarnessStore.getState().createSuite("Invalid");

    await expect(useEvalHarnessStore.getState().runSuite(id)).rejects.toThrow(/input is required/i);
    expect(fixture.execute).not.toHaveBeenCalled();
    expect(useEvalHarnessStore.getState().error).toMatch(/input is required/i);
    expect(useEvalHarnessStore.getState().activeRunId).toBeNull();
  });

  it("cancels an in-flight run through its AbortSignal and records it", async () => {
    const fixture = passingRuntime();
    fixture.execute = vi.fn((_target, _case, _runId, signal) => new Promise<never>((_, reject) => {
      signal.addEventListener("abort", () => reject(new DOMException("cancelled", "AbortError")), { once: true });
    }));
    __setEvalRuntimeFactoryForTests(() => fixture);
    const id = useEvalHarnessStore.getState().createSuite("Cancelable");
    const firstCase = useEvalHarnessStore.getState().suites[0].cases[0];
    useEvalHarnessStore.getState().updateCase(id, firstCase.id, {
      input: "Wait",
      expectations: { ...firstCase.expectations, contains: ["done"] },
    });

    const pending = useEvalHarnessStore.getState().runSuite(id);
    useEvalHarnessStore.getState().cancelRun();
    const run = await pending;

    expect(run.status).toBe("cancelled");
    expect(useEvalHarnessStore.getState().runs[0].status).toBe("cancelled");
    expect(useEvalHarnessStore.getState().activeRunId).toBeNull();
  });

  it("duplicates artifacts with fresh identities and clears suite-scoped history", async () => {
    const id = useEvalHarnessStore.getState().createSuite("Original");
    const firstCase = useEvalHarnessStore.getState().suites[0].cases[0];
    useEvalHarnessStore.getState().updateCase(id, firstCase.id, {
      input: "Return ready",
      expectations: { ...firstCase.expectations, contains: ["ready"] },
    });
    await useEvalHarnessStore.getState().runSuite(id);

    const copyId = useEvalHarnessStore.getState().duplicateSuite(id);
    const copy = useEvalHarnessStore.getState().suites.find((suite) => suite.id === copyId);
    expect(copy?.id).not.toBe(id);
    expect(copy?.cases[0].id).not.toBe(firstCase.id);
    expect(copy?.releaseGate).toBe(false);

    useEvalHarnessStore.getState().clearHistory(id);
    expect(useEvalHarnessStore.getState().runs.filter((run) => run.suiteId === id)).toHaveLength(0);
  });
});

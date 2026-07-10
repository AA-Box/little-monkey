import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { useRulesStore, type MemoryFact, type RuleFile } from "./rulesStore";

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

function makeFact(overrides: Partial<MemoryFact> = {}): MemoryFact {
  return {
    id: "fact-1",
    text: "Uses pnpm, not npm.",
    source: "agent",
    created_at: "2026-01-01T00:00:00.000Z",
    ...overrides,
  };
}

/** Routes a mocked `invoke` call to the right fixture by command name, since
 * `refresh` now fires `rules_read` and `memory_list` concurrently via
 * `Promise.all`. */
function mockInvokes(rules: RuleFile[], facts: MemoryFact[]) {
  invokeMock.mockImplementation((cmd: string) => {
    if (cmd === "rules_read") return Promise.resolve(rules);
    if (cmd === "memory_list") return Promise.resolve(facts);
    return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  useRulesStore.setState({ rules: [], facts: [] });
});

describe("rulesStore.refresh", () => {
  it("calls rules_read and caches the result", async () => {
    const rule = makeRule();
    mockInvokes([rule], []);

    await useRulesStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("rules_read");
    expect(useRulesStore.getState().rules).toEqual([rule]);
  });

  it("falls back to an empty rules list instead of throwing when rules_read errors", async () => {
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "rules_read") return Promise.reject(new Error("disk unavailable"));
      if (cmd === "memory_list") return Promise.resolve([]);
      return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
    });

    await expect(useRulesStore.getState().refresh()).resolves.toBeUndefined();

    expect(useRulesStore.getState().rules).toEqual([]);
  });

  it("calls memory_list and caches the facts for the current primary root", async () => {
    const fact = makeFact();
    mockInvokes([], [fact]);

    await useRulesStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("memory_list");
    expect(useRulesStore.getState().facts).toEqual([fact]);
  });

  it("falls back to an empty facts list instead of throwing when memory_list errors, without wiping out rules", async () => {
    const rule = makeRule();
    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "rules_read") return Promise.resolve([rule]);
      if (cmd === "memory_list") return Promise.reject(new Error("disk unavailable"));
      return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
    });

    await expect(useRulesStore.getState().refresh()).resolves.toBeUndefined();

    expect(useRulesStore.getState().facts).toEqual([]);
    expect(useRulesStore.getState().rules).toEqual([rule]);
  });

  it("drops a stale refresh that resolves after a newer one, instead of overwriting fresher state", async () => {
    // Mirrors two concurrent refreshes racing (e.g. a turn's post-remember
    // refresh vs. a Forget button's refresh in another split pane): the
    // first call started earlier but its backend reads resolve *later* than
    // the second, newer call's — IPC gives no ordering guarantee. The
    // earlier-started call must not be allowed to clobber the later one's
    // result once it finally resolves.
    const staleFact = makeFact({ id: "stale", text: "about to be forgotten" });
    const freshFact = makeFact({ id: "fresh", text: "the current truth" });

    let resolveStaleRulesRead!: (v: RuleFile[]) => void;
    let resolveStaleMemoryList!: (v: MemoryFact[]) => void;
    let callCount = 0;

    invokeMock.mockImplementation((cmd: string) => {
      callCount += 1;
      const isFirstCall = callCount <= 2; // the stale refresh's two invokes
      if (cmd === "rules_read") {
        return isFirstCall
          ? new Promise<RuleFile[]>((resolve) => {
              resolveStaleRulesRead = resolve;
            })
          : Promise.resolve([]);
      }
      if (cmd === "memory_list") {
        return isFirstCall
          ? new Promise<MemoryFact[]>((resolve) => {
              resolveStaleMemoryList = resolve;
            })
          : Promise.resolve([freshFact]);
      }
      return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
    });

    const stalePromise = useRulesStore.getState().refresh(); // starts first, hangs
    await useRulesStore.getState().refresh(); // starts second, resolves immediately

    expect(useRulesStore.getState().facts).toEqual([freshFact]);

    // Now let the stale (earlier-started) refresh finally resolve.
    resolveStaleRulesRead([]);
    resolveStaleMemoryList([staleFact]);
    await stalePromise;

    // The stale result must not have overwritten the fresher state.
    expect(useRulesStore.getState().facts).toEqual([freshFact]);
  });
});

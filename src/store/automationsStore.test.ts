import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "test" }) }));

import {
  flushAutomationsPersistence,
  useAutomationsStore,
  hydrateAutomations,
  type AutomationEntry,
} from "./automationsStore";

function makeEntry(overrides: Partial<AutomationEntry> = {}): AutomationEntry {
  return {
    id: "entry-1",
    recipeName: "nightly-deps-audit",
    cron: "0 3 * * *",
    enabled: true,
    catchUpIfMissed: false,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async () => null);
  useAutomationsStore.setState({
    entries: [],
    persistedEntries: [],
    persistError: null,
    hydrated: false,
    scheduler: {
      authority: "unknown",
      daemonRunning: false,
      synchronizedAtMs: null,
      syncError: null,
      issues: {},
      lastDeliveryAtMs: {},
    },
  });
});

afterEach(async () => {
  await flushAutomationsPersistence();
});

describe("automationsStore", () => {
  it("addEntry generates an id and appends the entry", () => {
    const created = useAutomationsStore.getState().addEntry({
      recipeName: "nightly-deps-audit",
      cron: "0 3 * * *",
      enabled: true,
      catchUpIfMissed: false,
    });

    expect(created.id).toBeTruthy();
    expect(useAutomationsStore.getState().entries).toEqual([created]);
  });

  it("publishes a durable scheduler snapshot only after Rust saves it", async () => {
    let resolveSave: (() => void) | undefined;
    invokeMock.mockImplementationOnce(
      () => new Promise<void>((resolve) => {
        resolveSave = resolve;
      }),
    );
    const created = useAutomationsStore.getState().addEntry({
      recipeName: "nightly-deps-audit",
      cron: "0 3 * * *",
      enabled: true,
      catchUpIfMissed: false,
    });

    const flushing = flushAutomationsPersistence();
    await Promise.resolve();
    expect(invokeMock).toHaveBeenCalledWith("automations_save", expect.any(Object));
    expect(useAutomationsStore.getState().persistedEntries).toEqual([]);

    resolveSave?.();
    await flushing;
    expect(useAutomationsStore.getState().persistedEntries).toEqual([created]);
  });

  it("keeps the previous durable scheduler snapshot when saving fails", async () => {
    invokeMock.mockRejectedValueOnce(new Error("disk full"));
    useAutomationsStore.getState().addEntry({
      recipeName: "nightly-deps-audit",
      cron: "0 3 * * *",
      enabled: true,
      catchUpIfMissed: false,
    });

    await flushAutomationsPersistence();

    expect(useAutomationsStore.getState().persistedEntries).toEqual([]);
    expect(useAutomationsStore.getState().persistError).toBe("disk full");
  });

  it("updateEntry patches an existing entry and is a no-op for an unknown id", () => {
    const entry = makeEntry();
    useAutomationsStore.setState({ entries: [entry] });

    useAutomationsStore.getState().updateEntry(entry.id, { cron: "0 4 * * *" });
    expect(useAutomationsStore.getState().entries[0].cron).toBe("0 4 * * *");

    useAutomationsStore.getState().updateEntry("does-not-exist", { cron: "0 5 * * *" });
    expect(useAutomationsStore.getState().entries[0].cron).toBe("0 4 * * *");
  });

  it("removeEntry removes only the matching entry", () => {
    const a = makeEntry({ id: "a" });
    const b = makeEntry({ id: "b" });
    useAutomationsStore.setState({ entries: [a, b] });

    useAutomationsStore.getState().removeEntry("a");

    expect(useAutomationsStore.getState().entries).toEqual([b]);
  });

  it("recordRun sets lastRunAt/lastStatus/lastSessionId on the matching entry only", () => {
    const a = makeEntry({ id: "a" });
    const b = makeEntry({ id: "b" });
    useAutomationsStore.setState({ entries: [a, b] });

    useAutomationsStore.getState().recordRun("a", "ok", "session-123");

    const [updatedA, untouchedB] = useAutomationsStore.getState().entries;
    expect(updatedA.lastStatus).toBe("ok");
    expect(updatedA.lastSessionId).toBe("session-123");
    expect(typeof updatedA.lastRunAt).toBe("number");
    expect(untouchedB).toEqual(b);
  });

  it("recordRun keeps the previous lastSessionId when none is given (e.g. an error before a session existed)", () => {
    const entry = makeEntry({ lastSessionId: "old-session" });
    useAutomationsStore.setState({ entries: [entry] });

    useAutomationsStore.getState().recordRun(entry.id, "error");

    expect(useAutomationsStore.getState().entries[0].lastSessionId).toBe("old-session");
    expect(useAutomationsStore.getState().entries[0].lastStatus).toBe("error");
  });
});

describe("hydrateAutomations", () => {
  it("loads persisted entries via automations_load", async () => {
    const entry = makeEntry();
    invokeMock.mockResolvedValueOnce(JSON.stringify({ version: 1, entries: [entry] }));

    await hydrateAutomations();

    expect(invokeMock).toHaveBeenCalledWith("automations_load");
    expect(useAutomationsStore.getState().entries).toEqual([entry]);
    expect(useAutomationsStore.getState().persistedEntries).toEqual([entry]);
    expect(useAutomationsStore.getState().hydrated).toBe(true);
  });

  it("keeps the empty default state when nothing has been saved yet", async () => {
    invokeMock.mockResolvedValueOnce(null);

    await hydrateAutomations();

    expect(useAutomationsStore.getState().entries).toEqual([]);
    expect(useAutomationsStore.getState().persistedEntries).toEqual([]);
    expect(useAutomationsStore.getState().hydrated).toBe(true);
  });

  it("surfaces a load failure in persistError instead of throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("disk unavailable"));

    await expect(hydrateAutomations()).resolves.toBeUndefined();

    expect(useAutomationsStore.getState().persistError).toBe("disk unavailable");
    expect(useAutomationsStore.getState().hydrated).toBe(false);
  });

  it("drops a malformed persisted entry (missing recipeName/cron) instead of crashing", async () => {
    invokeMock.mockResolvedValueOnce(JSON.stringify({ version: 1, entries: [{ id: "bad" }, makeEntry()] }));

    await hydrateAutomations();

    expect(useAutomationsStore.getState().entries).toHaveLength(1);
    expect(useAutomationsStore.getState().entries[0].recipeName).toBe("nightly-deps-audit");
  });

  it("fails closed on an invalid saved snapshot so daemon triggers are not erased", async () => {
    invokeMock.mockResolvedValueOnce("{not json");

    await hydrateAutomations();

    expect(useAutomationsStore.getState().hydrated).toBe(false);
    expect(useAutomationsStore.getState().persistError).toContain("left unchanged");
  });
});

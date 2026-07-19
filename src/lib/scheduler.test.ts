import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
const daemonMocks = vi.hoisted(() => ({
  status: vi.fn(),
  synchronize: vi.fn(),
}));
const runRecipeMock = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("./recipeScheduleClient", () => ({
  recipeSchedulerDaemonStatus: daemonMocks.status,
  synchronizeRecipeSchedules: daemonMocks.synchronize,
}));
vi.mock("./recipeRunner", () => ({ runRecipeNow: runRecipeMock }));

import {
  buildRecipeScheduleSyncItems,
  isEntryDue,
  runSchedulerTickForTests,
  stopScheduler,
  synchronizeSchedulerAuthority,
} from "./scheduler";
import type { AutomationEntry } from "../store/automationsStore";
import { useAutomationsStore } from "../store/automationsStore";
import { useRecipeStore, type DiscoveredRecipe } from "../store/recipeStore";
import { useSessionStore } from "../store/sessionStore";

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
  stopScheduler();
  invokeMock.mockReset();
  daemonMocks.status.mockReset();
  daemonMocks.synchronize.mockReset();
  runRecipeMock.mockReset();
  daemonMocks.status.mockResolvedValue({ installed: false, serviceRunning: false });
  daemonMocks.synchronize.mockResolvedValue({
    authority: "in_app",
    installed: false,
    serviceRunning: false,
    synchronizedAtMs: 1,
    activeTriggerIds: [],
    disabledTriggerIds: [],
    issues: [],
    lastDeliveryAtMs: {},
  });
  runRecipeMock.mockResolvedValue({ sessionId: "scheduled-session", done: Promise.resolve() });
  useAutomationsStore.setState({
    entries: [],
    persistedEntries: [],
    hydrated: true,
    persistError: null,
    scheduler: {
      authority: "unknown",
      daemonRunning: false,
      synchronizedAtMs: null,
      syncError: null,
      issues: {},
      lastDeliveryAtMs: {},
    },
  });
  useRecipeStore.setState({ recipes: [], loading: false, error: null });
  useSessionStore.setState({ runningTurns: {} });
});

describe("isEntryDue", () => {
  it("is due when the cron's most recent occurrence falls after the last check", async () => {
    invokeMock.mockResolvedValueOnce(2000);
    await expect(isEntryDue(makeEntry(), 1000)).resolves.toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("cron_previous", { expr: "0 3 * * *" });
  });

  it("is not due when the cron's most recent occurrence is at-or-before the last check", async () => {
    invokeMock.mockResolvedValueOnce(1000);
    await expect(isEntryDue(makeEntry(), 1000)).resolves.toBe(false);

    invokeMock.mockResolvedValueOnce(500);
    await expect(isEntryDue(makeEntry(), 1000)).resolves.toBe(false);
  });

  it("with catchUpIfMissed, compares against lastRunAt instead of the last check", async () => {
    // A schedule missed for days (lastCheckedAtMs is "now", far after the
    // most recent occurrence) still fires because it's never run before.
    invokeMock.mockResolvedValueOnce(500);
    const entry = makeEntry({ catchUpIfMissed: true, lastRunAt: undefined });
    await expect(isEntryDue(entry, 10_000)).resolves.toBe(true);
  });

  it("with catchUpIfMissed, is not due again once lastRunAt is at-or-after the most recent occurrence", async () => {
    invokeMock.mockResolvedValueOnce(500);
    const entry = makeEntry({ catchUpIfMissed: true, lastRunAt: 500 });
    await expect(isEntryDue(entry, 10_000)).resolves.toBe(false);
  });

  it("fails closed (never due) when the cron expression is invalid, instead of throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Invalid cron expression"));
    await expect(isEntryDue(makeEntry({ cron: "garbage" }), 1000)).resolves.toBe(false);
  });

  it("uses a daemon delivery as the due floor to prevent fallback replay", async () => {
    invokeMock.mockResolvedValueOnce(2_000);
    await expect(isEntryDue(makeEntry({ catchUpIfMissed: true }), 1_000, 2_000))
      .resolves.toBe(false);
  });
});

const discovered: DiscoveredRecipe = {
  path: "/workspace/.littlemonkey/recipes/nightly.yml",
  source: "workspace",
  recipe: {
    version: 1,
    name: "nightly-deps-audit",
    target: { ollama: "fixture" },
    permission_mode: "acceptEdits",
    prompt: "Review",
    params: {},
    output: { json: false },
  },
  error: null,
};

describe("recipe schedule authority", () => {
  it("builds a complete snapshot and preserves missing recipes for daemon-side disable", () => {
    const items = buildRecipeScheduleSyncItems([
      makeEntry(),
      makeEntry({ id: "missing", recipeName: "deleted-recipe", enabled: false }),
    ], [discovered]);
    expect(items).toEqual([
      {
        entryId: "entry-1",
        recipeName: "nightly-deps-audit",
        recipePath: discovered.path,
        cron: "0 3 * * *",
        enabled: true,
        permissionModeOverride: null,
      },
      expect.objectContaining({ entryId: "missing", recipePath: null, enabled: false }),
    ]);
  });

  it("reconciles only the last successfully persisted snapshot", async () => {
    const durable = makeEntry({ cron: "0 2 * * *" });
    const optimistic = makeEntry({ cron: "0 4 * * *" });
    useAutomationsStore.setState({
      entries: [optimistic],
      persistedEntries: [durable],
    });
    useRecipeStore.setState({ recipes: [discovered] });

    await synchronizeSchedulerAuthority();

    expect(daemonMocks.synchronize).toHaveBeenCalledWith([
      expect.objectContaining({ entryId: durable.id, cron: "0 2 * * *" }),
    ]);
  });

  it("makes an installed but stopped daemon authoritative and never runs the webview fallback", async () => {
    const entry = makeEntry();
    useAutomationsStore.setState({ entries: [entry], persistedEntries: [entry] });
    useRecipeStore.setState({ recipes: [discovered] });
    daemonMocks.synchronize.mockResolvedValue({
      authority: "daemon",
      installed: true,
      serviceRunning: false,
      synchronizedAtMs: 2,
      activeTriggerIds: ["managed"],
      disabledTriggerIds: [],
      issues: [],
      lastDeliveryAtMs: {},
    });

    await synchronizeSchedulerAuthority();
    await runSchedulerTickForTests();

    expect(useAutomationsStore.getState().scheduler.authority).toBe("daemon");
    expect(useAutomationsStore.getState().scheduler.daemonRunning).toBe(false);
    expect(runRecipeMock).not.toHaveBeenCalled();
  });

  it("runs the in-app fallback only after the backend confirms no daemon is installed", async () => {
    const entry = makeEntry();
    useAutomationsStore.setState({ entries: [entry], persistedEntries: [entry] });
    useRecipeStore.setState({ recipes: [discovered] });
    invokeMock.mockResolvedValue(Number.MAX_SAFE_INTEGER);

    await synchronizeSchedulerAuthority();
    await runSchedulerTickForTests();
    await Promise.resolve();
    await Promise.resolve();

    expect(useAutomationsStore.getState().scheduler.authority).toBe("in_app");
    expect(runRecipeMock).toHaveBeenCalledOnce();
  });

  it("fails closed when authority reconciliation cannot be verified", async () => {
    const entry = makeEntry();
    useAutomationsStore.setState({ entries: [entry], persistedEntries: [entry] });
    useRecipeStore.setState({ recipes: [discovered] });
    daemonMocks.synchronize.mockRejectedValue(new Error("daemon status unavailable"));
    invokeMock.mockResolvedValue(Number.MAX_SAFE_INTEGER);

    await synchronizeSchedulerAuthority();
    await runSchedulerTickForTests();

    expect(useAutomationsStore.getState().scheduler.authority).toBe("unknown");
    expect(useAutomationsStore.getState().scheduler.syncError).toContain("unavailable");
    expect(runRecipeMock).not.toHaveBeenCalled();
  });
});

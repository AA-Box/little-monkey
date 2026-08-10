import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  processUsageLedger: vi.fn(),
  daemonDecisions: vi.fn(),
}));

vi.mock("../lib/processUsage", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/processUsage")>();
  return { ...actual, processUsageLedger: (...args: unknown[]) => mocks.processUsageLedger(...args) };
});
vi.mock("../lib/daemonClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/daemonClient")>();
  return { ...actual, daemonDecisions: (...args: unknown[]) => mocks.daemonDecisions(...args) };
});

import { useResourceLedgerStore } from "./resourceLedgerStore";

beforeEach(() => {
  mocks.processUsageLedger.mockReset();
  mocks.daemonDecisions.mockReset();
  useResourceLedgerStore.setState({
    rows: [], totals: null, closedOnly: true,
    loadingLedger: false, ledgerError: null,
    decisions: [], loadingDecisions: false, decisionsError: null,
  });
});

describe("resource ledger store", () => {
  it("keeps a previous read when there is no backend to read from", async () => {
    // `processUsageLedger` resolves null outside Tauri (dev/browser profile).
    // That is "no backend", not "no usage" and not an error.
    useResourceLedgerStore.setState({ rows: [{ processId: "kept" } as never] });
    mocks.processUsageLedger.mockResolvedValue(null);
    await useResourceLedgerStore.getState().refreshLedger();
    expect(useResourceLedgerStore.getState().rows).toHaveLength(1);
    expect(useResourceLedgerStore.getState().ledgerError).toBeNull();
  });

  it("scopes the read to closed-out rows and follows the toggle", async () => {
    mocks.processUsageLedger.mockResolvedValue({ rows: [], totals: null });
    await useResourceLedgerStore.getState().setClosedOnly(false);
    expect(mocks.processUsageLedger).toHaveBeenCalledWith(expect.objectContaining({ closedOnly: false }));
  });

  it("records a failed decision read instead of reporting an empty log", async () => {
    // The decision log needs a Tauri command that does not exist yet, so this
    // path is live today. An empty list and an unreachable list are different
    // claims about the scheduler, and only one of them is true here.
    mocks.daemonDecisions.mockRejectedValue(new Error("command daemon_desktop_decisions not found"));
    await useResourceLedgerStore.getState().refreshDecisions();
    const state = useResourceLedgerStore.getState();
    expect(state.decisions).toEqual([]);
    expect(state.decisionsError).toContain("daemon_desktop_decisions");
    expect(state.loadingDecisions).toBe(false);
  });
});

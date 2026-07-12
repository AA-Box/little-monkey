import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

import { isEntryDue } from "./scheduler";
import type { AutomationEntry } from "../store/automationsStore";

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
});

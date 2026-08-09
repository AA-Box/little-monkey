import { describe, expect, it } from "vitest";

import {
  CostBudgetExceededError,
  assertCostBudgetAllowsRequest,
  attributeCost,
  calculateUsageCostUsd,
  compareBillingForMonth,
  evaluateCostBudget,
  monthKey,
  providerIdOfTargetKey,
  sanitizeWarningPercents,
  type CostBudgetPolicy,
  type CostUsageEntry,
} from "./costControlStore";
import { providerModelTargetKey } from "../lib/modelTargets";

const policy: CostBudgetPolicy = {
  enabled: true,
  dailyBudgetUsd: 1,
  monthlyBudgetUsd: 5,
  warningPercents: [0.5, 0.8, 0.95],
  enforcement: "pause",
};

function entry(
  occurredAtMs: number,
  costUsd: number | null,
  id = crypto.randomUUID(),
): CostUsageEntry {
  return {
    id,
    occurredAtMs,
    targetKey: "provider:openai:gpt",
    targetLabel: "OpenAI · GPT",
    sessionId: "session",
    runId: null,
    usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
    costUsd,
  };
}

describe("cost controls", () => {
  it("calculates input and output pricing without inventing a cost", () => {
    expect(
      calculateUsageCostUsd(
        { inputPerMillionUsd: 2, outputPerMillionUsd: 8 },
        { promptTokens: 500_000, completionTokens: 250_000, totalTokens: 750_000 },
      ),
    ).toBe(3);
    expect(
      calculateUsageCostUsd(
        undefined,
        { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
      ),
    ).toBeNull();
  });

  it("uses local calendar day/month boundaries and reports unknown calls", () => {
    const now = new Date(2026, 6, 30, 12).getTime();
    const today = new Date(2026, 6, 30, 9).getTime();
    const yesterday = new Date(2026, 6, 29, 23).getTime();
    const result = evaluateCostBudget(
      policy,
      [entry(today, 0.81), entry(today, null), entry(yesterday, 3)],
      now,
    );
    expect(result.status).toBe("warning");
    // Both windows warn, at different tiers: $0.81 of a $1 day is past 80%,
    // $3.81 of a $5 month is past 50% but not 80%.
    expect(result.warningWindows).toEqual(["daily", "monthly"]);
    expect(result.tiers).toEqual({ daily: 0.8, monthly: 0.5 });
    expect(result.highestTier).toBe(0.8);
    expect(result.daily).toEqual({ spentUsd: 0.81, unknownCalls: 1, knownCalls: 1 });
    expect(result.monthly.spentUsd).toBeCloseTo(3.81);
  });

  it("hard-pauses only when enforcement is pause and a configured limit is reached", () => {
    const now = new Date(2026, 6, 30, 12).getTime();
    const entries = [entry(new Date(2026, 6, 30, 9).getTime(), 1)];
    expect(() =>
      assertCostBudgetAllowsRequest({ policy, entries }, now),
    ).toThrow(CostBudgetExceededError);
    expect(() =>
      assertCostBudgetAllowsRequest({
        policy: { ...policy, enforcement: "warn" },
        entries,
      }, now),
    ).not.toThrow();
  });

  it("does not claim a budget is safe when accounting has unknown-priced calls", () => {
    const now = new Date(2026, 6, 30, 12).getTime();
    const result = evaluateCostBudget(
      policy,
      [entry(new Date(2026, 6, 30, 9).getTime(), null)],
      now,
    );
    expect(result.status).toBe("ok");
    expect(result.daily.unknownCalls).toBe(1);
  });
});

describe("multi-tier warning thresholds (K25)", () => {
  it("keeps tiers ascending, deduped and clamped, and never empties them", () => {
    expect(sanitizeWarningPercents([0.95, 0.5, 0.5, 0.8])).toEqual([0.5, 0.8, 0.95]);
    expect(sanitizeWarningPercents([5, -1])).toEqual([0.1, 0.99]);
    expect(sanitizeWarningPercents([])).toEqual([0.5, 0.8, 0.95]);
    expect(sanitizeWarningPercents(["nonsense"])).toEqual([0.5, 0.8, 0.95]);
  });

  it("migrates a pre-K25 single threshold instead of reverting to the defaults", () => {
    expect(sanitizeWarningPercents(undefined, 0.6)).toEqual([0.6]);
  });

  it("reports the highest tier crossed, not the first", () => {
    const now = new Date(2026, 6, 30, 12).getTime();
    const at = new Date(2026, 6, 30, 9).getTime();
    // $0.96 of a $1 day is past all three tiers; reporting 50% for the whole
    // climb is what a single-threshold policy could not distinguish.
    expect(evaluateCostBudget(policy, [entry(at, 0.96)], now).tiers.daily).toBe(0.95);
    expect(evaluateCostBudget(policy, [entry(at, 0.55)], now).tiers.daily).toBe(0.5);
    expect(evaluateCostBudget(policy, [entry(at, 0.1)], now).tiers.daily).toBeNull();
  });

  it("still reports how far past a warning tier an exceeded window went", () => {
    const now = new Date(2026, 6, 30, 12).getTime();
    const result = evaluateCostBudget(
      policy,
      [entry(new Date(2026, 6, 30, 9).getTime(), 2)],
      now,
    );
    expect(result.status).toBe("exceeded");
    expect(result.tiers.daily).toBe(0.95);
    expect(result.warningWindows).toEqual([]);
  });
});

describe("per-workspace and per-project attribution (K25)", () => {
  function attributed(
    costUsd: number | null,
    workspacePath: string | null,
    projectPath: string | null,
  ): CostUsageEntry {
    return { ...entry(1_000, costUsd), workspacePath, projectPath };
  }

  it("splits spend by workspace and by project independently", () => {
    const entries = [
      attributed(1, "/work/alpha", "/work/alpha"),
      attributed(2, "/work/beta", "/work/alpha"),
      attributed(4, "/work/beta", "/work/beta"),
    ];
    expect(attributeCost(entries, "workspace").map((row) => [row.key, row.spentUsd])).toEqual([
      ["/work/beta", 6],
      ["/work/alpha", 1],
    ]);
    expect(attributeCost(entries, "project").map((row) => [row.key, row.spentUsd])).toEqual([
      ["/work/beta", 4],
      ["/work/alpha", 3],
    ]);
  });

  it("buckets unattributed spend rather than dropping it or charging a folder", () => {
    const rows = attributeCost(
      [attributed(1, "/work/alpha", null), attributed(3, null, null)],
      "workspace",
    );
    expect(rows.map((row) => [row.key, row.spentUsd])).toEqual([
      ["", 3],
      ["/work/alpha", 1],
    ]);
    // The total over every bucket is the total that was recorded — nothing is
    // silently excluded for having no folder.
    expect(rows.reduce((sum, row) => sum + row.spentUsd, 0)).toBe(4);
  });

  it("counts unpriced calls per bucket without folding them into spend", () => {
    const [row] = attributeCost(
      [attributed(null, "/work/alpha", null), attributed(2, "/work/alpha", null)],
      "workspace",
    );
    expect(row).toMatchObject({ key: "/work/alpha", spentUsd: 2, knownCalls: 1, unknownCalls: 1 });
  });
});

describe("billing reconciliation (K25)", () => {
  const month = monthKey(new Date(2026, 6, 30, 9).getTime());

  function providerEntry(providerId: string, costUsd: number | null): CostUsageEntry {
    return {
      ...entry(new Date(2026, 6, 30, 9).getTime(), costUsd),
      targetKey: providerModelTargetKey(providerId, "some-model"),
    };
  }

  it("reads the provider out of a percent-encoded target key", () => {
    expect(providerIdOfTargetKey(providerModelTargetKey("open:ai", "gpt"))).toBe("open:ai");
    expect(providerIdOfTargetKey("ollama:llama3")).toBeNull();
    expect(providerIdOfTargetKey("local:whatever")).toBeNull();
  });

  it("shows the drift between an estimate and an entered bill without rewriting the estimate", () => {
    const entries = [providerEntry("openai", 3), providerEntry("openai", null)];
    const [row] = compareBillingForMonth(
      entries,
      {
        "openai 2026-07": {
          providerId: "openai",
          month,
          actualBilledUsd: 4,
          recordedAtMs: 0,
        },
      },
      month,
    );
    expect(row).toMatchObject({
      providerId: "openai",
      estimatedUsd: 3,
      unknownCalls: 1,
      actualBilledUsd: 4,
      driftUsd: 1,
    });
    expect(row.driftFraction).toBeCloseTo(0.25);
    // The per-call figure is untouched: a monthly total cannot be split back
    // across the calls that produced it.
    expect(entries[0].costUsd).toBe(3);
  });

  it("reports a bill for a provider the app estimated nothing for", () => {
    const rows = compareBillingForMonth(
      [],
      {
        "anthropic 2026-07": {
          providerId: "anthropic",
          month,
          actualBilledUsd: 40,
          recordedAtMs: 0,
        },
      },
      month,
    );
    expect(rows).toEqual([
      {
        providerId: "anthropic",
        estimatedUsd: 0,
        unknownCalls: 0,
        knownCalls: 0,
        actualBilledUsd: 40,
        driftUsd: 40,
        driftFraction: 1,
      },
    ]);
  });

  it("leaves drift unknown when no bill was entered", () => {
    const [row] = compareBillingForMonth([providerEntry("openai", 3)], {}, month);
    expect(row.actualBilledUsd).toBeNull();
    expect(row.driftUsd).toBeNull();
    expect(row.driftFraction).toBeNull();
  });
});

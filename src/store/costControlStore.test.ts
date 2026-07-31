import { describe, expect, it } from "vitest";

import {
  CostBudgetExceededError,
  assertCostBudgetAllowsRequest,
  calculateUsageCostUsd,
  evaluateCostBudget,
  type CostBudgetPolicy,
  type CostUsageEntry,
} from "./costControlStore";

const policy: CostBudgetPolicy = {
  enabled: true,
  dailyBudgetUsd: 1,
  monthlyBudgetUsd: 5,
  warningPercent: 0.8,
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
    expect(result.warningWindows).toEqual(["daily"]);
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

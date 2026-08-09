import { describe, expect, it } from "vitest";

import { accountByWorkspace, foldUsageRows, workspaceDisplayName } from "./workspaceAccounting";
import type { ProcessUsageRow } from "./processUsage";
import type { CostUsageEntry } from "../store/costControlStore";

function costEntry(workspacePath: string | null, costUsd: number | null): CostUsageEntry {
  return {
    id: crypto.randomUUID(),
    occurredAtMs: 1_000,
    targetKey: "provider:openai:gpt",
    targetLabel: "OpenAI · GPT",
    sessionId: "session",
    runId: null,
    usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
    costUsd,
    workspacePath,
    projectPath: workspacePath,
  };
}

/** A ledger row with only the fields this module reads; `unavailable` carries
 * the reason for every gap, exactly as the Rust side guarantees. */
function usageRow(
  workspace: string | null,
  measured: Partial<{ cpuTimeMs: number; gpuDeviceMs: number; peakRssBytes: number }>,
  wallTimeMs: number | null = null,
): ProcessUsageRow {
  const fields = [
    "cpuTimeMs",
    "peakRssBytes",
    "bytesRead",
    "bytesWritten",
    "bytesEgressed",
    "tokensIn",
    "tokensOut",
    "gpuResidentBytes",
    "gpuDeviceMs",
  ] as const;
  const usage = Object.fromEntries(fields.map((field) => [field, null])) as Record<
    string,
    number | null
  >;
  Object.assign(usage, measured);
  return {
    processId: crypto.randomUUID(),
    kind: "background_shell" as ProcessUsageRow["kind"],
    externalId: "x",
    runId: null,
    workspace,
    state: "exited" as ProcessUsageRow["state"],
    exitStatus: null,
    wallTimeMs,
    usage: {
      ...(usage as unknown as ProcessUsageRow["usage"]),
      unavailable: [
        ...fields
          .filter((field) => usage[field] === null)
          .map((field) => ({ field: field as string, reason: "not measured on this platform" })),
        ...(wallTimeMs === null ? [{ field: "wallTimeMs", reason: "still running" }] : []),
      ],
    },
  };
}

describe("foldUsageRows", () => {
  it("sums consumption and takes maxima of footprints, mirroring the Rust fold", () => {
    const totals = foldUsageRows([
      usageRow(null, { cpuTimeMs: 100, peakRssBytes: 10 }, 200),
      usageRow(null, { cpuTimeMs: 50, peakRssBytes: 40 }, 300),
    ]);
    expect(totals.cpuTimeMs).toEqual({ value: 150, measuredRows: 2, unavailableRows: 0 });
    expect(totals.wallTimeMs.value).toBe(500);
    // A peak is a maximum: adding two of them invents a moment nothing observed.
    expect(totals.peakRssBytes.value).toBe(40);
  });

  it("keeps a total unmeasured rather than reporting zero, and counts the rows that could not contribute", () => {
    const totals = foldUsageRows([
      usageRow(null, { cpuTimeMs: 100 }),
      usageRow(null, {}, 10),
    ]);
    expect(totals.gpuDeviceMs).toEqual({ value: null, measuredRows: 0, unavailableRows: 2 });
    expect(totals.cpuTimeMs).toEqual({ value: 100, measuredRows: 1, unavailableRows: 1 });
    expect(totals.rows).toBe(2);
  });
});

describe("accountByWorkspace", () => {
  it("joins a workspace's token bill to its device time on the same path", () => {
    const rows = accountByWorkspace(
      [costEntry("/work/alpha", 2)],
      [usageRow("/work/alpha", { cpuTimeMs: 900 }, 1_200)],
    );
    expect(rows).toHaveLength(1);
    expect(rows[0].key).toBe("/work/alpha");
    expect(rows[0].cost?.spentUsd).toBe(2);
    expect(rows[0].device?.cpuTimeMs.value).toBe(900);
  });

  it("keeps a workspace that only spent device time and one that only spent tokens", () => {
    const rows = accountByWorkspace(
      [costEntry("/work/alpha", 2)],
      [usageRow("/work/beta", { cpuTimeMs: 5 }, 5)],
    );
    expect(rows.map((row) => [row.key, row.cost !== null, row.device !== null])).toEqual([
      ["/work/alpha", true, false],
      ["/work/beta", false, true],
    ]);
  });

  it("sorts the unattributed bucket last however large it is", () => {
    const rows = accountByWorkspace(
      [costEntry(null, 100), costEntry("/work/alpha", 1)],
      [usageRow(null, { cpuTimeMs: 5 }, 5)],
    );
    expect(rows.map((row) => row.key)).toEqual(["/work/alpha", ""]);
    expect(rows[1].cost?.spentUsd).toBe(100);
    expect(rows[1].device?.rows).toBe(1);
  });

  it("does not pretend process rows belong to a project scope they never recorded", () => {
    const rows = accountByWorkspace(
      [costEntry("/work/alpha", 1)],
      [usageRow("/work/alpha", { cpuTimeMs: 5 }, 5)],
      "project",
    );
    // Grouping by project must not attach every process to "unattributed" —
    // that would read as a claim that nothing on the machine was measured.
    expect(rows.map((row) => [row.key, row.device])).toEqual([["/work/alpha", null]]);
  });

  it("does not merge a subdirectory into its parent workspace", () => {
    const rows = accountByWorkspace(
      [costEntry("/work/alpha", 1)],
      [usageRow("/work/alpha/packages/api", { cpuTimeMs: 5 }, 5)],
    );
    expect(rows.map((row) => row.key)).toEqual(["/work/alpha", "/work/alpha/packages/api"]);
  });
});

describe("workspaceDisplayName", () => {
  it("names the folder without losing the path it came from", () => {
    expect(workspaceDisplayName("/work/alpha")).toBe("alpha");
    expect(workspaceDisplayName("/work/alpha/")).toBe("alpha");
    expect(workspaceDisplayName("C:\\work\\alpha")).toBe("alpha");
    expect(workspaceDisplayName("alpha")).toBe("alpha");
  });
});

import { describe, expect, it } from "vitest";

import {
  contextHitRate,
  contextReuseFor,
  destinationsFor,
  measurementOf,
  renderMeasurement,
  renderTotal,
  totalIsPartial,
  USAGE_FIELDS,
  type ProcessUsageRow,
  type ProcessUsageTotal,
} from "./processUsage";

const UNEXPLAINED = "unavailable, and the ledger recorded no reason";

function row(overrides: Partial<ProcessUsageRow> = {}): ProcessUsageRow {
  return {
    processId: "proc-1",
    kind: "chat_turn",
    externalId: "turn-1",
    runId: null,
    workspace: null,
    state: "exited",
    exitStatus: "succeeded",
    wallTimeMs: 4_000,
    usage: {
      cpuTimeMs: 1_200,
      peakRssBytes: 64 * 1024 * 1024,
      bytesRead: 2_048,
      bytesWritten: 512,
      bytesEgressed: null,
      tokensIn: 900,
      tokensOut: 120,
      gpuResidentBytes: null,
      gpuDeviceMs: null,
      unavailable: [
        { field: "bytesEgressed", reason: "no egress was attributed to this process" },
        { field: "gpuResidentBytes", reason: "no runtime in this build reports GPU residency" },
        { field: "gpuDeviceMs", reason: "no runtime in this build reports GPU device time" },
      ],
    },
    ...overrides,
  };
}

function total(overrides: Partial<ProcessUsageTotal> = {}): ProcessUsageTotal {
  return { value: 100, measuredRows: 3, unavailableRows: 0, ...overrides };
}

describe("resource ledger measurements", () => {
  it("covers every wire field the ledger reports", () => {
    // Guards the one failure mode a rename would cause silently: a field spec
    // whose name no longer matches `process_usage.rs`'s `FIELD_*` consts reads
    // as unavailable forever, because no note will ever match it either.
    expect(USAGE_FIELDS.map((spec) => spec.field)).toEqual([
      "wallTimeMs",
      "cpuTimeMs",
      "peakRssBytes",
      "bytesRead",
      "bytesWritten",
      "bytesEgressed",
      "tokensIn",
      "tokensOut",
      "gpuResidentBytes",
      "gpuDeviceMs",
    ]);
  });

  it("reads wall time from the row and its gap reason from the usage notes", () => {
    // `wallTimeMs` is derived and lives beside `usage`, but its reason lives
    // inside `usage.unavailable` like every other gap.
    const live = row({
      state: "running",
      exitStatus: null,
      wallTimeMs: null,
      usage: {
        ...row().usage,
        unavailable: [
          ...row().usage.unavailable,
          { field: "wallTimeMs", reason: "this process has not exited, so its wall time is not final" },
        ],
      },
    });
    expect(measurementOf(live, "wallTimeMs")).toEqual({
      field: "wallTimeMs",
      value: null,
      reason: "this process has not exited, so its wall time is not final",
    });
    expect(measurementOf(row(), "wallTimeMs")).toEqual({ field: "wallTimeMs", value: 4_000, reason: null });
  });

  it("renders a null measurement as unavailable with its reason, never as 0", () => {
    const rendered = renderMeasurement(measurementOf(row(), "bytesEgressed"), "bytes", UNEXPLAINED);
    expect(rendered.available).toBe(false);
    expect(rendered).toEqual({
      available: false,
      reason: "no egress was attributed to this process",
    });
    // The whole point: nothing in the rendered output can be read as a number.
    expect(JSON.stringify(rendered)).not.toMatch(/\b0\b/);
    expect(JSON.stringify(rendered)).not.toContain("0 B");
  });

  it("still refuses to invent a zero when the reason is missing", () => {
    // Rust makes this unconstructible, but a contract the UI depends on is
    // worth failing safe on: unexplained still means unavailable.
    const orphan = row({
      usage: { ...row().usage, bytesRead: null },
    });
    expect(renderMeasurement(measurementOf(orphan, "bytesRead"), "bytes", UNEXPLAINED)).toEqual({
      available: false,
      reason: UNEXPLAINED,
    });
  });

  it("renders a real zero measurement as zero", () => {
    // The inverse of the rule, and the reason the rule matters: a measured 0 is
    // a fact and must not be hidden behind "unavailable".
    const measured = row({
      usage: {
        ...row().usage,
        bytesEgressed: 0,
        unavailable: row().usage.unavailable.filter((note) => note.field !== "bytesEgressed"),
      },
    });
    expect(renderMeasurement(measurementOf(measured, "bytesEgressed"), "bytes", UNEXPLAINED)).toEqual({
      available: true,
      text: "0 B",
    });
  });

  it("reports an unmeasured total as unknown rather than zero", () => {
    expect(renderTotal(total({ value: null, measuredRows: 0, unavailableRows: 10 }), "bytes", "no total")).toEqual({
      available: false,
      reason: "no total",
    });
  });

  it("flags a total that could not read every row", () => {
    expect(totalIsPartial(total({ measuredRows: 3, unavailableRows: 7 }))).toBe(true);
    expect(totalIsPartial(total({ measuredRows: 10, unavailableRows: 0 }))).toBe(false);
  });
});

describe("egress destinations", () => {
  const reached = {
    destinations: [
      { scheme: "https", host: "api.example.com", port: 443, requests: 5, firstSeenMs: 1, lastSeenMs: 2 },
    ],
    dropped: 0,
  };

  it("returns null for a process the ledger recorded nothing for", () => {
    // The distinction this whole surface rests on: an absent key means nothing
    // was recorded, which is not the same claim as "this process reached
    // nowhere" — so the caller gets null and renders nothing at all.
    expect(destinationsFor({ destinations: {} }, "p-1")).toBeNull();
    expect(destinationsFor(null, "p-1")).toBeNull();
    expect(destinationsFor({ destinations: { "p-1": { destinations: [], dropped: 0 } } }, "p-1")).toBeNull();
  });

  it("returns the record when anything at all was recorded, including only drops", () => {
    expect(destinationsFor({ destinations: { "p-1": reached } }, "p-1")).toBe(reached);
    // A process whose every destination fell past the cap still has something
    // true to say, and hiding it would under-report the traffic entirely.
    const onlyDropped = { destinations: [], dropped: 12 };
    expect(destinationsFor({ destinations: { "p-1": onlyDropped } }, "p-1")).toBe(onlyDropped);
  });
});

describe("measured prompt-cache reuse", () => {
  it("reports no rate for a runtime that reported no figure", () => {
    // Ollama and MLX report nothing, so these three cases must all read as "not
    // measured" rather than as a 0% hit rate — the one claim this app must never
    // make on a runtime's behalf.
    expect(contextReuseFor({ contextReuse: {} }, "p-1")).toBeNull();
    expect(contextReuseFor(null, "p-1")).toBeNull();
    expect(
      contextReuseFor({ contextReuse: { "p-1": { reusedTokens: 0, evaluatedTokens: 0 } } }, "p-1"),
    ).toBeNull();
    expect(contextHitRate(null)).toBeNull();
    expect(contextHitRate({ reusedTokens: 0, evaluatedTokens: 0 })).toBeNull();
  });

  it("reports a measured zero as a rate, because that is a measurement", () => {
    const cold = { reusedTokens: 0, evaluatedTokens: 1_000 };
    expect(contextReuseFor({ contextReuse: { "p-1": cold } }, "p-1")).toBe(cold);
    expect(contextHitRate(cold)).toBe(0);
  });

  it("weighs the rate by tokens rather than by turn", () => {
    // A 1000-token cold turn and a 10-token warm one: the process reused 9 of
    // 1010 prompt tokens. Averaging the two turns' own rates would say 45%.
    expect(contextHitRate({ reusedTokens: 9, evaluatedTokens: 1_001 })).toBeCloseTo(9 / 1010);
  });
});

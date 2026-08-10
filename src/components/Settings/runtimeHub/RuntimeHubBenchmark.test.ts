import { describe, expect, it } from "vitest";

import type { BenchmarkSample, BenchmarkSpread } from "../../../lib/runtimeHubClient";
import {
  measuredPeakMemory,
  measuredRate,
  measuredSpread,
  measuredStddev,
  measuredValue,
  unavailableReason,
} from "./RuntimeHubBenchmark";

function sample(overrides: Partial<BenchmarkSample["timings"]> = {}, rate: number | null = 42.5): BenchmarkSample {
  return {
    repeat: 1,
    warmup: false,
    timings: {
      totalMs: 1200,
      timeToFirstTokenMs: 200,
      decodeMs: 1000,
      inputTokens: 12,
      outputTokens: 128,
      error: null,
      unavailable: [],
      ...overrides,
    },
    decodeTokensPerSecond: rate,
  };
}

const spread: BenchmarkSpread = { n: 4, min: 180, median: 200, max: 260, stddev: 34.5 };

describe("measuredSpread", () => {
  it("renders the median between its min and max when the spread was measured", () => {
    const rendered = measuredSpread(spread, [], "timeToFirstTokenMs", "ms");
    expect(rendered).toEqual({ measured: true, text: "200 ms (min 180 ms, max 260 ms, n=4)" });
  });

  it("renders the matching note's reason when there is no spread", () => {
    const rendered = measuredSpread(
      null,
      [
        { field: "decodeTokensPerSecond", reason: "the wrong field" },
        { field: "timeToFirstTokenMs", reason: "every repeat was a discarded warm-up" },
      ],
      "timeToFirstTokenMs",
      "ms",
    );
    expect(rendered.measured).toBe(false);
    expect(rendered).not.toHaveProperty("text");
    if (rendered.measured) throw new Error("unreachable");
    expect(rendered.reason).toBe("every repeat was a discarded warm-up");
  });

  it("still gives a real sentence when no note matches the field", () => {
    const rendered = measuredSpread(null, [{ field: "somethingElse", reason: "unrelated" }], "decodeTokensPerSecond", "tok/s");
    if (rendered.measured) throw new Error("expected the unavailable branch");
    expect(rendered.reason).not.toBe("");
    expect(rendered.reason).toContain("decodeTokensPerSecond");
    expect(rendered.reason).toContain("no reason");
  });

  it("gives a real sentence with an empty note list", () => {
    const rendered = measuredSpread(null, [], "timeToFirstTokenMs", "ms");
    if (rendered.measured) throw new Error("expected the unavailable branch");
    expect(rendered.reason.length).toBeGreaterThan(20);
  });
});

describe("measuredStddev", () => {
  it("renders the deviation when more than one repeat was counted", () => {
    expect(measuredStddev(spread)).toEqual({ measured: true, text: "± 34.5" });
  });

  it("never renders a single repeat's absent spread as zero", () => {
    const rendered = measuredStddev({ n: 1, min: 200, median: 200, max: 200, stddev: null });
    expect(rendered.measured).toBe(false);
    expect(rendered).not.toHaveProperty("text");
    if (rendered.measured) throw new Error("unreachable");
    expect(rendered.reason).toContain("single repeat has no spread");
    expect(rendered.reason).not.toContain("0");
  });
});

describe("measuredPeakMemory", () => {
  it("labels the run's own peak only when this run raised the mark", () => {
    const rendered = measuredPeakMemory({
      processLifetimePeakBytes: 2_000_000_000,
      beforeBytes: 1_000_000_000,
      runPeakBytes: 2_000_000_000,
      unavailable: [],
    });
    if (!rendered.measured) throw new Error("expected the measured branch");
    expect(rendered.text).toContain("peak for this run");
  });

  it("renders an unraised lifetime mark as an upper bound, never as this run's peak", () => {
    const rendered = measuredPeakMemory({
      processLifetimePeakBytes: 2_000_000_000,
      beforeBytes: 2_000_000_000,
      runPeakBytes: null,
      unavailable: [
        {
          field: "runPeakRssBytes",
          reason: "pid 42's high-water mark did not rise during this run, so its peak was set earlier",
        },
      ],
    });
    if (!rendered.measured) throw new Error("expected the bounded branch");
    expect(rendered.text).toContain("at most");
    expect(rendered.text).toContain("this run did not raise");
    expect(rendered.text).toContain("set earlier");
    expect(rendered.text).not.toContain("peak for this run");
  });

  it("renders the reason alone when there is no mark at all", () => {
    const rendered = measuredPeakMemory({
      processLifetimePeakBytes: null,
      beforeBytes: null,
      runPeakBytes: null,
      unavailable: [{ field: "runPeakRssBytes", reason: "this runtime does not host a local process" }],
    });
    expect(rendered.measured).toBe(false);
    expect(rendered).not.toHaveProperty("text");
    if (rendered.measured) throw new Error("unreachable");
    expect(rendered.reason).toBe("this runtime does not host a local process");
  });

  it("gives a real sentence when the gap carries no note", () => {
    const rendered = measuredPeakMemory({
      processLifetimePeakBytes: null,
      beforeBytes: null,
      runPeakBytes: null,
      unavailable: [],
    });
    if (rendered.measured) throw new Error("expected the unavailable branch");
    expect(rendered.reason).toContain("runPeakRssBytes");
  });
});

describe("measuredRate", () => {
  it("renders the rate for a healthy repeat", () => {
    expect(measuredRate(sample())).toEqual({ measured: true, text: "42.5 tok/s" });
  });

  it("renders an errored repeat's error rather than a zero rate", () => {
    const rendered = measuredRate(sample({ error: "the runtime closed the stream after 2 tokens" }, null));
    expect(rendered.measured).toBe(false);
    expect(rendered).not.toHaveProperty("text");
    if (rendered.measured) throw new Error("unreachable");
    expect(rendered.reason).toBe("the runtime closed the stream after 2 tokens");
  });

  it("prefers the error even when a rate somehow accompanies it", () => {
    const rendered = measuredRate(sample({ error: "cancelled" }, 0));
    if (rendered.measured) throw new Error("expected the unavailable branch");
    expect(rendered.reason).toBe("cancelled");
  });

  it("renders the note when the rate is absent without an error", () => {
    const rendered = measuredRate(
      sample(
        { unavailable: [{ field: "decodeTokensPerSecond", reason: "one token cannot time a decode window" }] },
        null,
      ),
    );
    if (rendered.measured) throw new Error("expected the unavailable branch");
    expect(rendered.reason).toBe("one token cannot time a decode window");
  });
});

describe("measuredValue", () => {
  it("renders a number with its unit", () => {
    expect(measuredValue(128, [], "outputTokens", "tokens")).toEqual({ measured: true, text: "128 tokens" });
  });

  it("renders the note for a null, and never a zero", () => {
    const rendered = measuredValue(
      null,
      [{ field: "outputTokens", reason: "the runtime completed without reporting usage" }],
      "outputTokens",
      "tokens",
    );
    expect(rendered).toEqual({ measured: false, reason: "the runtime completed without reporting usage" });
  });
});

describe("unavailableReason", () => {
  it("never returns an empty string, even for a note with a blank reason", () => {
    expect(unavailableReason([{ field: "decodeMs", reason: "   " }], "decodeMs")).toContain("decodeMs");
    expect(unavailableReason([], "decodeMs")).not.toBe("");
  });
});

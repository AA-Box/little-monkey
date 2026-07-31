import { describe, expect, it } from "vitest";

import { formatBytes, formatDuration, formatTimestamp } from "./format";

describe("formatBytes", () => {
  it("keeps whole bytes whole and scales by 1024", () => {
    expect(formatBytes(0)).toBe("0 B");
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1024)).toBe("1.00 KB");
    expect(formatBytes(1024 * 1024)).toBe("1.00 MB");
  });

  it("drops to one decimal once the scaled value reaches double digits", () => {
    expect(formatBytes(10 * 1024)).toBe("10.0 KB");
    expect(formatBytes(9.5 * 1024)).toBe("9.50 KB");
  });

  it("renders the caller's placeholder for absent or non-finite input", () => {
    expect(formatBytes(null)).toBe("—");
    expect(formatBytes(undefined)).toBe("—");
    expect(formatBytes(Number.NaN)).toBe("—");
    expect(formatBytes(null, { fallback: "0 B" })).toBe("0 B");
  });

  it("clamps to the largest known unit rather than inventing one", () => {
    expect(formatBytes(1024 ** 6)).toMatch(/PB$/);
  });
});

describe("formatDuration", () => {
  it("keeps sub-second resolution in the default precise style", () => {
    expect(formatDuration(840)).toBe("840 ms");
    expect(formatDuration(2_400)).toBe("2.4 s");
    // Past ten seconds the decimal is noise, matching the previous copies.
    expect(formatDuration(42_000)).toBe("42 s");
    expect(formatDuration(185_000)).toBe("3m 5s");
  });

  it("collapses accumulated totals in the coarse style", () => {
    expect(formatDuration(45_000, { style: "coarse" })).toBe("45s");
    expect(formatDuration(12 * 60_000, { style: "coarse" })).toBe("12m");
    expect(formatDuration(3_900_000, { style: "coarse" })).toBe("1h 5m");
    expect(formatDuration(0, { style: "coarse", fallback: "0m" })).toBe("0m");
  });

  it("rejects absent and negative input via the placeholder", () => {
    expect(formatDuration(null)).toBe("—");
    expect(formatDuration(-1)).toBe("—");
    expect(formatDuration(Number.POSITIVE_INFINITY)).toBe("—");
  });
});

describe("formatTimestamp", () => {
  it("renders a real epoch value in the local locale", () => {
    // Exact text is locale-dependent by design; assert it is non-placeholder
    // and reflects the supplied instant rather than 'now'.
    const rendered = formatTimestamp(Date.UTC(2024, 0, 15, 12, 0, 0));
    expect(rendered).not.toBe("—");
    expect(rendered).toMatch(/2024/);
  });

  it("treats 0, null, and undefined as 'no timestamp'", () => {
    expect(formatTimestamp(0)).toBe("—");
    expect(formatTimestamp(null)).toBe("—");
    expect(formatTimestamp(undefined)).toBe("—");
  });

  it("includes seconds only when medium precision is requested", () => {
    const instant = Date.UTC(2024, 0, 15, 12, 34, 56);
    expect(formatTimestamp(instant, { timeStyle: "medium" }).length).toBeGreaterThan(
      formatTimestamp(instant).length,
    );
  });
});

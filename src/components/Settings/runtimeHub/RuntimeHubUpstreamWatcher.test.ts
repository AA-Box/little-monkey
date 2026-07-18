import { describe, expect, it } from "vitest";

import { describeCheckResult } from "./RuntimeHubUpstreamWatcher";

describe("Runtime Hub upstream watcher helpers", () => {
  it("reports that GitHub returned nothing when the scan found no closed pull requests", () => {
    expect(describeCheckResult(0, 0)).toBe("GitHub returned no closed pull requests for this query.");
  });

  it("uses singular wording for exactly one scanned pull request and one newly relevant result", () => {
    expect(describeCheckResult(1, 1)).toBe("Scanned 1 closed pull request — 1 was newly relevant.");
  });

  it("reports zero newly relevant results without implying the check failed", () => {
    expect(describeCheckResult(90, 0)).toBe("Scanned 90 closed pull requests — none were newly relevant.");
  });

  it("pluralizes newly relevant results correctly", () => {
    expect(describeCheckResult(30, 3)).toBe("Scanned 30 closed pull requests — 3 were newly relevant.");
  });
});

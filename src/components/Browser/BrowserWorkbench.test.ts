import { describe, expect, it } from "vitest";
import { buildBrowserEvidenceSummary, sanitizeWorkbenchRunId } from "./BrowserWorkbench";

describe("browser workbench evidence boundary", () => {
  it("creates a backend-safe task run id", () => {
    expect(sanitizeWorkbenchRunId("session / with secrets?"))
      .toBe("browser-workbench-session---with-secrets-");
    expect(sanitizeWorkbenchRunId("🔥")).toBe("browser-workbench---");
  });

  it("labels page evidence as untrusted and keeps the prompt bounded", () => {
    const summary = buildBrowserEvidenceSummary({
      url: "https://example.com/test",
      viewport: { width: 390, height: 844, deviceScaleFactor: 3, mobile: true },
      evidence: {
        screenshot: { id: "sha256:shot", size: 10 },
        dom: { id: "sha256:dom", size: 10 },
        accessibility: null,
        console: null,
        network: null,
        performance: null,
        actionCount: 3,
      },
      consoleExcerpt: "x".repeat(20_000),
    });
    expect(summary).toContain("Untrusted browser evidence");
    expect(summary).toContain("390x844");
    expect(summary.length).toBeLessThanOrEqual(12_000);
  });
});

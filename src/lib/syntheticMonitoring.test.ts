import { beforeEach, describe, expect, it, vi } from "vitest";

const startBrowserSessionMock = vi.fn();
const annotateBrowserMock = vi.fn();
const clickBrowserMock = vi.fn();
const inspectBrowserMock = vi.fn();
const captureBrowserEvidenceMock = vi.fn();
const stopBrowserSessionMock = vi.fn();

vi.mock("./browserVerification", async () => {
  const actual = await vi.importActual<typeof import("./browserVerification")>("./browserVerification");
  return {
    ...actual,
    startBrowserSession: (...args: unknown[]) => startBrowserSessionMock(...args),
    annotateBrowser: (...args: unknown[]) => annotateBrowserMock(...args),
    clickBrowser: (...args: unknown[]) => clickBrowserMock(...args),
    inspectBrowser: (...args: unknown[]) => inspectBrowserMock(...args),
    captureBrowserEvidence: (...args: unknown[]) => captureBrowserEvidenceMock(...args),
    stopBrowserSession: (...args: unknown[]) => stopBrowserSessionMock(...args),
  };
});

const readDurableArtifactMock = vi.fn();
vi.mock("./durableArtifacts", () => ({
  readDurableArtifact: (...args: unknown[]) => readDurableArtifactMock(...args),
}));

import {
  assertMonitorCondition,
  assertMonitorUrl,
  buildDiagnosisMessages,
  buildEvidenceExcerpt,
  createMonitor,
  diagnoseMonitorFailure,
  isMonitorDue,
  runMonitorJourney,
  waitForMonitorCondition,
  type DiagnosisCallResult,
  type MonitorRun,
  type SyntheticMonitor,
} from "./syntheticMonitoring";
import type { ChatMessage } from "./llamaClient";

function textArtifact(text: string) {
  return { blob: { id: "sha256:x", size: text.length }, contentBase64: Buffer.from(text, "utf-8").toString("base64") };
}

function baseMonitor(overrides: Partial<SyntheticMonitor> = {}): SyntheticMonitor {
  return {
    id: "monitor-1",
    name: "Homepage",
    url: "https://example.com/",
    targetEnv: "production",
    intervalMinutes: 5,
    enabled: true,
    waitForSelector: null,
    waitForText: null,
    waitTimeoutMs: 2_000,
    clickSelector: null,
    assertion: { type: "textPresent", value: "Welcome" },
    createdAtMs: 0,
    lastRunAtMs: null,
    ...overrides,
  };
}

beforeEach(() => {
  startBrowserSessionMock.mockReset();
  annotateBrowserMock.mockReset();
  clickBrowserMock.mockReset();
  inspectBrowserMock.mockReset();
  captureBrowserEvidenceMock.mockReset();
  stopBrowserSessionMock.mockReset();
  readDurableArtifactMock.mockReset();
});

describe("createMonitor / assertMonitorUrl", () => {
  it("builds a monitor with sane defaults and a trimmed name/url", () => {
    const monitor = createMonitor({
      name: "  Homepage  ",
      url: "  https://example.com/  ",
      targetEnv: "production",
      intervalMinutes: 2.6,
      assertion: { type: "textPresent", value: "Welcome" },
      now: 1_000,
    });
    expect(monitor.name).toBe("Homepage");
    expect(monitor.url).toBe("https://example.com/");
    expect(monitor.intervalMinutes).toBe(3);
    expect(monitor.enabled).toBe(true);
    expect(monitor.waitTimeoutMs).toBeGreaterThan(0);
    expect(monitor.lastRunAtMs).toBeNull();
    expect(monitor.createdAtMs).toBe(1_000);
  });

  it("clamps a sub-minute interval up to the 1 minute minimum", () => {
    const monitor = createMonitor({
      name: "Fast",
      url: "https://example.com/",
      targetEnv: "local",
      intervalMinutes: 0,
      assertion: { type: "urlPrefix", value: "https://example.com/" },
    });
    expect(monitor.intervalMinutes).toBe(1);
  });

  it("rejects non-http(s) URLs and URLs with embedded credentials", () => {
    expect(() => assertMonitorUrl("file:///etc/passwd")).toThrow();
    expect(() => assertMonitorUrl("https://user:pass@example.com/")).toThrow();
    expect(() => assertMonitorUrl("https://example.com/")).not.toThrow();
  });
});

describe("isMonitorDue", () => {
  it("is due immediately for a never-run monitor", () => {
    expect(isMonitorDue(baseMonitor({ lastRunAtMs: null }), 10_000)).toBe(true);
  });

  it("is never due for a disabled monitor", () => {
    expect(isMonitorDue(baseMonitor({ enabled: false, lastRunAtMs: null }), 10_000)).toBe(false);
  });

  it("is due once the interval has elapsed since the last run, not before", () => {
    const monitor = baseMonitor({ intervalMinutes: 5, lastRunAtMs: 0 });
    expect(isMonitorDue(monitor, 4 * 60_000)).toBe(false);
    expect(isMonitorDue(monitor, 5 * 60_000)).toBe(true);
  });
});

describe("waitForMonitorCondition", () => {
  it("is a no-op when the monitor sets neither a wait selector nor wait text", async () => {
    await waitForMonitorCondition("session-1", baseMonitor());
    expect(annotateBrowserMock).not.toHaveBeenCalled();
    expect(inspectBrowserMock).not.toHaveBeenCalled();
  });

  it("resolves as soon as the selector annotates successfully", async () => {
    annotateBrowserMock.mockResolvedValueOnce({});
    await waitForMonitorCondition("session-1", baseMonitor({ waitForSelector: "#ready" }));
    expect(annotateBrowserMock).toHaveBeenCalledWith("session-1", "#ready");
  });

  it("resolves once the DOM text appears", async () => {
    inspectBrowserMock.mockResolvedValue({ url: "https://example.com/", dom: { id: "dom-1", size: 5 } });
    readDurableArtifactMock.mockResolvedValue(textArtifact("<html>Loaded and ready</html>"));
    await waitForMonitorCondition("session-1", baseMonitor({ waitForText: "ready" }));
    expect(readDurableArtifactMock).toHaveBeenCalledWith("dom-1");
  });

  it("times out with a descriptive error when the condition never appears", async () => {
    annotateBrowserMock.mockRejectedValue(new Error("not found"));
    await expect(
      waitForMonitorCondition("session-1", baseMonitor({ waitForSelector: "#missing", waitTimeoutMs: 50 })),
    ).rejects.toThrow(/Timed out after 50ms/);
  });
});

describe("assertMonitorCondition", () => {
  it("passes a selectorPresent assertion when annotate succeeds", async () => {
    annotateBrowserMock.mockResolvedValueOnce({});
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "selectorPresent", value: "#ok" } })),
    ).resolves.toBeUndefined();
  });

  it("fails a selectorPresent assertion when annotate throws", async () => {
    annotateBrowserMock.mockRejectedValueOnce(new Error("no match"));
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "selectorPresent", value: "#missing" } })),
    ).rejects.toThrow(/Assertion failed/);
  });

  it("passes/fails a textPresent assertion based on the DOM snapshot", async () => {
    inspectBrowserMock.mockResolvedValue({ url: "https://example.com/", dom: { id: "dom-1", size: 5 } });
    readDurableArtifactMock.mockResolvedValue(textArtifact("Welcome to the site"));
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "textPresent", value: "Welcome" } })),
    ).resolves.toBeUndefined();
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "textPresent", value: "Goodbye" } })),
    ).rejects.toThrow(/expected the page to contain/);
  });

  it("passes/fails a urlPrefix assertion against the final page URL", async () => {
    inspectBrowserMock.mockResolvedValue({ url: "https://example.com/dashboard?x=1", dom: { id: "dom-1", size: 5 } });
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "urlPrefix", value: "https://example.com/dashboard" } })),
    ).resolves.toBeUndefined();
    await expect(
      assertMonitorCondition("session-1", baseMonitor({ assertion: { type: "urlPrefix", value: "https://example.com/login" } })),
    ).rejects.toThrow(/expected the final URL/);
  });
});

describe("buildEvidenceExcerpt", () => {
  it("concatenates console/network excerpts and skips missing ids", async () => {
    readDurableArtifactMock.mockImplementation(async (id: string) =>
      id === "console-1" ? textArtifact("console error: boom") : textArtifact("network: 500 /api"),
    );
    const excerpt = await buildEvidenceExcerpt({
      screenshotArtifactId: null,
      domArtifactId: null,
      consoleArtifactId: "console-1",
      networkArtifactId: "network-1",
    });
    expect(excerpt).toContain("console error: boom");
    expect(excerpt).toContain("network: 500 /api");
  });

  it("returns an empty string when there is no evidence to read", async () => {
    const excerpt = await buildEvidenceExcerpt({
      screenshotArtifactId: null,
      domArtifactId: null,
      consoleArtifactId: null,
      networkArtifactId: null,
    });
    expect(excerpt).toBe("");
  });

  it("never throws even if reading an artifact fails", async () => {
    readDurableArtifactMock.mockRejectedValue(new Error("gone"));
    const excerpt = await buildEvidenceExcerpt({
      screenshotArtifactId: null,
      domArtifactId: null,
      consoleArtifactId: "console-1",
      networkArtifactId: null,
    });
    expect(excerpt).toBe("");
  });
});

describe("runMonitorJourney", () => {
  function stubEvidence() {
    captureBrowserEvidenceMock.mockResolvedValue({
      screenshot: { id: "shot-1", size: 10 },
      dom: { id: "dom-1", size: 10 },
      accessibility: null,
      console: { id: "console-1", size: 10 },
      network: { id: "network-1", size: 10 },
      performance: null,
      actionCount: 1,
    });
    stopBrowserSessionMock.mockResolvedValue(undefined);
  }

  it("reports pass when the whole journey succeeds, without calling diagnose", async () => {
    startBrowserSessionMock.mockResolvedValue({
      sessionId: "session-1",
      runId: "run-1",
      currentUrl: "https://example.com/",
      startedAtMs: 0,
      actionCount: 0,
      cancelled: false,
      viewport: { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
    });
    inspectBrowserMock.mockResolvedValue({ url: "https://example.com/", dom: { id: "dom-1", size: 5 } });
    readDurableArtifactMock.mockResolvedValue(textArtifact("Welcome to the site"));
    stubEvidence();
    const diagnose = vi.fn();

    const run = await runMonitorJourney(baseMonitor(), { diagnose, now: () => 1_000 });

    expect(run.status).toBe("pass");
    expect(run.failureReason).toBeNull();
    expect(run.diagnosis).toBeNull();
    expect(run.evidence.screenshotArtifactId).toBe("shot-1");
    expect(diagnose).not.toHaveBeenCalled();
    expect(stopBrowserSessionMock).toHaveBeenCalledWith("session-1");
  });

  it("reports fail with a diagnosis when the assertion fails, and always tears the session down", async () => {
    startBrowserSessionMock.mockResolvedValue({
      sessionId: "session-1",
      runId: "run-1",
      currentUrl: "https://example.com/",
      startedAtMs: 0,
      actionCount: 0,
      cancelled: false,
      viewport: { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
    });
    inspectBrowserMock.mockResolvedValue({ url: "https://example.com/", dom: { id: "dom-1", size: 5 } });
    readDurableArtifactMock.mockResolvedValue(textArtifact("Something else entirely"));
    stubEvidence();
    const diagnose = vi.fn(async (_monitor: SyntheticMonitor, _run: MonitorRun, excerpt: string) =>
      excerpt ? "Likely a deploy regression; check the network log next." : null,
    );

    const run = await runMonitorJourney(baseMonitor({ assertion: { type: "textPresent", value: "Welcome" } }), { diagnose, now: () => 2_000 });

    expect(run.status).toBe("fail");
    expect(run.failureReason).toMatch(/expected the page to contain/);
    expect(run.diagnosis).toBe("Likely a deploy regression; check the network log next.");
    expect(diagnose).toHaveBeenCalledTimes(1);
    expect(stopBrowserSessionMock).toHaveBeenCalledWith("session-1");
  });

  it("reports error (not fail) when the browser session never starts, and never calls diagnose without evidence support", async () => {
    startBrowserSessionMock.mockRejectedValue(new Error("origin not reachable"));
    const diagnose = vi.fn(async () => "unreachable diagnosis");

    const run = await runMonitorJourney(baseMonitor(), { diagnose });

    expect(run.status).toBe("error");
    expect(run.failureReason).toMatch(/origin not reachable/);
    expect(stopBrowserSessionMock).not.toHaveBeenCalled();
    expect(diagnose).toHaveBeenCalledTimes(1);
  });

  it("keeps the run's full evidence and failure reason even if the diagnosis callback itself throws", async () => {
    startBrowserSessionMock.mockRejectedValue(new Error("boom"));
    const diagnose = vi.fn(async () => {
      throw new Error("model unavailable");
    });
    const run = await runMonitorJourney(baseMonitor(), { diagnose });
    expect(run.status).toBe("error");
    expect(run.diagnosis).toBeNull();
  });
});

describe("buildDiagnosisMessages / diagnoseMonitorFailure", () => {
  const run: MonitorRun = {
    id: "run-1",
    monitorId: "monitor-1",
    monitorName: "Homepage",
    url: "https://example.com/",
    targetEnv: "production",
    startedAtMs: 0,
    finishedAtMs: 100,
    status: "fail",
    latencyMs: 100,
    failureReason: "Assertion failed: expected the page to contain \"Welcome\".",
    diagnosis: null,
    evidence: { screenshotArtifactId: null, domArtifactId: null, consoleArtifactId: null, networkArtifactId: null },
  };

  it("includes the monitor target, failure reason, and evidence excerpt in the prompt", () => {
    const messages = buildDiagnosisMessages(baseMonitor(), run, "Console log: 500 error");
    expect(messages).toHaveLength(2);
    expect(messages[1].content).toContain("https://example.com/");
    expect(messages[1].content).toContain("Assertion failed");
    expect(messages[1].content).toContain("Console log: 500 error");
  });

  it("returns the trimmed diagnosis text on a normal call", async () => {
    const callModel = vi.fn().mockResolvedValue({ content: "  Likely a bad deploy.  ", streamError: null });
    const diagnosis = await diagnoseMonitorFailure(baseMonitor(), run, "", callModel);
    expect(diagnosis).toBe("Likely a bad deploy.");
  });

  it("fails closed to null on a stream error, empty content, or thrown error", async () => {
    await expect(diagnoseMonitorFailure(baseMonitor(), run, "", vi.fn().mockResolvedValue({ content: "", streamError: "down" }))).resolves.toBeNull();
    await expect(diagnoseMonitorFailure(baseMonitor(), run, "", vi.fn().mockResolvedValue({ content: "   ", streamError: null }))).resolves.toBeNull();
    await expect(diagnoseMonitorFailure(baseMonitor(), run, "", vi.fn().mockRejectedValue(new Error("boom")))).resolves.toBeNull();
  });

  it("aborts the call once the parent signal fires", async () => {
    const controller = new AbortController();
    const callModel = vi.fn(
      (_messages: ChatMessage[], signal: AbortSignal): Promise<DiagnosisCallResult> =>
        new Promise((_resolve, reject) => {
          signal.addEventListener("abort", () => reject(new Error("aborted")));
        }),
    );
    const pending = diagnoseMonitorFailure(baseMonitor(), run, "", callModel, controller.signal);
    controller.abort();
    await expect(pending).resolves.toBeNull();
  });
});

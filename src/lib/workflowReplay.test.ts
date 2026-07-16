import { beforeEach, describe, expect, it, vi } from "vitest";

const startBrowserSessionMock = vi.fn();
const navigateBrowserMock = vi.fn();
const clickBrowserMock = vi.fn();
const typeBrowserTextMock = vi.fn();
const scrollBrowserMock = vi.fn();
const annotateBrowserMock = vi.fn();
const captureBrowserEvidenceMock = vi.fn();
const inspectBrowserMock = vi.fn();
const stopBrowserSessionMock = vi.fn();

vi.mock("./browserVerification", async () => {
  const actual = await vi.importActual<typeof import("./browserVerification")>("./browserVerification");
  return {
    ...actual,
    startBrowserSession: (...args: unknown[]) => startBrowserSessionMock(...args),
    navigateBrowser: (...args: unknown[]) => navigateBrowserMock(...args),
    clickBrowser: (...args: unknown[]) => clickBrowserMock(...args),
    typeBrowserText: (...args: unknown[]) => typeBrowserTextMock(...args),
    scrollBrowser: (...args: unknown[]) => scrollBrowserMock(...args),
    annotateBrowser: (...args: unknown[]) => annotateBrowserMock(...args),
    captureBrowserEvidence: (...args: unknown[]) => captureBrowserEvidenceMock(...args),
    inspectBrowser: (...args: unknown[]) => inspectBrowserMock(...args),
    stopBrowserSession: (...args: unknown[]) => stopBrowserSessionMock(...args),
  };
});

import { cancelRegisteredRun, clearRunCancellationRegistryForTests } from "./runCancellationRegistry";
import { convertRecordingToDraft, createRecording, appendClickStep, appendTypeStep, stopRecording } from "./workflowRecorder";
import { startWorkflowReplay, type ReplayEvent } from "./workflowReplay";
import type { DraftWorkflow } from "./workflowRecorder";
import { useWorkflowDraftStore } from "../store/workflowDraftStore";

function evidence(id: string) {
  return {
    ok: true,
    url: "https://example.com/dashboard",
    evidence: {
      screenshot: { id, size: 10 },
      dom: null,
      accessibility: null,
      console: null,
      network: null,
      performance: null,
      actionCount: 1,
    },
  };
}

function buildEnabledDraft(): DraftWorkflow {
  let recording = createRecording("run-1", "https://example.com/login", 1_000);
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#username",
      rawValue: "jane",
      element: { tag: "input", role: "", ariaLabel: "Username", text: "" },
      screenshotArtifactId: null,
    },
    1_100,
  );
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#password",
      rawValue: "hunter2",
      element: { tag: "input", role: "", ariaLabel: "Password", text: "" },
      screenshotArtifactId: null,
    },
    1_200,
  );
  recording = appendClickStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#submit",
      element: { tag: "button", role: "button", ariaLabel: "Sign in", text: "Sign in" },
      screenshotArtifactId: null,
    },
    1_300,
  );
  // Deliberately no navigate step after the click: that would insert a
  // `waitForSelector` decision point (see workflowRecorder.test.ts, which
  // covers that conversion behavior directly) and this suite is testing the
  // replay runner, not the converter — a real wait would just slow the
  // suite down for no additional coverage here.
  recording = stopRecording(recording, 1_500);
  const draft = convertRecordingToDraft(recording, { name: "Login", now: 1_600 });
  useWorkflowDraftStore.setState({ drafts: [] });
  useWorkflowDraftStore.getState().saveDraft(draft);
  useWorkflowDraftStore.getState().markReviewed(draft.id);
  useWorkflowDraftStore.getState().enableDraft(draft.id);
  return useWorkflowDraftStore.getState().drafts[0];
}

beforeEach(() => {
  clearRunCancellationRegistryForTests();
  startBrowserSessionMock.mockReset().mockResolvedValue({
    sessionId: "session-1",
    runId: "run-1",
    currentUrl: "https://example.com/",
    startedAtMs: 0,
    actionCount: 0,
    cancelled: false,
    viewport: { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
  });
  navigateBrowserMock.mockReset().mockResolvedValue(evidence("sha256:nav"));
  clickBrowserMock.mockReset().mockResolvedValue(evidence("sha256:click"));
  typeBrowserTextMock.mockReset().mockResolvedValue(evidence("sha256:type"));
  scrollBrowserMock.mockReset().mockResolvedValue(evidence("sha256:scroll"));
  annotateBrowserMock.mockReset().mockResolvedValue({
    url: "https://example.com/dashboard",
    selector: "#submit",
    tag: "button",
    role: "button",
    ariaLabel: "Sign in",
    text: "Sign in",
    rect: { x: 0, y: 0, width: 10, height: 10 },
    evidence: evidence("sha256:wait").evidence,
  });
  captureBrowserEvidenceMock.mockReset().mockResolvedValue(evidence("sha256:verify").evidence);
  // Matches buildEnabledDraft()'s final ("click") step url, since it has no
  // trailing navigate — see the note above appendClickStep in that helper.
  inspectBrowserMock.mockReset().mockResolvedValue({
    url: "https://example.com/login",
    title: "Login",
    dom: { id: "sha256:dom", size: 1 },
    accessibility: { id: "sha256:ax", size: 1 },
    accessibilityIssues: [],
  });
  stopBrowserSessionMock.mockReset().mockResolvedValue(undefined);
});

describe("startWorkflowReplay", () => {
  it("refuses to run a workflow that has not been reviewed and enabled", () => {
    const recording = stopRecording(createRecording("run-1", "https://example.com"), 1);
    const draft = convertRecordingToDraft(recording);
    expect(() => startWorkflowReplay(draft, { runtimeInputs: {} })).toThrow(/enabled/i);
    expect(startBrowserSessionMock).not.toHaveBeenCalled();
  });

  it("runs every step, resolving the sensitive input only from the runtime-supplied value", async () => {
    const draft = buildEnabledDraft();
    const sensitiveInput = draft.inputs.find((input) => input.sensitive)!;
    const events: ReplayEvent[] = [];
    const handle = startWorkflowReplay(draft, {
      runtimeInputs: { [sensitiveInput.id]: "typed-at-replay-time" },
      onEvent: (event) => events.push(event),
    });
    const run = await handle.done;

    expect(run.status).toBe("completed");
    expect(run.steps.every((step) => step.status === "success")).toBe(true);
    expect(typeBrowserTextMock).toHaveBeenCalledWith(expect.any(String), "#password", "typed-at-replay-time");
    // Every successful step logs the screenshot evidence returned by that action.
    expect(run.steps.filter((step) => step.screenshotArtifactId).length).toBeGreaterThan(0);
    expect(events.some((event) => event.type === "run-end")).toBe(true);
    expect(stopBrowserSessionMock).toHaveBeenCalledWith("session-1");
  });

  it("never reads a sensitive input's stored default — it has none — and fails the run without one supplied", async () => {
    const draft = buildEnabledDraft();
    const handle = startWorkflowReplay(draft, { runtimeInputs: {} });
    const run = await handle.done;

    expect(run.status).toBe("failed");
    const failedStep = run.steps.find((step) => step.status === "failed");
    expect(failedStep?.error).toMatch(/missing runtime value/i);
    expect(stopBrowserSessionMock).toHaveBeenCalled();
  });

  it("uses the recorded default value for an ordinary (non-sensitive) input when none is supplied", async () => {
    const draft = buildEnabledDraft();
    const sensitiveInput = draft.inputs.find((input) => input.sensitive)!;
    const handle = startWorkflowReplay(draft, { runtimeInputs: { [sensitiveInput.id]: "value" } });
    await handle.done;
    expect(typeBrowserTextMock).toHaveBeenCalledWith(expect.any(String), "#username", "jane");
  });

  it("fails the run and stops the session when a step throws", async () => {
    const draft = buildEnabledDraft();
    const sensitiveInput = draft.inputs.find((input) => input.sensitive)!;
    clickBrowserMock.mockRejectedValueOnce(new Error("selector not found"));
    const handle = startWorkflowReplay(draft, { runtimeInputs: { [sensitiveInput.id]: "value" } });
    const run = await handle.done;

    expect(run.status).toBe("failed");
    expect(stopBrowserSessionMock).toHaveBeenCalled();
  });

  it("can be cancelled mid-run: remaining steps are marked cancelled and the session is stopped", async () => {
    const draft = buildEnabledDraft();
    const sensitiveInput = draft.inputs.find((input) => input.sensitive)!;
    let cancelled = false;
    const handle = startWorkflowReplay(draft, {
      runtimeInputs: { [sensitiveInput.id]: "value" },
      onEvent: (event) => {
        // Cancel as soon as the first step finishes.
        if (!cancelled && event.type === "step-end") {
          cancelled = true;
          cancelRegisteredRun(handle.runId);
        }
      },
    });
    const run = await handle.done;

    expect(run.status).toBe("cancelled");
    expect(run.steps.some((step) => step.status === "cancelled")).toBe(true);
    expect(stopBrowserSessionMock).toHaveBeenCalledWith("session-1");
  });

  it("fails verification when the replayed page does not match the recorded final URL", async () => {
    const draft = buildEnabledDraft();
    const sensitiveInput = draft.inputs.find((input) => input.sensitive)!;
    inspectBrowserMock.mockResolvedValue({
      url: "https://example.com/error",
      title: "Error",
      dom: { id: "sha256:dom", size: 1 },
      accessibility: { id: "sha256:ax", size: 1 },
      accessibilityIssues: [],
    });
    const handle = startWorkflowReplay(draft, { runtimeInputs: { [sensitiveInput.id]: "value" } });
    const run = await handle.done;

    expect(run.status).toBe("failed");
    const verifyStep = run.steps[run.steps.length - 1];
    expect(verifyStep.status).toBe("failed");
    expect(verifyStep.error).toMatch(/verification failed/i);
  });
});

/**
 * Record and Replay Workflows — replay runner (ROADMAP.md, Phase 1).
 *
 * Executes a saved `DraftWorkflow` step by step against the existing,
 * exact-origin, disposable Chromium worker (`browserVerification.ts`) — the
 * same primitives the Browser Workbench itself uses. No new browser
 * automation is implemented here.
 *
 * Two safety properties this module enforces, not just the UI around it:
 *
 *  - `startWorkflowReplay` throws unless `draft.status === "enabled"`. A
 *    draft can only reach that status through `workflowDraftStore`'s
 *    `enableDraft`, which itself refuses unless the draft has been marked
 *    reviewed — so replay is structurally unreachable for an unreviewed
 *    recording, not merely hidden behind a UI affordance.
 *  - Runtime-only inputs (every credential-like field, plus anything the
 *    user marked during review) are resolved *only* from the caller-supplied
 *    `runtimeInputs` map for this one run — never from `defaultValue` — so a
 *    secret can never be replayed from a persisted draft.
 *
 * Every step's evidence is the screenshot already returned by the browser
 * worker's own action result (`BrowserActionResult.evidence.screenshot`),
 * the same evidence trail the Browser Workbench's "Save evidence" already
 * relies on — nothing here captures screenshots on its own.
 */
import {
  annotateBrowser,
  captureBrowserEvidence,
  clickBrowser,
  inspectBrowser,
  isLoopbackBrowserUrl,
  navigateBrowser,
  scrollBrowser,
  startBrowserSession,
  stopBrowserSession,
  typeBrowserText,
  type BrowserActionResult,
} from "./browserVerification";
import { registerRunCancellation } from "./runCancellationRegistry";
import type { DraftWorkflow, DraftWorkflowStep, DraftWorkflowStepAction } from "./workflowRecorder";

export type ReplayStepStatus = "pending" | "running" | "success" | "failed" | "cancelled";

export interface ReplayStepLog {
  stepId: string;
  status: ReplayStepStatus;
  startedAtMs: number | null;
  finishedAtMs: number | null;
  screenshotArtifactId: string | null;
  detail: string;
  error: string | null;
}

export type ReplayRunStatus = "running" | "completed" | "failed" | "cancelled";

export interface ReplayRun {
  id: string;
  draftId: string;
  status: ReplayRunStatus;
  startedAtMs: number;
  finishedAtMs: number | null;
  steps: ReplayStepLog[];
}

export type ReplayEvent =
  | { type: "step-start"; run: ReplayRun; step: ReplayStepLog }
  | { type: "step-end"; run: ReplayRun; step: ReplayStepLog }
  | { type: "run-end"; run: ReplayRun };

export interface RunWorkflowReplayOptions {
  /** Keyed by input id (preferred) or input name. Must supply every
   * `runtimeOnly`/`sensitive` input; ordinary inputs fall back to their
   * recorded `defaultValue` when omitted. */
  runtimeInputs: Record<string, string>;
  allowLoopback?: boolean;
  onEvent?: (event: ReplayEvent) => void;
}

export interface ReplayRunHandle {
  runId: string;
  done: Promise<ReplayRun>;
}

function initialStepLog(step: DraftWorkflowStep): ReplayStepLog {
  return {
    stepId: step.id,
    status: "pending",
    startedAtMs: null,
    finishedAtMs: null,
    screenshotArtifactId: null,
    detail: "",
    error: null,
  };
}

function updateStep(run: ReplayRun, stepId: string, patch: Partial<ReplayStepLog>): ReplayRun {
  return { ...run, steps: run.steps.map((entry) => (entry.stepId === stepId ? { ...entry, ...patch } : entry)) };
}

function findStep(run: ReplayRun, stepId: string): ReplayStepLog {
  const step = run.steps.find((entry) => entry.stepId === stepId);
  if (!step) throw new Error("Replay step log is missing its own step.");
  return step;
}

function resolveInputValue(draft: DraftWorkflow, inputId: string, runtimeInputs: Record<string, string>): string {
  const input = draft.inputs.find((entry) => entry.id === inputId);
  if (!input) throw new Error("This draft workflow references an input that no longer exists.");
  if (input.runtimeOnly || input.sensitive) {
    const provided = runtimeInputs[input.id] ?? runtimeInputs[input.name];
    if (!provided) {
      throw new Error(
        `Missing runtime value for "${input.label || input.name}". Runtime inputs are never stored and must be supplied for every replay.`,
      );
    }
    return provided;
  }
  return runtimeInputs[input.id] ?? runtimeInputs[input.name] ?? input.defaultValue ?? "";
}

function sleep(ms: number, signal: AbortSignal): Promise<void> {
  if (signal.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

function fromActionResult(result: BrowserActionResult, detail: string): { screenshotArtifactId: string | null; detail: string } {
  return { screenshotArtifactId: result.evidence.screenshot?.id ?? null, detail };
}

async function waitForSelector(
  sessionId: string,
  selector: string | null,
  timeoutMs: number,
  reason: string,
  signal: AbortSignal,
): Promise<{ screenshotArtifactId: string | null; detail: string }> {
  if (!selector) {
    await sleep(Math.min(timeoutMs, 1_000), signal);
    return { screenshotArtifactId: null, detail: reason };
  }
  const deadline = Date.now() + timeoutMs;
  let lastError: string | null = null;
  while (Date.now() < deadline) {
    if (signal.aborted) throw new Error("Replay cancelled while waiting for the target element.");
    try {
      const annotation = await annotateBrowser(sessionId, selector);
      return { screenshotArtifactId: annotation.evidence.screenshot?.id ?? null, detail: `Found "${selector}" before continuing.` };
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
      if (Date.now() >= deadline) break;
      await sleep(Math.min(400, deadline - Date.now()), signal);
    }
  }
  throw new Error(`Timed out after ${timeoutMs}ms waiting for "${selector}": ${lastError ?? "not found"}`);
}

async function verifyOutcome(
  sessionId: string,
  action: Extract<DraftWorkflowStepAction, { type: "verify" }>,
): Promise<{ screenshotArtifactId: string | null; detail: string }> {
  const inspection = await inspectBrowser(sessionId);
  const evidence = await captureBrowserEvidence(sessionId);
  if (action.expectedUrlPrefix) {
    let actualPrefix = inspection.url;
    try {
      const parsed = new URL(inspection.url);
      actualPrefix = `${parsed.origin}${parsed.pathname}`;
    } catch {
      // Keep the raw URL if it fails to parse; the comparison below will
      // simply fail closed instead of silently passing.
    }
    if (actualPrefix !== action.expectedUrlPrefix) {
      throw new Error(`Verification failed: expected the page to end on ${action.expectedUrlPrefix}, but it was ${actualPrefix}.`);
    }
  }
  return { screenshotArtifactId: evidence.screenshot?.id ?? null, detail: action.description };
}

async function executeStep(
  sessionId: string,
  draft: DraftWorkflow,
  step: DraftWorkflowStep,
  runtimeInputs: Record<string, string>,
  signal: AbortSignal,
): Promise<{ screenshotArtifactId: string | null; detail: string }> {
  const action = step.action;
  switch (action.type) {
    case "navigate":
      return fromActionResult(await navigateBrowser(sessionId, action.url), `Navigated to ${action.url}`);
    case "click":
      return fromActionResult(await clickBrowser(sessionId, action.selector), action.description);
    case "type": {
      const value = resolveInputValue(draft, action.inputId, runtimeInputs);
      return fromActionResult(await typeBrowserText(sessionId, action.selector, value), action.description);
    }
    case "scroll":
      return fromActionResult(await scrollBrowser(sessionId, action.x, action.y), `Scrolled to (${action.x}, ${action.y})`);
    case "waitForSelector":
      return waitForSelector(sessionId, action.selector, action.timeoutMs, action.reason, signal);
    case "verify":
      return verifyOutcome(sessionId, action);
    default: {
      const exhaustive: never = action;
      throw new Error(`Unsupported draft workflow step: ${JSON.stringify(exhaustive)}`);
    }
  }
}

async function executeReplay(
  runId: string,
  draft: DraftWorkflow,
  options: RunWorkflowReplayOptions,
  signal: AbortSignal,
): Promise<ReplayRun> {
  let run: ReplayRun = {
    id: runId,
    draftId: draft.id,
    status: "running",
    startedAtMs: Date.now(),
    finishedAtMs: null,
    steps: draft.steps.map(initialStepLog),
  };
  const emit = (event: ReplayEvent) => options.onEvent?.(event);

  let sessionId: string | null = null;
  try {
    const allowLoopback = options.allowLoopback ?? isLoopbackBrowserUrl(draft.originUrl);
    // `startBrowserSession` derives the exact-origin grant from this same
    // URL internally (see `exactBrowserOrigin` in browserVerification.ts),
    // so passing the full recorded starting URL both grants the identical
    // single origin recording used *and* lands replay on the exact page the
    // recording started on — not just its bare origin, which could easily
    // be a different, unrelated page (e.g. a homepage vs. a login form).
    const session = await startBrowserSession({ runId, url: draft.originUrl, allowLoopback });
    sessionId = session.sessionId;

    for (const step of draft.steps) {
      if (signal.aborted) break;
      run = updateStep(run, step.id, { status: "running", startedAtMs: Date.now() });
      emit({ type: "step-start", run, step: findStep(run, step.id) });
      try {
        const result = await executeStep(sessionId, draft, step, options.runtimeInputs, signal);
        run = updateStep(run, step.id, {
          status: "success",
          finishedAtMs: Date.now(),
          screenshotArtifactId: result.screenshotArtifactId,
          detail: result.detail,
        });
        emit({ type: "step-end", run, step: findStep(run, step.id) });
      } catch (error) {
        const cancelled = signal.aborted;
        const message = error instanceof Error ? error.message : String(error);
        run = updateStep(run, step.id, {
          status: cancelled ? "cancelled" : "failed",
          finishedAtMs: Date.now(),
          error: cancelled ? null : message,
        });
        emit({ type: "step-end", run, step: findStep(run, step.id) });
        run = { ...run, status: cancelled ? "cancelled" : "failed", finishedAtMs: Date.now() };
        emit({ type: "run-end", run });
        return run;
      }
    }

    if (signal.aborted) {
      run = {
        ...run,
        steps: run.steps.map((entry) => (entry.status === "pending" ? { ...entry, status: "cancelled" as const } : entry)),
        status: "cancelled",
        finishedAtMs: Date.now(),
      };
    } else {
      run = { ...run, status: "completed", finishedAtMs: Date.now() };
    }
    emit({ type: "run-end", run });
    return run;
  } finally {
    if (sessionId) await stopBrowserSession(sessionId).catch(() => undefined);
  }
}

/**
 * Starts replaying `draft` against a fresh, disposable browser session and
 * returns immediately with a cancellable run id — mirroring
 * `recipeRunner.ts`'s `RecipeRunHandle` shape. Call `cancelRegisteredRun
 * (handle.runId)` (from `./runCancellationRegistry`) to cancel mid-run; the
 * in-flight step finishes (or fails) and every remaining pending step is
 * marked `"cancelled"` before the disposable browser session is torn down.
 */
export function startWorkflowReplay(draft: DraftWorkflow, options: RunWorkflowReplayOptions): ReplayRunHandle {
  if (draft.status !== "enabled") {
    throw new Error(
      "This workflow has not been reviewed and enabled. Replay is refused until an explicit user review/save/enable action.",
    );
  }
  const runId = crypto.randomUUID();
  const controller = new AbortController();
  const unregister = registerRunCancellation(runId, () => controller.abort());
  const done = executeReplay(runId, draft, options, controller.signal).finally(unregister);
  return { runId, done };
}

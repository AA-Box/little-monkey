/**
 * Synthetic Monitoring Agent (ROADMAP.md Phase 7, item 17) — MVP scope:
 * local and public http(s) URL targets only (loopback origins are allowed
 * the same way `workflowReplay.ts` allows them for local dev servers; a
 * genuinely private/staging network behind a VPN, or an API-only, non-
 * browser journey, are intentionally left as follow-ups — see the bottom of
 * this comment).
 *
 * A "monitor" is a URL plus a tiny journey: navigate (implicit — landing on
 * `url` is the first step, exactly like `startBrowserSession` already does
 * for every other browser-driven feature), wait for a selector or a literal
 * piece of text to appear, optionally click one element, then assert either
 * a selector is present, some text is present, or the final URL starts with
 * a given prefix. This module owns the journey's run logic and its evidence
 * capture; it deliberately reuses the EXACT same disposable Chromium worker
 * every other browser-driven feature in this app already uses
 * (`browserVerification.ts` — the real Rust browser worker backing
 * `BrowserWorkbench`/`workflowReplay.ts`) rather than adding any new browser
 * automation, and the same content-addressed evidence store
 * (`durableArtifacts.ts`) everything else in the app reads screenshots/logs
 * back from.
 *
 * The failure diagnosis is a single one-shot, tool-less local-model call —
 * the same shape `riskJudge.ts`'s `classifyToolCall` and `agentLoop.ts`'s
 * `sendForSummary` use — dependency-injected here via a `callModel`
 * parameter for the exact same reason `riskJudge.ts` takes one instead of
 * importing `attemptStream` itself (see that module's doc comment): it keeps
 * this module free of any dependency on `turnEngine.ts`/`agentLoop.ts`, and
 * trivially testable without a real model. `syntheticMonitoringStore.ts`
 * wires the real `resolveTarget`/`attemptStream` closure, exactly like
 * `agentLoop.ts` wires `riskJudge.ts`'s `classifyToolCall`.
 *
 * Follow-ups intentionally out of scope for this MVP (narrowing scope on
 * purpose, not an oversight): true authenticated "staging"/"production"
 * targets behind a private network or auth headers; API-only (non-browser)
 * journeys; uptime/latency percentile rollups across runs; alerting or
 * notifications on a failing run (today a failure just sits in the run
 * history for the user to notice in the panel).
 */
import {
  annotateBrowser,
  captureBrowserEvidence,
  clickBrowser,
  exactBrowserOrigin,
  inspectBrowser,
  isLoopbackBrowserUrl,
  startBrowserSession,
  stopBrowserSession,
  type BrowserEvidence,
} from "./browserVerification";
import { readDurableArtifact } from "./durableArtifacts";
import type { ChatMessage } from "./llamaClient";

export type MonitorTargetEnv = "local" | "staging" | "production";
export type MonitorAssertionType = "selectorPresent" | "textPresent" | "urlPrefix";

export interface MonitorAssertion {
  type: MonitorAssertionType;
  value: string;
}

export interface SyntheticMonitor {
  id: string;
  name: string;
  url: string;
  targetEnv: MonitorTargetEnv;
  /** Minutes between scheduled runs. Clamped to >= 1 by `createMonitor`. */
  intervalMinutes: number;
  enabled: boolean;
  /** At most one of `waitForSelector`/`waitForText` is normally set — if
   * both are, the selector wins (checked first) purely to keep the wait
   * loop's cost bounded to one live probe per poll. */
  waitForSelector: string | null;
  waitForText: string | null;
  waitTimeoutMs: number;
  clickSelector: string | null;
  assertion: MonitorAssertion;
  createdAtMs: number;
  lastRunAtMs: number | null;
}

export type MonitorRunStatus = "pass" | "fail" | "error";

export interface MonitorRunEvidence {
  screenshotArtifactId: string | null;
  domArtifactId: string | null;
  consoleArtifactId: string | null;
  networkArtifactId: string | null;
}

export interface MonitorRun {
  id: string;
  monitorId: string;
  monitorName: string;
  url: string;
  targetEnv: MonitorTargetEnv;
  startedAtMs: number;
  finishedAtMs: number;
  status: MonitorRunStatus;
  /** Wall-clock time from session start to the journey's natural end
   * (success or failure) — a simple, honest stand-in for real latency
   * percentiles, which are an explicit follow-up (see module doc comment). */
  latencyMs: number;
  failureReason: string | null;
  /** Set only for a `fail`/`error` run whose diagnosis call actually
   * produced text — `null` on a pass, or when diagnosis was skipped/timed
   * out/errored (this module always fails closed, never fabricates one). */
  diagnosis: string | null;
  evidence: MonitorRunEvidence;
}

export const DEFAULT_WAIT_TIMEOUT_MS = 15_000;
export const MIN_INTERVAL_MINUTES = 1;
export const DIAGNOSIS_TIMEOUT_MS = 15_000;
const MAX_EVIDENCE_EXCERPT_CHARS = 2_000;

/** Throws with a user-facing message unless `url` is a bare http(s) origin
 * this module can actually run a monitor against — reuses
 * `browserVerification.ts`'s own exact-origin grant derivation so a monitor
 * can never be saved with a URL the browser worker would reject at run
 * time anyway. */
export function assertMonitorUrl(url: string): void {
  exactBrowserOrigin(url);
}

export function createMonitor(input: {
  name: string;
  url: string;
  targetEnv: MonitorTargetEnv;
  intervalMinutes: number;
  waitForSelector?: string | null;
  waitForText?: string | null;
  waitTimeoutMs?: number;
  clickSelector?: string | null;
  assertion: MonitorAssertion;
  now?: number;
}): SyntheticMonitor {
  const url = input.url.trim();
  assertMonitorUrl(url);
  return {
    id: crypto.randomUUID(),
    name: input.name.trim() || url,
    url,
    targetEnv: input.targetEnv,
    intervalMinutes: Math.max(MIN_INTERVAL_MINUTES, Math.round(input.intervalMinutes) || MIN_INTERVAL_MINUTES),
    enabled: true,
    waitForSelector: input.waitForSelector?.trim() || null,
    waitForText: input.waitForText?.trim() || null,
    waitTimeoutMs: input.waitTimeoutMs && input.waitTimeoutMs > 0 ? input.waitTimeoutMs : DEFAULT_WAIT_TIMEOUT_MS,
    clickSelector: input.clickSelector?.trim() || null,
    assertion: { type: input.assertion.type, value: input.assertion.value.trim() },
    createdAtMs: input.now ?? Date.now(),
    lastRunAtMs: null,
  };
}

/** Whether `monitor` is due for a fresh run as of `nowMs` — a disabled
 * monitor is never due, and one that has never run is due immediately. */
export function isMonitorDue(monitor: SyntheticMonitor, nowMs: number): boolean {
  if (!monitor.enabled) return false;
  if (monitor.lastRunAtMs === null) return true;
  return nowMs - monitor.lastRunAtMs >= monitor.intervalMinutes * 60_000;
}

function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (ms <= 0 || signal?.aborted) return Promise.resolve();
  return new Promise((resolve) => {
    const onAbort = () => {
      clearTimeout(timer);
      resolve();
    };
    const timer = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

function decodeArtifactText(contentBase64: string): string {
  const bytes = Uint8Array.from(atob(contentBase64), (character) => character.charCodeAt(0));
  return new TextDecoder().decode(bytes);
}

async function readDomText(sessionId: string): Promise<string> {
  const inspection = await inspectBrowser(sessionId);
  const content = await readDurableArtifact(inspection.dom.id);
  return decodeArtifactText(content.contentBase64);
}

/** Polls (at most every 400ms, up to `monitor.waitTimeoutMs`) for
 * `monitor.waitForSelector`/`waitForText` to appear — a no-op when the
 * monitor sets neither. Mirrors `workflowReplay.ts`'s `waitForSelector`
 * polling shape, extended with a literal-text probe via the page's own DOM
 * artifact (there is no dedicated "wait for text" primitive on the browser
 * worker, so this reads the DOM snapshot `inspectBrowser` already exposes
 * rather than adding one). */
export async function waitForMonitorCondition(sessionId: string, monitor: SyntheticMonitor, signal?: AbortSignal): Promise<void> {
  if (!monitor.waitForSelector && !monitor.waitForText) return;
  const deadline = Date.now() + monitor.waitTimeoutMs;
  let lastError = "condition not met";
  do {
    if (signal?.aborted) throw new Error("Monitor run cancelled while waiting for the target condition.");
    try {
      if (monitor.waitForSelector) {
        await annotateBrowser(sessionId, monitor.waitForSelector);
        return;
      }
      if (monitor.waitForText) {
        const text = await readDomText(sessionId);
        if (text.includes(monitor.waitForText)) return;
        lastError = `Text "${monitor.waitForText}" was not found on the page yet.`;
      }
    } catch (error) {
      lastError = error instanceof Error ? error.message : String(error);
    }
    await sleep(Math.min(400, Math.max(0, deadline - Date.now())), signal);
  } while (Date.now() < deadline);
  throw new Error(`Timed out after ${monitor.waitTimeoutMs}ms waiting: ${lastError}`);
}

/** Evaluates `monitor.assertion` against the current page. Throws with a
 * user-facing message on failure — never returns `false`, so callers can
 * treat "assertion ran to completion" as "assertion passed". */
export async function assertMonitorCondition(sessionId: string, monitor: SyntheticMonitor): Promise<void> {
  const { assertion } = monitor;
  if (assertion.type === "selectorPresent") {
    try {
      await annotateBrowser(sessionId, assertion.value);
    } catch (error) {
      throw new Error(`Assertion failed: expected selector "${assertion.value}" to be present. ${error instanceof Error ? error.message : String(error)}`);
    }
    return;
  }
  if (assertion.type === "textPresent") {
    const text = await readDomText(sessionId);
    if (!text.includes(assertion.value)) {
      throw new Error(`Assertion failed: expected the page to contain the text "${assertion.value}".`);
    }
    return;
  }
  const inspection = await inspectBrowser(sessionId);
  let actualPrefix = inspection.url;
  try {
    const parsed = new URL(inspection.url);
    actualPrefix = `${parsed.origin}${parsed.pathname}`;
  } catch {
    // Fail closed on the raw URL if it doesn't parse — the comparison below
    // simply fails instead of silently passing.
  }
  if (actualPrefix !== assertion.value) {
    throw new Error(`Assertion failed: expected the final URL to start with "${assertion.value}", but it was "${actualPrefix}".`);
  }
}

/** Reads the console/network evidence artifacts back into bounded plain
 * text for the diagnosis prompt — the exact same `atob`+`TextDecoder`
 * decode `BrowserWorkbench.tsx`'s own `artifactText` helper uses, just
 * duplicated here (a lib module reaching into a component file for a
 * private helper would be the wrong direction of reuse). */
export async function buildEvidenceExcerpt(evidence: MonitorRunEvidence): Promise<string> {
  const parts: string[] = [];
  for (const [label, id] of [
    ["Console", evidence.consoleArtifactId],
    ["Network", evidence.networkArtifactId],
  ] as const) {
    if (!id) continue;
    try {
      const content = await readDurableArtifact(id);
      const text = decodeArtifactText(content.contentBase64).slice(0, MAX_EVIDENCE_EXCERPT_CHARS);
      if (text.trim()) parts.push(`${label} log (bounded):\n${text}`);
    } catch {
      // Evidence is best-effort for the diagnosis prompt — a blob that
      // failed to read back just means one fewer excerpt, never a thrown
      // error out of the whole run.
    }
  }
  return parts.join("\n\n");
}

export interface RunMonitorOptions {
  signal?: AbortSignal;
  now?: () => number;
  /** Called only when the run ends `fail`/`error`, with the run's own
   * (already-finished) record and a bounded evidence excerpt — returns the
   * diagnosis text to store, or `null` to store none. Never throws: any
   * failure inside this callback is the caller's responsibility to catch,
   * exactly like `riskJudge.ts`'s `classifyToolCall` fails closed on its own. */
  diagnose?: (monitor: SyntheticMonitor, run: MonitorRun, evidenceExcerpt: string, signal?: AbortSignal) => Promise<string | null>;
}

/** Runs one monitor journey end-to-end against a fresh, disposable browser
 * session: land on `monitor.url`, wait for the configured condition,
 * optionally click, assert, then always capture evidence and tear the
 * session down — regardless of which step failed. Never throws; every
 * outcome (including a browser-session start failure) comes back as a
 * `MonitorRun` with `status: "fail"`/`"error"` and a `failureReason`. */
export async function runMonitorJourney(monitor: SyntheticMonitor, options: RunMonitorOptions = {}): Promise<MonitorRun> {
  const signal = options.signal;
  const now = options.now ?? (() => Date.now());
  const startedAtMs = now();
  const runId = crypto.randomUUID();

  let sessionId: string | null = null;
  let status: MonitorRunStatus = "pass";
  let failureReason: string | null = null;
  let evidence: BrowserEvidence | null = null;

  try {
    const allowLoopback = isLoopbackBrowserUrl(monitor.url);
    const session = await startBrowserSession({ runId, url: monitor.url, allowLoopback });
    sessionId = session.sessionId;

    await waitForMonitorCondition(sessionId, monitor, signal);
    if (signal?.aborted) throw new Error("Monitor run cancelled.");

    if (monitor.clickSelector) {
      await clickBrowser(sessionId, monitor.clickSelector);
    }
    if (signal?.aborted) throw new Error("Monitor run cancelled.");

    await assertMonitorCondition(sessionId, monitor);
  } catch (error) {
    status = sessionId ? "fail" : "error";
    failureReason = error instanceof Error ? error.message : String(error);
  } finally {
    if (sessionId) {
      evidence = await captureBrowserEvidence(sessionId).catch(() => null);
      await stopBrowserSession(sessionId).catch(() => {});
    }
  }

  const finishedAtMs = now();
  const runEvidence: MonitorRunEvidence = {
    screenshotArtifactId: evidence?.screenshot?.id ?? null,
    domArtifactId: evidence?.dom?.id ?? null,
    consoleArtifactId: evidence?.console?.id ?? null,
    networkArtifactId: evidence?.network?.id ?? null,
  };

  const run: MonitorRun = {
    id: runId,
    monitorId: monitor.id,
    monitorName: monitor.name,
    url: monitor.url,
    targetEnv: monitor.targetEnv,
    startedAtMs,
    finishedAtMs,
    status,
    latencyMs: Math.max(0, finishedAtMs - startedAtMs),
    failureReason,
    diagnosis: null,
    evidence: runEvidence,
  };

  if (status === "pass" || !options.diagnose) return run;

  try {
    const excerpt = await buildEvidenceExcerpt(runEvidence);
    const diagnosis = await options.diagnose(monitor, run, excerpt, signal);
    return diagnosis ? { ...run, diagnosis } : run;
  } catch {
    // Diagnosis is advisory-only — a failed monitor run is still reported in
    // full even if the diagnosis call itself errors.
    return run;
  }
}

export interface DiagnosisCallResult {
  content: string;
  streamError: string | null;
}

/** The one-shot diagnosis prompt: the failure reason plus a bounded excerpt
 * of the console/network evidence already captured for this run. Exported
 * purely so it's independently testable, mirroring `riskJudge.ts`'s
 * `buildJudgeMessages`. */
export function buildDiagnosisMessages(monitor: SyntheticMonitor, run: MonitorRun, evidenceExcerpt: string): ChatMessage[] {
  return [
    {
      role: "system",
      content:
        "You are a terse site-reliability assistant diagnosing a failed synthetic monitor run for an autonomous monitoring agent. " +
        "Given the monitor's target, its failure reason, and a bounded excerpt of the browser console/network evidence captured at " +
        "the moment of failure, propose the single most likely root cause and one concrete next diagnostic step. " +
        "Reply with 2-4 short sentences of plain text only — no markdown, no lists, no preamble.",
    },
    {
      role: "user",
      content: [
        `Monitor: ${monitor.name}`,
        `Target (${monitor.targetEnv}): ${monitor.url}`,
        `Failure reason: ${run.failureReason ?? "(unknown)"}`,
        evidenceExcerpt ? `\n${evidenceExcerpt}` : "\n(No console/network evidence was captured for this run.)",
      ].join("\n"),
    },
  ];
}

/** Diagnoses a failed run via one one-shot, non-streaming, tool-less
 * `callModel` invocation, dependency-injected exactly like `riskJudge.ts`'s
 * `classifyToolCall` (see this module's doc comment for why). Fails closed
 * on anything malformed, errored, or slower than `DIAGNOSIS_TIMEOUT_MS`:
 * every one of those cases resolves `null`, never a fabricated diagnosis. */
export async function diagnoseMonitorFailure(
  monitor: SyntheticMonitor,
  run: MonitorRun,
  evidenceExcerpt: string,
  callModel: (messages: ChatMessage[], signal: AbortSignal) => Promise<DiagnosisCallResult>,
  signal?: AbortSignal,
): Promise<string | null> {
  const timeoutController = new AbortController();
  const timeoutId = setTimeout(() => timeoutController.abort(), DIAGNOSIS_TIMEOUT_MS);
  const onParentAbort = () => timeoutController.abort();
  if (signal) {
    if (signal.aborted) timeoutController.abort();
    else signal.addEventListener("abort", onParentAbort, { once: true });
  }
  try {
    const result = await callModel(buildDiagnosisMessages(monitor, run, evidenceExcerpt), timeoutController.signal);
    if (result.streamError) return null;
    const text = result.content.trim();
    return text || null;
  } catch {
    return null;
  } finally {
    clearTimeout(timeoutId);
    signal?.removeEventListener("abort", onParentAbort);
  }
}

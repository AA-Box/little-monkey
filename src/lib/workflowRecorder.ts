/**
 * Record and Replay Workflows — recording capture + draft conversion
 * (ROADMAP.md, Phase 1, "Record and Replay Workflows").
 *
 * This module owns the pure, framework-free logic for turning a live
 * Browser Workbench session into a reusable draft workflow:
 *
 *  - `createRecording` / `appendNavigateStep` / `appendClickStep` /
 *    `appendTypeStep` / `appendScrollStep` capture one ordered step at a
 *    time while a Browser Workbench session is being demonstrated. They are
 *    called by the UI layer (`BrowserWorkbench.tsx`) right after each real
 *    action already succeeds against the existing, exact-origin, disposable
 *    Chromium worker (`browserVerification.ts`) — no new browser automation
 *    is implemented here, only the record/convert/replay layer on top of it.
 *
 *  - `redactTypedValue` / `isCredentialLikeField` are the security-critical
 *    piece: any typed value that looks like a credential, token, or other
 *    secret is stripped *before* it is ever stored in a step, not merely
 *    masked in the UI later. Credential-like fields always become
 *    `sensitive`, `runtimeOnly` draft inputs with no stored default value —
 *    the only way to fill them in during replay is a fresh runtime prompt
 *    that is never persisted.
 *
 *  - `convertRecordingToDraft` turns the raw, ordered step list into a
 *    `DraftWorkflow`: named inputs, a preference for stable selectors
 *    (id / data-attr / aria-label) over brittle CSS paths, heuristically
 *    detected decision points (waits after a click that changed the page),
 *    and a trailing verification step. The result always starts in the
 *    `"draft"` status — nothing here ever marks a workflow `"enabled"`;
 *    that transition is a distinct, explicit user action
 *    (`workflowDraftStore.ts`'s `enableDraftWorkflow`).
 */

export type RecordedStepKind = "navigate" | "click" | "type" | "scroll";

export interface RecordedElementInfo {
  tag: string;
  role: string;
  ariaLabel: string;
  text: string;
}

export interface RecordedStep {
  id: string;
  kind: RecordedStepKind;
  atMs: number;
  url: string;
  selector: string | null;
  /** Typed value, already redacted by the time it lands here — `null`
   * whenever `valueRedacted` is true, and also `null` for non-"type" steps. */
  value: string | null;
  valueRedacted: boolean;
  element: RecordedElementInfo | null;
  scroll: { x: number; y: number } | null;
  screenshotArtifactId: string | null;
}

export interface BrowserRecording {
  id: string;
  runId: string;
  originUrl: string;
  startedAtMs: number;
  stoppedAtMs: number | null;
  steps: RecordedStep[];
}

// ---------------------------------------------------------------------------
// Credential redaction — the security-critical property of this feature.
// ---------------------------------------------------------------------------

const CREDENTIAL_FIELD_PATTERN =
  /password|passwd|pwd|secret|token|api[-_ ]?key|apikey|credit[-_ ]?card|card[-_ ]?number|\bcvv\b|\bcvc\b|\bssn\b|social[-_ ]?security|passcode|\bpin\b|\botp\b|auth[-_ ]?code|access[-_ ]?key|private[-_ ]?key|security[-_ ]?code/i;

export interface CredentialSignals {
  selector?: string | null;
  ariaLabel?: string | null;
  text?: string | null;
  fieldType?: string | null;
}

/** Extracts an HTML `type="..."` attribute embedded in a CSS selector, e.g.
 * `input[type="password"]` or `#login input[type=password]`. Best-effort:
 * the browser worker's `annotate` action does not expose DOM attributes
 * beyond tag/role/ariaLabel/text, so this is the only signal available for
 * a selector the operator wrote by hand. */
export function extractSelectorFieldType(selector: string | null | undefined): string | null {
  if (!selector) return null;
  const match = selector.match(/\[\s*type\s*=\s*["']?([a-z]+)["']?\s*\]/i);
  return match ? match[1].toLowerCase() : null;
}

/** True when any available signal about a typed-into field — its selector,
 * accessible label, visible text, or HTML `type` — looks like a credential,
 * token, or other secret. Deliberately over-inclusive: a false positive
 * only means an ordinary field becomes a named runtime input the user can
 * rename or reclassify during review; a false negative would leak a secret
 * into a persisted draft. */
export function isCredentialLikeField(signals: CredentialSignals): boolean {
  const fieldType = (signals.fieldType ?? extractSelectorFieldType(signals.selector))?.toLowerCase();
  if (fieldType === "password") return true;
  const haystacks = [signals.selector, signals.ariaLabel, signals.text].filter(
    (value): value is string => typeof value === "string" && value.length > 0,
  );
  return haystacks.some((value) => CREDENTIAL_FIELD_PATTERN.test(value));
}

export interface RedactedValue {
  value: string | null;
  redacted: boolean;
}

/** Redacts a typed value before it is ever stored in a recording. Credential
 * -like fields (per `isCredentialLikeField`) always come back with
 * `value: null` — never a masked/truncated echo of the secret — so nothing
 * derived from the real value ever reaches `localStorage`, a saved draft,
 * or a replay log. Call this at capture time, not just at review time. */
export function redactTypedValue(rawValue: string, signals: CredentialSignals): RedactedValue {
  if (isCredentialLikeField(signals)) return { value: null, redacted: true };
  return { value: rawValue, redacted: false };
}

// ---------------------------------------------------------------------------
// Stable selector preference.
// ---------------------------------------------------------------------------

export type SelectorStability = "id" | "data-attr" | "aria-label" | "css";

export function classifySelectorStability(selector: string): SelectorStability {
  const trimmed = selector.trim();
  if (/^#[A-Za-z][\w-]*$/.test(trimmed)) return "id";
  if (/\[\s*data-[\w-]+/i.test(trimmed)) return "data-attr";
  if (/\[\s*aria-label\s*=/i.test(trimmed)) return "aria-label";
  return "css";
}

export interface PreferredSelector {
  selector: string;
  stability: SelectorStability;
  /** True when `selector` was synthesized from element metadata rather than
   * the raw selector the operator typed (only possible for aria-label). */
  synthesized: boolean;
  /** The originally recorded selector, always kept around so the review UI
   * can show what was actually exercised during recording. */
  recordedSelector: string;
}

function escapeAttributeValue(value: string): string {
  return value.replace(/\\/g, "\\\\").replace(/"/g, '\\"');
}

/** Prefers a stable selector (id / data-attr / aria-label) over a brittle
 * CSS path. When the recorded selector is already stable it is kept as-is;
 * when it is a brittle CSS path but the element carries an `aria-label`,
 * a `[aria-label="..."]` selector is synthesized instead. Otherwise the
 * original brittle selector is kept, flagged `"css"` so the review UI can
 * warn the user. */
export function preferStableSelector(selector: string, element: RecordedElementInfo | null): PreferredSelector {
  const stability = classifySelectorStability(selector);
  if (stability !== "css") {
    return { selector, stability, synthesized: false, recordedSelector: selector };
  }
  if (element?.ariaLabel) {
    const tag = element.tag || "*";
    return {
      selector: `${tag}[aria-label="${escapeAttributeValue(element.ariaLabel)}"]`,
      stability: "aria-label",
      synthesized: true,
      recordedSelector: selector,
    };
  }
  return { selector, stability: "css", synthesized: false, recordedSelector: selector };
}

// ---------------------------------------------------------------------------
// Recording capture.
// ---------------------------------------------------------------------------

function newId(): string {
  return crypto.randomUUID();
}

export function createRecording(runId: string, originUrl: string, now: number = Date.now()): BrowserRecording {
  return { id: newId(), runId, originUrl, startedAtMs: now, stoppedAtMs: null, steps: [] };
}

export function stopRecording(recording: BrowserRecording, now: number = Date.now()): BrowserRecording {
  if (recording.stoppedAtMs !== null) return recording;
  return { ...recording, stoppedAtMs: now };
}

function appendStep(recording: BrowserRecording, step: RecordedStep): BrowserRecording {
  return { ...recording, steps: [...recording.steps, step] };
}

export function appendNavigateStep(
  recording: BrowserRecording,
  input: { url: string; screenshotArtifactId: string | null },
  now: number = Date.now(),
): BrowserRecording {
  return appendStep(recording, {
    id: newId(),
    kind: "navigate",
    atMs: now,
    url: input.url,
    selector: null,
    value: null,
    valueRedacted: false,
    element: null,
    scroll: null,
    screenshotArtifactId: input.screenshotArtifactId,
  });
}

export function appendClickStep(
  recording: BrowserRecording,
  input: { url: string; selector: string; element: RecordedElementInfo | null; screenshotArtifactId: string | null },
  now: number = Date.now(),
): BrowserRecording {
  return appendStep(recording, {
    id: newId(),
    kind: "click",
    atMs: now,
    url: input.url,
    selector: input.selector,
    value: null,
    valueRedacted: false,
    element: input.element,
    scroll: null,
    screenshotArtifactId: input.screenshotArtifactId,
  });
}

/** Appends a "type" step, redacting `rawValue` before it ever touches the
 * returned recording — see `redactTypedValue`. */
export function appendTypeStep(
  recording: BrowserRecording,
  input: {
    url: string;
    selector: string;
    rawValue: string;
    element: RecordedElementInfo | null;
    screenshotArtifactId: string | null;
  },
  now: number = Date.now(),
): BrowserRecording {
  const redaction = redactTypedValue(input.rawValue, {
    selector: input.selector,
    ariaLabel: input.element?.ariaLabel,
    text: input.element?.text,
  });
  return appendStep(recording, {
    id: newId(),
    kind: "type",
    atMs: now,
    url: input.url,
    selector: input.selector,
    value: redaction.value,
    valueRedacted: redaction.redacted,
    element: input.element,
    scroll: null,
    screenshotArtifactId: input.screenshotArtifactId,
  });
}

export function appendScrollStep(
  recording: BrowserRecording,
  input: { url: string; x: number; y: number; screenshotArtifactId: string | null },
  now: number = Date.now(),
): BrowserRecording {
  return appendStep(recording, {
    id: newId(),
    kind: "scroll",
    atMs: now,
    url: input.url,
    selector: null,
    value: null,
    valueRedacted: false,
    element: null,
    scroll: { x: input.x, y: input.y },
    screenshotArtifactId: input.screenshotArtifactId,
  });
}

// ---------------------------------------------------------------------------
// Draft workflow conversion.
// ---------------------------------------------------------------------------

export type DraftWorkflowStatus = "draft" | "enabled" | "archived";

export interface DraftWorkflowInput {
  id: string;
  /** Stable machine name, e.g. `"username"`. */
  name: string;
  label: string;
  /** Credential-like fields are always `sensitive` and force `runtimeOnly`.
   * The user can additionally mark any ordinary field as a runtime input
   * during review. */
  sensitive: boolean;
  /** When true, replay must prompt for this value fresh every run and the
   * value is never persisted in the draft (`defaultValue` stays `null`). */
  runtimeOnly: boolean;
  /** Only ever set for non-sensitive, non-runtime-only inputs — the literal
   * value captured during recording, reused unless the user overrides it. */
  defaultValue: string | null;
  sourceStepId: string;
}

export type DraftWorkflowStepAction =
  | { type: "navigate"; url: string }
  | { type: "click"; selector: string; selectorStability: SelectorStability; recordedSelector: string; description: string }
  | {
      type: "type";
      selector: string;
      selectorStability: SelectorStability;
      recordedSelector: string;
      description: string;
      inputId: string;
    }
  | { type: "scroll"; x: number; y: number }
  | { type: "waitForSelector"; selector: string | null; reason: string; timeoutMs: number }
  | { type: "verify"; description: string; expectedUrlPrefix: string | null };

export interface DraftWorkflowStep {
  id: string;
  action: DraftWorkflowStepAction;
  sourceStepId: string | null;
}

export interface DraftWorkflow {
  id: string;
  name: string;
  status: DraftWorkflowStatus;
  createdAt: number;
  updatedAt: number;
  reviewedAt: number | null;
  sourceRecordingId: string;
  originUrl: string;
  inputs: DraftWorkflowInput[];
  steps: DraftWorkflowStep[];
}

/** A pause longer than this between two recorded steps is treated as a
 * signal the operator was waiting for something (a navigation, an async
 * load) rather than acting instantly — the converter turns it into an
 * explicit `waitForSelector` decision point instead of baking in a fixed
 * delay. */
export const DECISION_POINT_GAP_MS = 1_500;

function elementDescription(kind: RecordedStepKind, element: RecordedElementInfo | null, selector: string): string {
  const label = element?.ariaLabel || element?.text?.slice(0, 60) || selector;
  return kind === "click" ? `Click "${label}"` : `Type into "${label}"`;
}

function inputNameFor(index: number, element: RecordedElementInfo | null, sensitive: boolean): string {
  const base = element?.ariaLabel || element?.text?.slice(0, 24) || (sensitive ? "secret" : "value");
  const slug = base
    .toLowerCase()
    .trim()
    .replace(/[^a-z0-9]+/g, "_")
    .replace(/^_+|_+$/g, "")
    .slice(0, 40);
  return slug ? `${slug}_${index}` : `input_${index}`;
}

/**
 * Converts a stopped recording into a draft workflow. Always returns
 * `status: "draft"` — nothing here ever enables replay. Every "type" step
 * becomes a named input: credential-like steps (already redacted at record
 * time, so `value` is `null`) become `sensitive`, `runtimeOnly` inputs with
 * no default; ordinary typed steps become editable, non-runtime inputs
 * seeded with the recorded literal value.
 */
export function convertRecordingToDraft(
  recording: BrowserRecording,
  options: { name?: string; now?: number } = {},
): DraftWorkflow {
  const now = options.now ?? Date.now();
  const inputs: DraftWorkflowInput[] = [];
  const steps: DraftWorkflowStep[] = [];
  let inputCounter = 0;

  const orderedSteps = recording.steps;
  for (let index = 0; index < orderedSteps.length; index += 1) {
    const recorded = orderedSteps[index];
    const previous = index > 0 ? orderedSteps[index - 1] : null;

    // Decision point: insert a wait when this step follows a click that
    // plausibly changed the page (a navigation, a different URL, or a long
    // pause before the next demonstrated action).
    if (previous && previous.kind === "click") {
      const urlChanged = previous.url !== recorded.url;
      const longGap = recorded.atMs - previous.atMs >= DECISION_POINT_GAP_MS;
      if (urlChanged || longGap || recorded.kind === "navigate") {
        steps.push({
          id: newId(),
          sourceStepId: recorded.id,
          action: {
            type: "waitForSelector",
            selector: recorded.selector,
            reason: urlChanged
              ? "The recorded click changed the page; wait for the new page's target before continuing."
              : "A pause was observed after the recorded click; wait for the target instead of a fixed delay.",
            timeoutMs: 8_000,
          },
        });
      }
    }

    if (recorded.kind === "navigate") {
      steps.push({ id: newId(), sourceStepId: recorded.id, action: { type: "navigate", url: recorded.url } });
      continue;
    }
    if (recorded.kind === "scroll" && recorded.scroll) {
      steps.push({
        id: newId(),
        sourceStepId: recorded.id,
        action: { type: "scroll", x: recorded.scroll.x, y: recorded.scroll.y },
      });
      continue;
    }
    if (recorded.kind === "click" && recorded.selector) {
      const preferred = preferStableSelector(recorded.selector, recorded.element);
      steps.push({
        id: newId(),
        sourceStepId: recorded.id,
        action: {
          type: "click",
          selector: preferred.selector,
          selectorStability: preferred.stability,
          recordedSelector: preferred.recordedSelector,
          description: elementDescription("click", recorded.element, recorded.selector),
        },
      });
      continue;
    }
    if (recorded.kind === "type" && recorded.selector) {
      const preferred = preferStableSelector(recorded.selector, recorded.element);
      const sensitive = recorded.valueRedacted;
      inputCounter += 1;
      const inputId = newId();
      inputs.push({
        id: inputId,
        name: inputNameFor(inputCounter, recorded.element, sensitive),
        label: recorded.element?.ariaLabel || elementDescription("type", recorded.element, recorded.selector),
        sensitive,
        runtimeOnly: sensitive,
        defaultValue: sensitive ? null : recorded.value,
        sourceStepId: recorded.id,
      });
      steps.push({
        id: newId(),
        sourceStepId: recorded.id,
        action: {
          type: "type",
          selector: preferred.selector,
          selectorStability: preferred.stability,
          recordedSelector: preferred.recordedSelector,
          description: elementDescription("type", recorded.element, recorded.selector),
          inputId,
        },
      });
      continue;
    }
  }

  const lastStep = orderedSteps[orderedSteps.length - 1];
  const finalUrl = lastStep?.url ?? recording.originUrl;
  let expectedUrlPrefix: string | null = null;
  try {
    const parsed = new URL(finalUrl);
    expectedUrlPrefix = `${parsed.origin}${parsed.pathname}`;
  } catch {
    expectedUrlPrefix = null;
  }
  steps.push({
    id: newId(),
    sourceStepId: lastStep?.id ?? null,
    action: {
      type: "verify",
      description: "Confirm replay reached the same page the recording ended on.",
      expectedUrlPrefix,
    },
  });

  return {
    id: newId(),
    name: options.name?.trim() || `Recorded workflow — ${new Date(now).toLocaleString()}`,
    status: "draft",
    createdAt: now,
    updatedAt: now,
    reviewedAt: null,
    sourceRecordingId: recording.id,
    originUrl: recording.originUrl,
    inputs,
    steps,
  };
}

/** Asserts no `sensitive` input in `draft` carries a stored default value —
 * the invariant credential redaction exists to guarantee. Intended for
 * tests and as a defensive check before persisting a draft. */
export function assertNoStoredSecrets(draft: DraftWorkflow): void {
  for (const input of draft.inputs) {
    if (input.sensitive && (input.defaultValue !== null || !input.runtimeOnly)) {
      throw new Error(`Sensitive draft input "${input.name}" must have no stored default and be runtime-only.`);
    }
  }
}

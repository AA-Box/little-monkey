import { useState } from "react";
import { AlertTriangle, Ban, Check, KeyRound, MousePointerClick, Navigation, ShieldCheck, TextCursorInput, Clock, X } from "lucide-react";

import type { DraftWorkflow, DraftWorkflowInput, DraftWorkflowStep } from "../../lib/workflowRecorder";
import { useWorkflowDraftStore } from "../../store/workflowDraftStore";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

interface WorkflowDraftReviewProps {
  /** A freshly converted draft (from `convertRecordingToDraft`) or an
   * already-saved one reopened for another look — either way this
   * component never persists anything until the user clicks a footer
   * button. */
  initialDraft: DraftWorkflow;
  onDiscard: () => void;
  onSaved: (draft: DraftWorkflow) => void;
}

function stepIcon(step: DraftWorkflowStep) {
  switch (step.action.type) {
    case "navigate":
      return <Navigation size={13} />;
    case "click":
      return <MousePointerClick size={13} />;
    case "type":
      return <TextCursorInput size={13} />;
    case "scroll":
      return <MousePointerClick size={13} className="rotate-90" />;
    case "waitForSelector":
      return <Clock size={13} />;
    case "verify":
      return <ShieldCheck size={13} />;
  }
}

function stepLabel(step: DraftWorkflowStep, inputs: DraftWorkflowInput[]): string {
  const action = step.action;
  switch (action.type) {
    case "navigate":
      return `Navigate to ${action.url}`;
    case "click":
      return action.description;
    case "type": {
      const input = inputs.find((entry) => entry.id === action.inputId);
      return `${action.description} → {{${input?.name ?? "value"}}}`;
    }
    case "scroll":
      return `Scroll to (${action.x}, ${action.y})`;
    case "waitForSelector":
      return `Wait: ${action.reason}`;
    case "verify":
      return action.description;
  }
}

function selectorBadge(step: DraftWorkflowStep): { label: string; tone: "success" | "warning" } | null {
  const action = step.action;
  if (action.type !== "click" && action.type !== "type") return null;
  if (action.selectorStability === "css") return { label: "brittle selector — review", tone: "warning" };
  return { label: action.selectorStability, tone: "success" };
}

/**
 * The one required stop between a raw recording and a replayable workflow —
 * ROADMAP.md's "Require user review before enabling replay." Nothing
 * upstream of this component can mark a draft `"enabled"`: `saveDraft`
 * always persists `status: "draft"`, and `workflowDraftStore.enableDraft`
 * refuses unless `markReviewed` has already run. Only this panel's "Save &
 * enable replay" button calls both.
 */
export function WorkflowDraftReview({ initialDraft, onDiscard, onSaved }: WorkflowDraftReviewProps) {
  const [draft, setDraft] = useState<DraftWorkflow>(initialDraft);
  const [error, setError] = useState<string | null>(null);

  function updateInput(inputId: string, patch: Partial<DraftWorkflowInput>) {
    setDraft((current) => ({
      ...current,
      inputs: current.inputs.map((input) => (input.id === inputId ? { ...input, ...patch } : input)),
    }));
  }

  function setRuntimeOnly(input: DraftWorkflowInput, runtimeOnly: boolean) {
    if (input.sensitive && !runtimeOnly) return; // A credential can never leave runtime-only.
    updateInput(input.id, { runtimeOnly, defaultValue: runtimeOnly ? null : input.defaultValue });
  }

  function setDefaultValue(input: DraftWorkflowInput, value: string) {
    if (input.sensitive || input.runtimeOnly) return;
    updateInput(input.id, { defaultValue: value });
  }

  function renameInput(input: DraftWorkflowInput, name: string) {
    const safe = name.trim().replace(/[^a-zA-Z0-9_]+/g, "_").slice(0, 60);
    if (!safe) return;
    updateInput(input.id, { name: safe });
  }

  function persist(enable: boolean) {
    setError(null);
    try {
      useWorkflowDraftStore.getState().saveDraft(draft);
      if (enable) {
        useWorkflowDraftStore.getState().markReviewed(draft.id);
        useWorkflowDraftStore.getState().enableDraft(draft.id);
      }
      const saved = useWorkflowDraftStore.getState().drafts.find((entry) => entry.id === draft.id);
      onSaved(saved ?? draft);
    } catch (cause) {
      setError(errorMessage(cause));
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/50 p-4" role="dialog" aria-modal="true" aria-labelledby="workflow-review-title">
      <div className="flex max-h-[85vh] w-full max-w-2xl flex-col overflow-hidden rounded-xl border border-border bg-background shadow-xl">
        <header className="flex items-center justify-between gap-3 border-b border-border px-4 py-3">
          <div className="min-w-0">
            <h2 id="workflow-review-title" className="text-sm font-semibold">Review recorded workflow</h2>
            <p className="mt-0.5 text-xs text-muted">Nothing replays until you explicitly save and enable it below.</p>
          </div>
          <button type="button" aria-label="Discard recording" onClick={onDiscard} className="rounded-md p-1.5 text-muted hover:bg-surface-2 hover:text-foreground">
            <X size={16} />
          </button>
        </header>

        <div className="min-h-0 flex-1 overflow-auto p-4">
          <label className="block text-xs text-muted">
            Workflow name
            <input
              value={draft.name}
              onChange={(event) => setDraft((current) => ({ ...current, name: event.target.value }))}
              className="mt-1 w-full rounded-md border border-border bg-surface px-3 py-2 text-sm text-foreground"
            />
          </label>

          <h3 className="mt-4 text-xs font-semibold uppercase tracking-wide text-muted">Inputs ({draft.inputs.length})</h3>
          {draft.inputs.length === 0 && <p className="mt-1 text-xs text-faint">This recording did not type into any fields.</p>}
          <div className="mt-2 space-y-2">
            {draft.inputs.map((input) => (
              <div key={input.id} className={`rounded-lg border p-2.5 ${input.sensitive ? "border-warning/40 bg-warning/10" : "border-border bg-surface"}`}>
                <div className="flex flex-wrap items-center gap-2">
                  {input.sensitive && <KeyRound size={13} className="shrink-0 text-warning" />}
                  <input
                    value={input.name}
                    onChange={(event) => renameInput(input, event.target.value)}
                    aria-label={`Input name for ${input.label}`}
                    className="min-w-0 flex-1 rounded-md border border-border bg-background px-2 py-1 font-mono text-xs text-foreground"
                  />
                  <label className="flex shrink-0 items-center gap-1.5 text-[11px] text-muted">
                    <input
                      type="checkbox"
                      checked={input.runtimeOnly}
                      disabled={input.sensitive}
                      onChange={(event) => setRuntimeOnly(input, event.target.checked)}
                    />
                    Runtime input (never stored)
                  </label>
                </div>
                <p className="mt-1 text-[11px] text-faint">{input.label}</p>
                {input.sensitive ? (
                  <p className="mt-1.5 text-[11px] text-warning">Detected as a credential — redacted at capture time. Always prompted fresh at replay, never saved.</p>
                ) : !input.runtimeOnly ? (
                  <label className="mt-1.5 block text-[11px] text-muted">
                    Default value (reused every replay unless overridden)
                    <input
                      value={input.defaultValue ?? ""}
                      onChange={(event) => setDefaultValue(input, event.target.value)}
                      className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
                    />
                  </label>
                ) : (
                  <p className="mt-1.5 text-[11px] text-faint">Will be prompted fresh at the start of every replay.</p>
                )}
              </div>
            ))}
          </div>

          <h3 className="mt-4 text-xs font-semibold uppercase tracking-wide text-muted">Steps ({draft.steps.length})</h3>
          <ol className="mt-2 space-y-1.5">
            {draft.steps.map((step, index) => {
              const badge = selectorBadge(step);
              return (
                <li key={step.id} className="flex items-start gap-2 rounded-md border border-border bg-surface px-2.5 py-1.5 text-xs">
                  <span className="mt-0.5 text-faint">{index + 1}.</span>
                  <span className="mt-0.5 shrink-0 text-muted">{stepIcon(step)}</span>
                  <span className="min-w-0 flex-1 break-words text-foreground">{stepLabel(step, draft.inputs)}</span>
                  {badge && (
                    <span className={`inline-flex shrink-0 items-center gap-1 rounded-full px-1.5 py-0.5 text-[10px] font-medium ${badge.tone === "success" ? "bg-success-soft text-success" : "bg-warning-soft text-warning"}`}>
                      {badge.tone === "warning" && <AlertTriangle size={10} />}
                      {badge.label}
                    </span>
                  )}
                </li>
              );
            })}
          </ol>

          {error && <p role="alert" className="mt-3 rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">{error}</p>}
        </div>

        <footer className="flex flex-wrap items-center justify-end gap-2 border-t border-border px-4 py-3">
          <Button variant="ghost" onClick={onDiscard}><Ban size={13} />Discard</Button>
          <Button variant="secondary" onClick={() => persist(false)}>Save as draft</Button>
          <Button variant="primary" onClick={() => persist(true)}><Check size={13} />Save &amp; enable replay</Button>
        </footer>
      </div>
    </div>
  );
}

export default WorkflowDraftReview;

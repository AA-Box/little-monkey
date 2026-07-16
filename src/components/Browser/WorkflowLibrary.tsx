import { useEffect, useState } from "react";
import {
  Ban,
  CheckCircle2,
  Clock,
  KeyRound,
  Loader2,
  Pencil,
  Play,
  ShieldOff,
  Square,
  Trash2,
  Workflow,
  XCircle,
} from "lucide-react";

import { artifactDataUrl, readDurableArtifact } from "../../lib/durableArtifacts";
import type { DraftWorkflow, DraftWorkflowStatus } from "../../lib/workflowRecorder";
import { cancelRegisteredRun } from "../../lib/runCancellationRegistry";
import { startWorkflowReplay, type ReplayRun, type ReplayStepLog } from "../../lib/workflowReplay";
import { useWorkflowDraftStore } from "../../store/workflowDraftStore";
import { Button, IconButton, StatusPill } from "../ui";
import { WorkflowDraftReview } from "./WorkflowDraftReview";

const STATUS_TONE: Record<DraftWorkflowStatus, "neutral" | "success" | "warning"> = {
  draft: "warning",
  enabled: "success",
  archived: "neutral",
};

function StepEvidenceThumbnail({ artifactId }: { artifactId: string | null }) {
  const [url, setUrl] = useState<string | null>(null);
  useEffect(() => {
    let current = true;
    if (!artifactId) {
      setUrl(null);
      return;
    }
    void readDurableArtifact(artifactId)
      .then((content) => current && setUrl(artifactDataUrl("image/png", content.contentBase64)))
      .catch(() => current && setUrl(null));
    return () => {
      current = false;
    };
  }, [artifactId]);
  if (!artifactId || !url) return null;
  return <img src={url} alt="Replay step screenshot evidence" className="mt-1.5 max-h-28 rounded-md border border-border bg-black/80 object-contain" />;
}

function StepStatusIcon({ status }: { status: ReplayStepLog["status"] }) {
  switch (status) {
    case "pending":
      return <Clock size={13} className="text-faint" />;
    case "running":
      return <Loader2 size={13} className="animate-spin text-accent" />;
    case "success":
      return <CheckCircle2 size={13} className="text-success" />;
    case "failed":
      return <XCircle size={13} className="text-danger" />;
    case "cancelled":
      return <Ban size={13} className="text-muted" />;
  }
}

function RunLog({ run }: { run: ReplayRun }) {
  return (
    <div className="mt-3 space-y-1.5 border-t border-border pt-3">
      {run.steps.map((step, index) => (
        <div key={step.stepId} className="rounded-md border border-border bg-background px-2.5 py-1.5 text-xs">
          <div className="flex items-center gap-2">
            <span className="text-faint">{index + 1}.</span>
            <StepStatusIcon status={step.status} />
            <span className="min-w-0 flex-1 truncate text-foreground">{step.detail || step.status}</span>
          </div>
          {step.error && <p className="mt-1 text-[11px] text-danger">{step.error}</p>}
          <StepEvidenceThumbnail artifactId={step.screenshotArtifactId} />
        </div>
      ))}
    </div>
  );
}

function RuntimeInputForm({
  draft,
  onCancel,
  onRun,
}: {
  draft: DraftWorkflow;
  onCancel: () => void;
  onRun: (values: Record<string, string>) => void;
}) {
  const runtimeInputs = draft.inputs.filter((input) => input.runtimeOnly);
  const [values, setValues] = useState<Record<string, string>>({});
  const missing = runtimeInputs.some((input) => !values[input.id]);
  return (
    <div className="mt-3 space-y-2 border-t border-border pt-3">
      {runtimeInputs.length === 0 ? (
        <p className="text-xs text-faint">This workflow has no runtime inputs — it will replay with its recorded defaults.</p>
      ) : (
        runtimeInputs.map((input) => (
          <label key={input.id} className="block text-xs text-muted">
            <span className="flex items-center gap-1">{input.sensitive && <KeyRound size={11} />} {input.label || input.name} (never stored)</span>
            <input
              type={input.sensitive ? "password" : "text"}
              value={values[input.id] ?? ""}
              onChange={(event) => setValues((current) => ({ ...current, [input.id]: event.target.value }))}
              autoComplete="off"
              className="mt-1 w-full rounded-md border border-border bg-background px-2 py-1.5 text-xs text-foreground"
            />
          </label>
        ))
      )}
      <div className="flex gap-2">
        <Button size="sm" variant="ghost" onClick={onCancel}>Cancel</Button>
        <Button size="sm" variant="primary" disabled={missing} onClick={() => onRun(values)}>
          <Play size={12} />Run now
        </Button>
      </div>
    </div>
  );
}

/**
 * Saved workflow library: enable / disable / re-review / delete drafts, and
 * run an enabled one — prompting fresh for every runtime input (never
 * persisted) and showing a live, cancellable, per-step evidence log.
 * ROADMAP.md acceptance: "Replayed actions are logged with screenshot/action
 * evidence and can be cancelled."
 */
export function WorkflowLibrary() {
  const drafts = useWorkflowDraftStore((state) => state.drafts.filter((draft) => draft.status !== "archived"));
  const [reviewing, setReviewing] = useState<DraftWorkflow | null>(null);
  const [promptingId, setPromptingId] = useState<string | null>(null);
  const [runByDraftId, setRunByDraftId] = useState<Record<string, ReplayRun>>({});
  // draftId -> runId. A record (not a single value) so cancelling or
  // tracking one draft's replay doesn't get clobbered by starting another
  // draft's replay at the same time.
  const [activeRuns, setActiveRuns] = useState<Record<string, string>>({});

  function runDraft(draft: DraftWorkflow, runtimeInputs: Record<string, string>) {
    setPromptingId(null);
    const handle = startWorkflowReplay(draft, {
      runtimeInputs,
      onEvent: (event) => setRunByDraftId((current) => ({ ...current, [draft.id]: event.run })),
    });
    setActiveRuns((current) => ({ ...current, [draft.id]: handle.runId }));
    void handle.done.finally(() => {
      setActiveRuns((current) => {
        if (current[draft.id] !== handle.runId) return current;
        const { [draft.id]: _removed, ...rest } = current;
        return rest;
      });
    });
  }

  if (drafts.length === 0 && !reviewing) {
    return (
      <div className="flex flex-1 flex-col items-center justify-center rounded-xl border border-dashed border-border p-8 text-center">
        <Workflow size={26} className="text-faint" />
        <h3 className="mt-3 text-sm font-medium">No recorded workflows yet</h3>
        <p className="mt-1 max-w-md text-xs leading-5 text-muted">
          Start a browser session, turn on Record, demonstrate the steps once, then review and save the draft it produces.
        </p>
      </div>
    );
  }

  return (
    <div className="space-y-3">
      {drafts.map((draft) => {
        const run = runByDraftId[draft.id];
        const runningRunId = activeRuns[draft.id];
        const isRunning = Boolean(runningRunId);
        return (
          <div key={draft.id} className="rounded-xl border border-border bg-surface p-3">
            <div className="flex flex-wrap items-start justify-between gap-2">
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <h4 className="truncate text-sm font-medium">{draft.name}</h4>
                  <StatusPill tone={STATUS_TONE[draft.status]}>{draft.status}</StatusPill>
                </div>
                <p className="mt-0.5 truncate text-[11px] text-faint">{draft.originUrl}</p>
                <p className="mt-0.5 text-[11px] text-faint">
                  {draft.steps.length} step(s) · {draft.inputs.length} input(s){draft.inputs.some((i) => i.sensitive) ? " · credential redacted" : ""}
                </p>
              </div>
              <div className="flex shrink-0 flex-wrap gap-1.5">
                {draft.status === "enabled" && !isRunning && (
                  <Button size="sm" variant="primary" onClick={() => setPromptingId(draft.id)}>
                    <Play size={12} />Run
                  </Button>
                )}
                {isRunning && (
                  <Button size="sm" variant="danger" onClick={() => runningRunId && cancelRegisteredRun(runningRunId)}>
                    <Square size={12} />Cancel
                  </Button>
                )}
                <IconButton size="sm" aria-label={`Review ${draft.name}`} onClick={() => setReviewing(draft)}>
                  <Pencil size={13} />
                </IconButton>
                {draft.status === "enabled" && (
                  <IconButton size="sm" aria-label={`Disable ${draft.name}`} onClick={() => useWorkflowDraftStore.getState().disableDraft(draft.id)}>
                    <ShieldOff size={13} />
                  </IconButton>
                )}
                <IconButton
                  size="sm"
                  variant="danger"
                  aria-label={`Delete ${draft.name}`}
                  onClick={() => {
                    if (window.confirm(`Delete the saved workflow "${draft.name}"? This cannot be undone.`)) {
                      useWorkflowDraftStore.getState().deleteDraft(draft.id);
                    }
                  }}
                >
                  <Trash2 size={13} />
                </IconButton>
              </div>
            </div>

            {promptingId === draft.id && (
              <RuntimeInputForm draft={draft} onCancel={() => setPromptingId(null)} onRun={(values) => runDraft(draft, values)} />
            )}
            {run && <RunLog run={run} />}
          </div>
        );
      })}

      {reviewing && (
        <WorkflowDraftReview
          initialDraft={reviewing}
          onDiscard={() => setReviewing(null)}
          onSaved={() => setReviewing(null)}
        />
      )}
    </div>
  );
}

export default WorkflowLibrary;

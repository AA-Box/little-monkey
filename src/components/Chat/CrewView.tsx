import { useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  CopyPlus,
  RotateCcw,
  ShieldCheck,
  Square,
  UsersRound,
  Wrench,
} from "lucide-react";
import ReactMarkdown from "react-markdown";

import { cancelCrewRun, crewActorPlainOutput, retryCrewRun } from "../../lib/crewRunner";
import type { CrewActorRun, CrewActorStatus, CrewRunStatus } from "../../lib/crewTypes";
import { useT } from "../../lib/i18n";
import { useSessionStore } from "../../store/sessionStore";
import { Button } from "../ui";
import { markdownComponents, PROSE_CLASSES } from "./MessageBubble";
import { errorMessage } from "../../lib/errors";
import { formatDuration } from "../../lib/format";

interface CrewViewProps {
  sessionId: string;
}

function statusTone(status: CrewActorStatus | CrewRunStatus): string {
  if (status === "completed") return "border-success/40 bg-success-soft text-success";
  if (status === "failed") return "border-danger/40 bg-danger-soft text-danger";
  if (status === "cancelled") return "border-warning/40 bg-warning-soft text-warning";
  if (status === "running") return "border-accent/40 bg-accent-soft text-accent";
  return "border-border bg-surface-2 text-muted";
}


function ActorCard({ actor }: { actor: CrewActorRun }) {
  const { t } = useT();
  return (
    <section className="overflow-hidden rounded-xl border border-border bg-background" aria-label={actor.name}>
      <header className="flex flex-wrap items-start justify-between gap-2 border-b border-border bg-surface px-3 py-2.5">
        <div className="min-w-0">
          <div className="flex min-w-0 items-center gap-2">
            <h3 className="truncate text-sm font-semibold text-foreground">{actor.name}</h3>
            <span className="shrink-0 text-[10px] font-medium uppercase tracking-wider text-faint">
              {t(actor.kind === "coordinator" ? "CrewView.coordinator" : "CrewView.member")}
            </span>
          </div>
          <p className="mt-0.5 line-clamp-2 text-xs text-muted">{actor.role}</p>
        </div>
        <span
          className={`rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase ${statusTone(actor.status)}`}
          role="status"
          aria-live="polite"
        >
          {t(`CrewView.status.${actor.status}`)}
        </span>
      </header>

      <div className="space-y-3 p-3">
        <div className="grid grid-cols-2 gap-x-3 gap-y-1 text-[11px] sm:grid-cols-4">
          <div><span className="text-faint">{t("CrewView.model")}</span><p className="truncate font-medium text-foreground" title={`${actor.modelTarget.label} · ${actor.modelTarget.displayName}`}>{actor.modelTarget.displayName}</p></div>
          <div><span className="text-faint">{t("CrewView.persona")}</span><p className="truncate font-medium text-foreground">{actor.persona?.name ?? t("CrewView.none")}</p></div>
          <div><span className="text-faint">{t("CrewView.context")}</span><p className="font-medium text-foreground">{t(`CrewView.contextPolicy.${actor.contextPolicy}`)}</p></div>
          <div><span className="text-faint">{t("CrewView.duration")}</span><p className="font-medium tabular-nums text-foreground">{formatDuration(actor.durationMs)}</p></div>
        </div>

        <div className="flex flex-wrap gap-1.5 text-[10px]">
          <span className="inline-flex items-center gap-1 rounded-full border border-border bg-surface px-2 py-0.5 text-muted">
            <ShieldCheck size={10} aria-hidden="true" />
            {t("CrewView.readOnly")}
          </span>
          <span className="rounded-full border border-border bg-surface px-2 py-0.5 tabular-nums text-muted">
            {t("CrewView.calls", { count: actor.modelCalls })}
          </span>
          <span className="rounded-full border border-border bg-surface px-2 py-0.5 tabular-nums text-muted">
            {t("CrewView.tokens", { count: actor.usage.totalTokens })}
          </span>
          <span className="rounded-full border border-border bg-surface px-2 py-0.5 tabular-nums text-muted">
            {t("CrewView.cost", { cost: actor.estimatedCostUsd.toFixed(4) })}
          </span>
          <span className="rounded-full border border-border bg-surface px-2 py-0.5 tabular-nums text-muted">
            {t("CrewView.permissions", { count: actor.permissions.length })}
          </span>
        </div>

        {(() => {
          // `crewActorPlainOutput` falls back to the member's raw text when
          // no explicit report was parsed — without it, a member that
          // answered plainly (or a coordinator that skipped the report
          // format) rendered as an empty card even though it produced real
          // output.
          const output = crewActorPlainOutput(actor);
          if (!output) return null;
          return (
            <div>
              <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-faint">
                {actor.report ? t("CrewView.explicitReport") : t("CrewView.plainOutput")}
              </p>
              <div className={`${PROSE_CLASSES} max-w-none text-sm`}>
                <ReactMarkdown components={markdownComponents}>{output}</ReactMarkdown>
              </div>
            </div>
          );
        })()}

        {actor.toolRequests.length > 0 && (
          <div>
            <p className="mb-1 text-[10px] font-semibold uppercase tracking-wider text-faint">{t("CrewView.toolRequests")}</p>
            <div className="space-y-1">
              {actor.toolRequests.map((request) => (
                <div key={request.id} className="flex items-start justify-between gap-2 rounded-md border border-border bg-surface px-2 py-1.5 text-[11px]">
                  <span className="min-w-0">
                    <span className="flex items-center gap-1 font-medium text-foreground"><Wrench size={10} aria-hidden="true" />{request.name}</span>
                    <span className="block truncate font-mono text-faint" title={request.arguments}>{request.arguments || "{}"}</span>
                  </span>
                  <span className={`shrink-0 rounded-full border px-1.5 py-0.5 text-[9px] font-semibold uppercase ${
                    request.status === "completed" ? statusTone("completed")
                      : request.status === "blocked" || request.status === "failed" ? statusTone("failed")
                        : request.status === "cancelled" ? statusTone("cancelled") : statusTone("running")
                  }`}>{request.status}</span>
                </div>
              ))}
            </div>
          </div>
        )}

        {actor.mutationProposals.length > 0 && (
          <div className="rounded-md border border-warning/40 bg-warning-soft px-2.5 py-2">
            <p className="text-[10px] font-semibold uppercase tracking-wider text-warning">{t("CrewView.mutationsTitle")}</p>
            <ul className="mt-1 space-y-1 text-xs text-foreground">
              {actor.mutationProposals.map((proposal) => <li key={proposal.id}>• {proposal.summary}</li>)}
            </ul>
          </div>
        )}

        {actor.error && (
          <p className="rounded-md border border-danger/40 bg-danger-soft px-2.5 py-2 text-xs text-danger">
            {actor.error}
          </p>
        )}

        {actor.transcript.length > 0 && (
          <details className="rounded-md border border-border bg-surface">
            <summary className="cursor-pointer px-2.5 py-1.5 text-[11px] font-medium text-muted focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent">
              {t("CrewView.privateTranscript", { count: actor.transcript.length })}
            </summary>
            <div className="max-h-56 space-y-1 overflow-y-auto border-t border-border p-2 [overscroll-behavior:contain]">
              {actor.transcript.map((entry) => (
                <div key={entry.id} className="rounded bg-background px-2 py-1.5 text-[11px]">
                  <div className="flex items-center justify-between gap-2 text-[9px] font-semibold uppercase tracking-wider text-faint">
                    <span>{entry.kind}</span><span>{actor.name}</span>
                  </div>
                  <pre className="mt-1 whitespace-pre-wrap break-words font-sans text-muted">{entry.content}</pre>
                </div>
              ))}
            </div>
          </details>
        )}
      </div>
    </section>
  );
}

export function CrewView({ sessionId }: CrewViewProps) {
  const { t } = useT();
  const run = useSessionStore((state) => state.sessions.find((session) => session.id === sessionId)?.crewRun ?? null);
  const promote = useSessionStore((state) => state.promoteCrewResult);
  const [actionError, setActionError] = useState<string | null>(null);

  if (!run) {
    return <div className="flex flex-1 items-center justify-center p-6 text-sm text-muted">{t("CrewView.missing")}</div>;
  }
  const active = run.status === "running";

  function retry() {
    setActionError(null);
    void retryCrewRun(sessionId).catch((error: unknown) => {
      setActionError(errorMessage(error));
    });
  }

  return (
    <main className="flex min-h-0 flex-1 flex-col bg-background" aria-busy={active}>
      <header className="shrink-0 border-b border-border bg-surface px-4 py-3">
        <div className="mx-auto flex max-w-6xl flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-accent-soft text-accent"><UsersRound size={17} aria-hidden="true" /></span>
              <div className="min-w-0">
                <h1 className="truncate text-base font-semibold text-foreground">{run.crewName}</h1>
                <p className="truncate text-xs text-muted">{run.input.prompt}</p>
              </div>
            </div>
          </div>
          <div className="flex flex-wrap items-center justify-end gap-1.5">
            <span className={`rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase ${statusTone(run.status)}`} role="status" aria-live="polite">
              {t(`CrewView.status.${run.status}`)}
            </span>
            <span className="rounded-full border border-border bg-background px-2 py-0.5 text-[10px] tabular-nums text-muted">
              {t("CrewView.budgetSummary", { calls: run.budget.modelCalls, maxCalls: run.limits.maxModelCalls, tokens: run.budget.totalTokens, maxTokens: run.limits.maxTotalTokens })}
            </span>
            {active ? (
              <Button variant="secondary" size="sm" onClick={() => cancelCrewRun(sessionId)}>
                <Square size={12} className="fill-current" aria-hidden="true" />{t("CrewView.cancelAll")}
              </Button>
            ) : (run.status === "failed" || run.status === "cancelled") ? (
              <Button variant="secondary" size="sm" onClick={retry}>
                <RotateCcw size={13} aria-hidden="true" />{t("CrewView.retry")}
              </Button>
            ) : null}
            <Button variant="primary" size="sm" disabled={run.status !== "completed"} onClick={() => promote(sessionId)}>
              <CopyPlus size={13} aria-hidden="true" />{t("CrewView.promote")}
            </Button>
          </div>
        </div>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4 [overscroll-behavior:contain]">
        <div className="mx-auto max-w-6xl space-y-4">
          {(run.error || actionError) && (
            <div className="flex items-start gap-2 rounded-lg border border-danger/40 bg-danger-soft px-3 py-2 text-sm text-danger" role="alert">
              <AlertTriangle size={15} className="mt-0.5 shrink-0" aria-hidden="true" />
              <span>{actionError ?? run.error}</span>
            </div>
          )}

          <section aria-labelledby={`crew-members-${run.id}`}>
            <div className="mb-2 flex items-center justify-between gap-2">
              <h2 id={`crew-members-${run.id}`} className="text-xs font-semibold uppercase tracking-wider text-faint">{t("CrewView.parallelDrafts")}</h2>
              <span className="text-[11px] text-muted">{t("CrewView.isolationNotice")}</span>
            </div>
            <div className="grid gap-3 lg:grid-cols-2">
              {run.members.map((member) => <ActorCard key={member.actorId} actor={member} />)}
            </div>
          </section>

          <section aria-labelledby={`crew-coordinator-${run.id}`}>
            <h2 id={`crew-coordinator-${run.id}`} className="mb-2 text-xs font-semibold uppercase tracking-wider text-faint">{t("CrewView.coordinatorSynthesis")}</h2>
            <ActorCard actor={run.coordinator} />
          </section>

          {run.finalAnswer && (
            <section className="rounded-xl border border-accent/40 bg-accent-soft/20 p-4" aria-labelledby={`crew-answer-${run.id}`}>
              <div className="mb-2 flex items-center gap-2">
                <CheckCircle2 size={16} className="text-success" aria-hidden="true" />
                <h2 id={`crew-answer-${run.id}`} className="text-sm font-semibold text-foreground">{t("CrewView.finalAnswer")}</h2>
              </div>
              <div className={`${PROSE_CLASSES} max-w-none`}><ReactMarkdown components={markdownComponents}>{run.finalAnswer}</ReactMarkdown></div>
            </section>
          )}

          {run.mutationProposals.length > 0 && (
            <section className="rounded-xl border border-warning/40 bg-warning-soft p-4" aria-labelledby={`crew-mutations-${run.id}`}>
              <div className="flex items-start gap-2">
                <AlertTriangle size={16} className="mt-0.5 shrink-0 text-warning" aria-hidden="true" />
                <div className="min-w-0 flex-1">
                  <h2 id={`crew-mutations-${run.id}`} className="text-sm font-semibold text-warning">{t("CrewView.mutationsTitle")}</h2>
                  <p className="mt-0.5 text-xs text-warning">{t("CrewView.mutationsDescription")}</p>
                  <ul className="mt-2 space-y-1.5">
                    {run.mutationProposals.map((proposal) => (
                      <li key={proposal.id} className="rounded-md border border-warning/30 bg-background/70 px-2.5 py-2 text-xs text-foreground">
                        <p className="font-medium">{proposal.summary}</p>
                        {proposal.details && <p className="mt-0.5 text-muted">{proposal.details}</p>}
                      </li>
                    ))}
                  </ul>
                </div>
              </div>
            </section>
          )}
        </div>
      </div>
    </main>
  );
}

export default CrewView;

import { useState } from "react";
import { AlertTriangle, Loader2, Scale, Square, Swords, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { DEBATE_ROLES, cancelDebate, startDebate } from "../../lib/debateRunner";
import {
  selectDebateRuns,
  useDebateStore,
  type DebatePosition,
  type DebateRoleStatus,
  type DebateStatus,
} from "../../store/debateStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface DebatePanelProps {
  onClose: () => void;
}

function runStatusTone(status: DebateStatus): PillTone {
  if (status === "completed") return "success";
  if (status === "failed") return "danger";
  if (status === "running") return "warning";
  return "neutral";
}

function roleStatusTone(status: DebateRoleStatus): PillTone {
  if (status === "completed") return "success";
  if (status === "failed") return "danger";
  if (status === "running") return "warning";
  return "neutral";
}

function formatTime(value: number): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(value);
}

function PositionCard({ position }: { position: DebatePosition }) {
  const { t } = useT();
  return (
    <div className="flex min-h-[180px] flex-col rounded-lg border border-border bg-surface p-3">
      <div className="flex items-center justify-between gap-2">
        <h3 className="text-sm font-semibold text-foreground">{position.roleLabel}</h3>
        <StatusPill tone={roleStatusTone(position.status)}>
          {t(`Debate.roleStatus.${position.status}`)}
        </StatusPill>
      </div>
      {position.status === "running" && (
        <div className="mt-3 flex items-center gap-2 text-xs text-muted">
          <Loader2 size={14} className="animate-spin" /> {t("Debate.roleStatus.running")}
        </div>
      )}
      {position.status === "failed" && (
        <p className="mt-3 text-xs text-danger">{position.error ?? t("Debate.roleGenericError")}</p>
      )}
      {position.status === "completed" && (
        <div className="mt-3 space-y-3">
          <div>
            <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("Debate.positionLabel")}
            </p>
            <p className="mt-1 whitespace-pre-wrap text-sm text-foreground">{position.position}</p>
          </div>
          <div>
            <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("Debate.objectionsLabel")}
            </p>
            {position.objections.length === 0 ? (
              <p className="mt-1 text-xs text-faint">{t("Debate.noObjections")}</p>
            ) : (
              <ul className="mt-1 list-disc space-y-1 pl-4 text-sm text-foreground">
                {position.objections.map((objection, index) => (
                  <li key={index}>{objection}</li>
                ))}
              </ul>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

export function DebatePanel({ onClose }: DebatePanelProps) {
  const { t } = useT();
  const runs = useDebateStore(selectDebateRuns);
  const activeRunId = useDebateStore((state) => state.activeRunId);
  const selectRun = useDebateStore((state) => state.selectRun);
  const activeRun = useDebateStore((state) => (state.activeRunId ? state.runs[state.activeRunId] ?? null : null));
  const [question, setQuestion] = useState("");
  const [validationError, setValidationError] = useState<string | null>(null);

  function handleRunDebate() {
    setValidationError(null);
    try {
      startDebate(question);
      setQuestion("");
    } catch (err) {
      setValidationError(err instanceof Error ? err.message : String(err));
    }
  }

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="debate-panel-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="debate-panel-title" className="flex items-center gap-2 text-base font-semibold text-foreground">
            <Swords size={16} className="text-muted" /> {t("Debate.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("Debate.subtitle")}</p>
        </div>
        <IconButton size="sm" onClick={onClose} aria-label={t("Debate.close")}>
          <X size={16} />
        </IconButton>
      </header>

      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        <nav
          className="flex max-h-56 w-full shrink-0 flex-col overflow-y-auto border-b border-border bg-surface [overscroll-behavior:contain] md:max-h-none md:w-72 md:border-b-0 md:border-r"
          aria-label={t("Debate.history")}
        >
          {runs.length === 0 ? (
            <p className="p-4 text-sm text-faint">{t("Debate.historyEmpty")}</p>
          ) : (
            runs.map((run) => (
              <button
                key={run.id}
                type="button"
                onClick={() => selectRun(run.id)}
                aria-current={run.id === activeRunId ? "true" : undefined}
                className={`w-full border-b border-border px-3 py-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent ${
                  run.id === activeRunId ? "bg-surface-2" : "hover:bg-surface-2/60"
                }`}
              >
                <div className="flex items-start justify-between gap-2">
                  <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{run.question}</span>
                  <StatusPill tone={runStatusTone(run.status)}>{t(`Debate.status.${run.status}`)}</StatusPill>
                </div>
                <div className="mt-2 flex items-center justify-between gap-2 text-xs text-faint">
                  <span className="truncate">{run.modelLabel}</span>
                  <time dateTime={new Date(run.createdAt).toISOString()}>{formatTime(run.createdAt)}</time>
                </div>
              </button>
            ))
          )}
        </nav>

        <div className="min-w-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
          <div className="mx-auto max-w-5xl space-y-5 p-5">
            <div className="rounded-lg border border-border bg-surface p-4">
              <label htmlFor="debate-question" className="text-sm font-medium text-foreground">
                {t("Debate.questionLabel")}
              </label>
              <textarea
                id="debate-question"
                value={question}
                onChange={(event) => setQuestion(event.target.value)}
                placeholder={t("Debate.questionPlaceholder")}
                rows={3}
                className="mt-2 w-full resize-none rounded-md border border-border bg-background p-2.5 text-sm text-foreground outline-none focus-visible:ring-2 focus-visible:ring-accent"
              />
              {validationError && (
                <p className="mt-2 text-xs text-danger">{validationError}</p>
              )}
              <div className="mt-3 flex items-center justify-between gap-3">
                <p className="text-xs text-faint">{t("Debate.roleListHint")}</p>
                <Button size="sm" onClick={handleRunDebate} disabled={question.trim().length === 0}>
                  <Scale size={14} /> {t("Debate.runButton")}
                </Button>
              </div>
            </div>

            {!activeRun ? (
              <div className="rounded-lg border border-dashed border-border p-8 text-center">
                <Swords size={24} className="mx-auto text-faint" />
                <p className="mt-2 text-sm font-medium text-foreground">{t("Debate.emptyStateTitle")}</p>
                <p className="mx-auto mt-1 max-w-md text-xs text-muted">{t("Debate.emptyStateBody")}</p>
              </div>
            ) : (
              <div className="space-y-5">
                <div className="flex items-start justify-between gap-4">
                  <div className="min-w-0">
                    <h2 className="text-lg font-semibold text-foreground">{activeRun.question}</h2>
                    <p className="mt-1 text-xs text-faint">{activeRun.modelLabel}</p>
                  </div>
                  <div className="flex shrink-0 flex-wrap items-center justify-end gap-2">
                    <StatusPill tone={runStatusTone(activeRun.status)}>{t(`Debate.status.${activeRun.status}`)}</StatusPill>
                    {activeRun.status === "running" && (
                      <Button variant="danger" size="sm" onClick={() => cancelDebate(activeRun.id)}>
                        <Square size={12} /> {t("Debate.cancelButton")}
                      </Button>
                    )}
                  </div>
                </div>

                {activeRun.error && (
                  <div role="alert" className="flex items-start gap-2 rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger">
                    <AlertTriangle size={14} className="mt-0.5 shrink-0" />
                    <span>{activeRun.error}</span>
                  </div>
                )}

                <div className="grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-3">
                  {DEBATE_ROLES.map((role) => {
                    const position = activeRun.positions.find((entry) => entry.roleId === role.id);
                    return position ? <PositionCard key={role.id} position={position} /> : null;
                  })}
                </div>

                {activeRun.synthesis && (
                  <div className="rounded-lg border border-accent/40 bg-surface p-4">
                    <h3 className="flex items-center gap-2 text-sm font-semibold text-foreground">
                      <Scale size={15} className="text-accent" /> {t("Debate.synthesisTitle")}
                    </h3>

                    {activeRun.synthesis.parseFailed && (
                      <p className="mt-2 rounded-md border border-warning/30 bg-warning-soft px-2.5 py-1.5 text-xs text-warning">
                        {t("Debate.rawFallbackNotice")}
                      </p>
                    )}

                    <div className="mt-3">
                      <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                        {t("Debate.recommendationLabel")}
                      </p>
                      <p className="mt-1 whitespace-pre-wrap text-sm text-foreground">
                        {activeRun.synthesis.recommendation}
                      </p>
                    </div>

                    {activeRun.synthesis.objectionHandling.length > 0 && (
                      <div className="mt-4">
                        <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                          {t("Debate.objectionsAddressedLabel")}
                        </p>
                        <ul className="mt-2 space-y-2">
                          {activeRun.synthesis.objectionHandling.map((entry, index) => (
                            <li key={index} className="rounded-md border border-border bg-background p-2.5">
                              <div className="flex items-center gap-2">
                                <StatusPill tone="neutral">{entry.roleLabel}</StatusPill>
                              </div>
                              <p className="mt-1.5 text-sm text-foreground">
                                <span className="font-medium">{t("Debate.objectionPrefix")}</span> {entry.objection}
                              </p>
                              <p className="mt-1 text-sm text-muted">
                                <span className="font-medium text-foreground">{t("Debate.resolutionLabel")}:</span>{" "}
                                {entry.resolution}
                              </p>
                            </li>
                          ))}
                        </ul>
                      </div>
                    )}

                    {activeRun.synthesis.tradeoffs && (
                      <div className="mt-4">
                        <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                          {t("Debate.tradeoffsLabel")}
                        </p>
                        <p className="mt-1 whitespace-pre-wrap text-sm text-foreground">
                          {activeRun.synthesis.tradeoffs}
                        </p>
                      </div>
                    )}

                    {activeRun.synthesis.whyThisWon && (
                      <div className="mt-4">
                        <p className="text-[11px] font-semibold uppercase tracking-wider text-faint">
                          {t("Debate.whyThisWonLabel")}
                        </p>
                        <p className="mt-1 whitespace-pre-wrap text-sm text-foreground">
                          {activeRun.synthesis.whyThisWon}
                        </p>
                      </div>
                    )}
                  </div>
                )}
              </div>
            )}
          </div>
        </div>
      </div>
    </section>
  );
}

export default DebatePanel;

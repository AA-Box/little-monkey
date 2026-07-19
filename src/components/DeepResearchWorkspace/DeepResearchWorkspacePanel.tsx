import { useMemo, useState } from "react";
import { Database, FileText, Globe, Plug, Search, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { cancelDeepResearch, startDeepResearch } from "../../lib/deepResearch";
import type { EvidenceSnippet, ResearchSourceKind, StepOutcome } from "../../lib/deepResearch";
import { selectDeepResearchRuns, useDeepResearchStore, type DeepResearchRun, type DeepResearchStatus } from "../../store/deepResearchStore";
import { Button, IconButton, StatusPill, type PillTone } from "../ui";

interface DeepResearchWorkspacePanelProps {
  onClose: () => void;
}

const KIND_ICON: Record<ResearchSourceKind, typeof Globe> = {
  web: Globe,
  file: FileText,
  knowledge: Database,
  connector: Plug,
};

const RUN_STATUS_TONE: Record<DeepResearchStatus, PillTone> = {
  planning: "neutral",
  researching: "neutral",
  synthesizing: "neutral",
  done: "success",
  error: "danger",
  cancelled: "warning",
};

type StepDisplayStatus = "queued" | "active" | StepOutcome["status"];

const STEP_STATUS_TONE: Record<StepDisplayStatus, PillTone> = {
  queued: "neutral",
  active: "neutral",
  searched: "success",
  skipped: "warning",
  error: "danger",
};

function stepDisplayStatus(stepId: string, run: DeepResearchRun): StepDisplayStatus {
  const result = run.stepResults.find((outcome) => outcome.step.id === stepId);
  if (result) return result.status;
  if (run.pendingStepId === stepId) return "active";
  return "queued";
}

/** Every evidence snippet collected so far this run, flattened and keyed by
 * citation id — what citation chips in the report resolve against. */
function evidenceById(run: DeepResearchRun): Map<string, EvidenceSnippet> {
  const map = new Map<string, EvidenceSnippet>();
  for (const outcome of run.stepResults) {
    for (const snippet of outcome.evidence) map.set(snippet.id, snippet);
  }
  return map;
}

function CitationChip({
  evidenceId,
  expanded,
  onToggle,
}: {
  evidenceId: string;
  expanded: boolean;
  onToggle: () => void;
}) {
  return (
    <button
      type="button"
      onClick={onToggle}
      aria-expanded={expanded}
      className={`rounded border px-1.5 py-0.5 text-[11px] font-medium transition-colors ${
        expanded ? "border-accent bg-accent/10 text-accent" : "border-border text-muted hover:bg-surface-2"
      }`}
    >
      [{evidenceId}]
    </button>
  );
}

function EvidenceCard({ snippet }: { snippet: EvidenceSnippet }) {
  const Icon = KIND_ICON[snippet.kind];
  const isUrl = snippet.sourceRef.startsWith("http://") || snippet.sourceRef.startsWith("https://");
  return (
    <div className="rounded-md border border-border bg-surface px-3 py-2 text-xs">
      <div className="mb-1 flex items-center gap-1.5 text-muted">
        <Icon size={12} aria-hidden="true" />
        {isUrl ? (
          <a href={snippet.sourceRef} target="_blank" rel="noreferrer" className="truncate text-accent hover:underline">
            {snippet.sourceLabel}
          </a>
        ) : (
          <span className="truncate">{snippet.sourceLabel}</span>
        )}
      </div>
      <p className="whitespace-pre-wrap text-foreground">{snippet.snippet}</p>
    </div>
  );
}

function StepRow({ stepId, run, t }: { stepId: string; run: DeepResearchRun; t: (key: string, vars?: Record<string, string | number>) => string }) {
  const step = run.plan?.steps.find((s) => s.id === stepId);
  if (!step) return null;
  const status = stepDisplayStatus(stepId, run);
  const outcome = run.stepResults.find((o) => o.step.id === stepId) ?? null;
  const Icon = KIND_ICON[step.kind];

  return (
    <div className="rounded-md border border-border bg-surface px-3 py-2">
      <div className="flex items-start justify-between gap-2">
        <div className="flex min-w-0 items-start gap-2">
          <Icon size={14} className="mt-0.5 shrink-0 text-muted" aria-hidden="true" />
          <div className="min-w-0">
            <p className="truncate text-sm font-medium text-foreground">{step.query}</p>
            <p className="truncate text-xs text-faint">{step.rationale}</p>
          </div>
        </div>
        <StatusPill tone={STEP_STATUS_TONE[status]}>{t(`DeepResearchWorkspacePanel.stepStatus.${status}`)}</StatusPill>
      </div>
      {outcome && outcome.status !== "searched" && outcome.reason && (
        <p className="mt-1.5 pl-6 text-xs text-muted">{outcome.reason}</p>
      )}
      {outcome && outcome.evidence.length > 0 && (
        <p className="mt-1.5 pl-6 text-xs text-faint">
          {t("DeepResearchWorkspacePanel.evidenceCount", { count: outcome.evidence.length })}
        </p>
      )}
    </div>
  );
}

export function DeepResearchWorkspacePanel({ onClose }: DeepResearchWorkspacePanelProps) {
  const { t } = useT();
  const runs = useDeepResearchStore(selectDeepResearchRuns);
  const selectedRunId = useDeepResearchStore((s) => s.selectedRunId);
  const selectRun = useDeepResearchStore((s) => s.selectRun);
  const selectedRun = useDeepResearchStore((s) => (s.selectedRunId ? s.runs[s.selectedRunId] : null));

  const [question, setQuestion] = useState("");
  const [expandedEvidence, setExpandedEvidence] = useState<Set<string>>(new Set());

  const isActive = selectedRun != null && ["planning", "researching", "synthesizing"].includes(selectedRun.status);

  const evidenceMap = useMemo(() => (selectedRun ? evidenceById(selectedRun) : new Map<string, EvidenceSnippet>()), [selectedRun]);

  const searchedCount = selectedRun?.stepResults.filter((o) => o.status === "searched").length ?? 0;
  const skippedCount = selectedRun?.stepResults.filter((o) => o.status !== "searched").length ?? 0;

  function toggleEvidence(id: string) {
    setExpandedEvidence((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function handleStart() {
    const trimmed = question.trim();
    if (!trimmed) return;
    startDeepResearch(trimmed);
    setQuestion("");
  }

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="deep-research-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="deep-research-title" className="text-base font-semibold text-foreground">
            {t("DeepResearchWorkspacePanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("DeepResearchWorkspacePanel.subtitle")}</p>
        </div>
        <IconButton size="sm" onClick={onClose} aria-label={t("DeepResearchWorkspacePanel.close")}>
          <X size={16} />
        </IconButton>
      </header>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        <div className="mx-auto flex max-w-3xl flex-col gap-4">
          <div className="rounded-lg border border-border bg-background p-4">
            <label htmlFor="deep-research-question" className="mb-1.5 block text-xs font-medium text-muted">
              {t("DeepResearchWorkspacePanel.questionLabel")}
            </label>
            <textarea
              id="deep-research-question"
              value={question}
              onChange={(e) => setQuestion(e.target.value)}
              placeholder={t("DeepResearchWorkspacePanel.questionPlaceholder")}
              rows={2}
              className="w-full resize-none rounded-md border border-border bg-surface px-3 py-2 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <div className="mt-2 flex justify-end">
              <Button variant="primary" size="sm" onClick={handleStart} disabled={!question.trim()}>
                <Search size={14} />
                {t("DeepResearchWorkspacePanel.start")}
              </Button>
            </div>
          </div>

          {runs.length > 0 && (
            <div className="flex flex-wrap gap-1.5">
              {runs.map((run) => (
                <button
                  key={run.id}
                  type="button"
                  onClick={() => selectRun(run.id)}
                  className={`max-w-[220px] truncate rounded-full border px-3 py-1 text-xs ${
                    run.id === selectedRunId
                      ? "border-accent bg-accent/10 text-accent"
                      : "border-border text-muted hover:bg-surface-2"
                  }`}
                  title={run.question}
                >
                  {run.question}
                </button>
              ))}
            </div>
          )}

          {selectedRun && (
            <>
              <div className="flex items-center justify-between gap-2 rounded-lg border border-border bg-background p-4">
                <div className="flex min-w-0 items-center gap-2">
                  <StatusPill tone={RUN_STATUS_TONE[selectedRun.status]}>
                    {t(`DeepResearchWorkspacePanel.runStatus.${selectedRun.status}`)}
                  </StatusPill>
                  <span className="truncate text-sm text-foreground">{selectedRun.question}</span>
                </div>
                {isActive && (
                  <Button variant="secondary" size="sm" onClick={() => cancelDeepResearch(selectedRun.id)}>
                    {t("DeepResearchWorkspacePanel.cancel")}
                  </Button>
                )}
              </div>

              {selectedRun.error && (
                <div role="alert" className="rounded-lg border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger">
                  {selectedRun.error}
                </div>
              )}

              {selectedRun.plan && (
                <section className="rounded-lg border border-border bg-background p-4" aria-label={t("DeepResearchWorkspacePanel.planTitle")}>
                  <div className="mb-2 flex items-center justify-between">
                    <h2 className="text-sm font-semibold text-foreground">{t("DeepResearchWorkspacePanel.planTitle")}</h2>
                    <span className="text-xs text-faint">
                      {t("DeepResearchWorkspacePanel.sourceMapSummary", { searched: searchedCount, skipped: skippedCount })}
                    </span>
                  </div>
                  <div className="flex flex-col gap-1.5">
                    {selectedRun.plan.steps.map((step) => (
                      <StepRow key={step.id} stepId={step.id} run={selectedRun} t={t} />
                    ))}
                  </div>
                </section>
              )}

              {selectedRun.report && (
                <section className="rounded-lg border border-border bg-background p-4" aria-label={t("DeepResearchWorkspacePanel.reportTitle")}>
                  <h2 className="mb-2 text-sm font-semibold text-foreground">{t("DeepResearchWorkspacePanel.reportTitle")}</h2>

                  {selectedRun.report.summary && <p className="mb-3 text-sm text-foreground">{selectedRun.report.summary}</p>}

                  {selectedRun.report.claims.length === 0 ? (
                    <p className="text-sm text-muted">{t("DeepResearchWorkspacePanel.noClaims")}</p>
                  ) : (
                    <ul className="flex flex-col gap-2.5">
                      {selectedRun.report.claims.map((claim) => (
                        <li key={claim.id} className="flex flex-col gap-1.5">
                          <div className="flex flex-wrap items-start gap-1.5 text-sm text-foreground">
                            <span>{claim.text}</span>
                            {claim.evidenceIds.map((evidenceId) => (
                              <CitationChip
                                key={evidenceId}
                                evidenceId={evidenceId}
                                expanded={expandedEvidence.has(evidenceId)}
                                onToggle={() => toggleEvidence(evidenceId)}
                              />
                            ))}
                          </div>
                          {claim.evidenceIds
                            .filter((id) => expandedEvidence.has(id))
                            .map((id) => {
                              const snippet = evidenceMap.get(id);
                              return snippet ? <EvidenceCard key={id} snippet={snippet} /> : null;
                            })}
                        </li>
                      ))}
                    </ul>
                  )}

                  {selectedRun.report.droppedClaimCount > 0 && (
                    <p className="mt-3 text-xs text-faint">
                      {t("DeepResearchWorkspacePanel.droppedClaims", { count: selectedRun.report.droppedClaimCount })}
                    </p>
                  )}

                  {selectedRun.report.openQuestions.length > 0 && (
                    <div className="mt-3 border-t border-border pt-3">
                      <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wider text-faint">
                        {t("DeepResearchWorkspacePanel.openQuestionsTitle")}
                      </h3>
                      <ul className="list-inside list-disc text-sm text-foreground">
                        {selectedRun.report.openQuestions.map((question_, index) => (
                          <li key={index}>{question_}</li>
                        ))}
                      </ul>
                    </div>
                  )}
                </section>
              )}
            </>
          )}

          {!selectedRun && runs.length === 0 && (
            <p className="text-sm text-muted">{t("DeepResearchWorkspacePanel.emptyState")}</p>
          )}
        </div>
      </div>
    </section>
  );
}

export default DeepResearchWorkspacePanel;

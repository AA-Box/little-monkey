import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, BrainCircuit, CopyPlus, Link2, RotateCcw, Sparkles, Square, StopCircle } from "lucide-react";
import ReactMarkdown from "react-markdown";

import {
  retryComparisonBranch,
  retryComparisonSynthesis,
  startComparisonSynthesis,
  stopComparison,
  stopComparisonBranch,
  stopComparisonSynthesis,
} from "../../lib/compareRunner";
import { useT } from "../../lib/i18n";
import type { ModelTargetSnapshot } from "../../lib/modelTargets";
import {
  useSessionStore,
  type ChatSession,
  type ComparisonBranchStatus,
  type ComparisonSynthesis,
  type ComparisonSynthesisStatus,
} from "../../store/sessionStore";
import { Button } from "../ui";
import { markdownComponents, PROSE_CLASSES } from "./MessageBubble";
import MessageList from "./MessageList";

interface CompareViewProps {
  groupId: string;
}

function formatDuration(durationMs: number | null): string | null {
  if (durationMs === null) return null;
  if (durationMs < 1000) return `${durationMs} ms`;
  return `${(durationMs / 1000).toFixed(durationMs < 10_000 ? 1 : 0)} s`;
}

function statusTone(status: ComparisonBranchStatus | ComparisonSynthesisStatus): string {
  if (status === "completed") return "border-success/40 bg-success-soft text-success";
  if (status === "failed") return "border-danger/40 bg-danger-soft text-danger";
  if (status === "cancelled" || status === "stale" || status === "queued") {
    return "border-warning/40 bg-warning-soft text-warning";
  }
  if (status === "running") return "border-accent/40 bg-accent-soft text-accent";
  return "border-border bg-surface-2 text-muted";
}

function formatBytes(value: number | null): string {
  if (value === null) return "—";
  const gib = value / 1024 ** 3;
  return `${gib.toFixed(gib < 10 ? 1 : 0)} GB`;
}

function BranchCard({ session, baseMessageCount }: { session: ChatSession; baseMessageCount: number }) {
  const { t } = useT();
  const activeSessionId = useSessionStore((state) => state.activeSessionId);
  const switchSession = useSessionStore((state) => state.switchSession);
  const promote = useSessionStore((state) => state.promoteComparisonBranch);
  const [actionError, setActionError] = useState<string | null>(null);
  const branch = session.comparisonBranch;
  if (!branch || !session.modelTarget) return null;

  const running = branch.status === "running";
  const queued = branch.status === "queued";
  const active = running || queued;
  const messages = session.messages.slice(baseMessageCount);
  const duration = formatDuration(branch.durationMs);
  const usage = branch.usage?.totalTokens ?? null;

  const retry = () => {
    setActionError(null);
    void retryComparisonBranch(session.id).catch((error: unknown) => {
      setActionError(error instanceof Error ? error.message : String(error));
    });
  };

  return (
    <section
      className={`flex min-h-0 min-w-0 flex-col overflow-hidden rounded-xl border bg-background ${
        activeSessionId === session.id ? "border-accent ring-1 ring-accent" : "border-border"
      }`}
      aria-label={`${session.modelTarget.label} ${session.modelTarget.displayName}`}
    >
      <header
        className="flex shrink-0 cursor-pointer items-center justify-between gap-2 border-b border-border bg-surface px-3 py-2 transition-colors duration-150 hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent"
        onClick={() => switchSession(session.id)}
        onKeyDown={(event) => {
          if (event.key !== "Enter" && event.key !== " ") return;
          event.preventDefault();
          switchSession(session.id);
        }}
        role="button"
        tabIndex={0}
        aria-label={t("CompareView.openBranch", { model: session.modelTarget.displayName })}
      >
        <div className="min-w-0">
          <p className="truncate text-sm font-semibold text-foreground">
            {session.modelTarget.displayName}
          </p>
          <p className="truncate text-[11px] text-faint">{session.modelTarget.label}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {duration && <span className="text-[11px] tabular-nums text-faint">{duration}</span>}
          {usage !== null && (
            <span className="text-[11px] tabular-nums text-faint">
              {t("CompareView.tokens", { count: usage })}
            </span>
          )}
          <span className={`rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase ${statusTone(branch.status)}`}>
            {t(`CompareView.status.${branch.status}`)}
          </span>
        </div>
      </header>

      <MessageList
        sessionId={session.id}
        messages={messages}
        messageIndexOffset={baseMessageCount}
        editingDisabled
      />

      {(branch.error || actionError) && (
        <p className="shrink-0 border-t border-danger/30 bg-danger-soft px-3 py-1.5 text-xs text-danger">
          {actionError ?? branch.error}
        </p>
      )}

      <footer className="flex shrink-0 items-center justify-end gap-1.5 border-t border-border bg-surface px-2 py-1.5">
        {active ? (
          <Button variant="ghost" size="sm" onClick={() => stopComparisonBranch(session.id)}>
            <Square size={12} className="fill-current" />
            {t("CompareView.stopBranch")}
          </Button>
        ) : (
          <Button variant="ghost" size="sm" onClick={retry}>
            <RotateCcw size={13} />
            {t("CompareView.retryBranch")}
          </Button>
        )}
        <Button
          variant="secondary"
          size="sm"
          onClick={() => promote(session.id)}
          disabled={branch.status !== "completed"}
        >
          <CopyPlus size={13} />
          {t("CompareView.promoteBranch")}
        </Button>
      </footer>
    </section>
  );
}

function SynthesisPanel({
  groupId,
  synthesis,
  completedTargets,
  canSynthesize,
}: {
  groupId: string;
  synthesis: ComparisonSynthesis | null;
  completedTargets: ModelTargetSnapshot[];
  canSynthesize: boolean;
}) {
  const { t } = useT();
  const switchSession = useSessionStore((state) => state.switchSession);
  const [selectedTargetKey, setSelectedTargetKey] = useState("");
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    if (completedTargets.some((target) => target.key === selectedTargetKey)) return;
    setSelectedTargetKey(completedTargets[0]?.key ?? "");
  }, [completedTargets, selectedTargetKey]);

  const selectedTarget =
    completedTargets.find((target) => target.key === selectedTargetKey) ?? completedTargets[0] ?? null;
  const running = synthesis?.status === "running";

  function synthesizeCurrent() {
    if (!selectedTarget) return;
    setActionError(null);
    try {
      const handle = startComparisonSynthesis(groupId, selectedTarget);
      void handle.done.catch((error: unknown) => {
        setActionError(error instanceof Error ? error.message : String(error));
      });
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    }
  }

  function retryFrozen() {
    setActionError(null);
    void retryComparisonSynthesis(groupId).catch((error: unknown) => {
      setActionError(error instanceof Error ? error.message : String(error));
    });
  }

  return (
    <section
      className="shrink-0 overflow-hidden rounded-xl border border-border bg-surface"
      aria-labelledby={`comparison-synthesis-${groupId}`}
      aria-busy={running}
    >
      <header className="flex flex-wrap items-center justify-between gap-2 border-b border-border px-3 py-2">
        <div className="flex min-w-0 items-center gap-2">
          <span className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-accent-soft text-accent">
            <BrainCircuit size={15} aria-hidden="true" />
          </span>
          <div className="min-w-0">
            <h2 id={`comparison-synthesis-${groupId}`} className="text-sm font-semibold text-foreground">
              {t("CompareView.synthesisTitle")}
            </h2>
            <p className="truncate text-[11px] text-faint">{t("CompareView.synthesisDescription")}</p>
          </div>
          {synthesis && (
            <span
              className={`rounded-full border px-2 py-0.5 text-[10px] font-semibold uppercase ${statusTone(synthesis.status)}`}
              role="status"
              aria-live="polite"
            >
              {t(`CompareView.synthesisStatus.${synthesis.status}`)}
            </span>
          )}
        </div>

        <div className="flex flex-wrap items-center justify-end gap-1.5">
          <label className="flex items-center gap-1.5 text-xs text-muted">
            <span>{t("CompareView.synthesisModel")}</span>
            <select
              value={selectedTarget?.key ?? ""}
              onChange={(event) => setSelectedTargetKey(event.target.value)}
              disabled={running || completedTargets.length === 0}
              className="h-8 max-w-48 cursor-pointer rounded-md border border-border bg-background px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed disabled:opacity-50"
            >
              {completedTargets.map((target) => (
                <option key={target.key} value={target.key}>
                  {target.label} · {target.displayName}
                </option>
              ))}
            </select>
          </label>
          {running ? (
            <Button variant="secondary" size="sm" onClick={() => stopComparisonSynthesis(groupId)}>
              <Square size={12} className="fill-current" aria-hidden="true" />
              {t("CompareView.stopSynthesis")}
            </Button>
          ) : (
            <>
              {synthesis && (
                <Button variant="ghost" size="sm" onClick={retryFrozen}>
                  <RotateCcw size={13} aria-hidden="true" />
                  {t("CompareView.retrySynthesis")}
                </Button>
              )}
              <Button
                variant="primary"
                size="sm"
                onClick={synthesizeCurrent}
                disabled={!canSynthesize || !selectedTarget}
                title={!canSynthesize ? t("CompareView.synthesisDisabled") : undefined}
              >
                <Sparkles size={13} aria-hidden="true" />
                {synthesis ? t("CompareView.synthesizeAgain") : t("CompareView.synthesize")}
              </Button>
            </>
          )}
        </div>
      </header>

      {synthesis?.status === "stale" && (
        <div className="flex items-start gap-2 border-b border-warning/30 bg-warning-soft px-3 py-2 text-xs text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
          <p>{t("CompareView.synthesisStale")}</p>
        </div>
      )}

      {(synthesis || actionError) && (
        <div className="max-h-[32vh] overflow-y-auto px-3 py-2.5 [overscroll-behavior:contain]">
          {synthesis && (
            <div className="mb-2 flex flex-wrap items-center gap-1.5" aria-label={t("CompareView.synthesisSources")}>
              {synthesis.sourceBranches.map((source) => (
                <button
                  key={source.sessionId}
                  type="button"
                  onClick={() => switchSession(source.sessionId)}
                  className="inline-flex cursor-pointer items-center gap-1 rounded-full border border-border bg-background px-2 py-1 text-[11px] font-medium text-muted transition-colors duration-150 hover:border-border-strong hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
                >
                  <Link2 size={11} aria-hidden="true" />
                  {source.label}
                </button>
              ))}
              {synthesis.durationMs !== null && (
                <span className="ml-auto text-[11px] tabular-nums text-faint">
                  {formatDuration(synthesis.durationMs)}
                  {synthesis.usage ? ` · ${t("CompareView.tokens", { count: synthesis.usage.totalTokens })}` : ""}
                </span>
              )}
            </div>
          )}
          {synthesis?.content ? (
            <div className={PROSE_CLASSES}>
              <ReactMarkdown components={markdownComponents}>{synthesis.content}</ReactMarkdown>
            </div>
          ) : synthesis?.status === "running" ? (
            <p className="text-sm text-muted">{t("CompareView.synthesisRunning")}</p>
          ) : !actionError ? (
            <p className="text-sm text-muted">{t("CompareView.synthesisEmpty")}</p>
          ) : null}
          {(synthesis?.error || actionError) && (
            <p className="mt-2 rounded-lg border border-danger/30 bg-danger-soft px-2.5 py-2 text-xs text-danger" role="alert">
              {actionError ?? synthesis?.error}
            </p>
          )}
        </div>
      )}
    </section>
  );
}

export default function CompareView({ groupId }: CompareViewProps) {
  const { t } = useT();
  const group = useSessionStore((state) => state.groups.find((candidate) => candidate.id === groupId) ?? null);
  const allSessions = useSessionStore((state) => state.sessions);
  const sessions = useMemo(
    () => allSessions
      .filter((session) => session.comparisonBranch?.comparisonId === groupId)
      .sort((a, b) => (a.comparisonBranch?.index ?? 0) - (b.comparisonBranch?.index ?? 0)),
    [allSessions, groupId],
  );
  const activeCount = sessions.filter(
    (session) => session.comparisonBranch?.status === "running" || session.comparisonBranch?.status === "queued",
  ).length;
  const baseMessageCount = group?.comparison?.baseMessageCount ?? 0;
  const terminal = sessions.every(
    (session) => session.comparisonBranch?.status !== "running" && session.comparisonBranch?.status !== "queued",
  );
  const completedTargets = useMemo(() => {
    const seen = new Set<string>();
    return sessions.flatMap((session): ModelTargetSnapshot[] => {
      if (session.comparisonBranch?.status !== "completed" || !session.modelTarget || seen.has(session.modelTarget.key)) {
        return [];
      }
      seen.add(session.modelTarget.key);
      return [session.modelTarget];
    });
  }, [sessions]);
  const targetSummary = useMemo(
    () => sessions.map((session) => session.modelTarget?.displayName).filter(Boolean).join(" · "),
    [sessions]
  );

  if (!group || group.kind !== "comparison" || !group.comparison) {
    return <div className="flex flex-1 items-center justify-center text-sm text-faint">{t("CompareView.missing")}</div>;
  }

  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col bg-background">
      <header className="shrink-0 border-b border-border bg-surface px-4 py-2.5">
        <div className="flex items-start justify-between gap-3">
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h1 className="text-sm font-semibold text-foreground">{t("CompareView.title")}</h1>
              <span className="rounded-full border border-border bg-background px-2 py-0.5 text-[10px] font-semibold uppercase text-muted">
                {t("CompareView.readOnlyBadge")}
              </span>
            </div>
            <p className="mt-1 line-clamp-2 text-sm text-muted">{group.comparison.prompt}</p>
            <p className="mt-0.5 truncate text-[11px] text-faint">{targetSummary}</p>
          </div>
          {activeCount > 0 && (
            <Button variant="secondary" size="sm" onClick={() => stopComparison(groupId)}>
              <StopCircle size={14} />
              {t("CompareView.stopAll", { count: activeCount })}
            </Button>
          )}
        </div>
      </header>

      {group.comparison.executionPlan?.mode === "local_sequential" && (
        <div className="flex shrink-0 items-start gap-2 border-b border-warning/30 bg-warning-soft px-4 py-2 text-xs text-warning">
          <AlertTriangle size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
          <div className="min-w-0">
            <p>
              {t("CompareView.memoryQueue", {
                estimate: formatBytes(group.comparison.executionPlan.estimatedLocalBytes),
                available: formatBytes(group.comparison.executionPlan.availableMemoryBytes),
              })}
            </p>
            {group.comparison.executionPlan.cleanupWarnings.map((warning) => (
              <p key={warning} className="mt-1" role="alert">{warning}</p>
            ))}
          </div>
        </div>
      )}

      <div className="flex min-h-0 flex-1 flex-col gap-2 p-2">
        <SynthesisPanel
          groupId={groupId}
          synthesis={group.comparison.synthesis}
          completedTargets={completedTargets}
          canSynthesize={terminal && completedTargets.length >= 2}
        />
        <div className="grid min-h-0 flex-1 grid-cols-1 gap-2 overflow-y-auto lg:grid-cols-2 lg:auto-rows-fr">
          {sessions.map((session) => (
            <BranchCard key={session.id} session={session} baseMessageCount={baseMessageCount} />
          ))}
        </div>
      </div>
    </div>
  );
}

import { useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Archive,
  ArchiveRestore,
  CheckCircle2,
  Clock3,
  Pause,
  Play,
  RefreshCw,
  RotateCcw,
  ShieldCheck,
  Square,
  X,
} from "lucide-react";

import {
  decideRunPermission,
  requestRunCancellation,
  type PermissionDecision,
  type RunEventEnvelopeWire,
  type RunRecord,
  type RunStatus,
} from "../../lib/runProtocol";
import {
  daemonCancel,
  daemonPause,
  daemonResume,
  daemonRetry,
  daemonStatus,
  isDaemonManagedRun,
} from "../../lib/daemonClient";
import { useT } from "../../lib/i18n";
import { initializeRunStore, useRunStore } from "../../store/runStore";
import { Button, IconButton, StatusPill, Tabs, type PillTone } from "../ui";
import { RunCapsulePanel } from "./RunCapsulePanel";
import { startRunCapsuleReplay } from "../../lib/runCapsuleReplay";

interface RunCenterProps {
  onClose: () => void;
}

interface PendingApproval {
  requestId: string;
  toolName: string;
  detail: string;
  operationSha256: string;
  expiresAtMs: number;
  riskLevel: string | null;
}

const TERMINAL = new Set<RunStatus>(["succeeded", "failed", "cancelled", "needs_reconciliation"]);

function statusTone(status: RunStatus): PillTone {
  if (status === "succeeded") return "success";
  if (status === "failed" || status === "needs_reconciliation") return "danger";
  if (status === "waiting_for_permission" || status === "paused" || status === "cancelling") return "warning";
  return "neutral";
}

function formatTime(value: number): string {
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(value);
}

function pendingApprovals(events: RunEventEnvelopeWire[]): PendingApproval[] {
  const pending = new Map<string, PendingApproval>();
  for (const envelope of events) {
    const event = envelope.event;
    if (event.type === "permission_requested") {
      pending.set(event.payload.request_id, {
        requestId: event.payload.request_id,
        toolName: event.payload.tool_name,
        detail: event.payload.detail,
        operationSha256: event.payload.operation_sha256,
        expiresAtMs: event.payload.expires_at_ms,
        riskLevel: event.payload.risk_level,
      });
    } else if (event.type === "permission_decided") {
      pending.delete(event.payload.request_id);
    }
  }
  return [...pending.values()];
}

function eventTitle(event: RunEventEnvelopeWire, t: (key: string, vars?: Record<string, string | number>) => string): string {
  return t(`RunCenter.event.${event.event.type}`);
}

function RunListItem({ run, selected, onSelect }: { run: RunRecord; selected: boolean; onSelect: () => void }) {
  const { t } = useT();
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-current={selected ? "true" : undefined}
      className={`w-full border-b border-border px-3 py-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent ${
        selected ? "bg-surface-2" : "hover:bg-surface-2/60"
      } ${run.archivedAtMs ? "opacity-60" : ""}`}
    >
      <div className="flex items-start justify-between gap-2">
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{run.spec.task}</span>
        <div className="flex shrink-0 items-center gap-1">
          {run.archivedAtMs && <StatusPill tone="neutral">{t("RunCenter.archived")}</StatusPill>}
          <StatusPill tone={statusTone(run.status)}>{t(`RunCenter.status.${run.status}`)}</StatusPill>
        </div>
      </div>
      <div className="mt-2 flex items-center justify-between gap-2 text-xs text-faint">
        <span className="truncate">{run.spec.target.label}</span>
        <time dateTime={new Date(run.spec.created_at_ms).toISOString()}>{formatTime(run.spec.created_at_ms)}</time>
      </div>
    </button>
  );
}

export function RunCenter({ onClose }: RunCenterProps) {
  const { t } = useT();
  const runs = useRunStore((state) => state.runs);
  const selectedRunId = useRunStore((state) => state.selectedRunId);
  const eventsByRun = useRunStore((state) => state.eventsByRun);
  const loading = useRunStore((state) => state.loading);
  const detailLoading = useRunStore((state) => state.detailLoading);
  const error = useRunStore((state) => state.error);
  const integrity = useRunStore((state) => state.integrity);
  const refresh = useRunStore((state) => state.refresh);
  const selectRun = useRunStore((state) => state.selectRun);
  const refreshRun = useRunStore((state) => state.refreshRun);
  const checkIntegrity = useRunStore((state) => state.checkIntegrity);
  const clearError = useRunStore((state) => state.clearError);
  const showArchived = useRunStore((state) => state.showArchived);
  const setShowArchived = useRunStore((state) => state.setShowArchived);
  const archiveRunAction = useRunStore((state) => state.archiveRun);
  const unarchiveRunAction = useRunStore((state) => state.unarchiveRun);
  const [actionError, setActionError] = useState<string | null>(null);
  const [actionBusy, setActionBusy] = useState<string | null>(null);
  const [managedRunIds, setManagedRunIds] = useState<string[]>([]);
  const [detailTab, setDetailTab] = useState<"capsule" | "events">("capsule");

  useEffect(() => {
    void initializeRunStore();
  }, []);

  useEffect(() => {
    void daemonStatus()
      .then((status) => setManagedRunIds(status.managedRunIds))
      .catch(() => setManagedRunIds([]));
  }, [runs.length, selectedRunId]);

  const selectedRun = runs.find((run) => run.spec.run_id === selectedRunId) ?? null;
  const events = selectedRunId ? eventsByRun[selectedRunId] ?? [] : [];
  const approvals = useMemo(() => pendingApprovals(events), [events]);
  const daemonManaged = selectedRunId ? isDaemonManagedRun(selectedRunId, managedRunIds) : false;

  useEffect(() => {
    setDetailTab("capsule");
  }, [selectedRunId]);

  async function decide(approval: PendingApproval, decision: PermissionDecision) {
    if (!selectedRunId) return;
    setActionBusy(approval.requestId);
    setActionError(null);
    try {
      await decideRunPermission(selectedRunId, approval.requestId, approval.operationSha256, decision);
      await refreshRun(selectedRunId);
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActionBusy(null);
    }
  }

  async function cancelRun() {
    if (!selectedRunId) return;
    setActionBusy("cancel");
    setActionError(null);
    try {
      if (daemonManaged) {
        await daemonCancel(selectedRunId, t("RunCenter.cancelReason"));
      } else {
        await requestRunCancellation(selectedRunId, t("RunCenter.cancelReason"));
      }
      await refreshRun(selectedRunId);
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActionBusy(null);
    }
  }

  async function archiveSelectedRun() {
    if (!selectedRunId) return;
    setActionBusy("archive");
    setActionError(null);
    try {
      await archiveRunAction(selectedRunId);
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActionBusy(null);
    }
  }

  async function unarchiveSelectedRun() {
    if (!selectedRunId) return;
    setActionBusy("unarchive");
    setActionError(null);
    try {
      await unarchiveRunAction(selectedRunId);
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActionBusy(null);
    }
  }

  async function daemonControl(action: "pause" | "resume" | "retry") {
    if (!selectedRunId || !daemonManaged) return;
    if (action === "retry") {
      const hasMutationBoundary = selectedRun?.status === "needs_reconciliation"
        || events.some((event) => event.event.type === "external_mutation_prepared");
      if (hasMutationBoundary && !window.confirm(t("RunCenter.retryMutationWarning"))) return;
      setActionBusy("retry");
      setActionError(null);
      try {
        await daemonRetry(selectedRunId, hasMutationBoundary);
        await refresh();
      } catch (caught) {
        setActionError(caught instanceof Error ? caught.message : String(caught));
      } finally {
        setActionBusy(null);
      }
      return;
    }
    setActionBusy(action);
    setActionError(null);
    try {
      await (action === "pause" ? daemonPause(selectedRunId) : daemonResume(selectedRunId));
      await refreshRun(selectedRunId);
    } catch (caught) {
      setActionError(caught instanceof Error ? caught.message : String(caught));
    } finally {
      setActionBusy(null);
    }
  }

  async function replaySafeCapsule() {
    if (!selectedRunId || !selectedRun) {
      throw new Error(t("RunCapsule.replayEngineUnavailable"));
    }
    setActionBusy("capsule-replay");
    try {
      if (daemonManaged) {
        // Never acknowledge side effects from the capsule surface. The daemon
        // independently rechecks its durable mutation markers and refuses a
        // retry if the frontend's conservative classifier missed a boundary.
        await daemonRetry(selectedRunId, false);
        await refresh();
      } else {
        const replay = await startRunCapsuleReplay(selectedRun);
        void replay.done.finally(() => void refresh());
      }
    } finally {
      setActionBusy(null);
    }
  }

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="run-center-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="run-center-title" className="text-base font-semibold text-foreground">{t("RunCenter.title")}</h1>
          <p className="truncate text-xs text-muted">{t("RunCenter.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <Button variant="ghost" size="sm" onClick={() => void checkIntegrity()}>
            <ShieldCheck size={14} /> {t("RunCenter.checkIntegrity")}
          </Button>
          <IconButton size="sm" onClick={() => void refresh()} aria-label={t("RunCenter.refresh")}>
            <RefreshCw size={15} className={loading ? "animate-spin" : ""} />
          </IconButton>
          <IconButton size="sm" onClick={onClose} aria-label={t("RunCenter.close")}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

      {(error || actionError) && (
        <div role="alert" className="flex items-start justify-between gap-3 border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          <span>{actionError ?? error}</span>
          <button type="button" className="underline" onClick={() => { setActionError(null); clearError(); }}>
            {t("RunCenter.dismiss")}
          </button>
        </div>
      )}

      {integrity && (
        <div className={`flex items-start gap-2 border-b px-4 py-2 text-xs ${integrity.ok ? "border-success/30 bg-success-soft text-success" : "border-danger/30 bg-danger-soft text-danger"}`}>
          {integrity.ok ? <CheckCircle2 size={15} /> : <AlertTriangle size={15} />}
          <div>
            <p className="font-medium">{integrity.ok ? t("RunCenter.integrityClean") : t("RunCenter.integrityFailed")}</p>
            {!integrity.ok && <ul className="mt-1 list-disc pl-4">{integrity.violations.map((item) => <li key={item}>{item}</li>)}</ul>}
          </div>
        </div>
      )}

      <div className="flex min-h-0 flex-1 flex-col md:flex-row">
        <nav className="flex max-h-56 w-full shrink-0 flex-col overflow-y-auto border-b border-border bg-surface [overscroll-behavior:contain] md:max-h-none md:w-72 md:border-b-0 md:border-r" aria-label={t("RunCenter.runHistory")}>
          <label className="flex shrink-0 items-center gap-2 border-b border-border px-3 py-2 text-xs text-muted">
            <input
              type="checkbox"
              checked={showArchived}
              onChange={(event) => void setShowArchived(event.target.checked)}
            />
            {t("RunCenter.showArchived")}
          </label>
          {loading && runs.length === 0 ? (
            <p className="p-4 text-sm text-faint">{t("RunCenter.loading")}</p>
          ) : runs.length === 0 ? (
            <div className="p-5 text-center">
              <Clock3 size={24} className="mx-auto text-faint" />
              <p className="mt-2 text-sm font-medium">{t("RunCenter.emptyTitle")}</p>
              <p className="mt-1 text-xs text-muted">{t("RunCenter.emptyDescription")}</p>
            </div>
          ) : runs.map((run) => (
            <RunListItem
              key={run.spec.run_id}
              run={run}
              selected={run.spec.run_id === selectedRunId}
              onSelect={() => void selectRun(run.spec.run_id)}
            />
          ))}
        </nav>

        <div className="min-w-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
          {!selectedRun ? (
            <p className="p-6 text-sm text-faint">{t("RunCenter.selectHint")}</p>
          ) : (
            <div className="mx-auto max-w-4xl space-y-5 p-5">
              <div className="flex items-start justify-between gap-4">
                <div className="min-w-0">
                  <h2 className="text-lg font-semibold text-foreground">{selectedRun.spec.task}</h2>
                  <p className="mt-1 break-all font-mono text-[11px] text-faint">{selectedRun.spec.run_id}</p>
                </div>
                <div className="flex shrink-0 flex-wrap items-center justify-end gap-2">
                  <StatusPill tone={statusTone(selectedRun.status)}>{t(`RunCenter.status.${selectedRun.status}`)}</StatusPill>
                  {daemonManaged && selectedRun.status === "paused" && (
                    <Button size="sm" disabled={actionBusy !== null} onClick={() => void daemonControl("resume")}>
                      <Play size={12} /> {t("RunCenter.resume")}
                    </Button>
                  )}
                  {daemonManaged && ["queued", "running", "waiting_for_permission"].includes(selectedRun.status) && (
                    <Button size="sm" disabled={actionBusy !== null} onClick={() => void daemonControl("pause")}>
                      <Pause size={12} /> {t("RunCenter.pause")}
                    </Button>
                  )}
                  {daemonManaged && TERMINAL.has(selectedRun.status) && (
                    <Button size="sm" disabled={actionBusy !== null} onClick={() => void daemonControl("retry")}>
                      <RotateCcw size={12} /> {t("RunCenter.retry")}
                    </Button>
                  )}
                  {!TERMINAL.has(selectedRun.status) && selectedRun.status !== "cancelling" && (
                    <Button variant="danger" size="sm" disabled={actionBusy !== null} onClick={() => void cancelRun()}>
                      <Square size={12} /> {t("RunCenter.requestCancellation")}
                    </Button>
                  )}
                  {TERMINAL.has(selectedRun.status) && !selectedRun.archivedAtMs && (
                    <Button size="sm" disabled={actionBusy !== null} onClick={() => void archiveSelectedRun()}>
                      <Archive size={12} /> {t("RunCenter.archive")}
                    </Button>
                  )}
                  {selectedRun.archivedAtMs && (
                    <Button size="sm" disabled={actionBusy !== null} onClick={() => void unarchiveSelectedRun()}>
                      <ArchiveRestore size={12} /> {t("RunCenter.unarchive")}
                    </Button>
                  )}
                </div>
              </div>

              <dl className="grid grid-cols-2 gap-3 rounded-lg border border-border bg-surface p-3 text-xs sm:grid-cols-4">
                <div><dt className="text-faint">{t("RunCenter.kind")}</dt><dd className="mt-1 font-medium">{selectedRun.spec.kind}</dd></div>
                <div><dt className="text-faint">{t("RunCenter.target")}</dt><dd className="mt-1 truncate font-medium">{selectedRun.spec.target.label}</dd></div>
                <div><dt className="text-faint">{t("RunCenter.created")}</dt><dd className="mt-1 font-medium">{formatTime(selectedRun.spec.created_at_ms)}</dd></div>
                <div><dt className="text-faint">{t("RunCenter.events")}</dt><dd className="mt-1 font-medium">{selectedRun.lastSequence}</dd></div>
              </dl>

              {approvals.length > 0 && (
                <section aria-labelledby="run-approvals-title">
                  <h3 id="run-approvals-title" className="text-sm font-semibold">{t("RunCenter.pendingApprovals")}</h3>
                  <div className="mt-2 space-y-2">
                    {approvals.map((approval) => {
                      const expired = Date.now() >= approval.expiresAtMs;
                      return (
                        <article key={approval.requestId} className="rounded-lg border border-warning/40 bg-warning-soft p-3 text-sm">
                          <div className="flex items-start justify-between gap-3">
                            <div><p className="font-medium">{approval.toolName}</p><p className="mt-1 text-xs text-muted">{approval.detail}</p></div>
                            {approval.riskLevel && <StatusPill tone="warning">{approval.riskLevel}</StatusPill>}
                          </div>
                          <p className="mt-2 font-mono text-[10px] text-faint" title={approval.operationSha256}>{t("RunCenter.digest")}: {approval.operationSha256.slice(0, 16)}…</p>
                          <p className="mt-1 text-[11px] text-faint">{t("RunCenter.expires")}: {formatTime(approval.expiresAtMs)}</p>
                          <div className="mt-3 flex flex-wrap gap-2">
                            {expired ? (
                              <Button size="sm" disabled={actionBusy !== null} onClick={() => void decide(approval, "expired")}>{t("RunCenter.markExpired")}</Button>
                            ) : (
                              <>
                                <Button size="sm" variant="primary" disabled={actionBusy !== null} onClick={() => void decide(approval, "allow_once")}>{t("RunCenter.allowOnce")}</Button>
                                <Button size="sm" disabled={actionBusy !== null} onClick={() => void decide(approval, "allow_for_run")}>{t("RunCenter.allowRun")}</Button>
                                <Button size="sm" variant="danger" disabled={actionBusy !== null} onClick={() => void decide(approval, "deny")}>{t("RunCenter.deny")}</Button>
                              </>
                            )}
                          </div>
                        </article>
                      );
                    })}
                  </div>
                </section>
              )}

              <Tabs
                tabs={[
                  { id: "capsule", label: t("RunCapsule.tab") },
                  { id: "events", label: t("RunCenter.eventHistory") },
                ]}
                active={detailTab}
                onChange={(id) => setDetailTab(id === "events" ? "events" : "capsule")}
              />

              {detailTab === "capsule" ? (
                <RunCapsulePanel
                  run={selectedRun}
                  events={events}
                  runs={runs}
                  replayEngineAvailable
                  actionBusy={actionBusy !== null}
                  onReplay={replaySafeCapsule}
                />
              ) : (
                <section aria-labelledby="run-events-title">
                  <h3 id="run-events-title" className="text-sm font-semibold">{t("RunCenter.eventHistory")}</h3>
                  {detailLoading && events.length === 0 ? (
                    <p className="mt-2 text-sm text-faint">{t("RunCenter.loading")}</p>
                  ) : events.length === 0 ? (
                    <p className="mt-2 rounded-lg border border-dashed border-border p-4 text-sm text-faint">{t("RunCenter.noEvents")}</p>
                  ) : (
                    <ol className="mt-2 space-y-2">
                      {events.map((event) => (
                        <li key={event.event_id} className="rounded-lg border border-border bg-surface p-3">
                          <div className="flex items-center justify-between gap-3 text-xs">
                            <span className="font-medium">#{event.sequence} · {eventTitle(event, t)}</span>
                            <time className="text-faint" dateTime={new Date(event.occurred_at_ms).toISOString()}>{formatTime(event.occurred_at_ms)}</time>
                          </div>
                          {(event.actor_id || event.emitter.kind) && <p className="mt-1 text-[11px] text-faint">{event.actor_id ? `${t("RunCenter.actor")}: ${event.actor_id} · ` : ""}{event.emitter.kind}</p>}
                          <details className="mt-2 text-xs">
                            <summary className="cursor-pointer text-muted hover:text-foreground">{t("RunCenter.details")}</summary>
                            <pre className="mt-2 max-h-56 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background p-2 text-[11px]">{JSON.stringify(event.event.payload, null, 2)}</pre>
                          </details>
                        </li>
                      ))}
                    </ol>
                  )}
                </section>
              )}
            </div>
          )}
        </div>
      </div>
    </section>
  );
}

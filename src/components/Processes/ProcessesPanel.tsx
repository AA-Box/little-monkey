import { useEffect, useMemo, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import { Loader2, Pause, Play, RefreshCw, Square, X } from "lucide-react";

import { IconButton, StatusPill } from "../ui";
import { statusTone } from "../../lib/statusTone";
import type { ProcessKind, ProcessRecord, ProcessResourceReport } from "../../lib/processTable";
import { fetchProcessResourceReport } from "../../lib/processTable";
import { ProcessResources } from "./ProcessResources";
import { canResume, canSuspend, processDisplayState } from "../../lib/processSignals";
import {
  selectStateCounts,
  startProcessCatchUp,
  subscribeToProcessChanges,
  useProcessStore,
} from "../../store/processStore";
import { useT } from "../../lib/i18n";
import { formatElapsed } from "../../lib/taskFormat";

/**
 * Every live agent process, across every kind, with its signals.
 *
 * This is the surface the process table was built for and did not have: until
 * now nothing in the frontend read `process_list` at all, so a chat turn, a
 * daemon job, a workflow run and a background shell were each only visible
 * inside whatever panel happened to own them — and `monkey processes` was the
 * only place the unified view existed.
 *
 * The state a row shows is DERIVED, not stored (`processDisplayState`). The
 * distinction that matters: `pause_pending` means a suspend is latched but the
 * loop has not reached its safe point yet, which for a long `run_shell` can be
 * minutes. Rendering that as "paused" would claim a park that has not
 * happened, and rendering it as "running" would hide that the user already
 * asked. The panel says the true third thing.
 *
 * Refusals are shown, not swallowed. A kind that does not honour a signal
 * returns the reason (`ProcessKind::signal_support`), and that reason is
 * exactly what the user needs to see instead of a button that appears to work.
 *
 * While it is open the panel polls, in addition to following
 * `processes://changed`. The event only carries writes made in *this* OS
 * process; `monkey processes signal` writes the same SQLite ledger from a
 * terminal and has no way to emit into this one, so without the poll a row
 * paused from the CLI keeps rendering "running" until a remount. The poll is
 * also what keeps each row's age moving, since that is computed from
 * `Date.now()` at render.
 */

type Translate = ReturnType<typeof useT>["t"];

/**
 * Written to the record's `signal_reason` and read back by `monkey processes`.
 * Deliberately NOT translated: an audit trail that changes language with the
 * reader's UI locale is worse than one that is always English.
 */
const SIGNAL_REASON = {
  suspend: "Paused from the Processes panel",
  resume: "Resumed from the Processes panel",
  stop: "Stopped from the Processes panel",
} as const;

export interface ProcessesPanelProps {
  onClose?: () => void;
}

function kindLabel(t: Translate, kind: ProcessKind): string {
  switch (kind) {
    case "chat_turn":
      return t("ProcessesPanel.kindChatTurn");
    case "daemon_job":
      return t("ProcessesPanel.kindDaemonJob");
    case "subagent":
      return t("ProcessesPanel.kindSubagent");
    case "crew_member":
      return t("ProcessesPanel.kindCrewMember");
    case "workflow_run":
      return t("ProcessesPanel.kindWorkflowRun");
    case "workflow_node":
      return t("ProcessesPanel.kindWorkflowNode");
    case "remote_run":
      return t("ProcessesPanel.kindRemoteRun");
    case "background_shell":
      return t("ProcessesPanel.kindBackgroundShell");
    case "foreground_shell":
      return t("ProcessesPanel.kindForegroundShell");
    case "browser_session":
      return t("ProcessesPanel.kindBrowserSession");
    case "side_task":
      return t("ProcessesPanel.kindSideTask");
    default:
      return kind;
  }
}

function stateLabel(t: Translate, record: ProcessRecord): string {
  switch (processDisplayState(record)) {
    case "admitted":
      return t("ProcessesPanel.stateAdmitted");
    case "running":
      return t("ProcessesPanel.stateRunning");
    case "suspended":
      return t("ProcessesPanel.stateSuspended");
    case "pause_pending":
      return t("ProcessesPanel.statePausePending");
    case "stopping":
      return t("ProcessesPanel.stateStopping");
    default:
      return t("ProcessesPanel.stateExited");
  }
}

/** One live process. The external id is shown verbatim rather than
 * prettified — it is what the user types into `monkey processes signal`, so a
 * paraphrase would make the panel and the CLI disagree. */
function ProcessRow({ record, now }: { record: ProcessRecord; now: number }) {
  const { t } = useT();
  const busy = useProcessStore((state) => state.pending[record.processId] === true);
  const signal = useProcessStore((state) => state.signal);
  const display = processDisplayState(record);
  // Fetched on demand rather than with the listing. The report builds a real
  // controller to ask this host what it would hold a tree with — which on Linux
  // creates a cgroup scope — and doing that for every row on every poll would be
  // a containment primitive per process per second, to answer a question nobody
  // asked yet.
  const [resources, setResources] = useState<ProcessResourceReport | null>(null);
  const [showResources, setShowResources] = useState(false);
  useEffect(() => {
    if (!showResources) return;
    let cancelled = false;
    void fetchProcessResourceReport(record.processId).then((report) => {
      if (!cancelled) setResources(report);
    });
    return () => {
      cancelled = true;
    };
    // `updatedAtMs` rather than the record: a row that has not changed needs no
    // second read, and a row that just exited needs its final numbers.
  }, [showResources, record.processId, record.updatedAtMs]);
  // `now` is passed in rather than read here so every row ages off the same
  // instant, and so the age advances on the panel's tick instead of freezing
  // until some unrelated store write happens to re-render this row.
  const elapsed = formatElapsed(now - (record.startedAtMs ?? record.createdAtMs));

  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 break-all font-mono text-xs leading-snug text-foreground">
          {record.externalId}
        </span>
        <StatusPill tone={statusTone(display)}>{stateLabel(t, record)}</StatusPill>
      </div>

      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs">
        <span className="text-muted">{kindLabel(t, record.kind)}</span>
        <span className="shrink-0 text-faint">{elapsed}</span>
        {record.nativePid !== null && (
          <span className="shrink-0 text-faint">{t("ProcessesPanel.pid", { pid: record.nativePid })}</span>
        )}
        {display === "running" && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>

      {/* The honest part: a latched suspend that has not landed says so, and
          says why it might take a while, rather than looking stuck. */}
      {display === "pause_pending" && (
        <p className="mt-1 text-[11px] text-faint">{t("ProcessesPanel.pausePendingHint")}</p>
      )}
      {record.signalReason && display !== "running" && (
        <p className="mt-1 truncate text-[11px] text-faint">{record.signalReason}</p>
      )}

      <div className="mt-2 flex items-center gap-1">
        {canSuspend(record) && (
          <IconButton
            size="sm"
            disabled={busy}
            aria-label={t("ProcessesPanel.suspendAriaLabel", { name: record.externalId })}
            onClick={() => void signal(record.processId, "suspend", SIGNAL_REASON.suspend)}
          >
            <Pause size={12} />
          </IconButton>
        )}
        {canResume(record) && (
          <IconButton
            size="sm"
            disabled={busy}
            aria-label={t("ProcessesPanel.resumeAriaLabel", { name: record.externalId })}
            onClick={() => void signal(record.processId, "resume", SIGNAL_REASON.resume)}
          >
            <Play size={12} />
          </IconButton>
        )}
        <IconButton
          size="sm"
          disabled={busy || record.signalIntent.stopRequested}
          aria-label={t("ProcessesPanel.stopAriaLabel", { name: record.externalId })}
          onClick={() => void signal(record.processId, "stop", SIGNAL_REASON.stop)}
        >
          <Square size={12} />
        </IconButton>
        <button
          type="button"
          className="cursor-pointer text-[11px] text-faint underline"
          aria-expanded={showResources}
          onClick={() => setShowResources((open) => !open)}
        >
          {t("ProcessesPanel.resourcesToggle")}
        </button>
      </div>

      {showResources && resources && <ProcessResources report={resources} />}
    </div>
  );
}

export function ProcessesPanel({ onClose }: ProcessesPanelProps) {
  const { t } = useT();
  const records = useProcessStore(useShallow((state) => state.records));
  // Derived here rather than subscribed as a selector: `selectStateCounts`
  // builds a fresh array of fresh objects per call, and `useShallow` compares
  // an array's elements by identity — so subscribing to it re-renders on every
  // store read and trips React's "Maximum update depth exceeded". `records` is
  // already shallow-compared above, which is the stable input this needs.
  const counts = useMemo(() => selectStateCounts(records), [records]);
  const loading = useProcessStore((state) => state.loading);
  const error = useProcessStore((state) => state.error);
  const [now, setNow] = useState(() => Date.now());

  // Rust owns the records, so read the current truth on mount rather than
  // assuming this window started everything it can see — then follow the change
  // event for this process's own writes, and poll for everyone else's.
  useEffect(() => {
    void useProcessStore.getState().refresh();
    let disposed = false;
    let unlisten: (() => void) | null = null;
    void subscribeToProcessChanges().then((cleanup) => {
      if (disposed) cleanup();
      else unlisten = cleanup;
    });
    const stopCatchUp = startProcessCatchUp(() => setNow(Date.now()));
    return () => {
      disposed = true;
      stopCatchUp();
      unlisten?.();
    };
  }, []);

  return (
    <aside className="flex h-full min-h-0 w-full flex-col bg-surface">
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
        <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
          {t("ProcessesPanel.title")}
          {records.length > 0 && <StatusPill tone="neutral">{records.length}</StatusPill>}
        </span>
        <div className="flex items-center gap-1">
          <IconButton
            size="sm"
            disabled={loading}
            onClick={() => void useProcessStore.getState().refresh()}
            aria-label={t("ProcessesPanel.refreshAriaLabel")}
          >
            <RefreshCw size={14} className={loading ? "animate-spin" : undefined} />
          </IconButton>
          {onClose && (
            <IconButton size="sm" onClick={onClose} aria-label={t("ProcessesPanel.closeAriaLabel")}>
              <X size={16} />
            </IconButton>
          )}
        </div>
      </div>

      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
        {error && (
          <div className="flex items-start gap-2 rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">
            <span className="min-w-0 flex-1">{error}</span>
            <button
              type="button"
              className="cursor-pointer underline"
              onClick={() => useProcessStore.getState().clearError()}
            >
              {t("ProcessesPanel.dismissError")}
            </button>
          </div>
        )}

        {counts.length > 0 && (
          <div className="flex flex-wrap items-center gap-1.5">
            {counts.map((entry) => (
              <StatusPill key={entry.state} tone={statusTone(entry.state)}>
                {`${stateCountLabel(t, entry.state)} ${entry.count}`}
              </StatusPill>
            ))}
          </div>
        )}

        {records.length === 0 && !loading && (
          <div className="flex flex-1 flex-col items-center justify-center py-12 text-center">
            <p className="text-xs text-faint">{t("ProcessesPanel.emptyState")}</p>
          </div>
        )}

        {records.map((record) => (
          <ProcessRow key={record.processId} record={record} now={now} />
        ))}
      </div>
    </aside>
  );
}

function stateCountLabel(t: Translate, state: string): string {
  switch (state) {
    case "admitted":
      return t("ProcessesPanel.stateAdmitted");
    case "running":
      return t("ProcessesPanel.stateRunning");
    case "suspended":
      return t("ProcessesPanel.stateSuspended");
    case "pause_pending":
      return t("ProcessesPanel.statePausePending");
    case "stopping":
      return t("ProcessesPanel.stateStopping");
    default:
      return t("ProcessesPanel.stateExited");
  }
}

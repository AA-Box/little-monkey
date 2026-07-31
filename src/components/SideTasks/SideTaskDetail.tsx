import {
  Archive,
  ArchiveRestore,
  CheckCircle2,
  CircleAlert,
  CircleDot,
  ExternalLink,
  ListTree,
  Loader2,
  Pause,
  Play,
  RotateCcw,
  Square,
  Upload,
  XCircle,
} from "lucide-react";

import { Button, StatusPill, type PillTone } from "../ui";
import {
  useSideTaskStore,
  type SideTaskRecord,
  type SideTaskStatus,
  type SideTaskToolOutcome,
} from "../../store/sideTaskStore";
import { usePermissionStore } from "../../store/permissionStore";
import {
  cancelSideTask,
  openSideTaskAsFullChat,
  pauseSideTask,
  promoteSideTask,
  resumeSideTask,
  retrySideTask,
} from "../../lib/sideTaskRunner";
import { statusTone as sharedStatusTone } from "../../lib/statusTone";

/** Shared side-task lifecycle/evidence view. The side-task pane's Details
 * disclosure and the Agent Inbox both render this exact component, so
 * pause/resume/cancel/retry/archive/promote behavior cannot drift between the
 * two task surfaces. */

export function statusTone(status: SideTaskStatus): PillTone {
  // A queued side task is work the user is actively waiting on.
  return sharedStatusTone(status, { queued: "warning" });
}

export function statusLabel(status: SideTaskStatus): string {
  switch (status) {
    case "queued":
      return "Queued";
    case "running":
      return "Running";
    case "paused":
      return "Paused";
    case "completed":
      return "Completed";
    case "error":
      return "Failed";
    case "cancelled":
      return "Cancelled";
    default:
      return status;
  }
}

export function sourceKindLabel(kind: SideTaskRecord["source"]["kind"]): string {
  switch (kind) {
    case "chat_message":
      return "Chat message";
    case "selected_files":
      return "Selected files";
    case "terminal_output":
      return "Terminal output";
    case "browser_evidence":
      return "Browser evidence";
    case "mcp_result":
      return "MCP result";
    default:
      return "Manual";
  }
}

function toolOutcomeIcon(outcome: SideTaskToolOutcome) {
  switch (outcome) {
    case "pending":
      return <Loader2 size={12} className="shrink-0 animate-spin text-faint" />;
    case "succeeded":
      return <CheckCircle2 size={12} className="shrink-0 text-success" />;
    case "failed":
      return <XCircle size={12} className="shrink-0 text-danger" />;
    case "denied":
      return <CircleAlert size={12} className="shrink-0 text-warning" />;
    case "cancelled":
      return <Square size={12} className="shrink-0 text-faint" />;
    default:
      return <CircleDot size={12} className="shrink-0 text-faint" />;
  }
}

export function SideTaskDetail({ task }: { task: SideTaskRecord }) {
  const approvalsWaiting = usePermissionStore(
    (state) => state.queue.filter((request) => request.agent_label === `Side task "${task.title}"`).length,
  );

  const running = task.status === "running";
  const paused = task.status === "paused";
  const terminal = task.status === "completed" || task.status === "error" || task.status === "cancelled";
  const canPause = running;
  const canResume = paused;
  const canCancel = running || paused || task.status === "queued";
  const canRetry = terminal;
  const canPromote = task.status === "completed" && task.finalReport !== null;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
      <div>
        <div className="flex items-center gap-2">
          <h3 className="min-w-0 flex-1 truncate text-sm font-semibold text-foreground">{task.title}</h3>
          <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
        </div>
        <p className="mt-1 text-xs text-faint">
          {sourceKindLabel(task.source.kind)} · {task.profile === "code" ? "Can edit files" : "Read-only"} · {task.modelLabel}
        </p>
        {task.retryOf && <p className="mt-0.5 text-[11px] text-faint">Retry of a previous attempt</p>}
      </div>

      <div className="flex flex-wrap gap-1.5">
        <Button variant="secondary" size="sm" onClick={() => canPause && pauseSideTask(task.id)} disabled={!canPause}>
          <Pause size={13} /> Pause
        </Button>
        <Button variant="secondary" size="sm" onClick={() => canResume && resumeSideTask(task.id)} disabled={!canResume}>
          <Play size={13} /> Resume
        </Button>
        <Button variant="danger" size="sm" onClick={() => canCancel && cancelSideTask(task.id)} disabled={!canCancel}>
          <Square size={13} /> Cancel
        </Button>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => {
            if (!canRetry) return;
            const retryId = retrySideTask(task.id);
            if (retryId) useSideTaskStore.getState().openTab(retryId);
          }}
          disabled={!canRetry}
        >
          <RotateCcw size={13} /> Retry
        </Button>
        {task.archivedAt ? (
          <Button variant="secondary" size="sm" onClick={() => useSideTaskStore.getState().unarchive(task.id)}>
            <ArchiveRestore size={13} /> Unarchive
          </Button>
        ) : (
          <Button variant="secondary" size="sm" onClick={() => useSideTaskStore.getState().archive(task.id)} disabled={!terminal}>
            <Archive size={13} /> Archive
          </Button>
        )}
        <Button variant="secondary" size="sm" onClick={() => openSideTaskAsFullChat(task.id)}>
          <ExternalLink size={13} /> Open as full task
        </Button>
        <Button variant="primary" size="sm" onClick={() => promoteSideTask(task.id)} disabled={!canPromote || task.promotedAt !== null}>
          <Upload size={13} /> {task.promotedAt ? "Promoted" : "Promote to chat"}
        </Button>
      </div>

      {approvalsWaiting > 0 && (
        <div className="flex items-center gap-1.5 rounded-md border border-warning bg-warning-soft px-2.5 py-1.5 text-xs text-warning">
          <CircleAlert size={13} className="shrink-0" />
          {approvalsWaiting === 1 ? "1 approval waiting" : `${approvalsWaiting} approvals waiting`}
        </div>
      )}

      {task.usage && (
        <p className="text-[11px] text-faint">
          {task.usage.totalTokens.toLocaleString("en-US")} tokens · {task.toolEvidence.length} tool call
          {task.toolEvidence.length === 1 ? "" : "s"}
        </p>
      )}

      <div>
        <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Prompt snapshot</div>
        <p className="whitespace-pre-wrap rounded-md border border-border bg-surface-2 p-2 text-xs text-muted">{task.prompt}</p>
      </div>

      {task.toolEvidence.length > 0 && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Tools used</div>
          <div className="flex flex-col gap-1">
            {task.toolEvidence.map((evidence) => (
              <div key={evidence.id} className="flex items-start gap-1.5 rounded-md border border-border bg-surface-2 p-2 text-[11px]">
                {toolOutcomeIcon(evidence.outcome)}
                <div className="min-w-0 flex-1">
                  <div className="truncate font-mono text-foreground">{evidence.name}</div>
                  {evidence.argsPreview && <div className="truncate text-faint">{evidence.argsPreview}</div>}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {task.artifacts.length > 0 && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Artifacts</div>
          <div className="flex flex-col gap-1">
            {task.artifacts.map((artifact) => (
              <div key={artifact.id} className="rounded-md border border-border bg-surface-2 p-2 text-[11px]">
                <div className="flex items-center gap-1.5 font-mono text-foreground">
                  <ListTree size={12} className="shrink-0 text-faint" />
                  <span className="truncate">{artifact.label}</span>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {task.error && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Error</div>
          <p className="whitespace-pre-wrap rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">{task.error}</p>
        </div>
      )}

      {task.finalReport && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Report</div>
          <p className="whitespace-pre-wrap rounded-md border border-border bg-surface-2 p-2 text-xs text-foreground">{task.finalReport}</p>
        </div>
      )}
    </div>
  );
}

export default SideTaskDetail;

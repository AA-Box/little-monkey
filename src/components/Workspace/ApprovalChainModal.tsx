import { useEffect, useState } from "react";
import { AlertTriangle, ListOrdered } from "lucide-react";
import { useApprovalChainStore } from "../../store/approvalChainStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";

function formatRemaining(remainingMs: number): string {
  const totalSeconds = Math.max(0, Math.ceil(remainingMs / 1000));
  const minutes = Math.floor(totalSeconds / 60);
  const seconds = totalSeconds % 60;
  return `${minutes}:${seconds.toString().padStart(2, "0")}`;
}

/**
 * Centered dialog shown for the current stage of a Human Approval Chain
 * (ROADMAP.md, Phase 3) — a multi-step counterpart to `PermissionModal`.
 * Mirrors `approvalChainStore.pending` exactly: one stage payload, or null.
 * A deny here stops the whole chain; approving advances to the next stage
 * (or finishes the chain), which arrives as a new `pending` value once the
 * backend emits it.
 */
export function ApprovalChainModal() {
  const pending = useApprovalChainStore((s) => s.pending);
  const respond = useApprovalChainStore((s) => s.respond);
  const { t } = useT();
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!pending) return;
    const interval = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(interval);
  }, [pending?.chain_id, pending?.stage_index]);

  if (!pending) return null;

  const remainingMs = pending.expires_at_ms - now;
  const stageNumber = pending.stage_index + 1;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="approval-chain-modal-title"
    >
      <div className="w-full max-w-md rounded-xl border border-border bg-background p-5 shadow-xl">
        <div className="flex items-start gap-3">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-warning-soft text-warning">
            <ListOrdered size={18} />
          </div>
          <div className="min-w-0 pt-0.5">
            <h2 id="approval-chain-modal-title" className="text-sm font-semibold text-foreground">
              {t("ApprovalChainModal.title")}
            </h2>
            <p className="mt-0.5 text-xs text-muted">
              {t("ApprovalChainModal.stageProgress", { current: stageNumber, total: pending.total_stages })}
            </p>
          </div>
        </div>

        <div className="mt-3 flex items-center gap-1.5">
          {Array.from({ length: pending.total_stages }, (_, index) => (
            <div
              key={index}
              className={`h-1.5 flex-1 rounded-full ${
                index < stageNumber ? "bg-accent" : "bg-surface-2"
              }`}
            />
          ))}
        </div>

        <p className="mt-3 text-sm font-medium text-foreground">{pending.label}</p>

        <div className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md border border-border bg-surface-2 p-2.5 font-mono text-xs text-muted">
          {pending.detail}
        </div>

        {pending.escalated && pending.escalate_message && (
          <div className="mt-3 flex items-start gap-2 rounded-md border border-warning/40 bg-warning-soft p-2.5 text-xs text-warning">
            <AlertTriangle size={14} className="mt-0.5 shrink-0" />
            <span>{pending.escalate_message}</span>
          </div>
        )}

        <p className="mt-3 text-xs text-faint">
          {t("ApprovalChainModal.timeRemaining", { time: formatRemaining(remainingMs) })}
        </p>

        <div className="mt-4 flex flex-col gap-2 sm:flex-row sm:justify-end">
          <Button type="button" variant="secondary" onClick={() => respond(false)}>
            {t("ApprovalChainModal.denyButton")}
          </Button>
          <Button type="button" variant="primary" onClick={() => respond(true)} autoFocus>
            {t("ApprovalChainModal.approveButton")}
          </Button>
        </div>
      </div>
    </div>
  );
}

export default ApprovalChainModal;

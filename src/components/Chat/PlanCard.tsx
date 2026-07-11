import { useState } from "react";
import ReactMarkdown from "react-markdown";
import { ClipboardList } from "lucide-react";

import { formatPlanNotice, runAgentTurn, type PlanNotice } from "../../lib/agentLoop";
import { selectTurnRunning, useSessionStore } from "../../store/sessionStore";
import { usePermissionStore } from "../../store/permissionStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { markdownComponents, PROSE_CLASSES } from "./MessageBubble";

/** The plain-text instruction sent as the new user turn once a plan is
 * approved — deliberately not a synthetic notice of its own (unlike the
 * `[Plan]`/`[Checkpoint]`/etc. prefixes), since this one really is meant to
 * read as something "said" to the model, exactly like any other typed
 * prompt, just triggered by a button instead of the input box. */
const PLAN_APPROVED_INSTRUCTION = "The plan is approved. Execute it now.";

export interface PlanCardProps {
  /** The session this notice belongs to — approving/dismissing rewrites the
   * notice in place (`updateMessageAt`) and, on approve, starts a new turn
   * in this same session (CheckpointRow's/MemoryRow's exact pattern). */
  sessionId: string;
  notice: PlanNotice;
  /** This notice message's index in the transcript — the target for the
   * in-place `updateMessageAt` rewrite on Approve/Keep planning. */
  messageIndex: number;
}

/**
 * Renders a `present_plan` notice: the model's proposed title, Markdown plan
 * body (same `prose` classes/`markdownComponents` as an assistant message —
 * see `MessageBubble.tsx`), and open questions (if any), with two buttons
 * while `notice.status === 'proposed'`:
 * - "Approve & start acting" rewrites the notice to `status: 'approved'`,
 *   switches the (global, single-Mutex-in-Rust — same caveat as
 *   `ModeSelector.tsx`) permission mode to `lastActMode`, and starts a new
 *   turn instructing the model to execute the approved plan.
 * - "Keep planning" just rewrites the notice to `status: 'dismissed'` — the
 *   model stays in Plan Mode and the user can keep refining via chat.
 *
 * Approve is disabled while this session's turn is running (mirrors
 * `CheckpointRow`'s "Rewind conversation" being disabled during a turn) —
 * because the permission mode is a single global value on the Rust side, a
 * turn running concurrently in the *other* split-pane session would also be
 * affected by the mode switch, same as picking a new mode in `ModeSelector`
 * today; disabling here only prevents THIS session's own in-flight turn from
 * racing its own mode switch and follow-up turn.
 */
export function PlanCard({ sessionId, notice, messageIndex }: PlanCardProps) {
  const { t } = useT();
  const turnRunning = useSessionStore(selectTurnRunning(sessionId));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const approve = async () => {
    setBusy(true);
    setError(null);
    try {
      useSessionStore.getState().updateMessageAt(sessionId, messageIndex, {
        content: formatPlanNotice({ ...notice, status: "approved" }),
      });
      const lastActMode = usePermissionStore.getState().lastActMode;
      await usePermissionStore.getState().setMode(lastActMode);
      void runAgentTurn(sessionId, PLAN_APPROVED_INSTRUCTION);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      setError(t("PlanCard.approveFailed", { error: message }));
    } finally {
      setBusy(false);
    }
  };

  const keepPlanning = () => {
    useSessionStore.getState().updateMessageAt(sessionId, messageIndex, {
      content: formatPlanNotice({ ...notice, status: "dismissed" }),
    });
  };

  return (
    <div className="flex justify-start">
      <div className="w-full max-w-[85%] min-w-0 overflow-hidden rounded-lg border border-border bg-surface-2">
        <div className="flex items-center gap-2 border-b border-border px-3 py-2">
          <ClipboardList size={14} className="shrink-0 text-faint" />
          <span className="truncate text-sm font-medium text-foreground">{notice.title}</span>
        </div>
        <div className="px-3 py-2">
          <div className={PROSE_CLASSES}>
            <ReactMarkdown components={markdownComponents}>{notice.plan}</ReactMarkdown>
          </div>
          {notice.openQuestions && notice.openQuestions.length > 0 && (
            <div className="mt-3 rounded-md border border-border bg-background px-3 py-2">
              <div className="mb-1 text-xs font-medium text-muted">{t("PlanCard.openQuestionsLabel")}</div>
              <ul className="list-inside list-disc space-y-0.5 text-sm text-foreground">
                {notice.openQuestions.map((question, index) => (
                  <li key={index}>{question}</li>
                ))}
              </ul>
            </div>
          )}
        </div>
        <div className="flex items-center gap-2 border-t border-border px-3 py-2">
          {notice.status === "proposed" ? (
            <>
              <Button
                type="button"
                variant="primary"
                size="sm"
                onClick={() => void approve()}
                disabled={busy || turnRunning}
                title={turnRunning ? t("PlanCard.approveDisabledTurnRunning") : undefined}
              >
                {busy ? t("PlanCard.approving") : t("PlanCard.approveButton")}
              </Button>
              <Button type="button" variant="secondary" size="sm" onClick={keepPlanning} disabled={busy}>
                {t("PlanCard.keepPlanningButton")}
              </Button>
            </>
          ) : notice.status === "approved" ? (
            <span className="text-xs font-medium text-muted">{t("PlanCard.approvedStatus")}</span>
          ) : (
            <span className="text-xs font-medium text-muted">{t("PlanCard.dismissedStatus")}</span>
          )}
          {error && <span className="text-xs text-danger">{error}</span>}
        </div>
      </div>
    </div>
  );
}

export default PlanCard;

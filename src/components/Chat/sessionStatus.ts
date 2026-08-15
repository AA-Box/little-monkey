import { parsePlanNotice } from "../../lib/agentLoop";
import type { ChatSession } from "../../store/sessionStore";

/**
 * What a sidebar row says about a conversation without opening it:
 *
 * - `working` — a turn is in flight (the row animates).
 * - `attention` — the turn stopped on something only the user can answer:
 *   today that means a plan still waiting to be approved.
 * - `error` — the last turn threw.
 * - `finished` — the last turn ended cleanly while you were elsewhere, or
 *   the row was hand-marked unread.
 * - `null` — nothing to say; the row shows no dot at all.
 */
export type SessionStatus = "working" | "attention" | "error" | "finished";

/**
 * Derives one row's status from state that already exists — `runningTurns`
 * and `turnOutcomes` (sessionStore) plus the transcript itself. Ordered by
 * urgency: a running turn outranks a stale outcome (a session that failed,
 * then was retried, is "working", not "error"), and something the user has
 * to answer outranks how the previous turn ended.
 */
export function sessionStatus(
  session: ChatSession,
  running: boolean,
  outcome: "done" | "error" | undefined,
): SessionStatus | null {
  if (running) return "working";
  if (awaitsPlanApproval(session)) return "attention";
  if (outcome === "error") return "error";
  if (outcome === "done" || session.unread) return "finished";
  return null;
}

/**
 * Whether the most recent exchange ended on a plan card the user hasn't
 * acted on. Scoped to messages after the last user message rather than the
 * whole transcript: a plan the user simply talked past is history, not a
 * standing request, and would otherwise pin an "attention" dot on the row
 * forever.
 */
function awaitsPlanApproval(session: ChatSession): boolean {
  for (let i = session.messages.length - 1; i >= 0; i -= 1) {
    const message = session.messages[i];
    if (message.role === "user") return false;
    if (parsePlanNotice(message)?.status === "proposed") return true;
  }
  return false;
}

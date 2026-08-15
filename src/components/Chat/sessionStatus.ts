import { parsePlanNotice } from "../../lib/agentLoop";
import type { PermissionRequest } from "../../store/permissionStore";
import type { ChatSession } from "../../store/sessionStore";

/**
 * What a sidebar row says about a conversation without opening it:
 *
 * - `working` — a turn is in flight (the row animates).
 * - `attention` — the conversation is stopped on something only the user can
 *   answer: a permission prompt, or a plan still waiting to be approved.
 * - `error` — the last turn threw.
 * - `finished` — the last turn ended cleanly while you were elsewhere, or
 *   the row was hand-marked unread.
 * - `null` — nothing to say; the row shows no dot at all.
 */
export type SessionStatus = "working" | "attention" | "error" | "finished";

/**
 * The sessions blocked on an unanswered permission prompt, resolved through
 * `sessionStore`'s `turnSessions` (`turnId -> sessionId`). A request whose
 * turn isn't in the map — one raised outside any turn, or by a turn this
 * window doesn't own — belongs to no row and is simply left out; the modal
 * still shows it.
 */
export function sessionsAwaitingPermission(
  queue: readonly PermissionRequest[],
  turnSessions: Record<string, string>,
): Set<string> {
  const blocked = new Set<string>();
  for (const request of queue) {
    const sessionId = request.turn_id ? turnSessions[request.turn_id] : undefined;
    if (sessionId) blocked.add(sessionId);
  }
  return blocked;
}

/**
 * Derives one row's status from state that already exists — `runningTurns`,
 * `turnOutcomes` and `turnSessions` (sessionStore), the permission queue,
 * and the transcript itself. Ordered by urgency: anything the user has to
 * answer comes first (a turn waiting on a permission prompt is stopped, not
 * working, even though its turn is still in flight), then a running turn —
 * which outranks a stale outcome, so a session that failed and was retried
 * reads as "working", not "error".
 */
export function sessionStatus(
  session: ChatSession,
  running: boolean,
  outcome: "done" | "error" | undefined,
  awaitingPermission = false,
): SessionStatus | null {
  if (awaitingPermission) return "attention";
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

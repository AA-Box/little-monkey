/**
 * `/btw` — a quick side question that never joins the conversation.
 *
 * Runs a single tool-free model call against the active target: the current
 * transcript is sent as context (see `buildSideQuestionWire`), the answer
 * streams into a `[Btw]` notice in the transcript for display, and every wire
 * builder strips `[Btw]` notices before contacting a model, so neither the
 * question nor the answer is ever part of the conversation the model sees on
 * later turns.
 *
 * Deliberately NOT a turn: no tools are offered, `runningTurns` is never set,
 * and usage is not recorded against the session (`recordUsage: false` — the
 * same stance as `subagent.ts`: a side call's tokens must not clobber the
 * session's own context-usage ring that `/usage` and context trimming read).
 */
import { attemptStream } from './turnEngine';
import { resolveTarget } from './agentLoop';
import { effortForTarget } from '../store/modelStore';
import { sessionMessages, useSessionStore } from '../store/sessionStore';
import { buildSideQuestionWire, formatBtwNotice, type BtwNotice } from './slashCommands';

/** One in-flight side question per session — keyed like `agentLoop.ts`'s
 * per-session turn controllers, so `/stop` can cancel it. */
const sideQuestionControllers = new Map<string, AbortController>();

/** Aborts the session's in-flight side question, if any. Returns whether one
 * was actually running (so `/stop` can word its notice honestly). */
export function stopSideQuestion(sessionId: string): boolean {
  const controller = sideQuestionControllers.get(sessionId);
  if (!controller) return false;
  controller.abort();
  return true;
}

export async function runSideQuestion(sessionId: string, question: string): Promise<void> {
  if (sideQuestionControllers.has(sessionId)) {
    throw new Error('A side question is already running in this chat. Use /stop to cancel it.');
  }
  if (useSessionStore.getState().runningTurns[sessionId]) {
    throw new Error('A model turn is running. Wait for it to finish (or /stop it) before asking a side question.');
  }

  // Snapshot the context and resolve the target BEFORE appending the pending
  // notice, so a resolution failure (no model configured) surfaces as a plain
  // command error instead of a stranded half-notice.
  const history = sessionMessages(sessionId);
  const target = await resolveTarget();

  const controller = new AbortController();
  sideQuestionControllers.set(sessionId, controller);

  const noticeIndex = sessionMessages(sessionId).length;
  const patchNotice = (notice: BtwNotice) =>
    useSessionStore.getState().updateMessageAt(sessionId, noticeIndex, { content: formatBtwNotice(notice) });
  useSessionStore.getState().addMessage(sessionId, {
    role: 'system',
    content: formatBtwNotice({ question, answer: '', ok: true, done: false }),
  });

  try {
    const result = await attemptStream(
      target,
      buildSideQuestionWire(history, question),
      [],
      controller.signal,
      effortForTarget(target),
      sessionId,
      (content) => patchNotice({ question, answer: content, ok: true, done: false }),
      false,
    );
    if (result.streamError !== null) {
      const answer = result.contentStarted
        ? `${result.content}\n\n(interrupted: ${result.streamError})`
        : result.streamError;
      patchNotice({ question, answer, ok: false, done: true });
      return;
    }
    patchNotice({ question, answer: result.content.trim() || '(no answer)', ok: true, done: true });
  } finally {
    sideQuestionControllers.delete(sessionId);
  }
}

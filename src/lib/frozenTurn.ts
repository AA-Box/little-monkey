/**
 * Re-entering a frozen chat turn — the half of K13 that makes freeze/restore
 * true end to end.
 *
 * The storage half already writes a durable image (`checkpoint_freeze_live`)
 * and gates a restore (`checkpoint_restorability`). What was missing is the
 * thing that acts on them, and its absence was not a gap so much as a lie: a
 * chat turn suspended and then left across a restart has a `suspended` process
 * row and no loop behind it, so `deliverPause`'s resume arm cleared an
 * in-memory latch nobody was holding and reported `"resumed"`. Every sweep
 * reported it again, and the turn never continued.
 *
 * ## Resuming starts a new turn, on purpose
 *
 * The frozen row is exited and the continuation is admitted as its own
 * `chat_turn`. The alternative — re-admitting the original `externalId` — would
 * put two rows on one id, and the ledger's own rule is that a run charged by two
 * rows is charged by neither. The image is what links them, which is what an
 * image is for.
 *
 * ## What is checked before re-entry, and what deliberately is not
 *
 * `checkpoint_restorability` owns the verdict. This module only supplies the
 * environment, and the two inputs are worth stating:
 *
 * - **Resident models is the one target the app would run right now.** The
 *   blocker it feeds says resuming against a different model "would continue the
 *   conversation in another model's voice" — so the question is not what is
 *   installed, it is what this next round trip would actually reach.
 * - **Live approvals is empty, and that is not a shortcut.** A cooperative loop
 *   parks at a round boundary, after the previous round's tool calls and their
 *   permission prompts have already resolved. A turn frozen there has no
 *   outstanding approval, so the image records none and nothing can expire.
 */
import { invoke, isTauri } from '@tauri-apps/api/core';

import { describeUsageTarget } from './turnEngine';
import { RESUME_NOTE_PREFIX, resolveTarget, runAgentTurn } from './agentLoop';
import { exitProcess, type ProcessRecord } from './processTable';
import { useSessionStore } from '../store/sessionStore';

/** Mirrors `CheckpointInfo` in `src-tauri/src/checkpoints.rs`, narrowed to the
 * fields a resume reads. */
interface FrozenCheckpoint {
  id: string;
  sessionId: string;
  frozenProcessId: string | null;
}

/** Mirrors `RestoreReport` / `Restorability`. */
interface RestoreReport {
  restorability:
    | { state: 'resumable'; processId: string }
    | { state: 'blocked'; blockers: string[] };
  determinismCaveats: string[];
  /** One sentence per blocker, in the same order — the codes above are stable
   * identifiers, not something to show a person. */
  blockerExplanations: string[];
}

/** The image for `processId`, or `null` when nothing on disk claims it. */
export async function frozenCheckpointFor(processId: string): Promise<FrozenCheckpoint | null> {
  if (!isTauri()) return null;
  const all = await invoke<FrozenCheckpoint[]>('checkpoint_list', { sessionId: null }).catch(
    () => [] as FrozenCheckpoint[],
  );
  return all.find((entry) => entry.frozenProcessId === processId) ?? null;
}

/**
 * What a resume attempt did. Named rather than boolean because "there was no
 * image" and "the image cannot be restored here" call for different answers from
 * the caller — the first is an ordinary miss, the second is a refusal a user
 * needs to see.
 */
export type FrozenResumeOutcome = 'resumed' | 'no-image' | 'blocked';

/**
 * Re-enters the turn `record` froze, if it can be re-entered here.
 *
 * A blocked restore is reported *into the session's own transcript*, not
 * swallowed: the blockers each carry an explanation written for whoever has to
 * fix the missing thing, and the person who pressed Resume is the only one who
 * can act on them.
 */
export async function resumeFrozenTurn(record: ProcessRecord): Promise<FrozenResumeOutcome> {
  const image = await frozenCheckpointFor(record.processId);
  if (!image) return 'no-image';

  const sessions = useSessionStore.getState().sessions;
  if (!sessions.some((session) => session.id === image.sessionId)) {
    // The conversation was deleted while the image sat on disk. There is
    // nothing to continue, so the row is retired rather than left latched and
    // re-delivered by every sweep forever.
    await exitProcess(record.processId, 'cancelled', 'Its conversation no longer exists.');
    return 'no-image';
  }

  const report = await invoke<RestoreReport>('checkpoint_restorability', {
    id: image.id,
    residentModels: [describeUsageTarget(await resolveTarget())],
    liveApprovals: [],
  }).catch(() => null);
  if (!report || report.restorability.state !== 'resumable') {
    const reasons = report?.blockerExplanations ?? [];
    useSessionStore.getState().addMessage(image.sessionId, {
      role: 'system',
      content: [
        `${RESUME_NOTE_PREFIX} This turn was frozen and cannot be resumed here.`,
        ...reasons,
      ].join('\n'),
    });
    // Retired rather than left latched. The row is `suspended` with a resume
    // pending, so leaving it would have the sweep re-deliver every two seconds
    // and append the same refusal to the transcript forever. The turn genuinely
    // cannot continue on this host, which is what `failed` says.
    await exitProcess(
      record.processId,
      'failed',
      'Its frozen image cannot be restored on this host.',
    );
    return 'blocked';
  }

  // Cleared before the loop starts, not after. A resume that re-entered and
  // then failed to clear would leave an image describing a turn that is now
  // running — and the next restart would offer to resume it a second time.
  await invoke('checkpoint_clear_freeze', { id: image.id }).catch(() => undefined);
  await exitProcess(record.processId, 'succeeded', 'Resumed from its frozen image.');

  // No new user message: the conversation is already whole, and the turn is
  // continuing rather than being asked something new. `runAgentTurn` rejects a
  // second turn in a session on its own, so a resume racing a live turn is
  // refused there rather than guarded again here.
  //
  // `parentTurnId` is the frozen row's own `externalId` — the id the turn was
  // durably accepted under. It is what lets the backend continue *that* turn,
  // with the execution context frozen when it was accepted, instead of starting
  // something new against the machine's current configuration.
  //
  // The image's own id is this Resume's identity, and it is the right one for
  // the same reason it identifies the image: it was minted when the turn froze,
  // long before anything was submitted, and every route back to this point —
  // a re-delivered resume signal from the process sweep, a retry after the
  // command timed out, a second press while the first is in flight — finds the
  // same image and therefore sends the same id. The daemon collapses those onto
  // one continuation. A minted-per-call id would make each of them a separate
  // run of the work, and a resume that continues a turn twice is the one
  // outcome nothing downstream can undo.
  //
  // The next Resume of this conversation is a different image (this one is
  // cleared above, and a turn that freezes again writes a new one), so an
  // intentional second resume is a different id and its own continuation.
  await runAgentTurn(image.sessionId, '', [], undefined, undefined, [], [], false, {
    resumedFromCheckpointId: image.id,
    determinismCaveats: report.determinismCaveats,
    parentTurnId: record.externalId || null,
    resumeRequestId: image.id,
  });
  return 'resumed';
}

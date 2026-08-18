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
 * ## The order the steps happen in is the feature
 *
 * A Resume retires three things: the frozen image, the suspended process, and
 * the operator's ability to ask again. It also creates one: the durable
 * continuation. Doing the retiring first is what this module used to do, and it
 * meant a crash — or an `invoke` that never landed, or a daemon that had just
 * stopped — in the window between them left the person with no image, a process
 * marked `succeeded` that had succeeded at nothing, and no continuation. The
 * frozen state was not recoverable from anywhere, because the only copy of it
 * had been deleted to make room for a resume that did not happen.
 *
 * So acceptance comes first and everything else follows from it:
 *
 * 1. the image is read, and the turn's own durable identity with it;
 * 2. `checkpoint_restorability` says whether the *image* can be re-entered here;
 * 3. the continuation is submitted, under an id minted long before this attempt;
 * 4. only once the backend answers that it holds the continuation, the image is
 *    cleared and the old process retired;
 * 5. the run is watched.
 *
 * Anything that fails before step 4 leaves every input to step 3 exactly as it
 * found them, so the next attempt — the sweep's, two seconds later, or the
 * operator's, tomorrow — is the *same* Resume rather than a second one.
 *
 * ## What is checked before re-entry, and what deliberately is not
 *
 * `checkpoint_restorability` owns the verdict on the image, and only on the
 * image: whether it is a freeze at all, whether the workspace it named still
 * exists, whether an approval it was waiting on has expired. The two things it
 * is deliberately not asked are worth stating:
 *
 * - **Which model this would run against is not this process's question.** It
 *   used to be — this module passed the app's currently selected target as the
 *   host's "resident models" — and that made the eligibility of a turn accepted
 *   on Monday depend on a dropdown the operator changed on Tuesday. The
 *   continuation runs what the accepted turn was *frozen* with, so whether that
 *   model and its credential are still reachable is the accepting backend's
 *   question, and it refuses the resume by name when they are not.
 * - **Live approvals is empty, and that is not a shortcut.** A cooperative loop
 *   parks at a round boundary, after the previous round's tool calls and their
 *   permission prompts have already resolved. A turn frozen there has no
 *   outstanding approval, so the image records none and nothing can expire.
 */
import { invoke, isTauri } from '@tauri-apps/api/core';

import { RESUME_NOTE_PREFIX, runAgentTurn } from './agentLoop';
import { submitDurableResume } from './daemonDesktopTurn';
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
 * What a resume attempt did.
 *
 * Named rather than boolean because the callers act differently on each, and the
 * split that matters most is between the last two: `blocked` is an answer, and
 * `deferred` is the absence of one. A blocked resume has been decided and said
 * so in the transcript, so re-delivering it would repeat a refusal the operator
 * already read. A deferred resume was never decided — the request may not have
 * reached the backend, or its answer may not have come back — so the image, the
 * suspended row and the request id are all still there and the next sweep tries
 * the identical Resume again.
 */
export type FrozenResumeOutcome = 'resumed' | 'no-image' | 'blocked' | 'deferred';

/** Retires the frozen row and says why, in the session's own transcript.
 *
 * Retired rather than left latched: the row is `suspended`, and a suspended
 * `chat_turn` reads as a pending resume on every sweep, so leaving it would
 * append the same refusal every two seconds forever. The image is deliberately
 * *not* cleared — a refusal is a statement about this host at this moment, and
 * the frozen state stays on disk for whoever fixes the missing thing.
 */
async function refuse(
  record: ProcessRecord,
  sessionId: string,
  reasons: string[],
  exitReason: string,
): Promise<'blocked'> {
  useSessionStore.getState().addMessage(sessionId, {
    role: 'system',
    content: [
      `${RESUME_NOTE_PREFIX} This turn was frozen and cannot be resumed here.`,
      ...reasons,
    ].join('\n'),
  });
  await exitProcess(record.processId, 'failed', exitReason);
  return 'blocked';
}

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

  // The id the turn was durably accepted under. Without it there is no accepted
  // turn to continue, and the only alternative to refusing would be starting
  // something new against the machine's current configuration — which is the
  // one failure freezing exists to prevent, dressed up as success.
  const parentTurnId = record.externalId || null;
  if (!parentTurnId) {
    return refuse(
      record,
      image.sessionId,
      [
        'This frozen turn predates durable turn identity, so it cannot be continued with the configuration it was accepted under. Ask again in a new turn.',
      ],
      'Its frozen image predates durable turn identity.',
    );
  }

  // `residentModels` is deliberately absent: see this module's header. What the
  // image can answer for is asked here; what the accepted turn's frozen context
  // answers for is asked, below, of the backend that holds it.
  const report = await invoke<RestoreReport>('checkpoint_restorability', {
    id: image.id,
    liveApprovals: [],
  }).catch(() => null);
  if (!report || report.restorability.state !== 'resumable') {
    return refuse(
      record,
      image.sessionId,
      report?.blockerExplanations ?? [],
      'Its frozen image cannot be restored on this host.',
    );
  }

  // The image's own id is this Resume's identity, and it is the right one for
  // the same reason it identifies the image: it was minted when the turn froze,
  // long before anything was submitted, and every route back to this point — a
  // re-delivered resume signal from the process sweep, a retry after the command
  // timed out, an app restarted mid-Resume, a second press while the first is in
  // flight — finds the same image on disk and therefore sends the same id. The
  // daemon collapses those onto one continuation. A minted-per-call id would
  // make each of them a separate run of the work, and a resume that continues a
  // turn twice is the one outcome nothing downstream can undo.
  //
  // It survives a crash for free, which is the point of taking it from the image
  // rather than holding it in memory: the image is not cleared until the line
  // below has already been answered, so a process that dies anywhere before then
  // leaves the next attempt reading the same id off the same file.
  //
  // The next Resume of this conversation is a different image (this one is
  // cleared once the continuation exists, and a turn that freezes again writes a
  // new one), so an intentional second resume is a different id and its own
  // continuation.
  const submission = await submitDurableResume(
    'desktop',
    image.sessionId,
    parentTurnId,
    image.id,
  );
  if (submission.state === 'pending') {
    // Nothing is known, so nothing is retired. The image, the suspended row and
    // the request id are exactly as they were, and the next sweep sends the
    // identical Resume — which the backend answers with the continuation it
    // made, if it made one.
    //
    // Logged rather than written into the transcript: the operator has not been
    // answered, because there is no answer yet, and a note per sweep would be
    // the refusal-forever bug wearing a different hat. But a Resume that retries
    // silently until the runner comes back still has to be diagnosable, and this
    // is the only place the transport's own reason survives.
    console.warn(
      `[frozenTurn] resume ${image.id} was not accepted; the image is kept and the sweep will retry:`,
      submission.error,
    );
    return 'deferred';
  }
  if (submission.state === 'refused') {
    return refuse(
      record,
      image.sessionId,
      [submission.reason],
      'The durable backend refused to continue this turn.',
    );
  }

  // Past this line the continuation exists durably, and everything below is
  // cleanup that the continuation does not depend on. A crash here leaves a
  // stale image whose next resume attempt re-sends the same request id and is
  // answered with this same continuation — one run, and then the image is
  // cleared on that pass instead of this one.
  await invoke('checkpoint_clear_freeze', { id: image.id }).catch(() => undefined);
  await exitProcess(record.processId, 'succeeded', 'Resumed from its frozen image.');

  // No new user message: the conversation is already whole, and the turn is
  // continuing rather than being asked something new. `runAgentTurn` rejects a
  // second turn in a session on its own, so a resume racing a live turn is
  // refused there rather than guarded again here.
  await runAgentTurn(image.sessionId, '', [], undefined, undefined, [], [], false, {
    resumedFromCheckpointId: image.id,
    determinismCaveats: report.determinismCaveats,
    parentTurnId,
    accepted: submission.accepted,
  });
  return 'resumed';
}

/**
 * Process-local latch for cooperative pause — the "block until resumed"
 * counterpart to `runCancellationRegistry.ts`'s "abort now". Keyed exactly
 * the way every loop already keys its process-table admission's `externalId`
 * (a turn id, a subagent's cancel id, a crew actor's durable run id), so the
 * single `processes://changed` fan-in in `App.tsx` can drive every loop
 * through one map without any loop needing to know Tauri exists.
 *
 * Side tasks are deliberately NOT routed through this module — see
 * `sideTaskRunner.ts`'s `waitUntilResumed`, which is the pre-existing
 * reference implementation and stays the single source of truth for that
 * kind, so its own Pause button and an incoming `process_signal` converge on
 * the same store instead of two competing latches.
 */
import { markProcessRunning, markProcessSuspended } from './processTable';

interface PauseEntry {
  paused: boolean;
  listeners: Set<() => void>;
}

const pauseLatches = new Map<string, PauseEntry>();

function entry(key: string): PauseEntry {
  let e = pauseLatches.get(key);
  if (!e) {
    e = { paused: false, listeners: new Set() };
    pauseLatches.set(key, e);
  }
  return e;
}

/** Called by the `processes://changed` fan-in whenever a live process's
 * `signalIntent.suspendRequested` flips. */
export function setPauseRequested(key: string, requested: boolean): void {
  const e = entry(key);
  if (e.paused === requested) return;
  e.paused = requested;
  e.listeners.forEach((listener) => listener());
}

export function isPauseRequested(key: string): boolean {
  return pauseLatches.get(key)?.paused ?? false;
}

/**
 * Drops `key`'s latch entirely — call from every loop's teardown so a long
 * session doesn't accumulate one entry per finished turn/task/actor forever.
 *
 * Releases anyone parked on it first. Deleting the map entry does NOT reach a
 * waiter: `waitWhileRequested` closes over the entry object and is subscribed to
 * that object's listener set, so dropping the key alone would leave the waiter
 * holding a latch nothing can ever clear.
 */
export function forgetPause(key: string): void {
  setPauseRequested(key, false);
  pauseLatches.delete(key);
}

/** Resolves once `key`'s latch clears or `signal` aborts. Never polls — same
 * mechanism as `sideTaskRunner.ts`'s `waitUntilResumed`. */
export function waitWhileRequested(key: string, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    const e = entry(key);
    if (signal.aborted || !e.paused) {
      resolve();
      return;
    }
    const onChange = () => {
      if (signal.aborted || !e.paused) {
        e.listeners.delete(onChange);
        signal.removeEventListener('abort', onAbort);
        resolve();
      }
    };
    const onAbort = () => onChange();
    e.listeners.add(onChange);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

/**
 * The one call every loop's safe point uses. Makes `state` honest around the
 * wait: `Suspended` only once actually parked here, `Running` again only
 * once actually resumed — never merely because a signal arrived somewhere
 * mid-flight. `processId` may be a promise (subagent/crew admit is
 * fire-and-forget) or a plain value/`null` — fail-soft, same posture as
 * every `processTable.ts` caller.
 */
export async function honourPause(
  key: string,
  processId: string | null | Promise<string | null>,
  signal: AbortSignal,
): Promise<void> {
  if (signal.aborted || !isPauseRequested(key)) return;
  const id = await processId;
  if (id) await markProcessSuspended(id);
  await waitWhileRequested(key, signal);
  if (id && !signal.aborted) await markProcessRunning(id);
}

export function clearPauseRegistryForTests(): void {
  pauseLatches.clear();
}

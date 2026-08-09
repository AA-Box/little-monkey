/**
 * The revert/reapply entry point every surface uses, so a compensating action
 * cannot be forgotten by one of them (roadmap K14).
 *
 * `checkpoint_revert` undoes the workspace files and, on the Rust side, the
 * facts the turn remembered. One compensator cannot live there: a follow-up
 * task chip is `taskSuggestionStore`'s, which is frontend state, so the undo has
 * to run here.
 *
 * ## Why that does not make `Compensation::Undo` a lie
 *
 * The variant states *what reverting does*, not which process does it. Calling
 * an undo this app owns end to end "unrecoverable" because its store happens to
 * sit on the far side of the IPC boundary would be the same mistake the memory
 * arm already corrected — not being snapshotted is not the same as not being
 * undoable, and neither is not being in Rust.
 *
 * What the split does cost is that a caller invoking `checkpoint_revert`
 * directly would skip the chip half. That is exactly why this module exists and
 * why nothing outside it calls those two commands: the compensator is attached
 * to the operation, not to each of the four buttons that trigger it.
 */
import { invoke } from '@tauri-apps/api/core';

import { useTaskSuggestionStore } from '../store/taskSuggestionStore';

/** The chips a turn staged, read before the revert rewrites its manifest. */
async function stagedSuggestions(id: string): Promise<string[]> {
  return invoke<string[]>('checkpoint_staged_task_suggestions', { id }).catch(() => []);
}

/**
 * Reverts checkpoint `id` and withdraws the follow-up chips its turn proposed.
 *
 * The read happens **before** the revert, mirroring `checkpoint_revert`'s own
 * ordering for remembered facts: the revert rewrites the manifest, and a caller
 * that read it afterwards could find the list it needs already changed.
 *
 * A chip the user already dismissed or started is left alone by the store's own
 * `dismiss`/`restore` guards, so withdrawing twice is a no-op rather than a
 * second state change.
 */
export async function revertCheckpoint(id: string): Promise<void> {
  const staged = await stagedSuggestions(id);
  await invoke('checkpoint_revert', { id });
  const store = useTaskSuggestionStore.getState();
  for (const suggestionId of staged) store.dismiss(suggestionId);
}

/** Re-applies checkpoint `id` and puts its withdrawn chips back.
 *
 * Symmetrical with the above for the reason `redo/` keeps a file's post-turn
 * bytes: an undo that cannot itself be undone is data loss with a friendly
 * name. */
export async function reapplyCheckpoint(id: string): Promise<void> {
  const staged = await stagedSuggestions(id);
  await invoke('checkpoint_reapply', { id });
  const store = useTaskSuggestionStore.getState();
  for (const suggestionId of staged) store.restore(suggestionId);
}

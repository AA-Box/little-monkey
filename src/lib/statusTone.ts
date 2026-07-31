import type { PillTone } from "../components/ui";

/**
 * One status-to-pill-tone mapping for every workbench panel.
 *
 * Thirteen panels each carried a private `statusTone` over their own status
 * union. The vocabularies overlap almost entirely — every panel has a
 * terminal success, a failure, an in-flight state, and an inert state — so
 * the differences between the copies were mostly accidental: the same word
 * ("queued", "paused") picked up a different colour depending on which panel
 * you were looking at.
 *
 * The default vocabulary below is the union of what those copies agreed on.
 * A panel whose domain genuinely disagrees passes `overrides`, which keeps
 * the disagreement explicit and local instead of hiding it in a thirteenth
 * near-copy.
 */
const DEFAULT_TONES: Record<string, PillTone> = {
  // Terminal success.
  passed: "success",
  completed: "success",
  succeeded: "success",
  success: "success",
  done: "success",
  resolved: "success",
  closed: "success",
  applied: "success",
  approved: "success",
  ready: "success",
  planned: "success",

  // Terminal failure.
  failed: "danger",
  error: "danger",
  errored: "danger",
  rejected: "danger",
  blocked: "danger",
  needs_reconciliation: "danger",

  // In flight / needs attention.
  running: "warning",
  streaming: "warning",
  pending: "warning",
  in_progress: "warning",
  waiting: "warning",
  waiting_for_permission: "warning",
  cancelling: "warning",
  retrying: "warning",

  // Inert.
  queued: "neutral",
  idle: "neutral",
  draft: "neutral",
  paused: "neutral",
  cancelled: "neutral",
  canceled: "neutral",
  skipped: "neutral",
  unknown: "neutral",
  not_started: "neutral",
};

/**
 * @param status  A status string from any panel's own union.
 * @param overrides Domain-specific mappings applied before the shared
 *   vocabulary — e.g. an incident's `declared` is a `danger`, while a side
 *   task's `queued` is a `warning` because the user is waiting on it.
 * @returns The pill tone; `neutral` for a status no vocabulary covers, which
 *   is the same conservative default every previous copy used.
 */
export function statusTone(
  status: string,
  overrides?: Readonly<Record<string, PillTone>>,
): PillTone {
  return overrides?.[status] ?? DEFAULT_TONES[status] ?? "neutral";
}

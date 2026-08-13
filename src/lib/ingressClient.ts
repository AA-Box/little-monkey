import { invoke } from "@tauri-apps/api/core";

/**
 * Conversation ingress, over the typed daemon bridge.
 *
 * A turn is one thing that arrived from outside and asked Little Monkey to do
 * something: a message on a channel, an inbound call, a paired phone, a peer
 * node, a voice turn. They all take the same durable path, so they all read
 * back through one call rather than one per subsystem.
 *
 * Nothing here carries message text or a credential — the backend listing has
 * no field for either. This is status: where it came from, what it became, and
 * why it failed if it did.
 */

/** Origins a turn can arrive on. Matches `ConversationSource` in Rust; the
 * strings are persisted, so they are part of the durable contract. */
export type ConversationSource =
  | "desktop"
  | "mobile"
  | "messaging_channel"
  | "peer"
  | "voice"
  | "telephone";

/** Where the turn is on its way to a run. `accepted` means Little Monkey has
 * it durably but the queue does not yet — a restart resumes those. `failed`
 * means it ran out of attempts and needs a human. */
export type IngressTurnState = "accepted" | "queued" | "failed";

export interface IngressTurn {
  ingress_id: string;
  source: ConversationSource;
  /** Account, device, line or node the turn arrived on. */
  source_account_id: string;
  /** The operator's own name for that account, when the origin has one. */
  account_label: string | null;
  /** The originating system's event id. Scoped by the account. */
  source_event_id: string;
  /** Durable session the turn continues. */
  session_key: string;
  state: IngressTurnState;
  attempts: number;
  /** Why the last submission attempt failed, if one did. */
  last_error: string | null;
  /** Which frozen-execution shape the turn was accepted with, and its digest.
   * Both null for a turn accepted before contexts were frozen. The digest is
   * what proves two runs of the same turn used the same configuration. */
  execution_version: number | null;
  execution_digest: string | null;
  job_id: string | null;
  run_id: string | null;
  run_state: string | null;
  /** Why the run failed, as the daemon recorded it. */
  run_error: string | null;
  created_at_ms: number;
  updated_at_ms: number;
}

export const SOURCE_LABELS: Record<ConversationSource, string> = {
  desktop: "Desktop",
  mobile: "Mobile device",
  messaging_channel: "Messaging channel",
  peer: "Peer node",
  voice: "Voice",
  telephone: "Phone",
};

export const ingressTurns = (source: ConversationSource | null = null, limit = 20) =>
  invoke<{ turns: IngressTurn[] }>("ingress_turns", { source, limit });

/** How this turn is doing, in one word an operator can act on.
 *
 * A turn and its run are two different facts — Little Monkey can have taken a
 * message that has not started running, and a queued turn whose run failed is
 * a run problem, not an ingress one — so the run's state wins once there is
 * one. */
export function turnStatus(turn: IngressTurn): "waiting" | "running" | "done" | "failed" {
  if (turn.state === "failed") return "failed";
  switch (turn.run_state) {
    case "succeeded":
      return "done";
    case "failed":
    case "cancelled":
    case "needs_reconciliation":
      return "failed";
    case null:
    case undefined:
    case "preparing":
    case "queued":
      return "waiting";
    default:
      return "running";
  }
}

/** The reason to show next to a failed turn, preferring the run's own error
 * over the submission one: if a run started, the submission worked. */
export function turnFailureReason(turn: IngressTurn): string | null {
  return turn.run_error ?? turn.last_error;
}

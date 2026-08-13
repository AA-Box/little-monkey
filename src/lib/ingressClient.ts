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
  /** Whether the accepted turn promised the workspace would be different
   * afterwards. Only a surface that can ask for file edits ever sets it. */
  mutation_required: boolean;
  /** Where that promise ended up, or null while the run is still going. */
  mutation_state: MutationState | null;
  /** What the run reported about the workspace, or why nothing could be read.
   * A file count and at most a tool's own error — never message text. */
  mutation_detail: string | null;
  /** The accepted turn this one continues. Null for a turn a person asked for
   * directly. */
  parent_ingress_id: string | null;
  continuation_kind: ContinuationKind | null;
  continuation_attempt: number;
  job_id: string | null;
  run_id: string | null;
  run_state: string | null;
  /** Why the run failed, as the daemon recorded it. */
  run_error: string | null;
  created_at_ms: number;
  updated_at_ms: number;
}

/** How an accepted turn's workspace-mutation contract was settled.
 * `corrected` means a durable corrective continuation was submitted;
 * `interrupted` means the run stopped before it could report, and nothing is
 * replayed automatically. */
export type MutationState = "satisfied" | "corrected" | "unmet" | "interrupted";

/** Why a turn exists that no person typed. Both kinds inherit their parent's
 * frozen execution context, so neither can run a configuration the original
 * turn was not accepted under. */
export type ContinuationKind = "mutation_correction" | "resume";

/** One turn with every continuation it produced, oldest first. */
export interface IngressTurnDetail {
  turn: IngressTurn | null;
  continuations: IngressTurn[];
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

/** One turn, by the identity its origin submitted it under, with its
 * continuations. How a surface watching a turn learns that the run answering
 * the operator is a continuation's rather than the one it submitted. */
export const ingressTurnShow = (source: ConversationSource, account: string, event: string) =>
  invoke<IngressTurnDetail>("ingress_turn_show", { source, account, event });

/** Asks the durable backend to continue an accepted turn that was frozen at a
 * tool boundary. The backend inherits the turn's frozen execution context; the
 * caller only gets the run to watch. */
export const ingressTurnResume = (source: ConversationSource, account: string, event: string) =>
  invoke<{ ingress_id: string; parent_ingress_id: string; job_id: string; run_id: string }>(
    "ingress_turn_resume",
    { source, account, event },
  );

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

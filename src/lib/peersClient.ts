import { invoke } from "@tauri-apps/api/core";

/**
 * Peers, over the typed daemon bridge.
 *
 * A peer is another Little Monkey installation the operator paired with
 * deliberately. Pairing is cryptographic — a pinned certificate and a signing
 * secret — and it is *not* trust: what a peer may ask for is a separate set of
 * grants, shown separately everywhere in the UI for exactly that reason.
 *
 * Nothing here carries a pairing token. An invitation is written to a file the
 * operator chooses and moved out of band, the same way a controller pairing
 * already works.
 */

/** What a peer may ask for. Each is granted on its own; none implies another,
 * and none of them reaches runs, approvals or the desktop. */
export type PeerGrant = "message" | "task" | "artifact";

export const PEER_GRANTS: { id: PeerGrant; labelKey: string; detailKey: string }[] = [
  { id: "message", labelKey: "PeersPanel.grantMessage", detailKey: "PeersPanel.grantMessageDetail" },
  { id: "task", labelKey: "PeersPanel.grantTask", detailKey: "PeersPanel.grantTaskDetail" },
  { id: "artifact", labelKey: "PeersPanel.grantArtifact", detailKey: "PeersPanel.grantArtifactDetail" },
];

/** An installation paired *into* this one: it can ask, subject to its grants. */
export interface InboundPeer {
  device_id: string;
  label: string;
  grants: PeerGrant[];
  state: "active" | "revoked";
  /** True when the pairing carries peer grants and nothing else — no runs, no
   * approvals, no desktop. False means the same credential is also a
   * controller or a companion device, which the UI has to say out loud. */
  peer_only: boolean;
  last_sequence: number;
}

/** An installation this one is paired *with*, reachable by alias. */
export interface OutboundPeer {
  alias: string;
  peer_id: string;
  peer_url: string;
  /** What the far side allows this installation to do there. */
  grants: PeerGrant[];
  certificate_sha256: string;
}

export interface PeerThreadMessage {
  message_id: string;
  direction: "inbound" | "outbound";
  kind: string;
  disposition: "accepted" | "rejected" | "delivered";
  rejection: string | null;
  job_id: string | null;
  created_at_ms: number;
}

export interface PeerThread {
  thread_id: string;
  peer_device_id: string;
  peer_instance_id: string;
  session_key: string;
  created_at_ms: number;
  last_activity_at_ms: number;
  message_count: number;
  recent: PeerThreadMessage[];
}

export const peersList = () => invoke<{ inbound: InboundPeer[]; outbound: OutboundPeer[] }>("peers_list");

export const peersInvite = (label: string, allow: PeerGrant[], expiresMinutes: number, output: string) =>
  invoke<{ pairing_id: string; expires_at_ms: number; grants: PeerGrant[]; output: string }>("peers_invite", {
    label,
    allow,
    expiresMinutes,
    output,
  });

export const peersAccept = (invitation: string, alias: string) =>
  invoke<{ alias: string; peer_id: string; peer_url: string; grants: PeerGrant[]; certificate_sha256: string }>(
    "peers_accept",
    { invitation, alias },
  );

export const peersGrant = (deviceId: string, allow: PeerGrant[]) =>
  invoke<{ device_id: string; grants: PeerGrant[] }>("peers_grant", { deviceId, allow });

export const peersRevoke = (deviceId: string, reason: string) =>
  invoke<void>("peers_revoke", { deviceId, reason });

export const peersThreads = (peer: string | null = null, limit = 20) =>
  invoke<{ threads: PeerThread[]; recipe: string }>("peers_threads", { peer, limit });

/** A fingerprint an operator can compare out of band, in readable groups.
 * Shown in full rather than truncated: a fingerprint you cannot compare
 * completely is decoration. */
export function formatFingerprint(sha256: string): string {
  return (sha256.match(/.{1,8}/g) ?? [sha256]).join(" ");
}

/** What a pairing actually means, in one phrase.
 *
 * Never "trusted": the pairing proves who the peer is, the grants decide what
 * it may do, and a peer with no grants is paired and powerless. */
export function standingSummary(peer: InboundPeer): "revoked" | "no-grants" | "mixed" | "peer-only" {
  if (peer.state === "revoked") return "revoked";
  if (peer.grants.length === 0) return "no-grants";
  return peer.peer_only ? "peer-only" : "mixed";
}

/** Whether a thread has anything the operator should look at: a refusal is
 * worth surfacing, an ordinary accepted exchange is not. */
export function hasRejection(thread: PeerThread): boolean {
  return thread.recent.some((message) => message.disposition === "rejected");
}

/** Declarative peer placement validation shared by workflow and DAG parser.
 * No daemon import or model-runtime side effects at parse time. */
const PEER_ALIAS_RE = /^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,119}$/;
export function validSwarmPeer(value: unknown): value is string {
  return typeof value === 'string' && PEER_ALIAS_RE.test(value);
}
export function assertReadOnlyPeerPlacement(peer: unknown, profile: string, isolated = false): asserts peer is string {
  if (!validSwarmPeer(peer) || profile !== 'explore' || isolated) {
    throw new Error('A workflow peer must be a valid paired alias, only for a read-only explore agent without worktree isolation.');
  }
}

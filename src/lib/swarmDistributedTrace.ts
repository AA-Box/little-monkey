/** Distributed resident swarm observations.
 *
 * Local journal transitions and HMAC/TLS-authorized peer state snapshots are
 * different evidence classes. Never combine them into a trusted completed
 * task, an election certificate, model correctness or provider invoice.
 */
import type { ResidentSwarmGraphStatus, DaemonSwarmReplica } from './daemonClient';

export interface DistributedTraceEvent {
  source: 'local-ledger' | 'signed-peer-observation';
  peerFingerprint: string | null;
  node: string;
  state: string;
  occurredAt: number;
  sequence: number;
  term: number | null;
  verifiedExecution: boolean;
}
export interface DistributedTraceOverview {
  graph: string;
  receivedPeers: number;
  freshPeers: number;
  stalePeers: number;
  invalidPeerRecords: number;
  latestReportedTerm: number | null;
  events: DistributedTraceEvent[];
}
const HEX = /^[0-9a-f]{64}$/;
const TOKEN = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,79}$/;
const FRESH_WINDOW_MS = 120_000;
export function composeDistributedSwarmTrace(
  graph: ResidentSwarmGraphStatus,
  snapshots: readonly DaemonSwarmReplica[],
  now = Date.now(),
): DistributedTraceOverview {
  if (!Number.isFinite(now) || now <= 0 || !TOKEN.test(graph.id)) {
    throw new Error('Invalid distributed trace context');
  }
  const events: DistributedTraceEvent[]=[];
  for(const row of (graph.events??[]).slice(-128)) {
    if(!Number.isSafeInteger(row.sequence)||row.sequence<1||
       !Number.isFinite(row.occurred_at_ms)||row.occurred_at_ms<=0||
       typeof row.state!=='string'||row.state.length>50) continue;
    events.push({
      source:'local-ledger',peerFingerprint:null,node:row.node_id??'graph',
      state:row.state,occurredAt:row.occurred_at_ms,
      sequence:row.sequence,term:null,
      // A local journal state is not a source-verified model grade.
      verifiedExecution:false,
    });
  }
  let fresh=0,stale=0,invalid=0;
  let latestTerm:number|null=null;
  const seen=new Set<string>();
  for(const peer of snapshots.slice(0,7)) {
    if(!peer||peer.graph_id!==graph.id||!HEX.test(peer.source_fingerprint)||
       !HEX.test(peer.roster_sha256)||seen.has(peer.source_fingerprint)||
       !Number.isSafeInteger(peer.sequence)||peer.sequence<1||
       !Number.isSafeInteger(peer.term)||peer.term<1||
       !Number.isFinite(peer.captured_at_ms)||peer.captured_at_ms<=0||
       peer.captured_at_ms>now+5000||peer.nodes.length>16||
       peer.nodes.some(node=>!TOKEN.test(node.id)||typeof node.state!=='string'||node.state.length>50)){
      invalid++;
      continue;
    }
    seen.add(peer.source_fingerprint);
    const freshEnough=now-peer.captured_at_ms<=FRESH_WINDOW_MS;
    if(freshEnough)fresh++;else stale++;
    latestTerm=Math.max(latestTerm??0,peer.term);
    // An authenticated observation attests to the publisher identity,
    // NOT the correctness or authoritative completion of its task.
    events.push({
      source:'signed-peer-observation',peerFingerprint:peer.source_fingerprint,
      node:'graph',state:peer.graph_state,occurredAt:peer.captured_at_ms,
      sequence:peer.sequence,term:peer.term,verifiedExecution:false,
    });
    for(const node of peer.nodes){
      events.push({
        source:'signed-peer-observation',peerFingerprint:peer.source_fingerprint,
        node:node.id,state:node.state,occurredAt:peer.captured_at_ms,
        sequence:peer.sequence,term:peer.term,verifiedExecution:false,
      });
    }
  }
  events.sort((a,b)=>a.occurredAt-b.occurredAt||
    (a.source==='local-ledger'?-1:1)||
    a.sequence-b.sequence||a.node.localeCompare(b.node));
  return {
    graph:graph.id,receivedPeers:seen.size,freshPeers:fresh,
    stalePeers:stale,invalidPeerRecords:invalid,
    latestReportedTerm:latestTerm,events:events.slice(-160),
  };
}

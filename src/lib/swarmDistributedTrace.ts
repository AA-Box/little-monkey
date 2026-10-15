/** Read-only cross-host DAG telemetry. Peer observations have been authenticated,
 * roster-checked and committed by the daemon's signed pinned-TLS receiver.
 * Even a real succeeded transition is NOT independent verification of a
 * model response, a permission grant, or a completed external side effect.
 */
import type { ResidentSwarmGraphStatus, DaemonSwarmReplica } from './daemonClient';

export interface DistributedTraceEvent {
  source: 'local-ledger' | 'signed-peer-observation';
  peerFingerprint: string | null;
  node: string;
  kind: string;
  state: string;
  occurredAt: number;
  sequence: number;
  term: number | null;
  verifiedExecution: false;
}
export interface DistributedTraceMeteredUsage {
  sourceFingerprint:string;
  node:string;
  status:'succeeded'|'failed';
  targetId:string;
  inputTokens:number;
  outputTokens:number;
  cachedInputTokens:number;
  modelCalls:number;
  elapsedMs:number;
  meteredCostMicros:number|null;
  providerBilled:false;
  independentlyQualityVerified:false;
}
export interface DistributedTraceOverview {
  graph: string;
  receivedPeers: number;
  freshPeers: number;
  stalePeers: number;
  invalidPeerRecords: number;
  historicalPeerEvents: number;
  meteredPeerRuns: number;
  meteredUsage: DistributedTraceMeteredUsage[];
  legacySnapshotPeers: number;
  latestReportedTerm: number | null;
  events: DistributedTraceEvent[];
}
const HEX = /^[0-9a-f]{64}$/;
const TOKEN = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,79}$/;
const STATES = new Set(['running','failing','cancelling','succeeded','failed','cancelled',
  'pending','pending_fallback','preparing','preparing_fallback','submitted','submitted_fallback']);
const EVENT_KINDS = new Set(['graph_created','graph_transition','node_created','node_transition']);
const FRESH_WINDOW_MS = 120_000;
const MAX_PEER_EVENTS = 48;
const isTime = (n: unknown): n is number => typeof n==='number' && Number.isSafeInteger(n) && n>0;
export function composeDistributedSwarmTrace(
  graph: ResidentSwarmGraphStatus,
  snapshots: readonly DaemonSwarmReplica[],
  now = Date.now(),
): DistributedTraceOverview {
  if (!isTime(now) || !TOKEN.test(graph.id)) {
    throw new Error('Invalid distributed trace context');
  }
  const events: DistributedTraceEvent[]=[];
  const meteredUsage: DistributedTraceMeteredUsage[]=[];
  for(const row of (graph.events??[]).slice(-128)) {
    if(!Number.isSafeInteger(row.sequence)||row.sequence<1||
       !isTime(row.occurred_at_ms)||row.occurred_at_ms>now+5000||
       !STATES.has(row.state)||
       typeof row.node_id!=='string'&&row.node_id!==null||
       (row.node_id!==null&&!TOKEN.test(row.node_id))||
       !EVENT_KINDS.has(row.kind)) continue;
    events.push({
      source:'local-ledger',peerFingerprint:null,node:row.node_id??'graph',
      kind:row.kind,state:row.state,occurredAt:row.occurred_at_ms,
      sequence:row.sequence,term:null,verifiedExecution:false,
    });
  }
  let fresh=0,stale=0,invalid=0,historical=0,legacy=0;
  let latestTerm:number|null=null;
  const seen=new Set<string>();
  for(const peer of snapshots.slice(0,7)) {
    if(!peer||peer.graph_id!==graph.id||
       !HEX.test(peer.source_fingerprint)||!HEX.test(peer.roster_sha256)||
       !HEX.test(peer.checksum)||seen.has(peer.source_fingerprint)||
       !Number.isSafeInteger(peer.sequence)||peer.sequence<1||
       !Number.isSafeInteger(peer.term)||peer.term<1||
       !isTime(peer.captured_at_ms)||peer.captured_at_ms>now+5000||
       !STATES.has(peer.graph_state)||!Array.isArray(peer.nodes)||
       peer.nodes.length===0||peer.nodes.length>16||
       peer.nodes.some(node=>!TOKEN.test(node.id)||!STATES.has(node.state))||
       (peer.version!==1 && peer.version!==2 && peer.version!==3)){
      invalid++;continue;
    }
    const remoteEvents = peer.trace_events??[];
    let prior = 0;
    if(!Array.isArray(remoteEvents)||remoteEvents.length>MAX_PEER_EVENTS||
       (peer.version===1&&remoteEvents.length>0)||
       remoteEvents.some(event=>{
         if(!event || !Number.isSafeInteger(event.sequence)||event.sequence<=prior||
           !isTime(event.occurred_at_ms)||event.occurred_at_ms>peer.captured_at_ms+5000||
           !EVENT_KINDS.has(event.kind)||!STATES.has(event.state)||
           (event.node_id!==null &&
             (typeof event.node_id!=='string'||!TOKEN.test(event.node_id)||
               !peer.nodes.some(node=>node.id===event.node_id)))) return true;
         prior=event.sequence;
         return false;
       })){
      invalid++;continue;
    }
    const usage=peer.model_usage??[];
    const usageNodes=new Set<string>();
    if(!Array.isArray(usage)||usage.length>16||
       (peer.version<3&&usage.length>0)||
       usage.some(row=>{
        if(!row||typeof row.node_id!=='string'||!TOKEN.test(row.node_id)||
          usageNodes.has(row.node_id)||!peer.nodes.some(node=>
            node.id===row.node_id && node.state===row.status)||
          (row.status!=='succeeded'&&row.status!=='failed')||
          typeof row.target_id!=='string'||
          !/^[A-Za-z0-9][A-Za-z0-9_./:@+%-]{0,159}$/.test(row.target_id)||
          !Number.isSafeInteger(row.elapsed_ms)||row.elapsed_ms<0||row.elapsed_ms>86_400_000||
          !Number.isSafeInteger(row.input_tokens)||row.input_tokens<0||row.input_tokens>100_000_000||
          !Number.isSafeInteger(row.output_tokens)||row.output_tokens<0||row.output_tokens>100_000_000||
          !Number.isSafeInteger(row.cached_input_tokens)||row.cached_input_tokens<0||
          row.cached_input_tokens>row.input_tokens||
          !Number.isSafeInteger(row.model_calls)||row.model_calls<1||row.model_calls>100_000||
          (row.cost_micros!==null&&
            (!Number.isSafeInteger(row.cost_micros)||row.cost_micros<0||
             row.cost_micros>1_000_000_000_000)))return true;
        usageNodes.add(row.node_id);
        return false;
       })){invalid++;continue;}
    seen.add(peer.source_fingerprint);
    for(const row of usage){
      meteredUsage.push({
        sourceFingerprint:peer.source_fingerprint,node:row.node_id,status:row.status,targetId:row.target_id,
        inputTokens:row.input_tokens,outputTokens:row.output_tokens,
        cachedInputTokens:row.cached_input_tokens,modelCalls:row.model_calls,
        elapsedMs:row.elapsed_ms,meteredCostMicros:row.cost_micros,
        providerBilled:false,independentlyQualityVerified:false,
      });
    }
    const freshEnough=now-peer.captured_at_ms<=FRESH_WINDOW_MS;
    if(freshEnough)fresh++;else stale++;
    latestTerm=Math.max(latestTerm??0,peer.term);
    if(remoteEvents.length){
      historical+=remoteEvents.length;
      for(const event of remoteEvents){
        events.push({
          source:'signed-peer-observation',peerFingerprint:peer.source_fingerprint,
          node:event.node_id??'graph',kind:event.kind,state:event.state,
          occurredAt:event.occurred_at_ms,sequence:event.sequence,
          term:peer.term,verifiedExecution:false,
        });
      }
    }else{
      // v1 peers have no event journal; their state is a point-in-time
      // snapshot. Never misrepresent its timestamp as the time a task ended.
      legacy++;
      events.push({
        source:'signed-peer-observation',peerFingerprint:peer.source_fingerprint,
        node:'graph',kind:'checkpoint_snapshot',state:peer.graph_state,
        occurredAt:peer.captured_at_ms,sequence:peer.sequence,
        term:peer.term,verifiedExecution:false,
      });
      for(const node of peer.nodes){
        events.push({
          source:'signed-peer-observation',peerFingerprint:peer.source_fingerprint,
          node:node.id,kind:'checkpoint_snapshot',state:node.state,
          occurredAt:peer.captured_at_ms,sequence:peer.sequence,
          term:peer.term,verifiedExecution:false,
        });
      }
    }
  }
  events.sort((a,b)=>a.occurredAt-b.occurredAt||
    a.source.localeCompare(b.source)||
    (a.peerFingerprint??'').localeCompare(b.peerFingerprint??'')||
    a.sequence-b.sequence||a.node.localeCompare(b.node));
  return {
    graph:graph.id,receivedPeers:seen.size,freshPeers:fresh,
    stalePeers:stale,invalidPeerRecords:invalid,
    historicalPeerEvents:historical,legacySnapshotPeers:legacy,
    meteredPeerRuns:meteredUsage.length,meteredUsage,
    latestReportedTerm:latestTerm,events:events.slice(-160),
  };
}

/** One-shot, read-only fallback across operator-approved paired peers.
 * An uncertain delivery, timeout, lost network, cancellation, or rejected
 * task must never cause duplicate work. Only a receiver-confirmed FAILED
 * terminal message with a unique local outbound receipt permits one alternate.
 */
import {peersList,peersOutbound,peersRemoteThread,type PeerOutboundMessage} from './peersClient';
import {runPairedPeerAgent,remoteTaskCorrelation,type RemoteAgentTask} from './swarmPeer';
import {validSwarmPeer} from './swarmPeerSchema';
import {chooseSwarmPeer,loadPeerObservations} from './swarmPeerMetrics';

export const PEER_FAILOVER_SUFFIX = '.confirmed-failure-alt1';

export async function verifiedFailedPeerReceipt(task:RemoteAgentTask):Promise<string|null> {
 const pool=task.peerCandidates;
 if(!pool || pool.length<2 || pool.length>3 ||
    !pool.includes(task.peer) || new Set(pool).size!==pool.length ||
    !pool.every(validSwarmPeer) || task.signal?.aborted)return null;
 const correlation=remoteTaskCorrelation(task.correlationRunId,task.correlationTaskPosition,task.prompt);
 const peers=await peersList();
 const granted=peers.outbound.filter(peer=>pool.includes(peer.alias)&&peer.grants.includes('task'));
 const permitted=new Set(granted.map(peer=>peer.alias));
 let initial:PeerOutboundMessage|null=null;
 for(const alias of pool){
   if(task.signal?.aborted)return null;
   const snapshot=await peersOutbound(alias,200);
   // Saturated history cannot demonstrate that an earlier attempt is absent.
   if(snapshot.messages.length>=200)return null;
   const hits=snapshot.messages.filter(m=>m.kind==='task_request'&&m.correlation_id===correlation);
   if(hits.length>1)return null;
   if(hits.length){
     if(initial || !permitted.has(alias) || hits[0].alias!==alias)return null;
     initial=hits[0];
   }
 }
 if(!initial||task.signal?.aborted)return null;
 const refreshed=await peersRemoteThread(initial.alias,initial.thread_id);
 if(task.signal?.aborted)return null;
 const matching=refreshed.messages.filter(m=>m.kind==='task_request' &&
  m.correlation_id===correlation && m.message_id===initial!.message_id &&
  m.thread_id===initial!.thread_id);
 if(matching.length!==1||matching[0].state!=='failed')return null;
 const alternatives=pool.filter(alias=>alias!==initial!.alias);
 return chooseSwarmPeer(alternatives,granted,loadPeerObservations())?initial.alias:null;
}

export async function runPairedPeerAgentWithFailover(task:RemoteAgentTask):Promise<string> {
 const result=await runPairedPeerAgent(task);
 if(task.signal?.aborted||!result.startsWith('{"error":')||
    !task.peerCandidates||task.peerCandidates.length<2)return result;
 let failedPeer:string|null=null;
 try{failedPeer=await verifiedFailedPeerReceipt(task);}
 catch{return result;} // insufficient evidence: never resubmit
 if(!failedPeer||task.signal?.aborted)return result;
 const remaining=task.peerCandidates.filter(alias=>alias!==failedPeer);
 // Distinct deterministic correlation; the underlying runner checks all
 // authorized peers' durable receipts before sending on this new stage.
 return runPairedPeerAgent({...task,peer:remaining[0],peerCandidates:remaining,
  correlationTaskPosition:task.correlationTaskPosition+PEER_FAILOVER_SUFFIX,
  failoverFrom:failedPeer});
}

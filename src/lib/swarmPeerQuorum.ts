/** Identity-bound, non-authoritative reviewer quorum across paired nodes.
 * The pairing layer supplies certificate-pinned identities and native task
 * grants. Claims remain untrusted model opinions; there is no Raft/BFT log.
 * This module only ensures separate votes do not come from one peer under
 * multiple aliases or from a peer whose authorization changed mid-run.
 */
import type {OutboundPeer} from './peersClient';
import {peersList} from './peersClient';
export interface AuthenticatedVoter {
 alias:string;
 peerId:string;
 certificateSha256:string;
}
const SHA=/^[a-f0-9]{64}$/;
const ALIAS=/^[a-zA-Z0-9][a-zA-Z0-9_.-]{0,119}$/;
export function authenticatedPeerVoters(
 aliases:readonly string[],registered:readonly OutboundPeer[],
):AuthenticatedVoter[]{
 if(aliases.length<2||aliases.length>6||new Set(aliases).size!==aliases.length||
   !aliases.every(alias=>ALIAS.test(alias)))throw Error('Reviewer quorum requires 2–6 distinct paired aliases.');
 const fingerprints=new Set<string>(),devices=new Set<string>();
 return aliases.map(alias=>{
  const matches=registered.filter(peer=>peer.alias===alias);
  if(matches.length!==1)throw Error('Reviewer quorum cannot resolve a unique paired peer.');
  const peer=matches[0];
  if(!peer.grants.includes('task')||peer.presence==='offline'||
    !peer.peer_id||peer.peer_id.length>200||
    !SHA.test(peer.certificate_sha256)||
    fingerprints.has(peer.certificate_sha256)||devices.has(peer.peer_id))
    throw Error('Reviewer quorum peer not distinct, authorized or reachable.');
  fingerprints.add(peer.certificate_sha256);
  devices.add(peer.peer_id);
  return {alias,peerId:peer.peer_id,certificateSha256:peer.certificate_sha256};
 });
}
export async function frozenAuthenticatedPeerVoters(aliases:readonly string[]){
 const {outbound}=await peersList();
 return authenticatedPeerVoters(aliases,outbound);
}
export async function revalidateAuthenticatedPeerVoters(snapshot:readonly AuthenticatedVoter[]):Promise<void>{
 const fresh=await frozenAuthenticatedPeerVoters(snapshot.map(x=>x.alias));
 if(snapshot.length!==fresh.length||snapshot.some((x,i)=>
   x.alias!==fresh[i].alias||x.peerId!==fresh[i].peerId||
   x.certificateSha256!==fresh[i].certificateSha256))
   throw Error('Reviewer quorum lost an authenticated voter or certificate identity changed.');
}

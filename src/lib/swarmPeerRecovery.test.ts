import {beforeEach,describe,it,expect,vi} from 'vitest';
const mocks=vi.hoisted(()=>({
 peersList:vi.fn(),peersOutbound:vi.fn(),peersRemoteThread:vi.fn(),runPairedPeerAgent:vi.fn(),
}));
vi.mock('./peersClient',()=>({
 peersList:mocks.peersList,peersOutbound:mocks.peersOutbound,peersRemoteThread:mocks.peersRemoteThread,
}));
vi.mock('./swarmPeer',()=>({
 remoteTaskCorrelation:(run:string,pos:string,prompt:string)=>run+':'+pos+':'+prompt,
 runPairedPeerAgent:mocks.runPairedPeerAgent,
}));
import {runPairedPeerAgentWithFailover,PEER_FAILOVER_SUFFIX} from './swarmPeerRecovery';
const task={sessionId:'s',workflowRunId:'w',taskId:'w#p0a0',description:'review',
 prompt:'Read evidence',peer:'node-a',peerCandidates:['node-a','node-b'],
 correlationRunId:'w',correlationTaskPosition:'p0a0'};
const receipt={alias:'node-a',message_id:'m1',thread_id:'thread-a',kind:'task_request',
 correlation_id:'w:p0a0:Read evidence',state:'failed',result_text:'model errored',
 sent_at_ms:1,checked_at_ms:2};
const peer=(alias:string)=>({alias,grants:['task'],presence:'online'});
beforeEach(()=>{
 for(const fn of Object.values(mocks))fn.mockReset();
 mocks.peersList.mockResolvedValue({outbound:[peer('node-a'),peer('node-b')]});
 mocks.peersOutbound.mockImplementation(async(alias:string)=>({messages:alias==='node-a'?[receipt]:[]}));
 mocks.peersRemoteThread.mockResolvedValue({messages:[receipt]});
 mocks.runPairedPeerAgent.mockResolvedValueOnce('{"error":"primary failed"}')
  .mockResolvedValueOnce('Recovered report');
});
describe('bounded cross-peer recovery',()=>{
 it('launches exactly one alternate under a new deterministic correlation',async()=>{
  expect(await runPairedPeerAgentWithFailover(task)).toBe('Recovered report');
  expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(2);
  expect(mocks.runPairedPeerAgent.mock.calls[1][0]).toMatchObject({
   peer:'node-b',peerCandidates:['node-b'],prompt:task.prompt,
   correlationTaskPosition:'p0a0'+PEER_FAILOVER_SUFFIX,
  });
 });
 it('never migrates work based on network error or a nonterminal remote receipt',async()=>{
  mocks.peersRemoteThread.mockResolvedValue({messages:[{...receipt,state:'running'}]});
  expect(await runPairedPeerAgentWithFailover(task)).toContain('primary failed');
  expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(1);
 });
 it('never retries cancelled, rejected or unverified succeeded messages',async()=>{
  for(const state of ['cancelled','rejected','succeeded']){
   mocks.runPairedPeerAgent.mockReset().mockResolvedValueOnce('{"error":"failed"}');
   mocks.peersRemoteThread.mockResolvedValue({messages:[{...receipt,state}]});
   await runPairedPeerAgentWithFailover(task);
   expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(1);
  }
 });
 it('fails closed for lost, duplicated, full or revoked original receipt',async()=>{
  const scenarios=[
   ()=>mocks.peersOutbound.mockResolvedValue({messages:[]}),
   ()=>mocks.peersOutbound.mockResolvedValue({messages:[receipt,receipt]}),
   ()=>mocks.peersOutbound.mockResolvedValue({messages:Array(200).fill(receipt)}),
   ()=>mocks.peersList.mockResolvedValue({outbound:[peer('node-b')]}),
  ];
  for(const scenario of scenarios){
   mocks.runPairedPeerAgent.mockReset().mockResolvedValueOnce('{"error":"failed"}');
   mocks.peersList.mockResolvedValue({outbound:[peer('node-a'),peer('node-b')]});
   mocks.peersOutbound.mockImplementation(async(alias:string)=>({messages:alias==='node-a'?[receipt]:[]}));
   scenario();
   await runPairedPeerAgentWithFailover(task);
   expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(1);
  }
 });
 it('respects offline alternatives and cancellation',async()=>{
  mocks.peersList.mockResolvedValue({outbound:[peer('node-a'),{...peer('node-b'),presence:'offline'}]});
  await runPairedPeerAgentWithFailover(task);
  expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(1);
  const controller=new AbortController();controller.abort();
  mocks.runPairedPeerAgent.mockReset().mockResolvedValueOnce('{"error":"aborted"}');
  await runPairedPeerAgentWithFailover({...task,signal:controller.signal});
  expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(1);
 });
 it('never retries failed alternates recursively',async()=>{
  mocks.runPairedPeerAgent.mockReset().mockResolvedValueOnce('{"error":"failed"}')
   .mockResolvedValueOnce('{"error":"alternate also failed"}');
  expect(await runPairedPeerAgentWithFailover(task)).toContain('alternate also failed');
  expect(mocks.runPairedPeerAgent).toHaveBeenCalledTimes(2);
 });
});

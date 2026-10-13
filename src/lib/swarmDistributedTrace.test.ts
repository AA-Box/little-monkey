import {describe,expect,it} from 'vitest';
import type {ResidentSwarmGraphStatus,DaemonSwarmReplica} from './daemonClient';
import {composeDistributedSwarmTrace} from './swarmDistributedTrace';
const NOW=1_800_000_000_000;
const graph:ResidentSwarmGraphStatus={
 id:'audit-graph',state:'running',created_at_ms:NOW-6000,updated_at_ms:NOW-1000,
 tasks:[{id:'audit',state:'submitted',job_id:'job-1',error:null}],
 events:[
  {sequence:1,node_id:null,kind:'graph_created',previous_state:null,
    state:'running',job_id:null,occurred_at_ms:NOW-6000},
  {sequence:2,node_id:'audit',kind:'node_transition',previous_state:'pending',
    state:'submitted',job_id:'job-1',occurred_at_ms:NOW-3000},
 ],
};
const peer:DaemonSwarmReplica={
 version:1,group:'research',graph_id:graph.id,
 source_fingerprint:'a'.repeat(64),roster_sha256:'b'.repeat(64),
 sequence:4,term:2,captured_at_ms:NOW-2000,graph_state:'running',
 nodes:[{id:'audit',state:'succeeded',job_id:'job-peer'}],
 checksum:'c'.repeat(64),
};
describe('unified durable/swarm peer trace',()=>{
 it('does not upgrade remote succeeded states to verified execution',()=>{
  const result=composeDistributedSwarmTrace(graph,[peer],NOW);
  expect(result.receivedPeers).toBe(1);
  expect(result.freshPeers).toBe(1);
  expect(result.latestReportedTerm).toBe(2);
  expect(result.events).toHaveLength(4);
  expect(result.events.every(x=>x.verifiedExecution===false)).toBe(true);
  expect(result.events.find(x=>x.source==='signed-peer-observation'&&x.node==='audit')?.state).toBe('succeeded');
  expect(result.events[0]?.source).toBe('local-ledger');
 });
 it('flags stale, forged and duplicate member observations without exposing remote authority',()=>{
  const stale={...peer,captured_at_ms:NOW-200_000};
  const forged={...peer,source_fingerprint:'not-a-certificate'};
  const duplicate={...peer,sequence:5};
  const result=composeDistributedSwarmTrace(graph,[stale,forged,duplicate],NOW);
  expect(result.receivedPeers).toBe(1);
  expect(result.stalePeers).toBe(1);
  expect(result.invalidPeerRecords).toBe(2);
 });
 it('accepts empty peer observations without hiding the local journal',()=>{
  const result=composeDistributedSwarmTrace(graph,[],NOW);
  expect(result.events).toHaveLength(2);
  expect(result.latestReportedTerm).toBeNull();
  expect(result.freshPeers).toBe(0);
 });
 it('does not accept future remote timestamps as fresh execution evidence',()=>{
  const result=composeDistributedSwarmTrace(graph,[{...peer,captured_at_ms:NOW+100_000}],NOW);
  expect(result.receivedPeers).toBe(0);
  expect(result.invalidPeerRecords).toBe(1);
 });
 it('renders original chronological signed peer events rather than inventing completion timestamps',()=>{
  const remote={...peer,version:2,trace_events:[
   {sequence:18,node_id:null,kind:'graph_created',state:'running',occurred_at_ms:NOW-9000},
   {sequence:23,node_id:'audit',kind:'node_transition',state:'succeeded',occurred_at_ms:NOW-4000},
  ]};
  const result=composeDistributedSwarmTrace(graph,[remote],NOW);
  expect(result.historicalPeerEvents).toBe(2);
  expect(result.legacySnapshotPeers).toBe(0);
  expect(result.events.find(x=>x.source==='signed-peer-observation' && x.node==='audit'))
   .toMatchObject({kind:'node_transition',occurredAt:NOW-4000,sequence:23,verifiedExecution:false});
 });
 it('accepts only valid source-run metering in authenticated v3 peer checkpoints',()=>{
  const usage={node_id:'audit',target_id:'provider:qualified-model',
    elapsed_ms:2500,input_tokens:100,output_tokens:50,cached_input_tokens:20,
    model_calls:2,cost_micros:15};
  const v3={...peer,version:3,model_usage:[usage]};
  const result=composeDistributedSwarmTrace(graph,[v3],NOW);
  expect(result.meteredPeerRuns).toBe(1);
  expect(result.meteredUsage[0]).toMatchObject({
    targetId:'provider:qualified-model',inputTokens:100,outputTokens:50,
    meteredCostMicros:15,providerBilled:false,independentlyQualityVerified:false,
  });
  for(const wrong of [
    {...usage,node_id:'unrelated'},
    {...usage,model_calls:0},
    {...usage,input_tokens:-1},
    {...usage,cached_input_tokens:101},
    {...usage,cost_micros:-1},
    {...usage,target_id:'host/private secret'},
  ]) {
    expect(composeDistributedSwarmTrace(graph,[{...v3,model_usage:[wrong]}],NOW)
      .invalidPeerRecords).toBe(1);
  }
  expect(composeDistributedSwarmTrace(graph,[{...peer,model_usage:[usage]}],NOW)
    .invalidPeerRecords).toBe(1);
 });
 it('fails closed on malformed remote journal, regressions, unrelated node and invalid checksum',()=>{
  const first={sequence:12,node_id:'audit',kind:'node_transition',state:'submitted',occurred_at_ms:NOW-3000};
  for(const trace_events of [
    [first,{...first,sequence:11}],
    [{...first,node_id:'other'}],
    [{...first,kind:'unknown'}],
    [{...first,occurred_at_ms:NOW+10_000}],
  ]){
   const result=composeDistributedSwarmTrace(graph,[{...peer,version:2,trace_events}],NOW);
   expect(result.invalidPeerRecords).toBe(1);
   expect(result.historicalPeerEvents).toBe(0);
  }
  expect(composeDistributedSwarmTrace(graph,[{...peer,checksum:'not-hash'}],NOW).invalidPeerRecords).toBe(1);
 });

});

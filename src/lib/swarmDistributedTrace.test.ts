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
});

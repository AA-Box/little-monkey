import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {verifyRemoteJournal} from './swarm-cross-host-telemetry-live-acceptance.mjs';
const fp='a'.repeat(64);
const event=(sequence,node_id,kind,state,occurred_at_ms)=>({
 sequence,node_id,kind,state,occurred_at_ms,
});
const sample=()=>({group:'grp',graph:'dag',replicas:[{
 version:2,group:'grp',graph_id:'dag',source_fingerprint:fp,
 roster_sha256:'b'.repeat(64),checksum:'c'.repeat(64),
 sequence:5,term:2,captured_at_ms:100000,
 nodes:[{id:'n',state:'succeeded'}],trace_events:[
  event(2,null,'graph_created','running',96000),
  event(3,'n','node_transition','succeeded',99000),
 ],
}]});
const input={group:'grp',graph:'dag',sourceFingerprint:fp,now:100000};
test('receiver verifies source identity, ordered trace and exact graph',()=>{
 const result=verifyRemoteJournal(sample(),input);
 assert.equal(result.status,'verified_on_independent_receiver');
 assert.equal(result.traceEvents,2);
 assert.equal(result.executedModelQualityVerified,false);
});
test('no model-based claims from a snapshot-only peer',()=>{
 const v=sample();v.replicas[0].version=1;v.replicas[0].trace_events=[];
 assert.throws(()=>verifyRemoteJournal(v,input),/legacy-only/);
});
test('rejects tampered, stale, unrelated, and replayed trace observations',()=>{
 for(const mutate of [
  s=>{s.replicas[0].trace_events[1].sequence=1;},
  s=>{s.replicas[0].trace_events[1].occurred_at_ms=200000;},
  s=>{s.replicas[0].trace_events[1].node_id='other';},
  s=>{s.replicas[0].trace_events[1].kind='unknown';},
  s=>{s.replicas[0].checksum='forged';},
  s=>{s.replicas[0].source_fingerprint='b'.repeat(64);},
  s=>{s.replicas[0].captured_at_ms=1;},
 ]){
  const v=sample();mutate(v);
  assert.throws(()=>verifyRemoteJournal(v,input));
 }
});
test('v3 independent receiver requires real metered usage for complete cross-host verification',()=>{
 const v=sample();
 const usage={node_id:'n',target_id:'provider:model',
   elapsed_ms:1400,input_tokens:700,output_tokens:100,
   cached_input_tokens:200,model_calls:1,cost_micros:250};
 assert.throws(()=>verifyRemoteJournal(v,{...input,requireModelUsage:true}),/metering/);
 v.replicas[0].version=3;v.replicas[0].model_usage=[usage];
 const accepted=verifyRemoteJournal(v,{...input,requireModelUsage:true});
 assert.equal(accepted.measuredPeerRuns,1);
 assert.equal(accepted.observedInputTokens,700);
 assert.equal(accepted.observedOutputTokens,100);
 assert.equal(accepted.observedCostMicros,250);
 assert.equal(accepted.providerBillingReconciled,false);
 for(const bad of [
   {...usage,node_id:'outsider'}, {...usage,model_calls:0},
   {...usage,output_tokens:-1}, {...usage,cached_input_tokens:800},
   {...usage,cost_micros:-1},
 ]){
   const corrupt=structuredClone(v);corrupt.replicas[0].model_usage=[bad];
   assert.throws(()=>verifyRemoteJournal(corrupt,{...input,requireModelUsage:true}));
 }
 const forgedVersion=structuredClone(v);forgedVersion.replicas[0].version=2;
 assert.throws(()=>verifyRemoteJournal(forgedVersion,input),/metering/);
});
test('live mode requires actual operator-confirmed devices and installed daemon',()=>{
 const run=spawnSync(process.execPath,
  ['scripts/swarm-cross-host-telemetry-live-acceptance.mjs',
   '--stage','verify','--group','grp','--graph','dag',
   '--source-fingerprint',fp],
  {encoding:'utf8'});
 assert.notEqual(run.status,0);
 assert.match(run.stderr,/not_verified/);
});

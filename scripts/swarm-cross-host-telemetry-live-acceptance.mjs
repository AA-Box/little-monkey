#!/usr/bin/env node
/** Strict, operator-run physical two-host DAG telemetry acceptance.
 *
 * Stage publish runs on real leader A. Stage verify MUST run on independent
 * receiver B, inspecting B's own durable SQLite records through its CLI.
 * No fixture is accepted as live proof and no peer is provisioned implicitly.
 */
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';
import {resolve} from 'node:path';
const option=(name)=>{const i=process.argv.indexOf(name);return i===-1?null:process.argv[i+1]??null;};
const ID=/^[A-Za-z0-9][A-Za-z0-9_.-]{0,79}$/;
const FP=/^[0-9a-f]{64}$/;
const KINDS=new Set(['graph_created','graph_transition','node_created','node_transition']);
const STATES=new Set(['running','failing','cancelling','succeeded','failed','cancelled',
 'pending','pending_fallback','preparing','preparing_fallback','submitted','submitted_fallback']);
const fail=(reason)=>{throw Error(reason);};
export function verifyRemoteJournal(view,{group,graph,sourceFingerprint,now=Date.now(),minimumEvents=2}){
 if(view?.group!==group||view?.graph!==graph||
    !Array.isArray(view.replicas)||view.replicas.length<1)fail('Remote receiver lacks a durable cluster checkpoint');
 const rows=view.replicas.filter(v=>v.source_fingerprint===sourceFingerprint);
 if(rows.length!==1)fail('Expected exactly one checkpoint with the enrolled source certificate identity');
 const snapshot=rows[0];
 if(snapshot.version!==2||snapshot.group!==group||snapshot.graph_id!==graph||
    !FP.test(snapshot.checksum??'')||!FP.test(snapshot.roster_sha256??'')||
    !Number.isSafeInteger(snapshot.sequence)||snapshot.sequence<1||
    !Number.isSafeInteger(snapshot.term)||snapshot.term<1||
    !Number.isSafeInteger(snapshot.captured_at_ms)||snapshot.captured_at_ms>now+5000||
    now-snapshot.captured_at_ms>120000||
    !Array.isArray(snapshot.nodes)||!Array.isArray(snapshot.trace_events)||
    snapshot.trace_events.length<minimumEvents||snapshot.trace_events.length>48)
   fail('Remote checkpoint is stale, malformed, legacy-only or lacks real journal events');
 const nodeIds=new Set(snapshot.nodes.map(n=>n.id));
 let last=0;
 for(const event of snapshot.trace_events){
  if(!Number.isSafeInteger(event.sequence)||event.sequence<=last||
     !KINDS.has(event.kind)||!STATES.has(event.state)||
     !Number.isSafeInteger(event.occurred_at_ms)||event.occurred_at_ms<1||
     event.occurred_at_ms>snapshot.captured_at_ms+5000||
     (event.node_id!==null&&!nodeIds.has(event.node_id)))
   fail('Invalid, out-of-order or unrelated remote durable event');
  last=event.sequence;
 }
 if(!snapshot.trace_events.some(e=>e.kind==='node_transition'))
   fail('Expected at least one REAL node transition from the original source host');
 return {status:'verified_on_independent_receiver',group,graph,
   sourceFingerprint,checkpointSequence:snapshot.sequence,term:snapshot.term,
   traceEvents:snapshot.trace_events.length,
   earliestEventMs:snapshot.trace_events[0].occurred_at_ms,
   latestEventMs:snapshot.trace_events.at(-1).occurred_at_ms,
   executedModelQualityVerified:false,
   note:'The receiving host durably observed DAG states. No model output or billing was verified.'};
}
function native(args){
 const r=spawnSync(option('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey',args,{
  encoding:'utf8',timeout:45000,maxBuffer:256*1024,env:process.env});
 if(r.error||r.status!==0||r.signal)fail('Installed daemon CLI failed: '+args.slice(0,2).join(' ')+
  ' ('+(r.error?.message??r.status)+')');
 try{return JSON.parse(r.stdout.trim());}catch{fail('Installed CLI did not emit a JSON receipt');}
}
function run(){
 const stage=option('--stage'),group=option('--group'),graph=option('--graph');
 const peer=option('--peer'),fp=option('--source-fingerprint');
 if(!['publish','verify'].includes(stage)||!ID.test(group??'')||
    !ID.test(graph??'')||!process.argv.includes('--confirm-independent-installed-peers'))
  fail('Require --stage publish|verify --group GROUP --graph GRAPH --confirm-independent-installed-peers');
 if(stage==='publish'){
   if(!ID.test(peer??''))fail('Publish needs --peer ENROLLED_ALIAS on source A');
   const response=native(['daemon','swarm-replicate',group,graph,peer]);
   if(response.remote_readback_verified!==true||response.execution_authorized!==false||
      response.receipt?.accepted!==true||response.receipt?.group!==group||
      response.receipt?.graph_id!==graph)
     fail('Source did not verify its entire remote SQLite checkpoint via signed readback');
   return {status:'verified_sender_signed_readback',group,graph,peer,
     checkpointSequence:response.receipt.sequence,
     next:'Run verify on physically separate receiver B with source A certificate fingerprint'};
 }
 if(!FP.test(fp??''))fail('Verify requires the source A enrolled --source-fingerprint');
 return verifyRemoteJournal(native(['daemon','swarm-replica-status',group,graph]),
  {group,graph,sourceFingerprint:fp});
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 try{
  if(process.argv.includes('--self-check')){
   const fp='a'.repeat(64);
   const sample={group:'grp',graph:'dag',replicas:[{
    version:2,group:'grp',graph_id:'dag',source_fingerprint:fp,
    roster_sha256:'b'.repeat(64),checksum:'c'.repeat(64),
    sequence:4,term:2,captured_at_ms:10000,
    nodes:[{id:'audit',state:'succeeded'}],trace_events:[
      {sequence:1,node_id:null,kind:'graph_created',state:'running',occurred_at_ms:8000},
      {sequence:2,node_id:'audit',kind:'node_transition',state:'succeeded',occurred_at_ms:9000},
    ],
   }]};
   const valid=verifyRemoteJournal(sample,{group:'grp',graph:'dag',
      sourceFingerprint:fp,now:10000});
   if(valid.traceEvents!==2)fail('Contract self-check failed');
   for(const mutate of [
     s=>{s.replicas[0].trace_events.reverse();},
     s=>{s.replicas[0].trace_events[1].node_id='unrelated';},
     s=>{s.replicas[0].version=1;},
     s=>{s.replicas[0].captured_at_ms=0;},
   ]){
     const forged=structuredClone(sample);mutate(forged);
     let rejected=false;
     try{verifyRemoteJournal(forged,{group:'grp',graph:'dag',sourceFingerprint:fp,now:10000});}
     catch{rejected=true;}
     if(!rejected)fail('Corrupt remote event was accepted');
   }
   console.log(JSON.stringify({status:'contract_passed',liveDevicesRun:false}));
  }else console.log(JSON.stringify(run()));
 }catch(error){
   console.error(JSON.stringify({status:'not_verified',error:String(error).slice(0,400)}));
   process.exitCode=1;
 }
}

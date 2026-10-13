#!/usr/bin/env node
/**
 * REAL operator-operated two-installation DAG replica acceptance.
 *
 * Stage 1: healthy pinned-TLS peer and persistent checkpoint readback.
 * Stage 2: explicitly isolated peer MUST fail a fresh signed network probe.
 * Stage 3: restored peer MUST pass a fresh signed probe and persist a NEW,
 *          higher sequence checkpoint with the exact known source identity.
 *
 * Only a pre-enrolled read-only graph and cluster are used. Never manages
 * firewall rules, stops services, touches approvals, or impersonates a peer.
 * A CLI failure alone is not proof of a physical partition: stage 2 also
 * requires the operator's independent attestation of network isolation.
 */
import {spawnSync} from 'node:child_process';
import {readFileSync,writeFileSync,openSync,closeSync,fsyncSync,renameSync,unlinkSync,lstatSync} from 'node:fs';
import {resolve,isAbsolute,dirname,basename,join} from 'node:path';
import {createHash,randomUUID} from 'node:crypto';

const arg=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null;};
const stage=arg('--stage'),group=arg('--group'),graph=arg('--graph'),peer=arg('--peer');
const evidence=arg('--evidence'),bin=arg('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
const approved=process.argv.includes('--confirm-operator-owned-readonly-cluster');
const isolated=process.argv.includes('--confirm-network-isolated-externally');
const restored=process.argv.includes('--confirm-network-restored-externally');
const id=/^[A-Za-z0-9][A-Za-z0-9_.-]{0,79}$/;
const fingerprint=/^[0-9a-f]{64}$/;
function fail(message){throw Error(message);}
function validateInput(){
  if(!['baseline','partition','restore'].includes(stage)||
     !id.test(group??'')||!id.test(graph??'')||!id.test(peer??'')||
     !approved||!isAbsolute(evidence??'')||/[\x00-\x1f]/.test(evidence)||
     (stage==='partition'&&!isolated)||(stage==='restore'&&!restored))
    fail('Usage: --stage baseline|partition|restore --group GROUP --graph GRAPH --peer ENROLLED_ALIAS --evidence /absolute/evidence.json --confirm-operator-owned-readonly-cluster [--confirm-network-isolated-externally | --confirm-network-restored-externally] [--monkey-bin PATH]');
}
function native(args,limit=30_000){
  const run=spawnSync(bin,args,{encoding:'utf8',timeout:limit,
    maxBuffer:256*1024,env:process.env});
  if(run.error||run.signal||run.status!==0)throw Error('Native signed peer command failed: '+args.slice(0,2).join(' '));
  let payload;
  try{payload=JSON.parse(run.stdout.trim());}catch{fail('Native CLI did not produce a JSON receipt');}
  return payload;
}
function probe(){
  const result=native(['peers','status',peer,'--json']);
  if(!result||typeof result!=='object')fail('Signed peer status probe gave an invalid receipt');
  return result;
}
function publish(){
  const value=native(['daemon','swarm-replicate',group,graph,peer],60_000);
  if(value.remote_readback_verified!==true||
     value.execution_authorized!==false||
     value.receipt?.accepted!==true||
     value.receipt?.group!==group||
     value.receipt?.graph_id!==graph||
     !Number.isSafeInteger(value.receipt.sequence)||value.receipt.sequence<1){
    fail('Remote SQLite readback did not attest this exact DAG checkpoint');
  }
  return value.receipt.sequence;
}
function payload(state,fields){
  return {version:1,kind:'native-two-device-dag-replication',
    group,graph,peer,...fields,state};
}
function fingerprintOf(v){
  return createHash('sha256').update(JSON.stringify(v)).digest('hex');
}
function save(data,isNew){
  const bytes=JSON.stringify(data,null,2)+'\n';
  const temp=join(dirname(evidence),'.'+basename(evidence)+'.'+randomUUID()+'.tmp');
  let fd;
  try{
    fd=openSync(temp,'wx',0o600);
    writeFileSync(fd,bytes);fsyncSync(fd);closeSync(fd);fd=undefined;
    if(isNew){
      const reserve=openSync(evidence,'wx',0o600);closeSync(reserve);
    }
    renameSync(temp,evidence);
  }finally{
    if(fd!==undefined)closeSync(fd);
    try{unlinkSync(temp);}catch{}
  }
}
function load(){
  const info=lstatSync(evidence);
  if(!info.isFile()||info.isSymbolicLink()||info.size>16*1024)
    fail('Evidence must be a bounded, regular, non-symlink local file');
  const value=JSON.parse(readFileSync(evidence,'utf8'));
  if(value?.version!==1||value.kind!=='native-two-device-dag-replication'||
     value.group!==group||value.graph!==graph||value.peer!==peer||
     !fingerprint.test(value.source_fingerprint)||
     !Number.isSafeInteger(value.baseline_sequence)||
     value.baseline_sequence<1||
     !fingerprint.test(value.checksum)||
     value.checksum!==fingerprintOf({
        source_fingerprint:value.source_fingerprint,
        baseline_sequence:value.baseline_sequence,
        group:value.group,graph:value.graph,peer:value.peer
     }))fail('Evidence identity, fields or checksum mismatched');
  return value;
}
function cluster(){
  const state=native(['peers','cluster-status',group]);
  if(state.group!==group||state.may_schedule!==true||
     !fingerprint.test(state.local_fingerprint??'')||
     !fingerprint.test(state.roster_sha256??''))
    fail('This installation does not currently have a signed majority lease');
  return state;
}
function run(){
  validateInput();
  if(stage==='baseline'){
    const c=cluster();
    probe();
    const sequence=publish();
    const proof={
      source_fingerprint:c.local_fingerprint,baseline_sequence:sequence,
      group,graph,peer
    };
    const checkpoint=payload('baseline_confirmed',{
      source_fingerprint:c.local_fingerprint,baseline_sequence:sequence,
      checksum:fingerprintOf(proof),
      createdAtMs:Date.now(),partitionSeen:false,
    });
    save(checkpoint,true);
    return {stage,status:'verified_remote_sqlite_baseline',
      baselineSequence:sequence,readback:true,
      note:'Operator must independently isolate the physical peer before partition stage.'};
  }
  const existing=load();
  if(stage==='partition'){
    if(existing.state!=='baseline_confirmed'||existing.partitionSeen)
      fail('Partition stage requires one unconsumed baseline');
    let reachable=true;
    try{probe();}catch{reachable=false;}
    if(reachable)fail('Signed peer is STILL reachable; no network partition observed');
    const next={...existing,state:'operator_partition_unreachable',
      partitionSeen:true,partitionObservedAtMs:Date.now()};
    save(next,false);
    return {stage,status:'unreachable_during_operator_declared_partition',
      baselineSequence:existing.baseline_sequence,
      caveat:'Unreachable could also mean service failure/revocation; physical network isolation is operator-attested, not detected by this CLI.'};
  }
  if(existing.state!=='operator_partition_unreachable'||!existing.partitionSeen)
    fail('Recovery stage requires independently recorded baseline and unreachable probe');
  const c=cluster();
  if(c.local_fingerprint!==existing.source_fingerprint)
    fail('The restored publication is not coming from the same enrolled source identity');
  probe();
  const sequence=publish();
  if(sequence<=existing.baseline_sequence)
    fail('Restored transport did not persist a strictly newer DAG revision');
  save({...existing,state:'recovered',restoredAtMs:Date.now(),
    restoredSequence:sequence},false);
  return {stage,status:'verified_replicated_checkpoint_after_reconnect',
    baselineSequence:existing.baseline_sequence,restoredSequence:sequence,
    operatorNetworkAttestation:true,
    note:'This confirms signed availability and durable checkpoint recovery, NOT exactly-once process migration or actual task-output verification.'};
}
if(process.argv.includes('--self-check')){
  const valid= id.test('cluster-1') && !id.test('../evil') &&
    fingerprint.test('a'.repeat(64)) && !fingerprint.test('x'.repeat(64));
  if(!valid)fail('CLI acceptance input guards failed');
  console.log(JSON.stringify({status:'contract_passed',mode:'no_network_or_models'}));
}else{
  try{console.log(JSON.stringify(run()));}
  catch(error){console.error(JSON.stringify({status:'not_verified',stage,
    error:String(error).slice(0,300)}));process.exitCode=1;}
}

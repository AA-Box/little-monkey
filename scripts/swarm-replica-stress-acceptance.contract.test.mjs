import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,chmodSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join,dirname,resolve} from 'node:path';
import {spawnSync} from 'node:child_process';
import {fileURLToPath} from 'node:url';

const script=resolve(dirname(fileURLToPath(import.meta.url)),'swarm-replica-stress-live-acceptance.mjs');
const common=['--group','test-group','--graph','graph-test','--peer','peer-b',
  '--rounds','10','--concurrency','3',
  '--confirm-operator-owned-readonly-cluster','--confirm-independent-machines',
  '--confirm-post-recovery'];
function fake(mode='normal'){
  const root=mkdtempSync(join(tmpdir(),'swarm-stress-contract-'));
  const bin=join(root,'monkey.mjs');
  writeFileSync(bin,`#!/usr/bin/env node
const args=process.argv.slice(2), mode=process.env.FAKE_MODE;
const result = args[0]==='peers'&&args[1]==='cluster-status'?
 {group:'test-group',may_schedule:true,local_fingerprint:'a'.repeat(64),roster_sha256:'b'.repeat(64)}:
 args[0]==='peers'&&args[1]==='status'?
 {alias:'peer-b',presence:'online',peer_id:'distinct-b',granted:['task']}:
 args[0]==='daemon'&&args[1]==='swarm-replicate'?
 {remote_readback_verified:mode!=='missing-readback',execution_authorized:false,
 receipt:{accepted:true,group:'test-group',graph_id:'graph-test',
 sequence:mode==='duplicate'?1:process.pid}}:null;
if(!result)process.exit(1);
console.log(JSON.stringify(result));
`);
  chmodSync(bin,0o700);
  const invoke=(additional=[],timeout=30_000)=>spawnSync(process.execPath,[script,...common,'--monkey-bin',bin,...additional],{
    env:{...process.env,FAKE_MODE:mode},encoding:'utf8',timeout
  });
  return {invoke,dispose:()=>rmSync(root,{recursive:true,force:true})};
}
test('refuses to run without explicit physical operator and post-recovery confirmations',()=>{
  const r=spawnSync(process.execPath,[script,'--group','test-group'],{encoding:'utf8'});
  assert.equal(r.status,2);
  assert.match(r.stderr,/confirm-independent-machines/);
});
test('rejects duplicate, zero, negative or too-large flags before spawning CLI',()=>{
  for(const extra of [['--rounds','1'],['--concurrency','9'],['--concurrency','0'],
    ['--rounds','5000'],['--group','../escape']]){
    const f=fake();try{assert.equal(f.invoke(extra).status,2);}finally{f.dispose();}
  }
});
test('accepts only all matching signed remote readbacks with unique sequences',()=>{
  const f=fake();
  try{
    const result=f.invoke();
    assert.equal(result.status,0,result.stderr);
    const record=JSON.parse(result.stdout);
    assert.equal(record.status,'verified_live_cli');
    assert.equal(record.uniqueSequences,10);
    assert.equal(record.failedPublications,0);
    assert.equal(record.independentHardwareAttestation,false);
  }finally{f.dispose();}
});
test('rejects a fabricated success missing a remote readback',()=>{
  const f=fake('missing-readback');
  try{
    const r=f.invoke();assert.equal(r.status,1,r.stdout);
    assert.match(JSON.parse(r.stderr).error,/matching, durable/);
  }finally{f.dispose();}
});
test('detects duplicate durable checkpoint sequence under concurrent load',()=>{
  const f=fake('duplicate');
  try{
    const r=f.invoke();assert.equal(r.status,1,r.stdout);
    assert.match(JSON.parse(r.stderr).error,/Lost or duplicated/);
  }finally{f.dispose();}
});

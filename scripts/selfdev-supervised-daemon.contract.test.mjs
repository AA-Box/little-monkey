import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,chmodSync,readFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
const rootScript=resolve('scripts/selfdev-transaction.mjs');
const supervisor=resolve('scripts/selfdev-supervised-daemon.mjs');
function call(script,...args){
 const r=spawnSync(process.execPath,[script,...args],{
   encoding:'utf8',timeout:30000,maxBuffer:256000,
 });
 let body;try{body=JSON.parse(r.stdout);}catch{}
 return{ok:r.status===0,stderr:r.stderr,body};
}
function fake(root,label,pid,broken=false) {
 const source=join(root,'candidate-'+label);
 const content=`#!/usr/bin/env node
const fs=require('fs'),path=require('path');
const args=process.argv.slice(2),home=args[args.indexOf('--agent-home')+1];
if(args.includes('--version')){console.log('Little Monkey CLI ${label}');process.exit(0)}
if(args.includes('profiles')&&args.includes('create')){
 const id=args[args.indexOf('create')+1];console.log(JSON.stringify({id}));process.exit(0);
}
const state=path.join(home,'test-daemon-installed.json');
if(args.includes('daemon')&&args.includes('status')){
 const value=fs.existsSync(state)?JSON.parse(fs.readFileSync(state,'utf8')):null;
 console.log(JSON.stringify({installed:!!value,service_running:!!value,
 heartbeat_fresh:!!value,pid:value?.pid??null,
 active:value?.active??0,queued:value?.queued??0,paused:0,waiting_approval:0,\n maintenance_drain:value?.drain??false,managed_run_ids:[]
 ${label.startsWith('LIVE')?',handoff_candidates:value?.handoff_candidates??0,handoff_attested:value?.handoff_attested??0,handoff_worker_ids:value?.handoff_worker_ids??[],handoff_adopted_count:0,handoff_adopted_daemon_pid:null':''}}));
 process.exit(0);
}
if(args.includes('daemon')&&args.includes('install')){
 if(${broken})process.exit(7);
 const prior=fs.existsSync(state)?JSON.parse(fs.readFileSync(state,'utf8')):{};
 fs.writeFileSync(state,JSON.stringify({pid:${pid},active:0,
  queued:prior.queued??2,drain:prior.drain??false}));process.exit(0);
}
if(args.includes('daemon')&&args.includes('drain')){
 const old=JSON.parse(fs.readFileSync(state,'utf8'));
 old.drain=args.includes('--enable');
 fs.writeFileSync(state,JSON.stringify(old));process.exit(0);
}
if(args.includes('daemon')&&args.includes('stop')){process.exit(0)}
if(args.includes('daemon')&&args.includes('uninstall')){
 fs.rmSync(state,{force:true});process.exit(0);
}
process.exit(5);
// filler source deliberately long enough to satisfy native artifact test
`;
 writeFileSync(source,content,{mode:0o700});chmodSync(source,0o700);
 return source;
}
test('Linux live handoff requires explicit prior enrollment and a proven executing worker',{
 skip:process.platform!=='linux',
},()=>{
 const temp=mkdtempSync(join(tmpdir(),'lm-hot-supervisor-'));
 const root=join(temp,'managed');
 try{
  const source=fake(temp,'LIVE-A',2111),target=fake(temp,'LIVE-B',2112);
  let out=call(rootScript,'stage','--candidate',source,'--root',root);
  assert.equal(out.ok,true,out.stderr);
  const shaA=out.body.sha;
  out=call(rootScript,'activate','--root',root,'--confirm-sha',shaA);
  assert.equal(out.ok,true,out.stderr);
  out=call(rootScript,'stage','--candidate',target,'--root',root);
  assert.equal(out.ok,true,out.stderr);
  const shaB=out.body.sha;
  out=call(supervisor,'enroll','--root',root,'--confirm-sha',shaA,
    '--live-handoff');
  assert.equal(out.ok,true,out.stderr);
  const manifest=JSON.parse(readFileSync(join(root,'.supervised-daemon.json'),'utf8'));
  assert.equal(manifest.liveHandoff,true);
  out=call(supervisor,'upgrade-live','--root',root,'--confirm-sha',shaB);
  assert.equal(out.ok,false,'No real attested worker = NEVER pretend a live transfer');
  assert.match(out.stderr,/verified original PID\/birth attestation/);
  assert.equal(call(rootScript,'status','--root',root).body.active,shaA);
 }finally{rmSync(temp,{recursive:true,force:true});}
});
test('supervisor enrolls isolated identity, restarts cold, and rolls back failed candidate',{
 skip:process.platform==='win32',
},()=>{
 const temp=mkdtempSync(join(tmpdir(),'lm-supervisor-contract-'));
 const root=join(temp,'managed');
 try{
  const a=fake(temp,'A',1201),b=fake(temp,'B',1202),bad=fake(temp,'BAD',1203,true);
  const stage=(candidate)=>{
   const r=call(rootScript,'stage','--candidate',candidate,'--root',root);
   assert.equal(r.ok,true,r.stderr);return r.body.sha;
  };
  const shaA=stage(a);
  let r=call(rootScript,'activate','--root',root,'--confirm-sha',shaA);
  assert.equal(r.ok,true,r.stderr);
  const shaB=stage(b),shaBad=stage(bad);
  r=call(supervisor,'enroll','--root',root,'--confirm-sha',shaA);
  assert.equal(r.ok,true,r.stderr);
  assert.match(r.body.profile,/^selfdev-[a-f0-9]{16}$/);
  assert.equal(r.body.servicePid,1201);
  // A timeout while a model job is active must NEVER stop or reinstall the
  // old service. This tests the load-bearing no-loss failure path.
  const daemonState=join(root,'supervised-agent-home','test-daemon-installed.json');
  const running=JSON.parse(readFileSync(daemonState,'utf8'));
  writeFileSync(daemonState,JSON.stringify({...running,active:1}));
  process.env.LM_SELFDEV_DRAIN_TIMEOUT_MS='120';
  try {
    r=call(supervisor,'upgrade','--root',root,'--confirm-sha',shaB);
    assert.equal(r.ok,false);
    assert.match(r.stderr,/ABORTED WITHOUT RESTART/);
    assert.match(r.stderr,/Active tasks did not finish/);
    const untouched=JSON.parse(readFileSync(daemonState,'utf8'));
    assert.equal(untouched.pid,1201);
    assert.equal(untouched.active,1);
    assert.equal(untouched.drain,false);
    assert.equal(call(rootScript,'status','--root',root).body.active,shaA);
  } finally {delete process.env.LM_SELFDEV_DRAIN_TIMEOUT_MS;}
  writeFileSync(daemonState,JSON.stringify({...running,active:0,drain:false}));
  r=call(supervisor,'upgrade','--root',root,'--confirm-sha',shaB);
  assert.equal(r.ok,true,r.stderr);
  assert.equal(r.body.servicePid,1202);
  assert.equal(r.body.liveSessionHandoff,false);
  assert.equal(r.body.queuedSessionsPreserved,2);
  r=call(supervisor,'upgrade','--root',root,'--confirm-sha',shaBad);
  assert.equal(r.ok,false,'failed candidate must not produce success artifact');
  assert.match(r.stderr,/ROLLED BACK/);
  r=call(supervisor,'status','--root',root);
  assert.equal(r.ok,true,r.stderr);
  assert.equal(r.body.active,shaB);
  assert.equal(r.body.daemon.pid,1202);
  assert.equal(readFileSync(join(root,'.selfdev-managed'),'utf8'),'selfdev-managed-v1\n');
 }finally{rmSync(temp,{recursive:true,force:true});}
});

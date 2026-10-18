#!/usr/bin/env node
/**
 * Operator-approved supervised COLD update for an explicitly enrolled
 * selfdev-only daemon identity. Never modifies default profile or signed app.
 * Live sessions must be absent. Reuses native service install's on-disk
 * recovery, but does NOT claim hot reload or live conversation handoff.
 */
import {existsSync,mkdirSync,readFileSync,writeFileSync,rmSync,lstatSync,readlinkSync} from 'node:fs';
import {homedir} from 'node:os';
import {join,resolve,basename} from 'node:path';
import {randomUUID} from 'node:crypto';
import {spawnSync} from 'node:child_process';

const argv=process.argv.slice(2),command=argv[0];
const opt=(flag)=>{const i=argv.indexOf(flag);return i>=0?argv[i+1]:null;};
const root=resolve(opt('--root')??join(homedir(),'.littlemonkey','selfdev'));
const mode='selfdev-managed-v1',marker=join(root,'.selfdev-managed');
const current=join(root,'current','monkey-cli'),previous=join(root,'previous','monkey-cli');
const selfdevScript=resolve('scripts/selfdev-transaction.mjs');
const managed=join(root,'.supervised-daemon.json'),lock=join(root,'.supervised-daemon.lock');
const agentHome=join(root,'supervised-agent-home');
const sha=opt('--confirm-sha');
const fail=(message)=>{throw Error(message);};
if(!['enroll','upgrade','status'].includes(command))fail('Usage: enroll|upgrade|status [--root PATH] [--confirm-sha SHA]');
if(process.platform==='win32')fail('Selfdev supervisor currently requires POSIX host');
if(!existsSync(marker)||lstatSync(marker).isSymbolicLink()||
    readFileSync(marker,'utf8')!==mode+'\n')
  fail('Unapproved selfdev root. Stage and activate through the existing transaction manager first.');
if(lstatSync(root).isSymbolicLink())fail('Refusing symlinked supervisor root');
function exec(file,args,timeout=300000){
 const result=spawnSync(file,args,{
  encoding:'utf8',timeout,maxBuffer:1024*1024,
  env:{...process.env,GIT_TERMINAL_PROMPT:'0'},
 });
 if(result.error||result.status!==0)fail(file+' '+args.slice(-3).join(' ')+
   ' failed: '+String(result.error?.message??result.stderr??result.stdout).slice(0,600));
 return result.stdout.trim();
}
function manager(action,...args){
 const raw=exec(process.execPath,[selfdevScript,action,'--root',root,...args]);
 const answer=JSON.parse(raw);if(answer.ok!==true)fail('Selfdev manager refused '+action);
 return answer;
}
function active(){
 const result=manager('status');
 return {active:result.active,previous:result.previous};
}
function meta(){
 if(!existsSync(managed)||lstatSync(managed).isSymbolicLink())
  fail('No authorized supervised daemon enrollment');
 const data=JSON.parse(readFileSync(managed,'utf8'));
 if(data.kind!=='selfdev-supervisor-v1' ||
    !/^selfdev-[a-f0-9]{16}$/.test(data.profile) ||
    data.agentHome!==agentHome)fail('Supervisor identity manifest invalid');
 return data;
}
function daemon(binary,profile,...args){
 if(!existsSync(binary)||lstatSync(binary).isSymbolicLink())
  fail('Missing managed immutable binary: '+binary);
 return exec(binary,['--agent-home',agentHome,'--profile',profile,'daemon',...args]);
}
function status(binary,profile){
 const value=JSON.parse(daemon(binary,profile,'status','--json'));
 for(const key of ['installed','service_running','heartbeat_fresh','active',
   'queued','waiting_approval','paused','managed_run_ids','maintenance_drain']){
  if(!(key in value))fail('Incomplete daemon status missing '+key);
 }
 return value;
}
function healthy(binary,profile,oldPid=null){
 let last;
 const deadline=Date.now()+300000; // large binary cold start
 do{
  try{
   const value=status(binary,profile);
   last=value;
   if(value.installed===true && value.service_running===true &&
      value.heartbeat_fresh===true && typeof value.pid==='number' &&
      (oldPid===null||value.pid!==oldPid))return value;
  }catch(error){last=String(error);}
  // Atomically wait without spawning an untracked process.
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,1000);
 }while(Date.now()<deadline);
 fail('Daemon post-restart acceptance did not pass: '+JSON.stringify(last).slice(0,300));
}
function requireHealthy(value){
 if(value.installed!==true||value.service_running!==true||
    value.heartbeat_fresh!==true)fail('Supervised daemon must be healthy');
}
function drainUntilSafe(binary,profile){
 daemon(binary,profile,'drain','--enable');
 const requested=process.env.LM_SELFDEV_DRAIN_TIMEOUT_MS;
 const timeout=requested===undefined?300000:Number(requested);
 if(!Number.isInteger(timeout)||timeout<100||timeout>300000)
  fail('Invalid drain timeout; require 100–300000 milliseconds');
 const deadline=Date.now()+timeout;
 let last;
 do{
  last=status(binary,profile);
  requireHealthy(last);
  if(last.maintenance_drain!==true)fail('Daemon has not acknowledged durable maintenance drain');
  if(last.active===0)return last;
  Atomics.wait(new Int32Array(new SharedArrayBuffer(4)),0,0,500);
 }while(Date.now()<deadline);
 fail('Active tasks did not finish before the bounded drain deadline; no upgrade performed');
}
function acquire(){
 try{mkdirSync(lock,{mode:0o700});}
 catch(error){if(error.code==='EEXIST')fail('Another supervisor process owns the update lock');throw error;}
 writeFileSync(join(lock,'owner.json'),JSON.stringify({pid:process.pid,started:new Date().toISOString()})+'\n',{flag:'wx',mode:0o600});
}
function release(){rmSync(lock,{recursive:true,force:true});}
function enroll(){
 if(existsSync(managed))fail('Supervisor enrollment already exists');
 const pointers=active();
 if(!sha||pointers.active!==sha||!/^[a-f0-9]{64}$/.test(sha))
  fail('Enroll requires explicitly confirmed current binary digest');
 // One selfdev-specific random profile to avoid ever modifying the real daemon.
 mkdirSync(agentHome,{recursive:true,mode:0o700});
 const display='selfdev-'+randomUUID().replaceAll('-','').slice(0,16);
 const output=exec(current,['--agent-home',agentHome,'profiles','create',display,'--json']);
 const identity=JSON.parse(output);
 const profile=identity.id;
 if(profile!==display)fail('Profile creator returned an unexpected identity');
 // Refuse to adopt any service or profile that already owns work.
 const initial=status(current,profile);
 if(initial.installed||initial.service_running||initial.active||initial.queued)
  fail('An existing service or pending job occupies the isolated identity');
 try{
  daemon(current,profile,'install');
  const observed=healthy(current,profile);
  writeFileSync(managed,JSON.stringify({
   kind:'selfdev-supervisor-v1',profile,agentHome,createdAt:new Date().toISOString(),
  },null,2)+'\n',{flag:'wx',mode:0o600});
  return {profile,active:sha,servicePid:observed.pid,supervised:true};
 }catch(error){
  // This disposable profile was just created by this enrollment operation.
  try{daemon(current,profile,'uninstall');}catch{}
  throw error;
 }
}
function upgrade(){
 const identity=meta();
 if(!sha||!/^[a-f0-9]{64}$/.test(sha))fail('Exact candidate SHA-256 is required');
 const pointers=active();
 if(!pointers.active||pointers.active===sha)
  fail('Update requires an existing healthy previous version and a different candidate SHA');
 const baseline=status(current,identity.profile);
 requireHealthy(baseline);
 if(baseline.maintenance_drain)fail('An existing maintenance drain needs operator review');
 let activated=false;
 let drainStarted=false;
 try{
  // Drain live model work naturally at a durable run boundary. New queued
  // work stays in SQLite and must NOT be cancelled by 'daemon stop'.
  drainStarted=true;
  const drained=drainUntilSafe(current,identity.profile);
  // Manager activation is atomic; reinstalling the SAME enrolled service
  // replaces its process without invoking the queue-cancelling Stop command.
  manager('activate','--confirm-sha',sha);
  activated=true;
  daemon(current,identity.profile,'install');
  const observed=healthy(current,identity.profile,baseline.pid);
  requireHealthy(observed);
  if(!observed.maintenance_drain||observed.active!==0||
    observed.queued<drained.queued ||
    observed.waiting_approval<drained.waiting_approval ||
    observed.paused<drained.paused)
   fail('Queued handoff acceptance failed: daemon resumed too early or pending work disappeared');
  daemon(current,identity.profile,'drain','--disable');
  const resumed=status(current,identity.profile);
  requireHealthy(resumed);
  if(resumed.maintenance_drain)
    fail('Candidate did not release durable maintenance drain');
  return{active:sha,previous:pointers.active,profile:identity.profile,
    servicePid:resumed.pid,accepted:true,drainedActiveJobs:drained.active,
    queuedSessionsPreserved:drained.queued,
    liveSessionHandoff:false,inFlightStreamsTransferred:false};
 }catch(error){
  // A drain timeout is NOT a crash and must never cause a service install:
  // reinstalling the old binary here would kill the very active jobs whose
  // completion we were waiting for. Clear the drain in-place and return.
  if(!activated && active().active===pointers.active){
    if(drainStarted){
      try {daemon(current,identity.profile,'drain','--disable');}
      catch(unwind){
        throw Error('UPGRADE ABORTED: '+error.message+
          '; existing daemon was NOT restarted; failed to clear maintenance drain: '+unwind.message);
      }
    }
    throw Error('UPGRADE ABORTED WITHOUT RESTART: '+error.message);
  }
  let recovery;
  try{
   // Even an activation that returned an error might have switched the
   // pointer before failing. Inspect the durable pointer rather than relying
   // exclusively on a local boolean.
   if(active().active!==pointers.active)manager('rollback','--confirm-rollback');
   daemon(current,identity.profile,'install');
   recovery=healthy(current,identity.profile);
   requireHealthy(recovery);
   if(drainStarted)daemon(current,identity.profile,'drain','--disable');
   recovery=status(current,identity.profile);
   if(recovery.maintenance_drain)
    fail('Rollback service did not release maintenance drain');
  }catch(repair){
   throw Error('UPGRADE FAILED: '+error.message+
    '; AUTOMATIC RECOVERY FAILED: '+repair.message+
    '; manual supervisor recovery required. Not safe to proceed.');
  }
  throw Error('UPGRADE FAILED AND ROLLED BACK to '+pointers.active+
    ' (healthy PID '+recovery.pid+'): '+error.message);
 }
}
try{
 if(command==='status'){
  const identity=meta();
  const pointers=active();
  const result=status(current,identity.profile);
  console.log(JSON.stringify({ok:true,profile:identity.profile,...pointers,daemon:result}));
 }else{
  acquire();
  try{
   const result=command==='enroll'?enroll():upgrade();
   console.log(JSON.stringify({ok:true,command,...result}));
  }finally{release();}
 }
}catch(error){
 console.error('SELFDEV_DAEMON_ERROR: '+String(error.message??error));
 process.exitCode=1;
}

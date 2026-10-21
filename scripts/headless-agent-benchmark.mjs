#!/usr/bin/env node
/**
 * Physical benchmark: real installed daemon, configured model, disposable
 * workspaces and OS process measurements. Does not model or simulate answers.
 * The daemon and its configuration are NEVER started or changed by this tool.
 */
import {spawn,spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync,readFileSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {parsePs,rssForTree,summary,completeTreePssKiB,verifyDurableModelCompletion} from './headless-workload-metrics.mjs';
import {initHeadlessFixtureRepository,assertHeadlessFixtureUnchanged} from './headless-fixture-repository.mjs';
const value=key=>{const i=process.argv.indexOf(key);return i<0?null:process.argv[i+1]??null;};
const target=value('--target'),binary=value('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
const levels=(value('--levels')??'1,2,5,10').split(',').map(Number);
const timeoutSeconds=Number(value('--timeout-seconds')??'600');
if(!process.argv.includes('--confirm-disposable') || !target || target.length>150 ||
 /[\x00-\x1f]/.test(target) || levels.length<1 || levels.length>6 ||
 levels.some(x=>!Number.isSafeInteger(x)||x<1||x>20) ||
 new Set(levels).size!==levels.length ||
 !Number.isSafeInteger(timeoutSeconds)||timeoutSeconds<30||timeoutSeconds>3600){
 console.error('Usage: --target INSTALLED_MODEL --levels 1,2,5,10 --timeout-seconds 30..3600 --confirm-disposable');
 process.exit(2);
}
if(!['linux','darwin'].includes(process.platform))throw Error('Requires Linux or macOS for real OS RSS metrics');
function invoke(args,timeout=30000){
 const out=spawnSync(binary,args,{encoding:'utf8',timeout,maxBuffer:2*1024*1024});
 if(out.error||out.status!==0)throw Error('monkey '+args.slice(0,3).join(' ')+': '+
  String(out.error?.message??out.stderr??out.stdout).slice(0,450));
 return out.stdout.trim();
}
function json(args){
 const raw=invoke(args);
 try{return JSON.parse(raw);}catch{throw Error('Installed CLI returned non-JSON for '+args.slice(0,3).join(' '));}
}
function runId(raw){
 const queue=[JSON.parse(raw)];
 while(queue.length){
  const x=queue.pop();
  if(!x||typeof x!=='object')continue;
  if(typeof x.run_id==='string' && /^[-\w]{5,140}$/.test(x.run_id))return x.run_id;
  if(typeof x.runId==='string' && /^[-\w]{5,140}$/.test(x.runId))return x.runId;
  queue.push(...Object.values(x).filter(v=>v&&typeof v==='object'));
 }
 throw Error('No durable run id from real CLI');
}
function sample(pid){
 const x=spawnSync('ps',['-axo','pid=,ppid=,rss='],{encoding:'utf8',timeout:10000,maxBuffer:12*1024*1024});
 if(x.error||x.status!==0)throw Error('OS process table unavailable');
 const rss=rssForTree(parsePs(x.stdout),pid);
 let pssKiB=null;
 if(process.platform==='linux'){
  const rollups=new Map();
  for(const child of rss.pids){
   try {rollups.set(child,readFileSync('/proc/'+child+'/smaps_rollup','utf8'));}
   catch {break;}
  }
  pssKiB=completeTreePssKiB(rss.pids,rollups);
 }
 return {...rss,pssKiB};
}
async function attach(id,deadline){
 return new Promise(resolve=>{
  const started=Date.now();
  const child=spawn(binary,['task','attach',id,'--follow','--json'],{stdio:['ignore','pipe','pipe']});
  let stdout='',stderr='',done=false;
  const finish=(ok,message)=>{
   if(done)return;done=true;
   resolve({runId:id,completed:ok,elapsedMs:Date.now()-started,...(message?{error:message}:{})});
  };
  child.stdout.on('data',b=>{if(stdout.length<16000)stdout+=b.toString().slice(0,16000-stdout.length);});
  child.stderr.on('data',b=>{if(stderr.length<2000)stderr+=b.toString().slice(0,2000-stderr.length);});
  child.on('error',e=>finish(false,e.message));
  child.on('close',code=>{
   if(code!==0){finish(false,stderr.slice(0,180)||'Attachment failed');return;}
   // The ledger and assistant-delta stream, not model claims, determine
   // whether the real fixture completed.
   let verified=false,reason='No durable terminal success evidence';
   try{
    const records=json(['task','status','--run-id',id,'--json']);
    verifyDurableModelCompletion(id,records,stdout,'RESPONSES_CAN_READ_THIS_FIXTURE');
    verified=true;reason='';
   }catch(error){reason=String(error).slice(0,180);}
   finish(verified,reason);
  });
  const timer=setInterval(()=>{
   if(Date.now()>deadline&&!done){
    clearInterval(timer);
    try{invoke(['task','cancel',id]);}catch{}
    child.kill();
    finish(false,'Timed out: cancellation requested for disposable task');
   }
   if(done)clearInterval(timer);
  },500);
  timer.unref();
 });
}
const state=json(['daemon','status','--json']);
if(state.installed!==true||state.service_running!==true||state.heartbeat_fresh!==true||
 !Number.isSafeInteger(state.pid)||state.pid<1)
 throw Error('An already-installed healthy daemon is required; benchmark will not install one');
const pid=state.pid,initial=sample(pid),all=[];
for(const count of levels){
 const roots=[],ids=[],immutableHeads=[];
 try{
  const baseline=sample(pid);
  for(let i=0;i<count;i++){
   const root=mkdtempSync(join(tmpdir(),'lm-headless-physical-'));
   roots.push(root);
   mkdirSync(join(root,'src'));
   writeFileSync(join(root,'src','fixture.txt'),
    'Disposable read-only model benchmark.\nMarker: RESPONSES_CAN_READ_THIS_FIXTURE\n');
   // Autonomous tasks require a committed Git HEAD and revision snapshot.
   immutableHeads.push(initHeadlessFixtureRepository(root));
   const prompt='Read src/fixture.txt in this disposable test workspace. '+
    'Return the exact marker and explain the observed file. Do not edit files, '+
    'run shell commands, make network requests or deploy anything.';
   ids.push(runId(invoke(['task','start',prompt,'--target',target,'--workspace',root,'--json'])));
  }
  const deadline=Date.now()+timeoutSeconds*1000;
  let done=false;
  const pending=Promise.all(ids.map(id=>attach(id,deadline))).then(result=>{done=true;return result;});
  const samples=[baseline];
  while(!done){
   samples.push(sample(pid));
   await Promise.race([pending,new Promise(resolve=>setTimeout(resolve,250))]);
  }
  const outcomes=await pending;
  // A read-only *prompt* is not a write-protection boundary. Validate the
  // actual committed Git checkout after each native model task, before
  // accounting its result as successful. This catches autonomous file
  // edits, new untracked files and unauthorized Git commits.
  for(let i=0;i<roots.length;i++){
   try{assertHeadlessFixtureUnchanged(roots[i],immutableHeads[i]);}
   catch(error){
    outcomes[i]={...outcomes[i],completed:false,
     error:String(error?.message??error).slice(0,180)};
   }
  }
  const stats=summary(count,outcomes,samples);
  const measuredPss=samples.map(row=>row.pssKiB)
   .filter(value=>Number.isSafeInteger(value)&&value>=0);
  const peakPssMiB=measuredPss.length===samples.length
   ? +(Math.max(...measuredPss)/1024).toFixed(2):null;
  all.push({...stats,target,
   baselineDaemonTreeRssMiB:+(baseline.rssKiB/1024).toFixed(2),
   peakRssDeltaMiB:+Math.max(0,stats.daemonTreePeakRssMiB-baseline.rssKiB/1024).toFixed(2),
   peakPssMiB,completePssSamples:measuredPss.length,totalRssSamples:samples.length,
   pssUnavailableReason:peakPssMiB===null
    ? (process.platform==='linux'?'Missing/inaccessible rollup or process-exit race':'Linux-only metric'):null,
   runs:outcomes});
  console.error('Real model headless '+count+': '+stats.completed+'/'+count+' completed');
 }finally{
  for(const id of ids){
   try{
    const status=json(['task','status','--run-id',id,'--json']);
    if(!Array.isArray(status)||status.length!==1||status[0].run_id!==id)
     throw Error('Unrecognized durable task status during cleanup');
    if(!['succeeded','failed','cancelled','needs_reconciliation'].includes(status[0].status))
     invoke(['task','cancel',id]);
   }catch{}
  }
  for(const dir of roots)rmSync(dir,{recursive:true,force:true});
 }
}
console.log(JSON.stringify({
 schema:1,kind:'physical-headless-daemon-model-benchmark',executedRealModels:true,
 metric:'ps RSS KiB summed over installed daemon PID and OS descendants',
 exclusions:'External model runtimes outside daemon tree excluded. Only complete Linux PSS snapshots are comparable in definition; no JCode head-to-head measurements.',
 machine:{platform:process.platform,arch:process.arch},daemonPid:pid,
 initialRssMiB:+(initial.rssKiB/1024).toFixed(2),
 results:all,recordedAt:new Date().toISOString(),
},null,2));
if(all.some(row=>row.failed>0))process.exitCode=1;

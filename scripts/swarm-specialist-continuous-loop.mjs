#!/usr/bin/env node
/**
 * Unattended, operator-opted-in real-model specialist learning and deployment.
 *
 * Runs the repository's independent executable-source oracle (not a model's
 * self-score), promotes only complete fresh observations and dispatches real
 * native CLI tasks through an atomic, versioned active routing manifest.
 *
 * This is a CLI routing boundary, NOT a desktop model-weight hot-swap.
 */
import {spawnSync} from 'node:child_process';
import {createHash,randomUUID} from 'node:crypto';
import {readFileSync,writeFileSync,openSync,closeSync,fsyncSync,renameSync,
  mkdirSync,rmdirSync,rmSync,realpathSync,lstatSync,existsSync,renameSync as moveDir} from 'node:fs';
import {join,resolve,isAbsolute,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';

const DOMAINS={
 security:{roles:['Threat model reviewer','Secrets exposure reviewer','Race condition reviewer'],fixtures:['auth-bypass','code-injection','safe-control']},
 frontend:{roles:['Frontend performance reviewer','Browser security reviewer','UI testability reviewer'],fixtures:['event-handler','accessibility-label','safe-control']},
 backend:{roles:['Backend caching reviewer','API resilience reviewer','Data privacy reviewer'],fixtures:['tenant-leak','timeout-budget','safe-control']},
 performance:{roles:['Queueing reviewer','Memory and cache reviewer','Capacity reviewer'],fixtures:['quadratic-scan','cache-recompute','safe-control']},
 general:{roles:['Documentation reviewer','Incident response reviewer','Integration reviewer'],fixtures:['configuration-flag','integration-contract','safe-control']},
};
const TARGET=/^ollama:[A-Za-z0-9_.:@+-]{2,140}$/;
const RUN=/^run-[A-Za-z0-9-]{6,200}$/;
const HASH=/^[0-9a-f]{64}$/;
const MAX_REPORT_BYTES=1_000_000;
const own=path=>Object.prototype.hasOwnProperty.call(DOMAINS,path);
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const strictKeys=(data,keys)=>Object.keys(data).sort().join(',')===keys.slice().sort().join(',');
const boundedInt=(v,min,max)=>Number.isSafeInteger(v)&&v>=min&&v<=max;
const fail=message=>{throw Error(message);};
function localDir(path,name){
 if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path)||
   path.split(/[\\/]/).includes('..'))fail(name+' requires a canonical local absolute path');
 const stat=lstatSync(path);
 if(!stat.isDirectory()||stat.isSymbolicLink()||realpathSync(path)!==resolve(path))
   fail(name+' must be an existing real canonical directory');
 return path;
}
export function parsePolicy(value){
 const x=typeof value==='string'?JSON.parse(value):value;
 if(!x||typeof x!=='object'||Array.isArray(x)||
    !strictKeys(x,['version','monkeyBin','stateDir','targets','domains',
      'repetitions','intervalSeconds','timeoutMs','minAccuracy','minImprovement','maxAgeHours']))
   fail('Expected exact operator specialist learning policy v1');
 if(x.version!==1||typeof x.monkeyBin!=='string'||!isAbsolute(x.monkeyBin)||
    !existsSync(x.monkeyBin)||!lstatSync(realpathSync(x.monkeyBin)).isFile())
   fail('Operator must select an installed absolute native monkey executable');
 const stateDir=localDir(x.stateDir,'State directory');
 if(!Array.isArray(x.targets)||x.targets.length<1||x.targets.length>8||
   new Set(x.targets).size!==x.targets.length||x.targets.some(t=>typeof t!=='string'||!TARGET.test(t)))
   fail('Only unique configured Ollama target identifiers are allowed');
 if(!Array.isArray(x.domains)||!x.domains.length||x.domains.length>5||
   new Set(x.domains).size!==x.domains.length||x.domains.some(d=>typeof d!=='string'||!own(d)))
   fail('Invalid or duplicate specialist domains');
 if(!boundedInt(x.repetitions,2,5)||!boundedInt(x.intervalSeconds,60,86_400)||
   !boundedInt(x.timeoutMs,10_000,3_600_000)||
   !boundedInt(x.maxAgeHours,1,168)||
   typeof x.minAccuracy!=='number'||x.minAccuracy<0.8||x.minAccuracy>1||
   typeof x.minImprovement!=='number'||x.minImprovement<0||x.minImprovement>0.25)
   fail('Invalid safety/quality bounds');
 if(realpathSync(x.monkeyBin).startsWith(stateDir+'/'))fail('Executable cannot live inside mutable routing state');
 return {...x,monkeyBin:realpathSync(x.monkeyBin),stateDir};
}
function readBounded(path,max=MAX_REPORT_BYTES){
 if(lstatSync(path).isSymbolicLink())fail('Symbolic evidence link refused');
 const stat=lstatSync(path);
 if(!stat.isFile()||stat.size<=0||stat.size>max)fail('Malformed or oversized state/evidence');
 const text=readFileSync(path,'utf8');
 if(Buffer.byteLength(text)>max)fail('Evidence changed while reading');
 return text;
}
function atomicJson(path,value){
 const tmp=path+'.'+randomUUID()+'.tmp';
 const data=JSON.stringify(value,null,2)+'\n';
 const fd=openSync(tmp,'wx',0o600);
 try{writeFileSync(fd,data);fsyncSync(fd);}finally{closeSync(fd);}
 try{
  renameSync(tmp,path);
  const directory=openSync(dirname(path),'r');
  try{fsyncSync(directory);}finally{closeSync(directory);}
 }catch(error){rmSync(tmp,{force:true});throw error;}
}
export function gradeReport(raw,domain,target,repetitions,now=Date.now(),maxAgeMs=3_600_000){
 const d=DOMAINS[domain];
 if(!d||!TARGET.test(target)||!boundedInt(repetitions,2,5)||
   !raw||typeof raw!=='object'||Array.isArray(raw)||
   raw.version!==2||raw.benchmark!=='live-specialist-source-fixtures'||
   raw.mode!=='real-model'||raw.domain!==domain||raw.target!==target)
   fail('Wrong live independent specialist benchmark identity');
 const when=typeof raw.generatedAt==='string'?Date.parse(raw.generatedAt):NaN;
 if(!Number.isFinite(when)||when>now+300_000||now-when>maxAgeMs)fail('Benchmark is stale or future-dated');
 const count=d.roles.length*d.fixtures.length*repetitions;
 if(!Array.isArray(raw.results)||raw.results.length!==count||
   !Array.isArray(raw.ranking)||raw.ranking.length!==d.roles.length)
   fail('Independent benchmark must cover every role, fixture, repetition');
 const seen=new Set(),runIds=new Set(),rows=new Map(d.roles.map(role=>[role,[]]));
 for(const result of raw.results){
  if(!result||typeof result!=='object'||Array.isArray(result)||
     !d.roles.includes(result.specialist)||!d.fixtures.includes(result.fixture)||
     !boundedInt(result.repeat,1,repetitions)||
     !boundedInt(result.durationMs,0,3_600_000)||
     typeof result.verified!=='boolean'||
     !(result.runId===null||typeof result.runId==='string'))
    fail('Malformed oracle observation');
  const key=JSON.stringify([result.specialist,result.fixture,result.repeat]);
  if(seen.has(key))fail('Repeated oracle fixture');
  seen.add(key);
  if(result.verified&&!RUN.test(result.runId??''))fail('A pass must have a real native task run receipt');
  if(result.runId!==null){
   if(!RUN.test(result.runId)||runIds.has(result.runId))fail('Invalid or repeated native run identity');
   runIds.add(result.runId);
  }
  rows.get(result.specialist).push(result);
 }
 const ranking=d.roles.map(role=>{
  const values=rows.get(role);
  const passes=values.filter(x=>x.verified);
  return {specialist:role,passes:passes.length,attempts:values.length,
    accuracy:Number((passes.length/values.length).toFixed(4)),
    averageVerifiedMs:passes.length
      ?Math.round(passes.reduce((sum,x)=>sum+x.durationMs,0)/passes.length):null};
 }).sort((a,b)=>b.accuracy-a.accuracy||
   (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity)||
   d.roles.indexOf(a.specialist)-d.roles.indexOf(b.specialist));
 for(let i=0;i<ranking.length;i++){
  if(JSON.stringify(raw.ranking[i])!==JSON.stringify(ranking[i]))
    fail('Model-supplied or altered rank contradicts oracle observations');
 }
 const total=ranking.reduce((s,x)=>s+x.passes,0);
 return {domain,target,generatedAt:raw.generatedAt,ranking,
   accuracy:Number((total/count).toFixed(4)),
   bestSpecialist:ranking[0].specialist,
   reportSha256:sha(JSON.stringify(raw))};
}
/** Decides ONLY between fully remeasured already-configured local targets. */
export function chooseDeployment(policy,reports,prior,now=Date.now()){
 const changes=[];
 for(const domain of policy.domains){
  const eligible=policy.targets.map(target=>reports.find(row=>row.domain===domain&&row.target===target))
    .filter(Boolean).filter(row=>row.accuracy>=policy.minAccuracy);
  eligible.sort((a,b)=>b.accuracy-a.accuracy||
    (a.ranking[0].averageVerifiedMs??Infinity)-(b.ranking[0].averageVerifiedMs??Infinity)||
    policy.targets.indexOf(a.target)-policy.targets.indexOf(b.target));
  const winner=eligible[0]??null;
  const previous=prior?.domains?.[domain]?.active??null;
  const priorTarget=previous?eligible.find(row=>row.target===previous.target):null;
  let active=null,reason='no_complete_passing_benchmark';
  if(priorTarget && winner && winner.target!==priorTarget.target &&
      winner.accuracy-priorTarget.accuracy<policy.minImprovement){
    active=priorTarget;reason='kept_existing_no_meaningful_gain';
  }else if(winner){active=winner;reason=winner.target===previous?.target?'verified_current':'promoted_verified_target';}
  // A failing previously active target is NEVER grandfathered in. If the
  // old winner regresses, this promotes the strongest remeasured fallback.
  changes.push({domain,active,previous:priorTarget&&priorTarget.target!==active?.target?{
    domain,target:priorTarget.target,accuracy:priorTarget.accuracy,
    bestSpecialist:priorTarget.bestSpecialist,reportSha256:priorTarget.reportSha256,
    generatedAt:priorTarget.generatedAt}:null,reason});
 }
 return {version:1,revision:(prior?.revision??0)+1,createdAt:now,
  domains:Object.fromEntries(changes.map(x=>[x.domain,{
    active:x.active?{domain:x.domain,target:x.active.target,
      accuracy:x.active.accuracy,bestSpecialist:x.active.bestSpecialist,
      reportSha256:x.active.reportSha256,generatedAt:x.active.generatedAt}:null,
    previous:x.previous,reason:x.reason}]))};
}
function validateRoutingState(state,policy,now=Date.now()){
 if(!state||state.version!==1||!boundedInt(state.revision,1,1_000_000)||
   !boundedInt(state.createdAt,1,now+300_000)||!state.domains||
   typeof state.domains!=='object'||Array.isArray(state.domains)||
   Object.keys(state.domains).some(x=>!policy.domains.includes(x)))fail('Corrupted routing state');
 for(const domain of policy.domains){
  const entry=state.domains[domain];
  if(!entry||!['no_complete_passing_benchmark','kept_existing_no_meaningful_gain',
    'verified_current','promoted_verified_target'].includes(entry.reason))
    fail('Unknown active specialist domain state');
  for(const candidate of [entry.active,entry.previous]){
   if(candidate===null)continue;
   if(!candidate||candidate.domain!==domain||
      !policy.targets.includes(candidate.target)||
      !DOMAINS[domain].roles.includes(candidate.bestSpecialist)||
      !HASH.test(candidate.reportSha256??'')||
      typeof candidate.accuracy!=='number'||candidate.accuracy<policy.minAccuracy||
      candidate.accuracy>1||
      typeof candidate.generatedAt!=='string'||
      !Number.isFinite(Date.parse(candidate.generatedAt))||
      now-Date.parse(candidate.generatedAt)>policy.maxAgeHours*3_600_000||
      Date.parse(candidate.generatedAt)>now+300_000)
     fail('Active specialist target has no recent independently graded evidence');
  }
 }
 return state;
}
function verifyEvidence(policy,entry,now){
 if(!entry)return;
 const file=join(policy.stateDir,'evidence-'+entry.domain+'-'+sha(entry.target).slice(0,20)+'-'+entry.reportSha256+'.json');
 const saved=JSON.parse(readBounded(file,MAX_REPORT_BYTES));
 if(saved.version!==1||!saved.raw||!saved.grade)
  fail('Deployed target has no durable independent oracle evidence');
 const checked=gradeReport(saved.raw,entry.domain,entry.target,
   policy.repetitions,now,policy.maxAgeHours*3_600_000);
 if(checked.reportSha256!==entry.reportSha256||
    checked.accuracy!==entry.accuracy||
    checked.bestSpecialist!==entry.bestSpecialist||
    checked.generatedAt!==entry.generatedAt||
    saved.grade.reportSha256!==checked.reportSha256)
  fail('Deployed routing decision and independent evidence disagree');
}
function state(policy,now=Date.now()){
 const path=join(policy.stateDir,'active-routing.json');
 if(!existsSync(path))return null;
 const current=validateRoutingState(JSON.parse(readBounded(path,64_000)),policy,now);
 for(const domain of policy.domains){
  verifyEvidence(policy,current.domains[domain].active,now);
  verifyEvidence(policy,current.domains[domain].previous,now);
 }
 return current;
}
export function benchmarkCycle(policy,{run=spawnSync,now=Date.now(),log=console.error}={}){
 const previous=state(policy,now);
 const results=[],reports=[];
 const fixtureScript=resolve(dirname(fileURLToPath(import.meta.url)),'swarm-specialist-live-benchmark.mjs');
 for(const domain of policy.domains)for(const target of policy.targets){
  const result=run(process.execPath,[fixtureScript,'--domain',domain,
    '--target',target,'--repetitions',String(policy.repetitions),
    '--timeout-ms',String(policy.timeoutMs),'--monkey-bin',policy.monkeyBin],{
    encoding:'utf8',timeout:policy.timeoutMs*policy.repetitions*9+60_000,
    maxBuffer:MAX_REPORT_BYTES,env:process.env,
  });
  let grade=null;
  try{
   if(result.error||![0,1].includes(result.status))fail('Oracle execution did not complete');
   const text=result.stdout?.trim();
   if(!text||Buffer.byteLength(text)>MAX_REPORT_BYTES)fail('Missing bounded independently graded output');
   const raw=JSON.parse(text);
   grade=gradeReport(raw,domain,target,policy.repetitions,Date.now());
   atomicJson(join(policy.stateDir,'evidence-'+domain+'-'+sha(target).slice(0,20)+'-'+grade.reportSha256+'.json'),
     {version:1,grade,raw});
   reports.push(grade);
  }catch(error){log(JSON.stringify({domain,target,status:'benchmark_rejected',error:String(error)}));}
  results.push({domain,target,accuracy:grade?.accuracy??null});
 }
 const currentTime=Date.now();
 const fresh=reports.filter(row=>Date.parse(row.generatedAt)>=currentTime-policy.maxAgeHours*3_600_000);
 const next=chooseDeployment(policy,fresh,previous,now);
 // A lost/rejected benchmark disables this domain; it never silently
 // preserves stale authority. A completed cycle is written in one rename.
 validateRoutingState(next,policy,now);
 atomicJson(join(policy.stateDir,'active-routing.json'),next);
 log(JSON.stringify({status:'specialist_cycle_completed',revision:next.revision,results,
   routing:Object.fromEntries(Object.entries(next.domains).map(([d,v])=>[d,v.active?.target??null]))}));
 return next;
}
export function dispatchRecipe(policy,domain,recipePath,{run=spawnSync,now=Date.now()}={}){
 if(!policy.domains.includes(domain))fail('Unapproved dispatch domain');
 const deployed=state(policy,now);
 const active=deployed?.domains?.[domain]?.active;
 if(!active)fail('No fresh independently verified deployed target; fail closed');
 const source=JSON.parse(readBounded(recipePath,256_000));
 if(!source||typeof source!=='object'||Array.isArray(source)||
   source.version!==1||source.permission_mode==='bypass'||
   typeof source.workspace!=='string'||!isAbsolute(source.workspace))
  fail('Require a valid existing native task recipe without bypass permissions');
 // Never edit the original recipe or its permissions/tools/system prompt.
 const temporary=join(policy.stateDir,'dispatch-'+randomUUID()+'.json');
 const fd=openSync(temporary,'wx',0o600);
 try{
  try{
   writeFileSync(fd,JSON.stringify({...source,target:{ollama:active.target.slice(7)}}));
   fsyncSync(fd);
  }finally{closeSync(fd);}
  const result=run(policy.monkeyBin,['task','run',temporary,'--json'],{
    encoding:'utf8',timeout:policy.timeoutMs,maxBuffer:MAX_REPORT_BYTES,env:process.env});
  if(result.error||result.status!==0)fail('Actual native task failed: '+String(result.error??result.stderr??'').slice(0,250));
  return {target:active.target,domain,specialist:active.bestSpecialist,
    nativeResult:result.stdout,revision:deployed.revision};
 }finally{rmSync(temporary,{force:true});}
}
/**
 * Crash-safe service ownership. A dead PID can be quarantined and reclaimed;
 * a live PID, unknown owner, or too-recent half-written lease always blocks.
 * Only read-only benchmarks are run; an orphan inference cannot mutate code.
 */
export function acquireCycleLock(stateDir,{
 pid=process.pid,now=Date.now(),alive=pid=>{
  try{process.kill(pid,0);return true;}catch(error){
   if(error?.code==='ESRCH')return false;
   return true; // EPERM and unknown OS errors must fail closed.
  }
 }}={}){
 const lock=join(stateDir,'.specialist-cycle-owner');
 const install=()=>{
  mkdirSync(lock,{mode:0o700});
  const token=randomUUID();
  try{atomicJson(join(lock,'owner.json'),{version:1,pid,token,createdAt:now});}
  catch(error){rmSync(lock,{recursive:true,force:true});throw error;}
  return ()=>{
   const owner=JSON.parse(readBounded(join(lock,'owner.json'),2048));
   if(owner.pid!==pid||owner.token!==token)fail('Lock ownership changed: do not remove another worker');
   rmSync(join(lock,'owner.json'));
   rmdirSync(lock);
  };
 };
 try{return install();}
 catch(error){
  if(error?.code!=='EEXIST')throw error;
 }
 if(lstatSync(lock).isSymbolicLink()||!lstatSync(lock).isDirectory())
  fail('Specialist worker lock must be a real directory');
 const ownerPath=join(lock,'owner.json');
 if(existsSync(ownerPath)){
  const owner=JSON.parse(readBounded(ownerPath,2048));
  if(owner.version!==1||!boundedInt(owner.pid,1,2**31-1)||
     typeof owner.token!=='string'||!/^[a-z0-9-]{36}$/i.test(owner.token)||
     !boundedInt(owner.createdAt,1,now+300_000)||alive(owner.pid))
   fail('A live or unverified specialist worker already owns this state directory');
 }else{
  // Owner creation may have been interrupted. Do not steal a young lock.
  const stat=lstatSync(lock);
  if(now-stat.mtimeMs<5*60_000)
   fail('A recent uninitialized worker lease cannot be safely reclaimed');
 }
 const quarantine=lock+'.crashed-'+randomUUID();
 moveDir(lock,quarantine); // only one contender can move this exact stale lock
 rmSync(quarantine,{recursive:true,force:true});
 return install();
}
const flag=name=>{const index=process.argv.indexOf(name);return index<0?null:process.argv[index+1]??null;};
async function main(){
 const policyPath=flag('--policy');
 if(!policyPath||!isAbsolute(policyPath)||!process.argv.includes('--confirm-unattended-local-evaluation'))
  fail('Use --policy /absolute/path.json --confirm-unattended-local-evaluation');
 const policy=parsePolicy(readBounded(policyPath,16_384));
 if(process.argv.includes('--dispatch')){
  const result=dispatchRecipe(policy,flag('--domain'),flag('--recipe'));
  process.stdout.write(JSON.stringify(result)+'\n');return;
 }
 const release=acquireCycleLock(policy.stateDir);
 try{
  do{
   benchmarkCycle(policy);
   if(process.argv.includes('--once'))break;
   await new Promise(done=>setTimeout(done,policy.intervalSeconds*1000));
  }while(true);
 }finally{release();}
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 main().catch(error=>{console.error(JSON.stringify({status:'specialist_loop_stopped',
   error:String(error).slice(0,600)}));process.exitCode=1;});
}

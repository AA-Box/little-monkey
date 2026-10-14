#!/usr/bin/env node
/**
 * Continuous unattended independently graded model quality, opt-in only.
 *
 * Run on an operator-owned machine with installed monkey CLI and approved
 * model targets. Each fixture uses a disposable repository and independent
 * executable node tests, never a model's assertion of its own correctness.
 * This program accepts no model-provided grading and never activates models.
 */
import {spawnSync} from 'node:child_process';
import {readFileSync,writeFileSync,mkdirSync,renameSync,openSync,closeSync,unlinkSync} from 'node:fs';
import {resolve,dirname,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {createHash} from 'node:crypto';
const arg=n=>{const i=process.argv.indexOf(n);return i<0?null:process.argv[i+1]??null};
const FIXTURES=['logic-regression','authorization-regression','parsing-regression'];
const TARGET=/^(ollama|provider|local):[a-zA-Z0-9_./:@+%-]{1,150}$/;
const fail=message=>{throw Error(message)};
export function judgeContinuousReport(value,{now=Date.now(),minAccuracy=0.8,
   previous=null}={}){
 if(!value||value.version!==1||value.mode!=='real-model'||
    value.benchmark!=='live-task-verified-fixtures'||!Array.isArray(value.results)||
    !Array.isArray(value.ranking)||value.ranking.length<1||value.ranking.length>8||
    value.results.length>120||typeof value.generatedAt!=='string'||
    Math.abs(Date.parse(value.generatedAt)-now)>300000)
   fail('Live independent fixture report missing or stale');
 const targets=new Set(value.ranking.map(r=>r?.target));
 if(targets.size!==value.ranking.length||[...targets].some(t=>!TARGET.test(t)))
   fail('Unconfigured or duplicate benchmark target');
 const seen=new Set(),runIds=new Set();
 const groups=new Map([...targets].map(t=>[t,[]]));
 for(const item of value.results){
  if(!item||!targets.has(item.target)||!FIXTURES.includes(item.fixture)||
    !Number.isInteger(item.repeat)||item.repeat<1||item.repeat>5||
    typeof item.verified!=='boolean'||!Number.isSafeInteger(item.durationMs)||
    item.durationMs<0||item.durationMs>3600000)
   fail('Malformed real-model benchmark observation');
  const key=item.target+':'+item.fixture+':'+item.repeat;
  if(seen.has(key))fail('Duplicate independent fixture grade');
  seen.add(key);
  if(item.verified){
   if(typeof item.runId!=='string'||!/^run-[A-Za-z0-9-]{6,200}$/.test(item.runId)||
      runIds.has(item.runId))fail('Missing or reused durable real-model run identifier');
   runIds.add(item.runId);
  }
  groups.get(item.target).push(item);
 }
 const ranks=[];
 for(const target of targets){
  const rows=groups.get(target),repeats=rows.length/3;
  if(!Number.isInteger(repeats)||repeats<3||repeats>5)
   fail('Continuous safety gate needs 3..5 real repeated fixtures per target');
  for(let repeat=1;repeat<=repeats;repeat++)
   for(const fixture of FIXTURES)
    if(!seen.has(target+':'+fixture+':'+repeat))fail('Partial independent quality grade');
  const good=rows.filter(x=>x.verified);
  ranks.push({target,attempts:rows.length,passes:good.length,
   accuracy:Number((good.length/rows.length).toFixed(4)),
   averageVerifiedMs:good.length?
    Math.round(good.reduce((n,x)=>n+x.durationMs,0)/good.length):null});
 }
 ranks.sort((a,b)=>b.accuracy-a.accuracy||
   (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity));
 if(JSON.stringify(ranks)!==JSON.stringify(value.ranking.map(r=>({
    target:r.target,attempts:r.attempts,passes:r.passes,accuracy:r.accuracy,
    averageVerifiedMs:r.averageVerifiedMs
  }))))fail('Untrusted benchmark ranking does not match independent grader');
 const prior=Array.isArray(previous?.report?.ranking)?new Map(
  previous.report.ranking.map(r=>[r.target,r])):new Map();
 const regressions=ranks.filter(r=>r.accuracy<minAccuracy ||
   (prior.has(r.target)&&r.accuracy+0.02<prior.get(r.target).accuracy));
 return {version:1,source:'native_disposable_repository_independent_tests',
  createdAt:new Date(now).toISOString(),
  validUntil:new Date(now+36*60*60*1000).toISOString(),
  status:regressions.length?'regression_blocked':'verified',
  regressionTargets:regressions.map(r=>r.target),
  report:value};
}
function atomic(path,value){
 if(!isAbsolute(path))fail('Continuous quality receipt requires absolute path');
 const destination=resolve(path),temp=destination+'.new-'+process.pid;
 mkdirSync(dirname(destination),{recursive:true,mode:0o700});
 let fd;
 try{fd=openSync(temp,'wx',0o600);
  writeFileSync(fd,JSON.stringify(value,null,2)+'\n','utf8');
  closeSync(fd);fd=null;renameSync(temp,destination);
 }catch(error){if(fd!==null&&fd!==undefined)closeSync(fd);
  try{unlinkSync(temp)}catch{}throw error}
}
function main(){
 const output=arg('--output'),targets=arg('--targets');
 if(!output||!targets||targets.length>1000)fail('Require --output ABSOLUTE_APP_DATA_PATH --targets approved,configured,comma-separated models');
 const selection=targets.split(',').map(s=>s.trim());
 if(selection.length<1||selection.length>8||new Set(selection).size!==selection.length||
    selection.some(t=>!TARGET.test(t)))fail('Malformed approved target list');
 let previous=null;
 try{previous=JSON.parse(readFileSync(output,'utf8'))}catch{}
 // Benchmark failures are real negative quality outcomes, not a reason to
 // forge a green receipt or retain a stale successful receipt indefinitely.
 const command=spawnSync(process.execPath,[
  fileURLToPath(new URL('./swarm-model-benchmark.mjs',import.meta.url)),
  '--targets',selection.join(','),'--repetitions','3',
 ],{encoding:'utf8',timeout:3_600_000,maxBuffer:1024*1024,
    env:process.env});
 let receipt;
 try{
  if(command.error||![0,1].includes(command.status)||!command.stdout)
   fail('Native model benchmark did not finish with durable reports');
  const report=JSON.parse(command.stdout);
  receipt=judgeContinuousReport(report,{previous});
 }catch(err){
  receipt={version:1,status:'blocked',source:'native_disposable_repository_independent_tests',
   createdAt:new Date().toISOString(),reason:'No complete independently verified fixture evidence'};
 }
 // The last receipt, even on failure, supersedes the previous one. A failing
 // quality run must never leave yesterday's certificate authorized as current.
 receipt.sha256=createHash('sha256').update(JSON.stringify(receipt)).digest('hex');
 atomic(output,receipt);
 console.log(JSON.stringify({status:receipt.status,source:receipt.source,
  checkedAt:receipt.createdAt,receipt:resolve(output),
  regressions:receipt.regressionTargets??[]}));
 if(receipt.status!=='verified')process.exitCode=1;
}
if(process.argv[1]&&resolve(process.argv[1])===fileURLToPath(import.meta.url)){
 try{main()}catch(e){console.error('QUALITY_NOT_VERIFIED: '+String(e.message).slice(0,200));process.exitCode=1}
}

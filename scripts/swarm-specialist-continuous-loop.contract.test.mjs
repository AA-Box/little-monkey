import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {gradeReport,parsePolicy,chooseDeployment,benchmarkCycle,dispatchRecipe,acquireCycleLock}
 from './swarm-specialist-continuous-loop.mjs';
const roles=['Threat model reviewer','Secrets exposure reviewer','Race condition reviewer'];
const fixtures=['auth-bypass','code-injection','safe-control'];
const targetA='ollama:trusted-a',targetB='ollama:trusted-b';
const now=Date.now();
function report(target,passing=27,repetitions=3){
 const results=[];
 let i=0;
 for(const specialist of roles)for(let repeat=1;repeat<=repetitions;repeat++)
  for(const fixture of fixtures){
   const verified=i<passing;
   results.push({specialist,fixture,repeat,
    runId:'run-'+target.split(':')[1].replaceAll('-','')+'-'+(i+1).toString().padStart(6,'0'),
    verified,durationMs:10+i});
   i++;
  }
 const ranking=roles.map(specialist=>{
  const rows=results.filter(r=>r.specialist===specialist);
  const passes=rows.filter(r=>r.verified);
  return {specialist,passes:passes.length,attempts:rows.length,
   accuracy:Number((passes.length/rows.length).toFixed(4)),
   averageVerifiedMs:passes.length?
    Math.round(passes.reduce((sum,r)=>sum+r.durationMs,0)/passes.length):null};
 }).sort((a,b)=>b.accuracy-a.accuracy||
  (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity)||
  roles.indexOf(a.specialist)-roles.indexOf(b.specialist));
 return {version:2,benchmark:'live-specialist-source-fixtures',
  mode:'real-model',domain:'security',target,generatedAt:new Date(now).toISOString(),
  ranking,results};
}
function fixture(){
 const stateDir=mkdtempSync(join(tmpdir(),'lm-specialist-auto-'));
 const policy=parsePolicy({version:1,monkeyBin:process.execPath,stateDir,
  targets:[targetA,targetB],domains:['security'],
  repetitions:3,intervalSeconds:60,timeoutMs:30000,minAccuracy:0.8,
  minImprovement:0.05,maxAgeHours:48});
 return {stateDir,policy,dispose:()=>rmSync(stateDir,{force:true,recursive:true})};
}
function fakeRunner(reports,seen=[]){
 return (cmd,args,opts)=>{
  seen.push({cmd,args,opts});
  const target=args[args.indexOf('--target')+1];
  return {status:reports[target]?0:2,stdout:reports[target]?JSON.stringify(reports[target]):'',
   stderr:reports[target]?'':'failed'};
 };
}
describe('continuous verified specialist learning and real native recipe deployment',()=>{
 it('refuses incomplete, duplicate, model-self-ranked and fake-run evidence',()=>{
  const good=report(targetA);
  assert.equal(gradeReport(good,'security',targetA,3,now).accuracy,1);
  assert.throws(()=>gradeReport({...good,results:good.results.slice(1)},'security',targetA,3,now),/cover/);
  const lie=structuredClone(good);lie.ranking[0].accuracy=0;
  assert.throws(()=>gradeReport(lie,'security',targetA,3,now),/contradicts/);
  const dupe=structuredClone(good);dupe.results[1].runId=dupe.results[0].runId;
  assert.throws(()=>gradeReport(dupe,'security',targetA,3,now),/repeated native run/);
  const noRun=structuredClone(good);noRun.results[0].runId=null;
  assert.throws(()=>gradeReport(noRun,'security',targetA,3,now),/real native task run/);
  const stale=structuredClone(good);
  stale.generatedAt=new Date(now-3_600_001).toISOString();
  assert.throws(()=>gradeReport(stale,'security',targetA,3,now),/stale/);
 });
 it('requires strict one-time operator policy and retains task permissions',()=>{
  const env=fixture();
  try{
   assert.throws(()=>parsePolicy({...env.policy,minAccuracy:0.3}),/safety/);
   assert.throws(()=>parsePolicy({...env.policy,targets:['not-ollama']}),/Ollama/);
   assert.throws(()=>parsePolicy({...env.policy,unknownFlag:true}),/exact/);
   const recipe=join(env.stateDir,'source.json');
   writeFileSync(recipe,JSON.stringify({version:1,workspace:env.stateDir,
    permission_mode:'ask',target:{ollama:'operator-default'},
    system:'Keep my original system rules',prompt:'Check real security'}));
   assert.throws(()=>dispatchRecipe(env.policy,'security',recipe),/No fresh/);
   benchmarkCycle(env.policy,{now,run:fakeRunner({
    [targetA]:report(targetA,27),[targetB]:report(targetB,24),
   }),log:()=>{}});
   let called=false;
   const execution=dispatchRecipe(env.policy,'security',recipe,{now,run:(bin,args)=>{
    called=true;
    assert.equal(bin,process.execPath);
    assert.deepEqual(args.slice(0,2),['task','run']);
    const submitted=JSON.parse(readFileSync(args[2],'utf8'));
    assert.equal(submitted.permission_mode,'ask');
    assert.equal(submitted.system,'Keep my original system rules');
    assert.deepEqual(submitted.target,{ollama:'trusted-a'});
    return {status:0,stdout:'{"status":"ok","run_id":"run-verified123"}'};
   }});
   assert.ok(called);
   assert.equal(execution.target,targetA);
   assert.equal(JSON.parse(readFileSync(recipe,'utf8')).target.ollama,'operator-default');
   assert.throws(()=>dispatchRecipe(env.policy,'security',recipe,{now,
    run:()=>({status:1,stderr:'task failed'})}),/Actual native task failed/);
   const bypass=join(env.stateDir,'bypass.json');
   writeFileSync(bypass,JSON.stringify({version:1,workspace:env.stateDir,permission_mode:'bypass'}));
   assert.throws(()=>dispatchRecipe(env.policy,'security',bypass,{now}),/bypass/);
  }finally{env.dispose();}
 });
 it('promotes only real improved targets, rechecks them, and rolls back on regression',()=>{
  const env=fixture();
  try{
   const passA=report(targetA,27),lowB=report(targetB,24);
   const first=benchmarkCycle(env.policy,{now,run:fakeRunner({
    [targetA]:passA,[targetB]:lowB}),log:()=>{}});
   assert.equal(first.domains.security.active.target,targetA);
   assert.equal(first.domains.security.active.accuracy,1);
   const promoted=benchmarkCycle(env.policy,{now:now+1000,run:fakeRunner({
    [targetA]:report(targetA,24),[targetB]:report(targetB,27)}),log:()=>{}});
   assert.equal(promoted.domains.security.active.target,targetB);
   assert.equal(promoted.domains.security.previous.target,targetA);
   const reverted=benchmarkCycle(env.policy,{now:now+2000,run:fakeRunner({
    [targetA]:report(targetA,27),[targetB]:report(targetB,10)}),log:()=>{}});
   assert.equal(reverted.domains.security.active.target,targetA);
   const disabled=benchmarkCycle(env.policy,{now:now+3000,
    run:fakeRunner({[targetA]:report(targetA,4),[targetB]:report(targetB,3)}),
    log:()=>{}});
   assert.equal(disabled.domains.security.active,null);
   const disk=JSON.parse(readFileSync(join(env.stateDir,'active-routing.json'),'utf8'));
   assert.equal(disk.domains.security.active,null);
  }finally{env.dispose();}
 });
 it('leaves fail-closed deployment when grading subprocess crashes or fabricates evidence',()=>{
  const env=fixture();
  try{
   benchmarkCycle(env.policy,{now,run:fakeRunner({[targetA]:report(targetA)}),log:()=>{}});
   const tampered=report(targetA);tampered.results[0].verified=false;
   const fail=benchmarkCycle(env.policy,{now:now+1000,run:fakeRunner({[targetA]:tampered}),log:()=>{}});
   assert.equal(fail.domains.security.active,null);
   const recipe=join(env.stateDir,'recipe.json');
   writeFileSync(recipe,JSON.stringify({version:1,workspace:env.stateDir,permission_mode:'ask'}));
   assert.throws(()=>dispatchRecipe(env.policy,'security',recipe,{now:now+1000}),/No fresh/);
  }finally{env.dispose();}
 });
 it('keeps existing target when improvement is smaller than policy threshold',()=>{
  const env=fixture();
  try{
   const current=gradeReport(report(targetA,27),'security',targetA,3,now);
   const close=gradeReport(report(targetB,26),'security',targetB,3,now);
   const previous=chooseDeployment(env.policy,[current,close],null,now);
   const next=chooseDeployment(env.policy,[current,close],previous,now+500);
   assert.equal(next.domains.security.active.target,targetA);
   assert.equal(next.domains.security.reason,'verified_current');
  }finally{env.dispose();}
 });
 it('preserves the previous working routing on a crash before manifest commit',()=>{
  const env=fixture();
  try{
   const first=benchmarkCycle(env.policy,{now,run:fakeRunner({
    [targetA]:report(targetA,27),[targetB]:report(targetB,10)}),log:()=>{}});
   assert.equal(first.domains.security.active.target,targetA);
   const manifest=join(env.stateDir,'active-routing.json');
   const previousBytes=readFileSync(manifest);
   benchmarkCycle(env.policy,{now:now+1000,run:fakeRunner({
    [targetA]:report(targetA,24),[targetB]:report(targetB,27)}),log:()=>{}});
   // Simulate power loss BEFORE the atomic manifest rename. New content-
   // addressed evidence exists but can never destroy old evidence.
   writeFileSync(manifest,previousBytes);
   const recipe=join(env.stateDir,'task.json');
   writeFileSync(recipe,JSON.stringify({version:1,workspace:env.stateDir,permission_mode:'ask'}));
   const result=dispatchRecipe(env.policy,'security',recipe,{now,run:()=>({
    status:0,stdout:'{"run_id":"run-recovered"}'})});
   assert.equal(result.target,targetA);
  }finally{env.dispose();}
 });
 it('reclaims a dead worker lock, but never steals a live or mismatched lease',()=>{
  const env=fixture();
  try{
   const old=acquireCycleLock(env.stateDir,{pid:12345,now,alive:()=>false});
   assert.throws(()=>acquireCycleLock(env.stateDir,{
    pid:23456,now:now+1000,alive:()=>true}),/live or unverified/);
   const recovered=acquireCycleLock(env.stateDir,{
    pid:23456,now:now+2000,alive:()=>false});
   assert.throws(()=>old(),/ownership changed/);
   recovered();
   const fresh=acquireCycleLock(env.stateDir,{pid:34567,now:now+3000,alive:()=>false});
   fresh();
  }finally{env.dispose();}
 });

});

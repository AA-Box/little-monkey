import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,lstatSync,rmSync,existsSync}
 from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {fileURLToPath} from 'node:url';
import {servicePlist,manageService,SERVICE_LABEL} from './swarm-specialist-service.mjs';

const worker=fileURLToPath(new URL('./swarm-specialist-continuous-loop.mjs',import.meta.url));
function setup(){
 const home=mkdtempSync(join(tmpdir(),'lm-swarm-launchd-'));
 const state=join(home,'state');mkdirSync(state);
 const policy=join(home,'policy.json');
 writeFileSync(policy,JSON.stringify({
  version:1,monkeyBin:process.execPath,stateDir:state,
  targets:['ollama:reviewed-a'],domains:['security'],
  repetitions:2,intervalSeconds:3600,timeoutMs:30000,
  minAccuracy:0.8,minImprovement:0.05,maxAgeHours:48,
 }));
 return {home,state,policy,plist:join(home,'Library','LaunchAgents',
  SERVICE_LABEL+'.plist'),close:()=>rmSync(home,{force:true,recursive:true})};
}
describe('macOS native user-level specialist learning supervisor contract',()=>{
 it('escapes plist values without executing shell input',()=>{
  const plist=servicePlist({
   nodePath:'/opt/bin/node',scriptPath:'/my path/worker.mjs',
   policyPath:'/my path/policy & approved.json',stateDir:'/my path/state',
  });
  assert.match(plist,/policy &amp; approved\.json/);
  assert.match(plist,/<key>KeepAlive<\/key>/);
  assert.match(plist,/com\.littlemonkey\.verified-specialist-learning/);
  assert.doesNotMatch(plist,/<key>Shell<\/key>/);
 });
 it('installs once with user-only permissions and bootstraps with argument vector',()=>{
  const x=setup(),calls=[];
  try{
   const run=(bin,args)=>{
    calls.push({bin,args});return {status:0,stdout:''};
   };
   const params={platform:'darwin',uid:501,home:x.home,
    scriptPath:worker,nodePath:process.execPath,run};
   const installed=manageService('install',x.policy,params);
   assert.equal(installed.installed,true);
   assert.equal(calls[0].bin,'/bin/launchctl');
   assert.deepEqual(calls[0].args.slice(0,2),['bootstrap','gui/501']);
   assert.equal(lstatSync(x.plist).mode&0o777,0o600);
   assert.match(readFileSync(x.plist,'utf8'),/--confirm-unattended-local-evaluation/);
   assert.match(readFileSync(x.plist,'utf8'),/--policy/);
   assert.throws(()=>manageService('install',x.policy,params),/already exists/);
   assert.deepEqual(manageService('status',null,params),{installed:true,running:true});
   const uninstalled=manageService('uninstall',null,params);
   assert.equal(uninstalled.installed,false);
   assert.equal(existsSync(x.plist),false);
  }finally{x.close();}
 });
 it('never leaves a registered plist behind after launchctl bootstrap failure',()=>{
  const x=setup();
  try{
   assert.throws(()=>manageService('install',x.policy,{
    platform:'darwin',uid:501,home:x.home,nodePath:process.execPath,
    scriptPath:worker,run:()=>({status:1,stderr:'bootstrap unavailable'})
   }),/launchd refused/);
   assert.equal(existsSync(x.plist),false);
  }finally{x.close();}
 });
 it('refuses to uninstall an unrelated service or operate as root',()=>{
  const x=setup();
  try{
   mkdirSync(join(x.home,'Library','LaunchAgents'),{recursive:true});
   writeFileSync(x.plist,'<?xml version="1.0"?><plist/>');
   assert.throws(()=>manageService('uninstall',null,{
    platform:'darwin',uid:501,home:x.home,run:()=>({status:0})}),/unrelated/);
   assert.equal(existsSync(x.plist),true);
   assert.throws(()=>manageService('status',null,{
    platform:'darwin',uid:0,home:x.home,run:()=>({status:0})}),/non-root/);
  }finally{x.close();}
 });
});

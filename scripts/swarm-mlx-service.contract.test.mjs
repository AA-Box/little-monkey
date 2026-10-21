import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {mlxLaunchAgent,manageMlxService,SERVICE_LABEL} from './swarm-mlx-service.mjs';

test('MLX user LaunchAgent is escape-safe, local-only, non-root, and has no shell',()=>{
 const config={nodePath:'/absolute/node',scriptPath:'/absolute/runner',
  policyPath:'/absolute/supervisor-policy.json',
  stateDir:'/Users/example/A & B'};
 const plist=mlxLaunchAgent(config);
 assert.match(plist,/com\.littlemonkey\.verified-mlx-serving/);
 assert.match(plist,/A &amp; B/);
 assert.match(plist,/--confirm-unattended-mlx-deployment/);
 assert.match(plist,/<key>KeepAlive<\/key>/);
 assert.doesNotMatch(plist,/<key>EnvironmentVariables<\/key>/);
 assert.throws(()=>mlxLaunchAgent({...config,scriptPath:'relative'}),/absolute/);
 assert.throws(()=>mlxLaunchAgent({...config,policyPath:'/foo\nbar'}),/safe/);
});

test('MLX service lifecycle creates and deletes only its own current-user service',()=>{
 const root=mkdtempSync(join(tmpdir(),'lm-mlx-launchagent-'));
 try{
  const home=join(root,'home'),model=join(root,'base'),inbox=join(root,'inbox'),
   outputRoot=join(root,'outputs'),stateDir=join(root,'state');
  for(const folder of [home,model,inbox,outputRoot,stateDir])mkdirSync(folder);
  const trainingPolicy=join(root,'learning.json');
  writeFileSync(trainingPolicy,JSON.stringify({version:1,model,
   inbox,outputRoot,python:process.execPath,iters:20,maxTestPpl:20,
   pollSeconds:60,batchLimit:1,fuseForImport:true}));
  const policy=join(root,'supervisor.json');
  writeFileSync(policy,JSON.stringify({version:1,trainingPolicy,
   monkeyBin:process.execPath,stateDir,listenPort:8765,
   backendPort:8766,probePort:8767,modelAlias:'reviewed-local',
   domains:['security'],repetitions:2,timeoutMs:30000,
   checkSeconds:60,minAccuracy:0.8,minImprovement:0.05}));
  let running=false;
  const calls=[];
  const run=(binary,args)=>{
   calls.push({binary,args});
   if(args[0]==='bootstrap'){running=true;return {status:0};}
   if(args[0]==='bootout'){running=false;return {status:0};}
   return {status:running?0:1};
  };
  const options={platform:'darwin',uid:501,home,run};
  assert.throws(()=>manageMlxService('install',policy,{...options,uid:0}),/non-root/);
  const installed=manageMlxService('install',policy,options);
  assert.equal(installed.installed,true);
  assert.equal(installed.running,true);
  const data=readFileSync(installed.plist,'utf8');
  assert.match(data,/little-monkey-current-user-verified-mlx-serving-v1/);
  assert.match(data,/--confirm-unattended-mlx-deployment/);
  assert.equal(manageMlxService('status',null,options).running,true);
  assert.throws(()=>manageMlxService('install',policy,options),/uninstalled/);
  const removed=manageMlxService('uninstall',null,options);
  assert.equal(removed.installed,false);
  assert.equal(manageMlxService('status',null,options).installed,false);
  assert.ok(calls.some(x=>x.args[0]==='bootstrap'));
  assert.ok(calls.some(x=>x.args[0]==='bootout'));
  assert.ok(!calls.some(x=>x.binary==='/bin/sh'));
 }finally{rmSync(root,{recursive:true,force:true});}
});

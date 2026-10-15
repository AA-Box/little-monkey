import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {parseServingPolicy,shouldPromote,candidateReceipt} from './swarm-mlx-verified-serving.mjs';

test('MLX supervised deployment requires approved fused training and strict localhost policy',()=>{
 const root=mkdtempSync(join(tmpdir(),'lm-verified-mlx-'));
 try{
  const model=join(root,'base'),inbox=join(root,'inbox'),outputRoot=join(root,'outputs'),
    stateDir=join(root,'state');
  for(const dir of [model,inbox,outputRoot,stateDir])mkdirSync(dir);
  const trainingPolicy=join(root,'learning.json');
  writeFileSync(trainingPolicy,JSON.stringify({version:1,model,python:process.execPath,
   inbox,outputRoot,iters:12,maxTestPpl:20,pollSeconds:60,batchLimit:1,fuseForImport:true}));
  const settings={version:1,trainingPolicy,stateDir,monkeyBin:process.execPath,
   listenPort:8765,backendPort:8766,probePort:8767,modelAlias:'little-monkey-mlx',
   domains:['security','backend'],repetitions:2,timeoutMs:30000,
   checkSeconds:60,minAccuracy:0.8,minImprovement:0.05};
  const policy=parseServingPolicy(settings);
  assert.equal(policy.training.fuseForImport,true);
  assert.equal(policy.backendPort,8766,'trusted outbound MLX backend port must be fixed');
  assert.deepEqual(policy.domains,['security','backend']);
  for(const invalid of [
   {...settings,listenPort:settings.backendPort},
   {...settings,listenPort:80},
   {...settings,backendPort:99999},
   {...settings,backendPort:8768}, // Reject other loopback services even on valid ports
   {...settings,probePort:settings.listenPort},
   {...settings,domains:['security','security']},
   {...settings,minAccuracy:0.6},
   {...settings,modelAlias:'../unsafe'},
   {...settings,unknown:'accepted'},
  ])assert.throws(()=>parseServingPolicy(invalid));
  assert.throws(()=>candidateReceipt(policy,'../unapproved'));
  const candidate=join(outputRoot,'run-approved');
  mkdirSync(candidate);
  mkdirSync(join(candidate,'fused-model'));
  writeFileSync(join(candidate,'training-receipt.json'),JSON.stringify({
   version:1,engine:'mlx_lm',baseModel:model,
   status:'validated_fused_model_requires_manual_runtime_import',
   fusedModelPath:join(candidate,'fused-model'),
   fusedTestPerplexity:6,fusedWeights:[{name:'model.safetensors',sizeBytes:10,sha256:'f'.repeat(64)}],
  }));
  const receipt=candidateReceipt(policy,'approved');
  assert.equal(receipt.modelPath,join(candidate,'fused-model'));
  assert.equal(receipt.receiptSha256.length,64);
  writeFileSync(join(candidate,'training-receipt.json'),JSON.stringify({
   ...JSON.parse(requireErrorFreeReceiptPlaceholder()),status:'candidate_unreviewed'}));
  function requireErrorFreeReceiptPlaceholder(){
   return JSON.stringify({version:1,engine:'mlx_lm',baseModel:model,
    fusedModelPath:join(candidate,'fused-model'),fusedTestPerplexity:6,
    fusedWeights:[{name:'model.safetensors',sizeBytes:10,sha256:'f'.repeat(64)}]});
  }
  assert.throws(()=>candidateReceipt(policy,'approved'),/gates/);
 }finally{rmSync(root,{force:true,recursive:true});}
});

test('only independently improved non-regressed models can be promoted',()=>{
 const current={security:0.83,backend:0.92};
 assert.equal(shouldPromote(null,current,0.8,0.05),true);
 assert.equal(shouldPromote(current,{security:0.94,backend:0.93},0.8,0.05),true);
 assert.equal(shouldPromote(current,{security:0.90,backend:0.90},0.8,0.05),false);
 assert.equal(shouldPromote(current,{security:0.89,backend:0.92},0.8,0.1),false);
 assert.equal(shouldPromote(current,{security:0.94},0.8,0.05),false);
 assert.equal(shouldPromote(current,{security:1,backend:1.2},0.8,0.05),false);
});

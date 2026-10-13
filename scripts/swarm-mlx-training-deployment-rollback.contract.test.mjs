import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {createHash} from 'node:crypto';
import {evaluateSse,rollbackAfterFault,verifiedFusion} from './swarm-mlx-training-deployment-rollback.mjs';
const sha=x=>createHash('sha256').update(x).digest('hex');
const messages=(id)=>[
 {type:'started',request_id:id},
 {type:'text_delta',text:'Ready.'},
 {type:'completed',input_tokens:10,output_tokens:3,cached_input_tokens:0},
].map(x=>'data: '+JSON.stringify(x)+'\n').join('');
describe('native MLX training -> live deployment -> fault rollback gates',()=>{
 it('accepts a real completed native HTTP generation only',()=>{
  assert.deepEqual(evaluateSse(messages('req1'),'req1'),{outputChars:6,inputTokens:10,outputTokens:3});
  assert.throws(()=>evaluateSse(messages('req1'),'other'),/target-specific/);
  assert.throws(()=>evaluateSse(messages('req1')+messages('req1'),'req1'),/target-specific/);
  assert.throws(()=>evaluateSse(messages('req1').replace('Ready.',''),'req1'),/empty output/);
  assert.throws(()=>evaluateSse(messages('req1').replace('"type":"text_delta"','"type":"error"'),'req1'),/target-specific/);
 });
 it('requires exact evaluated model bytes and held-out fused parity before switching',()=>{
  const root=mkdtempSync(join(tmpdir(),'mlx-lifecycle-proof-'));
  try{
   const output=join(root,'run'),model=join(output,'fused-model');mkdirSync(output);mkdirSync(model);
   const config='{"model_type":"mock"}',weight='test-shard';
   writeFileSync(join(model,'config.json'),config);
   writeFileSync(join(model,'model.safetensors'),weight);
   const receipt={version:1,engine:'mlx_lm',status:'validated_fused_model_requires_manual_runtime_import',
    baseModel:'/base/model',testPerplexity:2.0,fusedTestPerplexity:2.02,maxTestPpl:3,
    fusedModelPath:model,fusedConfigSha256:sha(config),fusedWeights:[{name:'model.safetensors',sha256:sha(weight),sizeBytes:weight.length}]};
   const receiptPath=join(output,'training-receipt.json');
   writeFileSync(receiptPath,JSON.stringify(receipt));
   assert.equal(verifiedFusion(output,'/base/model').model,model);
   assert.throws(()=>verifiedFusion(output,'/wrong/model'),/unverified/);
   writeFileSync(join(model,'model.safetensors'),'tampered');
   assert.throws(()=>verifiedFusion(output,'/base/model'),/shards changed/);
  }finally{rmSync(root,{recursive:true,force:true});}
 });
 it('switches from candidate to the verified previous model after injected live incident',async()=>{
  const calls=[],receipt={};
  await rollbackAfterFault({
   activate:async model=>{calls.push('start:'+model);return {model};},
   probe:async instance=>{calls.push('probe:'+instance.model);return {outputChars:6,outputTokens:3};},
   deactivate:async instance=>{calls.push('stop:'+instance.model);},
  },'/base','/fused',receipt);
  assert.deepEqual(calls,['start:/fused','probe:/fused','stop:/fused','start:/base','probe:/base','stop:/base']);
  assert.equal(receipt.rollback.verified,true);
  assert.equal(receipt.rollback.restoredModel,'/base');
 });
 it('fails closed when the rollback canary fails',async()=>{
  const receipt={};
  await assert.rejects(()=>rollbackAfterFault({
   activate:async model=>({model}),probe:async instance=>instance.model==='/base'
    ?{outputChars:0}:{outputChars:6},deactivate:async()=>{},
  },'/base','/fused',receipt),/Rollback was not independently verified/);
  assert.equal(receipt.rollback.verified,false);
 });
 it('does not count failed candidate inference as successful deployment even if restored',async()=>{
  const receipt={};
  await assert.rejects(()=>rollbackAfterFault({
   activate:async model=>({model}),probe:async instance=>instance.model==='/fused'
    ?{outputChars:0}:{outputChars:6,outputTokens:2},deactivate:async()=>{},
  },'/base','/fused',receipt),/Candidate inference failed, but rollback/);
  assert.equal(receipt.rollback.verified,true);
 });
});

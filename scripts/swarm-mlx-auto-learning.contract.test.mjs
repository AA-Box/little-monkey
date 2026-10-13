import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {inspectDataset} from './swarm-mlx-lora.mjs';
import {parseLearningPolicy,approvedBatch,claimBatch,learnOnce} from './swarm-mlx-auto-learning.mjs';
const create=()=>{
 const root=mkdtempSync(join(tmpdir(),'lm-learning-'));
 const model=join(root,'model'),inbox=join(root,'inbox'),outputRoot=join(root,'runs');
 mkdirSync(model);mkdirSync(inbox);mkdirSync(outputRoot);
 writeFileSync(join(model,'config.json'),'{"model_type":"qwen2"}');
 const batch=join(inbox,'operator-batch-1');mkdirSync(batch);
 for(const [kind,count] of [['train',8],['valid',3],['test',3]]){
  writeFileSync(join(batch,kind+'.jsonl'),
   Array.from({length:count},(_,i)=>JSON.stringify({text:kind+' independent sentence '+i})).join('\n'));
 }
 const observed=inspectDataset(batch).files;
 writeFileSync(join(batch,'ready.json'),JSON.stringify({version:1,approved:true,
  note:'Operator reviewed local independent train-valid-test sets',
  files:Object.fromEntries(Object.entries(observed).map(([k,v])=>[k,v.sha256]))}));
 const value={version:1,model,inbox,outputRoot,python:process.execPath,
  iters:25,maxTestPpl:20,pollSeconds:60,batchLimit:2,fuseForImport:true};
 return {root,batch,policy:parseLearningPolicy(JSON.stringify(value))};
};
describe('real opt-in auto-learning; no synthetic test results accepted',()=>{
 it('accepts only SHA256-attested disjoint operator batches',()=>{
  const x=create();try{
   assert.equal(approvedBatch(x.batch,'operator-batch-1').train.count,8);
   writeFileSync(join(x.batch,'train.jsonl'),'{"text":"tampered"}');
   assert.throws(()=>approvedBatch(x.batch,'operator-batch-1'));
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('ignores incomplete batches and quarantines rejected operator approvals without GPU subprocess',()=>{
  const x=create();try{
   const waiting=join(x.policy.inbox,'batch-unfinished');
   mkdirSync(waiting);writeFileSync(join(waiting,'train.jsonl'),'{"text":"in progress"}');
   const noApproval=learnOnce(x.policy,{execute:()=>{throw Error('GPU started');},log:()=>{}});
   assert.equal(noApproval[0].status,'failed_receipt_requires_operator_review');
   // Existing valid batch was claimed. An unapproved batch is safely ignored.
   assert.equal(readFileSync(join(waiting,'train.jsonl'),'utf8'),'{"text":"in progress"}');
   writeFileSync(join(waiting,'ready.json'),'{"version":1,"approved":true}');
   const quarantined=learnOnce(x.policy,{execute:()=>{throw Error('GPU started');},log:()=>{}});
   assert.equal(quarantined[0].status,'rejected_requires_operator_review');
   assert.match(readFileSync(join(x.policy.outputRoot,'claim-batch-unfinished.json'),'utf8'),/invalid_approval/);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('writes a single permanent claim before dispatch, never retrains same batch',()=>{
  const x=create();try{
   const c=claimBatch(x.policy.outputRoot,'operator-batch-1',{train:{sha256:'a'.repeat(64)}});
   assert.match(readFileSync(c,'utf8'),/operator-batch-1/);
   assert.throws(()=>claimBatch(x.policy.outputRoot,'operator-batch-1',{}),/EEXIST/);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('never mistakes an exit code for evaluated evidence and never repeats failed training',()=>{
  const x=create();try{
   let calls=0;
   const execute=()=>{calls++;return {status:0};};
   const logs=[];
   const first=learnOnce(x.policy,{execute,log:(line)=>logs.push(line)});
   assert.equal(calls,1);
   assert.equal(first[0].status,'failed_receipt_requires_operator_review');
   const second=learnOnce(x.policy,{execute,log:()=>{}});
   assert.deepEqual(second,[]);
   assert.equal(calls,1);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('rejects nonlocal, overlapping, arbitrary policy fields and unsafe thresholds',()=>{
  const x=create();try{
   const v=JSON.parse(JSON.stringify(x.policy));
   assert.throws(()=>parseLearningPolicy(JSON.stringify({...v,untrusted:'yes'})));
   assert.throws(()=>parseLearningPolicy(JSON.stringify({...v,model:v.outputRoot})));
   assert.throws(()=>parseLearningPolicy(JSON.stringify({...v,maxTestPpl:0})));
   assert.throws(()=>parseLearningPolicy(JSON.stringify({...v,python:'python3'})));
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
});

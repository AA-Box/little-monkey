import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {trainingPlan,inspectDataset,testPpl} from './swarm-mlx-lora.mjs';
function fixtures(){
 const root=mkdtempSync(join(tmpdir(),'swarm-adapter-contract-'));
 const model=join(root,'model'),data=join(root,'data');
 mkdirSync(model);mkdirSync(data);
 writeFileSync(join(model,'config.json'),'{}');
 const lines=(name,n)=>Array.from({length:n},(_,i)=>JSON.stringify({text:name+' verified example '+i})).join('\n')+'\n';
 for(const [kind,count] of [['train',8],['valid',3],['test',3]])
   writeFileSync(join(data,kind+'.jsonl'),lines(kind,count));
 return {root,model,data,options:{model,data,output:join(root,'adapter-1'),
   iters:'20',maxPpl:'15'},lines};
}
describe('real MLX adapter quality gate preflight',()=>{
 it('parses real MLX test loss and perplexity and rejects ambiguous or nonfinite reports',()=>{
  assert.equal(testPpl('Loading...\nTest loss 1.234, Test ppl 3.435.\n'),3.435);
  assert.equal(testPpl('Test loss 2.1, Test ppl 8.17.\nTest loss 1.7, Test ppl 5.47.'),5.47);
  assert.throws(()=>testPpl('No verified evaluation'),/finite positive/);
  assert.throws(()=>testPpl('Test loss nan, Test ppl 0.000'),/finite positive/);
  assert.throws(()=>testPpl('Test ppl Infinity'),/finite positive/);
 });

 it('validates independent train/valid/test inputs and an unused output path',()=>{
  const x=fixtures();try{
   const plan=trainingPlan(x.options);
   assert.equal(plan.iterations,20);
   assert.equal(plan.files.train.count,8);
   assert.equal(plan.files.valid.count,3);
   assert.equal(plan.files.test.count,3);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('refuses split leakage and silently overwriting approved adapters',()=>{
  const x=fixtures();try{
   writeFileSync(join(x.data,'test.jsonl'),x.lines('train',3));
   assert.throws(()=>inspectDataset(x.data),/leakage/);
   writeFileSync(join(x.data,'test.jsonl'),x.lines('test',3));
   mkdirSync(x.options.output);
   assert.throws(()=>trainingPlan(x.options),/new absolute/);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('rejects missing source evidence, impossible thresholds and training counts',()=>{
  const x=fixtures();try{
   assert.throws(()=>trainingPlan({...x.options,maxPpl:'0'}),/ceiling/);
   assert.throws(()=>trainingPlan({...x.options,iters:'10000000'}),/iterations/);
   assert.throws(()=>trainingPlan({...x.options,previous:join(x.root,'missing')}),/both required/);
   assert.throws(()=>trainingPlan({...x.options,output:'.'}),/new absolute/);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
});

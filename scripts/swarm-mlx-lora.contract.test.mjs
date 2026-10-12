import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,mkdirSync,writeFileSync,rmSync,symlinkSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join} from 'node:path';
import {trainingPlan,inspectDataset,testPpl,safeRegularBytes,safeRegularSha256,safeRegularSha256WithSize,fusedWeightsManifest,digest,snapshotVerifiedDataset} from './swarm-mlx-lora.mjs';
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
 it('executes from immutable private snapshots matching the preflight receipt',()=>{
  const x=fixtures();
  try{
   const plan=trainingPlan(x.options);
   const destination=join(x.root,'verified-snapshot');
   snapshotVerifiedDataset(x.data,destination,plan.files);
   for(const kind of ['train','valid','test']){
    assert.equal(digest(safeRegularBytes(join(destination,kind+'.jsonl'),'snapshot')),
      plan.files[kind].sha256);
   }
   writeFileSync(join(x.data,'train.jsonl'),x.lines('changed',8));
   assert.throws(()=>snapshotVerifiedDataset(x.data,join(x.root,'changed-snapshot'),plan.files),
     /Dataset changed since preflight/);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });

 it('hashes every fused shard and detects tampering and symlinked evidence',()=>{
  const x=fixtures();try{
   const folder=join(x.root,'fused-model');mkdirSync(folder);
   writeFileSync(join(folder,'model-00001-of-00002.safetensors'),'first shard');
   writeFileSync(join(folder,'model-00002-of-00002.safetensors'),'second shard');
   const receipt=fusedWeightsManifest(folder);
   assert.equal(receipt.length,2);
   assert.equal(receipt[0].sha256,digest('first shard'));
   assert.equal(receipt[1].sizeBytes,Buffer.byteLength('second shard'));
   assert.deepEqual(fusedWeightsManifest(folder),receipt);
   writeFileSync(join(folder,'model-00001-of-00002.safetensors'),'changed shard');
   assert.notDeepEqual(fusedWeightsManifest(folder),receipt);
   symlinkSync(join(x.model,'config.json'),join(folder,'link.safetensors'));
   assert.throws(()=>fusedWeightsManifest(folder),/symlink|ELOOP|regular/i);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });
 it('reads exactly the validated descriptor, rejecting symlinked input and weights',()=>{
  const x=fixtures();try{
   const weights=join(x.root,'adapter.safetensors');
   writeFileSync(weights,'fixed deterministic model bytes');
   assert.equal(safeRegularSha256(weights,'model weights'),digest('fixed deterministic model bytes'));
   const config=join(x.model,'config.json');
   assert.equal(safeRegularBytes(config,'config').toString('utf8'),'{}');
   const symlink=join(x.root,'injected.jsonl');
   symlinkSync(join(x.data,'train.jsonl'),symlink);
   assert.throws(()=>safeRegularBytes(symlink,'test'),/symlink|ELOOP|regular/i);
   assert.throws(()=>safeRegularSha256(symlink,'test'),/symlink|ELOOP|regular/i);
  }finally{rmSync(x.root,{recursive:true,force:true});}
 });

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

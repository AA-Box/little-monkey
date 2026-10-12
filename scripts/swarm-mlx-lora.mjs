#!/usr/bin/env node
/**
 * Opt-in Apple Silicon MLX LoRA/QLoRA training with held-out gates.
 * No user file is sent to an API, no active runtime or existing adapter is
 * modified, and only a newly created directory is written.
 *
 * Run against an already installed LOCAL MLX base model, with mlx-lm[train]
 * available in the selected Python interpreter.
 */
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {constants,openSync,closeSync,fstatSync,readSync,existsSync,lstatSync,mkdirSync,readFileSync,readdirSync,realpathSync,writeFileSync} from 'node:fs';
import {dirname,join,isAbsolute,resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

export function digest(bytes){return createHash('sha256').update(bytes).digest('hex');}
/** Bind checks and reads to the same nofollow file descriptor to prevent
 * pathname swaps between lstat and read. Hash large weights in chunks. */
function safeDescriptor(path,label,limit,consumer){
 const fd=openSync(path,constants.O_RDONLY|(constants.O_NOFOLLOW??0));
 try{
  const stat=fstatSync(fd);
  if(!stat.isFile()||stat.size<0||stat.size>limit)
   throw Error(label+' must be a bounded regular file');
  const value=consumer(fd,stat);
  const later=fstatSync(fd);
  if(later.size!==stat.size||later.mtimeMs!==stat.mtimeMs)
   throw Error(label+' changed during read');
  return value;
 }finally{closeSync(fd);}
}
export function safeRegularBytes(path,label,maxBytes=8*1024*1024){
 return safeDescriptor(path,label,maxBytes,(fd,stat)=>{
  const data=readFileSync(fd);
  if(data.length!==stat.size)throw Error(label+' changed during read');
  return data;
 });
}
export function safeRegularSha256(path,label,maxBytes=8*1024*1024*1024){
 return safeDescriptor(path,label,maxBytes,(fd,stat)=>{
  const hash=createHash('sha256'),buffer=Buffer.allocUnsafe(64*1024);
  for(let offset=0;offset<stat.size;){
   const got=readSync(fd,buffer,0,Math.min(buffer.length,stat.size-offset),offset);
   if(got<=0)throw Error(label+' changed during hash');
   hash.update(buffer.subarray(0,got));offset+=got;
  }
  return hash.digest('hex');
 });
}

const argument=name=>{const i=process.argv.indexOf(name);return i===-1?null:process.argv[i+1]??null;};
export function checkedLocalDir(path,label){
 if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path))throw Error(label+' requires an absolute path');
 if(!existsSync(path)||!lstatSync(path).isDirectory()||lstatSync(path).isSymbolicLink())
   throw Error(label+' must already be a real local directory');
 return realpathSync(path);
}
function records(path,label){
 const contents=safeRegularBytes(path,label,8*1024*1024);
 const lines=contents.toString('utf8').trim().split('\n');
 if(lines.length<3||lines.length>10000)throw Error(label+' requires 3–10,000 rows');
 const unique=new Set();
 for(const line of lines){
  if(line.length>32_000)throw Error(label+' contains an oversized row');
  const parsed=JSON.parse(line);
  if(!parsed||typeof parsed!=='object'||Array.isArray(parsed)||
     !((typeof parsed.text==='string'&&parsed.text.trim())||
       (Array.isArray(parsed.messages)&&parsed.messages.length>0)))
    throw Error(label+' requires MLX text or chat JSONL records');
  const key=digest(JSON.stringify(parsed));
  if(unique.has(key))throw Error(label+' contains repeated examples');
  unique.add(key);
 }
 return {hash:digest(contents),unique,count:lines.length};
}
export function inspectDataset(directory){
 const files={};
 const all=new Set();
 for(const kind of ['train','valid','test']){
  const file=records(join(directory,kind+'.jsonl'),kind+'.jsonl');
  if(kind==='train'&&file.count<8)throw Error('Training requires at least eight examples');
  for(const hash of file.unique){
   if(all.has(hash))throw Error('Train/validation/test leakage detected: identical row in two splits');
   all.add(hash);
  }
  files[kind]={sha256:file.hash,count:file.count};
 }
 return {files,all};
}
/** The MLX process sees only verified private byte-for-byte snapshots.
 * Never reopen user-controlled original JSONL paths after preflight. */
export function snapshotVerifiedDataset(source,folder,expected){
 mkdirSync(folder,{mode:0o700});
 for(const [name,identity] of Object.entries(expected)){
  if(!['train','valid','test'].includes(name)||!identity||
     typeof identity.sha256!=='string'||!/^[0-9a-f]{64}$/.test(identity.sha256))
    throw Error('Invalid dataset receipt');
  const bytes=safeRegularBytes(join(source,name+'.jsonl'),name+'.jsonl');
  if(digest(bytes)!==identity.sha256)throw Error('Dataset changed since preflight: '+name);
  writeFileSync(join(folder,name+'.jsonl'),bytes,{mode:0o600,flag:'wx'});
 }
 return folder;
}
export function testPpl(output){
 const match=[...output.matchAll(/Test\s+ppl\s+([0-9]+(?:\.[0-9]+)?)/ig)].at(-1);
 const n=match?Number(match[1]):NaN;
 if(!Number.isFinite(n)||n<=0)throw Error('MLX test did not report a finite positive held-out perplexity');
 return n;
}
function mlxCommand(python,command,args,dir,log){
 const result=spawnSync(python,['-m','mlx_lm',command,...args],{
  cwd:dir,encoding:'utf8',timeout:2*60*60*1000,maxBuffer:12*1024*1024,
  env:{...process.env,PYTHONUNBUFFERED:'1'},
 });
 const output=(result.stdout??'')+'\n'+(result.stderr??'');
 writeFileSync(log,output,{mode:0o600,flag:'wx'});
 if(result.error||result.status!==0)throw Error('MLX command failed; inspect protected log '+log);
 return output;
}
const mlx=(python,args,dir,log)=>mlxCommand(python,'lora',args,dir,log);
function manifest(output,value){
 writeFileSync(join(output,'training-receipt.json'),JSON.stringify(value,null,2)+'\n',
   {mode:0o600,flag:'wx'});
}
export function trainingPlan(opts){
 const model=checkedLocalDir(opts.model,'Base model'), data=checkedLocalDir(opts.data,'Training data');
 const inspected=inspectDataset(data);
 const output=opts.output;
 if(typeof output!=='string'||!isAbsolute(output)||/[\x00-\x1f]/.test(output)||
    existsSync(output))throw Error('Output must be a new absolute directory, never an existing adapter');
 const parent=checkedLocalDir(dirname(output),'Adapter output parent');
 if(resolve(output)!==join(parent,output.split(/[\\/]/).at(-1)))throw Error('Output path must be under a real local parent');
 const n=Number(opts.iters);
 const maxPpl=Number(opts.maxPpl);
 if(!Number.isSafeInteger(n)||n<10||n>5000)throw Error('Training iterations must be between 10 and 5,000');
 if(!Number.isFinite(maxPpl)||maxPpl<=0||maxPpl>100)
   throw Error('Operator must declare a finite held-out perplexity acceptance ceiling (0–100)');
 let previous=null, regression=null,regressionSha256=null;
 if(opts.previous||opts.regression){
  if(!opts.previous||!opts.regression)throw Error('Previous adapter and held-out regression directory are both required');
  previous=checkedLocalDir(opts.previous,'Previous approved adapter');
  if(!existsSync(join(previous,'adapters.safetensors'))||
     !existsSync(join(previous,'adapter_config.json')))
     throw Error('Previous adapter lacks MLX adapter weights or configuration');
  regression=checkedLocalDir(opts.regression,'Regression dataset');
  const rows=records(join(regression,'test.jsonl'),'regression test.jsonl');
  regressionSha256=rows.hash;
  for(const hash of rows.unique)if(inspected.all.has(hash))
    throw Error('Regression holdout overlaps training, validation or new test data');
 }
 return {model,data,output,iterations:n,maxTestPpl:maxPpl,
  ...(previous?{previous,regression,regressionSha256}:{}),
  files:inspected.files};
}
async function main(){
 const opts={model:argument('--model'),data:argument('--data'),output:argument('--output'),
  iters:argument('--iters')??'200',maxPpl:argument('--max-test-ppl'),
  previous:argument('--previous-adapter'),regression:argument('--regression-data')};
 const plan=trainingPlan(opts);
 if(process.argv.includes('--validate-only')){
  console.log(JSON.stringify({status:'data_validated_no_training',splits:plan.files,
    model:plan.model,output:plan.output}));return;
 }
 if(!process.argv.includes('--confirm-local-training'))
   throw Error('Use --confirm-local-training only after reviewing private data, target and destination');
 if(process.platform!=='darwin'||process.arch!=='arm64')
   throw Error('Real MLX adapter training requires macOS on Apple Silicon');
 const python=argument('--python')??'python3';
 if(python.includes('\0')||/[\r\n]/.test(python))throw Error('Invalid Python executable');
 mkdirSync(plan.output,{mode:0o700,recursive:false});
 const adapter=join(plan.output,'adapter');
 const result={version:1,status:'failed_or_not_verified',engine:'mlx_lm',
   baseModel:plan.model,modelConfigSha256:digest(safeRegularBytes(join(plan.model,'config.json'),'base model config')),
   createdAt:new Date().toISOString(),data:plan.files,iterations:plan.iterations,
   maxTestPpl:plan.maxTestPpl,previousAdapter:plan.previous??null};
 try{
  const workingData=snapshotVerifiedDataset(plan.data,join(plan.output,'data-snapshot'),plan.files);
  let workingRegression=null;
  if(plan.regression){
   const snapshotProof={test:{sha256:plan.regressionSha256}};
   workingRegression=snapshotVerifiedDataset(plan.regression,
     join(plan.output,'regression-snapshot'),snapshotProof);
  }
  const common=['--model',plan.model,'--data',workingData];
  const train=['--model',plan.model,'--train','--data',workingData,
    '--adapter-path',adapter,'--fine-tune-type','lora','--iters',String(plan.iterations),
    '--batch-size','1','--steps-per-eval','50'];
  if(plan.previous)train.push('--resume-adapter-file',join(plan.previous,'adapters.safetensors'));
  mlx(python,train,plan.output,join(plan.output,'train.private.log'));
  const weights=join(adapter,'adapters.safetensors');
  safeRegularBytes(join(adapter,'adapter_config.json'),'adapter configuration',512*1024);
  const adapterDigest=safeRegularSha256(weights,'trained adapter weights');
  const test=mlx(python,[...common,'--adapter-path',adapter,'--test', '--batch-size','1','--test-batches','-1'],
    plan.output,join(plan.output,'test.private.log'));
  const ppl=testPpl(test);
  result.testPerplexity=ppl;
  result.adapterSha256=adapterDigest;
  result.status='candidate_unreviewed';
  if(ppl>plan.maxTestPpl)throw Error('Independent held-out perplexity failed operator threshold');
  if(plan.previous){
   const old=mlx(python,['--model',plan.model,'--data',workingRegression,'--adapter-path',
     plan.previous,'--test','--batch-size','1','--test-batches','-1'],plan.output,join(plan.output,'old-regression.private.log'));
   const updated=mlx(python,['--model',plan.model,'--data',workingRegression,
     '--adapter-path',adapter,'--test','--batch-size','1','--test-batches','-1'],plan.output,join(plan.output,'new-regression.private.log'));
   result.regression={previousPpl:testPpl(old),newPpl:testPpl(updated)};
   const allowedIncrease=Number(argument('--max-regression-fraction')??'0.05');
   if(!Number.isFinite(allowedIncrease)||allowedIncrease<0||allowedIncrease>0.25)
     throw Error('Regression tolerance must be 0–25%');
   result.regression.maxFraction=allowedIncrease;
   if(result.regression.newPpl>result.regression.previousPpl*(1+allowedIncrease))
     throw Error('Historical regression holdout worsened beyond explicit gate');
  }
  result.status='validated_requires_manual_activation';
  if(process.argv.includes('--fuse-for-import')){
   if(!process.argv.includes('--confirm-fusion'))
    throw Error('Creating a new fused local model requires explicit --confirm-fusion');
   const fused=join(plan.output,'fused-model');
   mlxCommand(python,'fuse',['--model',plan.model,'--adapter-path',adapter,
     '--save-path',fused],plan.output,join(plan.output,'fuse.private.log'));
   if(!existsSync(fused)||!lstatSync(fused).isDirectory()||
      !existsSync(join(fused,'config.json'))||
      !readdirSync(fused).some(file=>file.endsWith('.safetensors')))
     throw Error('MLX fusion did not produce a complete local config/safetensors model');
   result.fusedModelPath=fused;
   result.fusedConfigSha256=digest(safeRegularBytes(join(fused,'config.json'),'fused model config'));
   // A file-level fusion receipt cannot prove inference quality. MLX can
   // generate a structurally valid but degraded fused model for some
   // quantization formats: independently evaluate the actual fused weights.
   const fusedReport=mlx(python,['--model',fused,'--data',workingData,
     '--adapter-path','','--test','--batch-size','1','--test-batches','-1'],
     plan.output,join(plan.output,'fused-test.private.log'));
   result.fusedTestPerplexity=testPpl(fusedReport);
   if(result.fusedTestPerplexity>plan.maxTestPpl ||
      result.fusedTestPerplexity>ppl*1.05)
     throw Error('Fused model failed held-out parity with its validated adapter');
   if(plan.regression){
     const fusedHistory=mlx(python,['--model',fused,'--data',workingRegression,
       '--adapter-path','','--test','--batch-size','1','--test-batches','-1'],
       plan.output,join(plan.output,'fused-regression.private.log'));
     result.fusedRegressionPpl=testPpl(fusedHistory);
     if(result.fusedRegressionPpl>result.regression.previousPpl*
       (1+result.regression.maxFraction))
       throw Error('Fused model failed historical-task regression gate');
   }
   result.status='validated_fused_model_requires_manual_runtime_import';
  }
  console.log(JSON.stringify({status:result.status,adapter,
    fusedModelPath:result.fusedModelPath??null,
    receipt:join(plan.output,'training-receipt.json'),
    heldOutPpl:ppl,regression:result.regression??null}));
 }catch(error){
  result.status='rejected_unverified';
  result.failure=String(error);
  process.exitCode=1;
  console.error(JSON.stringify({status:'not_verified',receipt:join(plan.output,'training-receipt.json'),
    error:'MLX training/evaluation failed or did not pass quality gates; inspect local protected receipt and logs'}));
 }finally{
  manifest(plan.output,result);
 }
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 main().catch(error=>{console.error(JSON.stringify({status:'not_executed',error:String(error)}));process.exitCode=1;});
}

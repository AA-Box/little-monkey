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
import {existsSync,lstatSync,mkdirSync,readFileSync,readdirSync,realpathSync,writeFileSync} from 'node:fs';
import {dirname,join,isAbsolute,resolve} from 'node:path';
import {fileURLToPath} from 'node:url';

export function digest(bytes){return createHash('sha256').update(bytes).digest('hex');}
const argument=name=>{const i=process.argv.indexOf(name);return i===-1?null:process.argv[i+1]??null;};
export function checkedLocalDir(path,label){
 if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path))throw Error(label+' requires an absolute path');
 if(!existsSync(path)||!lstatSync(path).isDirectory()||lstatSync(path).isSymbolicLink())
   throw Error(label+' must already be a real local directory');
 return realpathSync(path);
}
function records(path,label){
 if(!existsSync(path)||!lstatSync(path).isFile()||lstatSync(path).isSymbolicLink())
   throw Error(label+' must be a regular file, not a symlink');
 const contents=readFileSync(path);
 if(contents.length>8*1024*1024)throw Error(label+' exceeds 8 MiB');
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
function testPpl(output){
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
 let previous=null, regression=null;
 if(opts.previous||opts.regression){
  if(!opts.previous||!opts.regression)throw Error('Previous adapter and held-out regression directory are both required');
  previous=checkedLocalDir(opts.previous,'Previous approved adapter');
  if(!existsSync(join(previous,'adapters.safetensors'))||
     !existsSync(join(previous,'adapter_config.json')))
     throw Error('Previous adapter lacks MLX adapter weights or configuration');
  regression=checkedLocalDir(opts.regression,'Regression dataset');
  const rows=records(join(regression,'test.jsonl'),'regression test.jsonl');
  for(const hash of rows.unique)if(inspected.all.has(hash))
    throw Error('Regression holdout overlaps training, validation or new test data');
 }
 return {model,data,output,iterations:n,maxTestPpl:maxPpl,
  ...(previous?{previous,regression}:{}),files:inspected.files};
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
   baseModel:plan.model,modelConfigSha256:digest(readFileSync(join(plan.model,'config.json'))),
   createdAt:new Date().toISOString(),data:plan.files,iterations:plan.iterations,
   maxTestPpl:plan.maxTestPpl,previousAdapter:plan.previous??null};
 try{
  const common=['--model',plan.model,'--data',plan.data];
  const train=['--model',plan.model,'--train','--data',plan.data,
    '--adapter-path',adapter,'--fine-tune-type','lora','--iters',String(plan.iterations),
    '--batch-size','1','--steps-per-eval','50'];
  if(plan.previous)train.push('--resume-adapter-file',join(plan.previous,'adapters.safetensors'));
  mlx(python,train,plan.output,join(plan.output,'train.private.log'));
  const weights=join(adapter,'adapters.safetensors');
  if(!existsSync(weights)||!lstatSync(weights).isFile()||
     !existsSync(join(adapter,'adapter_config.json')))
    throw Error('MLX training exited without a real adapter weights/config pair');
  const test=mlx(python,[...common,'--adapter-path',adapter,'--test'],
    plan.output,join(plan.output,'test.private.log'));
  const ppl=testPpl(test);
  result.testPerplexity=ppl;
  result.adapterSha256=digest(readFileSync(weights));
  result.status='candidate_unreviewed';
  if(ppl>plan.maxTestPpl)throw Error('Independent held-out perplexity failed operator threshold');
  if(plan.previous){
   const old=mlx(python,['--model',plan.model,'--data',plan.regression,'--adapter-path',
     plan.previous,'--test'],plan.output,join(plan.output,'old-regression.private.log'));
   const updated=mlx(python,['--model',plan.model,'--data',plan.regression,
     '--adapter-path',adapter,'--test'],plan.output,join(plan.output,'new-regression.private.log'));
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
   result.fusedConfigSha256=digest(readFileSync(join(fused,'config.json')));
   result.status='validated_fused_model_requires_manual_runtime_import';
  }
  console.log(JSON.stringify({status:result.status,adapter,
    fusedModelPath:result.fusedModelPath??null,
    receipt:join(plan.output,'training-receipt.json'),
    heldOutPpl:ppl,regression:result.regression??null}));
 }catch(error){
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

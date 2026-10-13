#!/usr/bin/env node
/**
 * Opt-in, continuously running REAL MLX LoRA learning on Apple Silicon.
 *
 * Only datasets placed by the operator in an explicitly configured local
 * directory AND individually marked ready with exact train/valid/test hashes
 * are eligible. All training executes through the existing held-out, snapshot
 * and fused-regression quality gates, never via an improvised Python command.
 *
 * An exclusive per-batch claim is written BEFORE subprocess creation. Crashes
 * never silently retrain a possibly in-flight adapter. Operators must remove
 * a claim themselves if they deliberately want to repeat that batch.
 * Trained/fused models are NEVER registered/activated automatically.
 */
import {readdirSync,existsSync,mkdirSync,writeFileSync,
 rmdirSync,realpathSync,statSync,openSync,closeSync,fsyncSync} from 'node:fs';
import {join,isAbsolute,resolve,dirname} from 'node:path';
import {fileURLToPath} from 'node:url';
import {spawnSync} from 'node:child_process';
import {inspectDataset,checkedLocalDir,safeRegularBytes} from './swarm-mlx-lora.mjs';
const TOKEN=/^[A-Za-z0-9][A-Za-z0-9_.-]{0,78}$/;
const HEX=/^[0-9a-f]{64}$/;
function fail(msg){throw Error(msg);}
function assertPath(path,name){
 if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f]/.test(path))
  fail(name+' must be an absolute local filesystem path');
 return checkedLocalDir(path,name);
}
export function parseLearningPolicy(raw){
 if(typeof raw!=='string'||raw.length>16*1024)fail('Learning policy is oversized');
 const data=JSON.parse(raw);
 if(!data||typeof data!=='object'||Array.isArray(data)||
  Object.keys(data).sort().join(',')!=='batchLimit,fuseForImport,inbox,iters,maxTestPpl,model,outputRoot,pollSeconds,python,version')
  fail('Only the exact version-1 operator learning policy is accepted');
 if(data.version!==1||!Number.isSafeInteger(data.iters)||data.iters<10||data.iters>5000||
  !Number.isFinite(data.maxTestPpl)||data.maxTestPpl<=0||data.maxTestPpl>100||
  !Number.isSafeInteger(data.pollSeconds)||data.pollSeconds<60||data.pollSeconds>3600||
  !Number.isSafeInteger(data.batchLimit)||data.batchLimit<1||data.batchLimit>10||
  typeof data.fuseForImport!=='boolean')fail('Invalid bounded MLX learning policy');
 const model=assertPath(data.model,'MLX base model');
 const inbox=assertPath(data.inbox,'approved batch inbox');
 const outputRoot=assertPath(data.outputRoot,'MLX candidate output');
 if(inbox===outputRoot||inbox.startsWith(outputRoot+'/')||outputRoot.startsWith(inbox+'/'))
  fail('Training inbox and candidate output must be disjoint');
 if(typeof data.python!=='string'||!isAbsolute(data.python)||
  /[\x00-\x1f]/.test(data.python))fail('Python must be an absolute reviewed MLX environment binary');
 const python=realpathSync(data.python);
 if(!statSync(python).isFile())fail('Reviewed Python executable must be a regular file');
 if(model===outputRoot||outputRoot.startsWith(model+'/')||
   model===inbox||inbox.startsWith(model+'/'))
  fail('Do not train into or ingest the model source directory');
 return {version:1,model,inbox,outputRoot,python,
   iters:data.iters,maxTestPpl:data.maxTestPpl,pollSeconds:data.pollSeconds,
   batchLimit:data.batchLimit,fuseForImport:data.fuseForImport};
}
export function approvedBatch(directory,batch){
 if(!TOKEN.test(batch))fail('Invalid batch identifier');
 const inspected=inspectDataset(directory);
 const ready=JSON.parse(safeRegularBytes(join(directory,'ready.json'),
   'Approved batch receipt',16*1024).toString('utf8'));
 if(!ready||typeof ready!=='object'||Array.isArray(ready)||
    Object.keys(ready).sort().join(',')!=='approved,files,note,version'||
    ready.version!==1||ready.approved!==true||
    typeof ready.note!=='string'||ready.note.trim().length<5||ready.note.length>300||
    !ready.files||typeof ready.files!=='object'||Array.isArray(ready.files)||
    Object.keys(ready.files).sort().join(',')!=='test,train,valid')
   fail('A batch needs explicit operator approval and exact three-way source evidence');
 for(const key of ['train','valid','test']){
  if(typeof ready.files[key]!=='string'||!HEX.test(ready.files[key])||
     ready.files[key]!==inspected.files[key].sha256)
   fail('The approved dataset hash changed after the operator review: '+key);
 }
 return inspected.files;
}
export function claimBatch(outputRoot,batch,hashes,now=Date.now()){
 if(!TOKEN.test(batch)||!Number.isSafeInteger(now)||now<=0)
  fail('Invalid learning claim');
 const claim=join(outputRoot,'claim-'+batch+'.json');
 const payload={version:1,batch,claimedAt:now,hashes};
 // Durable no-retry marker BEFORE any Python/MLX process starts. A stale
 // claim requires an explicit human reset; it can never create duplicates.
 const fd=openSync(claim,'wx',0o600);
 try{
  writeFileSync(fd,JSON.stringify(payload)+'\n');
  // Claim durability must precede launching another copy of expensive GPU
  // training. On a crash, the operator must explicitly review the claim.
  fsyncSync(fd);
 }finally{closeSync(fd);}
 return claim;
}
export function learnOnce(policy,{execute=spawnSync,log=console.log}={}){
 const results=[];
 const entries=readdirSync(policy.inbox,{withFileTypes:true})
   .filter(e=>e.isDirectory()&&TOKEN.test(e.name))
   .sort((a,b)=>a.name.localeCompare(b.name)).slice(0,512);
 for(const entry of entries){
  if(results.length>=policy.batchLimit)break;
  const name=entry.name,source=join(policy.inbox,name);
  const out=join(policy.outputRoot,'run-'+name);
  const claim=join(policy.outputRoot,'claim-'+name+'.json');
  if(existsSync(out)||existsSync(claim))continue;
  // Dataset writers commonly create a folder before its last JSONL row or
  // approval file. Ignore these in-progress batches instead of killing the
  // continuous worker or accidentally treating them as approved.
  if(!existsSync(join(source,'ready.json')))continue;
  let hashes;
  try{
   hashes=approvedBatch(checkedLocalDir(source,'approved batch'),name);
  }catch(error){
   // Approval exists but has changed or is malformed. Quarantine this
   // batch durably. Never retrain it later just because files changed
   // underneath a running worker; explicit operator reset is required.
   claimBatch(policy.outputRoot,name,{rejected:'invalid_approval_or_changed_dataset'});
   log(JSON.stringify({batch:name,status:'rejected_requires_operator_review',
     error:String(error).slice(0,250)}));
   results.push({batch:name,status:'rejected_requires_operator_review'});
   continue;
  }
  claimBatch(policy.outputRoot,name,hashes);
  // From this point errors are terminal until an operator explicitly
  // reviews/clears claim-N.json; unattended retry is not safe.
  const args=[resolve(dirname(fileURLToPath(import.meta.url)),'swarm-mlx-lora.mjs'),
    '--model',policy.model,'--data',source,'--output',out,
    '--iters',String(policy.iters),'--max-test-ppl',String(policy.maxTestPpl),
    '--python',policy.python,'--confirm-local-training'];
  if(policy.fuseForImport)args.push('--fuse-for-import','--confirm-fusion');
  const outcome=execute(process.execPath,args,{encoding:'utf8',
    timeout:3*60*60*1000,maxBuffer:1024*1024,
    // No inherited source-control or AI-provider tokens to local training.
    env:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',
      TMPDIR:process.env.TMPDIR??'/tmp',PYTHONUNBUFFERED:'1'}});
  if(outcome?.error||outcome?.status!==0){
    log(JSON.stringify({batch:name,status:'failed_requires_operator_review',
      claim,error:outcome?.error?.message??'MLX training quality/reliability failure'}));
    results.push({batch:name,status:'failed_requires_operator_review'});
    continue;
  }
  let receipt;
  try{
    receipt=JSON.parse(safeRegularBytes(join(out,'training-receipt.json'),
      'MLX finished training receipt',512*1024).toString('utf8'));
  }catch{
    log(JSON.stringify({batch:name,status:'failed_receipt_requires_operator_review'}));
    results.push({batch:name,status:'failed_receipt_requires_operator_review'});
    continue;
  }
  const expected=policy.fuseForImport
    ?'validated_fused_model_requires_manual_runtime_import'
    :'validated_requires_manual_activation';
  if(receipt.version!==1||receipt.engine!=='mlx_lm'||
     receipt.baseModel!==policy.model||receipt.status!==expected ||
     !['train','valid','test'].every(key=>
       receipt.data?.[key]?.sha256===hashes[key].sha256)||
     !Number.isFinite(receipt.testPerplexity)||
     receipt.testPerplexity>policy.maxTestPpl||
     (policy.fuseForImport && (!Array.isArray(receipt.fusedWeights)||
      receipt.fusedWeights.length<1))){
    log(JSON.stringify({batch:name,status:'failed_receipt_requires_operator_review'}));
    results.push({batch:name,status:'failed_receipt_requires_operator_review'});
    continue;
  }
  log(JSON.stringify({batch:name,status:'validated_candidate_pending_native_activation',
   receipt:join(out,'training-receipt.json'),heldOutPpl:receipt.testPerplexity}));
  results.push({batch:name,status:'validated_candidate_pending_native_activation'});
 }
 return results;
}
function option(name){const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1];}
async function main(){
 if(!process.argv.includes('--confirm-auto-training'))fail('Real background training requires --confirm-auto-training');
 if(process.platform!=='darwin'||process.arch!=='arm64')
  fail('Automatic MLX fine tuning needs an operator-owned Apple Silicon Mac');
 const policyPath=option('--policy');
 if(!policyPath||!isAbsolute(policyPath))fail('Use --policy /absolute/approved-learning-policy.json');
 const policy=parseLearningPolicy(safeRegularBytes(policyPath,'Learning policy',16*1024).toString('utf8'));
 const lock=join(policy.outputRoot,'.swarm-learning-owner.lock');
 mkdirSync(lock,{mode:0o700});
 const once=process.argv.includes('--once');
 try{
  do{
   learnOnce(policy);
   if(once)break;
   await new Promise(done=>setTimeout(done,policy.pollSeconds*1000));
  }while(true);
 }finally{
  // A killed process leaves this lock in place. This is deliberate:
  // never steal a potentially still-running GPU training slot.
  rmdirSync(lock);
 }
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1]))
 main().catch(error=>{console.error(JSON.stringify({status:'learning_not_started',
  error:String(error)}));process.exitCode=1;});

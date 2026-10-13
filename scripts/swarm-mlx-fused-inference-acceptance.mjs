#!/usr/bin/env node
/** Real Apple Silicon fused-model load and inference acceptance.
 *
 * The independent held-out and regression oracle was already run by
 * swarm-mlx-lora.mjs. This additional step RECHECKS actual fused bytes and
 * loads those exact weights in MLX-LM to produce fresh nonempty inference.
 * Optionally probes a separately configured former model for rollback
 * viability. It NEVER installs, activates, replaces, or removes a model.
 */
import {spawnSync} from 'node:child_process';
import {createHash} from 'node:crypto';
import {constants,openSync,closeSync,fstatSync,readSync,lstatSync,realpathSync} from 'node:fs';
import {resolve,isAbsolute,join} from 'node:path';
import {fileURLToPath} from 'node:url';
const opt=k=>{const i=process.argv.indexOf(k);return i<0?null:process.argv[i+1]??null;};
const token=/^[0-9a-f]{64}$/;
const required=['--receipt','--python'];
const receiptPath=opt('--receipt'),python=opt('--python');
const prior=opt('--previous-model');
const consent=process.argv.includes('--confirm-local-mlx-inference');
const inspect=path=>{
  if(!path||!isAbsolute(path)||/[\x00-\x1f]/.test(path)||path.split(/[\/\\]/).includes('..'))
    throw Error('Only absolute, canonical, operator-local paths are supported');
  const entry=lstatSync(path);
  if(entry.isSymbolicLink()||(!entry.isFile()&&!entry.isDirectory()))
    throw Error('MLX acceptance rejects symbolic links and special files');
  return realpathSync(path);
};
/** The same open, nofollow descriptor is used for file validation and every
 * byte read. A path swap between checking and hashing cannot redirect the
 * receipt or fused weights to different contents. Read bounded chunks, never
 * an unbounded re-open of a file validated by pathname. */
function withVerifiedFile(path,max,label,reader){
  if(typeof constants.O_NOFOLLOW!=='number')
    throw Error('Platform cannot guarantee no-follow '+label+' reads');
  const fd=openSync(path,constants.O_RDONLY|constants.O_NOFOLLOW);
  try{
    const before=fstatSync(fd);
    if(!before.isFile()||before.size<=0||before.size>max)
      throw Error('Invalid bounded '+label);
    const result=reader(fd,before.size);
    const after=fstatSync(fd);
    if(after.size!==before.size||after.mtimeMs!==before.mtimeMs||
       after.ctimeMs!==before.ctimeMs)
      throw Error(label+' changed during descriptor-bound read');
    return {result,size:before.size};
  }finally{closeSync(fd);}
}
function bytesOf(fd,size){
  const output=Buffer.alloc(size);
  let offset=0;
  while(offset<size){
    const received=readSync(fd,output,offset,size-offset,offset);
    if(received===0)throw Error('File truncated during read');
    offset+=received;
  }
  return output;
}
export function readReceipt(path){
  const {result}=withVerifiedFile(path,512*1024,'MLX receipt',bytesOf);
  return JSON.parse(result.toString('utf8'));
}
export async function digestFile(path,max){
  const {result,size}=withVerifiedFile(path,max,'fused model shard',(fd,length)=>{
    const hash=createHash('sha256'),buffer=Buffer.allocUnsafe(64*1024);
    let offset=0;
    while(offset<length){
      const received=readSync(fd,buffer,0,Math.min(buffer.length,length-offset),offset);
      if(received<=0)throw Error('Fused weight truncated during hashing');
      hash.update(buffer.subarray(0,received));offset+=received;
    }
    return hash.digest('hex');
  });
  return {size,sha256:result};
}
function inference(py,modelPath){
  const script=[
    'import json,sys',
    'from mlx_lm import load,generate',
    'model,tokenizer=load(sys.argv[1])',
    'result=generate(model,tokenizer,prompt="Respond with the word ready.",max_tokens=24)',
    'if not isinstance(result,str) or not (1<=len(result.strip())<=8192): raise RuntimeError("empty or oversized real inference")',
    'print(json.dumps({"modelLoaded":True,"outputChars":len(result.strip())}))',
  ].join('\n');
  const ran=spawnSync(py,['-c',script,modelPath],{encoding:'utf8',
    timeout:300_000,maxBuffer:128*1024,
    env:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',
      TMPDIR:process.env.TMPDIR??'/tmp',PYTHONUNBUFFERED:'1'}});
  if(ran.error||ran.status!==0)
    throw Error('Real Apple Silicon MLX model load or generation failed');
  // Do not publish inference text: only the bounded measurement leaves the
  // isolated test process. Discard package startup chatter.
  const last=ran.stdout.split(/\r?\n/).filter(Boolean).at(-1);
  let info;
  try{info=JSON.parse(last);}catch{throw Error('MLX inference evidence was malformed');}
  if(info.modelLoaded!==true||!Number.isSafeInteger(info.outputChars)||
     info.outputChars<1||info.outputChars>8192)
    throw Error('MLX did not produce independently observed assistant text');
  return info.outputChars;
}
async function main(){
  if(process.argv.includes('--self-check')){
    if(!token.test('a'.repeat(64))||token.test('x'.repeat(64))||
       required.some(k=>typeof k!=='string'))
      throw Error('Fused-model acceptance input guards broken');
    console.log(JSON.stringify({status:'contract_passed',mode:'no_model'}));return;
  }
  if(!consent||process.platform!=='darwin'||process.arch!=='arm64'||
     !receiptPath||!python)
    throw Error('An operator-owned Apple Silicon Mac and explicit --receipt, --python and --confirm-local-mlx-inference are required');
  // Homebrew/virtualenv Python entrypoints commonly are symlinks; resolve
  // the selected binary to its reviewed regular target rather than refusing
  // a legitimate Apple Silicon MLX environment on path shape alone.
  const hasUnsafePathSegments=value=>[...value].some(char=>char.charCodeAt(0)<32||char.charCodeAt(0)===127)||
    value.replaceAll('\\\\','/').split('/').includes('..');
  if(!isAbsolute(python)||hasUnsafePathSegments(python))
    throw Error('Python must be an absolute local selected executable');
  if(!isAbsolute(receiptPath)||hasUnsafePathSegments(receiptPath))
    throw Error('Receipt must be an absolute local path');
  const py=realpathSync(python);
  // A Python path is invoked as an executable by the OS; do not pre-check
  // it with lstat and then assume the executable cannot change afterwards.
  const data=readReceipt(receiptPath);
  if(data.version!==1||data.engine!=='mlx_lm'||
     data.status!=='validated_fused_model_requires_manual_runtime_import'||
     !Number.isFinite(data.testPerplexity)||data.testPerplexity<=0||
     !Number.isFinite(data.fusedTestPerplexity)||data.fusedTestPerplexity<=0||
     !Number.isFinite(data.maxTestPpl)||data.maxTestPpl<=0||
     data.fusedTestPerplexity>data.maxTestPpl||
     data.fusedTestPerplexity>data.testPerplexity*1.05||
     !token.test(data.fusedConfigSha256??'')||
     !Array.isArray(data.fusedWeights)||data.fusedWeights.length<1||
     data.fusedWeights.length>128)
    throw Error('Fused holdout and adapter parity gates have not passed');
  const model=inspect(data.fusedModelPath);
  if(!lstatSync(model).isDirectory())
    throw Error('Fused model path is not a local directory');
  const config=await digestFile(join(model,'config.json'),1024*1024);
  if(config.sha256!==data.fusedConfigSha256)
    throw Error('Fused model configuration changed after evaluation');
  const seen=new Set();
  for(const row of data.fusedWeights){
    if(!row||typeof row.name!=='string'||
       !/^[A-Za-z0-9_.-]+\.safetensors$/.test(row.name)||
       seen.has(row.name)||!token.test(row.sha256??'')||
       !Number.isSafeInteger(row.sizeBytes)||row.sizeBytes<1)
      throw Error('Invalid evaluated fused shard manifest');
    seen.add(row.name);
    const actual=await digestFile(join(model,row.name),128*1024**3);
    if(actual.sha256!==row.sha256||actual.size!==row.sizeBytes)
      throw Error('Fused model shard differs from independently evaluated weights');
  }
  const candidateChars=inference(py,model);
  let previousChars=null;
  if(prior){
    const previous=inspect(prior);
    if(previous===model||!lstatSync(previous).isDirectory())
      throw Error('Rollback-candidate path must be a different installed local model');
    previousChars=inference(py,previous);
  }
  console.log(JSON.stringify({
    version:1,mode:'real-apple-silicon-fused-mlx-inference',
    status:'verified_inference_requires_separate_desktop_activation',
    fusedModelPath:model,weightShards:seen.size,
    fusedHeldOutPpl:data.fusedTestPerplexity,
    candidateInferenceChars:candidateChars,
    rollbackModelInferenceChars:previousChars,
    note:'This is real MLX inference, NOT desktop activation, running-provider handoff or a live rollback test.',
  }));
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
main().catch(e=>{
  console.error(JSON.stringify({status:'not_verified',error:String(e).slice(0,300)}));
  process.exitCode=1;
});
}

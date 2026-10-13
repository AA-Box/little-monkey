#!/usr/bin/env node
/**
 * Explicitly opted-in Apple Silicon MLX learning -> validated fused weights ->
 * actual native task source-oracle -> stable localhost serving deployment.
 *
 * This is a standalone local OpenAI-compatible endpoint for monkey's existing
 * local_url recipe target, NOT implicit desktop/Tauri MLX model activation.
 * All unverified serving states return 503; no ungraded model is deployed.
 */
import {spawn} from 'node:child_process';
import {createServer,request as httpRequest} from 'node:http';
import {connect as tcpConnect} from 'node:net';
import {randomUUID,createHash} from 'node:crypto';
import {readFileSync,writeFileSync,existsSync,readdirSync,openSync,closeSync,fsyncSync,
 renameSync,rmSync,mkdirSync,realpathSync,lstatSync} from 'node:fs';
import {join,dirname,resolve,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {parseLearningPolicy} from './swarm-mlx-auto-learning.mjs';
import {gradeReport,acquireCycleLock} from './swarm-specialist-continuous-loop.mjs';
const ROOT=dirname(fileURLToPath(import.meta.url));
const allowed=new Set(['security','frontend','backend','performance','general']);
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
const integer=(n,min,max)=>Number.isSafeInteger(n)&&n>=min&&n<=max;
const fail=message=>{throw Error(message);};
function readFile(path,max=512*1024){
 const st=lstatSync(path);
 if(st.isSymbolicLink()||!st.isFile()||st.size<1||st.size>max)
  fail('Refusing missing, linked or oversized MLX evidence');
 return readFileSync(path,'utf8');
}
function checkedDir(path){
 if(typeof path!=='string'||!isAbsolute(path)||path.split(/[\\/]/).includes('..')||
    /[\x00-\x1f]/.test(path))fail('Expected canonical local directory');
 const stat=lstatSync(path);
 if(!stat.isDirectory()||stat.isSymbolicLink()||realpathSync(path)!==resolve(path))
  fail('Untrusted MLX directory path');
 return path;
}
export function parseServingPolicy(raw){
 const p=typeof raw==='string'?JSON.parse(raw):raw;
 if(!p||Array.isArray(p)||typeof p!=='object'||
  Object.keys(p).sort().join(',')!==
  'backendPort,checkSeconds,domains,listenPort,minAccuracy,minImprovement,modelAlias,monkeyBin,probePort,repetitions,stateDir,timeoutMs,trainingPolicy,version')
  fail('Expected exact supervised MLX serving policy version 1');
 if(p.version!==1||!integer(p.backendPort,1024,65535)||!integer(p.listenPort,1024,65535)||
  !integer(p.probePort,1024,65535)||
  new Set([p.backendPort,p.listenPort,p.probePort]).size!==3||typeof p.modelAlias!=='string'||
  !/^[A-Za-z0-9_.-]{2,80}$/.test(p.modelAlias)||!integer(p.repetitions,2,5)||
  !integer(p.timeoutMs,10000,3600000)||!integer(p.checkSeconds,60,86400)||
  !Array.isArray(p.domains)||p.domains.length<1||p.domains.length>5||
  new Set(p.domains).size!==p.domains.length||p.domains.some(d=>!allowed.has(d))||
  typeof p.minAccuracy!=='number'||p.minAccuracy<0.8||p.minAccuracy>1||
  typeof p.minImprovement!=='number'||p.minImprovement<0||p.minImprovement>0.25)
  fail('Invalid MLX serving safety and quality bounds');
 const stateDir=checkedDir(p.stateDir);
 if(typeof p.monkeyBin!=='string'||!isAbsolute(p.monkeyBin))fail('Missing installed monkey binary');
 const monkeyBin=realpathSync(p.monkeyBin);
 if(!lstatSync(monkeyBin).isFile()||monkeyBin.startsWith(stateDir+'/'))
  fail('Native CLI binary must be independent of serving state');
 if(typeof p.trainingPolicy!=='string'||!isAbsolute(p.trainingPolicy))
  fail('Training policy must be a reviewed absolute path');
 const training=parseLearningPolicy(readFile(p.trainingPolicy,16384));
 if(!training.fuseForImport)fail('Serving only fused models with existing held-out gates');
 if(stateDir===training.outputRoot||stateDir.startsWith(training.outputRoot+'/')||
    training.outputRoot.startsWith(stateDir+'/'))fail('Separate training and deployment state directories required');
 return {...p,stateDir,monkeyBin,training};
}
export function shouldPromote(previous,next,minimum,improvement){
 if(!next||Object.values(next).length===0||Object.values(next).some(v=>
    typeof v!=='number'||!Number.isFinite(v)||v<minimum||v>1))return false;
 if(!previous)return true;
 if(Object.keys(previous).sort().join(',')!==Object.keys(next).sort().join(','))return false;
 return Object.keys(next).every(d=>next[d]>=previous[d]) &&
  Object.keys(next).some(d=>next[d]-previous[d]>=improvement);
}
function atomicState(file,value){
 const temp=file+'.'+randomUUID()+'.tmp',fd=openSync(temp,'wx',0o600);
 try{writeFileSync(fd,JSON.stringify(value,null,2)+'\n');fsyncSync(fd);}
 finally{closeSync(fd);}
 try{
  renameSync(temp,file);
  const directory=openSync(dirname(file),'r');
  try{fsyncSync(directory);}finally{closeSync(directory);}
 }catch(e){rmSync(temp,{force:true});throw e;}
}
export function candidateReceipt(policy,batch){
 if(typeof batch!=='string'||!/^[A-Za-z0-9][A-Za-z0-9_.-]{0,78}$/.test(batch))
  fail('Invalid approved batch identifier');
 const root=join(policy.training.outputRoot,'run-'+batch);
 checkedDir(root);
 const receiptPath=join(root,'training-receipt.json');
 const bytes=readFile(receiptPath),receipt=JSON.parse(bytes);
 const modelPath=join(root,'fused-model');
 if(receipt.version!==1||receipt.engine!=='mlx_lm'||
    receipt.baseModel!==policy.training.model||
    receipt.status!=='validated_fused_model_requires_manual_runtime_import'||
    receipt.fusedModelPath!==modelPath||
    !Array.isArray(receipt.fusedWeights)||receipt.fusedWeights.length<1||
    !Number.isFinite(receipt.fusedTestPerplexity)||
    receipt.fusedTestPerplexity<=0||
    receipt.fusedTestPerplexity>policy.training.maxTestPpl)
  fail('Fused model did not pass independently recorded training gates');
 checkedDir(modelPath);
 return {receiptPath,modelPath,batch,receiptSha256:sha(bytes)};
}
function execute(program,args,{timeoutMs=600000,accepted=[0],environment=process.env}={}){
 return new Promise((resolve,reject)=>{
  const child=spawn(program,args,{stdio:['ignore','pipe','pipe'],env:environment});
  let stdout='',stderr='',ended=false;
  const timer=setTimeout(()=>child.kill('SIGKILL'),timeoutMs);
  function append(data,kind){
   if(ended)return;
   if((kind==='out'?stdout.length:stderr.length)+data.length>2*1024*1024){
    ended=true;child.kill('SIGKILL');reject(Error('Bounded model subprocess output exceeded'));return;
   }
   if(kind==='out')stdout+=data;else stderr+=data;
  }
  child.stdout.on('data',v=>append(v.toString('utf8'),'out'));
  child.stderr.on('data',v=>append(v.toString('utf8'),'err'));
  child.on('error',e=>{clearTimeout(timer);if(!ended){ended=true;reject(e);}});
  child.on('exit',(code,signal)=>{clearTimeout(timer);if(ended)return;ended=true;
   if(accepted.includes(code))resolve({stdout,stderr});
   else reject(Error('MLX local process failed: '+String(signal??code)+' '+stderr.slice(-200)));
  });
 });
}
function postToBackend(port,payload,timeoutMs=120000){
 return new Promise((resolve,reject)=>{
  const data=Buffer.from(JSON.stringify(payload));
  const req=httpRequest({hostname:'127.0.0.1',port,path:'/v1/chat/completions',
   method:'POST',headers:{'content-type':'application/json','content-length':String(data.length)},
   timeout:timeoutMs},res=>{
    let body='';
    res.on('data',chunk=>{body+=chunk;if(body.length>1024*1024)req.destroy(Error('Oversized MLX response'));});
    res.on('end',()=>{if(res.statusCode!==200)return reject(Error('MLX backend HTTP '+res.statusCode));
     try{resolve(JSON.parse(body));}catch{reject(Error('MLX backend sent invalid JSON'));}
    });
   });
  req.on('timeout',()=>req.destroy(Error('MLX inference timed out')));
  req.on('error',reject);req.end(data);
 });
}
function portOccupied(port){
 return new Promise(resolve=>{
  const client=tcpConnect({host:'127.0.0.1',port});
  client.setTimeout(2500,()=>{client.destroy();resolve(true);});
  client.once('connect',()=>{client.destroy();resolve(true);});
  client.once('error',error=>{client.destroy();resolve(error?.code!=='ECONNREFUSED');});
 });
}
async function assertModelIdentity(port,expected){
 const models=await new Promise((resolve,reject)=>{
  const req=httpRequest({hostname:'127.0.0.1',port,path:'/v1/models',method:'GET',timeout:15000},res=>{
   let data='';res.on('data',chunk=>{data+=chunk;if(data.length>65536)req.destroy(Error('Model identity oversized'));});
   res.on('end',()=>{if(res.statusCode!==200)return reject(Error('MLX model list rejected'));
    try{resolve(JSON.parse(data));}catch{reject(Error('Invalid live MLX model identity'));}
   });
  });req.on('error',reject);req.on('timeout',()=>req.destroy(Error('Model identity timed out')));req.end();
 });
 if(!models||!Array.isArray(models.data)||!models.data.some(row=>row?.id===expected))
  fail('Live MLX server does not prove the exact fused/base model selected for deployment');
}
async function probe(port){
 const output=await postToBackend(port,{messages:[{role:'user',content:'Reply briefly.'}],max_tokens:24,stream:false});
 const answer=output?.choices?.[0]?.message?.content;
 if(typeof answer!=='string'||answer.trim().length<1||answer.length>16384)
  fail('MLX serving process did not generate a real assistant response');
}
async function startBackend(policy,modelPath){
 // With one local model in memory, stop the previous service before this
 // call. This intentionally causes temporary 503 rather than an OOM hot-swap.
 if(await portOccupied(policy.backendPort))
  fail('Refusing an already occupied MLX backend port; another process may own the model');
 const child=spawn(policy.training.python,['-m','mlx_lm.server',
  '--model',modelPath,'--host','127.0.0.1','--port',String(policy.backendPort)],{
  stdio:['ignore','ignore','pipe'],env:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',
   TMPDIR:process.env.TMPDIR??'/tmp',PYTHONUNBUFFERED:'1'},
 });
 child.stderr.on('data',v=>process.stderr.write(String(v).slice(0,2048)));
 child.on('error',e=>console.error('MLX model process error:',String(e)));
 const deadline=Date.now()+180000;
 while(Date.now()<deadline){
  if(child.exitCode!==null)fail('Local MLX service exited before ready');
  try{await probe(policy.backendPort);await assertModelIdentity(policy.backendPort,modelPath);return child;}
  catch{await new Promise(done=>setTimeout(done,2500));}
 }
 child.kill('SIGKILL');fail('MLX service failed real generation before readiness deadline');
}
async function stopBackend(child){
 if(!child||child.exitCode!==null)return;
 const exited=new Promise(done=>child.once('exit',done));
 child.kill('SIGTERM');
 await Promise.race([exited,new Promise(done=>setTimeout(done,10000))]);
 if(child.exitCode===null){child.kill('SIGKILL');await exited;}
}
async function independentOracle(policy){
 const scores={},target='local-url:http://127.0.0.1:'+policy.probePort+'|'+policy.modelAlias;
 for(const domain of policy.domains){
  const run=await execute(process.execPath,[join(ROOT,'swarm-specialist-live-benchmark.mjs'),
   '--domain',domain,'--target',target,'--repetitions',String(policy.repetitions),
   '--timeout-ms',String(policy.timeoutMs),'--monkey-bin',policy.monkeyBin],{
   // A benchmark exits 1 if some models fail source fixtures. Valid, complete
   // lower accuracy data must still be graded; only content matters.
   accepted:[0,1],timeoutMs:policy.timeoutMs*policy.repetitions*9+120000,
  });
  const grade=gradeReport(JSON.parse(run.stdout),domain,target,policy.repetitions,
   Date.now(),policy.checkSeconds*1000+300000);
  if(grade.accuracy<policy.minAccuracy)fail('MLX did not meet actual native source-oracle quality: '+domain);
  scores[domain]=grade.accuracy;
 }
 return scores;
}
async function fusedAcceptance(policy,candidate,previous){
 const args=[join(ROOT,'swarm-mlx-fused-inference-acceptance.mjs'),
  '--receipt',candidate.receiptPath,'--python',policy.training.python,
  '--confirm-local-mlx-inference'];
 if(previous&&previous!==candidate.modelPath)args.push('--previous-model',previous);
 const r=await execute(process.execPath,args,{timeoutMs:15*60*1000,
  environment:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',TMPDIR:process.env.TMPDIR??'/tmp'}});
 const evidence=JSON.parse(r.stdout.trim());
 if(evidence.status!=='verified_inference_requires_separate_desktop_activation'||
    evidence.fusedModelPath!==candidate.modelPath||
    !integer(evidence.candidateInferenceChars,1,8192)||
    (previous&&!integer(evidence.rollbackModelInferenceChars,1,8192)))
  fail('Candidate and previous models failed real fused-weight inference acceptance');
}
function deployedState(policy){
 const file=join(policy.stateDir,'active-mlx.json');
 if(!existsSync(file))return null;
 const saved=JSON.parse(readFile(file,16384));
 if(saved.version!==1||typeof saved.modelPath!=='string'||!saved.scores||
    Object.keys(saved.scores).sort().join(',')!==policy.domains.slice().sort().join(','))
  fail('Invalid previously deployed MLX state');
 if(saved.batch===null){
  if(saved.modelPath!==policy.training.model)fail('Unverified base model reference changed');
 }else{
  const candidate=candidateReceipt(policy,saved.batch);
  if(candidate.modelPath!==saved.modelPath||candidate.receiptSha256!==saved.receiptSha256)
   fail('Previously deployed fused MLX evidence changed');
 }
 return saved;
}
function localProxy(policy,state){
 return createServer(async(req,res)=>{
  const error=(status,message)=>{res.writeHead(status,{'content-type':'application/json'});
   res.end(JSON.stringify({error:message}));};
  if(req.method==='GET'&&req.url==='/v1/models'){
   if(!state.active)return error(503,'No verified MLX route');
   res.writeHead(200,{'content-type':'application/json'});
   return res.end(JSON.stringify({object:'list',data:[{id:policy.modelAlias,object:'model',owned_by:'local'}]}));
  }
  if(req.method!=='POST'||req.url!=='/v1/chat/completions')
   return error(404,'Unsupported MLX endpoint');
  if(!state.active)return error(503,'MLX model is switching or has not passed quality gates');
  const generation=state.generation;
  let chunks=[],size=0;
  try{
   for await(const chunk of req){
    size+=chunk.length;if(size>512*1024)fail('Request exceeds 512KiB');chunks.push(chunk);
   }
   const payload=JSON.parse(Buffer.concat(chunks).toString('utf8'));
   if(!payload||typeof payload!=='object'||Array.isArray(payload)||
      payload.model!==policy.modelAlias||!Array.isArray(payload.messages))
    fail('Invalid local MLX request model identity');
   delete payload.model; // MLX-LM serves the model loaded by its operator-owned process
   if(!state.active||state.generation!==generation)return error(503,'MLX route switched during request');
   const body=Buffer.from(JSON.stringify(payload));
   const upstream=httpRequest({hostname:'127.0.0.1',port:policy.backendPort,
    path:'/v1/chat/completions',method:'POST',
    headers:{'content-type':'application/json','content-length':String(body.length)},
    timeout:policy.timeoutMs},answer=>{
    res.writeHead(answer.statusCode??502,{'content-type':answer.headers['content-type']??'application/json'});
    answer.pipe(res);
   });
   upstream.on('error',()=>{if(!res.headersSent)error(502,'MLX backend unavailable');else res.destroy();});
   upstream.on('timeout',()=>upstream.destroy(Error('MLX backend timed out')));
   res.on('close',()=>{if(!res.writableEnded)upstream.destroy();});
   upstream.end(body);
  }catch(e){if(!res.headersSent)error(400,'Invalid bounded MLX request: '+String(e).slice(0,100));}
 });
}
export async function serve(policy){
 if(process.platform!=='darwin'||process.arch!=='arm64')
  fail('Automatic fused MLX deployment requires an operator-owned Apple Silicon Mac');
 const unlock=acquireCycleLock(policy.stateDir);
 const state={active:false,generation:0,model:null,child:null,scores:null,metadata:null};
 const server=localProxy(policy,state);
 // The separate local acceptance proxy exposes candidate inference only while
 // independent native grading runs. Production routing remains 503 until passed.
 const probeState={active:false,generation:0};
 const probeServer=localProxy(policy,probeState);
 let stopping=false;
 const close=()=>{stopping=true;};
 process.on('SIGINT',close);process.on('SIGTERM',close);
 try{
  await new Promise((done,reject)=>{server.once('error',reject);
   server.listen(policy.listenPort,'127.0.0.1',done);});
  await new Promise((done,reject)=>{probeServer.once('error',reject);
   probeServer.listen(policy.probePort,'127.0.0.1',done);});
  const switchModel=async(path)=>{
   state.active=false;state.generation++;
   probeState.active=false;probeState.generation++;
   await stopBackend(state.child);state.child=null;
   const child=await startBackend(policy,path);
   state.child=child;
   child.once('exit',()=>{
    if(state.child===child){state.active=false;state.generation++;}
   });
   probeState.active=true;
   try{return {modelPath:path,scores:await independentOracle(policy)};}
   catch(e){await stopBackend(child);state.child=null;throw e;}
   finally{probeState.active=false;probeState.generation++;}
  };
  const publish=(tested,metadata)=>{
   const saved={version:1,modelPath:tested.modelPath,scores:tested.scores,
    batch:metadata?.batch??null,receiptSha256:metadata?.receiptSha256??null,verifiedAt:new Date().toISOString()};
   atomicState(join(policy.stateDir,'active-mlx.json'),saved);
   state.model=saved.modelPath;state.scores=saved.scores;state.metadata=saved;
   state.generation++;state.active=true;
   return saved;
  };
  const previous=deployedState(policy);
  if(previous?.batch)await fusedAcceptance(policy,candidateReceipt(policy,previous.batch),null);
  try{publish(await switchModel(previous?.modelPath??policy.training.model),previous);}
  catch(e){
   if(!previous||previous.modelPath===policy.training.model)throw e;
   console.error('MLX recovery failed; regrading base model:',String(e));
   publish(await switchModel(policy.training.model),null);
  }
  while(!stopping){
   try{await probe(policy.backendPort);
    probeState.active=true;
    state.scores=await independentOracle(policy);
   }catch(e){
    state.active=false;state.generation++;
    if(state.model===policy.training.model)throw e;
    console.error('MLX model regressed; independently regrading base rollback:',String(e));
    publish(await switchModel(policy.training.model),null);
   }finally{probeState.active=false;probeState.generation++;}
   try{
    await execute(process.execPath,[join(ROOT,'swarm-mlx-auto-learning.mjs'),
     '--policy',policy.trainingPolicy,'--confirm-auto-training','--once'],{
     timeoutMs:3*60*60*1000+60000,
     environment:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',TMPDIR:process.env.TMPDIR??'/tmp'},
    });
   }catch(e){console.error('Approved local MLX training did not complete:',String(e));}
   for(const name of readdirSync(policy.training.outputRoot)
     .filter(x=>/^run-[A-Za-z0-9][A-Za-z0-9_.-]{0,78}$/.test(x)).sort()){
    const batch=name.slice(4),attempt=join(policy.stateDir,'attempt-'+batch);
    if(existsSync(attempt))continue;
    mkdirSync(attempt,{mode:0o700}); // crash-safe at-most-once model deployment attempt
    try{
     const candidate=candidateReceipt(policy,batch);
     if(state.model===candidate.modelPath)continue;
     await fusedAcceptance(policy,candidate,state.model);
     const oldPath=state.model,oldMetadata=state.metadata,oldScores=state.scores;
     try{
      const trial=await switchModel(candidate.modelPath);
      if(!shouldPromote(oldScores,trial.scores,policy.minAccuracy,policy.minImprovement))
       fail('Unattended candidate lacks independently measured native-task improvement');
      publish(trial,candidate);
      console.log(JSON.stringify({status:'verified_local_mlx_deployed',batch,scores:trial.scores}));
     }catch(e){
      // Recover the exact preceding model and verified receipt identity,
      // never relabel a previously fused model as an untrained base model.
      publish(await switchModel(oldPath),oldMetadata);
      throw e;
     }
    }catch(e){console.error('Candidate rejected; last passing model retained:',batch,String(e));}
   }
   await new Promise(done=>setTimeout(done,policy.checkSeconds*1000));
  }
 }finally{
  state.active=false;state.generation++;
  server.close();probeServer.close();
  await stopBackend(state.child);
  unlock();
 }
}
const option=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1];};
if(process.argv[1]&&fileURLToPath(import.meta.url)===resolve(process.argv[1])){
 (async()=>{
  if(!process.argv.includes('--confirm-unattended-mlx-deployment'))
   fail('Real local model changes require --confirm-unattended-mlx-deployment');
  const file=option('--policy');
  if(!file||!isAbsolute(file))fail('Use --policy /absolute/path/serving-policy.json');
  await serve(parseServingPolicy(readFile(file,16384)));
 })().catch(e=>{console.error(JSON.stringify({status:'mlx_serving_stopped',error:String(e)}));process.exitCode=1;});
}
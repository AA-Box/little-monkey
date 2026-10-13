#!/usr/bin/env node
/**
 * Operator-owned Apple Silicon live acceptance. Exercises the actual service
 * shipped in the signed MLX package, not a mock or a Desktop UI transition.
 * A disposable service is used; no registered/running user model is modified.
 */
import {spawn} from 'node:child_process';
import {createServer,connect} from 'node:net';
import {mkdtempSync,openSync,closeSync,existsSync,writeFileSync,rmSync,realpathSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {dirname,join,isAbsolute} from 'node:path';
import {fileURLToPath} from 'node:url';
import {randomUUID} from 'node:crypto';
import {checkedLocalDir,safeRegularBytes,digest,fusedWeightsManifest,trainingPlan} from './swarm-mlx-lora.mjs';

const ROOT=dirname(fileURLToPath(import.meta.url));
const service=join(ROOT,'../packaging/mlx/service/mlx_server.py');
const train=join(ROOT,'swarm-mlx-lora.mjs');
const option=(name)=>{const i=process.argv.indexOf(name);return i===-1?null:process.argv[i+1]??null;};
const fail=(message)=>{throw Error(message);};
const pause=(ms)=>new Promise(done=>setTimeout(done,ms));
function exactAbsolute(path,label){
 if(typeof path!=='string'||!isAbsolute(path)||/[\x00-\x1f\x7f]/.test(path)||path.split(/[\\/]/).includes('..'))
  fail(label+' must be an absolute, non-traversing path');
 return path;
}
export function verifiedFusion(output,base){
 const data=JSON.parse(safeRegularBytes(join(output,'training-receipt.json'),'training receipt',512*1024).toString('utf8'));
 if(data.version!==1||data.engine!=='mlx_lm'||
    data.status!=='validated_fused_model_requires_manual_runtime_import'||
    data.baseModel!==base||!Number.isFinite(data.testPerplexity)||data.testPerplexity<=0||
    !Number.isFinite(data.fusedTestPerplexity)||data.fusedTestPerplexity<=0||
    !Number.isFinite(data.maxTestPpl)||data.maxTestPpl<=0||
    data.fusedTestPerplexity>data.maxTestPpl||data.fusedTestPerplexity>data.testPerplexity*1.05||
    !Array.isArray(data.fusedWeights)||!data.fusedWeights.length||
    !/^[0-9a-f]{64}$/.test(data.fusedConfigSha256??''))fail('Fused weight or quality receipt is unverified');
 const model=checkedLocalDir(data.fusedModelPath,'fused model');
 if(model!==join(output,'fused-model'))fail('The fused model is not located in this training run');
 const config=digest(safeRegularBytes(join(model,'config.json'),'fused model config',1024*1024));
 if(config!==data.fusedConfigSha256)fail('Fused model configuration changed');
 const measured=fusedWeightsManifest(model);
 if(JSON.stringify(measured)!==JSON.stringify(data.fusedWeights))fail('Evaluated fused shards changed');
 return {model,receipt:data,weights:measured};
}
export function evaluateSse(body,requestId){
 if(typeof body!=='string'||body.length>128*1024)fail('Invalid bounded production service stream');
 const events=body.split(/\r?\n/).filter(line=>line.startsWith('data: ')).map(line=>JSON.parse(line.slice(6)));
 if(events.length<3||events[0]?.type!=='started'||events[0]?.request_id!==requestId||
    events.at(-1)?.type!=='completed'||
    events.filter(x=>x.type==='started').length!==1||
    events.filter(x=>x.type==='completed').length!==1||
    events.some(x=>x.type==='error'||!['started','text_delta','completed'].includes(x.type)))
  fail('Native service did not complete a clean, target-specific MLX generation');
 const text=events.filter(x=>x.type==='text_delta').map(x=>x.text).join('');
 const last=events.at(-1);
 if(typeof text!=='string'||!text.trim()||text.length>8192||
    !Number.isSafeInteger(last.input_tokens)||last.input_tokens<1||
    !Number.isSafeInteger(last.output_tokens)||last.output_tokens<1)
  fail('Native service returned empty output or invalid model token evidence');
 return {outputChars:text.trim().length,inputTokens:last.input_tokens,outputTokens:last.output_tokens};
}
export async function rollbackAfterFault({activate,probe,deactivate},previous,candidate,receipt){
 let active=null;
 try{
  active=await activate(candidate);
  const observed=await probe(active);
  if(!observed?.outputChars)fail('Candidate inference failed');
  // Deliberately inject a post-activation incident, not just a bad path.
  // This forces the actual externally running candidate -> previous transition.
  throw Error('acceptance_injected_post_activation_fault');
 }catch(error){
  try{
   if(active){const stopping=active;active=null;await deactivate(stopping);}
   active=await activate(previous);
   const restored=await probe(active);
   if(!restored?.outputChars)fail('Restored model failed its native inference canary');
   receipt.rollback={verified:true,reason:String(error),restoredModel:previous,
    restoredOutputChars:restored.outputChars,restoredOutputTokens:restored.outputTokens};
  }catch(recoveryFailure){
   receipt.rollback={verified:false,error:String(recoveryFailure)};
   throw Error('Rollback was not independently verified: '+String(recoveryFailure));
  }
  if(!String(error).includes('acceptance_injected_post_activation_fault'))
   fail('Candidate inference failed, but rollback to the previous model was verified');
 }finally{if(active)await deactivate(active);}
}
async function unusedPort(){
 return new Promise((done,reject)=>{
  const server=createServer();server.once('error',reject);
  server.listen(0,'127.0.0.1',()=>{
   const port=server.address().port;
   server.close(error=>error?reject(error):done(port));
  });
 });
}
async function reachable(port){
 return new Promise(resolveResult=>{
  const socket=connect({host:'127.0.0.1',port});
  socket.setTimeout(1000);
  socket.on('connect',()=>{socket.end();resolveResult(true);});
  const no=()=>{socket.destroy();resolveResult(false);};
  socket.on('error',no);socket.on('timeout',no);
 });
}
async function awaitExit(child,timeoutMs=15_000){
 if(child.exitCode!==null||child.signalCode!==null)return;
 await new Promise(done=>{
  const timer=setTimeout(()=>{
   if(child.exitCode===null&&child.signalCode===null)child.kill('SIGKILL');
  },timeoutMs);
  child.once('close',()=>{clearTimeout(timer);done();});
 });
}
async function startService(python,model,logs){
 const port=await unusedPort();
 const logPath=join(logs,'mlx-service-'+randomUUID()+'.private.log');
 const fd=openSync(logPath,'wx',0o600);
 let child;
 try{
  child=spawn(python,[service,'--host','127.0.0.1','--port',String(port),'--model',model],{
   stdio:['ignore',fd,fd],env:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',
    TMPDIR:process.env.TMPDIR??'/tmp',PYTHONUNBUFFERED:'1'},
  });
 }finally{closeSync(fd);}
 const instance={model,port,child,logPath};
 let spawnError=null;child.once('error',error=>{spawnError=error;});
 try{
  const end=Date.now()+300_000;
  while(Date.now()<end){
   if(spawnError||child.exitCode!==null||child.signalCode!==null)fail('MLX service exited before loading model; check private log');
   if(await reachable(port))return instance;
   await pause(250);
  }
  fail('MLX service failed to load in five minutes; check protected log');
 }catch(error){await stopService(instance);throw error;}
}
async function stopService(instance){
 if(!instance)return;
 if(instance.child.exitCode===null&&instance.child.signalCode===null)instance.child.kill('SIGTERM');
 await awaitExit(instance.child);
 if(await reachable(instance.port))fail('Stopped model still has a reachable native service port');
}
async function nativeProbe(instance){
 if(instance.child.exitCode!==null||instance.child.signalCode!==null)fail('Inactive service cannot pass a canary');
 const requestId=randomUUID();
 const controller=new AbortController();const timer=setTimeout(()=>controller.abort(),120_000);
 try{
  const res=await fetch('http://127.0.0.1:'+instance.port+'/v1/generate',{
   method:'POST',signal:controller.signal,headers:{'Content-Type':'application/json'},
   body:JSON.stringify({requestId,messages:[{role:'user',text:'Reply with a short sentence about readiness.'}],maxTokens:32,temperature:0}),
  });
  if(res.status!==200||res.headers.get('content-type')?.split(';')[0]!=='text/event-stream')
   fail('Production MLX service did not answer the real generation endpoint');
  const reader=res.body.getReader();const pieces=[];let total=0;
  for(;;){const {done,value}=await reader.read();if(done)break;total+=value.byteLength;
   if(total>128*1024)fail('Native MLX generation overflowed bounded acceptance response');pieces.push(value);}
  const bytes=Buffer.concat(pieces);
  return evaluateSse(bytes.toString('utf8'),requestId);
 }finally{clearTimeout(timer);}
}
async function trainReal(python,plan,iterations,gate,logs){
 const log=join(logs,'real-mlx-training.private.log');
 const fd=openSync(log,'wx',0o600);
 const args=[train,'--model',plan.model,'--data',plan.data,'--output',plan.output,
  '--iters',String(iterations),'--max-test-ppl',String(gate),'--python',python,
  '--confirm-local-training','--fuse-for-import','--confirm-fusion'];
 let child;
 try{child=spawn(process.execPath,args,{stdio:['ignore',fd,fd],
   env:{PATH:process.env.PATH??'',HOME:process.env.HOME??'',TMPDIR:process.env.TMPDIR??'/tmp',PYTHONUNBUFFERED:'1'}});
 }finally{closeSync(fd);}
 const status=await new Promise(done=>{
  child.once('error',()=>done(-1));child.once('close',(code)=>done(code??-1));
 });
 if(status!==0)fail('Real held-out training, fusion, or fused parity gate failed; inspect protected logs');
}
async function main(){
 if(process.platform!=='darwin'||process.arch!=='arm64'||!process.argv.includes('--confirm-disposable-native-switch'))
  fail('Only an explicitly authorized real Apple Silicon host can perform native lifecycle acceptance');
 const model=checkedLocalDir(option('--model'),'MLX base model');
 const data=checkedLocalDir(option('--data'),'MLX dataset');
 const previous=checkedLocalDir(option('--previous-model')??model,'Previous rollback model');
 const output=exactAbsolute(option('--output'),'New output');
 const iterations=option('--iters')??'200',gate=option('--max-test-ppl');
 const plan=trainingPlan({model,data,output,iters:iterations,maxPpl:gate});
 if(previous===output||output.startsWith(previous+'/'))fail('Do not place candidate output within the previous production model');
 const python=realpathSync(exactAbsolute(option('--python'),'Python executable'));
 const logs=mkdtempSync(join(tmpdir(),'swarm-native-mlx-acceptance-'));
 const receipt={version:1,status:'not_verified',acceptance:'native-mlx-train-fuse-live-service-rollback',
  platform:process.platform,architecture:process.arch,baseline:previous,
  desktopActivationTested:false,startedAt:new Date().toISOString()};
 let active=null;
 try{
  active=await startService(python,previous,logs);
  const initial=await nativeProbe(active);
  receipt.baselineCanary={passed:true,...initial};
  await stopService(active);active=null;
  await trainReal(python,plan,iterations,gate,logs);
  const fusion=verifiedFusion(output,plan.model);
  receipt.training={heldOutPpl:fusion.receipt.testPerplexity,fusedHeldOutPpl:fusion.receipt.fusedTestPerplexity,
   fusedWeightShards:fusion.weights.length,adapterSha256:fusion.receipt.adapterSha256,
   fusedConfigSha256:fusion.receipt.fusedConfigSha256};
  // The real packaged MLX Python server loads the fused weights and serves a
  // genuine HTTP generation. It is never a fabricated mlx_lm.load-only proof.
  active=await startService(python,fusion.model,logs);
  const candidate=await nativeProbe(active);
  receipt.candidate={passed:true,model:fusion.model,...candidate};
  await stopService(active);active=null;
  verifiedFusion(output,plan.model); // detect weight mutation during serving
  await rollbackAfterFault({activate:async path=>{
    active=await startService(python,path,logs);return active;
   },probe:nativeProbe,deactivate:async server=>{await stopService(server);if(active===server)active=null;}},
   previous,fusion.model,receipt);
  verifiedFusion(output,plan.model); // after fault and recovery
  if(!receipt.rollback?.verified)fail('Rollback acceptance did not pass');
  receipt.status='verified_native_service_lifecycle';
  receipt.completedAt=new Date().toISOString();
  writeFileSync(join(output,'native-lifecycle-acceptance.json'),JSON.stringify(receipt,null,2)+'\n',{flag:'wx',mode:0o600});
  console.log(JSON.stringify({status:receipt.status,evidence:join(output,'native-lifecycle-acceptance.json'),
   note:'Isolated real MLX service switch and rollback; Desktop/Tauri activation is not tested.'}));
 }catch(error){
  receipt.status='not_verified';receipt.failedAt=new Date().toISOString();
  receipt.failure=String(error).slice(0,500);
  if(existsSync(output)){
   try{writeFileSync(join(output,'native-lifecycle-acceptance.json'),JSON.stringify(receipt,null,2)+'\n',
    {flag:'wx',mode:0o600});}catch{}
  }
  throw error;
 }finally{
  if(active)await stopService(active);
  // Protected logs are kept on failure for local diagnosis, never uploaded.
  if(receipt.status==='verified_native_service_lifecycle')rmSync(logs,{recursive:true,force:true});
 }
}
if(process.argv[1]&&fileURLToPath(import.meta.url)===process.argv[1]){
 main().catch(error=>{console.error(JSON.stringify({status:'not_verified',error:String(error).slice(0,400)}));process.exitCode=1;});
}

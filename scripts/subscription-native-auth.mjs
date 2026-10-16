#!/usr/bin/env node
/**
 * Use existing vendor CLI authentication. NEVER read/copy/parse their OAuth
 * tokens or credential stores. Codex execution is delegated to its official
 * signed-in client. Claude is launched as the official interactive client
 * only; third-party subscription routing is deliberately not implemented.
 */
import {spawn,spawnSync} from 'node:child_process';
import {mkdtempSync,rmSync,chmodSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
const args=process.argv.slice(2),provider=args[0],action=args[1];
const opt=(key)=>{const i=args.indexOf(key);return i<0?null:args[i+1]??null;};
const binaries={codex:process.env.LITTLE_MONKEY_CODEX_BIN??'codex',
 claude:process.env.LITTLE_MONKEY_CLAUDE_BIN??'claude'};
if(!['codex','claude'].includes(provider) ||
 !['status','login','run','open'].includes(action)){
 console.error('Usage: subscription-native-auth.mjs codex|claude status|login|run|open [--prompt TEXT] --consent');
 process.exit(2);
}
const bin=binaries[provider];
const fail=message=>{throw Error(message);};
const approved=()=>{if(!args.includes('--consent'))fail('Explicit --consent required');};
const authenticated=()=>{
 const command=provider==='codex'?['login','status']:['auth','status'];
 const v=spawnSync(bin,command,{encoding:'utf8',timeout:15000,
  stdio:['ignore','pipe','pipe'],maxBuffer:65536});
 return !v.error && v.status===0;
};
async function interactive(command){
 approved();
 const code=await new Promise((resolve,reject)=>{
  const child=spawn(bin,command,{stdio:'inherit',env:process.env});
  child.on('error',reject);
  child.on('close',resolve);
 });
 if(code!==0)fail('Official vendor client exited unsuccessfully');
 console.log(JSON.stringify({provider,action,usedOfficialClient:true}));
}
async function runCodex(){
 approved();
 const prompt=opt('--prompt');
 if(typeof prompt!=='string'||!prompt.trim()||prompt.length>4000||prompt.includes('\0'))
  fail('Requires explicit prompt of 1–4000 characters');
 if(!authenticated())fail('Official Codex CLI is not logged in; use codex login');
 const workspace=mkdtempSync(join(tmpdir(),'lm-codex-native-'));
 chmodSync(workspace,0o700);
 try{
  const cliArgs=['exec','--json','--ephemeral','--sandbox','read-only',
   '--skip-git-repo-check','--cd',workspace,prompt];
  const output=await new Promise((resolve,reject)=>{
   const child=spawn(bin,cliArgs,{cwd:workspace,env:process.env,
    stdio:['ignore','pipe','pipe']});
   let stdout='',tooLarge=false;
   const timer=setTimeout(()=>child.kill(),120000);
   child.stdout.on('data',chunk=>{
    if(stdout.length+chunk.length>2097152){tooLarge=true;child.kill();}
    else stdout+=chunk.toString('utf8');
   });
   // Never expose raw stderr: it may include identity/account metadata.
   child.stderr.resume();
   child.on('error',e=>{clearTimeout(timer);reject(e);});
   child.on('close',code=>{clearTimeout(timer);resolve({code,stdout,tooLarge});});
  });
  if(output.code!==0||output.tooLarge)fail('Official Codex execution failed');
  let completed=false,answer='';
  for(const line of output.stdout.split(/\r?\n/)){
   if(!line.trim())continue;
   const event=JSON.parse(line);
   if(event.type==='turn.failed'||event.type==='error')fail('Codex turn failed');
   if(event.type==='turn.completed')completed=true;
   if(event.type==='item.completed'&&event.item?.type==='agent_message'&&
      typeof event.item.text==='string'){
    answer+=event.item.text.slice(0,10000-answer.length);
   }
  }
  if(!completed||!answer.trim())fail('No verified completed Codex assistant turn');
  console.log(JSON.stringify({provider:'codex',action:'run',
   usedOfficialClient:true,completed:true,response:answer}));
 }finally{rmSync(workspace,{force:true,recursive:true});}
}
try{
 if(action==='status'){
  const ok=authenticated();
  console.log(JSON.stringify({provider,action,authenticated:ok,
   credentialBytesRead:0,credentialsHandledBy:'vendor CLI'}));
  if(!ok)process.exitCode=1;
 }else if(action==='login'){
  await interactive(provider==='codex'?['login']:['auth','login']);
 }else if(action==='open'){
  await interactive([]);
 }else if(provider==='codex'){
  await runCodex();
 }else fail('Claude subscription cannot be used as a third-party model API. Open Claude Code instead.');
}catch(error){
 console.error('NATIVE_AUTH_ERROR: '+String(error.message??error));
 process.exitCode=1;
}

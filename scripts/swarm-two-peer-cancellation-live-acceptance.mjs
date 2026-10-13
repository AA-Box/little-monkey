#!/usr/bin/env node
/**
 * Operator-controlled real remote cancellation: submit exactly one read-only
 * task, cancel only its original owned receipt, verify a terminal cancelled
 * state. No retry dispatch, no forced credentials or changed peer grants.
 */
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';
const get = name => { const idx=process.argv.indexOf(name); return idx<0?null:process.argv[idx+1]??null; };
const peer=get('--peer'),bin=get('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
const timeout=Number(get('--timeout-seconds')??'180');
const interval=Number(get('--poll-seconds')??'3');
const confirmed=process.argv.includes('--confirm-disposable');
if(!confirmed || !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,119}$/.test(peer??'') ||
   !Number.isSafeInteger(timeout) || timeout<15 || timeout>600 ||
   !Number.isSafeInteger(interval) || interval<1 || interval>60){
  console.error('Usage: --peer ALIAS --confirm-disposable [--monkey-bin PATH] [--timeout-seconds 15..600] [--poll-seconds 1..60]');
  process.exit(2);
}
function cli(args){
  const result=spawnSync(bin,args,{encoding:'utf8',timeout:30_000,maxBuffer:2*1024*1024,env:process.env});
  if(result.error||result.status!==0) throw Error('CLI failed: '+args.slice(0,2).join(' '));
  try{return JSON.parse(result.stdout.trim());}
  catch {throw Error('CLI response is not valid JSON: '+args.slice(0,2).join(' '));}
}
const sleep=ms=>new Promise(resolve=>setTimeout(resolve,ms));
const correlation='cancel-e2e-'+randomUUID().replaceAll('-','').slice(0,24);
let receipt=null,ack=null;
try {
  const status=cli(['peers','status',peer,'--json']);
  if(!Array.isArray(status.granted)||!status.granted.includes('task'))
    throw Error('The receiver has not granted peer tasks.');
  const prompt='Disposable remote cancellation acceptance. READ ONLY. Independently audit the configured test repository thoroughly, reviewing multiple files and documenting source paths, without editing files or running deployments. Cancellation correlation '+correlation+'.';
  cli(['peers','send',peer,prompt,'--task','--correlation',correlation,'--json']);
  const outbound=cli(['peers','outbound','--alias',peer,'--limit','200','--json']);
  receipt=outbound.messages?.find(row=>row.correlation_id===correlation&&row.kind==='task_request');
  if(!receipt?.thread_id||!receipt?.message_id) throw Error('No unique owned outbound task receipt. Do not resubmit.');
  ack=cli(['peers','cancel-task',peer,receipt.thread_id,receipt.message_id,'--json']);
  if(ack.status!=='cancellation_requested' && ack.status!=='cancellation_already_requested') {
    throw Error('Cancellation was not accepted while the remote task was running.');
  }
  const deadline=Date.now()+timeout*1000;
  let verified=false;
  while(Date.now()<deadline){
    try {
      const thread=cli(['peers','remote-thread',peer,receipt.thread_id,'--json']);
      const current=thread.messages?.find(row=>row.message_id===receipt.message_id&&row.correlation_id===correlation);
      if(current?.state==='cancelled'){verified=true;break;}
      if(['succeeded','failed','rejected'].includes(current?.state))
        throw Error('Remote task ended without verified cancellation: '+current.state);
    }catch(error){
      if(String(error).includes('ended without verified cancellation')) throw error;
      // Connectivity failures are inconclusive, and never trigger a resend.
    }
    await sleep(interval*1000);
  }
  if(!verified) throw Error('No confirmed cancelled terminal state. Inspect original remote receipt.');
  console.log(JSON.stringify({mode:'real-two-installation-cancellation',status:'verified',peer,
    threadId:receipt.thread_id,messageId:receipt.message_id,correlation,
    cancellationAcknowledgement:ack.status,
    note:'Test verifies remote terminal state, not a simulated worker cancellation.'},null,2));
}catch(error){
  console.error(JSON.stringify({mode:'real-two-installation-cancellation',status:'not_verified',peer,
    threadId:receipt?.thread_id??null,messageId:receipt?.message_id??null,correlation,
    error:String(error),note:'Never resend automatically. Inspect the same durable remote receipt.'},null,2));
  process.exitCode=1;
}

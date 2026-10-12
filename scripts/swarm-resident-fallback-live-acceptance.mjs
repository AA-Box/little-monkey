#!/usr/bin/env node
/**
 * Two-phase REAL resident swarm read-only failover acceptance.
 *
 * The operator supplies an already registered disposable graph whose root
 * primary PLAN recipe is designed to fail and whose alternative PLAN recipe
 * can succeed. Capture the primary receipt before it fails, then verify
 * the terminal result. No model mocks, no service restarts and no CLI
 * permissions are enabled by this harness.
 */
import {createHash} from 'node:crypto';
import {spawnSync} from 'node:child_process';
import {readFileSync,writeFileSync} from 'node:fs';
import {setTimeout as sleep} from 'node:timers/promises';

const opt=(name)=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null;};
const identifier=/^[A-Za-z0-9][A-Za-z0-9_.-]{1,79}$/;
function deterministicId(graph,node){
 return 'job-'+createHash('sha256').update('resident-swarm:'+graph+':'+node).digest('hex').slice(0,32);
}
if(process.argv.includes('--self-check')){
 const root=deterministicId('swarm-test','leader');
 const alternate=deterministicId('swarm-test','fallback:leader');
 if(root===alternate||root.length!==36||alternate.length!==36)process.exitCode=1;
 else console.log(JSON.stringify({status:'contract_only',distinctDeterministicJobs:true}));
}else{
 const graphId=opt('--graph'),root=opt('--root'),checkpoint=opt('--checkpoint'),phase=opt('--phase');
 const binary=opt('--monkey-bin')||process.env.LITTLE_MONKEY_CLI||'monkey';
 const approved=process.argv.includes('--confirm-disposable');
 const deadline=Number(opt('--timeout-seconds')??300);
 if(!approved||!['capture','verify'].includes(phase)||!identifier.test(graphId??'')||
    !identifier.test(root??'')||!checkpoint||checkpoint.length>1024||
    /[\x00-\x1f]/.test(checkpoint)||!Number.isSafeInteger(deadline)||deadline<10||deadline>1800){
  console.error('Usage: --phase capture|verify --graph SWARM_ID --root NODE_ID --checkpoint ABS_PATH --confirm-disposable [--timeout-seconds 300]');
  process.exitCode=2;
 }else{
  const run=(args)=>{
   const result=spawnSync(binary,args,{encoding:'utf8',timeout:20_000,maxBuffer:2_000_000,env:process.env});
   if(result.error||result.status!==0)throw Error('Installed CLI returned a failure for '+args.slice(0,2).join(' '));
   const output=JSON.parse(result.stdout.trim());
   if(!output||typeof output!=='object')throw Error('Invalid daemon response');
   return output;
  };
  const status=()=>{
   const daemon=run(['daemon','status','--json']);
   if(!daemon.installed||!daemon.service_running||!daemon.heartbeat_fresh)
     throw Error('Healthy installed daemon required; no simulation permitted.');
   const response=run(['daemon','swarm-status','--id',graphId]);
   const graph=response.graphs?.find(g=>g.id===graphId);
   if(!graph||!Array.isArray(graph.tasks))throw Error('Expected durable registered graph not found.');
   const selected=graph.tasks.find(t=>t.id===root);
   if(!selected)throw Error('Configured root node not found.');
   return {graph,selected};
  };
  try{
   const primary=deterministicId(graphId,root),fallback=deterministicId(graphId,'fallback:'+root);
   if(phase==='capture'){
    const {graph,selected}=status();
    if(graph.state!=='running'||selected.state!=='submitted'||selected.job_id!==primary)
      throw Error('Capture BEFORE primary terminal failure, with its actual durable submitted job.');
    writeFileSync(checkpoint,JSON.stringify({
      version:1,mode:'resident-fallback-before',graph:graphId,root,
      primaryJob:primary,fallbackJob:fallback,createdAt:Date.now(),
    },null,2)+'\n',{flag:'wx',mode:0o600});
    console.log(JSON.stringify({status:'captured_not_verified',graph:graphId,root,
      primaryJob:primary,expectedFallbackJob:fallback,checkpoint}));
   }else{
    const saved=JSON.parse(readFileSync(checkpoint,'utf8'));
    if(saved.version!==1||saved.mode!=='resident-fallback-before'||
       saved.graph!==graphId||saved.root!==root||saved.primaryJob!==primary||
       saved.fallbackJob!==fallback)throw Error('Protected capture does not match this graph and root.');
    let latest;
    const end=Date.now()+deadline*1000;
    while(Date.now()<end){
      latest=status();
      const {graph,selected}=latest;
      if(selected.job_id!==null&&selected.job_id!==primary&&selected.job_id!==fallback)
        throw Error('Unknown new job receipt is not the authorized fallback.');
      if(graph.state==='failed'||graph.state==='cancelled'||selected.state==='failed'||
         selected.state==='cancelled')throw Error('Graph or replacement failed; no successful failover.');
      if(graph.state==='succeeded'&&selected.state==='succeeded'&&selected.job_id===fallback)break;
      await sleep(2000);
    }
    if(latest?.graph.state!=='succeeded'||latest.selected.state!=='succeeded'||
       latest.selected.job_id!==fallback)throw Error('Replacement did not achieve verified terminal graph success within window.');
    console.log(JSON.stringify({status:'verified_real_read_only_failover',
      graph:graphId,root,primaryJob:primary,fallbackJob:fallback,
      graphState:latest.graph.state,
      note:'Proves distinct durable fallback submission and scheduler terminal success, not external model correctness or remote leader election.'}));
   }
  }catch(error){
   console.error(JSON.stringify({status:'not_verified',phase,graph:graphId,error:String(error)}));
   process.exitCode=1;
  }
 }
}

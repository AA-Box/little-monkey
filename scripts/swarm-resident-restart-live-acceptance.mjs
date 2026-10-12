#!/usr/bin/env node
/**
 * REAL resident restart/reconciliation acceptance, two deliberate phases.
 *
 * First phase records one already registered ACTIVE disposable swarm graph,
 * the service PID and durable task-job ids in a protected checkpoint.
 *
 * Operator then restarts their prepared Little Monkey installed service.
 * NEVER stop the service automatically: it could own unrelated live work.
 *
 * Second phase checks that a NEW daemon process reattached to the same
 * graph and original job IDs, retained already completed work, and did
 * not auto-replay ambiguous external mutations.
 */
import {spawnSync} from 'node:child_process';
import {readFileSync,writeFileSync} from 'node:fs';
const option=name=>{const i=process.argv.indexOf(name);return i<0?null:process.argv[i+1]??null;};
const phase=option('--phase'),id=option('--graph'),file=option('--checkpoint');
const cli=option('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
const approved=process.argv.includes('--confirm-disposable');
const goodId=/^swarm-[A-Za-z0-9._-]{2,75}$/;
if(!approved || !['capture','verify'].includes(phase) || !goodId.test(id??'') ||
   !file || file.length>1024 || /[\r\n\0]/.test(file)) {
  console.error('Usage: --phase capture|verify --graph swarm-ID --checkpoint ABSOLUTE_JSON_PATH --confirm-disposable [--monkey-bin PATH]');
  process.exit(2);
}
function run(args) {
  const out=spawnSync(cli,args,{encoding:'utf8',timeout:20_000,
    maxBuffer:2*1024*1024,env:process.env});
  if(out.error||out.status!==0)throw Error('Installed daemon command failed: '+args.slice(0,2).join(' '));
  try{return JSON.parse(out.stdout.trim());}
  catch{throw Error('Installed daemon did not produce valid JSON.');}
}
function readGraph(){
  const data=run(['daemon','swarm-status','--id',id]);
  const graph=data.graphs?.find(g=>g.id===id);
  if(!graph||!Array.isArray(graph.tasks)||!graph.tasks.length)throw Error('Disposable graph not present in durable daemon state.');
  const jobIds=new Set();
  for(const task of graph.tasks) {
    if(typeof task.id!=='string'||!['pending','submitted','succeeded','failed','cancelled'].includes(task.state)) {
      throw Error('Unexpected durable task state.');
    }
    if(task.job_id!=null){
      if(typeof task.job_id!=='string'||jobIds.has(task.job_id))throw Error('Duplicate or invalid job receipt.');
      jobIds.add(task.job_id);
    }
  }
  return graph;
}
function snapshot(graph){
  return Object.fromEntries(graph.tasks.map(t=>[t.id,{state:t.state,jobId:t.job_id??null}]));
}
try{
  const status=run(['daemon','status','--json']);
  if(!status.installed||!status.service_running||!status.heartbeat_fresh||
     !Number.isSafeInteger(status.pid)||status.pid<=0) {
    throw Error('A live installed daemon with fresh heartbeat is required.');
  }
  const graph=readGraph();
  if(phase==='capture'){
    if(graph.state!=='running'||!graph.tasks.some(t=>t.state==='pending'||t.state==='submitted')) {
      throw Error('Capture a real active, unfinished disposable DAG before restart.');
    }
    const receipt={version:1,mode:'resident-restart-before',id,pid:status.pid,
      savedAt:Date.now(),nodes:snapshot(graph)};
    // Never overwrite a previous incomplete acceptance receipt.
    writeFileSync(file,JSON.stringify(receipt,null,2)+'\n',{flag:'wx',mode:0o600});
    console.log(JSON.stringify({status:'captured_not_verified',graph:id,pid:status.pid,
      nodes:graph.tasks.length,checkpoint:file,
      next:'Restart the test daemon under operator control; then run --phase verify with this same checkpoint.'},null,2));
  }else{
    const before=JSON.parse(readFileSync(file,'utf8'));
    if(before.version!==1||before.mode!=='resident-restart-before'||before.id!==id||
       !Number.isSafeInteger(before.pid)||!before.nodes||
       typeof before.nodes!=='object'||Array.isArray(before.nodes))throw Error('Invalid protected checkpoint.');
    if(before.pid===status.pid||Date.now()<=before.savedAt)
      throw Error('A different, restarted daemon process was not observed.');
    if(!['running','succeeded','failed','cancelled'].includes(graph.state))
      throw Error('Graph is not in a recognized durable state.');
    const now=snapshot(graph);
    const names=Object.keys(before.nodes).sort();
    if(JSON.stringify(names)!==JSON.stringify(Object.keys(now).sort()))
      throw Error('Graph lost, added or replaced task nodes across restart.');
    for(const name of names){
      const old=before.nodes[name],newer=now[name];
      if(old.jobId!=null&&newer.jobId!==old.jobId)
        throw Error('Existing queued job identity was replaced or replayed.');
      if(old.state==='succeeded'&&newer.state!=='succeeded')
        throw Error('Previously verified completed work regressed after restart.');
      if(old.state==='failed'&&newer.state==='submitted')
        throw Error('Failed work was automatically dispatched again.');
    }
    if(graph.state==='running')throw Error('Graph has not reached a safe terminal state; retry verification of the SAME receipt later.');
    const submitted=Object.values(now).filter(t=>t.jobId!=null);
    if(!submitted.length)throw Error('No durable job from the captured graph survived the restart.');
    console.log(JSON.stringify({status:'verified_safe_reconciliation',
      graph:id,originalPid:before.pid,newPid:status.pid,graphState:graph.state,
      preservedTasks:names.length,preservedJobs:submitted.length,
      caveat:'This proves durable reattachment/non-replay, not exactly-once external side effects or a successful repair.'},null,2));
  }
}catch(error){
  console.error(JSON.stringify({status:'not_verified',phase,graph:id,error:String(error),
    note:'Do not create a replacement graph. Inspect the original job IDs and operator actions.'},null,2));
  process.exitCode=1;
}

#!/usr/bin/env node
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,chmodSync,rmSync} from 'node:fs';
import {join,resolve} from 'node:path';
import {tmpdir} from 'node:os';
import {spawnSync} from 'node:child_process';

const script=resolve('scripts/swarm-resident-restart-live-acceptance.mjs');
function fixture(mode){
  const dir=mkdtempSync(join(tmpdir(),'swarm-restart-contract-'));
  const fake=join(dir,'fake-cli.mjs'),checkpoint=join(dir,'checkpoint.json');
  const code=[
    '#!/usr/bin/env node',
    "const args=process.argv.slice(2), phase=process.env.MOCK_RESTART_PHASE, mode=process.env.MOCK_RESTART_MODE;",
    "if(args[0]!=='daemon')process.exit(2);",
    "if(args[1]==='status')console.log(JSON.stringify({installed:true,service_running:true,heartbeat_fresh:true,pid:phase==='capture'||mode==='same-pid'?111:222}));",
    "else if(args[1]==='swarm-status'){",
    "const initial=phase==='capture';",
    "const id=args[args.indexOf('--id')+1];",
    "const tasks=initial?[{id:'audit',state:'submitted',job_id:'job-original'},{id:'fix',state:'pending',job_id:null}]:",
    " [{id:'audit',state:'failed',job_id:mode==='changed-job'?'job-forged':'job-original'},{id:'fix',state:'cancelled',job_id:null}];",
    "console.log(JSON.stringify({graphs:[{id,state:initial?'running':'failed',tasks}]}));",
    "}else process.exit(4);",
  ].join('\n');
  writeFileSync(fake,code);chmodSync(fake,0o700);
  const call=(phase,extra=[])=>spawnSync(process.execPath,[script,
    '--phase',phase,'--graph','swarm-contract-example',
    '--checkpoint',checkpoint,'--monkey-bin',fake,'--confirm-disposable',...extra],
    {encoding:'utf8',timeout:6000,cwd:resolve('.'),env:{...process.env,MOCK_RESTART_MODE:mode,MOCK_RESTART_PHASE:phase}});
  return {call,cleanup:()=>rmSync(dir,{recursive:true,force:true})};
}
test('acceptance refuses malformed or unconfirmed input before CLI',()=>{
  const response=spawnSync(process.execPath,[script,'--phase','capture',
    '--graph','../escape','--checkpoint','/tmp/checkpoint.json'],{encoding:'utf8'});
  assert.equal(response.status,2);
});
test('safely verifies a new service process and the SAME durable job after restart',()=>{
  const f=fixture('normal');
  try {
    const capture=f.call('capture');
    assert.equal(capture.status,0,capture.stderr);
    assert.equal(JSON.parse(capture.stdout).status,'captured_not_verified');
    const checked=f.call('verify');
    assert.equal(checked.status,0,checked.stderr);
    const outcome=JSON.parse(checked.stdout);
    assert.equal(outcome.status,'verified_safe_reconciliation');
    assert.equal(outcome.originalPid,111);assert.equal(outcome.newPid,222);
    assert.equal(outcome.preservedJobs,1);
  }finally{f.cleanup();}
});
test('cannot claim restart when service PID is unchanged',()=>{
  const f=fixture('same-pid');
  try {
    assert.equal(f.call('capture').status,0);
    assert.equal(f.call('verify').status,1);
    assert.match(f.call('verify').stderr,/different, restarted/);
  }finally{f.cleanup();}
});
test('refuses any replacement of an existing job identity',()=>{
  const f=fixture('changed-job');
  try {
    assert.equal(f.call('capture').status,0);
    const response=f.call('verify');
    assert.equal(response.status,1);
    assert.match(response.stderr,/job identity was replaced/);
  }finally{f.cleanup();}
});

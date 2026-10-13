#!/usr/bin/env node
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {mkdtempSync,writeFileSync,chmodSync,readFileSync,rmSync} from 'node:fs';
import {tmpdir} from 'node:os';
import {join,resolve} from 'node:path';
import {spawnSync} from 'node:child_process';
const script=resolve('scripts/swarm-two-peer-cancellation-live-acceptance.mjs');
function fixture(mode){
  const dir=mkdtempSync(join(tmpdir(),'swarm-cancel-contract-'));
  const binary=join(dir,'fake-cli.mjs'),state=join(dir,'state.json');
  writeFileSync(state,JSON.stringify({calls:[],correlation:null}));
  writeFileSync(binary,`#!/usr/bin/env node
import {readFileSync,writeFileSync} from 'node:fs';
const args=process.argv.slice(2),file=process.env.MOCK_CANCEL_STATE;
const data=JSON.parse(readFileSync(file,'utf8'));data.calls.push(args);
const output=o=>{writeFileSync(file,JSON.stringify(data));console.log(JSON.stringify(o));};
if(args[0]!=='peers')process.exit(3);
if(args[1]==='status')output({granted:process.env.MOCK_CANCEL_MODE==='no-grant'?[]:['task']});
else if(args[1]==='send'){data.correlation=args[args.indexOf('--correlation')+1];output({sent:true});}
else if(args[1]==='outbound')output({messages:[{kind:'task_request',message_id:'msg-original',thread_id:'thread-original',correlation_id:data.correlation}]});
else if(args[1]==='cancel-task')output({status:process.env.MOCK_CANCEL_MODE==='already-terminal'?'already_terminal':'cancellation_requested'});
else if(args[1]==='remote-thread')output({messages:[{message_id:'msg-original',correlation_id:data.correlation,state:process.env.MOCK_CANCEL_MODE==='bad-terminal'?'succeeded':'cancelled'}]});
else process.exit(4);
`);
  chmodSync(binary,0o700);
  const call=(more=[])=>spawnSync(process.execPath,[script,'--peer','test-peer','--monkey-bin',binary,'--timeout-seconds','15','--poll-seconds','1',...more],
    {cwd:resolve('.'),encoding:'utf8',timeout:8_000,env:{...process.env,MOCK_CANCEL_STATE:state,MOCK_CANCEL_MODE:mode}});
  return {call,read:()=>JSON.parse(readFileSync(state,'utf8')),
    cleanup:()=>rmSync(dir,{recursive:true,force:true})};
}
test('requires operator confirmation before contacting peer',()=>{
  const f=fixture('ok');try{assert.equal(f.call().status,2);assert.equal(f.read().calls.length,0);}finally{f.cleanup();}
});
test('cancels only owned receipt and confirms true terminal state',()=>{
  const f=fixture('ok');try{
    const r=f.call(['--confirm-disposable']);assert.equal(r.status,0,r.stderr);
    assert.equal(JSON.parse(r.stdout).status,'verified');
    const actions=f.read().calls.map(x=>x[1]);
    assert.equal(actions.filter(x=>x==='send').length,1);
    assert.equal(actions.filter(x=>x==='cancel-task').length,1);
    const cancel=f.read().calls.find(x=>x[1]==='cancel-task');
    assert.deepEqual(cancel.slice(2,5),['test-peer','thread-original','msg-original']);
  }finally{f.cleanup();}
});
test('refuses without peer task grant, never dispatches',()=>{
  const f=fixture('no-grant');try{
    assert.equal(f.call(['--confirm-disposable']).status,1);
    assert.equal(f.read().calls.filter(x=>x[1]==='send').length,0);
  }finally{f.cleanup();}
});
test('rejects finished tasks and cannot falsely label success as cancellation',()=>{
  for(const mode of ['already-terminal','bad-terminal']){
    const f=fixture(mode);try{
      const r=f.call(['--confirm-disposable']);assert.equal(r.status,1,mode);
      assert.equal(JSON.parse(r.stderr).status,'not_verified');
      assert.equal(f.read().calls.filter(x=>x[1]==='send').length,1);
    }finally{f.cleanup();}
  }
});

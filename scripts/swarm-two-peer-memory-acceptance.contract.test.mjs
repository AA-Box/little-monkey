#!/usr/bin/env node
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';

const script = resolve('scripts/swarm-two-peer-memory-live-acceptance.mjs');
function fixture(mode) {
  const dir = mkdtempSync(join(tmpdir(), 'swarm-memory-contract-'));
  const cli = join(dir, 'mock-monkey.mjs');
  const state = join(dir, 'calls.json');
  writeFileSync(state, JSON.stringify({calls:[],sync:0,local:null,deleted:false}));
  writeFileSync(cli, `#!/usr/bin/env node
import {readFileSync,writeFileSync} from 'node:fs';
const args=process.argv.slice(2),path=process.env.MOCK_MEMORY_STATE;
const data=JSON.parse(readFileSync(path,'utf8'));
data.calls.push(args);
function output(value) {writeFileSync(path,JSON.stringify(data));console.log(JSON.stringify(value));}
if(args[0]!=='peers') process.exit(2);
if(args[1]==='status') output({granted:process.env.MOCK_MEMORY_MODE==='no-grant'?[]:['memory-read','memory-write']});
else if(args[1]==='memory-put') {
  data.local=args[3];data.deleted=args.includes('--deleted');
  output({id:data.local,updated:true,revision:data.deleted?2:1});
} else if(args[1]==='memory-sync') {
  data.sync++;
  output({pulled:process.env.MOCK_MEMORY_MODE==='no-remote'?0:data.sync===1?1:0,pushed:1});
} else if(args[1]==='memory-search') {
  const matches=[{entry:{id:'receiver-record',author:'peer-fingerprint',deleted:false}}];
  if(data.local&&!data.deleted) matches.push({entry:{id:data.local,author:'@local',deleted:false}});
  if(process.env.MOCK_MEMORY_MODE==='wrong-author') matches[0].entry.author='@local';
  output({matches});
} else process.exit(3);
`);
  chmodSync(cli,0o700);
  const call=(extra=[])=>spawnSync(process.execPath,[script,'--peer','paired-node',
    '--namespace','lm-e2e-contract-test','--receiver-record','receiver-record',
    '--monkey-bin',cli,...extra],{cwd:resolve('.'),encoding:'utf8',timeout:9000,
    env:{...process.env,MOCK_MEMORY_STATE:state,MOCK_MEMORY_MODE:mode}});
  return {call,read:()=>JSON.parse(readFileSync(state,'utf8')),
    cleanup:()=>rmSync(dir,{recursive:true,force:true})};
}
test('refuses to access CLI before explicit operator confirmation',()=>{
  const ctx=fixture('ok');try{
    assert.equal(ctx.call().status,2);
    assert.equal(ctx.read().calls.length,0);
  }finally{ctx.cleanup();}
});
test('checks replicated receiver evidence, idempotency, and tombstone',()=>{
  const ctx=fixture('ok');try{
    const outcome=ctx.call(['--confirm-disposable']);
    assert.equal(outcome.status,0,outcome.stderr);
    const value=JSON.parse(outcome.stdout);
    assert.deepEqual(value.checks,['pairing-grants','bidirectional-sync',
      'receiver-owned-search','idempotent-repeat','versioned-tombstone']);
    assert.equal(ctx.read().sync,3);
    assert.equal(ctx.read().calls.filter(x=>x[1]==='memory-put').length,2);
  }finally{ctx.cleanup();}
});
test('never writes a vector without granted memory capabilities',()=>{
  const ctx=fixture('no-grant');try{
    assert.equal(ctx.call(['--confirm-disposable']).status,1);
    assert.equal(ctx.read().calls.filter(x=>x[1]==='memory-put').length,0);
  }finally{ctx.cleanup();}
});
test('cannot claim verified replication without receiver-owned data',()=>{
  for(const mode of ['no-remote','wrong-author']){
    const ctx=fixture(mode);try{
      const response=ctx.call(['--confirm-disposable']);
      assert.equal(response.status,1,mode);
      assert.equal(JSON.parse(response.stderr).status,'not_verified');
    }finally{ctx.cleanup();}
  }
});

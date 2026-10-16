import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {mkdtempSync,writeFileSync,chmodSync,readFileSync,existsSync,rmSync} from 'node:fs';
import {join,resolve} from 'node:path';
import {tmpdir} from 'node:os';
const script=resolve('scripts/subscription-native-auth.mjs');
test('official native Codex auth uses existing CLI, never reads tokens and gates completed answers',{
 skip:process.platform==='win32'?'POSIX fake executable smoke':false,
},()=>{
 const folder=mkdtempSync(join(tmpdir(),'lm-native-auth-test-'));
 try{
  const bin=join(folder,'codex-fake');
  const logfile=join(folder,'argv.json');
  const content=[
   '#!/usr/bin/env node',
   "const {writeFileSync}=require('node:fs');",
   "const command=process.argv.slice(2);",
   "writeFileSync(process.env.LM_AUTH_TEST_LOG,JSON.stringify(command));",
   "if(command[0]==='login'&&command[1]==='status')process.exit(0);",
   "if(command[0]==='exec'){",
   "  console.log(JSON.stringify({type:'item.completed',item:{type:'agent_message',text:'Read-only native response'}}));",
   "  console.log(JSON.stringify({type:'turn.completed'}));",
   "  process.exit(0);",
   "}",
   "process.exit(1);",
  ].join('\n')+'\n';
  writeFileSync(bin,content,{mode:0o700});
  chmodSync(bin,0o700);
  const invoke=(args)=>spawnSync(process.execPath,[script,'codex',...args],{
   encoding:'utf8',timeout:20000,
   env:{...process.env,LITTLE_MONKEY_CODEX_BIN:bin,LM_AUTH_TEST_LOG:logfile},
  });
  let r=invoke(['status']);
  assert.equal(r.status,0,r.stderr);
  assert.equal(JSON.parse(r.stdout).credentialBytesRead,0);
  r=invoke(['run','--prompt','Write a summary']);
  assert.notEqual(r.status,0,'Unsigned user consent must fail');
  r=invoke(['run','--prompt','Write a summary','--consent']);
  assert.equal(r.status,0,r.stderr);
  const output=JSON.parse(r.stdout);
  assert.equal(output.completed,true);
  assert.equal(output.response,'Read-only native response');
  assert.equal(output.usedOfficialClient,true);
  const args=JSON.parse(readFileSync(logfile,'utf8'));
  assert.deepEqual(args.slice(0,9),['exec','--json','--ephemeral',
   '--sandbox','read-only','--skip-git-repo-check','--cd',args[7],'Write a summary']);
  assert.ok(args[7].includes('lm-codex-native-'));
  assert.equal(existsSync(args[7]),false,'Ephemeral workspace must be deleted');
  assert.ok(!args.includes('--dangerously-bypass-approvals-and-sandbox'));
  assert.ok(!args.some(x=>String(x).includes('auth.json')));
 }finally{rmSync(folder,{recursive:true,force:true});}
});
test('Claude subscription API delegation is refused, native status only is supported',{
 skip:process.platform==='win32'?'POSIX fake executable smoke':false,
},()=>{
 const folder=mkdtempSync(join(tmpdir(),'lm-claude-auth-test-'));
 try{
  const bin=join(folder,'claude-fake');
  writeFileSync(bin,'#!/bin/sh\nif [ "$1" = "auth" ] && [ "$2" = "status" ]; then exit 0; fi\nexit 1\n',{mode:0o700});
  chmodSync(bin,0o700);
  const invoke=args=>spawnSync(process.execPath,[script,'claude',...args],{
   encoding:'utf8',env:{...process.env,LITTLE_MONKEY_CLAUDE_BIN:bin},
  });
  assert.equal(invoke(['status']).status,0);
  const attempt=invoke(['run','--prompt','hello','--consent']);
  assert.notEqual(attempt.status,0);
  assert.match(attempt.stderr,/cannot be used as a third-party model API/);
 }finally{rmSync(folder,{recursive:true,force:true});}
});

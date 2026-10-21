#!/usr/bin/env node
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {resolve} from 'node:path';
const entry=resolve('scripts/swarm-repository-repair-live-acceptance.mjs');
function invoke(args){
  return spawnSync(process.execPath,[entry,...args],
    {encoding:'utf8',cwd:resolve('.'),timeout:20000});
}
test('offline source fixture oracle is self-contained',()=>{
  const r=invoke(['--self-check']);
  assert.equal(r.status,0,r.stderr);
  assert.equal(JSON.parse(r.stdout).status,'source-oracle-passed');
});
test('no reviewer and same-model reviewer are rejected before CLI invocation',()=>{
  for(const extra of [[],['--review-target','ollama:coder']]){
    const r=invoke(['--target','ollama:coder',...extra,
      '--confirm-manual-approvals','--monkey-bin','not-a-real-installed-cli']);
    assert.equal(r.status,2,r.stderr);
    assert.match(r.stderr,/different-installed-model/);
  }
});
test('malformed reviewers are rejected without daemon execution',()=>{
  const r=invoke(['--target','ollama:coder','--review-target','http://untrusted',
    '--confirm-manual-approvals','--monkey-bin','not-a-real-installed-cli']);
  assert.equal(r.status,2,r.stderr);
});

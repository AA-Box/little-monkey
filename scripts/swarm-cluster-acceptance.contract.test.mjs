import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {dirname} from 'node:path';
const script=resolve(dirname(fileURLToPath(import.meta.url)),'swarm-cluster-live-acceptance.mjs');
describe('actual peer-quorum harness fails closed without operator-configured machines',()=>{
 it('refuses to access native election without operator confirmation',()=>{
  const result=spawnSync(process.execPath,[script,'--group','team'],{encoding:'utf8'});
  assert.equal(result.status,2);
  assert.match(result.stderr,/confirm-operator-cluster/);
 });
 it('rejects invalid group, majority claim or voter threshold before touching CLI',()=>{
  for(const args of [
   ['--group','../escape','--confirm-operator-cluster'],
   ['--group','team','--min-voters','1','--confirm-operator-cluster'],
   ['--group','team','--min-voters','NaN','--confirm-operator-cluster'],
  ]){
   const result=spawnSync(process.execPath,[script,...args],{encoding:'utf8'});
   assert.equal(result.status,2);
  }
 });
});

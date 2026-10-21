import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {resolve} from 'node:path';
const script=resolve('scripts/accept-real-agent-exports.mjs');
test('a real vendor export cannot be collected without explicit consent',()=>{
 const response=spawnSync(process.execPath,[script,'--out','/tmp/no-capture'],{
  encoding:'utf8',timeout:5000});
 assert.notEqual(response.status,0);
 assert.match(response.stderr,/explicit-private-export-consent/);
});
test('no fabricated session is permitted when a real vendor source is absent',()=>{
 const response=spawnSync(process.execPath,[script,
   '--explicit-private-export-consent',
   '--claude-file','/this/session-does-not-exist.jsonl'],{
  encoding:'utf8',timeout:5000});
 assert.notEqual(response.status,0);
 assert.match(response.stderr,/VENDOR EXPORT ACCEPTANCE FAILED/);
});

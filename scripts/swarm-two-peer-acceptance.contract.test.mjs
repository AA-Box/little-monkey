#!/usr/bin/env node
/**
 * Contract-test the operator-run two-node acceptance harness.
 * This intentionally uses a fake CLI only to verify harness parsing, grant
 * checks, correlation, terminal-result checks and non-retry policy.
 * Live machine/model verification is a separate, mandatory release gate.
 */
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { spawnSync } from 'node:child_process';
const script = resolve('scripts/swarm-two-peer-live-acceptance.mjs');

function fixture(mode) {
  const root = mkdtempSync(join(tmpdir(), 'swarm-peer-acceptance-contract-'));
  const executable = join(root, 'fake-monkey.mjs');
  const statePath = join(root, 'calls.json');
  writeFileSync(statePath, JSON.stringify({ calls: [] }));
  writeFileSync(executable, `#!/usr/bin/env node
import { readFileSync, writeFileSync } from 'node:fs';
const statePath = process.env.LITTLE_MONKEY_FAKE_STATE;
const mode = process.env.LITTLE_MONKEY_FAKE_MODE;
const args = process.argv.slice(2);
const data = JSON.parse(readFileSync(statePath, 'utf8'));
data.calls.push(args);
function output(value) { writeFileSync(statePath, JSON.stringify(data)); console.log(JSON.stringify(value)); }
if (args[0] !== 'peers') { process.exit(2); }
if (args[1] === 'status') output({granted: mode === 'no-grant' ? [] : ['task']});
else if (args[1] === 'send') {
  const index = args.indexOf('--correlation');
  if (index < 0 || !args[index+1]) process.exit(2);
  data.correlation = args[index+1];
  output({sent:true,thread_id:'thread-test',message_id:'message-test'});
} else if (args[1] === 'outbound') {
  output({messages:[{kind:'task_request',thread_id:'thread-test',message_id:'message-test',correlation_id:data.correlation,state:'accepted'}]});
} else if (args[1] === 'remote-thread') {
  output({messages:[{kind:'task_request',thread_id:'thread-test',message_id:'message-test',correlation_id:data.correlation,
    state:mode === 'remote-reject' ? 'rejected' : 'succeeded',
    result_text:mode === 'remote-reject' ? 'Denied by receiver' : 'README.md: verified project source'}]});
} else process.exit(2);
`);
  chmodSync(executable, 0o700);
  const run = (extra=[]) => spawnSync(process.execPath, [
    script,'--peer','test-node','--monkey-bin',executable,'--expect','README.md',
    '--timeout-seconds','15','--poll-seconds','1',...extra,
  ], {encoding:'utf8',timeout:15_000, env:{...process.env,LITTLE_MONKEY_FAKE_STATE:statePath,LITTLE_MONKEY_FAKE_MODE:mode}});
  return {run, calls:()=>JSON.parse(readFileSync(statePath,'utf8')).calls,
    cleanup:()=>rmSync(root,{recursive:true,force:true})};
}

test('harness accepts only a matching terminal result and never resends', () => {
  const instance=fixture('success');
  try {
    const run=instance.run();
    assert.equal(run.status,0,run.stderr);
    const result=JSON.parse(run.stdout);
    assert.equal(result.mode,'real-two-installation-peer-task');
    assert.equal(result.result.status,'verified_terminal_result');
    assert.equal(instance.calls().filter(c=>c[1]==='send').length,1);
    assert.equal(instance.calls().filter(c=>c[1]==='remote-thread').length,1);
    const outbound=instance.calls().find(c=>c[1]==='outbound');
    assert.deepEqual(outbound.slice(0,2),['peers','outbound']);
  } finally {instance.cleanup();}
});

test('harness rejects missing peer grants without dispatch', () => {
  const instance=fixture('no-grant');
  try {
    const run=instance.run();
    assert.equal(run.status,1);
    const report=JSON.parse(run.stderr);
    assert.match(report.error,/granted task requests/);
    assert.equal(instance.calls().filter(c=>c[1]==='send').length,0);
  } finally {instance.cleanup();}
});

test('harness fails on receiver-side rejection and does not retry dispatch', () => {
  const instance=fixture('remote-reject');
  try {
    const run=instance.run();
    assert.equal(run.status,1);
    const report=JSON.parse(run.stderr);
    assert.match(report.error,/terminated with state=rejected/);
    assert.equal(instance.calls().filter(c=>c[1]==='send').length,1);
  } finally {instance.cleanup();}
});

test('malformed input is rejected before a CLI is invoked', () => {
  const result=spawnSync(process.execPath,[script,'--peer','../invalid'],{encoding:'utf8',timeout:5_000});
  assert.equal(result.status,2);
  assert.match(result.stderr,/Usage:/);
});

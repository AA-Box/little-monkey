import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, chmodSync, rmSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { parseArgs, parseRunId, runBenchmark } from './swarm-headless-performance.mjs';

test('CLI arguments are bounded and rejects invalid/missing targets', () => {
  assert.throws(() => parseArgs([]), /target/);
  assert.throws(() => parseArgs(['--target','ollama:test','--workers','2,2']), /Invalid workload/);
  assert.throws(() => parseArgs(['--target','ollama:test','--workers','9']), /Invalid workload/);
  assert.throws(() => parseArgs(['--target','ollama:test','--workers','1','--unknown','yes']), /Unknown option/);
  assert.throws(() => parseArgs(['--target','ollama:test','--timeout-ms','5000']), /Invalid workload/);
  assert.deepEqual(parseArgs(['--target','ollama:qwen','--workers','1,2','--repetitions','2']).counts,[1,2]);
  assert.equal(parseRunId('progress\n{"run_id":"task-valid-task"}\n'),'task-valid-task');
  assert.throws(() => parseRunId('{"run_id":"../../evil"}'),/valid run id/);
});

test('concurrent harness checks independent fixtures and does not accept a no-op worker', async () => {
  const root = mkdtempSync(join(tmpdir(),'monkey-perf-harness-contract-'));
  const mock = join(root,'monkey');
  const source = String.raw`#!/usr/bin/env node
const fs = require('node:fs'), path = require('node:path');
const args = process.argv.slice(2);
if (args[0] !== 'task') process.exit(2);
if (args[1] === 'start') {
  const idx = args.indexOf('--workspace');
  if (idx < 0) process.exit(2);
  if (process.env.FAKE_MONKEY_BAD !== '1') {
    fs.writeFileSync(path.join(args[idx+1],'src/module.mjs'),
      'export const add = (a, b) => a + b;\n');
  }
  process.stdout.write('{"run_id":"task-test-1234"}\n');
} else if (args[1] === 'attach' || args[1] === 'cancel') {
  process.exit(0);
} else process.exit(2);
`;
  try {
    writeFileSync(mock,source,{mode:0o700});chmodSync(mock,0o700);
    const opts = parseArgs(['--target','mock:verified','--workers','1,2','--timeout-ms','10000','--monkey-bin',mock]);
    const good = await runBenchmark(opts);
    assert.equal(good.results.length,3);
    assert.equal(good.tiers.length,2);
    assert.ok(good.results.every(r=>r.verified));
    assert.ok(good.tiers.every(t=>t.throughputVerifiedPerMinute>0));
    assert.equal(good.classification,'task-latency-and-oracle-verified-throughput-not-daemon-memory');
    process.env.FAKE_MONKEY_BAD='1';
    const bad = await runBenchmark({...opts,counts:[1]});
    assert.equal(bad.results.length,1);
    assert.equal(bad.results[0].verified,false);
    assert.match(bad.results[0].error,/Command failed|command failed|Unapproved|status 1|node/);
    if(bad.results[0].repository) {
      assert.ok(existsSync(bad.results[0].repository));
      rmSync(bad.results[0].repository,{recursive:true,force:true});
    }
  } finally {
    delete process.env.FAKE_MONKEY_BAD;
    rmSync(root,{recursive:true,force:true});
  }
});

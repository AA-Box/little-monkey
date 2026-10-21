#!/usr/bin/env node
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {spawnSync} from 'node:child_process';
import {resolve} from 'node:path';
const cli=resolve('scripts/swarm-specialist-live-benchmark.mjs');
test('all fifteen independent source oracles pass without a configured model',()=>{
  const child=spawnSync(process.execPath,[cli,'--self-check','--domain','all'],{
    encoding:'utf8',timeout:25_000,maxBuffer:1024*1024,
  });
  assert.equal(child.status,0,child.stderr);
  const report=JSON.parse(child.stdout.trim());
  assert.equal(report.mode,'no-model');
  assert.equal(report.checkedFixtures,15);
  assert.deepEqual(report.domains,['security','frontend','backend','performance','general']);
  assert.match(report.warning,/not an actual model/);
});
test('each domain separately has exactly three source-grounded fixtures',()=>{
  for(const domain of ['security','frontend','backend','performance','general']){
    const child=spawnSync(process.execPath,[cli,'--self-check','--domain',domain],{
      encoding:'utf8',timeout:10_000,maxBuffer:1024*1024,
    });
    assert.equal(child.status,0,child.stderr);
    const report=JSON.parse(child.stdout.trim());
    assert.equal(report.checkedFixtures,3);
    assert.deepEqual(report.domains,[domain]);
  }
});
test('unsupported domain cannot silently fall back to security or run a model',()=>{
  const child=spawnSync(process.execPath,[cli,'--domain','unrecognized','--self-check'],{
    encoding:'utf8',timeout:5000,
  });
  assert.notEqual(child.status,0);
});
test('a real-model benchmark refuses to execute without an explicit model target',()=>{
  const child=spawnSync(process.execPath,[cli,'--domain','backend','--repetitions','1'],{
    encoding:'utf8',timeout:5000,
  });
  assert.equal(child.status,2);
});

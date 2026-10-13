#!/usr/bin/env node
/**
 * Live, real-model quality benchmark for Little Monkey's native task runner.
 * Requires installed monkey-cli and an explicitly chosen configured model
 * target. Uses disposable repos, actual model execution and independent tests.
 * No synthetic model completion is substituted for a real response.
 *
 * Usage:
 *   pnpm test:swarm-models:live -- --targets ollama:<model>,provider:<model>
 *   node scripts/swarm-model-benchmark.mjs --targets "<target1>,<target2>" --repetitions 3
 */
import { execFileSync, spawnSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { tmpdir } from 'node:os';

const arg = name => {
  const i = process.argv.indexOf(name);
  return i < 0 ? undefined : process.argv[i + 1];
};
const targets = (arg('--targets') ?? process.env.LITTLE_MONKEY_SWARM_BENCHMARK_TARGETS ?? '')
  .split(',').map(s => s.trim()).filter(Boolean);
const repetitions = Number(arg('--repetitions') ?? '1');
const binary = arg('--monkey-bin') ?? process.env.LITTLE_MONKEY_CLI ?? 'monkey';
const timeoutMs = Number(arg('--timeout-ms') ?? '600000');
const keep = process.env.LITTLE_MONKEY_KEEP_EVAL_REPO === '1';
if (!targets.length || targets.length > 8 || new Set(targets).size !== targets.length ||
    targets.some(t => t.length > 150 || /[\x00-\x1f]/.test(t)) ||
    !Number.isSafeInteger(repetitions) || repetitions < 1 || repetitions > 5 ||
    !Number.isSafeInteger(timeoutMs) || timeoutMs < 10_000 || timeoutMs > 3_600_000) {
  console.error('Provide --targets <target1,target2>, --repetitions 1..5 and optional --timeout-ms 10000..3600000.');
  process.exit(2);
}

function invoke(cmd, args, cwd, timeout = timeoutMs) {
  const result = spawnSync(cmd, args, {
    cwd, encoding: 'utf8', timeout, maxBuffer: 8 * 1024 * 1024,
    env: process.env,
  });
  if (result.error || result.status !== 0) throw Error(
    (result.error?.message ?? result.stderr?.trim() ?? result.stdout?.trim() ?? 'command failed').slice(0, 500),
  );
  return result.stdout;
}
function write(root, path, content) {
  mkdirSync(dirname(join(root, path)), { recursive: true });
  writeFileSync(join(root, path), content);
}
function parseRunId(output) {
  const matches = [output.trim(), ...output.split(/\r?\n/).reverse()].filter(line => line.startsWith('{'));
  for (const part of matches) {
    try {
      const payload = JSON.parse(part);
      const queue = [payload];
      while (queue.length) {
        const value = queue.pop();
        if (!value || typeof value !== 'object') continue;
        if (typeof value.run_id === 'string') return value.run_id;
        if (typeof value.runId === 'string') return value.runId;
        if (typeof value.id === 'string' && value.id.startsWith('run-')) return value.id;
        queue.push(...Object.values(value).filter(v => v && typeof v === 'object'));
      }
    } catch { /* ignore progress noise */ }
  }
  throw Error('Little Monkey CLI did not return a durable run id.');
}
const fixtures = [
  {
    id: 'logic-regression',
    source: 'export const total = (a, b) => a - b;\n',
    tests: 'import assert from "node:assert/strict";\nimport {total} from "./src/module.mjs";\nassert.equal(total(7,3),10);\nassert.equal(total(-1,-2),-3);\n',
    goal: 'Fix src/module.mjs so the test suite passes. Keep changes minimal. Do not modify tests or MONKEY.md. Run node test.mjs for verification.',
    mustNotContain: 'a - b',
  },
  {
    id: 'authorization-regression',
    source: 'export function isOwner(requestUserId, resourceOwnerId) { return requestUserId !== resourceOwnerId; }\n',
    tests: 'import assert from "node:assert/strict";\nimport {isOwner} from "./src/module.mjs";\nassert.equal(isOwner("alice","alice"),true);\nassert.equal(isOwner("alice","bob"),false);\n',
    goal: 'Fix the authorization correctness bug in src/module.mjs. Do not add a bypass or weaken tests. Do not modify tests or MONKEY.md. Verify with node test.mjs.',
    mustNotContain: '!== resourceOwnerId',
  },
  {
    id: 'parsing-regression',
    source: 'export function parseList(text) { return text.split(","); }\n',
    tests: 'import assert from "node:assert/strict";\nimport {parseList} from "./src/module.mjs";\nassert.deepEqual(parseList(" a , b, c "),["a","b","c"]);\nassert.deepEqual(parseList(""),[]);\n',
    goal: 'Fix src/module.mjs to trim items and omit empty entries, including a completely empty input. Do not modify test.mjs or MONKEY.md. Verify with node test.mjs.',
    mustNotContain: '.split(",");',
  },
];
function suite(target, fixture, repeat) {
  const root = mkdtempSync(join(tmpdir(), 'little-monkey-swarm-model-'));
  const start = Date.now();
  let runId;
  let passed = false;
  let error = null;
  try {
    write(root, 'package.json', '{"private":true,"type":"module"}\n');
    write(root, 'src/module.mjs', fixture.source);
    write(root, 'test.mjs', fixture.tests);
    write(root, 'MONKEY.md', 'Fix only src/module.mjs; never edit test.mjs or MONKEY.md. Run the tests. No network, no deployments.\n');
    invoke('git', ['init', '-q'], root);
    invoke('git', ['add', '.'], root);
    invoke('git', ['-c', 'user.name=Swarm Benchmark', '-c', 'user.email=benchmark@example.invalid',
      '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'], root);
    try {
      invoke(process.execPath, ['test.mjs'], root);
      throw Error('Benchmark fixture baseline unexpectedly passed.');
    } catch (cause) {
      if (String(cause).includes('baseline unexpectedly passed')) throw cause;
    }
    runId = parseRunId(invoke(binary, [
      'task', 'start', fixture.goal, '--target', target, '--workspace', root, '--json',
    ], root));
    invoke(binary, ['task','attach',runId,'--follow','--json'], root);
    invoke(process.execPath, ['test.mjs'], root);
    const changed = invoke('git', ['status','--porcelain=v1','--untracked-files=all'], root)
      .split(/\r?\n/).filter(Boolean).map(line => line.slice(3).trim());
    if (changed.length !== 1 || changed[0] !== 'src/module.mjs') throw Error('Model edited unrelated or no files.');
    if (readFileSync(join(root, 'src/module.mjs'), 'utf8').includes(fixture.mustNotContain)) {
      throw Error('Known faulty expression was not removed.');
    }
    passed = true;
  } catch (cause) {
    error = cause instanceof Error ? cause.message.slice(0, 500) : 'Unknown benchmark error';
  } finally {
    if (!keep) rmSync(root, { recursive: true, force: true });
  }
  return {
    target, fixture: fixture.id, repeat, runId, verified: passed,
    durationMs: Date.now() - start, ...(error ? { error } : {}),
    ...(keep ? { repository: root } : {}),
  };
}

const results = [];
for (const target of targets) {
  for (let repeat = 1; repeat <= repetitions; repeat++) {
    for (const fixture of fixtures) {
      const outcome = suite(target, fixture, repeat);
      results.push(outcome);
      console.error(`[${target}] ${fixture.id} #${repeat}: ${outcome.verified ? 'PASS' : 'FAIL'}`);
    }
  }
}
const ranking = targets.map(target => {
  const subset = results.filter(r => r.target === target);
  const passes = subset.filter(r => r.verified);
  return {
    target, passes: passes.length, attempts: subset.length,
    accuracy: Number((passes.length / subset.length).toFixed(4)),
    averageVerifiedMs: passes.length
      ? Math.round(passes.reduce((sum, r) => sum + r.durationMs, 0) / passes.length)
      : null,
  };
}).sort((a, b) => b.accuracy - a.accuracy ||
    (a.averageVerifiedMs ?? Infinity) - (b.averageVerifiedMs ?? Infinity));

console.log(JSON.stringify({
  version: 1, benchmark: 'live-task-verified-fixtures', mode: 'real-model',
  generatedAt: new Date().toISOString(), ranking, results,
}, null, 2));
if (results.some(r => !r.verified)) process.exitCode = 1;

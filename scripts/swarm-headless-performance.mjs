#!/usr/bin/env node
/**
 * Headless concurrent real-model acceptance benchmark. All samples run the
 * installed Little Monkey CLI and independently execute a failing-before
 * source fixture after completion. No synthetic completions or self-grades.
 *
 * node scripts/swarm-headless-performance.mjs --target ollama:qwen2.5:7b \
 *   --workers 1,2,4 --repetitions 2 --timeout-ms 600000
 *
 * The output is end-to-end task latency and verified throughput, NOT
 * daemon RSS/CPU or a memory comparison with JCode. Those measurements need
 * a separate, OS-specific, process-tree sampling harness.
 */
import { execFile, execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { join, dirname, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { pathToFileURL } from 'node:url';

export function parseArgs(argv) {
  const opts = new Map();
  for (let i = 0; i < argv.length; i += 2) {
    if (!/^--[a-z-]+$/.test(argv[i]) || !argv[i + 1] || opts.has(argv[i])) {
      throw Error('Expected unique --target, --workers, --repetitions, --timeout-ms, --monkey-bin options.');
    }
    opts.set(argv[i], argv[i + 1]);
  }
  for (const key of opts.keys()) {
    if (!['--target','--workers','--repetitions','--timeout-ms','--monkey-bin'].includes(key)) {
      throw Error('Unknown option: ' + key);
    }
  }
  const target = opts.get('--target') ?? '';
  if (!target || target.length > 150 || /[\x00-\x1f]/.test(target)) {
    throw Error('Explicit bounded --target is required.');
  }
  const counts = (opts.get('--workers') ?? '1,2,4').split(',').map(Number);
  const repetitions = Number(opts.get('--repetitions') ?? 1);
  const timeoutMs = Number(opts.get('--timeout-ms') ?? 600000);
  if (!counts.length || counts.length > 4 || counts.some(n => !Number.isSafeInteger(n) || n < 1 || n > 8) ||
      new Set(counts).size !== counts.length || !Number.isSafeInteger(repetitions) ||
      repetitions < 1 || repetitions > 3 || !Number.isSafeInteger(timeoutMs) ||
      timeoutMs < 10000 || timeoutMs > 3600000 ||
      counts.reduce((s, n) => s + n, 0) * repetitions > 48) {
    throw Error('Invalid workload: worker counts 1..8, max four tiers, repetitions 1..3, <=48 tasks, timeout 10s..1h.');
  }
  const binary = opts.get('--monkey-bin') ?? process.env.LITTLE_MONKEY_CLI ?? 'monkey';
  if (!binary || /[\x00-\x1f]/.test(binary) || binary.length > 1024) {
    throw Error('Invalid CLI executable.');
  }
  return { target, counts, repetitions, timeoutMs, binary };
}
export function parseRunId(output) {
  for (const line of [output.trim(), ...output.trim().split(/\r?\n/).reverse()]) {
    try {
      const json = JSON.parse(line);
      if (typeof json?.run_id === 'string' && /^(?:task|run)-[a-zA-Z0-9_-]{4,150}$/.test(json.run_id)) return json.run_id;
      if (typeof json?.runId === 'string' && /^(?:task|run)-[a-zA-Z0-9_-]{4,150}$/.test(json.runId)) return json.runId;
    } catch { /* structured progress is optional */ }
  }
  throw Error('CLI did not return a valid run id.');
}
function invoke(program, args, cwd, timeoutMs) {
  return new Promise((fulfil, reject) => {
    execFile(program, args, { cwd, encoding:'utf8', timeout:timeoutMs, maxBuffer:8*1024*1024 }, (error, stdout, stderr) => {
      if (error) reject(new Error((error.message + ' ' + (stderr ?? '')).slice(0,500)));
      else fulfil(stdout);
    });
  });
}
function fixture(root) {
  const file = join(root,'src/module.mjs');
  mkdirSync(dirname(file),{recursive:true});
  writeFileSync(file,'export const add = (a, b) => a - b;\n');
  writeFileSync(join(root,'package.json'),'{"private":true,"type":"module"}\n');
  writeFileSync(join(root,'test.mjs'),
    'import assert from "node:assert/strict"; import {add} from "./src/module.mjs"; ' +
    'assert.equal(add(7,3),10); assert.equal(add(-4,2),-2);\n');
  writeFileSync(join(root,'MONKEY.md'),
    'Only src/module.mjs may change. Run node test.mjs. Do not change tests or instructions. Do not deploy or access the network.\n');
  const git = (...args) => execFileSync('git',args,{cwd:root,encoding:'utf8',timeout:30000});
  git('init','-q');
  git('add','.');
  git('-c','user.name=Benchmark','-c','user.email=benchmark@example.invalid',
    '-c','commit.gpgsign=false','commit','-qm','baseline');
  try {
    execFileSync(process.execPath,['test.mjs'],{cwd:root,stdio:'ignore',timeout:10000});
    throw Error('Invalid benchmark fixture: baseline passed.');
  } catch (error) {
    if (String(error).includes('baseline passed')) throw error;
  }
}
async function oneTask(opts, tier, repetition, index) {
  const root = mkdtempSync(join(tmpdir(),'monkey-headless-perf-'));
  let runId, verified = false, error = null;
  const started = performance.now();
  try {
    fixture(root);
    const output = await invoke(opts.binary,[
      'task','start','Fix src/module.mjs so node test.mjs passes. Never edit tests, instructions, or other files. Verify results.',
      '--target',opts.target,'--workspace',root,'--json',
    ],root,opts.timeoutMs);
    runId = parseRunId(output);
    await invoke(opts.binary,['task','attach',runId,'--follow','--json'],root,opts.timeoutMs);
    execFileSync(process.execPath,['test.mjs'],{cwd:root,stdio:'pipe',timeout:10000});
    const changed = execFileSync('git',['status','--porcelain=v1','--untracked-files=all'],
      {cwd:root,encoding:'utf8',timeout:30000}).split(/\r?\n/).filter(Boolean).map(s=>s.slice(3));
    if (changed.length !== 1 || changed[0] !== 'src/module.mjs') throw Error('Unapproved, missing, or unrelated changes.');
    if (readFileSync(join(root,'src/module.mjs'),'utf8').includes('a - b')) throw Error('Incorrect expression remains.');
    verified = true;
  } catch (cause) {
    error = cause instanceof Error ? cause.message.slice(0,500) : 'Task failed';
    if (runId) {
      // Explicit cancellation on timeout/failure before disposing the fixture.
      try { await invoke(opts.binary,['task','cancel',runId],root,10000); } catch { /* report original failure */ }
    }
  } finally {
    if (process.env.LITTLE_MONKEY_KEEP_EVAL_REPO !== '1') {
      // Cancel was requested above before removal. A stuck executor still
      // requires operator inspection of the durable run.
      if (verified || !runId) rmSync(root,{recursive:true,force:true});
    }
  }
  return {tier,repetition,index,runId:runId??null,verified,
    durationMs:Math.round(performance.now()-started),...(error?{error}:{}),
    ...(process.env.LITTLE_MONKEY_KEEP_EVAL_REPO==='1'||(!verified&&runId)?{repository:root}:{})};
}
export async function runBenchmark(opts) {
  const results=[], tiers=[];
  for (const count of opts.counts) {
    for (let repetition=1;repetition<=opts.repetitions;repetition++) {
      const start=performance.now();
      const batch=await Promise.all(Array.from({length:count},(_,index)=>
        oneTask(opts,count,repetition,index+1)));
      const wallMs=Math.max(1,Math.round(performance.now()-start));
      results.push(...batch);
      tiers.push({workers:count,repetition,wallMs,
        verified:batch.filter(x=>x.verified).length,
        throughputVerifiedPerMinute:Number((batch.filter(x=>x.verified).length*60000/wallMs).toFixed(4)),
        latencyMs:batch.map(x=>x.durationMs).sort((a,b)=>a-b)});
    }
  }
  return {version:1,benchmark:'headless-real-model-concurrency',target:opts.target,
    generatedAt:new Date().toISOString(),classification:'task-latency-and-oracle-verified-throughput-not-daemon-memory',
    tiers,results};
}
async function main() {
  let options;
  try { options=parseArgs(process.argv.slice(2)); } catch(error) {
    console.error(error.message);process.exitCode=2;return;
  }
  const outcome=await runBenchmark(options);
  console.log(JSON.stringify(outcome,null,2));
  if(outcome.results.some(r=>!r.verified))process.exitCode=1;
}
if(process.argv[1] && import.meta.url===pathToFileURL(resolve(process.argv[1])).href) await main();

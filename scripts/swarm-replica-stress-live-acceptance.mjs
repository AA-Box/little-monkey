#!/usr/bin/env node
/**
 * Operator-only two-installation checkpoint stress gate. Uses the installed
 * monkey CLI's signed, TLS-pinned, exact readback-verified swarm-replicate.
 * Never configures peers, grants, machines, leader leases or workspaces.
 * Concurrent publications deliberately expose SQLite, transport and ordering
 * races. ANY uncertain attempt fails the entire run; there are no retries.
 *
 * This is NOT proof of physical isolation: use the separate three-stage
 * network-recovery harness on independently operated machines first.
 */
import { spawn } from 'node:child_process';
import { performance } from 'node:perf_hooks';

const value = key => {
  const i = process.argv.indexOf(key);
  return i === -1 ? null : (process.argv[i + 1] ?? null);
};
const validId = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,79}$/;
const validFingerprint = /^[0-9a-f]{64}$/;
const group = value('--group');
const graph = value('--graph');
const peer = value('--peer');
const bin = value('--monkey-bin') ?? process.env.LITTLE_MONKEY_CLI ?? 'monkey';
const rounds = Number(value('--rounds') ?? 30);
const concurrency = Number(value('--concurrency') ?? 1);
const additionalGraphs = value('--additional-graphs')?.split(',') ?? [];
const graphs = [graph, ...additionalGraphs];
const maxP95Ms = Number(value('--max-p95-ms') ?? 30_000);
const confirmed = process.argv.includes('--confirm-operator-owned-readonly-cluster') &&
  process.argv.includes('--confirm-independent-machines') &&
  process.argv.includes('--confirm-post-recovery');
const uniqueFlags = ['--group','--graph','--peer','--monkey-bin','--rounds',
  '--concurrency','--max-p95-ms','--additional-graphs'];
const duplicatedFlag = uniqueFlags.some(flag => process.argv.filter(item => item === flag).length > 1);
const usage = 'Usage: --group GROUP --graph PRESTAGED_GRAPH --peer ENROLLED_ALIAS ' +
  '[--rounds 10..500] [--concurrency 1..8] [--additional-graphs id1,id2,...] [--max-p95-ms 1000..60000] ' +
  '--confirm-operator-owned-readonly-cluster --confirm-independent-machines ' +
  '--confirm-post-recovery [--monkey-bin PATH]';
if (duplicatedFlag || ![group,graph,peer].every(s => validId.test(s ?? '')) ||
  !graphs.every(s => validId.test(s ?? '')) || new Set(graphs).size !== graphs.length ||
  graphs.length > 8 || concurrency > graphs.length ||
  !Number.isSafeInteger(rounds) || rounds < 10 || rounds > 500 ||
  !Number.isSafeInteger(concurrency) || concurrency < 1 || concurrency > 8 ||
  concurrency > rounds || !Number.isSafeInteger(maxP95Ms) ||
  maxP95Ms < 1000 || maxP95Ms > 60_000 || !confirmed) {
  console.error(usage);
  process.exit(2);
}

/** Bound each child's runtime and stdout. Never leak stderr or command output
 * into acceptance reports: they may contain private process details. */
function cli(args, timeoutMs = 35_000) {
  return new Promise((resolveResult, reject) => {
    const child = spawn(bin, args, { stdio: ['ignore','pipe','ignore'], env: process.env });
    let stdout = '';
    let settled = false;
    const finish = (error, output) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      if (error) reject(error); else resolveResult(output);
    };
    const timer = setTimeout(() => {
      child.kill('SIGKILL');
      finish(Error('Timed out native command: ' + args.slice(0,2).join(' ')));
    }, timeoutMs);
    child.stdout.on('data', buf => {
      stdout += buf.toString('utf8');
      if (stdout.length > 256*1024) {
        child.kill('SIGKILL');
        finish(Error('Native command output exceeded limit'));
      }
    });
    child.on('error', () => finish(Error('Unable to launch installed native CLI')));
    child.on('close', code => {
      if (settled) return;
      if (code !== 0) return finish(Error('Native command failed: ' + args.slice(0,2).join(' ')));
      try { finish(null, JSON.parse(stdout.trim())); }
      catch { finish(Error('Native command returned invalid JSON')); }
    });
  });
}

const must = (condition, error) => { if (!condition) throw Error(error); };
async function cluster() {
  const v = await cli(['peers','cluster-status',group]);
  must(v?.group === group && v.may_schedule === true &&
    validFingerprint.test(v.local_fingerprint ?? '') &&
    validFingerprint.test(v.roster_sha256 ?? ''),
    'No active signed cluster majority and scheduling lease');
  return v;
}
async function contact() {
  const v = await cli(['peers','status',peer,'--json']);
  must(v?.alias === peer && v.presence === 'online' &&
    typeof v.peer_id === 'string' && v.peer_id.length > 0 && v.peer_id.length < 200 &&
    Array.isArray(v.granted) && v.granted.includes('task'),
    'A fresh signed peer hello did not confirm the expected task-granted identity');
  return v;
}
async function publish(graphId) {
  const started = performance.now();
  const value = await cli(['daemon','swarm-replicate',group,graphId,peer], 45_000);
  const receipt = value?.receipt;
  must(value?.remote_readback_verified === true &&
    value?.execution_authorized === false && receipt?.accepted === true &&
    receipt?.group === group && receipt?.graph_id === graphId &&
    Number.isSafeInteger(receipt?.sequence) && receipt.sequence > 0,
    'No matching, durable, remote TLS-signed SQLite checkpoint readback');
  return {graph:graphId, sequence:receipt.sequence, latencyMs:performance.now()-started};
}

async function run() {
  const started = performance.now();
  const before = await cluster();
  const identity = await contact();
  // One writer per graph: the receiver intentionally rejects stale snapshots
  // for a single graph. Competing writers on one graph are not a valid
  // all-success stress test. Separate preapproved graph keys allow real
  // parallel transport/SQLite contention without violating monotonicity.
  const active = graphs.slice(0,concurrency);
  const baselines = await Promise.all(active.map(id => publish(id)));
  const baselineByGraph = new Map(baselines.map(v => [v.graph,v.sequence]));
  const observations = [];
  const errors = [];
  async function worker(index) {
    const assignedGraph = active[index];
    for (let step=index;step<rounds && errors.length===0;step+=concurrency) {
      try { observations.push(await publish(assignedGraph)); }
      catch (e) { errors.push(String(e)); }
    }
  }
  await Promise.all(active.map((_,i) => worker(i)));
  const last = await cluster();
  const remote = await contact();
  must(last.local_fingerprint === before.local_fingerprint &&
    last.roster_sha256 === before.roster_sha256 &&
    remote.peer_id === identity.peer_id,
    'Cluster roster, sender or receiver identity changed during stress');
  must(errors.length === 0 && observations.length === rounds,
    'At least one signed publication failed or could not be verified');
  const perGraph = active.map(id => {
    const sequences = observations.filter(v=>v.graph===id).map(v=>v.sequence);
    must(sequences.every(s=>s>baselineByGraph.get(id)) &&
      new Set(sequences).size === sequences.length,
      'Lost or duplicated a durable checkpoint sequence under concurrency');
    return {graph:id,baselineSequence:baselineByGraph.get(id),
      minSequence:Math.min(...sequences),maxSequence:Math.max(...sequences),
      uniqueSequences:sequences.length};
  });
  const timings = observations.map(v=>v.latencyMs).sort((a,b)=>a-b);
  const p95 = timings[Math.ceil(timings.length*0.95)-1];
  must(p95 <= maxP95Ms, 'Observed p95 latency exceeded operator-selected gate');
  return {
    mode:'operator-two-installation-signed-checkpoint-stress', status:'verified_live_cli',
    operatorAttestsIndependentMachines:true,
    independentHardwareAttestation:false,
    group, graph, peer, sourceFingerprint:before.local_fingerprint,
    rosterSha256:before.roster_sha256, receiverInstanceId:identity.peer_id,
    rounds, concurrency, perGraph,
    uniqueSequences:rounds, failedPublications:0,
    elapsedMs:Math.round(performance.now()-started),
    throughputPerSecond:Number((rounds/((performance.now()-started)/1000)).toFixed(2)),
    p50Ms:Math.round(timings[Math.ceil(timings.length*0.5)-1]),
    p95Ms:Math.round(p95), maxP95Ms,
    proof:'Each success was a signed peer POST plus matching remote SQLite readback, not a mock ACK.',
    limitation:'Physical independence and actual network cut require separate externally witnessed operator evidence.'
  };
}
try { console.log(JSON.stringify(await run(),null,2)); }
catch (e) {
  console.error(JSON.stringify({mode:'operator-two-installation-signed-checkpoint-stress',
    status:'not_verified',group,graph,peer,error:String(e).slice(0,300),
    note:'Do not treat uncertain writes as successful and do not retry automatically.'}));
  process.exitCode = 1;
}

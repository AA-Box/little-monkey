#!/usr/bin/env node
/**
 * Real, operator-approved vector-memory test across TWO installed nodes.
 * NEVER configures pairing/ACLs or selects remote endpoints.
 * All test state belongs to an explicitly disposable lm-e2e- namespace.
 * The other node must already contain a locally authored receiver record
 * using the same model and vector; run separately with roles reversed.
 */
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';

const arg = name => { const i = process.argv.indexOf(name); return i < 0 ? null : process.argv[i + 1] ?? null; };
const peer = arg('--peer');
const namespace = arg('--namespace');
const remoteId = arg('--receiver-record');
const bin = arg('--monkey-bin') ?? process.env.LITTLE_MONKEY_CLI ?? 'monkey';
const approved = process.argv.includes('--confirm-disposable');
const identifier = /^[A-Za-z0-9][A-Za-z0-9_.-]{0,119}$/;
if (!approved || !identifier.test(peer ?? '') ||
    !/^lm-e2e-[a-z0-9-]{3,60}$/.test(namespace ?? '') ||
    !identifier.test(remoteId ?? '')) {
  console.error('Usage: --peer PAIRED_ALIAS --namespace lm-e2e-DISPOSABLE --receiver-record REMOTE_LOCAL_RECORD --confirm-disposable [--monkey-bin PATH]');
  process.exit(2);
}

function run(args) {
  const result = spawnSync(bin, args, { encoding: 'utf8', timeout: 30_000,
    maxBuffer: 2 * 1024 * 1024, env: process.env });
  // Do not leak a provider error body, vector text or transport secrets.
  if (result.error || result.status !== 0) throw Error('CLI failed: ' + args.slice(0, 2).join(' '));
  try { return JSON.parse(result.stdout.trim()); }
  catch { throw Error('CLI produced invalid JSON: ' + args.slice(0, 2).join(' ')); }
}

const vector = '1,0,0';
const model = 'lm-e2e-vector-v1';
const ownId = 'own-' + randomUUID().replaceAll('-', '').slice(0, 24);
let created = false;
const checks = [];
try {
  const status = run(['peers', 'status', peer, '--json']);
  if (!Array.isArray(status.granted) || !['memory-read', 'memory-write'].every(grant => status.granted.includes(grant))) {
    throw Error('Peer memory-read and memory-write grants are required.');
  }
  checks.push('pairing-grants');
  const put = run(['peers', 'memory-put', namespace, ownId, model, vector,
    'disposable verification ' + ownId, '--revision', '1']);
  if (put.updated !== true || put.id !== ownId) throw Error('Cannot verify local vector insertion.');
  created = true;
  const first = run(['peers', 'memory-sync', peer, namespace]);
  if (!Number.isInteger(first.pulled) || first.pulled < 1 ||
      !Number.isInteger(first.pushed) || first.pushed < 1) {
    throw Error('An actual bidirectional replication exchange was not observed.');
  }
  checks.push('bidirectional-sync');
  const found = run(['peers', 'memory-search', namespace, model, vector, '--top-k', '10']);
  if (!Array.isArray(found.matches) ||
      !found.matches.some(item => item?.entry?.id === remoteId &&
        item.entry.author !== '@local' && item.entry.deleted === false)) {
    throw Error('An actual receiver-authored vector was not returned from local search.');
  }
  checks.push('receiver-owned-search');
  const repeated = run(['peers', 'memory-sync', peer, namespace]);
  if (repeated.pulled !== 0) throw Error('Repeated sync was not idempotent.');
  checks.push('idempotent-repeat');
  const removal = run(['peers', 'memory-put', namespace, ownId, model, vector, '',
    '--revision', '2', '--deleted']);
  if (removal.updated !== true || removal.revision !== 2) {
    throw Error('Revision-2 tombstone was not durably accepted.');
  }
  const tombstoneSync = run(['peers', 'memory-sync', peer, namespace]);
  if (!Number.isInteger(tombstoneSync.pushed) || tombstoneSync.pushed < 1) {
    throw Error('The receiver did not acknowledge tombstone replication.');
  }
  const after = run(['peers', 'memory-search', namespace, model, vector, '--top-k', '10']);
  if (!Array.isArray(after.matches) || after.matches.some(item =>
      item?.entry?.id === ownId && item.entry.author === '@local')) {
    throw Error('A tombstoned local record remained queryable.');
  }
  checks.push('versioned-tombstone');
  console.log(JSON.stringify({mode:'real-two-installation-memory',status:'verified',
    namespace, peer, localId:ownId, receiverId:remoteId, checks,
    note:'Repeat from the other machine; no peer grants were changed.'},null,2));
} catch (error) {
  console.error(JSON.stringify({mode:'real-two-installation-memory',status:'not_verified',
    namespace, peer, localId: created ? ownId : null, checks, error:String(error),
    note:'Do not automatically retry an uncertain outcome; inspect the disposable namespace.'},null,2));
  process.exitCode = 1;
}

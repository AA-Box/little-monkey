#!/usr/bin/env node
/**
 * Operator-run acceptance test against TWO REAL paired Little Monkey
 * installations. The current machine is the sender; --peer names a separately
 * configured recipient. Uses the real monkey-cli, signed TLS peer transport,
 * actual receiver-side model, durable outbound receipts, and terminal results.
 *
 * Never creates pairings, broadens grants, changes policies or selects a
 * receiver workspace. The remote peer-task recipe MUST be manually configured
 * for read-only test work before starting.
 *
 * Example:
 *   node scripts/swarm-two-peer-live-acceptance.mjs --peer lab-mac \
 *       --expect "README" --timeout-seconds 180
 */
import { spawnSync } from 'node:child_process';
import { randomUUID } from 'node:crypto';

function flag(name) {
  const position = process.argv.indexOf(name);
  return position >= 0 ? process.argv[position + 1] : undefined;
}
const alias = flag('--peer');
const binary = flag('--monkey-bin') ?? process.env.LITTLE_MONKEY_CLI ?? 'monkey';
const expectText = flag('--expect');
const timeoutSeconds = Number(flag('--timeout-seconds') ?? '300');
const intervalSeconds = Number(flag('--poll-seconds') ?? '5');
if (!alias || !/^[A-Za-z0-9][A-Za-z0-9_.-]{0,119}$/.test(alias) ||
    !Number.isSafeInteger(timeoutSeconds) || timeoutSeconds < 15 || timeoutSeconds > 900 ||
    !Number.isSafeInteger(intervalSeconds) || intervalSeconds < 1 || intervalSeconds > 60 ||
    (expectText && expectText.length > 300)) {
  console.error('Usage: --peer PAIRED_ALIAS [--monkey-bin PATH] [--expect EXACT_TEXT] [--timeout-seconds 15..900] [--poll-seconds 1..60]');
  process.exit(2);
}

function run(args) {
  const result = spawnSync(binary, args, {
    encoding: 'utf8', timeout: 30_000, maxBuffer: 2 * 1024 * 1024,
    env: process.env,
  });
  if (result.error || result.status !== 0) {
    throw Error('monkey ' + args.slice(0, 2).join(' ') + ': ' +
      String(result.error?.message ?? result.stderr?.trim() ?? result.stdout?.trim() ?? 'failed').slice(0, 500));
  }
  const output = result.stdout.trim();
  try { return JSON.parse(output); }
  catch { throw Error('CLI response was not valid machine-readable JSON: ' + output.slice(0, 250)); }
}
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const startedAt = Date.now();
const correlation = 'live-' + randomUUID().replaceAll('-', '').slice(0, 24);
let outbound = null;
let verification = null;
try {
  const status = run(['peers', 'status', alias, '--json']);
  if (!Array.isArray(status.granted) || !status.granted.includes('task')) {
    throw Error('The receiving peer has not granted task requests; configure pairing first.');
  }
  const prompt = [
    'Read-only live swarm acceptance test. Do not edit files or execute deployment.',
    'Examine the receiver\'s operator-configured test repository. Report a real',
    'source-file path and one verifiable observation from that file.',
    'If no repository is configured, say so instead of fabricating evidence.',
    'Acceptance correlation: ' + correlation,
  ].join(' ');
  // New correlation and read-only request: no existing production task is
  // replayed; the recipient remains authoritative for model/tools/approvals.
  run(['peers', 'send', alias, prompt, '--task', '--correlation', correlation, '--json']);
  const records = run(['peers', 'outbound', '--alias', alias, '--limit', '200', '--json']);
  outbound = records.messages?.find(item => item.correlation_id === correlation &&
    item.kind === 'task_request');
  if (!outbound?.thread_id || !outbound?.message_id) {
    throw Error('No durable outbound task receipt found; inspect Peers history before retrying.');
  }
  const deadline = Date.now() + timeoutSeconds * 1000;
  let lastTransientError = null;
  while (Date.now() < deadline) {
    try {
      const rows = run(['peers', 'remote-thread', alias, outbound.thread_id, '--json']);
      const result = rows.messages?.find(item => item.correlation_id === correlation &&
        item.message_id === outbound.message_id);
      if (result?.state === 'succeeded') {
        if (typeof result.result_text !== 'string' || !result.result_text.trim()) {
          throw Error('Receiver reported succeeded but returned no actual model result.');
        }
        if (expectText && !result.result_text.includes(expectText)) {
          throw Error('The real peer result did not contain the explicitly expected evidence string.');
        }
        verification = { status: 'verified_terminal_result', textLength: result.result_text.length };
        break;
      }
      if (['failed', 'rejected', 'cancelled'].includes(result?.state)) {
        throw Error('Remote task terminated with state=' + result.state + ': ' +
          String(result.result_text ?? 'no details').slice(0, 300));
      }
      lastTransientError = null;
    } catch (error) {
      if (/terminated with state|returned no actual|expected evidence/.test(String(error))) throw error;
      lastTransientError = String(error);
      // Loss of connectivity is not proof of failure. Poll the SAME receipt
      // after reconnecting; never submit a second task automatically.
    }
    await sleep(intervalSeconds * 1000);
  }
  if (!verification) throw Error('No successful terminal result before timeout. ' +
    (lastTransientError ?? 'Inspect the same durable thread; do not resend blindly.'));
  console.log(JSON.stringify({
    mode: 'real-two-installation-peer-task',
    receiverAlias: alias, messageId: outbound.message_id, threadId: outbound.thread_id,
    correlation, durationMs: Date.now() - startedAt,
    result: verification, note: 'This proves transport and real receiver task completion, not arbitrary code-swarm parity.',
  }, null, 2));
} catch (error) {
  console.error(JSON.stringify({
    mode: 'real-two-installation-peer-task', status: 'not_verified',
    receiverAlias: alias, correlation,
    threadId: outbound?.thread_id ?? null, messageId: outbound?.message_id ?? null,
    error: String(error),
    note: 'An uncertain outcome is not retried automatically. Inspect its durable receipt.',
  }, null, 2));
  process.exitCode = 1;
}

#!/usr/bin/env node
/** Delegate only to installed official clients; never inspect their credential stores. */
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [provider, action, ...options] = process.argv.slice(2);
const binaries = {
  codex: process.env.LITTLE_MONKEY_CODEX_BIN ?? 'codex',
  claude: process.env.LITTLE_MONKEY_CLAUDE_BIN ?? 'claude',
  cursor: process.env.LITTLE_MONKEY_CURSOR_BIN ?? 'cursor-agent',
};
const messages = {
  CONSENT_REQUIRED: 'Explicit --consent required',
  INVALID_PROMPT: 'Requires an explicit prompt of 1–4000 characters',
  INVALID_TIMEOUT: 'Choose a timeout between 100 and 300000 milliseconds',
  CLI_MISSING: 'Official client executable is unavailable',
  STATUS_TIMEOUT: 'Official client status check timed out',
  SESSION_EXPIRED: 'Official client session is missing or expired; sign in again',
  API_KEY_SESSION: 'A separately billed API-key session is not subscription sign-in',
  SESSION_UNVERIFIED: 'Official client did not confirm subscription sign-in',
  AUTH_STATUS_FAILED: 'Official client status check failed',
  CLIENT_TIMEOUT: 'Official client timed out',
  CANCELLED: 'Official client operation was cancelled',
  CLIENT_FAILED: 'Official client exited unsuccessfully',
  OUTPUT_LIMIT: 'Official client output exceeded the allowed size',
  PROTOCOL_ERROR: 'Official client returned an invalid event stream',
  TURN_FAILED: 'Official client reported a failed turn',
  INCOMPLETE_TURN: 'Official client did not complete an assistant turn',
  UNSUPPORTED_ACTION: 'Subscription execution is unavailable for this client; open the official client instead',
};
class NativeAuthError extends Error {
  constructor(code) { super(messages[code]); this.code = code; }
}
const fail = (code) => { throw new NativeAuthError(code); };
const option = (name) => {
  const index = options.indexOf(name);
  return index < 0 ? null : options[index + 1] ?? null;
};

if (!Object.hasOwn(binaries, provider) || !['status', 'login', 'run', 'open'].includes(action)) {
  console.error('Usage: subscription-native-auth.mjs codex|claude|cursor status|login|run|open [--prompt TEXT] [--timeout-ms N] --consent');
  process.exit(2);
}
const bin = binaries[provider];

function vendorEnvironment() {
  // A parent API key or custom provider setting must not silently change
  // subscription execution into separately billed or third-party routing.
  const env = { ...process.env };
  for (const name of [
    'OPENAI_API_KEY', 'CODEX_API_KEY', 'CODEX_ACCESS_TOKEN',
    'OPENAI_BASE_URL', 'OPENAI_API_BASE', 'CODEX_BASE_URL',
    'CODEX_API_BASE_URL', 'OPENAI_ORG_ID', 'OPENAI_PROJECT_ID',
    'ANTHROPIC_API_KEY', 'ANTHROPIC_AUTH_TOKEN', 'ANTHROPIC_BASE_URL',
    'CLAUDE_CODE_USE_BEDROCK', 'CLAUDE_CODE_USE_VERTEX', 'CLAUDE_CODE_USE_FOUNDRY',
    'CURSOR_API_KEY', 'CURSOR_BASE_URL', 'CURSOR_ENDPOINT',
  ]) delete env[name];
  return env;
}
function timeoutMs() {
  const requested = option('--timeout-ms');
  if (requested === null) return action === 'run' ? 120000 : 300000;
  const value = Number(requested);
  if (!Number.isInteger(value) || value < 100 || value > 300000) fail('INVALID_TIMEOUT');
  return value;
}
function approved() {
  if (!options.includes('--consent')) fail('CONSENT_REQUIRED');
}
function sessionStatus() {
  const command = provider === 'codex' ? ['login', 'status'] :
    provider === 'claude' ? ['auth', 'status'] : ['status'];
  const result = spawnSync(bin, command, {
    encoding: 'utf8', timeout: 15000, maxBuffer: 65536,
    stdio: ['ignore', 'pipe', 'pipe'], env: vendorEnvironment(),
  });
  if (result.error?.code === 'ENOENT') return { authenticated: false, code: 'CLI_MISSING' };
  if (result.error?.code === 'ETIMEDOUT') return { authenticated: false, code: 'STATUS_TIMEOUT' };
  const output = String(result.stdout ?? '') + '\n' + String(result.stderr ?? '');
  if (result.error || result.status !== 0) {
    const expired = /expired|not logged in|logged out|not authenticated|please log in/i.test(output);
    return { authenticated: false, code: expired ? 'SESSION_EXPIRED' : 'AUTH_STATUS_FAILED' };
  }
  if (provider === 'codex') {
    if (/logged\s+in[^\r\n]*chatgpt/i.test(output)) return { authenticated: true };
    if (/api[ -]?key/i.test(output)) return { authenticated: false, code: 'API_KEY_SESSION' };
    return { authenticated: false, code: 'SESSION_UNVERIFIED' };
  }
  if(provider==='cursor'){
    // The official Cursor CLI exposes browser login/status. Never read its
    // credential files or infer a subscription from an API-key environment.
    // Status output is human-readable and not stable; ambiguous text fails.
    if(/api[ -]?key/i.test(output))return {authenticated:false,code:'API_KEY_SESSION'};
    if(/not authenticated|not logged in|logged out|login required/i.test(output))
      return {authenticated:false,code:'SESSION_EXPIRED'};
    if(/authenticated|logged in|signed in/i.test(output))
      return {authenticated:true};
    return {authenticated:false,code:'SESSION_UNVERIFIED'};
  }
  try {
    const status = JSON.parse(result.stdout);
    if (status.loggedIn !== true) return { authenticated: false, code: 'SESSION_EXPIRED' };
    if (status.authMethod !== 'claude.ai') return { authenticated: false, code: 'API_KEY_SESSION' };
    return { authenticated: true };
  } catch {
    return { authenticated: false, code: 'SESSION_UNVERIFIED' };
  }
}

function runClient(argv, { cwd, interactive = false, timeout }) {
  return new Promise((resolve, reject) => {
    const detached = !interactive && process.platform !== 'win32';
    const child = spawn(bin, argv, {
      cwd, env: vendorEnvironment(), detached,
      stdio: interactive ? 'inherit' : ['ignore', 'pipe', 'pipe'],
    });
    const chunks = [];
    let bytes = 0;
    let stopped = null;
    let escalation;
    const kill = (signal) => {
      try {
        if (detached) process.kill(-child.pid, signal);
        else child.kill(signal);
      } catch { /* The client already exited. */ }
    };
    const stop = (code) => {
      if (stopped) return;
      stopped = code;
      kill('SIGTERM');
      escalation = setTimeout(() => kill('SIGKILL'), 1000);
      escalation.unref();
    };
    const onInterrupt = () => stop('CANCELLED');
    const timer = setTimeout(() => stop('CLIENT_TIMEOUT'), timeout);
    process.once('SIGINT', onInterrupt);
    process.once('SIGTERM', onInterrupt);
    if (!interactive) {
      child.stdout.on('data', (chunk) => {
        bytes += chunk.length;
        if (bytes > 2 * 1024 * 1024) stop('OUTPUT_LIMIT');
        else chunks.push(chunk);
      });
      // Raw stderr can contain account or environment details; discard it.
      child.stderr.resume();
    }
    const cleanup = () => {
      clearTimeout(timer);
      clearTimeout(escalation);
      process.off('SIGINT', onInterrupt);
      process.off('SIGTERM', onInterrupt);
    };
    child.on('error', (error) => {
      cleanup();
      reject(new NativeAuthError(error.code === 'ENOENT' ? 'CLI_MISSING' : 'CLIENT_FAILED'));
    });
    child.on('close', (code) => {
      cleanup();
      if (stopped) reject(new NativeAuthError(stopped));
      else if (code !== 0) reject(new NativeAuthError('CLIENT_FAILED'));
      else resolve(Buffer.concat(chunks).toString('utf8'));
    });
  });
}

async function runCodex() {
  approved();
  const prompt = option('--prompt');
  if (typeof prompt !== 'string' || !prompt.trim() || prompt.length > 4000 || prompt.includes('\0')) {
    fail('INVALID_PROMPT');
  }
  const status = sessionStatus();
  if (!status.authenticated) fail(status.code);
  const workspace = mkdtempSync(join(tmpdir(), 'lm-codex-native-'));
  chmodSync(workspace, 0o700);
  try {
    const output = await runClient([
      'exec', '--json', '--ephemeral', '--ignore-user-config',
      '--sandbox', 'read-only', '--skip-git-repo-check', '--cd', workspace, '--', prompt,
    ], { cwd: workspace, timeout: timeoutMs() });
    let completed = false;
    let answer = '';
    try {
      for (const line of output.split(/\r?\n/)) {
        if (!line.trim()) continue;
        const event = JSON.parse(line);
        if (event.type === 'turn.failed' || event.type === 'error') fail('TURN_FAILED');
        if (event.type === 'turn.completed') completed = true;
        if (event.type === 'item.completed' && event.item?.type === 'agent_message' &&
            typeof event.item.text === 'string') {
          answer += event.item.text.slice(0, 10000 - answer.length);
        }
      }
    } catch (error) {
      if (error instanceof NativeAuthError) throw error;
      fail('PROTOCOL_ERROR');
    }
    if (!completed || !answer.trim()) fail('INCOMPLETE_TURN');
    console.log(JSON.stringify({ ok: true, provider, action, completed: true,
      usedOfficialClient: true, response: answer }));
  } finally {
    rmSync(workspace, { force: true, recursive: true });
  }
}

try {
  if (action === 'status') {
    const status = sessionStatus();
    console.log(JSON.stringify({ ok: status.authenticated, provider, action, ...status,
      credentialBytesRead: 0, credentialsHandledBy: 'vendor CLI' }));
    if (!status.authenticated) process.exitCode = 1;
  } else if (action === 'run' && provider === 'codex') {
    await runCodex();
  } else if (action === 'run') {
    approved();
    fail('UNSUPPORTED_ACTION');
  } else {
    approved();
    await runClient(action === 'open' ? [] : provider === 'claude' ? ['auth', 'login'] : ['login'],
      { interactive: true, timeout: timeoutMs() });
    console.log(JSON.stringify({ ok: true, provider, action, usedOfficialClient: true }));
  }
} catch (error) {
  const code = error instanceof NativeAuthError ? error.code : 'CLIENT_FAILED';
  console.error(JSON.stringify({ ok: false, provider, action, code, message: messages[code] }));
  process.exitCode = 1;
}

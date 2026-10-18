import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';

const script = resolve('scripts/subscription-native-auth.mjs');
const posixOnly = process.platform === 'win32' ? 'Mock executable uses a POSIX shebang' : false;

function fixture() {
  const folder = mkdtempSync(join(tmpdir(), 'lm-native-auth-test-'));
  const bin = join(folder, 'official-client-mock');
  const logfile = join(folder, 'invocation.json');
  writeFileSync(bin, `#!/usr/bin/env node
const {writeFileSync}=require('node:fs');
const args=process.argv.slice(2), scenario=process.env.LM_TEST_SCENARIO;
writeFileSync(process.env.LM_TEST_LOG,JSON.stringify({args,pid:process.pid,
  apiKey:!!process.env.OPENAI_API_KEY,accessToken:!!process.env.CODEX_ACCESS_TOKEN,
  baseUrl:!!process.env.OPENAI_BASE_URL,org:!!process.env.OPENAI_ORG_ID,
  anthropicKey:!!process.env.ANTHROPIC_API_KEY,
  anthropicBase:!!process.env.ANTHROPIC_BASE_URL}));
if(args[0]==='login'&&args[1]==='status') {
  if(scenario==='status-hang') setInterval(()=>{},1000);
  if(scenario==='expired') {console.error('Session expired');process.exit(1)}
  console.log(scenario==='api-key'?'Logged in using API key':
    scenario==='unknown'?'Authenticated':'Logged in using ChatGPT');
  process.exit(0);
}
if(args[0]==='auth'&&args[1]==='status') {
  if(scenario==='expired') {console.log(JSON.stringify({loggedIn:false}));process.exit(0)}
  if(scenario==='api-key') {console.log(JSON.stringify({loggedIn:true,authMethod:'api-key'}));process.exit(0)}
  if(scenario==='unknown') {console.log('not-json');process.exit(0)}
  console.log(JSON.stringify({loggedIn:true,authMethod:'claude.ai',email:'secret@example.invalid'}));
  process.exit(0);
}
if(args[0]==='exec') {
  if(scenario==='hang') {setInterval(()=>{},1000);return}
  if(scenario==='large') {process.stdout.write('x'.repeat(2*1024*1024+1));setInterval(()=>{},1000);return}
  if(scenario==='malformed') {console.log('{invalid');process.exit(0)}
  if(scenario==='failed') {console.log(JSON.stringify({type:'turn.failed',error:'SECRET'}));process.exit(0)}
  if(scenario==='nonzero') {console.error('SECRET');process.exit(3)}
  console.log(JSON.stringify({type:'item.completed',item:{type:'agent_message',text:'Read-only native response'}}));
  if(scenario!=='incomplete') console.log(JSON.stringify({type:'turn.completed'}));
  process.exit(0);
}
if(scenario==='hang') {setInterval(()=>{},1000);return}
process.exit(0);
`, { mode: 0o700 });
  chmodSync(bin, 0o700);
  const env = {
    ...process.env, LITTLE_MONKEY_CODEX_BIN: bin, LITTLE_MONKEY_CLAUDE_BIN: bin,
    LM_TEST_LOG: logfile, OPENAI_API_KEY: 'SECRET', CODEX_ACCESS_TOKEN: 'SECRET',
    OPENAI_BASE_URL: 'https://secret.invalid', OPENAI_ORG_ID: 'SECRET',
    ANTHROPIC_API_KEY: 'SECRET', ANTHROPIC_BASE_URL: 'https://secret.invalid',
  };
  const invoke = (provider, args, extra = {}) => spawnSync(process.execPath,
    [script, provider, ...args], { encoding: 'utf8', timeout: 20000, env: { ...env, ...extra } });
  return { folder, bin, logfile, env, invoke, cleanup: () => rmSync(folder, { recursive: true, force: true }) };
}
function errorCode(result) {
  assert.notEqual(result.status, 0);
  assert.doesNotMatch(result.stderr, /SECRET|secret@example/);
  return JSON.parse(result.stderr || result.stdout).code;
}

test('official session status distinguishes sign-in, billing mode, expiry, and missing CLI',
  { skip: posixOnly }, () => {
    const mock = fixture();
    try {
      assert.equal(JSON.parse(mock.invoke('codex', ['status']).stdout).authenticated, true);
      assert.equal(errorCode(mock.invoke('codex', ['status'], { LM_TEST_SCENARIO: 'api-key' })), 'API_KEY_SESSION');
      assert.equal(errorCode(mock.invoke('codex', ['status'], { LM_TEST_SCENARIO: 'expired' })), 'SESSION_EXPIRED');
      assert.equal(errorCode(mock.invoke('codex', ['status'], { LM_TEST_SCENARIO: 'unknown' })), 'SESSION_UNVERIFIED');
      assert.equal(errorCode(mock.invoke('codex', ['status'], { LITTLE_MONKEY_CODEX_BIN: join(mock.folder, 'absent') })), 'CLI_MISSING');
      const claude = JSON.parse(mock.invoke('claude', ['status']).stdout);
      assert.equal(claude.authenticated, true);
      assert.doesNotMatch(JSON.stringify(claude), /secret@example/);
      assert.equal(errorCode(mock.invoke('claude', ['status'], { LM_TEST_SCENARIO: 'expired' })), 'SESSION_EXPIRED');
      assert.equal(errorCode(mock.invoke('claude', ['status'], { LM_TEST_SCENARIO: 'api-key' })), 'API_KEY_SESSION');
      assert.equal(errorCode(mock.invoke('claude', ['status'], { LM_TEST_SCENARIO: 'unknown' })), 'SESSION_UNVERIFIED');
    } finally { mock.cleanup(); }
  });

test('execution requires consent and returns only completed assistant text',
  { skip: posixOnly }, () => {
    const mock = fixture();
    try {
      assert.equal(errorCode(mock.invoke('codex', ['run', '--prompt', 'Summarize'])), 'CONSENT_REQUIRED');
      assert.equal(errorCode(mock.invoke('codex', ['run', '--prompt', '', '--consent'])), 'INVALID_PROMPT');
      const result = mock.invoke('codex', ['run', '--prompt', '--not-a-flag', '--consent']);
      assert.equal(result.status, 0, result.stderr);
      const output = JSON.parse(result.stdout);
      assert.equal(output.response, 'Read-only native response');
      assert.equal(output.completed, true);
      const recorded = JSON.parse(readFileSync(mock.logfile, 'utf8'));
      assert.deepEqual(recorded.args.slice(0, 4), ['exec', '--json', '--ephemeral', '--ignore-user-config']);
      assert.ok(recorded.args.includes('read-only'));
      assert.deepEqual(recorded.args.slice(-2), ['--', '--not-a-flag']);
      const workspace = recorded.args[recorded.args.indexOf('--cd') + 1];
      assert.ok(workspace.includes('lm-codex-native-'));
      assert.equal(existsSync(workspace), false);
      for (const key of ['apiKey', 'accessToken', 'baseUrl', 'org', 'anthropicKey', 'anthropicBase']) {
        assert.equal(recorded[key], false, key);
      }
    } finally { mock.cleanup(); }
  });

test('malformed, failed, incomplete, oversized, and nonzero protocol runs fail closed',
  { skip: posixOnly }, () => {
    const mock = fixture();
    try {
      const expected = { malformed: 'PROTOCOL_ERROR', failed: 'TURN_FAILED',
        incomplete: 'INCOMPLETE_TURN', large: 'OUTPUT_LIMIT', nonzero: 'CLIENT_FAILED' };
      for (const [scenario, code] of Object.entries(expected)) {
        const result = mock.invoke('codex', ['run', '--prompt', 'Summarize', '--consent'],
          { LM_TEST_SCENARIO: scenario });
        assert.equal(errorCode(result), code, scenario);
        const recorded = JSON.parse(readFileSync(mock.logfile, 'utf8'));
        const workspace = recorded.args[recorded.args.indexOf('--cd') + 1];
        assert.equal(existsSync(workspace), false, scenario);
      }
    } finally { mock.cleanup(); }
  });

test('timeouts stop hung execution and interactive login; unsupported delegation stays closed',
  { skip: posixOnly }, () => {
    const mock = fixture();
    try {
      assert.equal(errorCode(mock.invoke('codex', ['run', '--prompt', 'Summarize', '--consent', '--timeout-ms', '150'],
        { LM_TEST_SCENARIO: 'hang' })), 'CLIENT_TIMEOUT');
      assert.equal(errorCode(mock.invoke('claude', ['login', '--consent', '--timeout-ms', '150'],
        { LM_TEST_SCENARIO: 'hang' })), 'CLIENT_TIMEOUT');
      assert.equal(errorCode(mock.invoke('claude', ['run', '--prompt', 'Hello', '--consent'])), 'UNSUPPORTED_ACTION');
      assert.equal(errorCode(mock.invoke('claude', ['open'])), 'CONSENT_REQUIRED');
    } finally { mock.cleanup(); }
  });

test('SIGTERM cancels and reaps the native execution', { skip: posixOnly }, async () => {
  const mock = fixture();
  try {
    const child = spawn(process.execPath, [script, 'codex', 'run', '--prompt', 'Summarize',
      '--consent', '--timeout-ms', '5000'], {
      env: { ...mock.env, LM_TEST_SCENARIO: 'hang' }, stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stderr = '';
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    const deadline = Date.now() + 3000;
    while ((!existsSync(mock.logfile) ||
      JSON.parse(readFileSync(mock.logfile, 'utf8')).args[0] !== 'exec') && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    assert.ok(existsSync(mock.logfile), 'mock must start before cancellation');
    child.kill('SIGTERM');
    const code = await new Promise((resolve) => child.on('close', resolve));
    assert.equal(code, 1);
    assert.equal(JSON.parse(stderr).code, 'CANCELLED');
  } finally { mock.cleanup(); }
});

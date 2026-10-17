import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';

const script = fileURLToPath(new URL('./updater-acceptance-preflight.mjs', import.meta.url));
const signature = Buffer.from('untrusted comment: signature from test key\nRWtest\ntrusted comment: timestamp:1\nRWtest\n').toString('base64');
const entry = {
  url: 'https://github.com/AA-Box/little-monkey/releases/download/1.10.0/Little.Monkey_aarch64.app.tar.gz',
  signature,
};

function run(manifest, extra = [], installedVersion = '1.9.0') {
  const directory = mkdtempSync(join(tmpdir(), 'lm-updater-test-'));
  try {
    const path = join(directory, 'latest.json');
    writeFileSync(path, JSON.stringify(manifest));
    const result = spawnSync(process.execPath, [script,
      '--platform', 'darwin-aarch64', '--installed-version', installedVersion,
      '--manifest-file', path, ...extra], { encoding: 'utf8', timeout: 5000 });
    return { status: result.status, output: JSON.parse(result.status === 0 ? result.stdout : result.stderr) };
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
}

const manifest = (override = {}) => ({
  version: '1.10.0', platforms: { 'darwin-aarch64': entry }, ...override,
});

test('valid signed release metadata is ready for external acceptance, not installation', () => {
  const result = run(manifest());
  assert.equal(result.status, 0);
  assert.equal(result.output.status, 'READY FOR EXTERNAL ACCEPTANCE');
  assert.equal(result.output.manifestValidated, true);
  assert.equal(result.output.signatureVerified, false);
  assert.equal(result.output.installationVerified, false);
});

test('requires a newer release', () => {
  const result = run(manifest(), [], '1.10.0');
  assert.equal(result.status, 1);
  assert.equal(result.output.code, 'NO_NEWER_RELEASE');
});

test('malformed release version produces a structured failure', () => {
  const result = run(manifest({ version: ['1.10.0'] }));
  assert.equal(result.status, 1);
  assert.equal(result.output.code, 'INVALID_VERSION');
});

test('requires an updater artifact on the exact release', () => {
  const result = run(manifest({ platforms: { 'darwin-aarch64': {
    ...entry, url: 'https://github.com/AA-Box/little-monkey/releases/download/1.10.0/download.zip',
  } } }));
  assert.equal(result.status, 1);
  assert.equal(result.output.code, 'ARTIFACT_URL_INVALID');
});

test('rejects missing signature and missing platform', () => {
  const unsigned = run(manifest({ platforms: { 'darwin-aarch64': { ...entry, signature: '' } } }));
  assert.equal(unsigned.status, 1);
  assert.equal(unsigned.output.code, 'SIGNATURE_MISSING');
  const absent = run(manifest({ platforms: {} }));
  assert.equal(absent.status, 1);
  assert.equal(absent.output.code, 'PLATFORM_MISSING');
});

test('artifact filename must match the selected release', () => {
  const directory = mkdtempSync(join(tmpdir(), 'lm-updater-mismatch-'));
  try {
    const artifact = join(directory, 'other.app.tar.gz');
    writeFileSync(artifact, 'wrong file');
    const result = run(manifest(), ['--artifact', artifact]);
    assert.equal(result.status, 1);
    assert.equal(result.output.code, 'ARTIFACT_MISMATCH');
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

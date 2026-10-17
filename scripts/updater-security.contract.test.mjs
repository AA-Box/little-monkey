import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

const manifest = readFileSync(new URL('../src-tauri/Cargo.toml', import.meta.url), 'utf8');
const lock = readFileSync(new URL('../src-tauri/Cargo.lock', import.meta.url), 'utf8');
const config = JSON.parse(readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
const release = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');

test('signed updater keeps TLS but excludes the vulnerable ZIP extraction feature', () => {
  const dependency = manifest.match(/^tauri-plugin-updater\s*=\s*\{([^\n]+)\}/m)?.[1];
  assert.ok(dependency, 'updater must declare its features explicitly');
  assert.match(dependency, /default-features\s*=\s*false/);
  assert.match(dependency, /features\s*=\s*\[\s*"rustls-tls"\s*\]/);
  assert.doesNotMatch(lock, /^name = "zip"$/m, 'ZIP must be absent from the updater build graph');
});

test('release still signs raw updater artifacts for the pinned verification key', () => {
  const updater = config.plugins.updater;
  assert.ok(updater.endpoints.every((endpoint) => endpoint.startsWith('https://')));
  const key = Buffer.from(updater.pubkey, 'base64').toString('utf8');
  assert.match(key, /^untrusted comment: minisign public key:/);
  assert.match(key, /^RW[A-Za-z0-9+/]{40,}={0,2}$/m);
  assert.equal(config.bundle.createUpdaterArtifacts, true);
  assert.match(release, /TAURI_SIGNING_PRIVATE_KEY:\s*\$\{\{ secrets\.TAURI_SIGNING_PRIVATE_KEY \}\}/);
  assert.match(release, /includeUpdaterJson:\s*true/);
});

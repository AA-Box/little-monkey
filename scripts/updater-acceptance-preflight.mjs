#!/usr/bin/env node
/** Inspect a staged signed release without installing it on the operator's machine. */
import { spawnSync } from 'node:child_process';
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join } from 'node:path';

const config = JSON.parse(readFileSync(new URL('../src-tauri/tauri.conf.json', import.meta.url), 'utf8'));
const args = process.argv.slice(2);
const value = (flag) => {
  const index = args.indexOf(flag);
  return index < 0 ? null : args[index + 1] ?? null;
};
const platform = value('--platform');
const installedVersion = value('--installed-version');
const manifestFile = value('--manifest-file');
const artifact = value('--artifact');
const formats = {
  'darwin-aarch64': /\.app\.tar\.gz$/,
  'darwin-x86_64': /\.app\.tar\.gz$/,
  'windows-aarch64': /(?:-setup\.exe|\.msi)$/,
  'windows-x86_64': /(?:-setup\.exe|\.msi)$/,
  'linux-aarch64': /\.AppImage$/,
  'linux-x86_64': /\.AppImage$/,
};
class PreflightError extends Error {
  constructor(code) { super(code); this.code = code; }
}
const fail = (code) => { throw new PreflightError(code); };
const versionParts = (version) => /^\d+\.\d+\.\d+$/.test(version ?? '')
  ? version.split('.').map(Number) : null;
const greaterVersion = (next, current) => {
  const a = versionParts(next), b = versionParts(current);
  if (!a || !b) fail('INVALID_VERSION');
  for (let i = 0; i < 3; i += 1) {
    if (a[i] !== b[i]) return a[i] > b[i];
  }
  return false;
};
const decode = (encoded, label) => {
  if (typeof encoded !== 'string' || !/^[A-Za-z0-9+/]+={0,2}$/.test(encoded)) fail(label);
  const text = Buffer.from(encoded, 'base64').toString('utf8');
  if (!/^untrusted comment: [^\n]+\n[A-Za-z0-9+/]+={0,2}(?:\n|$)/.test(text)) fail(label);
  return text;
};

try {
  if (!Object.hasOwn(formats, platform) || !installedVersion) fail('USAGE');
  const source = config.plugins.updater.endpoints[0];
  const response = manifestFile ? null : await fetch(source, { signal: AbortSignal.timeout(15000) });
  if (response && !response.ok) fail('MANIFEST_UNAVAILABLE');
  const raw = manifestFile ? readFileSync(manifestFile, 'utf8') : await response.text();
  if (raw.length > 1024 * 1024) fail('MANIFEST_INVALID');
  let manifest;
  try { manifest = JSON.parse(raw); } catch { fail('MANIFEST_INVALID'); }
  if (!greaterVersion(manifest.version, installedVersion)) fail('NO_NEWER_RELEASE');
  const entry = manifest.platforms?.[platform];
  if (!entry || typeof entry.url !== 'string') fail('PLATFORM_MISSING');
  let url;
  try { url = new URL(entry.url); } catch { fail('ARTIFACT_URL_INVALID'); }
  if (url.protocol !== 'https:' || url.host !== 'github.com' || url.username ||
      url.password || url.search || url.hash ||
      !url.pathname.startsWith(`/AA-Box/little-monkey/releases/download/${manifest.version}/`) ||
      !formats[platform].test(url.pathname)) fail('ARTIFACT_URL_INVALID');
  const name = basename(url.pathname);
  const signature = decode(entry.signature, 'SIGNATURE_MISSING');
  const publicKey = decode(config.plugins.updater.pubkey, 'PUBLIC_KEY_INVALID');
  if (!/^trusted comment: /m.test(signature)) fail('SIGNATURE_MISSING');
  let signatureVerified = false;
  if (artifact) {
    if (!existsSync(artifact) || basename(artifact) !== name) fail('ARTIFACT_MISMATCH');
    const temporary = mkdtempSync(join(tmpdir(), 'lm-updater-verify-'));
    try {
      const signaturePath = join(temporary, 'artifact.minisig');
      const keyPath = join(temporary, 'updater.pub');
      writeFileSync(signaturePath, signature, { mode: 0o600 });
      writeFileSync(keyPath, publicKey, { mode: 0o600 });
      const result = spawnSync('minisign', ['-Vm', artifact, '-x', signaturePath, '-p', keyPath],
        { encoding: 'utf8', timeout: 120000, maxBuffer: 4096 });
      if (result.error?.code === 'ENOENT') fail('MINISIGN_MISSING');
      if (result.error || result.status !== 0) fail('SIGNATURE_INVALID');
      signatureVerified = true;
    } finally { rmSync(temporary, { recursive: true, force: true }); }
  }
  console.log(JSON.stringify({ status: 'READY FOR EXTERNAL ACCEPTANCE',
    manifestValidated: true, signatureVerified, installationVerified: false,
    releaseVersion: manifest.version, platform, artifactName: name,
    artifactUrl: entry.url, fixture: Boolean(manifestFile) }));
} catch (error) {
  const code = error instanceof PreflightError ? error.code : 'PREFLIGHT_FAILED';
  console.error(JSON.stringify({ status: 'FAILED', code }));
  process.exitCode = 1;
}

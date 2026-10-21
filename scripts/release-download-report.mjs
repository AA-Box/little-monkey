#!/usr/bin/env node
// Public GitHub *asset requests*, not app installs or unique users.
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';

const INSTALLER = /(?:\.dmg|\.exe|\.msi|\.AppImage|\.deb|\.rpm|\.app\.tar\.gz)$/i;
export function installerPlatform(name) {
  if (!INSTALLER.test(name)) return null;
  if (/\.(?:dmg|app\.tar\.gz)$/i.test(name)) return 'macOS';
  if (/\.(?:exe|msi)$/i.test(name)) return 'Windows';
  return 'Linux';
}

export function summarizeRelease(release) {
  if (!release || typeof release.tag_name !== 'string' || !Array.isArray(release.assets)) {
    throw new Error('Expected a GitHub release with tag_name and assets');
  }
  const platforms = { macOS: 0, Windows: 0, Linux: 0 };
  const installers = [];
  let otherAssetRequests = 0;
  for (const a of release.assets) {
    if (typeof a.name !== 'string' || !Number.isSafeInteger(a.download_count) || a.download_count < 0) {
      throw new Error('Invalid asset count or name');
    }
    const platform = installerPlatform(a.name);
    if (platform) {
      platforms[platform] += a.download_count;
      installers.push({ name: a.name, requests: a.download_count, platform });
    } else {
      otherAssetRequests += a.download_count;
    }
  }
  return { tag: release.tag_name, publishedAt: release.published_at ?? null, platforms, installers, otherAssetRequests, installerRequests: Object.values(platforms).reduce((a, b) => a + b, 0) };
}

async function main() {
  const response = await fetch('https://api.github.com/repos/AA-Box/little-monkey/releases/latest', {
    headers: { Accept: 'application/vnd.github+json', 'User-Agent': 'little-monkey-public-release-download-report' },
  });
  if (!response.ok) throw new Error(`GitHub API HTTP ${response.status}: ${response.statusText}`);
  const summary = summarizeRelease(await response.json());
  console.log(`GitHub release: ${summary.tag} (published ${summary.publishedAt ?? 'unknown'})`);
  console.log('Public download requests, not unique downloads, users, or successful installs:');
  for (const [platform, count] of Object.entries(summary.platforms)) console.log(`  ${platform}: ${count}`);
  console.log(`  Total installer asset requests: ${summary.installerRequests}`);
  console.log(`  Other asset requests (e.g. signatures, manifests, extensions): ${summary.otherAssetRequests}`);
  console.log('Installer assets:');
  for (const a of summary.installers) console.log(`  ${a.requests}\t${a.name}`);
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main().catch(err => { console.error(err.message); process.exitCode = 1; });
}

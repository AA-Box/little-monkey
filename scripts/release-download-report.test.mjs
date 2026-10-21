import test from 'node:test';
import assert from 'node:assert/strict';
import { installerPlatform, summarizeRelease } from './release-download-report.mjs';

test('only installer formats are counted as installer asset requests', () => {
  assert.equal(installerPlatform('Little.Monkey_x64.dmg'), 'macOS');
  assert.equal(installerPlatform('Little.Monkey_arm64-setup.exe'), 'Windows');
  assert.equal(installerPlatform('Little.Monkey_amd64.AppImage'), 'Linux');
  assert.equal(installerPlatform('Little.Monkey_1.0_x64.deb'), 'Linux');
  assert.equal(installerPlatform('Little.Monkey_aarch64.app.tar.gz'), 'macOS');
  for (const name of ['latest.json', 'SBOM.cyclonedx.json', 'setup.exe.sig', 'AppImage.asc', 'little-monkey.vsix', 'little-monkey-jetbrains.zip']) {
    assert.equal(installerPlatform(name), null, name);
  }
});

test('shows public requests, not an invented unique user count', () => {
  assert.deepEqual(summarizeRelease({
    tag_name: 'v1', published_at: '2026-01-01T00:00:00Z',
    assets: [
      { name: 'mac.dmg', download_count: 3 },
      { name: 'win.exe', download_count: 2 },
      { name: 'win.exe.sig', download_count: 4 },
      { name: 'linux.deb', download_count: 5 },
    ],
  }), {
    tag: 'v1', publishedAt: '2026-01-01T00:00:00Z',
    platforms: { macOS: 3, Windows: 2, Linux: 5 },
    installerRequests: 10,
    otherAssetRequests: 4,
    installers: [
      { name: 'mac.dmg', requests: 3, platform: 'macOS' },
      { name: 'win.exe', requests: 2, platform: 'Windows' },
      { name: 'linux.deb', requests: 5, platform: 'Linux' },
    ],
  });
});

test('rejects malformed API responses rather than guessing', () => {
  assert.throws(() => summarizeRelease({ tag_name: 'v2', assets: [{ name: 'app.dmg', download_count: -1 }] }), /Invalid/);
  assert.throws(() => summarizeRelease({ tag_name: 'v2' }), /Expected/);
});

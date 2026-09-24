import assert from "node:assert/strict";
import test from "node:test";

import {
  patchManagedRuntimeManifest,
  pinRelationFromHistory,
  publishedReleaseCandidates,
  releaseAssetsFor,
  releaseAssetsForWithRetry,
  selectNewestPublishedRelease,
} from "./update-managed-sd-runtime.mjs";

const sha899 = "28b454bda1af8ed0262c517a689f8330a814cfb7";
const sha900 = "c92d73c408515c94beef32161bb5960764fde7a0";

function release(tag_name, target_commitish, published_at, assets = []) {
  return {
    tag_name,
    target_commitish,
    published_at,
    draft: false,
    prerelease: false,
    html_url: `https://github.com/leejet/stable-diffusion.cpp/releases/tag/${tag_name}`,
    assets,
  };
}

test("release selection follows default-branch ancestry, not publication order", () => {
  const releases = [
    // 899 was published later, which is exactly the upstream ordering trap
    // that made GitHub's latest-release label misleading.
    release("master-899-28b454b", sha899, "2026-09-22T21:06:10Z"),
    release("master-900-c92d73c", sha900, "2026-09-22T20:52:10Z"),
  ];
  const candidates = publishedReleaseCandidates(releases);
  const selected = selectNewestPublishedRelease(candidates, [sha900, sha899]);
  assert.equal(selected.release.tag_name, "master-900-c92d73c");
  assert.equal(selected.commit, sha900);
});

test("pin relation is derived locally from newest-first upstream history", () => {
  const older = "74988b290e40155fe2313914e44b979b750e958b";
  const newerUnpublished = "f".repeat(40);
  const history = [newerUnpublished, sha900, sha899, older];
  assert.equal(pinRelationFromHistory(older, sha900, history), "ahead");
  assert.equal(pinRelationFromHistory(sha900, sha900, history), "identical");
  assert.equal(pinRelationFromHistory(newerUnpublished, sha900, history), "behind");
  assert.throws(
    () => pinRelationFromHistory("e".repeat(40), sha900, history),
    /Current managed SD commit was not found/,
  );
});

test("drafts and prereleases can never become the managed runtime", () => {
  const stable = release("master-900-c92d73c", sha900, "2026-09-22T20:52:10Z");
  const draft = { ...release("master-901-deadbee", "deadbeef".padEnd(40, "0"), "2026-09-23T00:00:00Z"), draft: true };
  const prerelease = { ...release("master-902-feedbee", "feedbeef".padEnd(40, "0"), "2026-09-23T01:00:00Z"), prerelease: true };
  const candidates = publishedReleaseCandidates([draft, prerelease, stable]);
  assert.deepEqual(candidates.map((entry) => entry.release.tag_name), [stable.tag_name]);
});

test("required release assets must carry GitHub SHA-256 digests", () => {
  const candidate = {
    commit: sha900,
    tagShortSha: "c92d73c",
    release: release("master-900-c92d73c", sha900, "2026-09-22T20:52:10Z", [
      {
        name: "sd-master-c92d73c-bin-Darwin-macOS-26.6.2-arm64.zip",
        digest: `sha256:${"a".repeat(64)}`,
      },
      {
        name: "sd-master-c92d73c-bin-win-vulkan-x64.zip",
        digest: `sha256:${"b".repeat(64)}`,
      },
    ]),
  };
  assert.deepEqual(releaseAssetsFor(candidate), {
    macArchive: "sd-master-c92d73c-bin-Darwin-macOS-26.6.2-arm64.zip",
    macSha256: "a".repeat(64),
    windowsArchive: "sd-master-c92d73c-bin-win-vulkan-x64.zip",
    windowsSha256: "b".repeat(64),
  });
});

test("release assets are retried while upstream is still publishing them", async () => {
  const mac = {
    name: "sd-master-c92d73c-bin-Darwin-macOS-26.6.2-arm64.zip",
    digest: `sha256:${"a".repeat(64)}`,
  };
  const windows = {
    name: "sd-master-c92d73c-bin-win-vulkan-x64.zip",
    digest: `sha256:${"b".repeat(64)}`,
  };
  const candidate = {
    commit: sha900,
    tagShortSha: "c92d73c",
    release: release("master-900-c92d73c", sha900, "2026-09-22T20:52:10Z", [mac]),
  };
  let refreshes = 0;
  let sleeps = 0;

  const assets = await releaseAssetsForWithRetry(
    candidate,
    async () => {
      refreshes += 1;
      return release("master-900-c92d73c", sha900, "2026-09-22T20:52:10Z", [mac, windows]);
    },
    {
      attempts: 2,
      delayMs: 0,
      sleepFn: async () => {
        sleeps += 1;
      },
    },
  );

  assert.equal(refreshes, 1);
  assert.equal(sleeps, 1);
  assert.equal(assets.windowsArchive, windows.name);
  assert.equal(assets.windowsSha256, "b".repeat(64));
});

test("manifest patch changes only the pin and the two upstream archive records", () => {
  const oldCommit = "74988b290e40155fe2313914e44b979b750e958b";
  const input = `export const MANAGED_SD_VERSION = "master-890-74988b2";\nexport const MANAGED_SD_SOURCE_COMMIT =\n  "${oldCommit}";\nexport const MANAGED_SD_ASSETS = Object.freeze({\n  "aarch64-apple-darwin": {\n    archive: "sd-master-74988b2-bin-Darwin-macOS-26.6.2-arm64.zip",\n    sha256: "${"1".repeat(64)}",\n  },\n  "x86_64-pc-windows-msvc": {\n    archive: "sd-master-74988b2-bin-win-vulkan-x64.zip",\n    sha256: "${"2".repeat(64)}",\n  },\n});\n`;
  const output = patchManagedRuntimeManifest(input, {
    currentVersion: "master-890-74988b2",
    currentCommit: oldCommit,
    version: "master-900-c92d73c",
    commit: sha900,
    macArchive: "sd-master-c92d73c-bin-Darwin-macOS-26.6.2-arm64.zip",
    macSha256: "a".repeat(64),
    windowsArchive: "sd-master-c92d73c-bin-win-vulkan-x64.zip",
    windowsSha256: "b".repeat(64),
  });
  assert.match(output, /MANAGED_SD_VERSION = "master-900-c92d73c"/);
  assert.match(output, new RegExp(sha900));
  assert.match(output, /sd-master-c92d73c-bin-Darwin-macOS-26\.6\.2-arm64\.zip/);
  assert.match(output, /sd-master-c92d73c-bin-win-vulkan-x64\.zip/);
  assert.doesNotMatch(output, /master-890-74988b2/);
});

#!/usr/bin/env node

import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

const UPSTREAM_REPOSITORY = "leejet/stable-diffusion.cpp";
const RELEASE_TAG = /^master-(\d+)-([0-9a-f]{7,40})$/i;
const MAX_RELEASE_PAGES = 5;
const MAX_COMMIT_PAGES = 10;
const PER_PAGE = 100;
const RELEASE_ASSET_RETRY_ATTEMPTS = 13;
const RELEASE_ASSET_RETRY_DELAY_MS = 5_000;

const FILES = Object.freeze({
  manifest: "scripts/lib/managedRuntimeManifest.mjs",
  manifestTest: "scripts/lib/managedRuntimeManifest.test.mjs",
  managedRuntimeRust: "src-tauri/src/managed_runtime.rs",
  buildRust: "src-tauri/build.rs",
});

function githubHeaders() {
  const headers = {
    Accept: "application/vnd.github+json",
    "User-Agent": "little-monkey-sd-runtime-updater",
    "X-GitHub-Api-Version": "2022-11-28",
  };
  if (process.env.GITHUB_TOKEN) {
    headers.Authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  }
  return headers;
}

async function githubJson(path) {
  const response = await fetch(`https://api.github.com${path}`, {
    headers: githubHeaders(),
  });
  if (!response.ok) {
    const text = await response.text();
    throw new Error(`GitHub API ${response.status} for ${path}: ${text.slice(0, 500)}`);
  }
  return response.json();
}

async function paged(path, maxPages) {
  const entries = [];
  for (let page = 1; page <= maxPages; page += 1) {
    const separator = path.includes("?") ? "&" : "?";
    const batch = await githubJson(`${path}${separator}per_page=${PER_PAGE}&page=${page}`);
    if (!Array.isArray(batch)) throw new Error(`Expected an array from ${path}`);
    entries.push(...batch);
    if (batch.length < PER_PAGE) break;
  }
  return entries;
}

export function publishedReleaseCandidates(releases) {
  return releases
    .filter((release) => !release.draft && !release.prerelease)
    .map((release) => {
      const match = RELEASE_TAG.exec(release.tag_name ?? "");
      if (!match) return null;
      return {
        release,
        build: Number(match[1]),
        tagShortSha: match[2].toLowerCase(),
        commit:
          typeof release.target_commitish === "string" &&
          /^[0-9a-f]{40}$/i.test(release.target_commitish)
            ? release.target_commitish.toLowerCase()
            : null,
      };
    })
    .filter(Boolean);
}

export function selectNewestPublishedRelease(candidates, defaultBranchHistory) {
  const order = new Map(defaultBranchHistory.map((sha, index) => [sha.toLowerCase(), index]));
  const onBranch = candidates
    .filter((candidate) => candidate.commit && order.has(candidate.commit))
    .map((candidate) => ({ ...candidate, historyIndex: order.get(candidate.commit) }))
    .sort((a, b) => a.historyIndex - b.historyIndex);
  if (onBranch.length === 0) {
    throw new Error("No published stable-diffusion.cpp release was found in the fetched default-branch history");
  }
  return onBranch[0];
}

export function pinRelationFromHistory(currentCommit, latestCommit, defaultBranchHistory) {
  const current = currentCommit.toLowerCase();
  const latest = latestCommit.toLowerCase();
  if (current === latest) return "identical";

  const currentIndex = defaultBranchHistory.findIndex((sha) => sha.toLowerCase() === current);
  const latestIndex = defaultBranchHistory.findIndex((sha) => sha.toLowerCase() === latest);
  if (latestIndex === -1) {
    throw new Error("Newest published release commit is missing from fetched default-branch history");
  }
  if (currentIndex === -1) {
    throw new Error(
      `Current managed SD commit was not found within ${MAX_COMMIT_PAGES * PER_PAGE} commits of upstream default branch`,
    );
  }
  // History is newest-first. These names intentionally mirror GitHub's compare
  // status used by the previous implementation: "ahead" means latest is ahead
  // of the current pin and therefore should replace it; "behind" means the
  // current pin is newer than the newest published release and must be kept.
  return currentIndex > latestIndex ? "ahead" : "behind";
}

function parseSha256Digest(asset) {
  const digest = asset?.digest;
  if (typeof digest !== "string" || !/^sha256:[0-9a-f]{64}$/i.test(digest)) {
    throw new Error(`Upstream asset ${asset?.name ?? "<missing>"} has no trustworthy SHA-256 digest`);
  }
  return digest.slice("sha256:".length).toLowerCase();
}

export function releaseAssetsFor(candidate) {
  const short = candidate.commit.slice(0, 7);
  if (!candidate.tagShortSha.startsWith(short) && !short.startsWith(candidate.tagShortSha)) {
    throw new Error(
      `Release tag ${candidate.release.tag_name} does not identify target commit ${candidate.commit}`,
    );
  }
  const assets = candidate.release.assets ?? [];
  const mac = assets.find((asset) =>
    new RegExp(`^sd-master-${short}-bin-Darwin-macOS-[^-]+-arm64\\.zip$`).test(asset.name),
  );
  const windows = assets.find(
    (asset) => asset.name === `sd-master-${short}-bin-win-vulkan-x64.zip`,
  );
  if (!mac || !windows) {
    const error = new Error(
      `Release ${candidate.release.tag_name} is missing Little Monkey's required macOS ARM64 or Windows x64 Vulkan archive`,
    );
    error.code = "INCOMPLETE_RELEASE_ASSETS";
    throw error;
  }
  return {
    macArchive: mac.name,
    macSha256: parseSha256Digest(mac),
    windowsArchive: windows.name,
    windowsSha256: parseSha256Digest(windows),
  };
}

export async function releaseAssetsForWithRetry(
  candidate,
  refreshRelease,
  {
    attempts = RELEASE_ASSET_RETRY_ATTEMPTS,
    delayMs = RELEASE_ASSET_RETRY_DELAY_MS,
    sleepFn = (ms) => new Promise((resolveSleep) => setTimeout(resolveSleep, ms)),
  } = {},
) {
  let current = candidate;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      return releaseAssetsFor(current);
    } catch (error) {
      if (error?.code !== "INCOMPLETE_RELEASE_ASSETS" || attempt === attempts) {
        throw error;
      }
    }

    await sleepFn(delayMs);
    current = {
      ...current,
      release: await refreshRelease(current.release),
    };
  }

  throw new Error("Unreachable release asset retry state");
}

function replaceLiteral(text, oldValue, newValue, label, minimum = 1) {
  const occurrences = text.split(oldValue).length - 1;
  if (occurrences < minimum) {
    throw new Error(`Could not find ${label} (${oldValue}) in the expected file`);
  }
  return text.split(oldValue).join(newValue);
}

function replaceTargetArchive(text, target, archive, sha256) {
  const escaped = target.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const pattern = new RegExp(
    `("${escaped}"\\s*:\\s*\\{\\s*\\n\\s*archive:\\s*)"[^"]+"(,\\s*\\n\\s*sha256:\\s*)"[0-9a-f]{64}"`,
  );
  if (!pattern.test(text)) {
    throw new Error(`Could not locate archive metadata for ${target}`);
  }
  return text.replace(pattern, `$1"${archive}"$2"${sha256}"`);
}

export function patchManagedRuntimeManifest(
  text,
  { currentVersion, currentCommit, version, commit, macArchive, macSha256, windowsArchive, windowsSha256 },
) {
  let next = replaceLiteral(text, `MANAGED_SD_VERSION = "${currentVersion}"`, `MANAGED_SD_VERSION = "${version}"`, "managed SD version");
  next = replaceLiteral(next, currentCommit, commit, "managed SD source commit");
  next = replaceTargetArchive(next, "aarch64-apple-darwin", macArchive, macSha256);
  next = replaceTargetArchive(next, "x86_64-pc-windows-msvc", windowsArchive, windowsSha256);
  return next;
}

function readCurrentPin(manifestText) {
  const version = /MANAGED_SD_VERSION\s*=\s*"([^"]+)"/.exec(manifestText)?.[1];
  const commit = /MANAGED_SD_SOURCE_COMMIT\s*=\s*\n?\s*"([0-9a-f]{40})"/.exec(manifestText)?.[1];
  if (!version || !commit) throw new Error("Could not read the current managed SD pin");
  return { version, commit: commit.toLowerCase() };
}

async function resolveCandidateCommits(candidates) {
  for (const candidate of candidates) {
    if (candidate.commit) continue;
    const commit = await githubJson(
      `/repos/${UPSTREAM_REPOSITORY}/commits/${encodeURIComponent(candidate.release.tag_name)}`,
    );
    if (!/^[0-9a-f]{40}$/i.test(commit.sha ?? "")) {
      throw new Error(`Release ${candidate.release.tag_name} resolved to an invalid commit SHA`);
    }
    candidate.commit = commit.sha.toLowerCase();
  }
}

async function discoverLatestRelease() {
  const [repository, releases] = await Promise.all([
    githubJson(`/repos/${UPSTREAM_REPOSITORY}`),
    paged(`/repos/${UPSTREAM_REPOSITORY}/releases`, MAX_RELEASE_PAGES),
  ]);
  const candidates = publishedReleaseCandidates(releases);
  await resolveCandidateCommits(candidates);

  const history = [];
  let selected = null;
  for (let page = 1; page <= MAX_COMMIT_PAGES; page += 1) {
    const commits = await githubJson(
      `/repos/${UPSTREAM_REPOSITORY}/commits?sha=${encodeURIComponent(repository.default_branch)}&per_page=${PER_PAGE}&page=${page}`,
    );
    if (!Array.isArray(commits) || commits.length === 0) break;
    history.push(...commits.map((entry) => entry.sha.toLowerCase()));

    if (!selected) {
      const visible = candidates.filter((candidate) => history.includes(candidate.commit));
      if (visible.length > 0) selected = selectNewestPublishedRelease(visible, history);
    }
    if (commits.length < PER_PAGE) break;
  }
  if (!selected) {
    throw new Error(
      `No published release was found within ${MAX_COMMIT_PAGES * PER_PAGE} commits of ${repository.default_branch}`,
    );
  }
  return { selected, history };
}

function applyPin(root, update) {
  const manifestPath = resolve(root, FILES.manifest);
  const manifestText = readFileSync(manifestPath, "utf8");
  writeFileSync(manifestPath, patchManagedRuntimeManifest(manifestText, update));

  for (const relative of [FILES.manifestTest, FILES.managedRuntimeRust]) {
    const path = resolve(root, relative);
    let text = readFileSync(path, "utf8");
    text = replaceLiteral(text, update.currentVersion, update.version, `${relative} SD version`);
    text = replaceLiteral(
      text,
      update.currentCommit,
      update.commit,
      `${relative} SD commit`,
      relative === FILES.manifestTest ? 1 : 0,
    );
    writeFileSync(path, text);
  }

  const buildPath = resolve(root, FILES.buildRust);
  const buildText = readFileSync(buildPath, "utf8");
  writeFileSync(
    buildPath,
    replaceLiteral(
      buildText,
      `sd-${update.currentVersion}`,
      `sd-${update.version}`,
      `${FILES.buildRust} staged SD directory`,
    ),
  );
}

async function main() {
  // The updater deliberately has no arbitrary filesystem-path arguments. It
  // only rewrites Little Monkey pin files beneath its current working directory
  // and emits its machine-readable result to stdout. Workflows that need an
  // isolated fixture change cwd before invoking it and redirect stdout to a
  // runner-owned path. This keeps untrusted CLI/environment data out of every
  // filesystem write sink.
  const root = process.cwd();
  const apply = process.argv.includes("--apply");
  const manifestText = readFileSync(resolve(root, FILES.manifest), "utf8");
  const current = readCurrentPin(manifestText);
  const { selected: latest, history } = await discoverLatestRelease();
  const relation = pinRelationFromHistory(current.commit, latest.commit, history);

  if (relation === "behind") {
    const result = {
      changed: false,
      reason: "current-pin-ahead-of-latest-published-release",
      currentVersion: current.version,
      currentCommit: current.commit,
      version: latest.release.tag_name,
      commit: latest.commit,
    };
    console.log(JSON.stringify(result));
    return;
  }

  const assets = await releaseAssetsForWithRetry(
    latest,
    () =>
      githubJson(
        `/repos/${UPSTREAM_REPOSITORY}/releases/tags/${encodeURIComponent(latest.release.tag_name)}`,
      ),
  );
  const changed = relation === "ahead";
  const result = {
    changed,
    reason: changed ? "newer-published-release" : "already-current",
    currentVersion: current.version,
    currentCommit: current.commit,
    version: latest.release.tag_name,
    commit: latest.commit,
    releaseUrl: latest.release.html_url,
    ...assets,
  };
  if (apply && changed) applyPin(root, { ...result });
  console.log(JSON.stringify(result));
}

const invoked = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (invoked) {
  main().catch((error) => {
    console.error(error.stack ?? String(error));
    process.exitCode = 1;
  });
}

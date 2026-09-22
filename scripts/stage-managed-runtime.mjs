#!/usr/bin/env node
// Stages one pinned managed runtime for the current release target. Official
// archives are downloaded and SHA-256 verified. A runtime target may instead
// opt into a pinned source build when upstream publishes no binary for that
// architecture; the exact Git commit is checked before CMake is allowed to run.
// The resulting self-contained tree is bundled as a Tauri resource.
//
// Usage: node scripts/stage-managed-runtime.mjs [runtime-id]
//   llama (default) — llama.cpp `llama-server`
//   llama-tts       — llama.cpp `llama-tts`
//   sd              — stable-diffusion.cpp `sd-server`
// The target triple comes from MANAGED_RUNTIME_TARGET, CLI_SIDECAR_TARGET, or
// the host. Every configured managed runtime target must resolve to either a
// verified archive or a pinned source build.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, extname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { hostTriple } from "./lib/cliSidecarPlaceholder.mjs";
import { managedRuntimeArchiveExtractor } from "./lib/managedRuntimeArchive.mjs";
import {
  managedRuntime,
  managedRuntimeProvenance,
  managedRuntimeSourceCmakeArgs,
  serverFileName,
  stagedRuntimeDirectory,
} from "./lib/managedRuntimeManifest.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const runtime = managedRuntime(process.argv[2] ?? "llama");
const target =
  process.env.MANAGED_RUNTIME_TARGET ||
  process.env.CLI_SIDECAR_TARGET ||
  hostTriple();
const asset = runtime.assets[target];
if (!asset) {
  throw new Error(
    `No managed ${runtime.manifestRuntime} runtime is pinned for target ${target}. ` +
      `Supported targets: ${Object.keys(runtime.assets).join(", ")}`,
  );
}

const provenance = managedRuntimeProvenance(asset);
const serverName = serverFileName(runtime, target);
const stageRoot = join(
  repoRoot,
  "src-tauri",
  "resources",
  "managed-runtime",
  stagedRuntimeDirectory(runtime),
);
const stagedBinary = join(stageRoot, serverName);
const stagedManifest = join(stageRoot, "runtime-manifest.json");

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function walkFiles(directory) {
  const candidates = [];
  const walk = (current) => {
    for (const name of readdirSync(current)) {
      const path = join(current, name);
      const stat = lstatSync(path);
      if (stat.isDirectory()) walk(path);
      else candidates.push(path);
    }
  };
  walk(directory);
  return candidates;
}

function manifestProvenanceMatches(manifest) {
  if (provenance.archiveSha256) {
    return (
      manifest.archiveSha256 === provenance.archiveSha256 &&
      manifest.sourceCommit == null
    );
  }
  return (
    manifest.sourceCommit === provenance.sourceCommit &&
    manifest.archiveSha256 == null
  );
}

function cachedStageIsCurrent() {
  if (!existsSync(stagedBinary) || !existsSync(stagedManifest)) return false;
  try {
    const manifest = JSON.parse(readFileSync(stagedManifest, "utf8"));
    if (
      manifest.schemaVersion !== 1 ||
      manifest.runtime !== runtime.manifestRuntime ||
      manifest.version !== runtime.version ||
      manifest.target !== target ||
      manifest.sourceUrl !== asset.url ||
      !manifestProvenanceMatches(manifest)
    ) {
      return false;
    }
    return manifest.files.every(
      (file) =>
        typeof file.name === "string" &&
        typeof file.sha256 === "string" &&
        !file.name.includes("/") &&
        !file.name.includes("\\") &&
        existsSync(join(stageRoot, file.name)) &&
        sha256File(join(stageRoot, file.name)) === file.sha256,
    );
  } catch {
    return false;
  }
}

if (cachedStageIsCurrent()) {
  console.log(
    `[stage-managed-runtime] ${runtime.id} ${runtime.version} already staged for ${target}`,
  );
  process.exit(0);
}

const workRoot = mkdtempSync(
  join(tmpdir(), `little-monkey-${runtime.id}-runtime-`),
);
const extractRoot = join(workRoot, "extract");
const publishRoot = join(workRoot, "publish");
mkdirSync(extractRoot);
mkdirSync(publishRoot);

function copyExecutable(source, destination) {
  copyFileSync(source, destination);
  if (!target.includes("windows")) chmodSync(destination, 0o755);
}

async function stageArchiveAsset() {
  const archivePath = join(workRoot, basename(asset.archive));
  console.log(`[stage-managed-runtime] downloading ${asset.url}`);
  const response = await fetch(asset.url, { redirect: "follow" });
  if (!response.ok || !response.body) {
    throw new Error(
      `Runtime download failed (${response.status} ${response.statusText})`,
    );
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  const actualArchiveSha = createHash("sha256").update(bytes).digest("hex");
  if (actualArchiveSha !== asset.sha256) {
    throw new Error(
      `Runtime archive checksum mismatch: expected ${asset.sha256}, got ${actualArchiveSha}`,
    );
  }
  writeFileSync(archivePath, bytes);

  // Windows and macOS ship bsdtar, which reads zip archives too. Linux uses
  // unzip for zip assets because GNU tar does not accept them.
  const [extractCommand, extractArgs] = managedRuntimeArchiveExtractor(
    archivePath,
    extractRoot,
  );
  execFileSync(extractCommand, extractArgs, { stdio: "inherit" });

  const candidates = walkFiles(extractRoot);
  const server = candidates.find((path) => basename(path) === serverName);
  if (!server) {
    throw new Error(`Verified archive did not contain ${serverName}`);
  }
  const serverDirectory = dirname(server);
  const extraNames = (runtime.extraBinaries ?? []).map((name) =>
    target.includes("windows") ? `${name}.exe` : name,
  );
  for (const name of extraNames) {
    if (!candidates.some((path) => basename(path) === name)) {
      throw new Error(`Verified archive did not contain ${name}`);
    }
  }
  const executableNames = new Set([serverName, ...extraNames]);

  const shouldStage = (path) => {
    if (dirname(path) !== serverDirectory) return false;
    const name = basename(path);
    if (executableNames.has(name) || name === "LICENSE") return true;
    if (extname(name).toLowerCase() === ".txt") return true;
    if (target.includes("windows")) return extname(name).toLowerCase() === ".dll";
    if (target.includes("apple")) return name.endsWith(".dylib");
    return name.includes(".so");
  };

  const selected = candidates.filter(shouldStage);
  for (const name of executableNames) {
    if (!selected.some((path) => basename(path) === name)) {
      throw new Error(`Runtime staging lost ${name}`);
    }
  }
  for (const source of selected) {
    const destination = join(publishRoot, basename(source));
    copyFileSync(source, destination);
    if (!target.includes("windows") && executableNames.has(basename(source))) {
      chmodSync(destination, 0o755);
    }
  }
}

function stageSourceAsset() {
  if (runtime.id !== "sd") {
    throw new Error(
      `Pinned source builds are not implemented for managed runtime ${runtime.id}`,
    );
  }
  const sourceRoot = join(workRoot, "source");
  const buildRoot = join(workRoot, "build");
  const sourceRepo = "https://github.com/leejet/stable-diffusion.cpp.git";

  console.log(
    `[stage-managed-runtime] building stable-diffusion.cpp ${asset.sourceCommit} for ${target}`,
  );
  execFileSync("git", ["init", sourceRoot], { stdio: "inherit" });
  execFileSync("git", ["-C", sourceRoot, "remote", "add", "origin", sourceRepo], {
    stdio: "inherit",
  });
  execFileSync(
    "git",
    ["-C", sourceRoot, "fetch", "--depth", "1", "origin", asset.sourceCommit],
    { stdio: "inherit" },
  );
  execFileSync("git", ["-C", sourceRoot, "checkout", "--detach", "FETCH_HEAD"], {
    stdio: "inherit",
  });
  const checkedOut = execFileSync("git", ["-C", sourceRoot, "rev-parse", "HEAD"], {
    encoding: "utf8",
  }).trim();
  if (checkedOut !== asset.sourceCommit) {
    throw new Error(
      `stable-diffusion.cpp source checkout mismatch: expected ${asset.sourceCommit}, got ${checkedOut}`,
    );
  }
  execFileSync(
    "git",
    ["-C", sourceRoot, "submodule", "update", "--init", "--recursive", "--depth", "1"],
    { stdio: "inherit" },
  );

  execFileSync(
    "cmake",
    [
      "-S",
      sourceRoot,
      "-B",
      buildRoot,
      ...managedRuntimeSourceCmakeArgs(asset),
    ],
    { stdio: "inherit" },
  );
  execFileSync(
    "cmake",
    [
      "--build",
      buildRoot,
      "--config",
      "Release",
      "--target",
      "sd-server",
      "--parallel",
      process.env.CMAKE_BUILD_PARALLEL_LEVEL || "2",
    ],
    { stdio: "inherit" },
  );

  const serverCandidates = walkFiles(buildRoot).filter(
    (path) => basename(path) === serverName,
  );
  if (serverCandidates.length !== 1) {
    throw new Error(
      `Pinned source build produced ${serverCandidates.length} ${serverName} binaries; expected exactly one`,
    );
  }
  copyExecutable(serverCandidates[0], join(publishRoot, serverName));

  // Keep the upstream license beside the source-built binary just as the
  // official release archives do for their notices.
  const license = join(sourceRoot, "LICENSE");
  if (existsSync(license)) copyFileSync(license, join(publishRoot, "LICENSE"));
}

try {
  if (asset.archive) await stageArchiveAsset();
  else stageSourceAsset();

  const executableNames = new Set([
    serverName,
    ...(runtime.extraBinaries ?? []).map((name) =>
      target.includes("windows") ? `${name}.exe` : name,
    ),
  ]);
  const files = readdirSync(publishRoot)
    .filter((name) => statSync(join(publishRoot, name)).isFile())
    .sort()
    .map((name) => ({
      name,
      sha256: sha256File(join(publishRoot, name)),
      sizeBytes: statSync(join(publishRoot, name)).size,
      executable: executableNames.has(name),
    }));

  const manifest = {
    schemaVersion: 1,
    runtime: runtime.manifestRuntime,
    version: runtime.version,
    target,
    sourceUrl: asset.url,
    ...(provenance.archiveSha256
      ? { archiveSha256: provenance.archiveSha256 }
      : { sourceCommit: provenance.sourceCommit }),
    files,
  };
  writeFileSync(
    join(publishRoot, "runtime-manifest.json"),
    `${JSON.stringify(manifest, null, 2)}\n`,
  );

  rmSync(stageRoot, { recursive: true, force: true });
  mkdirSync(dirname(stageRoot), { recursive: true });
  cpSync(publishRoot, stageRoot, { recursive: true });
  console.log(
    `[stage-managed-runtime] staged ${files.length} ${runtime.id} files for ${target} at ${stageRoot}`,
  );
} finally {
  rmSync(workRoot, { recursive: true, force: true });
}

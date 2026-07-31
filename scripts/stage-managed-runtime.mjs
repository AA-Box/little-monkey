#!/usr/bin/env node
// Downloads one pinned official llama.cpp archive for the current release
// target, verifies its SHA-256, extracts only llama-server + its adjacent
// runtime libraries/license, and stages that self-contained tree as a Tauri
// resource. The resulting runtime is owned by Little Monkey; end users do not
// need Ollama or a system llama.cpp installation.

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
import {
  MANAGED_LLAMA_ASSETS,
  MANAGED_LLAMA_VERSION,
} from "./lib/managedRuntimeManifest.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const target =
  process.env.MANAGED_RUNTIME_TARGET ||
  process.env.CLI_SIDECAR_TARGET ||
  hostTriple();
const asset = MANAGED_LLAMA_ASSETS[target];
if (!asset) {
  throw new Error(
    `No managed llama.cpp runtime is pinned for target ${target}. ` +
      `Supported targets: ${Object.keys(MANAGED_LLAMA_ASSETS).join(", ")}`,
  );
}

const stageRoot = join(
  repoRoot,
  "src-tauri",
  "resources",
  "managed-runtime",
  `llama-${MANAGED_LLAMA_VERSION}`,
);
const stagedBinary = join(
  stageRoot,
  target.includes("windows") ? "llama-server.exe" : "llama-server",
);
const stagedManifest = join(stageRoot, "runtime-manifest.json");

function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

function cachedStageIsCurrent() {
  if (!existsSync(stagedBinary) || !existsSync(stagedManifest)) return false;
  try {
    const manifest = JSON.parse(readFileSync(stagedManifest, "utf8"));
    if (
      manifest.schemaVersion !== 1 ||
      manifest.version !== MANAGED_LLAMA_VERSION ||
      manifest.target !== target ||
      manifest.archiveSha256 !== asset.sha256
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
    `[stage-managed-runtime] ${MANAGED_LLAMA_VERSION} already staged for ${target}`,
  );
  process.exit(0);
}

const workRoot = mkdtempSync(join(tmpdir(), "little-monkey-llama-runtime-"));
const archivePath = join(workRoot, basename(asset.archive));
const extractRoot = join(workRoot, "extract");
const publishRoot = join(workRoot, "publish");
mkdirSync(extractRoot);
mkdirSync(publishRoot);

try {
  console.log(`[stage-managed-runtime] downloading ${asset.url}`);
  const response = await fetch(asset.url, { redirect: "follow" });
  if (!response.ok || !response.body) {
    throw new Error(
      `Runtime download failed (${response.status} ${response.statusText})`,
    );
  }
  const bytes = Buffer.from(await response.arrayBuffer());
  // Verify the pinned digest in memory, before the download reaches the
  // filesystem, so an archive that fails its checksum is never written at all.
  const actualArchiveSha = createHash("sha256").update(bytes).digest("hex");
  if (actualArchiveSha !== asset.sha256) {
    throw new Error(
      `Runtime archive checksum mismatch: expected ${asset.sha256}, got ${actualArchiveSha}`,
    );
  }
  writeFileSync(archivePath, bytes);

  // bsdtar is present on GitHub's macOS/Linux/Windows images and on supported
  // local developer platforms. Passing each argument separately avoids a
  // shell and keeps archive paths inert.
  execFileSync("tar", ["-xf", archivePath, "-C", extractRoot], {
    stdio: "inherit",
  });

  const candidates = [];
  const walk = (directory) => {
    for (const name of readdirSync(directory)) {
      const path = join(directory, name);
      const stat = lstatSync(path);
      if (stat.isDirectory()) walk(path);
      else candidates.push(path);
    }
  };
  walk(extractRoot);

  const serverName = target.includes("windows")
    ? "llama-server.exe"
    : "llama-server";
  const server = candidates.find((path) => basename(path) === serverName);
  if (!server) {
    throw new Error(`Verified archive did not contain ${serverName}`);
  }
  const serverDirectory = dirname(server);

  const shouldStage = (path) => {
    if (dirname(path) !== serverDirectory) return false;
    const name = basename(path);
    if (name === serverName || name === "LICENSE") return true;
    if (target.includes("windows")) return extname(name).toLowerCase() === ".dll";
    if (target.includes("apple")) return name.endsWith(".dylib");
    return name.includes(".so");
  };

  const selected = candidates.filter(shouldStage);
  if (!selected.some((path) => basename(path) === serverName)) {
    throw new Error(`Runtime staging lost ${serverName}`);
  }

  for (const source of selected) {
    // copyFileSync dereferences archive symlinks. That intentionally produces
    // a flat, portable tree whose versioned and compatibility library names
    // all remain valid after Tauri packages it.
    const destination = join(publishRoot, basename(source));
    copyFileSync(source, destination);
    if (!target.includes("windows") && basename(source) === serverName) {
      chmodSync(destination, 0o755);
    }
  }

  const files = readdirSync(publishRoot)
    .filter((name) => statSync(join(publishRoot, name)).isFile())
    .sort()
    .map((name) => ({
      name,
      sha256: sha256File(join(publishRoot, name)),
      sizeBytes: statSync(join(publishRoot, name)).size,
      executable: name === serverName,
    }));
  writeFileSync(
    join(publishRoot, "runtime-manifest.json"),
    `${JSON.stringify(
      {
        schemaVersion: 1,
        runtime: "llama.cpp",
        version: MANAGED_LLAMA_VERSION,
        target,
        sourceUrl: asset.url,
        archiveSha256: asset.sha256,
        files,
      },
      null,
      2,
    )}\n`,
  );

  rmSync(stageRoot, { recursive: true, force: true });
  mkdirSync(dirname(stageRoot), { recursive: true });
  cpSync(publishRoot, stageRoot, { recursive: true });
  console.log(
    `[stage-managed-runtime] staged ${files.length} files for ${target} at ${stageRoot}`,
  );
} finally {
  rmSync(workRoot, { recursive: true, force: true });
}

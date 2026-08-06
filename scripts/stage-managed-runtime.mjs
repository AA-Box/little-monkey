#!/usr/bin/env node
// Downloads one pinned official runtime archive for the current release
// target, verifies its SHA-256, extracts only the server binary + its adjacent
// runtime libraries/licenses, and stages that self-contained tree as a Tauri
// resource. The resulting runtime is owned by Little Monkey; end users do not
// need Ollama, a system llama.cpp, ComfyUI, or a Python environment.
//
// Usage: node scripts/stage-managed-runtime.mjs [runtime-id]
//   llama (default) — llama.cpp `llama-server`
//   sd              — stable-diffusion.cpp `sd-server`
// The target triple comes from MANAGED_RUNTIME_TARGET, CLI_SIDECAR_TARGET, or
// the host. A runtime that publishes no binary for the target exits non-zero.

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
  managedRuntime,
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
  const detail =
    `No managed ${runtime.manifestRuntime} runtime is pinned for target ${target}. ` +
    `Supported targets: ${Object.keys(runtime.assets).join(", ")}`;
  // An optional runtime simply does not exist on some hosts. Skipping is the
  // correct outcome — the app already treats an absent tree as "this feature
  // is unavailable here" — so a release build for such a target must not fail.
  if (!runtime.optional) throw new Error(detail);
  console.log(`[stage-managed-runtime] ${detail} Skipping.`);
  process.exit(0);
}

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

function cachedStageIsCurrent() {
  if (!existsSync(stagedBinary) || !existsSync(stagedManifest)) return false;
  try {
    const manifest = JSON.parse(readFileSync(stagedManifest, "utf8"));
    if (
      manifest.schemaVersion !== 1 ||
      manifest.runtime !== runtime.manifestRuntime ||
      manifest.version !== runtime.version ||
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
    `[stage-managed-runtime] ${runtime.id} ${runtime.version} already staged for ${target}`,
  );
  process.exit(0);
}

const workRoot = mkdtempSync(
  join(tmpdir(), `little-monkey-${runtime.id}-runtime-`),
);
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

  const server = candidates.find((path) => basename(path) === serverName);
  if (!server) {
    throw new Error(`Verified archive did not contain ${serverName}`);
  }
  const serverDirectory = dirname(server);

  // Extra executables the runtime ships and the app also launches.
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
    // `.txt` covers stable-diffusion.cpp's ggml.txt / stable-diffusion.cpp.txt
    // license notices, which upstream ships instead of a bare LICENSE file.
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
    // copyFileSync dereferences archive symlinks. That intentionally produces
    // a flat, portable tree whose versioned and compatibility library names
    // all remain valid after Tauri packages it.
    const destination = join(publishRoot, basename(source));
    copyFileSync(source, destination);
    if (!target.includes("windows") && executableNames.has(basename(source))) {
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
      executable: executableNames.has(name),
    }));
  writeFileSync(
    join(publishRoot, "runtime-manifest.json"),
    `${JSON.stringify(
      {
        schemaVersion: 1,
        runtime: runtime.manifestRuntime,
        version: runtime.version,
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
    `[stage-managed-runtime] staged ${files.length} ${runtime.id} files for ${target} at ${stageRoot}`,
  );
} finally {
  rmSync(workRoot, { recursive: true, force: true });
}

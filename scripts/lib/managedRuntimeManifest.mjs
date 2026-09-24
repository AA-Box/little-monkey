// Pinned native runtimes used by Little Monkey's release builds. Official
// upstream archives are preferred when stable-diffusion.cpp publishes one for
// the target. Targets without a compatible upstream archive are built from the
// exact pinned upstream commit by stage-managed-runtime.mjs, so Studio remains
// available on every desktop architecture Little Monkey ships.
//
// Three runtimes ship on these rails:
//   llama     — llama.cpp `llama-server`, chat and embedding inference
//   llama-tts — llama.cpp `llama-tts`, speech generation and voice cloning
//   sd        — stable-diffusion.cpp `sd-server`, image and video generation
//
// llama and llama-tts are the same project at two pins on purpose. Speech needs
// the libmtmd rewrite of llama-tts, which reads a backbone plus an mmproj and
// clones a voice from a plain audio clip; the chat pin predates it. Moving the
// chat pin to reach that would re-qualify every chat and embedding path for a
// feature neither uses, so speech gets its own tree instead. The duplicate
// shared libraries cost a few hundred MB and buy total isolation: a speech
// regression cannot reach chat, and the two can be re-pinned independently.
//
// Keep every asset map target-triple keyed: release.yml, the staging script and
// the Rust side already speak Rust triples, so there is one unambiguous source
// of truth for packaging and tests. The `id` and `version` of each runtime must
// stay in step with `ManagedRuntimeSpec` in src-tauri/src/managed_runtime.rs and
// with the staged directory names in src-tauri/build.rs.

import { createHash } from "node:crypto";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export const MANIFEST_FILE = "runtime-manifest.json";

export const MANAGED_LLAMA_VERSION = "b9637";
export const MANAGED_TTS_VERSION = "b10278";
// Latest published stable-diffusion.cpp release at integration time. It is
// deliberately pinned to a release rather than moving master; newer master-only
// fixes are tracked separately and are not claimed as part of this runtime.
export const MANAGED_SD_VERSION = "master-908-88411ef";
export const MANAGED_SD_SOURCE_COMMIT =
  "88411ef1e0688ff2df1010aeeb5d92b2d8cea2be";

const llamaBase = `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_LLAMA_VERSION}`;
const ttsBase = `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_TTS_VERSION}`;
const sdBase = `https://github.com/leejet/stable-diffusion.cpp/releases/download/${MANAGED_SD_VERSION}`;
const sdReleasePage = `https://github.com/leejet/stable-diffusion.cpp/releases/tag/${MANAGED_SD_VERSION}`;

export const MANAGED_LLAMA_ASSETS = Object.freeze({
  "aarch64-apple-darwin": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-macos-arm64.tar.gz`,
    sha256: "72a93f3e68c31de3e438d462669aad1fcdb423b995e9c41033cc7d27a9a3ac69",
  },
  "x86_64-apple-darwin": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-macos-x64.tar.gz`,
    sha256: "71743f8db0958e7c266cceb7add7b16aa418a964667e471094aa6ae65b9c8298",
  },
  "aarch64-unknown-linux-gnu": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-ubuntu-arm64.tar.gz`,
    sha256: "211d9e9ee738698beb7ca271be82661ae2b5da3fbb489cf7d9e4e6ed601be106",
  },
  "x86_64-unknown-linux-gnu": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-ubuntu-x64.tar.gz`,
    sha256: "a50ee14f021a9d8e92e30f622f7e3be1318ee1125bb9a9ba8d2025388df48743",
  },
  "aarch64-pc-windows-msvc": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-win-cpu-arm64.zip`,
    sha256: "db1d3f4c13c08b693f539e100bf6d3a435148b0ffc186b044fdd65d490cc6df7",
  },
  "x86_64-pc-windows-msvc": {
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-win-cpu-x64.zip`,
    sha256: "f7783c2b8c007f95e710ac40f26a24861a80b603b0b739fc54d7c926a4716c1e",
  },
});

// The speech tree. Same six targets and the same CPU archives as the chat
// runtime — TTS is small enough that the CPU build is not the bottleneck, and
// staying on the plain archives keeps this pin trivial to move.
export const MANAGED_TTS_ASSETS = Object.freeze({
  "aarch64-apple-darwin": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-macos-arm64.tar.gz`,
    sha256: "7b007be069f9b4509453813ceeb82643db38e329ee5ae2c59d767a82897d9d88",
  },
  "x86_64-apple-darwin": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-macos-x64.tar.gz`,
    sha256: "f935a12e1f15414c46090e4ffd861120e5561190a3be2eb09926f4709513a6fe",
  },
  "aarch64-unknown-linux-gnu": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-ubuntu-arm64.tar.gz`,
    sha256: "f4dfa82fe0a15375bef580781edc6488940660126265bbecb5a79e28208ce7a1",
  },
  "x86_64-unknown-linux-gnu": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-ubuntu-x64.tar.gz`,
    sha256: "af49b8fdd473e7aeea60745480bf81fac67dcae136797e883b83a0c1f6c82774",
  },
  "aarch64-pc-windows-msvc": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-win-cpu-arm64.zip`,
    sha256: "21a8e0e4177fa1833052e7de4e82d8fd326acb5731ed236441c77afc2174be05",
  },
  "x86_64-pc-windows-msvc": {
    archive: `llama-${MANAGED_TTS_VERSION}-bin-win-cpu-x64.zip`,
    sha256: "2f7cde0ed9e76ccac9f095ede8c6469dc8690cbdd04d9e0a60179d8ff14b8cfa",
  },
});

// Upstream publishes accelerated Qwen-Image-2.1-capable binaries for Apple
// silicon and Windows x64. Linux x64 is intentionally source-built on Little
// Monkey's Ubuntu 22.04 release baseline: the upstream Ubuntu 24.04 archive
// requires newer GLIBC / libstdc++ symbols than that compatibility floor. The
// remaining unpublished architectures use CPU baselines.
export const MANAGED_SD_ASSETS = Object.freeze({
  "aarch64-apple-darwin": {
    archive: "sd-master-88411ef-bin-Darwin-macOS-26.6.2-arm64.zip",
    sha256: "acf9cd2219e69bdfc0faa0d6daaafa696d9551c723b74d6e37f2cd78da386d5a",
  },
  "x86_64-apple-darwin": {
    sourceCommit: MANAGED_SD_SOURCE_COMMIT,
    backend: "cpu",
    cmakeArgs: ["-DCMAKE_OSX_ARCHITECTURES=x86_64"],
  },
  "aarch64-unknown-linux-gnu": {
    sourceCommit: MANAGED_SD_SOURCE_COMMIT,
    backend: "cpu",
    cmakeArgs: [],
  },
  "x86_64-unknown-linux-gnu": {
    sourceCommit: MANAGED_SD_SOURCE_COMMIT,
    backend: "vulkan",
    cmakeArgs: [],
  },
  "aarch64-pc-windows-msvc": {
    sourceCommit: MANAGED_SD_SOURCE_COMMIT,
    backend: "cpu",
    // Pinned ggml rejects the MSVC frontend on ARM. The native Windows ARM64
    // runner ships clang-cl through the Visual Studio ClangCL toolset.
    cmakeArgs: ["-T", "ClangCL"],
  },
  "x86_64-pc-windows-msvc": {
    archive: "sd-master-88411ef-bin-win-vulkan-x64.zip",
    sha256: "e9d089361a00bd30b1e23cc39d2a98745536688acbf5e9e68ce2498a548d07e1",
  },
});

for (const asset of Object.values(MANAGED_LLAMA_ASSETS)) {
  asset.url = `${llamaBase}/${asset.archive}`;
}
for (const asset of Object.values(MANAGED_TTS_ASSETS)) {
  asset.url = `${ttsBase}/${asset.archive}`;
}
for (const asset of Object.values(MANAGED_SD_ASSETS)) {
  asset.url = asset.archive ? `${sdBase}/${asset.archive}` : sdReleasePage;
}

export const MANAGED_RUNTIMES = Object.freeze({
  llama: Object.freeze({
    id: "llama",
    manifestRuntime: "llama.cpp",
    version: MANAGED_LLAMA_VERSION,
    serverBaseName: "llama-server",
    assets: MANAGED_LLAMA_ASSETS,
  }),
  "llama-tts": Object.freeze({
    id: "llama-tts",
    manifestRuntime: "llama.cpp",
    version: MANAGED_TTS_VERSION,
    // The launchable binary here is llama-tts itself: it is a one-shot process
    // that loads its weights, writes one wav and exits, not a server.
    serverBaseName: "llama-tts",
    assets: MANAGED_TTS_ASSETS,
  }),
  sd: Object.freeze({
    id: "sd",
    manifestRuntime: "stable-diffusion.cpp",
    version: MANAGED_SD_VERSION,
    serverBaseName: "sd-server",
    assets: MANAGED_SD_ASSETS,
  }),
});

/** Resolves a runtime by id, failing loudly on an unknown one. */
export function managedRuntime(id) {
  const runtime = MANAGED_RUNTIMES[id];
  if (!runtime) {
    throw new Error(
      `Unknown managed runtime "${id}". Known runtimes: ${Object.keys(MANAGED_RUNTIMES).join(", ")}`,
    );
  }
  return runtime;
}

/**
 * The legacy manifest field is named archiveSha256. For source-built fallback
 * targets there is no downloaded archive, so store a SHA-256 fingerprint of
 * the exact Git commit identifier. The manifest itself is SHA-256 pinned into
 * the Rust binary, and the staging path independently verifies HEAD == commit
 * before compiling, so this remains immutable provenance without a schema
 * migration for the other managed runtimes.
 */
export function managedRuntimeProvenance(asset) {
  if (asset.archive && asset.sha256) return asset.sha256;
  if (asset.sourceCommit) {
    return createHash("sha256").update(asset.sourceCommit).digest("hex");
  }
  throw new Error("Managed runtime asset has no archive digest or source commit");
}

/** Resolves the CMake configure arguments for a source-built runtime. */
export function managedRuntimeSourceCmakeArgs(asset) {
  if (!asset.sourceCommit) return [];
  const args = [
    "-DCMAKE_BUILD_TYPE=Release",
    "-DSD_BUILD_SHARED_LIBS=OFF",
    "-DSD_BUILD_SHARED_GGML_LIB=OFF",
    "-DGGML_NATIVE=OFF",
    "-DSD_WEBP=OFF",
    "-DSD_WEBM=OFF",
    "-DSD_SERVER_BUILD_FRONTEND=OFF",
  ];
  if (asset.backend === "metal") args.push("-DSD_METAL=ON");
  if (asset.backend === "vulkan") args.push("-DSD_VULKAN=ON");
  return [...args, ...(asset.cmakeArgs ?? [])];
}

/** Directory name used for a staged runtime resource. */
export function stagedRuntimeDirectory(runtime) {
  return `${runtime.id}-${runtime.version}`;
}

/**
 * Recomputes a staged manifest's per-file digests and sizes from what is on
 * disk right now, preserving every other field (the Rust side parses this with
 * `deny_unknown_fields`).
 *
 * Codesigning rewrites every Mach-O in the tree *after* staging hashed it, so
 * without this the shipped manifest describes the unsigned upstream binaries
 * and `managed_runtime.rs` rejects the whole tree — which is exactly how 1.2.0
 * shipped a llama-server that never starts.
 */
export function restampRuntimeManifest(directory) {
  const path = join(directory, MANIFEST_FILE);
  const manifest = JSON.parse(readFileSync(path, "utf8"));
  manifest.files = manifest.files.map((file) => {
    const staged = join(directory, file.name);
    return {
      ...file,
      sha256: createHash("sha256").update(readFileSync(staged)).digest("hex"),
      sizeBytes: statSync(staged).size,
    };
  });
  writeFileSync(path, `${JSON.stringify(manifest, null, 2)}\n`);
  return manifest;
}

/** The launchable binary's file name inside a runtime tree, per target. */
export function serverFileName(runtime, target) {
  return target.includes("windows")
    ? `${runtime.serverBaseName}.exe`
    : runtime.serverBaseName;
}

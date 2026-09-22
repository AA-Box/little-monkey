// Pinned native runtimes used by Little Monkey's release builds. Official
// upstream archives are preferred when stable-diffusion.cpp publishes one for
// the target. Targets without an upstream archive are built from the exact
// pinned upstream commit by stage-managed-runtime.mjs, so Studio remains
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
// First upstream stable-diffusion.cpp release with Qwen-Image 2.1 support.
export const MANAGED_SD_VERSION = "master-883-137f740";
export const MANAGED_SD_SOURCE_COMMIT =
  "137f7409bbfb98c70a350a57d6a135487080db96";

const llamaBase = `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_LLAMA_VERSION}`;
const ttsBase = `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_TTS_VERSION}`;
const sdBase = `https://github.com/leejet/stable-diffusion.cpp/releases/download/${MANAGED_SD_VERSION}`;

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
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-win-cpu-x64.tar.gz`,
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
    archive: `llama-${MANAGED_TTS_VERSION}-bin-win-cpu-x64.tar.gz`,
    sha256: "2f7cde0ed9e76ccac9f095ede8c6469dc8690cbdd04d9e0a60179d8ff14b8cfa",
  },
});

// Upstream publishes accelerated Qwen-Image-2.1-capable binaries for these
// three hosts. The other three targets are source-built from the exact same
// commit as CPU baselines. A CPU fallback is deliberate: platform availability
// is the contract; acceleration is an optimization and must never decide
// whether Studio exists.
export const MANAGED_SD_ASSETS = Object.freeze({
  "aarch64-apple-darwin": {
    archive: "sd-master-137f740-bin-Darwin-macOS-26.6.2-arm64.zip",
    sha256: "d850bc4eaa0a2254f0a44b10e1ce1ebf6b7dd866c3ea48a3a73246d49173212f",
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
    archive: "sd-master-137f740-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip",
    sha256: "4b65cfa5e7d4ced8fe43185b314bcba994413655e6273c78aeaaaaeab2ecec0f",
  },
  "aarch64-pc-windows-msvc": {
    sourceCommit: MANAGED_SD_SOURCE_COMMIT,
    backend: "cpu",
    cmakeArgs: [],
  },
  "x86_64-pc-windows-msvc": {
    archive: "sd-master-137f740-bin-win-vulkan-x64.zip",
    sha256: "c76b8427d4dd4946f1f2e088512835550f7c6a17565cf2064ca7b656a6d7f7a6",
  },
});

for (const asset of Object.values(MANAGED_LLAMA_ASSETS)) {
  asset.url = `${llamaBase}/${asset.archive}`;
}
for (const asset of Object.values(MANAGED_TTS_ASSETS)) {
  asset.url = `${ttsBase}/${asset.archive}`;
}
for (const asset of Object.values(MANAGED_SD_ASSETS)) {
  if (asset.archive) asset.url = `${sdBase}/${asset.archive}`;
  else
    asset.url = `https://github.com/leejet/stable-diffusion.cpp/commit/${asset.sourceCommit}`;
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

/** Resolves the immutable provenance token stored in a runtime manifest. */
export function managedRuntimeProvenance(asset) {
  if (asset.archive && asset.sha256) {
    return { archiveSha256: asset.sha256, sourceCommit: null };
  }
  if (asset.sourceCommit) {
    return { archiveSha256: null, sourceCommit: asset.sourceCommit };
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

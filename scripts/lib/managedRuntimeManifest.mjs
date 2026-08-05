// Pinned, official native runtime archives used by Little Monkey's release
// builds. Every archive digest comes from the matching GitHub release asset and
// is verified before extraction by stage-managed-runtime.mjs.
//
// Two runtimes ship on these rails:
//   llama — llama.cpp `llama-server`, chat and embedding inference
//   sd    — stable-diffusion.cpp `sd-server`, image and video generation
//
// Keep every asset map target-triple keyed: release.yml, the staging script and
// the Rust side already speak Rust triples, so there is one unambiguous source
// of truth for packaging and tests. The `id` and `version` of each runtime must
// stay in step with `ManagedRuntimeSpec` in src-tauri/src/managed_runtime.rs and
// with the staged directory names in src-tauri/build.rs.

export const MANAGED_LLAMA_VERSION = "b9637";
export const MANAGED_SD_VERSION = "master-812-ea7f0c8";

const llamaBase = `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_LLAMA_VERSION}`;
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
    archive: `llama-${MANAGED_LLAMA_VERSION}-bin-win-cpu-x64.zip`,
    sha256: "f7783c2b8c007f95e710ac40f26a24861a80b603b0b739fc54d7c926a4716c1e",
  },
});

// Upstream publishes GPU-accelerated builds only for these three hosts: Metal
// on Apple silicon, Vulkan on x86_64 Linux and Windows. Vulkan is deliberate —
// it covers NVIDIA, AMD and Intel from one archive, where CUDA would need a
// separate 362 MB build plus a CUDA runtime. Other hosts get no managed sd
// runtime and Studio stays unavailable there.
export const MANAGED_SD_ASSETS = Object.freeze({
  "aarch64-apple-darwin": {
    archive: "sd-master-ea7f0c8-bin-Darwin-macOS-26.5.2-arm64.zip",
    sha256: "a9ba3ccd1e9e984691d10b143f4c0c801b96351e486272a2a60e930de49cca85",
  },
  "x86_64-unknown-linux-gnu": {
    archive: "sd-master-ea7f0c8-bin-Linux-Ubuntu-24.04-x86_64-vulkan.zip",
    sha256: "a98d446ead81b956a97fa5e04d5aea8acdba0a36e5547c1de88a0a1c0fa7cfd8",
  },
  "x86_64-pc-windows-msvc": {
    archive: "sd-master-ea7f0c8-bin-win-vulkan-x64.zip",
    sha256: "ac785dc435faf616fd9ff1eb864beb6927dcc8f9a7f1875bbfdeab0a3a86b089",
  },
});

for (const asset of Object.values(MANAGED_LLAMA_ASSETS)) {
  asset.url = `${llamaBase}/${asset.archive}`;
}
for (const asset of Object.values(MANAGED_SD_ASSETS)) {
  asset.url = `${sdBase}/${asset.archive}`;
}

export const MANAGED_RUNTIMES = Object.freeze({
  llama: Object.freeze({
    id: "llama",
    manifestRuntime: "llama.cpp",
    version: MANAGED_LLAMA_VERSION,
    serverBaseName: "llama-server",
    assets: MANAGED_LLAMA_ASSETS,
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

/** The staged Tauri resource directory name for a runtime. */
export function stagedRuntimeDirectory(runtime) {
  return `${runtime.id}-${runtime.version}`;
}

/** The launchable binary's file name inside a runtime tree, per target. */
export function serverFileName(runtime, target) {
  return target.includes("windows")
    ? `${runtime.serverBaseName}.exe`
    : runtime.serverBaseName;
}

// Pinned, official llama.cpp CPU runtime archives used by Little Monkey's
// release builds. Every archive digest comes from the matching GitHub release
// asset and is verified before extraction by stage-managed-runtime.mjs.
//
// Keep this map target-triple keyed: release.yml and the local staging script
// already speak Rust triples, so there is one unambiguous source of truth for
// both packaging and tests.

export const MANAGED_LLAMA_VERSION = "b9637";

const releaseBase =
  `https://github.com/ggml-org/llama.cpp/releases/download/${MANAGED_LLAMA_VERSION}`;

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

for (const asset of Object.values(MANAGED_LLAMA_ASSETS)) {
  asset.url = `${releaseBase}/${asset.archive}`;
}

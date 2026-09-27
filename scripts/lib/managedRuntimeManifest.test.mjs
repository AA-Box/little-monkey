import assert from "node:assert/strict";
import test from "node:test";

import {
  MANAGED_LLAMA_ASSETS,
  MANAGED_SD_ASSETS,
  MANAGED_SD_SOURCE_COMMIT,
  MANAGED_SD_VERSION,
  managedRuntimeProvenance,
  managedRuntimeSourceCmakeArgs,
} from "./managedRuntimeManifest.mjs";

const RELEASE_TARGETS = [
  "aarch64-apple-darwin",
  "x86_64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
  "aarch64-pc-windows-msvc",
  "x86_64-pc-windows-msvc",
];

const SOURCE_BUILD_TARGETS = [
  "x86_64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "x86_64-unknown-linux-gnu",
  "aarch64-pc-windows-msvc",
];

const CPU_FALLBACK_TARGETS = [
  "x86_64-apple-darwin",
  "aarch64-unknown-linux-gnu",
  "aarch64-pc-windows-msvc",
];

const UPSTREAM_ARCHIVE_TARGETS = [
  "aarch64-apple-darwin",
  "x86_64-pc-windows-msvc",
];

test("stable-diffusion runtime covers every desktop release target", () => {
  assert.deepEqual(
    Object.keys(MANAGED_SD_ASSETS).sort(),
    [...RELEASE_TARGETS].sort(),
  );
  assert.deepEqual(
    Object.keys(MANAGED_SD_ASSETS).sort(),
    Object.keys(MANAGED_LLAMA_ASSETS).sort(),
  );
});

test("stable-diffusion runtime is pinned to the latest qualified Qwen Image 2.1 release", () => {
  assert.equal(MANAGED_SD_VERSION, "master-920-2f88688");
  assert.equal(
    MANAGED_SD_SOURCE_COMMIT,
    "2f886889e6e8b78738d6b87f7191f6018557c551",
  );

  for (const target of RELEASE_TARGETS) {
    assert.match(managedRuntimeProvenance(MANAGED_SD_ASSETS[target]), /^[0-9a-f]{64}$/);
  }
});

test("compatible published targets use verified upstream accelerated archives", () => {
  for (const target of UPSTREAM_ARCHIVE_TARGETS) {
    const asset = MANAGED_SD_ASSETS[target];
    assert.equal(typeof asset.archive, "string", target);
    assert.match(asset.sha256, /^[0-9a-f]{64}$/, target);
    assert.equal(asset.sourceCommit, undefined, target);
    assert.match(asset.url, /releases\/download\/master-920-2f88688\//, target);
  }
});

test("source-built targets are tied to the exact upstream release commit", () => {
  for (const target of SOURCE_BUILD_TARGETS) {
    const asset = MANAGED_SD_ASSETS[target];
    assert.equal(asset.archive, undefined, target);
    assert.equal(asset.sourceCommit, MANAGED_SD_SOURCE_COMMIT, target);
    assert.match(asset.url, /releases\/tag\/master-920-2f88688$/, target);

    const args = managedRuntimeSourceCmakeArgs(asset);
    assert.ok(args.includes("-DSD_BUILD_SHARED_LIBS=OFF"), target);
    assert.ok(args.includes("-DSD_BUILD_SHARED_GGML_LIB=OFF"), target);
    assert.ok(args.includes("-DGGML_NATIVE=OFF"), target);
    assert.ok(args.includes("-DSD_WEBP=OFF"), target);
    assert.ok(args.includes("-DSD_WEBM=OFF"), target);
    assert.ok(args.includes("-DSD_SERVER_BUILD_FRONTEND=OFF"), target);
  }
});

test("unpublished architectures use portable CPU builds", () => {
  for (const target of CPU_FALLBACK_TARGETS) {
    const asset = MANAGED_SD_ASSETS[target];
    assert.equal(asset.backend, "cpu", target);
    const args = managedRuntimeSourceCmakeArgs(asset);
    assert.ok(!args.includes("-DSD_METAL=ON"), target);
    assert.ok(!args.includes("-DSD_VULKAN=ON"), target);
  }

  assert.ok(
    managedRuntimeSourceCmakeArgs(MANAGED_SD_ASSETS["x86_64-apple-darwin"]).includes(
      "-DCMAKE_OSX_ARCHITECTURES=x86_64",
    ),
  );
});

test("Linux x64 is built with Vulkan on the Ubuntu 22.04 compatibility baseline", () => {
  const asset = MANAGED_SD_ASSETS["x86_64-unknown-linux-gnu"];
  assert.equal(asset.archive, undefined);
  assert.equal(asset.backend, "vulkan");
  assert.ok(managedRuntimeSourceCmakeArgs(asset).includes("-DSD_VULKAN=ON"));
});

test("Windows ARM uses clang-cl because pinned ggml rejects MSVC on ARM", () => {
  const args = managedRuntimeSourceCmakeArgs(
    MANAGED_SD_ASSETS["aarch64-pc-windows-msvc"],
  );
  const toolsetIndex = args.indexOf("-T");
  assert.notEqual(toolsetIndex, -1);
  assert.equal(args[toolsetIndex + 1], "ClangCL");
});

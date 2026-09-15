#!/usr/bin/env node
// Authenticate the exact sherpa-onnx native archive Cargo will statically link.
// sherpa-onnx-sys accepts this directory through SHERPA_ONNX_ARCHIVE_DIR, so
// its own downloader is never part of a Little Monkey build.

import { createHash, randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

export const RUNTIME_VERSION = "1.13.3";

export const RUNTIME_ARCHIVES = Object.freeze({
  "x86_64-unknown-linux-gnu": {
    name: "sherpa-onnx-v1.13.3-linux-x64-static-no-tts-lib.tar.bz2",
    bytes: 19_115_962,
    sha256: "89851a2d6bf5e4cdf3f6bef5b8cbcc2c2982eb87e7aea2e40aaec2806887263c",
  },
  "aarch64-unknown-linux-gnu": {
    name: "sherpa-onnx-v1.13.3-linux-aarch64-static-lib.tar.bz2",
    bytes: 19_327_721,
    sha256: "19b345d73048774452baa775782d0ba75d705a31356ba78acba5521b3faf0933",
  },
  "x86_64-apple-darwin": {
    name: "sherpa-onnx-v1.13.3-osx-x64-static-no-tts-lib.tar.bz2",
    bytes: 17_343_674,
    sha256: "887803fc313c49601916172e981d824074b157f426c9af2106deabb9bd7d1af4",
  },
  "aarch64-apple-darwin": {
    name: "sherpa-onnx-v1.13.3-osx-arm64-static-no-tts-lib.tar.bz2",
    bytes: 17_561_024,
    sha256: "49105e206cf229f1c4cb4707275bc904874c899a1304c506707ba56b40580123",
  },
  "x86_64-pc-windows-msvc": {
    name: "sherpa-onnx-v1.13.3-win-x64-static-MD-Release-no-tts-lib.tar.bz2",
    bytes: 86_255_473,
    sha256: "e4fd99f97ceb288144882d40d05d8e667289bd629c5ff67f0f65f806dd8f25b0",
  },
  "aarch64-pc-windows-msvc": {
    name: "sherpa-onnx-v1.13.3-win-arm64-static-MD-Release-no-tts-lib.tar.bz2",
    bytes: 89_129_055,
    sha256: "412e0ff0dfc94c8f5abf73faf4ed68fdef8fc9b361e4ab78d13624ac2a0b539d",
  },
});

const digestOf = (bytes) => createHash("sha256").update(bytes).digest("hex");

export function verifyRuntimeArchive(path, expected) {
  try {
    const bytes = readFileSync(path);
    return bytes.byteLength === expected.bytes && digestOf(bytes) === expected.sha256;
  } catch {
    return false;
  }
}

function defaultTarget() {
  const architecture = process.arch === "arm64" ? "aarch64" : process.arch === "x64" ? "x86_64" : "";
  const system = process.platform === "darwin"
    ? "apple-darwin"
    : process.platform === "linux"
      ? "unknown-linux-gnu"
      : process.platform === "win32"
        ? "pc-windows-msvc"
        : "";
  return architecture && system ? `${architecture}-${system}` : "";
}

export async function stageSherpaRuntime(requestedTarget = process.env.CARGO_BUILD_TARGET ?? defaultTarget()) {
  const expected = RUNTIME_ARCHIVES[requestedTarget];
  if (!expected) {
    throw new Error(
      `unsupported sherpa-onnx desktop target "${requestedTarget}"; expected one of ${Object.keys(RUNTIME_ARCHIVES).join(", ")}`,
    );
  }
  const root = join(dirname(fileURLToPath(import.meta.url)), "..");
  const destinationDirectory = join(root, "src-tauri", "resources", "sherpa-onnx-runtime");
  const destination = join(destinationDirectory, expected.name);
  if (verifyRuntimeArchive(destination, expected)) {
    console.log(`[stage-sherpa-runtime] already staged ${destination}`);
    return;
  }
  mkdirSync(destinationDirectory, { recursive: true });
  const transaction = join(destinationDirectory, `.${expected.name}.${randomUUID()}`);
  const previous = join(destinationDirectory, `.${expected.name}.previous.${randomUUID()}`);
  const url = `https://github.com/k2-fsa/sherpa-onnx/releases/download/v${RUNTIME_VERSION}/${expected.name}`;
  try {
    console.log(`[stage-sherpa-runtime] downloading ${url}`);
    const response = await fetch(url, { redirect: "follow" });
    if (!response.ok) throw new Error(`runtime download returned ${response.status}`);
    const bytes = Buffer.from(await response.arrayBuffer());
    if (bytes.byteLength !== expected.bytes) {
      throw new Error(`runtime archive is ${bytes.byteLength} bytes, expected ${expected.bytes}`);
    }
    const digest = digestOf(bytes);
    if (digest !== expected.sha256) {
      throw new Error(`runtime archive checksum mismatch: expected ${expected.sha256}, got ${digest}`);
    }
    writeFileSync(transaction, bytes, { flag: "wx" });
    if (existsSync(destination)) renameSync(destination, previous);
    try {
      renameSync(transaction, destination);
    } catch (error) {
      if (existsSync(previous) && !existsSync(destination)) renameSync(previous, destination);
      throw error;
    }
    rmSync(previous, { force: true });
    console.log(`[stage-sherpa-runtime] staged ${destination} (sha256 ${digest})`);
  } finally {
    rmSync(transaction, { force: true });
    if (existsSync(previous) && existsSync(destination)) rmSync(previous, { force: true });
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  await stageSherpaRuntime(process.argv[2] ?? process.env.CARGO_BUILD_TARGET ?? defaultTarget());
}

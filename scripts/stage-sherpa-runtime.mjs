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
    name: "sherpa-onnx-v1.13.3-linux-x64-static-lib.tar.bz2",
    bytes: 20_289_645,
    sha256: "f4908b2abdaadb24fc8885c2e671598b922a1a97fd07db14afea648bab459aac",
  },
  "aarch64-unknown-linux-gnu": {
    name: "sherpa-onnx-v1.13.3-linux-aarch64-static-lib.tar.bz2",
    bytes: 19_327_721,
    sha256: "19b345d73048774452baa775782d0ba75d705a31356ba78acba5521b3faf0933",
  },
  "x86_64-apple-darwin": {
    name: "sherpa-onnx-v1.13.3-osx-x64-static-lib.tar.bz2",
    bytes: 18_330_073,
    sha256: "9469e3a03a28756e85a2f1125ac997da5a182c539c45b191bfc59ba6903c06e2",
  },
  "aarch64-apple-darwin": {
    name: "sherpa-onnx-v1.13.3-osx-arm64-static-lib.tar.bz2",
    bytes: 18_735_820,
    sha256: "8a524849ea13db3abe667f5f785280b2396dee17856c912e22cb24d0344b9a5a",
  },
  "x86_64-pc-windows-msvc": {
    name: "sherpa-onnx-v1.13.3-win-x64-static-MT-Release-lib.tar.bz2",
    bytes: 114_663_805,
    sha256: "f6555701d6397d74f1302b0666a661f32708b599a14a5fde80835d4902fcd315",
  },
  "aarch64-pc-windows-msvc": {
    name: "sherpa-onnx-v1.13.3-win-arm64-static-MT-Release-lib.tar.bz2",
    bytes: 119_074_753,
    sha256: "b198b3227e5b87018bc99584d0e8a7b5f895e07550b39c6b0db7f577d632a5b3",
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

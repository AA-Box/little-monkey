#!/usr/bin/env node
// Stage the pinned sherpa-onnx open-vocabulary KWS model used while Talk is
// armed. The archive is authenticated before extraction and every file the
// runtime will open is authenticated again before the directory is activated.
// No application code downloads these weights at run time.

import { createHash, randomUUID } from "node:crypto";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

export const MODEL_ID = "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01";
export const MODEL_URL =
  `https://github.com/k2-fsa/sherpa-onnx/releases/download/kws-models/${MODEL_ID}.tar.bz2`;
export const MODEL_ARCHIVE_BYTES = 17_626_723;
export const MODEL_ARCHIVE_SHA256 =
  "f170013b4716e41b62b9bfd809687c207cef798ef9bc6534d524e17af9b6561a";

export const MODEL_FILES = Object.freeze({
  "encoder-epoch-12-avg-2-chunk-16-left-64.onnx": {
    bytes: 12_174_219,
    sha256: "063fbc1aeae8a9b574607a331a00e60371846ef9eaa3c1d9ea48176665dfc693",
  },
  "decoder-epoch-12-avg-2-chunk-16-left-64.onnx": {
    bytes: 1_063_189,
    sha256: "f61ebd3eed3773a44d088d53dfae92dbb6aec4839f4dcaee2d402414741663a3",
  },
  "joiner-epoch-12-avg-2-chunk-16-left-64.onnx": {
    bytes: 642_462,
    sha256: "0d7a37e749d8055223029318d6ffae82db1dae2d315d0892a68ba5dad17c1d2d",
  },
  "tokens.txt": {
    bytes: 5_006,
    sha256: "fd2ded4050a55d2b1578870ba8697d02371980217806b7558bd0a5cc60f3ba53",
  },
  "bpe.model": {
    bytes: 244_837,
    sha256: "c8a2a0129c4ab8e463164c142f82d25649661b122c8cd0b7aab5c9e80b90ad24",
  },
  "README.md": {
    bytes: 726,
    sha256: "74e42d37d63acd2366b042151d576fcc0c4917a41de339414a37f1e79f4e80e2",
  },
});

export const FIXTURE_FILES = Object.freeze({
  "test_wavs/0.wav": {
    bytes: 212_044,
    sha256: "6bc58a4efdf20daac252b6b1502632601a71efe0308f6757dc1eda34891a7e4f",
  },
  "test_wavs/1.wav": {
    bytes: 534_924,
    sha256: "5143a6ba93c4b274e2c4ac22deb75c2c48936c853f0519add1de828b6c79cc5a",
  },
});

const BUNDLED_FILES = Object.freeze({ ...MODEL_FILES, ...FIXTURE_FILES });

const digestOf = (bytes) => createHash("sha256").update(bytes).digest("hex");

export function verifyModelDirectory(directory) {
  return Object.entries(BUNDLED_FILES).every(([name, expected]) => {
    try {
      const bytes = readFileSync(join(directory, name));
      return bytes.byteLength === expected.bytes && digestOf(bytes) === expected.sha256;
    } catch {
      return false;
    }
  });
}

export async function stageWakeWordModel() {
  const root = join(dirname(fileURLToPath(import.meta.url)), "..");
  const resources = join(root, "src-tauri", "resources");
  const destination = join(resources, "local-wake-word");
  if (verifyModelDirectory(destination)) {
    console.log(`[stage-wake-word-model] already staged ${destination}`);
    return;
  }

  mkdirSync(resources, { recursive: true });
  const transaction = join(resources, `.local-wake-word-${randomUUID()}`);
  const archive = join(transaction, `${MODEL_ID}.tar.bz2`);
  const extracted = join(transaction, MODEL_ID);
  const candidate = join(transaction, "candidate");
  const previous = join(resources, `.local-wake-word-previous-${randomUUID()}`);
  try {
    mkdirSync(transaction, { recursive: false });
    console.log(`[stage-wake-word-model] downloading ${MODEL_URL}`);
    const response = await fetch(MODEL_URL, { redirect: "follow" });
    if (!response.ok) throw new Error(`model download returned ${response.status}`);
    const bytes = Buffer.from(await response.arrayBuffer());
    if (bytes.byteLength !== MODEL_ARCHIVE_BYTES) {
      throw new Error(`model archive is ${bytes.byteLength} bytes, expected ${MODEL_ARCHIVE_BYTES}`);
    }
    const digest = digestOf(bytes);
    if (digest !== MODEL_ARCHIVE_SHA256) {
      throw new Error(`model archive checksum mismatch: expected ${MODEL_ARCHIVE_SHA256}, got ${digest}`);
    }
    writeFileSync(archive, bytes);
    const unpack = spawnSync("tar", ["-xjf", archive, "-C", transaction], {
      encoding: "utf8",
      windowsHide: true,
    });
    if (unpack.status !== 0) {
      throw new Error(`could not extract KWS model: ${(unpack.stderr || unpack.stdout).trim()}`);
    }
    if (!verifyModelDirectory(extracted)) {
      throw new Error("the extracted KWS model does not match its pinned file manifest");
    }
    mkdirSync(candidate);
    for (const name of Object.keys(BUNDLED_FILES)) {
      const destinationFile = join(candidate, name);
      mkdirSync(dirname(destinationFile), { recursive: true });
      cpSync(join(extracted, name), destinationFile);
    }
    if (!verifyModelDirectory(candidate)) {
      throw new Error("the minimal KWS bundle does not match its pinned file manifest");
    }
    if (existsSync(destination)) renameSync(destination, previous);
    try {
      renameSync(candidate, destination);
    } catch (error) {
      if (existsSync(previous) && !existsSync(destination)) renameSync(previous, destination);
      throw error;
    }
    rmSync(previous, { recursive: true, force: true });
    console.log(
      `[stage-wake-word-model] staged ${destination} (${statSync(destination).isDirectory() ? "verified" : "invalid"}, archive sha256 ${digest})`,
    );
  } finally {
    rmSync(transaction, { recursive: true, force: true });
  }
}

if (process.argv[1] === fileURLToPath(import.meta.url)) await stageWakeWordModel();

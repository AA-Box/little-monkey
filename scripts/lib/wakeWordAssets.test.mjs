import assert from "node:assert/strict";
import { mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import {
  FIXTURE_FILES,
  MODEL_ARCHIVE_SHA256,
  MODEL_FILES,
  MODEL_ID,
  verifyModelDirectory,
} from "../stage-wake-word-model.mjs";
import {
  RUNTIME_ARCHIVES,
  RUNTIME_VERSION,
  verifyRuntimeArchive,
} from "../stage-sherpa-runtime.mjs";

test("wake assets pin one runtime for all six desktop targets", () => {
  assert.equal(RUNTIME_VERSION, "1.13.3");
  assert.deepEqual(Object.keys(RUNTIME_ARCHIVES).sort(), [
    "aarch64-apple-darwin",
    "aarch64-pc-windows-msvc",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-unknown-linux-gnu",
  ]);
  for (const expected of Object.values(RUNTIME_ARCHIVES)) {
    assert.match(expected.name, /v1\.13\.3/);
    assert.match(expected.sha256, /^[a-f0-9]{64}$/);
    assert.ok(expected.bytes > 1_000_000);
  }
});

test("the vendored Rust build hook enforces the same runtime manifest", () => {
  const buildHook = readFileSync(join(process.cwd(), "vendor/sherpa-onnx-sys/build.rs"), "utf8");
  for (const expected of Object.values(RUNTIME_ARCHIVES)) {
    assert.ok(buildHook.includes(`\"${expected.name}\"`));
    assert.ok(buildHook.includes(expected.sha256));
    assert.ok(buildHook.includes(expected.bytes.toLocaleString("en-US").replaceAll(",", "_")));
  }
});

test("model manifest is exact and rejects a partial directory", () => {
  assert.equal(MODEL_ID, "sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01");
  assert.match(MODEL_ARCHIVE_SHA256, /^[a-f0-9]{64}$/);
  assert.deepEqual(Object.keys(MODEL_FILES).sort(), [
    "README.md",
    "bpe.model",
    "decoder-epoch-12-avg-2-chunk-16-left-64.onnx",
    "encoder-epoch-12-avg-2-chunk-16-left-64.onnx",
    "joiner-epoch-12-avg-2-chunk-16-left-64.onnx",
    "tokens.txt",
  ]);
  assert.deepEqual(Object.keys(FIXTURE_FILES).sort(), ["test_wavs/0.wav", "test_wavs/1.wav"]);
  const directory = join(tmpdir(), `little-monkey-kws-assets-${process.pid}-${Date.now()}`);
  mkdirSync(directory);
  try {
    writeFileSync(join(directory, "tokens.txt"), "partial");
    assert.equal(verifyModelDirectory(directory), false);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("the installer manifest includes every runtime model file but no test audio", () => {
  const tauri = JSON.parse(
    readFileSync(join(process.cwd(), "src-tauri/tauri.conf.json"), "utf8"),
  );
  const resources = tauri.bundle.resources;
  for (const name of Object.keys(MODEL_FILES)) {
    assert.ok(resources.includes(`resources/local-wake-word/${name}`), name);
  }
  assert.equal(resources.some((name) => name.includes("test_wavs")), false);
});

test("runtime verification rejects an archive with the right size but wrong digest", () => {
  const directory = join(tmpdir(), `little-monkey-kws-runtime-${process.pid}-${Date.now()}`);
  const archive = join(directory, "runtime.tar.bz2");
  mkdirSync(directory);
  try {
    writeFileSync(archive, Buffer.alloc(32));
    assert.equal(
      verifyRuntimeArchive(archive, { bytes: 32, sha256: "0".repeat(64) }),
      false,
    );
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

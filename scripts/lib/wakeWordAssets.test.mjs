import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { basename, join } from "node:path";
import test from "node:test";

import {
  FIXTURE_DIRECTORY,
  FIXTURE_FILES,
  fixtureManifestByBasename,
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

/**
 * Scratch space inside the repository's own ignored cache, not the operating
 * system's temp directory.
 *
 * `mkdtemp` under `/tmp` is safe in itself, but everything written beneath it
 * still lands in a world-writable directory, which is what a scanner sees and
 * what an attacker on a shared build host would go looking for. A fixture that
 * exists for the length of one assertion has no reason to be there at all.
 */
function scratchDirectory(prefix) {
  const cache = join(process.cwd(), "node_modules", ".cache");
  mkdirSync(cache, { recursive: true });
  return mkdtempSync(join(cache, prefix));
}

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
  // Keyed by their path inside the archive; staged flat, outside the bundle.
  assert.deepEqual(Object.keys(FIXTURE_FILES).sort(), ["test_wavs/0.wav", "test_wavs/1.wav"]);
  assert.deepEqual(Object.keys(fixtureManifestByBasename()).sort(), ["0.wav", "1.wav"]);
  const directory = scratchDirectory("little-monkey-kws-assets-");
  try {
    writeFileSync(join(directory, "tokens.txt"), "partial");
    assert.equal(verifyModelDirectory(directory), false);
  } finally {
    rmSync(directory, { recursive: true, force: true });
  }
});

test("the installer packages the model directory whole, and test audio is not in it", () => {
  const tauri = JSON.parse(
    readFileSync(join(process.cwd(), "src-tauri/tauri.conf.json"), "utf8"),
  );
  const resources = tauri.bundle.resources;
  // A glob rather than six filenames, because `tauri-build` resolves resource
  // paths on every compile: a list of files nobody has staged yet fails every
  // `cargo build` in a fresh checkout, which is how this arrangement was found.
  assert.ok(resources.includes("resources/local-wake-word/**/*"));
  // Which makes "no test audio in the installer" a property of the directory.
  // Nothing may stage a fixture into the packaged tree.
  assert.equal(
    Object.keys(FIXTURE_FILES).some((name) =>
      resources.some((resource) => resource.includes(basename(name)))
    ),
    false,
  );
  assert.equal(FIXTURE_DIRECTORY, "local-wake-word-fixtures");
  assert.ok(!FIXTURE_DIRECTORY.startsWith("local-wake-word/"));
});

test("a fresh checkout can still resolve the packaged glob", () => {
  // `tauri-build` fails the build outright when a resource path matches
  // nothing, so the directory carries one tracked file that staging never
  // overwrites. Without it, cloning the repository and running `cargo test`
  // fails before any test runs.
  const placeholder = join(process.cwd(), "src-tauri/resources/local-wake-word/PLACEHOLDER.md");
  assert.ok(readFileSync(placeholder, "utf8").length > 0);
  assert.equal(Object.keys(MODEL_FILES).includes("PLACEHOLDER.md"), false);
});

test("runtime verification rejects an archive with the right size but wrong digest", () => {
  const directory = scratchDirectory("little-monkey-kws-runtime-");
  const archive = join(directory, "runtime.tar.bz2");
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

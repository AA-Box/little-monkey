// Sanity check for restampRuntimeManifest, the step that keeps a staged
// runtime manifest describing the *signed* binaries. When this silently stops
// working, macOS release builds ship a managed runtime whose every file fails
// its own checksum, and llama-server can never be launched at all (1.2.0).
// Run with: pnpm test:manifest

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { restampRuntimeManifest } from "./managedRuntimeManifest.mjs";

const directory = mkdtempSync(join(tmpdir(), "little-monkey-manifest-test-"));

try {
  // "signed" stands in for the post-codesign bytes: different content and a
  // different length than whatever staging hashed.
  writeFileSync(join(directory, "llama-server"), "signed llama-server bytes");
  writeFileSync(join(directory, "libllama.dylib"), "signed dylib");
  writeFileSync(
    join(directory, "runtime-manifest.json"),
    JSON.stringify({
      schemaVersion: 1,
      runtime: "llama.cpp",
      version: "b9637",
      target: "aarch64-apple-darwin",
      sourceUrl: "https://github.com/ggml-org/llama.cpp/releases/download/b9637/x.tar.gz",
      archiveSha256: "a".repeat(64),
      files: [
        { name: "llama-server", sha256: "b".repeat(64), sizeBytes: 1, executable: true },
        { name: "libllama.dylib", sha256: "c".repeat(64), sizeBytes: 2, executable: false },
      ],
    }),
  );

  restampRuntimeManifest(directory);
  const manifest = JSON.parse(
    readFileSync(join(directory, "runtime-manifest.json"), "utf8"),
  );

  for (const file of manifest.files) {
    const bytes = readFileSync(join(directory, file.name));
    assert.equal(
      file.sha256,
      createHash("sha256").update(bytes).digest("hex"),
      `${file.name} digest was not restamped`,
    );
    assert.equal(file.sizeBytes, bytes.length, `${file.name} size was not restamped`);
  }

  // Every other field must survive verbatim: the Rust side parses this with
  // serde's deny_unknown_fields and re-checks provenance against it.
  assert.equal(manifest.schemaVersion, 1);
  assert.equal(manifest.runtime, "llama.cpp");
  assert.equal(manifest.version, "b9637");
  assert.equal(manifest.target, "aarch64-apple-darwin");
  assert.equal(manifest.archiveSha256, "a".repeat(64));
  assert.deepEqual(
    manifest.files.map((file) => [file.name, file.executable]),
    [
      ["llama-server", true],
      ["libllama.dylib", false],
    ],
  );

  console.log("[runtimeManifest.test] ok");
} finally {
  rmSync(directory, { recursive: true, force: true });
}

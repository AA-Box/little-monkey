// Sanity check for the Mach-O detection helper codesign-managed-runtime.mjs
// relies on to find every binary it must sign. This is the one part of the
// notarization fix that can be verified without a macOS runner, so it is
// worth locking in: if this ever misclassifies a binary as "not Mach-O",
// codesign-managed-runtime.mjs would silently skip signing it and the
// managed llama.cpp runtime would fail Apple notarization again with no
// warning until deep into a release build.
//
// Run directly: node scripts/lib/machO.test.mjs

import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { isMachOFile } from "./machO.mjs";

const root = mkdtempSync(join(tmpdir(), "little-monkey-macho-test-"));

const file = (name, bytes) => {
  const path = join(root, name);
  writeFileSync(path, bytes);
  return path;
};

try {
  // Real magic numbers Mach-O / universal binaries start with, each padded
  // with a few trailing bytes so the "at least 4 bytes" path is exercised.
  const machOCases = {
    "macho32.bin": [0xfe, 0xed, 0xfa, 0xce],
    "macho32-swapped.bin": [0xce, 0xfa, 0xed, 0xfe],
    "macho64.dylib": [0xfe, 0xed, 0xfa, 0xcf],
    "macho64-swapped.dylib": [0xcf, 0xfa, 0xed, 0xfe],
    "fat.bin": [0xca, 0xfe, 0xba, 0xbe],
    "fat-swapped.bin": [0xbe, 0xba, 0xfe, 0xca],
  };
  for (const [name, magic] of Object.entries(machOCases)) {
    const path = file(name, Buffer.from([...magic, 0x00, 0x00, 0x00, 0x00]));
    assert.equal(isMachOFile(path), true, `${name} should be detected as Mach-O`);
  }

  // Non Mach-O content that a staged managed-runtime directory also
  // contains today (LICENSE, runtime-manifest.json) or could plausibly
  // contain in the future - none of these should ever be signed.
  assert.equal(
    isMachOFile(file("LICENSE", "MIT License\n\nCopyright...")),
    false,
  );
  assert.equal(
    isMachOFile(file("runtime-manifest.json", JSON.stringify({ schemaVersion: 1 }))),
    false,
  );
  assert.equal(isMachOFile(file("empty.bin", Buffer.alloc(0))), false);
  assert.equal(isMachOFile(file("short.bin", Buffer.from([0xfe, 0xed]))), false);
  assert.equal(
    isMachOFile(file("elf.bin", Buffer.from([0x7f, 0x45, 0x4c, 0x46]))),
    false,
    "an ELF binary (Linux .so) must never be treated as Mach-O",
  );

  console.log("[machO.test] all cases passed");
} finally {
  rmSync(root, { recursive: true, force: true });
}

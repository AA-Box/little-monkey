/**
 * The bootstrap placeholder must never cost anyone a real `monkey-cli`.
 *
 * Run with: pnpm test:cli-sidecar
 *
 * tauri-build copies the staged sidecar over `target/<profile>/monkey-cli` on
 * every build of this package, and an incremental build relinks nothing — so
 * whatever is staged is what a developer's next `cargo test` runs. These tests
 * state the invariant in the only terms that survive that copy: after staging,
 * the staged bytes are either a real binary's or there was no real binary to
 * stage. See scripts/lib/cliSidecarPlaceholder.mjs for the mechanism.
 */
import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  builtCliPath,
  DEBUG,
  ensureSidecarPlaceholder,
  RELEASE,
  sidecarStagedPath,
} from "./cliSidecarPlaceholder.mjs";

const TRIPLE = "aarch64-apple-darwin";
/** Stands in for a linked binary. Only its bytes matter to any of this. */
const REAL_BINARY = Buffer.from("\x7fELF a real 60 MB binary, in spirit");

function repo() {
  const root = mkdtempSync(join(tmpdir(), "cli-sidecar-"));
  mkdirSync(join(root, "src-tauri", "target", "debug"), { recursive: true });
  return root;
}

function writeBuiltCli(root, profile, bytes) {
  const dir = join(root, "src-tauri", "target", profile);
  mkdirSync(dir, { recursive: true });
  const path = join(dir, "monkey-cli");
  writeFileSync(path, bytes);
  return path;
}

/** What tauri-build does with the staged path on every build of the package. */
function tauriBuildCopiesSidecarIntoTarget(root, profile) {
  const staged = readFileSync(sidecarStagedPath(root, TRIPLE, false));
  writeFileSync(join(root, "src-tauri", "target", profile, "monkey-cli"), staged);
}

test("a fresh checkout with nothing built gets an empty placeholder", () => {
  const root = repo();
  try {
    const staged = ensureSidecarPlaceholder(root, TRIPLE, false);
    assert.equal(staged, sidecarStagedPath(root, TRIPLE, false));
    // Empty is the point: it exists, which is all tauri-build's existence
    // check wants, and it displaces nothing because nothing is built yet.
    assert.equal(readFileSync(staged).length, 0);
    assert.equal(builtCliPath(root, false, { profile: DEBUG }), undefined);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("an incremental build cannot replace a real binary with a placeholder", () => {
  const root = repo();
  try {
    const built = writeBuiltCli(root, "debug", REAL_BINARY);
    // The sequence that used to destroy it: stage, then let tauri-build copy
    // the staged file over the binary cargo will not relink.
    ensureSidecarPlaceholder(root, TRIPLE, false);
    tauriBuildCopiesSidecarIntoTarget(root, "debug");
    assert.deepEqual(readFileSync(built), REAL_BINARY, "the real binary survived staging");

    // And repeatedly, because a developer runs this on every `pnpm test:rust`.
    for (let i = 0; i < 5; i += 1) {
      ensureSidecarPlaceholder(root, TRIPLE, false);
      tauriBuildCopiesSidecarIntoTarget(root, "debug");
    }
    assert.deepEqual(readFileSync(built), REAL_BINARY);
    assert.ok(readFileSync(built).length > 0);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a stale staged binary is refreshed rather than copied back over a newer one", () => {
  const root = repo();
  try {
    // A previous build staged an older binary; the current build produced a
    // different one. Nothing here consults a timestamp — a macOS copy carries
    // its source's mtime, so only the bytes can decide.
    const staged = sidecarStagedPath(root, TRIPLE, false);
    mkdirSync(join(root, "src-tauri", "binaries"), { recursive: true });
    writeFileSync(staged, Buffer.from("an older binary"));
    const built = writeBuiltCli(root, "debug", REAL_BINARY);

    ensureSidecarPlaceholder(root, TRIPLE, false);
    tauriBuildCopiesSidecarIntoTarget(root, "debug");
    assert.deepEqual(readFileSync(built), REAL_BINARY);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("staging never writes through a worktree's symlink into another checkout", () => {
  const root = repo();
  const other = repo();
  try {
    const shared = writeBuiltCli(other, "release", Buffer.from("the main checkout's binary"));
    mkdirSync(join(root, "src-tauri", "binaries"), { recursive: true });
    symlinkSync(shared, sidecarStagedPath(root, TRIPLE, false));
    writeBuiltCli(root, "debug", REAL_BINARY);

    ensureSidecarPlaceholder(root, TRIPLE, false);
    assert.deepEqual(
      readFileSync(shared),
      Buffer.from("the main checkout's binary"),
      "the other checkout's binary was not written through",
    );
    assert.deepEqual(readFileSync(sidecarStagedPath(root, TRIPLE, false)), REAL_BINARY);
  } finally {
    rmSync(root, { recursive: true, force: true });
    rmSync(other, { recursive: true, force: true });
  }
});

test("a tree already holding a zero-byte binary is repaired rather than left broken", () => {
  const root = repo();
  try {
    // The state an older placeholder left behind. Cargo rebuilds a missing
    // output but not an empty one, so this has to go for the tree to recover.
    const built = writeBuiltCli(root, "debug", Buffer.alloc(0));
    ensureSidecarPlaceholder(root, TRIPLE, false);
    assert.throws(() => readFileSync(built), /ENOENT/, "the empty binary was cleared");
    assert.equal(readFileSync(sidecarStagedPath(root, TRIPLE, false)).length, 0);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("each profile stages its own binary when both exist", () => {
  const root = repo();
  try {
    // `pnpm stage:cli` builds --release while a debug binary from `cargo test`
    // sits in the same tree. Answering with the debug one would seed the
    // release destination with a debug executable — a substitution that still
    // runs, which is what makes it worth a test.
    const debugBinary = Buffer.from("the debug binary");
    const releaseBinary = Buffer.from("the release binary");
    writeBuiltCli(root, DEBUG, debugBinary);
    writeBuiltCli(root, RELEASE, releaseBinary);

    ensureSidecarPlaceholder(root, TRIPLE, false, { profile: RELEASE });
    assert.deepEqual(readFileSync(sidecarStagedPath(root, TRIPLE, false)), releaseBinary);
    tauriBuildCopiesSidecarIntoTarget(root, RELEASE);
    assert.deepEqual(
      readFileSync(join(root, "src-tauri", "target", RELEASE, "monkey-cli")),
      releaseBinary,
    );

    ensureSidecarPlaceholder(root, TRIPLE, false, { profile: DEBUG });
    assert.deepEqual(readFileSync(sidecarStagedPath(root, TRIPLE, false)), debugBinary);
    tauriBuildCopiesSidecarIntoTarget(root, DEBUG);
    assert.deepEqual(
      readFileSync(join(root, "src-tauri", "target", DEBUG, "monkey-cli")),
      debugBinary,
    );

    // Neither profile's binary was ever seeded from the other's.
    assert.deepEqual(
      readFileSync(join(root, "src-tauri", "target", RELEASE, "monkey-cli")),
      releaseBinary,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a bootstrap for one profile leaves the other profile's binary alone", () => {
  const root = repo();
  try {
    // Only a release binary exists and the debug bootstrap runs: it must not
    // stage the release binary into the debug destination either.
    const releaseBinary = Buffer.from("the release binary");
    writeBuiltCli(root, RELEASE, releaseBinary);

    const staged = ensureSidecarPlaceholder(root, TRIPLE, false, { profile: DEBUG });
    assert.equal(readFileSync(staged).length, 0, "an empty placeholder, not the wrong profile");
    assert.deepEqual(
      readFileSync(join(root, "src-tauri", "target", RELEASE, "monkey-cli")),
      releaseBinary,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a cross build stages from its own triple-named target directory", () => {
  const root = repo();
  try {
    const target = "x86_64-pc-windows-msvc";
    const dir = join(root, "src-tauri", "target", target, "release");
    mkdirSync(dir, { recursive: true });
    writeFileSync(join(dir, "monkey-cli.exe"), REAL_BINARY);
    // The host's own debug output must not be mistaken for the cross build's.
    writeBuiltCli(root, "debug", Buffer.from("the host binary"));

    assert.equal(
      builtCliPath(root, true, { profile: RELEASE, explicitTarget: target }),
      join(dir, "monkey-cli.exe"),
    );
    const staged = ensureSidecarPlaceholder(root, target, true, {
      profile: RELEASE,
      explicitTarget: target,
    });
    assert.deepEqual(readFileSync(staged), REAL_BINARY);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

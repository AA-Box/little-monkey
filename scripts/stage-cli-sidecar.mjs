#!/usr/bin/env node
// Builds `monkey-cli` and stages it into src-tauri/binaries/ under the
// target-triple-suffixed name Tauri's `externalBin` sidecar convention
// expects (see src-tauri/tauri.conf.json's bundle.externalBin), so
// `pnpm dev:app` / `pnpm tauri build` never trip "sidecar binary not
// found" — wired in as the last step of `pnpm stage:all` (which
// `dev:app` runs before handing over to `tauri dev`) and as the first
// half of beforeBuildCommand.
//
// It deliberately does *not* run from beforeDevCommand: the Tauri CLI
// starts a 180s countdown for `build.devUrl` the moment it spawns that
// command, and a cold `cargo build --release` here outlasts it, so the
// dev run died with "Could not connect to http://127.0.0.1:1420/ after
// 180s" before Vite was ever reached. Staging ahead of `tauri dev`
// leaves Vite as the only thing that countdown is timing.
//
// Local dev: builds for the host triple (no `--target` flag, so it doesn't
// require a rustup target to be installed beyond the default host one).
// CI (release.yml): set CLI_SIDECAR_TARGET to the matrix target explicitly
// and this passes `--target` through to cargo instead of guessing.
//
// Writes an empty placeholder at the staged path *before* invoking cargo —
// see scripts/lib/cliSidecarPlaceholder.mjs for the bootstrap-cycle reason
// this step exists at all — then overwrites it with the real binary once
// the build succeeds.

import { execFileSync } from "node:child_process";
import { chmodSync, copyFileSync, renameSync, rmSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  cliDestination,
  ensureSidecarPlaceholder,
  filesIdentical,
  hostTriple,
  RELEASE,
} from "./lib/cliSidecarPlaceholder.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = join(repoRoot, "src-tauri", "Cargo.toml");

const explicitTarget = process.env.CLI_SIDECAR_TARGET;
const target = explicitTarget || hostTriple();
const isWindows = target.includes("windows");

// This builds `--release`, so the release binary is both what should be
// staged and where tauri-build will copy it back onto. Naming the profile
// keeps a debug binary sitting in the same tree out of the release path.
const stagedPath = ensureSidecarPlaceholder(repoRoot, target, isWindows, {
  profile: RELEASE,
  explicitTarget,
});

const cargoArgs = ["build", "--release", "--bin", "monkey-cli", "--manifest-path", manifestPath];
if (explicitTarget) cargoArgs.push("--target", explicitTarget);

console.log(`[stage-cli-sidecar] cargo ${cargoArgs.join(" ")}`);
execFileSync("cargo", cargoArgs, { stdio: "inherit" });

const builtPath = cliDestination(repoRoot, isWindows, { profile: RELEASE, explicitTarget });

// The one thing that must never be staged is an empty file: tauri-build
// copies whatever is here over the target directory's `monkey-cli`, and a
// zero-byte sidecar reaches the bundle looking exactly like a real one.
if (statSync(builtPath).size === 0) {
  throw new Error(`${builtPath} is empty — refusing to stage it as the sidecar`);
}
// Same rerun-if-changed cycle the placeholder step avoids: rewriting the
// staged path with bytes it already holds invalidates tauri-build's build
// script and forces the next cargo build to redo everything.
if (filesIdentical(builtPath, stagedPath)) {
  console.log(`[stage-cli-sidecar] ${stagedPath} already current`);
} else {
  // Stage through a sibling and rename over, rather than copying onto the
  // path in place. A copy in place keeps the inode, and an endpoint security
  // agent that has pinned that inode goes on killing every later exec from it
  // — the sidecar then dies with SIGKILL while the identical bytes run fine
  // from any other path. A rename gives each staged build a new inode, so a
  // pin cannot outlive the build it was taken against. The rename is atomic,
  // which also means a crash mid-copy can no longer leave a truncated sidecar
  // at the path tauri-build copies into the bundle.
  const staging = `${stagedPath}.staging`;
  try {
    copyFileSync(builtPath, staging);
    if (!isWindows) chmodSync(staging, 0o755);
    renameSync(staging, stagedPath);
  } catch (error) {
    rmSync(staging, { force: true });
    throw error;
  }
  console.log(`[stage-cli-sidecar] staged ${stagedPath}`);
}

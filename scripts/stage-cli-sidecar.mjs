#!/usr/bin/env node
// Builds `monkey-cli` and stages it into src-tauri/binaries/ under the
// target-triple-suffixed name Tauri's `externalBin` sidecar convention
// expects (see src-tauri/tauri.conf.json's bundle.externalBin), so
// `pnpm tauri dev` / `pnpm tauri build` never trip "sidecar binary not
// found" — wired in as the first half of both beforeDevCommand and
// beforeBuildCommand.
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
import { chmodSync, copyFileSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  cliDestination,
  ensureSidecarPlaceholder,
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
copyFileSync(builtPath, stagedPath);
if (!isWindows) chmodSync(stagedPath, 0o755);

console.log(`[stage-cli-sidecar] staged ${stagedPath}`);

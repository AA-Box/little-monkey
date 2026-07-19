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
import { chmodSync, copyFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ensureSidecarPlaceholder, hostTriple } from "./lib/cliSidecarPlaceholder.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const manifestPath = join(repoRoot, "src-tauri", "Cargo.toml");

const explicitTarget = process.env.CLI_SIDECAR_TARGET;
const target = explicitTarget || hostTriple();
const isWindows = target.includes("windows");

const stagedPath = ensureSidecarPlaceholder(repoRoot, target, isWindows);

const cargoArgs = ["build", "--release", "--bin", "monkey-cli", "--manifest-path", manifestPath];
if (explicitTarget) cargoArgs.push("--target", explicitTarget);

console.log(`[stage-cli-sidecar] cargo ${cargoArgs.join(" ")}`);
execFileSync("cargo", cargoArgs, { stdio: "inherit" });

const builtName = isWindows ? "monkey-cli.exe" : "monkey-cli";
const builtDir = explicitTarget
  ? join(repoRoot, "src-tauri", "target", explicitTarget, "release")
  : join(repoRoot, "src-tauri", "target", "release");
const builtPath = join(builtDir, builtName);

copyFileSync(builtPath, stagedPath);
if (!isWindows) chmodSync(stagedPath, 0o755);

console.log(`[stage-cli-sidecar] staged ${stagedPath}`);

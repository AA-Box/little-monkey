#!/usr/bin/env node
// Cheap bootstrap step for `pnpm test:rust`/plain Rust CI (ci.yml): creates
// an empty placeholder at the externalBin path tauri-build's build script
// checks for, without building the real monkey-cli binary — see
// scripts/lib/cliSidecarPlaceholder.mjs for why this is needed at all.
// `pnpm stage:cli` (used by tauri dev/build and the release workflow) does
// the real build and overwrites this placeholder with the real binary.
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { ensureSidecarPlaceholder, hostTriple } from "./lib/cliSidecarPlaceholder.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const explicitTarget = process.env.CLI_SIDECAR_TARGET;
const target = explicitTarget || hostTriple();
const isWindows = target.includes("windows");

const path = ensureSidecarPlaceholder(repoRoot, target, isWindows, explicitTarget);
console.log(`[ensure-cli-sidecar-placeholder] ${path}`);

#!/usr/bin/env node
// Codesigns every Mach-O binary (dylibs + llama-server) that
// stage-managed-runtime.mjs downloads from upstream llama.cpp and stages
// as a Tauri "resource". Resources are copied into the .app's
// Contents/Resources verbatim - Tauri signs the main app binary itself and
// any bundle.externalBin sidecars (see monkey-cli in tauri.conf.json), but
// it has no idea these resource files exist and never signs them.
//
// Apple's notarization service inspects every Mach-O inside the uploaded
// bundle, so each one needs its own Developer ID signature, hardened
// runtime, and secure timestamp. Without this step, every dylib and the
// llama-server executable under managed-runtime/ fail notarization with
// "Archive contains critical validation errors" - this is what broke both
// macOS builds (x86_64 and aarch64) in the 1.1.0 release CI run.
//
// Binaries are found by content (see lib/machO.mjs), not by filename, so a
// future llama.cpp release that renames, adds, or restructures its binaries
// is signed automatically instead of silently shipping unsigned and failing
// notarization again.
//
// No-ops on non-Apple targets and whenever APPLE_SIGNING_IDENTITY isn't
// set (plain local dev builds), so it is always safe to call unconditionally
// from tauri.conf.json's beforeBuildCommand on every platform.

import { execFileSync } from "node:child_process";
import { existsSync, readdirSync, statSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { isMachOFile } from "./lib/machO.mjs";
import { MANAGED_LLAMA_VERSION } from "./lib/managedRuntimeManifest.mjs";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const target =
  process.env.MANAGED_RUNTIME_TARGET || process.env.CLI_SIDECAR_TARGET || "";
const identity = process.env.APPLE_SIGNING_IDENTITY;

if (!target.includes("apple")) {
  console.log("[codesign-managed-runtime] not an Apple target, skipping");
  process.exit(0);
}

if (!identity) {
  console.log(
    "[codesign-managed-runtime] APPLE_SIGNING_IDENTITY not set, skipping (unsigned local build)",
  );
  process.exit(0);
}

const stageRoot = join(
  repoRoot,
  "src-tauri",
  "resources",
  "managed-runtime",
  `llama-${MANAGED_LLAMA_VERSION}`,
);

if (!existsSync(stageRoot)) {
  throw new Error(
    `[codesign-managed-runtime] ${stageRoot} does not exist - run "pnpm stage:runtime" first`,
  );
}

const binaries = readdirSync(stageRoot)
  .map((name) => join(stageRoot, name))
  .filter((path) => statSync(path).isFile() && isMachOFile(path));

if (binaries.length === 0) {
  throw new Error(
    `[codesign-managed-runtime] no Mach-O binaries found under ${stageRoot} - ` +
      "the managed runtime staging step may have changed. Refusing to continue " +
      "rather than silently notarizing an unsigned bundle.",
  );
}

for (const path of binaries) {
  console.log(`[codesign-managed-runtime] signing ${path}`);
  execFileSync(
    "codesign",
    ["--force", "--options", "runtime", "--timestamp", "--sign", identity, path],
    { stdio: "inherit" },
  );
}

// Verify immediately and fail loudly here, in seconds, instead of ~15
// minutes later inside Apple's notarization service with the error buried
// in a JSON report on the Actions "Annotations" tab.
for (const path of binaries) {
  execFileSync("codesign", ["--verify", "--strict", "--verbose=2", path], {
    stdio: "inherit",
  });
}

console.log(
  `[codesign-managed-runtime] signed and verified ${binaries.length} binaries for ${target}`,
);

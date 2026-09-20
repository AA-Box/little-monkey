#!/usr/bin/env node
// Removes the previously bundled .app before `tauri build` writes a new one.
//
// The bundler copies the freshly built executables over the ones already in
// Contents/MacOS. When the app from the last build is still running — which it
// is, every single time you are building to test a change — that copy does not
// take, and the bundler reports success anyway. The result is an .app whose
// Resources are new and whose binary is however old the last build that ran
// with the app closed was: measured at eight days stale, with every Rust
// command added since silently missing from the running app while the frontend
// changes around them shipped normally. Days of "the fix does nothing".
//
// Unlinking the bundle first sidesteps it: a running process keeps its inode
// and the bundler writes into an empty directory, so the copy cannot lose.
// Cheap, too — the bundler rewrites every one of those files regardless.
import { rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
// Honours CARGO_TARGET_DIR, which the worktree setup points at a shared dir.
const targetDir = process.env.CARGO_TARGET_DIR ?? join(repoRoot, "src-tauri", "target");

for (const profile of ["release", "debug"]) {
  rmSync(join(targetDir, profile, "bundle", "macos"), { recursive: true, force: true });
}

// Shared by stage-cli-sidecar.mjs and ensure-cli-sidecar-placeholder.mjs.
//
// tauri-build's build script checks that every `bundle.externalBin` path
// resolves to a real file on disk, and it runs on *every* cargo build of
// this package — not just `tauri build`/`tauri dev`, but a plain
// `cargo check`/`cargo test`/`cargo build --bin monkey-cli` too. That's a
// bootstrap cycle: compiling `monkey-cli` itself first compiles
// `little_monkey_lib`, which runs this same check. Creating an empty
// placeholder at the expected path before the first real cargo invocation
// breaks the cycle — the check only verifies existence, not content, and
// nothing at compile time reads the staged file itself. `stage-cli-sidecar.mjs`
// overwrites the placeholder with the real compiled binary right after.
//
// # Why a placeholder cannot be written whenever one is wanted
//
// tauri-build does not only *check* the staged path. It copies it into the
// cargo target directory under the sidecar's un-suffixed name — for this
// project `src-tauri/binaries/monkey-cli-<triple>` lands on
// `src-tauri/target/<profile>/monkey-cli`, which is exactly the path
// `CARGO_BIN_EXE_monkey-cli` hands to `tests/cli_processes_limits.rs`, and
// exactly where cargo puts the binary it links.
//
// On a *fresh* build that is harmless: the copy runs from the build script,
// before cargo links the binary, so the real binary lands last. On an
// *incremental* build where `monkey-cli` is already up to date, cargo relinks
// nothing — so the copy is last, and an empty placeholder replaces a working
// 60 MB binary with a zero-byte file. Cargo's fingerprint still says the
// binary is current, so nothing rebuilds it, and every later run of the CLI
// fails for a reason nothing in the failure names.
//
// The invariant that closes this: **an empty placeholder is written only when
// there is no real binary for it to displace.** When one exists it is staged
// instead, so the copy that used to destroy it now restores exactly the same
// bytes. What decides is the file's identity, never its timestamp — which on
// macOS a copy carries across from its source anyway.
import { execFileSync } from "node:child_process";
import {
  chmodSync,
  constants,
  copyFileSync,
  lstatSync,
  mkdirSync,
  statSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";

export function sidecarStagedPath(repoRoot, target, isWindows) {
  return join(repoRoot, "src-tauri", "binaries", `monkey-cli-${target}${isWindows ? ".exe" : ""}`);
}

function fileSize(path) {
  try {
    const stats = statSync(path);
    return stats.isFile() ? stats.size : 0;
  } catch {
    return 0;
  }
}

function isSymlink(path) {
  try {
    return lstatSync(path).isSymbolicLink();
  } catch {
    return false;
  }
}

/**
 * The real `monkey-cli` a previous cargo build left behind, if there is one.
 *
 * Debug first: that is the profile `cargo test` and `cargo run` use, so it is
 * the binary an incremental developer build stands to lose. A cross build
 * (`CLI_SIDECAR_TARGET`) puts its output under a triple-named directory
 * instead, so that layout is the one consulted in that case.
 */
export function builtCliPath(repoRoot, isWindows, explicitTarget) {
  return cliDestinations(repoRoot, isWindows, explicitTarget).find((path) => fileSize(path) > 0);
}

function cliDestinations(repoRoot, isWindows, explicitTarget) {
  const name = isWindows ? "monkey-cli.exe" : "monkey-cli";
  const targetRoot = join(repoRoot, "src-tauri", "target");
  const profiles = explicitTarget
    ? [join(targetRoot, explicitTarget, "debug"), join(targetRoot, explicitTarget, "release")]
    : [join(targetRoot, "debug"), join(targetRoot, "release")];
  return profiles.map((profile) => join(profile, name));
}

/**
 * Clear a zero-byte `monkey-cli` out of the target directory.
 *
 * Recovery for a tree that a previous version of this script already damaged,
 * and for anything else that leaves a truncated file there — an interrupted
 * copy, a full disk. Cargo rebuilds an output that is missing but not one that
 * is merely empty, so removing it is what lets the next build repair itself
 * instead of failing several minutes later inside a test that runs the CLI.
 *
 * Safe by construction: an empty file is never a binary anyone can run.
 */
function removeEmptyBuiltCli(repoRoot, isWindows, explicitTarget) {
  for (const path of cliDestinations(repoRoot, isWindows, explicitTarget)) {
    try {
      if (!isSymlink(path) && statSync(path).isFile() && statSync(path).size === 0) {
        unlinkSync(path);
      }
    } catch {
      // Nothing there, or nothing removable. The build decides what happens.
    }
  }
}

/**
 * Put something at the staged sidecar path that tauri-build can copy into the
 * target directory without destroying anything: the real binary when one
 * exists, an empty placeholder only when none does.
 */
export function ensureSidecarPlaceholder(repoRoot, target, isWindows, explicitTarget) {
  mkdirSync(join(repoRoot, "src-tauri", "binaries"), { recursive: true });
  const path = sidecarStagedPath(repoRoot, target, isWindows);
  removeEmptyBuiltCli(repoRoot, isWindows, explicitTarget);
  const built = builtCliPath(repoRoot, isWindows, explicitTarget);
  if (built) {
    // Not create-if-missing: a stale binary from an older build would be
    // copied over the current one just as destructively as an empty
    // placeholder. Staging the current binary makes tauri-build's copy a
    // no-op in content, whatever was staged before.
    //
    // Unlinked rather than written through, because a worktree that shares
    // one build with the main checkout stages this path as a symlink, and
    // writing through that would reach into another checkout's binaries.
    if (isSymlink(path)) unlinkSync(path);
    copyFileSync(built, path, constants.COPYFILE_FICLONE);
    if (!isWindows) chmodSync(path, 0o755);
    return path;
  }
  try {
    // The "wx" flag makes create-if-missing a single atomic step. An
    // existsSync check followed by writeFileSync races concurrent cargo
    // builds and could truncate a real binary staged in between.
    writeFileSync(path, "", { flag: "wx" });
    if (!isWindows) chmodSync(path, 0o755);
  } catch (error) {
    if (error.code !== "EEXIST") throw error;
  }
  return path;
}

export function hostTriple() {
  const out = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const match = out.match(/^host:\s*(\S+)/m);
  if (!match) {
    throw new Error("could not determine host target triple from `rustc -vV`");
  }
  return match[1];
}

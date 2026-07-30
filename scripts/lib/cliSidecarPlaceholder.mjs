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
import { execFileSync } from "node:child_process";
import { chmodSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

export function sidecarStagedPath(repoRoot, target, isWindows) {
  return join(repoRoot, "src-tauri", "binaries", `monkey-cli-${target}${isWindows ? ".exe" : ""}`);
}

/** Create-if-missing only — never clobbers an already-staged real binary. */
export function ensureSidecarPlaceholder(repoRoot, target, isWindows) {
  mkdirSync(join(repoRoot, "src-tauri", "binaries"), { recursive: true });
  const path = sidecarStagedPath(repoRoot, target, isWindows);
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

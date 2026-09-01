/**
 * Where the `tauri dev` staging steps are allowed to run.
 *
 * Run with: pnpm test:tauri-dev-command
 *
 * **What this proves:** that `build.beforeDevCommand` starts nothing but the
 * Vite dev server, and that the staging chain runs ahead of `tauri dev` in
 * `pnpm dev:app` instead. The Tauri CLI starts a fixed 180s countdown for
 * `build.devUrl` the moment it spawns `beforeDevCommand`, and it exposes no
 * knob to lengthen it — `TAURI_CLI_NO_DEV_SERVER_WAIT` only skips the wait
 * outright, which would race the sidecar staging it is waiting on. So a
 * staging step in front of `pnpm dev` spends that countdown on itself: a cold
 * `pnpm stage:cli` is a `cargo build --release`, six minutes on a warm cache,
 * and the dev run died with "Could not connect to `http://127.0.0.1:1420/`
 * after 180s" having never reached Vite at all. Vite alone answers in under a
 * second, so the countdown is only ever timing Vite.
 *
 * **What this does NOT prove:** that the staged runtimes or the sidecar are
 * correct — `pnpm test:cli-sidecar` and `pnpm test:runtime-archive` hold
 * those — or that `tauri dev` reaches a window. It reads the two manifests
 * and nothing else.
 */
import { strict as assert } from "node:assert";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..", "..");
const readJson = (...parts) => JSON.parse(readFileSync(join(repoRoot, ...parts), "utf8"));

const scripts = readJson("package.json").scripts;
const beforeDevCommand = readJson("src-tauri", "tauri.conf.json").build.beforeDevCommand;

test("beforeDevCommand starts the dev server and nothing else", () => {
  assert.equal(beforeDevCommand, "pnpm dev");
});

test("no staging step runs inside the dev-server countdown", () => {
  // `stage:cli` is the expensive one, but any of these ahead of `pnpm dev`
  // spends the countdown before Vite is started.
  for (const step of ["stage:runtime", "stage:runtime:tts", "stage:runtime:sd", "stage:cli"]) {
    assert.ok(
      !beforeDevCommand.includes(step),
      `beforeDevCommand runs "${step}" before starting Vite; move it into stage:all`,
    );
  }
});

test("dev:app stages before handing over to tauri dev", () => {
  const devApp = scripts["dev:app"];
  assert.ok(devApp, "package.json is missing the dev:app script");
  const stageAt = devApp.indexOf("stage:all");
  const tauriAt = devApp.indexOf("tauri dev");
  assert.ok(stageAt !== -1, "dev:app must run stage:all");
  assert.ok(tauriAt !== -1, "dev:app must run tauri dev");
  assert.ok(stageAt < tauriAt, "dev:app must stage before starting tauri dev");
});

test("stage:all covers every step beforeDevCommand used to run", () => {
  const stageAll = scripts["stage:all"];
  assert.ok(stageAll, "package.json is missing the stage:all script");
  for (const step of ["stage:runtime", "stage:runtime:tts", "stage:runtime:sd", "stage:cli"]) {
    assert.ok(stageAll.includes(step), `stage:all must run "${step}"`);
  }
});

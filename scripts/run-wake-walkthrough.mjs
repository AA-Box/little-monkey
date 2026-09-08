#!/usr/bin/env node
// Portable entry point for the wake-word acceptance walkthrough.
//
// The obvious form — `LITTLE_MONKEY_WAKE_WALKTHROUGH_E2E=1 vitest run ...` in a
// package script — works in a POSIX shell and not at all on Windows. A
// workflow's `shell: bash` default covers the step, but pnpm runs its own
// scripts through the platform shell, so on Windows the line reaches cmd.exe
// and it says so:
//
//   'LITTLE_MONKEY_WAKE_WALKTHROUGH_E2E' is not recognized as an internal or
//   external command, operable program or batch file.
//
// Setting the variable here keeps one command that behaves the same on every
// host, which matters because this is the command the docs tell an operator to
// run.

import { spawnSync } from "node:child_process";

function run(command, args) {
  const result = spawnSync(command, args, {
    stdio: "inherit",
    env: { ...process.env, LITTLE_MONKEY_WAKE_WALKTHROUGH_E2E: "1" },
    // `cargo` and `pnpm` are `.cmd` shims on Windows, which `spawn` will not
    // execute without a shell.
    shell: process.platform === "win32",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) process.exit(result.status ?? 1);
}

run("cargo", ["build", "--manifest-path", "src-tauri/Cargo.toml", "--bin", "wake-word-e2e"]);
run("pnpm", ["exec", "vitest", "run", "src/lib/wakeWordWalkthrough.e2e.test.ts"]);

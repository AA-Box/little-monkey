/**
 * Backstop against the Red-Team Lab regrowing a copy of the Rust permission
 * table.
 *
 * The behavioural guards are the real defence: `permissions.rs`'s
 * `red_team_corpus_*` tests assert the table itself, and
 * `redTeamLiveLoop.test.ts` asserts the loop actually wraps untrusted results.
 * This file exists because a reviewer can talk themselves into "just inline the
 * floor list, it's only six entries" — which is precisely how the original
 * copy drifted six file classes behind `permissions.rs` while every test in the
 * lab stayed green.
 */
import { readFileSync } from "node:fs";
import path from "node:path";

import { describe, expect, it } from "vitest";

const LIB_DIR = path.dirname(new URL(import.meta.url).pathname);
const runnerSource = readFileSync(path.join(LIB_DIR, "redTeamRunner.ts"), "utf8");

/** Only the executable half — doc comments legitimately name these symbols to
 * explain what was removed and why. */
const runnerCode = runnerSource
  .replace(/\/\*[\s\S]*?\*\//g, "")
  .replace(/^\s*\/\/.*$/gm, "");

describe("redTeamRunner does not reimplement the Rust permission table", () => {
  it.each([
    ["SHELL_RC_FILES", "the floored shell rc list lives in permissions.rs"],
    ["SCRIPT_EXECUTING_MANIFESTS", "the floored manifest list lives in permissions.rs"],
    ["pathRiskFloor", "path_risk_floor is Rust-only"],
    ["modeShortCircuit", "mode_short_circuit is Rust-only"],
  ])("has no %s (%s)", (symbol) => {
    expect(runnerCode).not.toContain(symbol);
  });

  it("asks the backend for the gate verdict", () => {
    expect(runnerCode).toContain("permission_dry_run");
  });

  it("does not hardcode the mode decision table", () => {
    // A reintroduced copy would need to branch on the mode names.
    for (const mode of ["acceptEdits", "bypass"]) {
      const occurrences = runnerCode.split(`"${mode}"`).length - 1;
      expect(
        occurrences,
        `redTeamRunner.ts branches on the "${mode}" mode — the decision belongs to permissions.rs`,
      ).toBe(0);
    }
  });

  it("observes the untrusted-content boundary but never reimplements it", () => {
    // Containment still calls the real boundary functions; what it must never do
    // is grow its own wrapping logic to compare against.
    expect(runnerCode).toContain("protectToolResult");
    expect(runnerCode).not.toContain("BEGIN UNTRUSTED");
  });
});

describe("the fixture corpus is shared with Rust, not duplicated", () => {
  it("is a JSON data file the TypeScript loader reads rather than inline literals", () => {
    const loader = readFileSync(path.join(LIB_DIR, "redTeamFixtures.ts"), "utf8");
    expect(loader).toContain('from "./redTeamFixtures.json"');
  });

  it("is the same file permissions.rs compiles in", () => {
    const permissions = readFileSync(
      path.join(LIB_DIR, "..", "..", "src-tauri", "src", "permissions.rs"),
      "utf8",
    );
    expect(permissions).toContain('include_str!("../../src/lib/redTeamFixtures.json")');
  });
});

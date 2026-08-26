#!/usr/bin/env node

import { execFileSync, spawnSync } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

function arg(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const target = arg("--target") ?? process.env.LITTLE_MONKEY_AUTONOMOUS_EVAL_TARGET;
const monkey = arg("--monkey-bin") ?? process.env.LITTLE_MONKEY_CLI ?? "monkey";
if (!target) {
  console.error("Live autonomous evaluation requires --target <target> or LITTLE_MONKEY_AUTONOMOUS_EVAL_TARGET.");
  process.exit(2);
}

const repository = mkdtempSync(join(tmpdir(), "little-monkey-live-autonomous-eval-"));
const keep = process.env.LITTLE_MONKEY_KEEP_EVAL_REPO === "1";
const startedAt = Date.now();

function write(path, content) {
  mkdirSync(dirname(join(repository, path)), { recursive: true });
  writeFileSync(join(repository, path), content);
}

function git(args) {
  return execFileSync("git", args, { cwd: repository, encoding: "utf8" });
}

function run(command, args, options = {}) {
  const result = spawnSync(command, args, {
    cwd: options.cwd ?? repository,
    encoding: "utf8",
    env: process.env,
    maxBuffer: 16 * 1024 * 1024,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    const stderr = (result.stderr ?? "").trim();
    const stdout = (result.stdout ?? "").trim();
    throw new Error(`${command} ${args.join(" ")} failed (${result.status})\n${stderr || stdout}`);
  }
  return result.stdout ?? "";
}

function jsonCandidates(text) {
  const candidates = [];
  const trimmed = text.trim();
  if (trimmed) candidates.push(trimmed);
  for (const line of text.split(/\r?\n/).reverse()) if (line.trim().startsWith("{")) candidates.push(line.trim());
  return candidates;
}

function parseJsonOutput(text) {
  for (const candidate of jsonCandidates(text)) {
    try { return JSON.parse(candidate); } catch { /* try next candidate */ }
  }
  throw new Error(`Expected JSON from monkey task command. Output:\n${text.slice(-4000)}`);
}

function findRunId(value) {
  if (!value || typeof value !== "object") return undefined;
  for (const key of ["run_id", "runId", "id"]) {
    if (typeof value[key] === "string" && value[key]) return value[key];
  }
  for (const nested of Object.values(value)) {
    const found = findRunId(nested);
    if (found) return found;
  }
  return undefined;
}

function changedFiles() {
  return git(["status", "--porcelain=v1", "--untracked-files=all"])
    .split(/\r?\n/)
    .filter(Boolean)
    .map((line) => line.slice(3).trim())
    .sort();
}

try {
  write("package.json", JSON.stringify({ private: true, type: "module", scripts: { test: "node test.mjs" } }, null, 2) + "\n");
  write("src/math.mjs", "export function add(a, b) { return a - b; }\n");
  write("test.mjs", "import assert from 'node:assert/strict';\nimport { add } from './src/math.mjs';\nassert.equal(add(2, 3), 5);\n");
  write("MONKEY.md", "# Evaluation repository\n\nFix production code, never weaken or delete tests. Before completion run `npm test` and require it to pass. Do not modify files outside this repository.\n");

  git(["init", "-q"]);
  git(["config", "user.email", "live-eval@example.invalid"]);
  git(["config", "user.name", "Live Autonomous Eval"]);
  git(["add", "."]);
  git(["-c", "commit.gpgsign=false", "commit", "-qm", "broken baseline"]);

  let baselineFailed = false;
  try { run(process.execPath, ["test.mjs"]); } catch { baselineFailed = true; }
  if (!baselineFailed) throw new Error("Evaluation fixture is invalid: its baseline test unexpectedly passes.");

  const objective = [
    "Find and fix the cause of the failing repository test.",
    "Do not modify test.mjs or MONKEY.md.",
    "Make the smallest production change in src/math.mjs.",
    "Run npm test before completion and only claim success with execution evidence.",
  ].join(" ");

  const startOutput = run(monkey, ["task", "start", objective, "--target", target, "--workspace", repository, "--json"], { cwd: repository });
  const start = parseJsonOutput(startOutput);
  const runId = findRunId(start);
  if (!runId) throw new Error(`monkey task start did not return a run id: ${startOutput.slice(-4000)}`);

  const attachOutput = run(monkey, ["task", "attach", runId, "--follow", "--json"], { cwd: repository });
  const attached = parseJsonOutput(attachOutput);

  run("npm", ["test"], { cwd: repository });
  const changed = changedFiles();
  if (!changed.includes("src/math.mjs")) throw new Error(`Model run did not mutate src/math.mjs. Changed files: ${changed.join(", ") || "none"}`);
  for (const forbidden of ["test.mjs", "MONKEY.md"]) {
    if (changed.includes(forbidden)) throw new Error(`Model run changed forbidden evaluation file ${forbidden}.`);
  }
  const source = readFileSync(join(repository, "src/math.mjs"), "utf8");
  if (source.includes("a - b")) throw new Error("Production bug remains after the autonomous run.");

  console.log(JSON.stringify({
    fixture: "live-model-one-file-bug",
    target,
    runId,
    changedFiles: changed,
    verification: "npm test",
    verified: true,
    wallTimeMs: Date.now() - startedAt,
    finalRun: attached,
    repository: keep ? repository : undefined,
  }, null, 2));
} catch (error) {
  console.error(error instanceof Error ? error.stack ?? error.message : String(error));
  console.error(`Evaluation repository: ${repository}`);
  process.exitCode = 1;
} finally {
  if (!keep && process.exitCode !== 1) rmSync(repository, { recursive: true, force: true });
}

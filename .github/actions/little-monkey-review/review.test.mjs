import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const root = mkdtempSync(join(tmpdir(), "little-monkey-action-test-"));
const statePath = join(root, "gh-state.json");
const eventPath = join(root, "event.json");
const outputPath = join(root, "output.txt");
const summaryPath = join(root, "summary.md");
const fakeGh = join(root, "gh");
const fakeFetch = join(root, "fake-fetch.mjs");

writeFileSync(statePath, JSON.stringify({ body: null, posts: 0, patches: 0 }));
writeFileSync(eventPath, JSON.stringify({ pull_request: { number: 17 } }));
writeFileSync(fakeGh, `#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
const path = process.env.FAKE_GH_STATE;
const state = JSON.parse(fs.readFileSync(path, "utf8"));
const save = () => fs.writeFileSync(path, JSON.stringify(state));
if (args[0] === "pr" && args[1] === "view") {
  process.stdout.write(JSON.stringify({ number: 17, title: "Fixture PR", state: "OPEN", headRefOid: "${"b".repeat(40)}", isCrossRepository: false }));
} else if (args[0] === "pr" && args[1] === "diff") {
  process.stdout.write("diff --git a/src/lib.js b/src/lib.js\\n--- a/src/lib.js\\n+++ b/src/lib.js\\n@@ -1 +1,2 @@\\n old\\n+new\\n");
} else if (args[0] === "api" && args[1] === "user") {
  process.stdout.write("fixture-bot\\n");
} else if (args[0] === "api" && args.includes("--paginate")) {
  const comments = state.body ? [{ id: 99, body: state.body, user: { login: "fixture-bot" } }] : [];
  process.stdout.write(JSON.stringify([comments]));
} else if (args[0] === "api" && args.includes("POST")) {
  let input = ""; process.stdin.setEncoding("utf8"); process.stdin.on("data", c => input += c); process.stdin.on("end", () => { state.body = JSON.parse(input).body; state.posts += 1; save(); process.stdout.write(JSON.stringify({ id: 99 })); });
} else if (args[0] === "api" && args.includes("PATCH")) {
  let input = ""; process.stdin.setEncoding("utf8"); process.stdin.on("data", c => input += c); process.stdin.on("end", () => { state.body = JSON.parse(input).body; state.patches += 1; save(); process.stdout.write(JSON.stringify({ id: 99 })); });
} else {
  process.stderr.write("unhandled fake gh: " + JSON.stringify(args)); process.exitCode = 1;
}
`);
chmodSync(fakeGh, 0o755);
writeFileSync(fakeFetch, `
globalThis.fetch = async (url, options) => {
  if (String(url) !== "http://127.0.0.1:11434/api/chat" || options?.method !== "POST") throw new Error("unexpected Ollama request");
  const parsed = JSON.parse(options.body);
  if (parsed.model !== "fixture-model") throw new Error("unexpected model");
  return new Response(JSON.stringify({ message: { content: JSON.stringify({ summary: "Fixture review", findings: [{ severity: "warning", path: "src/lib.js", line: 2, title: "Fixture", body: "Check the added line" }] }) } }), { status: 200, headers: { "content-type": "application/json" } });
};
`);

const env = {
  ...process.env,
  NODE_OPTIONS: `--import=${fakeFetch}`,
  PATH: `${root}:${process.env.PATH}`,
  FAKE_GH_STATE: statePath,
  RUNNER_ENVIRONMENT: "self-hosted",
  RUNNER_TEMP: root,
  GITHUB_EVENT_PATH: eventPath,
  GITHUB_REPOSITORY: "owner/repo",
  GITHUB_OUTPUT: outputPath,
  GITHUB_STEP_SUMMARY: summaryPath,
  GH_TOKEN: "fixture-token",
  LM_REVIEW_MODEL: "fixture-model",
  LM_OLLAMA_URL: "http://127.0.0.1:11434",
  LM_REPOSITORY: "owner/repo",
  LM_PR_NUMBER: "17",
  LM_PUBLISH: "true",
  LM_FAIL_ON_BLOCKING: "false",
  LM_MAX_DIFF_BYTES: "8388608",
};

try {
  for (let run = 0; run < 2; run += 1) {
    writeFileSync(outputPath, "");
    const result = spawnSync(process.execPath, [join(here, "review.mjs")], { env, encoding: "utf8" });
    assert.equal(result.status, 0, `${result.stdout}\n${result.stderr}`);
    const outputs = readFileSync(outputPath, "utf8");
    assert.match(outputs, /report-digest=[a-f0-9]{64}/);
    assert.match(outputs, /comment-id=99/);
  }
  const state = JSON.parse(readFileSync(statePath, "utf8"));
  assert.equal(state.posts, 1);
  assert.equal(state.patches, 1);
  assert.equal((state.body.match(/little-monkey-review-v1/g) || []).length, 1);
  assert.match(state.body, /src\/lib\.js.*2/);
  const audit = readFileSync(join(root, "little-monkey-review-audit.jsonl"), "utf8").trim().split("\n").map(JSON.parse);
  assert.equal(audit.length, 4);
  assert.deepEqual(audit.map((entry) => entry.action), ["create_report", "create_report", "update_report", "update_report"]);
  assert.deepEqual(audit.map((entry) => entry.outcome), ["pending", "success", "pending", "success"]);
  assert.ok(audit.every((entry) => /^[a-f0-9]{64}$/.test(entry.requestDigest)));
} finally {
  rmSync(root, { recursive: true, force: true });
}

console.log("little-monkey-review action fixture passed");

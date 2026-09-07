#!/usr/bin/env node

/**
 * Real-provider acceptance for desktop realtime voice.
 *
 * Runs the app itself — the same webview, the same WebRTC stack, the same
 * native keychain broker, the same tool executor — with the acceptance harness
 * enabled, then reads the evidence the native side writes. The operator speaks
 * one short request when the run says so, and answers one question at the end:
 * whether the answer was audible, which is the only part of the chain no
 * measurement inside the app can reach.
 *
 * Exit codes: 0 every step passed and the operator heard the answer, 2 every
 * measurable step passed but the speaker is unconfirmed, 1 something failed.
 *
 *   pnpm test:realtime:live --path README.md
 *
 * Requires an OpenAI key saved through Settings, a workspace open on the file
 * given by --path, network access, and microphone permission.
 */

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createInterface } from "node:readline";
import { tmpdir } from "node:os";
import { join } from "node:path";

function arg(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const testPath = arg("--path") ?? process.env.LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH;
if (!testPath) {
  console.error("Realtime acceptance requires --path <file readable in the open workspace>.");
  process.exit(2);
}

const timeoutMs = Number(arg("--timeout-ms") ?? 420_000);
const directory = mkdtempSync(join(tmpdir(), "little-monkey-realtime-acceptance-"));
const reportPath = join(directory, "report.json");
const keep = process.env.LITTLE_MONKEY_KEEP_ACCEPTANCE_REPORT === "1";

console.log("Starting Little Monkey with the realtime acceptance harness enabled.");
console.log(`Speak one short request when prompted: ask it to read ${testPath}.`);

const child = spawn("pnpm", ["tauri", "dev"], {
  stdio: "inherit",
  env: {
    ...process.env,
    LITTLE_MONKEY_REALTIME_ACCEPTANCE: "1",
    LITTLE_MONKEY_REALTIME_ACCEPTANCE_REPORT: reportPath,
    VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE: "1",
    VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH: testPath,
  },
});

const timer = setTimeout(() => {
  console.error(`\nNo acceptance evidence after ${Math.round(timeoutMs / 1000)}s; stopping the app.`);
  child.kill("SIGTERM");
}, timeoutMs);

/**
 * Nothing inside the app can observe the output device, the OS mixer, or the
 * speaker, so the last link in "streamed spoken answer → speaker" is the
 * operator's own ears. It is asked for here rather than assumed, and a run
 * whose measurable steps all pass is still only UNVERIFIED until answered.
 */
async function askWhetherHeard() {
  if (process.env.LITTLE_MONKEY_REALTIME_ASSUME_HEARD === "1") return true;
  if (!process.stdin.isTTY) return null;
  const reader = createInterface({ input: process.stdin, output: process.stdout });
  try {
    const answer = await new Promise((resolve) =>
      reader.question("\nDid you hear the spoken answer through your speaker? [y/N] ", resolve));
    return /^y(es)?$/i.test(answer.trim());
  } finally {
    reader.close();
  }
}

async function finish(code) {
  clearTimeout(timer);
  if (!existsSync(reportPath)) {
    console.error("\nThe app exited without writing acceptance evidence.");
    if (!keep) rmSync(directory, { recursive: true, force: true });
    process.exit(code === 0 ? 1 : code ?? 1);
  }
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  console.log("\nRealtime voice acceptance");
  console.log(`  model            ${report.model} (${report.turnDetection})`);
  console.log(`  continuations    ${report.continuationsRequested}`);
  for (const step of report.steps) {
    console.log(`  ${step.status === "passed" ? "PASS" : "FAIL"}  ${step.id} — ${step.detail}`);
  }
  if (report.error) console.log(`  error            ${report.error}`);
  console.log(`  metrics          ${JSON.stringify(report.metrics)}`);

  const measurablePassed = report.status === "passed";
  const heard = measurablePassed ? await askWhetherHeard() : null;
  const speaker = heard === true ? "PASS" : heard === false ? "FAIL" : "UNVERIFIED";
  console.log(`  ${speaker}  physical_speaker — ${
    heard === true
      ? "the operator confirmed hearing the answer"
      : heard === false
        ? "the operator did not hear the answer, so the path past the audio element is broken"
        : "not asked or not answered; no measurement here can reach the output device"
  }`);
  console.log(keep ? `\nEvidence kept at ${reportPath}` : "");
  if (!keep) rmSync(directory, { recursive: true, force: true });
  if (!measurablePassed || heard === false) process.exit(1);
  // Exit 2 keeps "measured everything, nobody confirmed hearing it" distinct
  // from a pass, so the definition of done cannot be closed by a green exit
  // code alone.
  process.exit(heard === true ? 0 : 2);
}

child.on("error", (error) => {
  clearTimeout(timer);
  console.error(`Could not start the app: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code) => { void finish(code ?? 0); });

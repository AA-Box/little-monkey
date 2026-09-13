#!/usr/bin/env node

/**
 * LAYER 2 — live OpenAI acceptance for desktop realtime voice. OPTIONAL.
 *
 * This run answers the one question the credential-free layer cannot: whether
 * OpenAI's servers accept this exact session. Everything else it exercises —
 * the routing, the bridge, the ledger, the playback path — is already proven
 * without any provider account by `pnpm test:realtime:loopback`, so a machine
 * with no OpenAI key is not a machine with unproven Voice Everywhere. It is a
 * machine that cannot answer an external-service question, and this script
 * says which precondition is missing and stops, rather than reporting a
 * failure that reads like a defect in the feature.
 *
 * Runs the app itself — the same webview, the same WebRTC stack, the same
 * native keychain broker, the same tool executor — with the acceptance harness
 * enabled, then reads the evidence the native side writes. The operator speaks
 * one short request when the run says so, and answers two questions at the end
 * — whether the answer was audible, and whether it stopped the moment it was
 * interrupted. Those are the parts of the chain no measurement inside the app
 * can reach, and no flag can answer them on the operator's behalf.
 *
 * Exit codes: 0 every step passed and the operator confirmed both, 2 every
 * measurable step passed but nobody witnessed it, 3 a precondition for this
 * optional layer is missing so it never ran, 1 something failed.
 *
 *   pnpm test:realtime:live --path README.md
 *
 * Preconditions, all required and none of them assumed: an OpenAI key saved
 * through Settings, a workspace open on the file given by --path, network
 * access, microphone permission, and an operator at this terminal to answer
 * the two questions at the end.
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

// The two closing questions are the point of this run, so a terminal nobody is
// sitting at cannot satisfy it. Said up front rather than after several minutes
// of app and provider time, and with the layer that *is* automatable named.
if (!process.stdin.isTTY) {
  console.error("Realtime live acceptance needs an operator at an interactive terminal to confirm what was heard.");
  console.error("For the credential-free routing acceptance that runs unattended, use: pnpm test:realtime:loopback");
  process.exit(3);
}

const timeoutMs = Number(arg("--timeout-ms") ?? 420_000);
const directory = mkdtempSync(join(tmpdir(), "little-monkey-realtime-acceptance-"));
const reportPath = join(directory, "report.json");
const keep = process.env.LITTLE_MONKEY_KEEP_ACCEPTANCE_REPORT === "1";

console.log("Layer 2 — live OpenAI realtime acceptance (optional; layer 1 is pnpm test:realtime:loopback).");
console.log("Requires: an OpenAI key saved through Settings, network access, microphone permission,");
console.log("a workspace open on the file below, and you at this terminal at the end.");
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
 * speaker, so the last two links in "streamed spoken answer → speaker" and
 * "interruption → audible silence" are the operator's own ears. There is
 * deliberately no environment variable that can assert these: a confirmation a
 * script can set is not a confirmation, and the one thing this whole harness
 * exists to avoid is a green result nobody witnessed. Unanswered stays
 * UNVERIFIED, which is its own exit code.
 */
async function askOperator(questions) {
  if (!process.stdin.isTTY) return questions.map(() => null);
  const reader = createInterface({ input: process.stdin, output: process.stdout });
  const answers = [];
  try {
    for (const question of questions) {
      if (question.skip?.(answers)) {
        answers.push(null);
        continue;
      }
      const answer = await new Promise((resolve) => reader.question(`\n${question.prompt} [y/N] `, resolve));
      answers.push(/^y(es)?$/i.test(answer.trim()));
    }
  } finally {
    reader.close();
  }
  return answers;
}

const OPERATOR_QUESTIONS = [
  {
    id: "physical_speaker",
    prompt: "Did you hear the spoken answer through your speaker?",
    pass: "the operator confirmed hearing the answer",
    fail: "the operator did not hear the answer, so the path past the audio element is broken",
  },
  {
    id: "audible_interruption",
    prompt: "Did the answer stop immediately when it was interrupted?",
    pass: "the operator confirmed the answer stopped immediately",
    fail: "the operator heard the answer continue after the interruption",
    // Meaningless if nothing was audible in the first place.
    skip: (answers) => answers[0] !== true,
  },
];

async function finish(code) {
  clearTimeout(timer);
  if (!existsSync(reportPath)) {
    console.error("\nThe app exited without writing acceptance evidence.");
    if (!keep) rmSync(directory, { recursive: true, force: true });
    process.exit(code === 0 ? 1 : code ?? 1);
  }
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  // A missing credential is a precondition this optional layer did not meet,
  // not a defect in the feature. Reporting it as a plain failure is what made
  // "the realtime engine is untested" sound like a gap in Voice Everywhere
  // rather than an absent OpenAI account.
  const configured = report.steps.find((step) => step.id === "provider_configured");
  if (configured?.status === "failed" && /keychain boundary/.test(report.error ?? "")) {
    console.error("\nLayer 2 did not run: no OpenAI key is available through the native keychain boundary.");
    console.error("Save one in Settings, or run the credential-free layer 1: pnpm test:realtime:loopback");
    if (!keep) rmSync(directory, { recursive: true, force: true });
    process.exit(3);
  }
  console.log("\nRealtime voice acceptance — layer 2, live OpenAI");
  console.log(`  provider         ${report.providerId}`);
  console.log(`  model            ${report.model} (${report.turnDetection})`);
  console.log(`  continuations    ${report.continuationsRequested}`);
  for (const step of report.steps) {
    console.log(`  ${step.status === "passed" ? "PASS" : "FAIL"}  ${step.id} — ${step.detail}`);
  }
  if (report.error) console.log(`  error            ${report.error}`);
  console.log(`  metrics          ${JSON.stringify(report.metrics)}`);

  const measurablePassed = report.status === "passed";
  const answers = measurablePassed
    ? await askOperator(OPERATOR_QUESTIONS)
    : OPERATOR_QUESTIONS.map(() => null);
  OPERATOR_QUESTIONS.forEach((question, index) => {
    const answer = answers[index];
    const verdict = answer === true ? "PASS" : answer === false ? "FAIL" : "UNVERIFIED";
    const detail = answer === true
      ? question.pass
      : answer === false
        ? question.fail
        : "not asked or not answered; no measurement here can reach the operator's ears";
    console.log(`  ${verdict}  ${question.id} — ${detail}`);
  });
  console.log(keep ? `\nEvidence kept at ${reportPath}` : "");
  if (!keep) rmSync(directory, { recursive: true, force: true });
  if (!measurablePassed || answers.some((answer) => answer === false)) process.exit(1);
  // Exit 2 keeps "measured everything, nobody witnessed it" distinct from a
  // pass, so the definition of done cannot be closed by a green exit code.
  process.exit(answers.every((answer) => answer === true) ? 0 : 2);
}

child.on("error", (error) => {
  clearTimeout(timer);
  console.error(`Could not start the app: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code) => { void finish(code ?? 0); });

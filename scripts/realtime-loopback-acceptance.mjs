#!/usr/bin/env node

/**
 * LAYER 1 — Voice Everywhere acceptance with no provider credentials.
 *
 * Runs the app itself with the acceptance harness pointed at the loopback
 * realtime provider: a real RTCPeerConnection, a real data channel, a real
 * audio track and a real SDP exchange, with the production session, routing,
 * tool bridge and session store throughout. The only thing that is not
 * production is the far end — instead of OpenAI's servers, a peer on this
 * computer that speaks the same event dialect. So this proves the routing,
 * which is the feature; it does not prove anything about a model, and every
 * step whose sentence would have claimed otherwise says so in its own evidence
 * line rather than passing quietly.
 *
 * No OpenAI key, no network, no keychain broker, and nobody has to be sitting
 * here: the local peer answers the audio it is given rather than waiting for a
 * person to speak.
 *
 *   pnpm test:realtime:loopback --path README.md
 *
 * Exit codes: 0 every step passed, 1 something failed.
 *
 * What this layer deliberately does NOT cover: that OpenAI's servers accept
 * this exact session (LAYER 2 — `pnpm test:realtime:live`), and that a person
 * heard the answer come out of a speaker and heard it stop when interrupted
 * (the two operator questions the live run asks, which no measurement can
 * answer on the operator's behalf).
 */

import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

function arg(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

const testPath = arg("--path") ?? process.env.LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH ?? "README.md";
const timeoutMs = Number(arg("--timeout-ms") ?? 420_000);
const directory = mkdtempSync(join(tmpdir(), "little-monkey-realtime-loopback-"));
const reportPath = join(directory, "report.json");
const keep = process.env.LITTLE_MONKEY_KEEP_ACCEPTANCE_REPORT === "1";

console.log("Layer 1 — Voice Everywhere routing against a local realtime peer.");
console.log("No OpenAI key, no network to a provider, no keychain broker, no operator input.");
console.log(`The run asks the peer to read ${testPath}, which must be readable in the open workspace.`);

const child = spawn("pnpm", ["tauri", "dev"], {
  stdio: "inherit",
  env: {
    ...process.env,
    LITTLE_MONKEY_REALTIME_ACCEPTANCE: "1",
    LITTLE_MONKEY_REALTIME_ACCEPTANCE_REPORT: reportPath,
    VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE: "1",
    VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE_PATH: testPath,
    VITE_LITTLE_MONKEY_REALTIME_ACCEPTANCE_PROVIDER: "loopback",
  },
});

const timer = setTimeout(() => {
  console.error(`\nNo acceptance evidence after ${Math.round(timeoutMs / 1000)}s; stopping the app.`);
  child.kill("SIGTERM");
}, timeoutMs);

function finish(code) {
  clearTimeout(timer);
  if (!existsSync(reportPath)) {
    console.error("\nThe app exited without writing acceptance evidence.");
    if (!keep) rmSync(directory, { recursive: true, force: true });
    process.exit(code === 0 ? 1 : code ?? 1);
  }
  const report = JSON.parse(readFileSync(reportPath, "utf8"));
  console.log("\nRealtime voice acceptance — layer 1, local peer");
  console.log(`  provider         ${report.providerId}`);
  console.log(`  model            ${report.model} (${report.turnDetection})`);
  console.log(`  continuations    ${report.continuationsRequested}`);
  for (const step of report.steps) {
    console.log(`  ${step.status === "passed" ? "PASS" : "FAIL"}  ${step.id} — ${step.detail}`);
  }
  if (report.error) console.log(`  error            ${report.error}`);
  console.log(`  metrics          ${JSON.stringify(report.metrics)}`);
  console.log("\n  NOT COVERED HERE  that OpenAI's servers accept this session — pnpm test:realtime:live");
  console.log("  NOT COVERED HERE  that a person heard the answer, and heard it stop — the live run asks");

  // A run that quietly fell back to the credentialed provider would otherwise
  // report a green layer 2 under a layer 1 heading, which is precisely the
  // confusion this split exists to end.
  if (report.providerId !== "loopback") {
    console.error(`\nThis run reported provider ${report.providerId}; the loopback provider was never used.`);
    if (!keep) rmSync(directory, { recursive: true, force: true });
    process.exit(1);
  }
  console.log(keep ? `\nEvidence kept at ${reportPath}` : "");
  if (!keep) rmSync(directory, { recursive: true, force: true });
  process.exit(report.status === "passed" ? 0 : 1);
}

child.on("error", (error) => {
  clearTimeout(timer);
  console.error(`Could not start the app: ${error.message}`);
  process.exit(1);
});
child.on("exit", (code) => { finish(code ?? 0); });

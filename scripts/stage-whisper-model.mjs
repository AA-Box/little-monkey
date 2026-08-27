#!/usr/bin/env node
// Downloads the pinned multilingual Whisper model the built-in local speech
// backend runs, verifies its SHA-256, and stages it as a bundled resource.
//
// Bundling rather than downloading on first launch is what makes the backend
// honestly zero-config: the model is present the moment the app is installed,
// so a first run with no network — or behind a proxy that refuses huggingface
// .co — still transcribes. src-tauri/src/local_whisper.rs keeps the download
// as a fallback for development trees where this script has not run.
//
// Keep MODEL_URL, MODEL_SHA256 and MODEL_FILE in step with the constants of
// the same name in src-tauri/src/local_whisper.rs.
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const MODEL_FILE = "ggml-base-q5_1.bin";
const MODEL_URL =
  "https://huggingface.co/ggerganov/whisper.cpp/resolve/f281eb45af861ab5e5297d23694b7d46e090c02c/ggml-base-q5_1.bin";
const MODEL_SHA256 =
  "422f1ae452ade6f30a004d7e5c6a43195e4433bc370bf23fac9cc591f01a8898";
const MODEL_BYTES = 59_707_625;

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const destinationDir = join(root, "src-tauri", "resources", "local-whisper");
const destination = join(destinationDir, MODEL_FILE);

const digestOf = (bytes) => createHash("sha256").update(bytes).digest("hex");

// Already staged and intact: re-running the build must not re-download 57MB.
// One read, deliberately: sizing the file and then opening it separately is a
// race, and it answers a weaker question than the bytes themselves do.
try {
  const staged = readFileSync(destination);
  if (staged.byteLength === MODEL_BYTES && digestOf(staged) === MODEL_SHA256) {
    console.log(`[stage-whisper-model] already staged ${destination}`);
    process.exit(0);
  }
} catch {
  // Not staged yet, which is the normal path on a clean checkout.
}

console.log(`[stage-whisper-model] downloading ${MODEL_URL}`);
const response = await fetch(MODEL_URL, { redirect: "follow" });
if (!response.ok) {
  throw new Error(`model download returned ${response.status}`);
}
const bytes = Buffer.from(await response.arrayBuffer());

if (bytes.byteLength !== MODEL_BYTES) {
  throw new Error(
    `model is ${bytes.byteLength} bytes, expected ${MODEL_BYTES}`,
  );
}
const digest = digestOf(bytes);
if (digest !== MODEL_SHA256) {
  throw new Error(`model checksum mismatch: expected ${MODEL_SHA256}, got ${digest}`);
}

// Write beside the destination and rename, so an interrupted run never leaves a
// half-written model that later looks staged.
mkdirSync(destinationDir, { recursive: true });
const staging = `${destination}.staging`;
try {
  // codeql[js/http-to-file-access]: these bytes are pinned by MODEL_SHA256 and
  // verified above, in memory, before anything reaches the filesystem — a
  // mismatch throws and nothing is written. Downloading a pinned artefact to
  // disk is the purpose of this script; the digest is what makes it safe, and
  // scripts/stage-managed-runtime.mjs stages every other runtime the same way.
  writeFileSync(staging, bytes);
  renameSync(staging, destination);
} catch (error) {
  rmSync(staging, { force: true });
  throw error;
}
console.log(
  `[stage-whisper-model] staged ${destination} (${bytes.byteLength} bytes, sha256 ${digest})`,
);

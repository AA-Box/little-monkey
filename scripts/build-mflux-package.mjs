#!/usr/bin/env node

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { buildManifest, canonicalJson, signManifest } from "./lib/mlxPackage.mjs";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SOURCE = join(ROOT, "packaging/mflux/service/mflux_image_server.py");
const OUTPUT = join(ROOT, "packaging/mflux/dist");
const VERSION = process.env.MFLUX_VERSION ?? "0.18.0";
const KEY_ID = "release-2026-1";
const SOURCE_ID = "little-monkey-mflux-image";
const COMPONENT_ID = "mflux-image-runtime-apple-silicon";

function build() {
  if (process.platform !== "darwin" || process.arch !== "arm64") {
    throw new Error(`MFLUX Image Runtime packages are macOS arm64 only; this is ${process.platform}/${process.arch}`);
  }
  rmSync(OUTPUT, { recursive: true, force: true });
  mkdirSync(OUTPUT, { recursive: true });
  execFileSync("python3", ["-m", "venv", "--copies", join(OUTPUT, "runtime")], { stdio: "inherit" });
  const python = join(OUTPUT, "runtime/bin/python3");
  execFileSync(python, ["-m", "pip", "install", "--quiet", "--upgrade", `mflux==${VERSION}`], {
    stdio: "inherit",
  });
  pruneBytecode(OUTPUT);
  mkdirSync(join(OUTPUT, "service"), { recursive: true });
  cpSync(SOURCE, join(OUTPUT, "service/mflux_image_server.py"));
  const pythonExecutable = "runtime/bin/python3";
  const version = `mflux-${VERSION}+${pythonVersion(python)}`;
  let manifest = buildManifest({
    root: OUTPUT,
    packageVersion: version,
    pythonExecutable,
    serviceEntry: "service/mflux_image_server.py",
    keyId: KEY_ID,
  });
  if (process.env.MFLUX_SIGNING_KEY) {
    manifest = signManifest(manifest, process.env.MFLUX_SIGNING_KEY);
  } else {
    manifest = { ...manifest, signatureAlgorithm: "none", signatureBase64: "" };
    console.warn("MFLUX_SIGNING_KEY is unset — wrote an unsigned manifest.");
  }
  writeFileSync(join(OUTPUT, "mlx-package.json"), canonicalJson(manifest));
  publish(version, manifest);
}

function publish(version, manifest) {
  const archive = join(ROOT, "packaging/mflux", `mflux-image-runtime-${version}.tar.gz`);
  const entries = readdirSync(OUTPUT).sort();
  execFileSync("tar", ["-czf", archive, "-C", OUTPUT, ...entries], {
    stdio: "inherit",
    env: { ...process.env, COPYFILE_DISABLE: "1" },
  });
  const bytes = readFileSync(archive);
  const entry = {
    schemaVersion: 1,
    sourceId: SOURCE_ID,
    componentId: COMPONENT_ID,
    kind: "mflux_image_runtime",
    displayName: "MFLUX Image Runtime (Apple silicon)",
    accelerator: null,
    version,
    channel: "stable",
    downloadUrl: process.env.MFLUX_DOWNLOAD_URL ?? `file://${archive}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    sizeBytes: bytes.length,
    publishedAtMs: Number(process.env.SOURCE_DATE_EPOCH ?? 0) * 1000,
    compatibilityNote: `Requires Apple silicon. Ships ${manifest.files.length} verified files.`,
    metadata: { mfluxVersion: VERSION },
  };
  writeFileSync(join(ROOT, "packaging/mflux/mflux-catalog.json"), `${JSON.stringify([entry], null, 2)}\n`);
  console.log(`archive: ${archive} (${(bytes.length / 1e6).toFixed(0)} MB)`);
}

function pruneBytecode(directory) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const absolute = join(directory, entry.name);
    if (entry.isDirectory()) {
      if (entry.name === "__pycache__") rmSync(absolute, { recursive: true, force: true });
      else pruneBytecode(absolute);
    } else if (entry.name.endsWith(".pyc")) {
      rmSync(absolute, { force: true });
    }
  }
}

function pythonVersion(interpreter) {
  return execFileSync(interpreter, ["-c", "import sys;print('py%d.%d' % sys.version_info[:2])"])
    .toString()
    .trim();
}

build();

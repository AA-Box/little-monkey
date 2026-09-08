#!/usr/bin/env node
/**
 * Builds — and, given a key, signs — the MLX service package the app installs.
 *
 *   pnpm mlx:keygen
 *   pnpm mlx:package
 *   MLX_SIGNING_KEY=... pnpm mlx:package
 *
 * The package is self-contained and signed. In addition to the pinned MLX
 * Python stack it carries a Lily binary built from one immutable upstream
 * commit. `service/runtime_router.py` is the only serviceEntry: it selects Lily
 * conservatively on supported M5+/macOS 26+ Qwen3.6 hosts and otherwise execs
 * the normal MLX service. No user PATH executable is trusted at runtime.
 */

import { execFileSync } from "node:child_process";
import { createHash, generateKeyPairSync } from "node:crypto";
import {
  chmodSync,
  cpSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { buildManifest, canonicalJson, serviceRevision, signManifest } from "./lib/mlxPackage.mjs";

const REPOSITORY_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SOURCE_SERVICE = join(REPOSITORY_ROOT, "packaging/mlx/service/mlx_server.py");
const SOURCE_VIDEO_SERVICE = join(REPOSITORY_ROOT, "packaging/mlx/service/mlx_video_server.py");
const SOURCE_ROUTER = join(REPOSITORY_ROOT, "packaging/mlx/service/runtime_router.py");
const SOURCE_LILY_MANAGED = join(REPOSITORY_ROOT, "packaging/mlx/service/lily_managed.py");
const OUTPUT_ROOT = join(REPOSITORY_ROOT, "packaging/mlx/dist");

/** Must match MLX_RELEASE_KEY_ID in src-tauri/src/m3_production.rs. */
const KEY_ID = "release-2026-1";
const MLX_VERSION = "0.32.2";
const MLX_LM_VERSION = "0.31.3";
const MLX_VLM_VERSION = "0.6.17";
const MLX_VIDEO_COMMIT = "87db56a51758fefb748a359b90a5283bb8ba4837";
/**
 * Audited Lily source revision. Do not use a branch/tag here: a managed
 * executable must be reproducibly attributable to the source we reviewed.
 */
const LILY_GARDEN_COMMIT = "1ed972ed3f0bd5616c997c9507c25616c63394fc";
const LILY_REPOSITORY = "https://github.com/perplexityai/pplx-garden.git";
const SOURCE_ID = "little-monkey-mlx";
const COMPONENT_ID = "mlx-runtime-apple-silicon";
const ARCHIVE_PREFIX = "mlx-runtime";

function keygen() {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const raw = publicKey.export({ type: "spki", format: "der" }).subarray(-32);
  process.stdout.write(
    [
      "# Private key — store as a CI secret, never commit it:",
      privateKey.export({ type: "pkcs8", format: "pem" }).trim(),
      "",
      "# Public key hex — paste into MLX_RELEASE_PUBLIC_KEY_HEX",
      "# (src-tauri/src/m3_production.rs) and keep MLX_RELEASE_KEY_ID in step:",
      raw.toString("hex"),
      "",
    ].join("\n"),
  );
}

function buildLily() {
  const checkout = mkdtempSync(join(tmpdir(), "little-monkey-lily-"));
  try {
    execFileSync("git", ["init", "--quiet", checkout], { stdio: "inherit" });
    execFileSync("git", ["-C", checkout, "remote", "add", "origin", LILY_REPOSITORY], {
      stdio: "inherit",
    });
    execFileSync(
      "git",
      ["-C", checkout, "fetch", "--quiet", "--depth=1", "origin", LILY_GARDEN_COMMIT],
      { stdio: "inherit" },
    );
    const fetched = execFileSync("git", ["-C", checkout, "rev-parse", "FETCH_HEAD"])
      .toString()
      .trim();
    if (fetched !== LILY_GARDEN_COMMIT) {
      throw new Error(`Lily source identity mismatch: wanted ${LILY_GARDEN_COMMIT}, got ${fetched}`);
    }
    execFileSync("git", ["-C", checkout, "checkout", "--quiet", "--detach", "FETCH_HEAD"], {
      stdio: "inherit",
    });

    const lilyRoot = join(checkout, "lily");
    // Lily pins Rust 1.92 in its own rust-toolchain.toml. Build from its own
    // directory so rustup honors that file and Cargo.lock is mandatory.
    execFileSync("cargo", ["build", "--release", "--locked"], {
      cwd: lilyRoot,
      stdio: "inherit",
    });
    const built = join(lilyRoot, "target/release/lily");
    const destination = join(OUTPUT_ROOT, "bin/lily");
    mkdirSync(dirname(destination), { recursive: true });
    cpSync(built, destination);
    chmodSync(destination, 0o755);

    mkdirSync(join(OUTPUT_ROOT, "licenses/lily"), { recursive: true });
    cpSync(join(lilyRoot, "LICENSE"), join(OUTPUT_ROOT, "licenses/lily/LICENSE"));
    cpSync(join(lilyRoot, "NOTICE"), join(OUTPUT_ROOT, "licenses/lily/NOTICE"));
  } finally {
    rmSync(checkout, { recursive: true, force: true });
  }
}

function build() {
  if (process.platform !== "darwin" || process.arch !== "arm64") {
    throw new Error(`MLX packages are macOS arm64 only; this is ${process.platform}/${process.arch}`);
  }
  rmSync(OUTPUT_ROOT, { recursive: true, force: true });
  mkdirSync(OUTPUT_ROOT, { recursive: true });

  console.log("creating the packaged interpreter…");
  execFileSync("python3", ["-m", "venv", "--copies", join(OUTPUT_ROOT, "runtime")], {
    stdio: "inherit",
  });
  const python = join(OUTPUT_ROOT, "runtime/bin/python3");
  execFileSync(
    python,
    [
      "-m",
      "pip",
      "install",
      "--quiet",
      "--upgrade",
      `mlx==${MLX_VERSION}`,
      `mlx-lm==${MLX_LM_VERSION}`,
      `mlx-vlm==${MLX_VLM_VERSION}`,
    ],
    { stdio: "inherit" },
  );

  console.log("adding the video engine…");
  execFileSync(
    python,
    [
      "-m",
      "pip",
      "install",
      "--quiet",
      `mlx-video @ git+https://github.com/Blaizzy/mlx-video.git@${MLX_VIDEO_COMMIT}`,
    ],
    { stdio: "inherit" },
  );
  execFileSync(python, ["-m", "pip", "check"], { stdio: "inherit" });
  execFileSync(
    python,
    [
      "-c",
      [
        "import importlib.metadata as m",
        `assert m.version('mlx') == '${MLX_VERSION}', m.version('mlx')`,
        `assert m.version('mlx-lm') == '${MLX_LM_VERSION}', m.version('mlx-lm')`,
        `assert m.version('mlx-vlm') == '${MLX_VLM_VERSION}', m.version('mlx-vlm')`,
      ].join(";"),
    ],
    { stdio: "inherit" },
  );

  console.log(`building Lily at ${LILY_GARDEN_COMMIT.slice(0, 12)}…`);
  buildLily();
  pruneBytecode(OUTPUT_ROOT);

  mkdirSync(join(OUTPUT_ROOT, "service"), { recursive: true });
  cpSync(SOURCE_SERVICE, join(OUTPUT_ROOT, "service/mlx_server.py"));
  cpSync(SOURCE_VIDEO_SERVICE, join(OUTPUT_ROOT, "service/mlx_video_server.py"));
  cpSync(SOURCE_ROUTER, join(OUTPUT_ROOT, "service/runtime_router.py"));
  cpSync(SOURCE_LILY_MANAGED, join(OUTPUT_ROOT, "service/lily_managed.py"));

  const pythonExecutable = "runtime/bin/python3";
  const sources = [SOURCE_SERVICE, SOURCE_VIDEO_SERVICE, SOURCE_ROUTER, SOURCE_LILY_MANAGED];
  const version =
    `mlx-${MLX_VERSION}+mlx-lm-${MLX_LM_VERSION}+mlx-vlm-${MLX_VLM_VERSION}` +
    `+video-${MLX_VIDEO_COMMIT.slice(0, 12)}+lily-${LILY_GARDEN_COMMIT.slice(0, 12)}` +
    `+${pythonVersion(join(OUTPUT_ROOT, pythonExecutable))}+svc-${serviceRevision(sources)}`;
  let manifest = buildManifest({
    root: OUTPUT_ROOT,
    packageVersion: version,
    pythonExecutable,
    serviceEntry: "service/runtime_router.py",
    keyId: KEY_ID,
  });

  const signingKey = process.env.MLX_SIGNING_KEY;
  if (signingKey) {
    manifest = signManifest(manifest, signingKey);
    console.log(`signed ${manifest.files.length} files as ${version}`);
  } else {
    manifest = { ...manifest, signatureAlgorithm: "none", signatureBase64: "" };
    console.warn(
      "MLX_SIGNING_KEY is unset — wrote an UNSIGNED manifest. The app will refuse to install it.",
    );
  }
  writeFileSync(join(OUTPUT_ROOT, "mlx-package.json"), canonicalJson(manifest));
  console.log(`package ready: ${OUTPUT_ROOT}`);
  publish(version, manifest);
}

function publish(version, manifest) {
  const archive = join(REPOSITORY_ROOT, "packaging/mlx", `${ARCHIVE_PREFIX}-${version}.tar.gz`);
  const entries = readdirSync(OUTPUT_ROOT).sort();
  execFileSync("tar", ["-czf", archive, "-C", OUTPUT_ROOT, ...entries], {
    stdio: "inherit",
    env: { ...process.env, COPYFILE_DISABLE: "1" },
  });

  const bytes = readFileSync(archive);
  const entry = {
    schemaVersion: 1,
    sourceId: SOURCE_ID,
    componentId: COMPONENT_ID,
    kind: "mlx_runtime",
    displayName: "MLX runtime (Apple silicon)",
    accelerator: "Lily on M5+ / macOS 26+ for Qwen3.6-35B-A3B",
    version,
    channel: "stable",
    downloadUrl: process.env.MLX_DOWNLOAD_URL ?? `file://${archive}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    sizeBytes: bytes.length,
    publishedAtMs: Number(process.env.SOURCE_DATE_EPOCH ?? 0) * 1000,
    compatibilityNote:
      `Requires Apple silicon. Carries MLX ${MLX_VERSION}, mlx-lm ${MLX_LM_VERSION}, ` +
      `mlx-vlm ${MLX_VLM_VERSION}, the pinned MLX video engine, and Lily ${LILY_GARDEN_COMMIT.slice(0, 12)}. ` +
      `Lily acceleration is selected only on M5+/macOS 26+ with its exact Qwen3.6 affine-Q4 model; ` +
      `all other models and unsupported Lily request surfaces use the normal MLX engine. ` +
      `Ships ${manifest.files.length} files.`,
    metadata: {
      mlxVersion: MLX_VERSION,
      mlxLmVersion: MLX_LM_VERSION,
      mlxVlmVersion: MLX_VLM_VERSION,
      mlxVideoCommit: MLX_VIDEO_COMMIT,
      lilyGardenCommit: LILY_GARDEN_COMMIT,
      lilyModel: "Qwen3.6-35B-A3B",
      lilyMinimumMacos: "26",
      lilyMinimumAppleMGeneration: 5,
    },
  };
  const catalog = join(REPOSITORY_ROOT, "packaging/mlx", "mlx-catalog.json");
  writeFileSync(catalog, `${JSON.stringify([entry], null, 2)}\n`);
  console.log(`archive: ${archive} (${(bytes.length / 1e6).toFixed(0)} MB)`);
  console.log(`catalog: ${catalog}`);
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

if (process.argv[2] === "keygen") keygen();
else build();

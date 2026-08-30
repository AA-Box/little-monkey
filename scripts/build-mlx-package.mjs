#!/usr/bin/env node
/**
 * Builds — and, given a key, signs — the MLX service package the app installs.
 *
 *   pnpm mlx:keygen                    # once: print a publisher keypair
 *   pnpm mlx:package                   # build the tree, leave it unsigned
 *   MLX_SIGNING_KEY=... pnpm mlx:package   # build and sign
 *
 * The private key is never read from a file in this repository and is never
 * written to one. It arrives in `MLX_SIGNING_KEY` as a PKCS#8 PEM (the same
 * shape as TAURI_SIGNING_PRIVATE_KEY, which release.yml already supplies from
 * a CI secret) because signing a package is authorization to place executable
 * code inside the app's private runtime directory.
 *
 * Without the key the build still runs and writes an unsigned manifest. That
 * is deliberate: it lets the tree, the digests, and the Python service be
 * exercised locally, and the installer refuses the result — `signatureAlgorithm:
 * "none"` is rejected outright by validate_manifest — so an unsigned package
 * can never be mistaken for an installable one. This mirrors
 * codesign-managed-runtime.mjs, which likewise no-ops without its identity.
 */

import { execFileSync } from "node:child_process";
import { createHash, generateKeyPairSync } from "node:crypto";
import { cpSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { buildManifest, canonicalJson, serviceRevision, signManifest } from "./lib/mlxPackage.mjs";

const REPOSITORY_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const SOURCE_SERVICE = join(REPOSITORY_ROOT, "packaging/mlx/service/mlx_server.py");
const SOURCE_VIDEO_SERVICE = join(REPOSITORY_ROOT, "packaging/mlx/service/mlx_video_server.py");
const OUTPUT_ROOT = join(REPOSITORY_ROOT, "packaging/mlx/dist");

/** Must match MLX_RELEASE_KEY_ID in src-tauri/src/m3_production.rs. */
const KEY_ID = "release-2026-1";
/** Pinned so a package states exactly which MLX it carries. */
const MLX_LM_VERSION = "0.28.4";
/**
 * The video engine, pinned to a commit rather than a version.
 *
 * mlx-video publishes no releases and no tags, its `__version__` has read
 * "0.0.1" since the repository was created, and the name `mlx-video` on PyPI
 * belongs to an unrelated project — so a version range would be meaningless and
 * a bare `pip install mlx-video` would install the wrong software. A commit is
 * the only thing that identifies what this package actually carries.
 */
const MLX_VIDEO_COMMIT = "87db56a51758fefb748a359b90a5283bb8ba4837";
/** Catalog identity. `componentId` is the stable key the component hub
 *  versions against, so it must not carry the version. */
const SOURCE_ID = "little-monkey-mlx";
const COMPONENT_ID = "mlx-runtime-apple-silicon";
const ARCHIVE_PREFIX = "mlx-runtime";

function keygen() {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  // The raw 32 bytes are the tail of the DER SubjectPublicKeyInfo; that hex is
  // what MLX_RELEASE_PUBLIC_KEY_HEX holds.
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

function build() {
  if (process.platform !== "darwin" || process.arch !== "arm64") {
    // The manifest hard-pins macos/aarch64 and the installer re-checks it
    // against the probed host, so a package built anywhere else is a package
    // nothing can install.
    throw new Error(`MLX packages are macOS arm64 only; this is ${process.platform}/${process.arch}`);
  }
  rmSync(OUTPUT_ROOT, { recursive: true, force: true });
  mkdirSync(OUTPUT_ROOT, { recursive: true });

  // A self-contained interpreter, not the user's: the manifest names the
  // interpreter it was signed with, and the installer execs that exact file.
  console.log("creating the packaged interpreter…");
  execFileSync("python3", ["-m", "venv", "--copies", join(OUTPUT_ROOT, "runtime")], {
    stdio: "inherit",
  });
  execFileSync(
    join(OUTPUT_ROOT, "runtime/bin/python3"),
    ["-m", "pip", "install", "--quiet", "--upgrade", `mlx-lm==${MLX_LM_VERSION}`],
    { stdio: "inherit" },
  );
  // Installed into the same interpreter rather than a second one: both services
  // are launched from this venv, and mlx-lm and mlx-video agree on mlx itself.
  console.log("adding the video engine…");
  execFileSync(
    join(OUTPUT_ROOT, "runtime/bin/python3"),
    [
      "-m",
      "pip",
      "install",
      "--quiet",
      `mlx-video @ git+https://github.com/Blaizzy/mlx-video.git@${MLX_VIDEO_COMMIT}`,
    ],
    { stdio: "inherit" },
  );

  // Compiled bytecode is regenerable cache, not payload: it is nearly half the
  // files a fresh venv contains, every one of which would otherwise be
  // digested, signed, and re-verified on each launch for no benefit.
  pruneBytecode(OUTPUT_ROOT);

  mkdirSync(join(OUTPUT_ROOT, "service"), { recursive: true });
  cpSync(SOURCE_SERVICE, join(OUTPUT_ROOT, "service/mlx_server.py"));
  // The second service. It is not the manifest's `serviceEntry` — that names
  // the one the M3 runtime adapter launches — but it is covered by the same
  // digests, so Studio can only ever run a file this package signed for.
  cpSync(SOURCE_VIDEO_SERVICE, join(OUTPUT_ROOT, "service/mlx_video_server.py"));

  const pythonExecutable = "runtime/bin/python3";
  // `svc-` is what makes a service-only fix a new version rather than a
  // same-named rebuild nothing upgrades to — see `serviceRevision`.
  const version = `mlx-lm-${MLX_LM_VERSION}+video-${MLX_VIDEO_COMMIT.slice(0, 12)}+${pythonVersion(
    join(OUTPUT_ROOT, pythonExecutable),
  )}+svc-${serviceRevision([SOURCE_SERVICE, SOURCE_VIDEO_SERVICE])}`;
  let manifest = buildManifest({
    root: OUTPUT_ROOT,
    packageVersion: version,
    pythonExecutable,
    serviceEntry: "service/mlx_server.py",
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

/**
 * Packs the tree for distribution and writes the catalog entry that points at
 * it.
 *
 * The archive is what a feed serves and the component hub downloads; the entry
 * is what a catalog source lists. Both are emitted here so the digest in the
 * entry is, by construction, the digest of the archive beside it — the one
 * pair that must never be assembled by hand.
 */
function publish(version, manifest) {
  const archive = join(REPOSITORY_ROOT, "packaging/mlx", `${ARCHIVE_PREFIX}-${version}.tar.gz`);
  // Top-level names are listed explicitly rather than packing `.`, because BSD
  // tar writes `./runtime/...` for the latter and the extractor rejects a `.`
  // path segment. COPYFILE_DISABLE stops macOS adding `._` AppleDouble
  // entries, which are not in the manifest and so would be dropped anyway —
  // but only after being downloaded.
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
    accelerator: null,
    version,
    channel: "stable",
    // Rewritten by the release workflow once the asset has a real URL. Left
    // as the file name locally so the entry is still valid JSON to inspect.
    downloadUrl: process.env.MLX_DOWNLOAD_URL ?? `file://${archive}`,
    sha256: createHash("sha256").update(bytes).digest("hex"),
    sizeBytes: bytes.length,
    publishedAtMs: Number(process.env.SOURCE_DATE_EPOCH ?? 0) * 1000,
    compatibilityNote:
      `Requires Apple silicon. Carries the MLX chat runtime and the MLX video ` +
      `engine. Ships ${manifest.files.length} files.`,
    metadata: {},
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

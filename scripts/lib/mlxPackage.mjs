/**
 * Builds and signs the MLX service package the app installs.
 *
 * The format is not ours to choose: `MlxPackageManifest` in
 * `src-tauri/src/mlx_runtime.rs` parses this with `deny_unknown_fields`, and
 * the installer re-derives every digest before it publishes a byte. The two
 * rules that are easy to get wrong and impossible to debug from the error
 * message are encoded here rather than left to the caller:
 *
 *   1. `files` must be sorted ascending by path, byte order, no duplicates.
 *   2. the signature covers the canonical JSON of the manifest *without* the
 *      `signatureBase64` key — not with it emptied, without it — with every
 *      object's keys sorted recursively and no whitespace.
 *
 * `canonicalJson` mirrors `canonical_json`/`canonicalize` in mlx_runtime.rs.
 * Both sides are pinned to the same fixture bytes by tests, in mlxPackage.test.mjs
 * here and in the `canonical_manifest_bytes_match_the_packaging_script` Rust
 * test, so a change to either canonicalizer fails rather than silently
 * producing packages that no longer verify.
 */

import { createHash, sign as cryptoSign, createPrivateKey } from "node:crypto";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";

/** Schema the Rust side accepts. Bump only alongside MLX_PACKAGE_SCHEMA_VERSION. */
export const MLX_PACKAGE_SCHEMA_VERSION = 1;

/** Recursively sorts object keys and serializes compactly, matching serde. */
export function canonicalJson(value) {
  return JSON.stringify(sortKeys(value));
}

function sortKeys(value) {
  if (Array.isArray(value)) return value.map(sortKeys);
  if (value === null || typeof value !== "object") return value;
  const sorted = {};
  for (const key of Object.keys(value).sort()) sorted[key] = sortKeys(value[key]);
  return sorted;
}

/** Every regular file under `root`, as manifest-relative POSIX paths. */
function walk(root, directory = root, found = []) {
  for (const entry of readdirSync(directory, { withFileTypes: true })) {
    const absolute = join(directory, entry.name);
    // Symlinks are skipped rather than followed: the installer refuses a tree
    // it cannot digest as regular files, and following one out of the package
    // would sign bytes that are not in it.
    if (entry.isDirectory()) walk(root, absolute, found);
    else if (entry.isFile()) found.push(relative(root, absolute).split(sep).join("/"));
  }
  return found;
}

/**
 * Describes a built package tree as an unsigned manifest.
 *
 * `pythonExecutable` and `serviceEntry` are relative paths inside `root`. The
 * interpreter is marked executable because the installer rejects a manifest
 * whose named interpreter is not — it is about to exec that exact file.
 */
export function buildManifest({ root, packageVersion, pythonExecutable, serviceEntry, keyId }) {
  const paths = walk(root).sort();
  if (paths.length === 0) throw new Error(`${root} contains no files to package`);
  for (const required of [pythonExecutable, serviceEntry]) {
    if (!paths.includes(required)) {
      throw new Error(`${required} is named in the manifest but missing from ${root}`);
    }
  }
  const files = paths.map((path) => {
    const bytes = readFileSync(join(root, path));
    return {
      path,
      sizeBytes: bytes.length,
      sha256: createHash("sha256").update(bytes).digest("hex"),
      // The interpreter must be executable; everything else is data the
      // service reads, and shipping it non-executable keeps the installed tree
      // to one runnable file.
      executable: path === pythonExecutable || (statSync(join(root, path)).mode & 0o111) !== 0,
    };
  });
  return {
    schemaVersion: MLX_PACKAGE_SCHEMA_VERSION,
    packageVersion,
    targetOs: "macos",
    targetArchitecture: "aarch64",
    pythonExecutable,
    serviceEntry,
    files,
    signatureAlgorithm: "ed25519",
    signatureKeyId: keyId,
  };
}

/** The exact bytes an Ed25519 signature must cover. */
export function signedPayload(manifest) {
  const { signatureBase64: _dropped, ...unsigned } = manifest;
  return Buffer.from(canonicalJson(unsigned), "utf8");
}

/**
 * Signs a manifest, returning it with `signatureBase64` filled in.
 *
 * `privateKeyPem` is read from the environment by the caller and never written
 * to the repository — the publisher key is the whole authorization to put
 * executable code into the app's private runtime directory.
 */
export function signManifest(manifest, privateKeyPem) {
  const key = createPrivateKey(privateKeyPem);
  if (key.asymmetricKeyType !== "ed25519") {
    throw new Error(`signing key is ${key.asymmetricKeyType}, expected ed25519`);
  }
  const signature = cryptoSign(null, signedPayload(manifest), key);
  return { ...manifest, signatureBase64: signature.toString("base64") };
}

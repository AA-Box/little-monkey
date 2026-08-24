import { createHash, sign, verify } from "node:crypto";
import { copyFile, lstat, mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

export const EXTENSION_PACKAGE_PREFIX = "extension.";
export const LMX_SCHEMA_VERSION = 1;
export const MAX_FILES = 128;
export const MAX_PATH_CHARS = 512;
export const MAX_FILE_BYTES = 3 * 1024 * 1024;
export const MAX_TOTAL_BYTES = 3 * 1024 * 1024;
export const MAX_MANIFEST_BYTES = 256 * 1024;

export function sha256(value) {
  return createHash("sha256").update(value).digest("hex");
}

export function canonical(value) {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  return `{${Object.keys(value).sort().map((key) => `${JSON.stringify(key)}:${canonical(value[key])}`).join(",")}}`;
}

function safeRelative(relative) {
  const normalized = relative.split(path.sep).join("/");
  if (!normalized || normalized.length > MAX_PATH_CHARS || normalized.startsWith("/") || /^[A-Za-z]:/.test(normalized)) throw new Error(`unsafe package path: ${relative}`);
  const parts = normalized.split("/");
  if (parts.some((part) => !part || part === "." || part === "..")) throw new Error(`unsafe package path: ${relative}`);
  return normalized;
}

async function collectFiles(root, directory = root, output = []) {
  for (const entry of (await readdir(directory)).sort()) {
    const absolute = path.join(directory, entry);
    const stat = await lstat(absolute);
    if (stat.isSymbolicLink()) throw new Error(`symlinks are not permitted in .lmx packages: ${absolute}`);
    if (stat.isDirectory()) {
      await collectFiles(root, absolute, output);
      continue;
    }
    if (!stat.isFile()) throw new Error(`unsupported package entry: ${absolute}`);
    const relative = safeRelative(path.relative(root, absolute));
    if (relative === "extension.json") continue;
    if (stat.size > MAX_FILE_BYTES) throw new Error(`${relative} exceeds the per-file .lmx limit`);
    output.push({ absolute, relative, size: stat.size });
    if (output.length > MAX_FILES) throw new Error(`.lmx package exceeds ${MAX_FILES} files`);
  }
  return output;
}

export async function packExtension(sourceDirectory) {
  const root = path.resolve(sourceDirectory);
  const manifest = JSON.parse(await readFile(path.join(root, "extension.json"), "utf8"));
  if (!manifest.extension_id || !manifest.version || !manifest.component?.path) throw new Error("extension.json is missing extension_id, version, or component.path");
  const manifestText = canonical(manifest);
  if (Buffer.byteLength(manifestText, "utf8") > MAX_MANIFEST_BYTES) throw new Error("extension.json exceeds the .lmx manifest metadata limit");
  const files = await collectFiles(root);
  let total = 0;
  const filesBase64 = {};
  for (const file of files.sort((left, right) => left.relative.localeCompare(right.relative))) {
    const bytes = await readFile(file.absolute);
    total += bytes.length;
    if (total > MAX_TOTAL_BYTES) throw new Error(".lmx decoded payload exceeds its limit");
    filesBase64[file.relative] = bytes.toString("base64");
  }
  const componentPath = safeRelative(manifest.component.path);
  if (!(componentPath in filesBase64)) throw new Error("declared component is missing from package files");
  const envelope = { schema_version: LMX_SCHEMA_VERSION, manifest, files_base64: filesBase64 };
  const text = canonical(envelope);
  return {
    envelope,
    text,
    package_sha256: sha256(Buffer.from(text, "utf8")),
    manifest_sha256: sha256(Buffer.from(manifestText, "utf8")),
  };
}

export async function writePackage(sourceDirectory, outputPath) {
  const packed = await packExtension(sourceDirectory);
  await mkdir(path.dirname(path.resolve(outputPath)), { recursive: true });
  await writeFile(outputPath, packed.text, "utf8");
  return packed;
}

export async function publishIntoSnapshot(lmxPath, snapshotPath, registryRoot) {
  const lmxText = await readFile(lmxPath, "utf8");
  const envelope = JSON.parse(lmxText);
  if (canonical(envelope) !== lmxText) throw new Error(".lmx is not in deterministic marketplace encoding; repack it first");
  const manifest = envelope.manifest;
  if (!manifest?.extension_id || !manifest?.version) throw new Error(".lmx has no extension identity/version");
  const packageId = `${EXTENSION_PACKAGE_PREFIX}${manifest.extension_id}`;
  const bundleSha = sha256(Buffer.from(lmxText, "utf8"));
  const manifestSha = sha256(Buffer.from(canonical(manifest), "utf8"));

  const snapshot = JSON.parse(await readFile(snapshotPath, "utf8"));
  if (!snapshot.packages || !snapshot.signature) throw new Error("snapshot is not an M4 registry snapshot");
  if (snapshot.signature.signature_hex) throw new Error("refusing to mutate an already-signed registry snapshot; clear/rebuild it first");
  const provenanceRegistry = manifest.provenance?.source?.curated_registry?.registry_id;
  if (!provenanceRegistry || provenanceRegistry !== snapshot.registry_id) {
    throw new Error(`marketplace extension provenance must name target curated registry ${snapshot.registry_id}`);
  }
  const versions = Array.isArray(snapshot.packages[packageId]) ? snapshot.packages[packageId] : [];
  const filtered = versions.filter((entry) => entry.version !== manifest.version);
  filtered.push({ version: manifest.version, bundle_sha256: bundleSha, manifest_sha256: manifestSha });
  filtered.sort((left, right) => left.version.localeCompare(right.version, undefined, { numeric: true }));
  snapshot.packages[packageId] = filtered;

  const artifactDirectory = path.join(path.resolve(registryRoot), "extensions", manifest.extension_id);
  await mkdir(artifactDirectory, { recursive: true });
  await copyFile(lmxPath, path.join(artifactDirectory, `${manifest.version}.lmx`));
  await writeFile(snapshotPath, `${JSON.stringify(snapshot, null, 2)}\n`, "utf8");
  return { package_id: packageId, version: manifest.version, bundle_sha256: bundleSha, manifest_sha256: manifestSha };
}

function registrySigningShape(snapshot) {
  return {
    schema_version: snapshot.schema_version,
    registry_id: snapshot.registry_id,
    sequence: snapshot.sequence,
    generated_unix_ms: snapshot.generated_unix_ms,
    refresh_after_unix_ms: snapshot.refresh_after_unix_ms,
    expires_unix_ms: snapshot.expires_unix_ms,
    packages: Object.fromEntries(Object.keys(snapshot.packages).sort().map((key) => [key, snapshot.packages[key]])),
    revocations: snapshot.revocations,
    signature: {
      trust_root_id: snapshot.signature.trust_root_id,
      key_id: snapshot.signature.key_id,
      algorithm: snapshot.signature.algorithm,
      signature_hex: "",
    },
  };
}

export function registrySigningPayload(snapshot) {
  if (snapshot.signature?.algorithm !== "ed25519") throw new Error("M4 registry signature algorithm must be ed25519");
  return Buffer.from(JSON.stringify(registrySigningShape(snapshot)), "utf8");
}

export async function signRegistry(snapshotPath, privateKeyPath) {
  const snapshot = JSON.parse(await readFile(snapshotPath, "utf8"));
  const privateKey = await readFile(privateKeyPath, "utf8");
  snapshot.signature.signature_hex = sign(null, registrySigningPayload(snapshot), privateKey).toString("hex");
  await writeFile(snapshotPath, `${JSON.stringify(snapshot, null, 2)}\n`, "utf8");
  return snapshot.signature.signature_hex;
}

export async function verifyRegistry(snapshotPath, publicKeyPath) {
  const snapshot = JSON.parse(await readFile(snapshotPath, "utf8"));
  const signature = Buffer.from(snapshot.signature.signature_hex, "hex");
  if (signature.length === 0) return false;
  const publicKey = await readFile(publicKeyPath, "utf8");
  return verify(null, registrySigningPayload(snapshot), publicKey, signature);
}

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (command === "pack") {
    const [sourceDirectory, outputPath] = args;
    if (!sourceDirectory || !outputPath) throw new Error("usage: marketplace.mjs pack <extension-directory> <output.lmx>");
    const packed = await writePackage(sourceDirectory, outputPath);
    process.stdout.write(`${JSON.stringify({ output: path.resolve(outputPath), package_sha256: packed.package_sha256, manifest_sha256: packed.manifest_sha256 })}\n`);
    return;
  }
  if (command === "publish") {
    const [lmxPath, snapshotPath, registryRoot] = args;
    if (!lmxPath || !snapshotPath || !registryRoot) throw new Error("usage: marketplace.mjs publish <package.lmx> <unsigned-m4-snapshot.json> <static-registry-root>");
    process.stdout.write(`${JSON.stringify(await publishIntoSnapshot(lmxPath, snapshotPath, registryRoot))}\n`);
    return;
  }
  if (command === "sign-registry") {
    const [snapshotPath, privateKeyPath] = args;
    if (!snapshotPath || !privateKeyPath) throw new Error("usage: marketplace.mjs sign-registry <m4-snapshot.json> <ed25519-private-key.pem> <trust-root-id> <key-id>");
    process.stdout.write(`${await signRegistry(snapshotPath, privateKeyPath)}\n`);
    return;
  }
  if (command === "verify-registry") {
    const [snapshotPath, publicKeyPath] = args;
    if (!snapshotPath || !publicKeyPath) throw new Error("usage: marketplace.mjs verify-registry <m4-snapshot.json> <ed25519-public-key.pem>");
    if (!(await verifyRegistry(snapshotPath, publicKeyPath))) process.exitCode = 1;
    return;
  }
  throw new Error("usage: marketplace.mjs <pack|publish|sign-registry|verify-registry> ...");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) await main();

import { mkdir, writeFile, writeTextFile } from "@tauri-apps/plugin-fs";
import { tempDir } from "@tauri-apps/api/path";

import {
  executableExtensionsClient,
  type CapabilityKind,
  type ExtensionApproval,
  type ExtensionDetail,
  type ExtensionManifest,
  type ExtensionPreview,
  type PermissionDeclaration,
} from "./executableExtensionsClient";

export const EXTENSION_REGISTRY_SCHEMA = 1;
export const LMX_SCHEMA = 1;
export const MAX_LMX_BYTES = 64 * 1024 * 1024;
export const MAX_LMX_FILES = 128;
export const MAX_LMX_FILE_BYTES = 32 * 1024 * 1024;

export type ExtensionCategory =
  | "developer_tools"
  | "channels"
  | "models"
  | "search"
  | "knowledge"
  | "productivity"
  | "devices"
  | "speech"
  | "automation"
  | "connectors";

export interface ExtensionRegistrySource {
  source_id: string;
  display_name: string;
  index_url: string;
  /** Raw Ed25519 public key, base64 encoded. Trust is configured by the user;
   * a downloaded index can never introduce its own trust root. */
  public_key_base64: string;
  key_id: string;
  enabled: boolean;
  added_at_ms: number;
  last_sequence: number;
  last_snapshot_sha256: string | null;
}

export interface ExtensionRegistryEntry {
  extension_id: string;
  version: string;
  display_name: string;
  description: string;
  publisher: string;
  category: ExtensionCategory;
  package_url: string;
  package_sha256: string;
  manifest_sha256: string;
  host_api: { minimum: string; maximum_exclusive?: string | null };
  capabilities: CapabilityKind[];
  permissions: PermissionDeclaration[];
  platforms: string[];
  architectures: string[];
  source_url: string | null;
  docs_url: string | null;
  changelog: string | null;
  deprecated: boolean;
  revoked: boolean;
}

export interface ExtensionRegistrySnapshot {
  schema_version: number;
  registry_id: string;
  sequence: number;
  generated_at_ms: number;
  refresh_after_ms: number;
  expires_at_ms: number;
  entries: ExtensionRegistryEntry[];
  revocations: Array<{ extension_id: string; version: string | null; reason: string }>;
  signature: {
    algorithm: "ed25519";
    key_id: string;
    signature_base64: string;
  };
}

export interface VerifiedExtensionRegistry {
  source: ExtensionRegistrySource;
  snapshot: ExtensionRegistrySnapshot;
  snapshot_sha256: string;
  verified_at_ms: number;
}

export interface LmxEnvelope {
  schema_version: number;
  manifest: ExtensionManifest;
  /** Files are base64 bytes keyed by source-relative path. `extension.json`
   * is reconstructed from `manifest`, so it must not appear here. */
  files_base64: Record<string, string>;
}

export interface MarketplaceInstallPreview {
  registry: VerifiedExtensionRegistry;
  entry: ExtensionRegistryEntry;
  source_path: string;
  runtime_preview: ExtensionPreview;
}

function base64Bytes(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
}

function bytesBase64(bytes: Uint8Array): string {
  let binary = "";
  const block = 0x8000;
  for (let index = 0; index < bytes.length; index += block) {
    binary += String.fromCharCode(...bytes.subarray(index, index + block));
  }
  return btoa(binary);
}

async function sha256Bytes(bytes: Uint8Array): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

async function sha256Text(value: string): Promise<string> {
  return sha256Bytes(new TextEncoder().encode(value));
}

function canonical(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value);
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort().map((key) => `${JSON.stringify(key)}:${canonical(object[key])}`).join(",")}}`;
}

function registrySigningPayload(snapshot: ExtensionRegistrySnapshot): Uint8Array {
  const { signature: _signature, ...unsigned } = snapshot;
  return new TextEncoder().encode(canonical(unsigned));
}

async function verifyEd25519(publicKeyBase64: string, signatureBase64: string, payload: Uint8Array): Promise<boolean> {
  const publicKey = await crypto.subtle.importKey(
    "raw",
    base64Bytes(publicKeyBase64),
    { name: "Ed25519" },
    false,
    ["verify"],
  );
  return crypto.subtle.verify({ name: "Ed25519" }, publicKey, base64Bytes(signatureBase64), payload);
}

function validHttpsOrLocal(url: string): boolean {
  try {
    const parsed = new URL(url);
    return parsed.protocol === "https:" || (parsed.protocol === "http:" && ["127.0.0.1", "localhost", "[::1]"].includes(parsed.hostname));
  } catch {
    return false;
  }
}

function validateId(value: string, field: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9_.:-]{0,159}$/.test(value)) throw new Error(`${field} is not a bounded identifier.`);
}

function validateRegistrySnapshot(snapshot: ExtensionRegistrySnapshot, source: ExtensionRegistrySource, nowMs: number): void {
  if (snapshot.schema_version !== EXTENSION_REGISTRY_SCHEMA) throw new Error(`Unsupported extension registry schema ${snapshot.schema_version}.`);
  validateId(snapshot.registry_id, "registry_id");
  if (!Number.isSafeInteger(snapshot.sequence) || snapshot.sequence < 1) throw new Error("Registry sequence must be a positive integer.");
  if (snapshot.sequence < source.last_sequence) throw new Error(`Registry rollback refused: sequence ${snapshot.sequence} is older than trusted sequence ${source.last_sequence}.`);
  if (snapshot.signature.algorithm !== "ed25519") throw new Error("Registry signature must use Ed25519.");
  if (snapshot.signature.key_id !== source.key_id) throw new Error("Registry signing key does not match the configured trust root.");
  if (snapshot.expires_at_ms <= nowMs) throw new Error("Registry snapshot is expired.");
  if (snapshot.generated_at_ms > nowMs + 5 * 60_000) throw new Error("Registry snapshot is implausibly far in the future.");
  if (snapshot.entries.length > 10_000) throw new Error("Registry contains too many entries.");
  const seen = new Set<string>();
  for (const entry of snapshot.entries) {
    validateId(entry.extension_id, "extension_id");
    if (!/^\d+\.\d+\.\d+$/.test(entry.version)) throw new Error(`${entry.extension_id} has a non-canonical version.`);
    if (!validHttpsOrLocal(entry.package_url)) throw new Error(`${entry.extension_id} has an unsafe package URL.`);
    if (!/^[a-f0-9]{64}$/i.test(entry.package_sha256) || !/^[a-f0-9]{64}$/i.test(entry.manifest_sha256)) throw new Error(`${entry.extension_id} has an invalid digest.`);
    const key = `${entry.extension_id}@${entry.version}`;
    if (seen.has(key)) throw new Error(`Registry contains duplicate ${key}.`);
    seen.add(key);
  }
}

export async function fetchVerifiedRegistry(source: ExtensionRegistrySource, nowMs = Date.now()): Promise<VerifiedExtensionRegistry> {
  if (!source.enabled) throw new Error(`${source.display_name} is disabled.`);
  if (!validHttpsOrLocal(source.index_url)) throw new Error("Registry URL must be HTTPS (localhost HTTP is allowed for development). ");
  const response = await fetch(source.index_url, { cache: "no-store", redirect: "follow" });
  if (!response.ok) throw new Error(`Registry returned HTTP ${response.status}.`);
  const raw = await response.text();
  if (raw.length > 16 * 1024 * 1024) throw new Error("Registry index exceeds the 16 MiB limit.");
  const snapshot = JSON.parse(raw) as ExtensionRegistrySnapshot;
  validateRegistrySnapshot(snapshot, source, nowMs);
  if (!(await verifyEd25519(source.public_key_base64, snapshot.signature.signature_base64, registrySigningPayload(snapshot)))) {
    throw new Error("Registry signature verification failed.");
  }
  return {
    source,
    snapshot,
    snapshot_sha256: await sha256Text(canonical(snapshot)),
    verified_at_ms: nowMs,
  };
}

function safeRelativePath(path: string): string {
  const normalized = path.replaceAll("\\", "/");
  if (!normalized || normalized.startsWith("/") || normalized.includes("\0")) throw new Error(`Unsafe extension package path: ${path}`);
  const parts = normalized.split("/");
  if (parts.some((part) => !part || part === "." || part === "..")) throw new Error(`Unsafe extension package path: ${path}`);
  if (/^[A-Za-z]:/.test(normalized)) throw new Error(`Unsafe extension package path: ${path}`);
  return normalized;
}

function validateLmx(envelope: LmxEnvelope): void {
  if (envelope.schema_version !== LMX_SCHEMA) throw new Error(`Unsupported .lmx schema ${envelope.schema_version}.`);
  if (!envelope.manifest?.extension_id || typeof envelope.files_base64 !== "object") throw new Error("Malformed .lmx package.");
  const entries = Object.entries(envelope.files_base64);
  if (entries.length === 0 || entries.length > MAX_LMX_FILES) throw new Error("Invalid .lmx file count.");
  let total = 0;
  const normalized = new Set<string>();
  for (const [path, encoded] of entries) {
    const safe = safeRelativePath(path);
    const collisionKey = safe.normalize("NFC").toLowerCase();
    if (normalized.has(collisionKey)) throw new Error(`Duplicate/colliding .lmx path: ${path}`);
    normalized.add(collisionKey);
    const bytes = base64Bytes(encoded);
    if (bytes.byteLength > MAX_LMX_FILE_BYTES) throw new Error(`${path} exceeds the per-file .lmx limit.`);
    total += bytes.byteLength;
    if (total > MAX_LMX_BYTES) throw new Error(".lmx decoded payload exceeds 64 MiB.");
  }
  const component = safeRelativePath(envelope.manifest.component.path);
  if (!Object.prototype.hasOwnProperty.call(envelope.files_base64, component)) throw new Error(".lmx does not contain the declared component.");
}

async function joinTemp(root: string, relative: string): Promise<string> {
  const separator = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  return `${root.replace(/[\\/]$/, "")}${separator}${relative.replaceAll("/", separator)}`;
}

export async function downloadAndMaterializeLmx(entry: ExtensionRegistryEntry): Promise<string> {
  const response = await fetch(entry.package_url, { cache: "no-store", redirect: "follow" });
  if (!response.ok) throw new Error(`Extension package returned HTTP ${response.status}.`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.byteLength > MAX_LMX_BYTES * 2) throw new Error("Encoded .lmx exceeds the download limit.");
  const digest = await sha256Bytes(bytes);
  if (digest.toLowerCase() !== entry.package_sha256.toLowerCase()) throw new Error("Extension package digest does not match the signed registry entry.");
  let envelope: LmxEnvelope;
  try { envelope = JSON.parse(new TextDecoder().decode(bytes)) as LmxEnvelope; } catch { throw new Error("Extension package is not valid .lmx JSON."); }
  validateLmx(envelope);
  if (envelope.manifest.extension_id !== entry.extension_id || envelope.manifest.version !== entry.version) throw new Error("Extension package identity does not match its registry entry.");
  const manifestDigest = await sha256Text(canonical(envelope.manifest));
  if (manifestDigest.toLowerCase() !== entry.manifest_sha256.toLowerCase()) throw new Error("Extension manifest digest does not match its registry entry.");

  const root = await tempDir();
  const directory = await joinTemp(root, `little-monkey-extension-${entry.extension_id.replace(/[^A-Za-z0-9_.-]/g, "-")}-${entry.version}-${crypto.randomUUID()}`);
  await mkdir(directory, { recursive: true });
  await writeTextFile(await joinTemp(directory, "extension.json"), `${JSON.stringify(envelope.manifest, null, 2)}\n`);
  for (const [relative, encoded] of Object.entries(envelope.files_base64)) {
    const safe = safeRelativePath(relative);
    const parts = safe.split("/");
    if (parts.length > 1) await mkdir(await joinTemp(directory, parts.slice(0, -1).join("/")), { recursive: true });
    await writeFile(await joinTemp(directory, safe), base64Bytes(encoded));
  }
  return directory;
}

export async function previewMarketplaceInstall(registry: VerifiedExtensionRegistry, entry: ExtensionRegistryEntry): Promise<MarketplaceInstallPreview> {
  if (entry.revoked || registry.snapshot.revocations.some((revocation) => revocation.extension_id === entry.extension_id && (revocation.version === null || revocation.version === entry.version))) {
    throw new Error(`${entry.extension_id}@${entry.version} is revoked.`);
  }
  const source_path = await downloadAndMaterializeLmx(entry);
  const runtime_preview = await executableExtensionsClient.discover(source_path);
  if (runtime_preview.manifest.extension_id !== entry.extension_id || runtime_preview.manifest.version !== entry.version) throw new Error("Runtime preview identity differs from signed registry metadata.");
  return { registry, entry, source_path, runtime_preview };
}

export function approvalForPreview(preview: ExtensionPreview, grants: ExtensionApproval["grants"], acknowledgeHighRisk: boolean, acknowledgeUntrusted: boolean): ExtensionApproval {
  return {
    approval_digest: preview.approval_digest,
    grants,
    allow_unsigned: false,
    allow_untrusted: acknowledgeUntrusted,
    allow_high_risk: acknowledgeHighRisk,
  };
}

export function isSafeAutomaticUpdate(preview: ExtensionPreview, installed: ExtensionDetail, entry: ExtensionRegistryEntry): { safe: boolean; reasons: string[] } {
  const reasons: string[] = [];
  if (entry.revoked || entry.deprecated) reasons.push("registry marks this release revoked/deprecated");
  if (installed.manifest.publisher !== preview.manifest.publisher) reasons.push("publisher changed");
  if (installed.trust.state !== "verified" || preview.trust.state !== "verified") reasons.push("publisher/runtime trust is not verified");
  if (preview.permission_diff?.expands_authority) reasons.push("permissions expand authority");
  if (!preview.compatible) reasons.push(preview.compatibility_reason ?? "host API/platform is incompatible");
  if (preview.requires_unsigned_approval || preview.requires_untrusted_approval || preview.requires_high_risk_approval) reasons.push("runtime requires a new trust/risk acknowledgement");
  return { safe: reasons.length === 0, reasons };
}

export function registryEntryKey(entry: ExtensionRegistryEntry): string {
  return `${entry.extension_id}@${entry.version}`;
}

export function latestEntries(registries: VerifiedExtensionRegistry[]): ExtensionRegistryEntry[] {
  const byExtension = new Map<string, ExtensionRegistryEntry>();
  for (const registry of registries) {
    for (const entry of registry.snapshot.entries) {
      if (entry.revoked) continue;
      const current = byExtension.get(entry.extension_id);
      if (!current || compareSemver(entry.version, current.version) > 0) byExtension.set(entry.extension_id, entry);
    }
  }
  return [...byExtension.values()].sort((left, right) => left.display_name.localeCompare(right.display_name));
}

export function compareSemver(left: string, right: string): number {
  const a = left.split(".").map(Number);
  const b = right.split(".").map(Number);
  for (let index = 0; index < 3; index += 1) {
    const delta = (a[index] ?? 0) - (b[index] ?? 0);
    if (delta !== 0) return delta;
  }
  return 0;
}

/** SDK helper used by tests/scripts to produce byte-identical .lmx envelopes. */
export function deterministicLmxText(manifest: ExtensionManifest, files: Record<string, Uint8Array>): string {
  const files_base64 = Object.fromEntries(Object.entries(files).sort(([a], [b]) => a.localeCompare(b)).map(([path, bytes]) => [safeRelativePath(path), bytesBase64(bytes)]));
  return canonical({ schema_version: LMX_SCHEMA, manifest, files_base64 });
}

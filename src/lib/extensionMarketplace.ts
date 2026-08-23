import { invoke } from "@tauri-apps/api/core";
import { tempDir } from "@tauri-apps/api/path";
import { mkdir, writeFile, writeTextFile } from "@tauri-apps/plugin-fs";

import {
  executableExtensionsClient,
  type ExtensionDetail,
  type ExtensionManifest,
  type ExtensionPreview,
} from "./executableExtensionsClient";
import type {
  AdditionalRegistryRecord,
  RegistrySnapshot,
} from "./ecosystemClient";

/**
 * Executable extensions deliberately do NOT become M4 declarative packages.
 * The existing signed M4 registry is reused only as an artifact index: entries
 * in the reserved `extension.` namespace bind an extension id/version to an
 * immutable .lmx digest and manifest digest. The downloaded bytes then go
 * through the existing executable-extension runtime, which independently
 * validates the manifest/component/signature and permission approval digest.
 */
export const M4_EXTENSION_PACKAGE_PREFIX = "extension.";
export const LMX_SCHEMA = 1;
/** `tool_web_fetch` has a host-owned 5 MiB response-body ceiling. Keep the
 * deterministic package comfortably below it even after base64/JSON overhead. */
export const MAX_LMX_DOWNLOAD_CHARS = 5 * 1024 * 1024;
export const MAX_LMX_FILES = 128;
export const MAX_LMX_PATH_CHARS = 512;
export const MAX_LMX_FILE_BYTES = 3 * 1024 * 1024;
export const MAX_LMX_DECODED_BYTES = 3 * 1024 * 1024;
export const MAX_LMX_MANIFEST_BYTES = 256 * 1024;

interface RegistryPackageVersionWire {
  version: string;
  bundle_sha256: string;
  manifest_sha256: string;
}

interface FetchResultWire {
  url: string;
  final_url: string;
  title: string | null;
  content_type: string;
  markdown: string;
  total_chars: number;
  truncated: boolean;
}

export interface MarketplaceRegistry {
  record: AdditionalRegistryRecord;
  snapshot: RegistrySnapshot;
}

export interface ExtensionRegistryEntry {
  registry_source_id: string;
  registry_display_name: string;
  registry_snapshot_sha256: string;
  package_id: string;
  extension_id: string;
  version: string;
  package_url: string;
  package_sha256: string;
  manifest_sha256: string;
  revoked: boolean;
  revocation_reason: string | null;
}

export interface LmxEnvelope {
  schema_version: number;
  manifest: ExtensionManifest;
  /** Files are base64 bytes keyed by source-relative path. extension.json is
   * reconstructed from `manifest` and therefore must not appear here. */
  files_base64: Record<string, string>;
}

export interface MarketplaceInstallPreview {
  registry: MarketplaceRegistry;
  entry: ExtensionRegistryEntry;
  source_path: string;
  runtime_preview: ExtensionPreview;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function isSha256(value: unknown): value is string {
  return typeof value === "string" && /^[a-f0-9]{64}$/i.test(value);
}

function parseSemver(value: string): [number, number, number] | null {
  const match = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/.exec(value);
  return match ? [Number(match[1]), Number(match[2]), Number(match[3])] : null;
}

export function compareSemver(left: string, right: string): number {
  const a = parseSemver(left);
  const b = parseSemver(right);
  if (!a || !b) return left.localeCompare(right);
  for (let index = 0; index < 3; index += 1) {
    if (a[index] !== b[index]) return a[index] - b[index];
  }
  return 0;
}

function validRegistryLocation(value: string): URL {
  const url = new URL(value);
  if (url.protocol !== "https:" && !(url.protocol === "http:" && ["localhost", "127.0.0.1", "[::1]"].includes(url.hostname))) {
    throw new Error(`Registry ${value} must use HTTPS (localhost HTTP is allowed for development).`);
  }
  if (url.username || url.password) throw new Error("Registry URLs cannot contain credentials.");
  return url;
}

/** Static-registry convention used by the publisher CLI. The URL itself does
 * not need to be trusted: the already-verified M4 snapshot binds the bytes to
 * `bundle_sha256`, so a compromised mirror can only cause a refusal. */
export function extensionArtifactUrl(registryLocation: string, extensionId: string, version: string): string {
  const registry = validRegistryLocation(registryLocation);
  const base = new URL(".", registry);
  return new URL(`extensions/${encodeURIComponent(extensionId)}/${version}.lmx`, base).toString();
}

function parseRegistryVersion(value: unknown): RegistryPackageVersionWire | null {
  if (!value || typeof value !== "object") return null;
  const candidate = value as Partial<RegistryPackageVersionWire>;
  if (typeof candidate.version !== "string" || !parseSemver(candidate.version)) return null;
  if (!isSha256(candidate.bundle_sha256) || !isSha256(candidate.manifest_sha256)) return null;
  return {
    version: candidate.version,
    bundle_sha256: candidate.bundle_sha256.toLowerCase(),
    manifest_sha256: candidate.manifest_sha256.toLowerCase(),
  };
}

function nestedPackageTarget(value: unknown): { packageId: string; version: string | null } | null {
  if (!value || typeof value !== "object") return null;
  const record = value as Record<string, unknown>;
  const packageId = typeof record.package_id === "string" ? record.package_id : null;
  if (packageId) {
    return {
      packageId,
      version: typeof record.version === "string" ? record.version : null,
    };
  }
  for (const child of Object.values(record)) {
    const target = nestedPackageTarget(child);
    if (target) return target;
  }
  return null;
}

function revocationFor(snapshot: RegistrySnapshot, packageId: string, version: string): string | null {
  for (const raw of snapshot.revocations ?? []) {
    const target = nestedPackageTarget(raw);
    if (!target || target.packageId !== packageId) continue;
    if (target.version !== null && target.version !== version) continue;
    if (raw && typeof raw === "object") {
      const record = raw as Record<string, unknown>;
      if (typeof record.reason === "string" && record.reason.trim()) return record.reason.trim();
    }
    return "revoked by the signed M4 registry";
  }
  return null;
}

export function marketplaceRegistries(records: AdditionalRegistryRecord[]): MarketplaceRegistry[] {
  return records
    .filter((record): record is AdditionalRegistryRecord & { verified: NonNullable<AdditionalRegistryRecord["verified"]> } => record.verified !== null)
    .map((record) => ({ record, snapshot: record.verified.snapshot }));
}

export function extensionEntriesFromRegistries(registries: MarketplaceRegistry[]): ExtensionRegistryEntry[] {
  const entries: ExtensionRegistryEntry[] = [];
  for (const registry of registries) {
    const location = registry.record.source.location;
    for (const [packageId, rawVersions] of Object.entries(registry.snapshot.packages ?? {})) {
      if (!packageId.startsWith(M4_EXTENSION_PACKAGE_PREFIX)) continue;
      const extensionId = packageId.slice(M4_EXTENSION_PACKAGE_PREFIX.length);
      if (!extensionId) continue;
      for (const raw of rawVersions) {
        const version = parseRegistryVersion(raw);
        if (!version) continue; // Rust already verified the snapshot; fail-soft for old frontend shapes.
        const reason = revocationFor(registry.snapshot, packageId, version.version);
        entries.push({
          registry_source_id: registry.record.source.source_id,
          registry_display_name: registry.record.source.display_name,
          registry_snapshot_sha256: registry.record.verified!.snapshot_sha256,
          package_id: packageId,
          extension_id: extensionId,
          version: version.version,
          package_url: extensionArtifactUrl(location, extensionId, version.version),
          package_sha256: version.bundle_sha256,
          manifest_sha256: version.manifest_sha256,
          revoked: reason !== null,
          revocation_reason: reason,
        });
      }
    }
  }
  return entries.sort((left, right) =>
    left.extension_id.localeCompare(right.extension_id) || compareSemver(right.version, left.version),
  );
}

export function latestEntries(registries: MarketplaceRegistry[]): ExtensionRegistryEntry[] {
  const latest = new Map<string, ExtensionRegistryEntry>();
  for (const entry of extensionEntriesFromRegistries(registries)) {
    if (entry.revoked) continue;
    const current = latest.get(entry.extension_id);
    if (!current || compareSemver(entry.version, current.version) > 0) latest.set(entry.extension_id, entry);
  }
  return [...latest.values()].sort((left, right) => left.extension_id.localeCompare(right.extension_id));
}

function base64Bytes(value: string): Uint8Array {
  const binary = atob(value);
  const bytes = new Uint8Array(binary.length);
  for (let index = 0; index < binary.length; index += 1) bytes[index] = binary.charCodeAt(index);
  return bytes;
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

function safeRelativePath(path: string): string {
  const normalized = path.replaceAll("\\", "/");
  if (!normalized || normalized.length > MAX_LMX_PATH_CHARS || normalized.startsWith("/") || normalized.includes("\0") || /^[A-Za-z]:/.test(normalized)) {
    throw new Error(`Unsafe .lmx path: ${path}`);
  }
  const parts = normalized.split("/");
  if (parts.some((part) => !part || part === "." || part === "..")) throw new Error(`Unsafe .lmx path: ${path}`);
  return normalized;
}

export function validateLmxEnvelope(envelope: LmxEnvelope): void {
  if (envelope.schema_version !== LMX_SCHEMA) throw new Error(`Unsupported .lmx schema ${String(envelope.schema_version)}.`);
  if (!envelope.manifest?.extension_id || !envelope.manifest?.version || !envelope.files_base64 || typeof envelope.files_base64 !== "object") {
    throw new Error("Malformed .lmx package.");
  }
  if (new TextEncoder().encode(canonical(envelope.manifest)).byteLength > MAX_LMX_MANIFEST_BYTES) {
    throw new Error(".lmx manifest exceeds its bounded metadata limit.");
  }
  const files = Object.entries(envelope.files_base64);
  if (files.length === 0 || files.length > MAX_LMX_FILES) throw new Error("Invalid .lmx file count.");
  const collisions = new Set<string>();
  let decodedBytes = 0;
  for (const [rawPath, encoded] of files) {
    const path = safeRelativePath(rawPath);
    const collisionKey = path.normalize("NFC").toLocaleLowerCase("en-US");
    if (collisions.has(collisionKey)) throw new Error(`Duplicate/colliding .lmx path: ${rawPath}`);
    collisions.add(collisionKey);
    const bytes = base64Bytes(encoded);
    if (bytes.byteLength > MAX_LMX_FILE_BYTES) throw new Error(`${rawPath} exceeds the per-file .lmx limit.`);
    decodedBytes += bytes.byteLength;
    if (decodedBytes > MAX_LMX_DECODED_BYTES) throw new Error(".lmx decoded payload exceeds its limit.");
  }
  const component = safeRelativePath(envelope.manifest.component.path);
  if (!Object.prototype.hasOwnProperty.call(envelope.files_base64, component)) {
    throw new Error(".lmx does not contain the component declared by extension.json.");
  }
  if (Object.keys(envelope.files_base64).some((path) => safeRelativePath(path) === "extension.json")) {
    throw new Error(".lmx must not contain a second extension.json; the signed manifest is the single source of truth.");
  }
}

async function fetchPackageText(url: string): Promise<string> {
  const result = await invoke<FetchResultWire>("tool_web_fetch", {
    url,
    max_chars: MAX_LMX_DOWNLOAD_CHARS,
    start_index: 0,
    turn_id: null,
    tool_call_id: crypto.randomUUID(),
  });
  if (result.truncated || result.total_chars > MAX_LMX_DOWNLOAD_CHARS) {
    throw new Error(`.lmx package exceeds the ${MAX_LMX_DOWNLOAD_CHARS} character marketplace limit.`);
  }
  if (!/^application\/(?:json|[^;]+\+json)|^text\/plain/i.test(result.content_type)) {
    throw new Error(`.lmx must be served as JSON/text; received ${result.content_type || "unknown content type"}.`);
  }
  return result.markdown;
}

async function joinTemp(root: string, relative: string): Promise<string> {
  const separator = root.includes("\\") && !root.includes("/") ? "\\" : "/";
  return `${root.replace(/[\\/]$/, "")}${separator}${relative.replaceAll("/", separator)}`;
}

export async function downloadAndMaterializeLmx(entry: ExtensionRegistryEntry): Promise<string> {
  if (entry.revoked) throw new Error(entry.revocation_reason ?? `${entry.extension_id}@${entry.version} is revoked.`);
  const raw = await fetchPackageText(entry.package_url);
  const packageDigest = await sha256Text(raw);
  if (packageDigest.toLowerCase() !== entry.package_sha256.toLowerCase()) {
    throw new Error(".lmx bytes do not match the digest in the verified M4 registry snapshot.");
  }
  let envelope: LmxEnvelope;
  try {
    envelope = JSON.parse(raw) as LmxEnvelope;
  } catch {
    throw new Error("Downloaded extension is not valid .lmx JSON.");
  }
  validateLmxEnvelope(envelope);
  if (envelope.manifest.extension_id !== entry.extension_id || envelope.manifest.version !== entry.version) {
    throw new Error(".lmx identity/version does not match the verified M4 catalog entry.");
  }
  const manifestDigest = await sha256Text(canonical(envelope.manifest));
  if (manifestDigest.toLowerCase() !== entry.manifest_sha256.toLowerCase()) {
    throw new Error(".lmx manifest does not match the manifest digest in the verified M4 registry snapshot.");
  }

  const root = await tempDir();
  const safeId = entry.extension_id.replace(/[^A-Za-z0-9_.-]/g, "-");
  const directory = await joinTemp(root, `little-monkey-lmx-${safeId}-${entry.version}-${crypto.randomUUID()}`);
  await mkdir(directory, { recursive: true });
  await writeTextFile(await joinTemp(directory, "extension.json"), `${JSON.stringify(envelope.manifest, null, 2)}\n`);
  for (const [relative, encoded] of Object.entries(envelope.files_base64)) {
    const path = safeRelativePath(relative);
    const parts = path.split("/");
    if (parts.length > 1) await mkdir(await joinTemp(directory, parts.slice(0, -1).join("/")), { recursive: true });
    await writeFile(await joinTemp(directory, path), base64Bytes(encoded));
  }
  return directory;
}

export async function previewMarketplaceInstall(registry: MarketplaceRegistry, entry: ExtensionRegistryEntry): Promise<MarketplaceInstallPreview> {
  if (registry.record.source.source_id !== entry.registry_source_id) throw new Error("Registry provenance mismatch.");
  if (registry.record.verified?.snapshot_sha256 !== entry.registry_snapshot_sha256) {
    throw new Error("Registry snapshot changed; refresh the marketplace before installing.");
  }
  const source_path = await downloadAndMaterializeLmx(entry);
  const runtime_preview = await executableExtensionsClient.discover(source_path);
  if (runtime_preview.manifest.extension_id !== entry.extension_id || runtime_preview.manifest.version !== entry.version) {
    throw new Error("Executable runtime preview disagrees with the verified M4 catalog identity.");
  }
  return { registry, entry, source_path, runtime_preview };
}

export function isSafeAutomaticUpdate(
  preview: ExtensionPreview,
  installed: ExtensionDetail,
  entry: ExtensionRegistryEntry,
): { safe: boolean; reasons: string[] } {
  const reasons: string[] = [];
  if (entry.revoked) reasons.push(entry.revocation_reason ?? "registry revoked this release");
  if (compareSemver(entry.version, installed.active_version) <= 0) reasons.push("candidate is not newer than the active version");
  if (installed.manifest.publisher !== preview.manifest.publisher) reasons.push("publisher changed");
  if (installed.trust.state !== "verified" || preview.trust.state !== "verified") reasons.push("runtime publisher trust is not verified");
  if (installed.trust.trust_root_id !== preview.trust.trust_root_id || installed.trust.key_id !== preview.trust.key_id) reasons.push("publisher signing lineage changed");
  if (preview.permission_diff?.expands_authority) reasons.push("permissions expand authority");
  if (!preview.compatible) reasons.push(preview.compatibility_reason ?? "host API/platform is incompatible");
  if (preview.blockers.length > 0) reasons.push(...preview.blockers);
  if (preview.requires_unsigned_approval || preview.requires_untrusted_approval || preview.requires_high_risk_approval) {
    reasons.push("runtime requires a new trust/risk acknowledgement");
  }
  return { safe: reasons.length === 0, reasons };
}

export function marketplaceDiagnostic(records: AdditionalRegistryRecord[]): string[] {
  const findings: string[] = [];
  for (const record of records) {
    if (!record.verified) findings.push(`${record.source.display_name}: registry has no verified snapshot`);
    else if (record.verified.snapshot.expires_unix_ms <= Date.now()) findings.push(`${record.source.display_name}: verified snapshot is expired`);
    if (record.last_verification_error) findings.push(`${record.source.display_name}: ${record.last_verification_error}`);
  }
  return findings;
}

export function formatMarketplaceFailure(context: string, error: unknown): string {
  return `${context}: ${errorMessage(error)}`;
}

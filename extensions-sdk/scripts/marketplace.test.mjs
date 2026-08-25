import assert from "node:assert/strict";
import { generateKeyPairSync } from "node:crypto";
import { mkdtemp, mkdir, readFile, readdir, symlink, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";

import {
  packExtension,
  publishIntoSnapshot,
  signRegistry,
  verifyRegistry,
  writePackage,
} from "./marketplace.mjs";

function manifest(registryId = "fixture-registry") {
  return {
    schema_version: 1,
    extension_id: "com.example.echo",
    version: "1.2.3",
    display_name: "Echo",
    description: "fixture",
    host_api: { minimum: "1.0.0" },
    component: { path: "component.wasm", sha256: "0".repeat(64) },
    capabilities: [{ capability_id: "echo", kind: "tool", display_name: "Echo", description: "fixture", input_schema: { type: "object" } }],
    permissions: [],
    config_schema: [],
    secret_slots: [],
    dependencies: [],
    compatibility: { minimum_app_version: "0.1.0", maximum_app_version_exclusive: null, platforms: [], architectures: [] },
    publisher: "example",
    provenance: { publisher: "example", source: { curated_registry: { registry_id: registryId } }, source_revision: "a".repeat(40), build_reproducible: true },
    signature: null,
    checksums: { "component.wasm": "0".repeat(64) },
  };
}

async function extensionFixture(root, registryId = "fixture-registry") {
  await mkdir(root, { recursive: true });
  await writeFile(path.join(root, "extension.json"), `${JSON.stringify(manifest(registryId), null, 2)}\n`);
  await writeFile(path.join(root, "component.wasm"), Buffer.from([0, 97, 115, 109]));
  await mkdir(path.join(root, "assets"));
  await writeFile(path.join(root, "assets", "note.txt"), "hello");
}

function unsignedSnapshot() {
  return {
    schema_version: 1,
    registry_id: "fixture-registry",
    sequence: 1,
    generated_unix_ms: 1000,
    refresh_after_unix_ms: 2000,
    expires_unix_ms: 3000,
    packages: {},
    revocations: [],
    signature: { trust_root_id: "fixture-root", key_id: "fixture-key", algorithm: "ed25519", signature_hex: "" },
  };
}

test("packExtension is deterministic and hashes exact compact .lmx bytes", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-"));
  const source = path.join(temp, "extension");
  await extensionFixture(source);
  const first = await packExtension(source);
  const second = await packExtension(source);
  assert.equal(first.text, second.text);
  assert.equal(first.package_sha256, second.package_sha256);
  assert.equal(first.manifest_sha256, second.manifest_sha256);
  assert.ok(!first.text.endsWith("\n"));
  assert.deepEqual(Object.keys(first.envelope.files_base64), ["assets/note.txt", "component.wasm"]);
});

test("packExtension rejects symlinks instead of following them", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-link-"));
  const source = path.join(temp, "extension");
  await extensionFixture(source);
  await symlink(path.join(source, "component.wasm"), path.join(source, "alias.wasm"));
  await assert.rejects(() => packExtension(source), /symlinks are not permitted/);
});

test("packExtension rejects paths the native marketplace cannot materialize portably", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-path-"));
  const source = path.join(temp, "extension");
  await extensionFixture(source);
  await writeFile(path.join(source, "assets", "café.txt"), "unicode");
  await assert.rejects(() => packExtension(source), /unsafe package path/);
});

test("packExtension rejects case-colliding paths when the host filesystem can represent them", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-case-"));
  const source = path.join(temp, "extension");
  await extensionFixture(source);
  await writeFile(path.join(source, "assets", "Note.txt"), "collision");
  const names = await readdir(path.join(source, "assets"));
  if (names.includes("note.txt") && names.includes("Note.txt")) {
    await assert.rejects(() => packExtension(source), /duplicate\/reserved package path/);
  }
});

test("publishIntoSnapshot reuses M4 packages and static extension layout", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-publish-"));
  const source = path.join(temp, "extension");
  const lmx = path.join(temp, "echo.lmx");
  const snapshotPath = path.join(temp, "index.json");
  const registryRoot = path.join(temp, "public");
  await extensionFixture(source);
  const packed = await writePackage(source, lmx);
  await writeFile(snapshotPath, `${JSON.stringify(unsignedSnapshot(), null, 2)}\n`);
  const result = await publishIntoSnapshot(lmx, snapshotPath, registryRoot);
  assert.equal(result.package_id, "extension.com.example.echo");
  assert.equal(result.bundle_sha256, packed.package_sha256);
  assert.equal(result.manifest_sha256, packed.manifest_sha256);
  const snapshot = JSON.parse(await readFile(snapshotPath, "utf8"));
  assert.deepEqual(snapshot.packages[result.package_id], [{ version: "1.2.3", bundle_sha256: packed.package_sha256, manifest_sha256: packed.manifest_sha256 }]);
  assert.equal(await readFile(path.join(registryRoot, "extensions", "com.example.echo", "1.2.3.lmx"), "utf8"), packed.text);
});

test("publishIntoSnapshot refuses extension provenance for another registry", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-provenance-"));
  const source = path.join(temp, "extension");
  const lmx = path.join(temp, "echo.lmx");
  const snapshotPath = path.join(temp, "index.json");
  await extensionFixture(source, "other-registry");
  await writePackage(source, lmx);
  await writeFile(snapshotPath, `${JSON.stringify(unsignedSnapshot(), null, 2)}\n`);
  await assert.rejects(
    () => publishIntoSnapshot(lmx, snapshotPath, path.join(temp, "public")),
    /provenance must name target curated registry fixture-registry/,
  );
});

test("M4 registry signing helper round-trips Ed25519 payload", async () => {
  const temp = await mkdtemp(path.join(os.tmpdir(), "lm-marketplace-sign-"));
  const snapshotPath = path.join(temp, "index.json");
  const privateKeyPath = path.join(temp, "private.pem");
  const publicKeyPath = path.join(temp, "public.pem");
  const { privateKey, publicKey } = generateKeyPairSync("ed25519", {
    privateKeyEncoding: { type: "pkcs8", format: "pem" },
    publicKeyEncoding: { type: "spki", format: "pem" },
  });
  await writeFile(privateKeyPath, privateKey);
  await writeFile(publicKeyPath, publicKey);
  await writeFile(snapshotPath, `${JSON.stringify(unsignedSnapshot(), null, 2)}\n`);
  const signature = await signRegistry(snapshotPath, privateKeyPath);
  assert.match(signature, /^[a-f0-9]+$/);
  assert.equal(await verifyRegistry(snapshotPath, publicKeyPath), true);
});

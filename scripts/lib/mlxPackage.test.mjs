/**
 * The packaging side of the MLX install contract.
 *
 * Run with `node --test scripts/lib/mlxPackage.test.mjs`, like the other
 * script tests in this directory.
 *
 * CANONICAL_FIXTURE below is the load-bearing assertion: the same string is
 * asserted byte for byte by the Rust test
 * `canonical_manifest_bytes_match_the_packaging_script` in mlx_runtime.rs. If
 * either canonicalizer drifts, one of the two tests fails — which is the only
 * way to catch it, because a drifted signature is indistinguishable from a
 * tampered package at install time and reports the same "signature is invalid".
 */

import { deepStrictEqual, ok, strictEqual, throws } from "node:assert";
import { generateKeyPairSync, verify as cryptoVerify } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { buildManifest, canonicalJson, signedPayload, signManifest } from "./mlxPackage.mjs";

const CANONICAL_FIXTURE =
  '{"files":[{"executable":true,"path":"bin/python","sha256":"' +
  "0000000000000000000000000000000000000000000000000000000000000000" +
  '","sizeBytes":3}],"packageVersion":"mlx-0.1.0","pythonExecutable":"bin/python",' +
  '"schemaVersion":1,"serviceEntry":"service/mlx_server.py","signatureAlgorithm":"ed25519",' +
  '"signatureKeyId":"release-2026-1","targetArchitecture":"aarch64","targetOs":"macos"}';

const FIXTURE_MANIFEST = {
  schemaVersion: 1,
  packageVersion: "mlx-0.1.0",
  targetOs: "macos",
  targetArchitecture: "aarch64",
  pythonExecutable: "bin/python",
  serviceEntry: "service/mlx_server.py",
  files: [
    {
      path: "bin/python",
      sizeBytes: 3,
      sha256: "0000000000000000000000000000000000000000000000000000000000000000",
      executable: true,
    },
  ],
  signatureAlgorithm: "ed25519",
  signatureKeyId: "release-2026-1",
};

test("canonical JSON sorts every key and matches the Rust canonicalizer", () => {
  strictEqual(canonicalJson(FIXTURE_MANIFEST), CANONICAL_FIXTURE);
  // Key order in the input must not change the output, which is the whole
  // point: the two languages build this object in different orders.
  const shuffled = Object.fromEntries(Object.entries(FIXTURE_MANIFEST).reverse());
  strictEqual(canonicalJson(shuffled), CANONICAL_FIXTURE);
});

test("the signed payload drops signatureBase64 rather than emptying it", () => {
  const signed = { ...FIXTURE_MANIFEST, signatureBase64: "ignored" };
  strictEqual(signedPayload(signed).toString("utf8"), CANONICAL_FIXTURE);
  ok(!signedPayload(signed).toString("utf8").includes("signatureBase64"));
});

test("a signed manifest verifies under the matching public key", () => {
  const { privateKey, publicKey } = generateKeyPairSync("ed25519");
  const signed = signManifest(FIXTURE_MANIFEST, privateKey.export({ type: "pkcs8", format: "pem" }));
  ok(
    cryptoVerify(null, signedPayload(signed), publicKey, Buffer.from(signed.signatureBase64, "base64")),
  );
  // ...and does not verify once a single byte of the covered manifest moves,
  // which is what makes the digest list worth carrying.
  const tampered = { ...signed, packageVersion: "mlx-0.1.1" };
  ok(
    !cryptoVerify(
      null,
      signedPayload(tampered),
      publicKey,
      Buffer.from(signed.signatureBase64, "base64"),
    ),
  );
});

test("a non-ed25519 key is refused rather than producing an unusable package", () => {
  const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
  throws(
    () => signManifest(FIXTURE_MANIFEST, privateKey.export({ type: "pkcs8", format: "pem" })),
    /expected ed25519/,
  );
});

test("buildManifest sorts files, digests them, and refuses a missing entry point", () => {
  const root = mkdtempSync(join(tmpdir(), "mlx-pkg-"));
  try {
    mkdirSync(join(root, "bin"));
    mkdirSync(join(root, "service"));
    // Written out of order on purpose — the manifest must come back sorted.
    writeFileSync(join(root, "service/mlx_server.py"), "print(1)\n");
    writeFileSync(join(root, "bin/python"), "#!/bin/sh\n");
    chmodSync(join(root, "bin/python"), 0o755);

    const manifest = buildManifest({
      root,
      packageVersion: "mlx-test",
      pythonExecutable: "bin/python",
      serviceEntry: "service/mlx_server.py",
      keyId: "release-2026-1",
    });
    deepStrictEqual(
      manifest.files.map((file) => file.path),
      ["bin/python", "service/mlx_server.py"],
    );
    ok(manifest.files.every((file) => /^[0-9a-f]{64}$/.test(file.sha256)));
    strictEqual(manifest.files[0].executable, true, "the interpreter must be executable");

    throws(
      () =>
        buildManifest({
          root,
          packageVersion: "mlx-test",
          pythonExecutable: "bin/python",
          serviceEntry: "service/absent.py",
          keyId: "release-2026-1",
        }),
      /missing from/,
    );
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

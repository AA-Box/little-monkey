#!/usr/bin/env node
/**
 * Offline, distributor-only issuance of a MiniMax H3 entitlement receipt.
 *
 * THIS DOES NOT GRANT MINI MAX RIGHTS. Run ONLY after legal review of an
 * actual, separately issued written MiniMax authorization covering the named
 * holder, territory, product and usage period. The private key is external
 * and never checked into the repository or embedded into the application.
 *
 * Usage:
 * node scripts/issue-minimax-entitlement.mjs \
 *  --private-key /secure/issuer-ed25519.pem \
 *  --license-reference "MiniMax written authorization #..." \
 *  --holder-id "Organization-123" --regions eu,us \
 *  --expires 2027-01-01T00:00:00Z \
 *  --written-minimax-authorization-reviewed
 */
import { readFileSync } from "node:fs";
import { createPrivateKey, createPublicKey, sign } from "node:crypto";

function parseArgs(args) {
  const result = {};
  for (let i = 0; i < args.length; i++) {
    const key = args[i];
    if (key === "--written-minimax-authorization-reviewed") {
      result.reviewed = true;
      continue;
    }
    if (!key.startsWith("--") || i + 1 >= args.length) {
      throw new Error("Invalid argument list");
    }
    result[key.slice(2)] = args[++i];
  }
  return result;
}

export function issueSignedGrant(opts, pem, now = new Date()) {
  if (!opts.reviewed) {
    throw new Error("An actual separate MiniMax written license must be reviewed first.");
  }
  for (const key of ["license-reference", "holder-id", "regions", "expires"]) {
    if (!opts[key] || opts[key].length > 256) throw new Error(`Missing or oversized ${key}`);
  }
  const regions = opts.regions.split(",").map((v) => v.trim());
  if (regions.length === 0 || regions.length > 4 || new Set(regions).size !== regions.length ||
      regions.some((v) => !["eu", "uk", "us", "kr", "other"].includes(v))) {
    throw new Error("Invalid explicit territory list");
  }
  const notBeforeUnix = Math.floor(now.getTime() / 1000);
  const expiresAtUnix = Math.floor(new Date(opts.expires).getTime() / 1000);
  if (!Number.isFinite(expiresAtUnix) || expiresAtUnix <= notBeforeUnix ||
      expiresAtUnix > notBeforeUnix + 366 * 86400) {
    throw new Error("Entitlement expiry must be in the next 366 days");
  }
  const payload = {
    audience: "little-monkey",
    modelFamily: "minimax-h3",
    licenseReference: opts["license-reference"],
    holderId: opts["holder-id"],
    allowedRegions: regions,
    notBeforeUnix,
    expiresAtUnix,
  };
  const key = createPrivateKey(pem);
  if (key.asymmetricKeyType !== "ed25519") throw new Error("Issuer must use an Ed25519 private key");
  const bytes = Buffer.from(JSON.stringify(payload), "utf8");
  const signature = sign(null, bytes, key);
  const der = createPublicKey(key).export({ format: "der", type: "spki" });
  // RFC 8410 Ed25519 SPKI: 12-byte DER prefix followed by 32 raw bytes.
  const header = Buffer.from("302a300506032b6570032100", "hex");
  if (!der.subarray(0, header.length).equals(header) || der.length !== 44) {
    throw new Error("Unexpected Ed25519 public key representation");
  }
  return {
    receipt: {
      payloadBase64: bytes.toString("base64"),
      signatureBase64: signature.toString("base64"),
    },
    issuerPublicKeyBase64: der.subarray(header.length).toString("base64"),
  };
}

if (process.argv[1]?.endsWith("issue-minimax-entitlement.mjs")) {
  try {
    const opts = parseArgs(process.argv.slice(2));
    if (!opts["private-key"]) throw new Error("Provide a secure Ed25519 private key PEM path");
    const result = issueSignedGrant(opts, readFileSync(opts["private-key"]), new Date());
    // Receipt JSON is copied by the authorized holder into the app; the
    // distributor installs ONLY the public key as build-time trust root.
    process.stdout.write(JSON.stringify(result, null, 2) + "\n");
  } catch (error) {
    process.stderr.write(String(error.message ?? error) + "\n");
    process.exitCode = 1;
  }
}

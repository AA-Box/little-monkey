import { readFile, writeFile } from "node:fs/promises";
import { sign } from "node:crypto";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

export function assertRepresentableNumbers(value) {
  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new Error("manifest contains a non-finite number");
    }
    if (Number.isInteger(value) && !Number.isSafeInteger(value)) {
      throw new Error("manifest contains an integer outside JavaScript's safe range");
    }
  }
  if (Array.isArray(value)) value.forEach(assertRepresentableNumbers);
  else if (value && typeof value === "object") {
    Object.values(value).forEach(assertRepresentableNumbers);
  }
}

function sortedJson(value) {
  if (Array.isArray(value)) return value.map(sortedJson);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.keys(value)
        .sort()
        .map((key) => [key, sortedJson(value[key])]),
    );
  }
  return value;
}

function provenanceSource(value) {
  if (value.local_folder) {
    return {
      local_folder: { canonical_path: value.local_folder.canonical_path },
    };
  }
  if (value.git) {
    return {
      git: {
        remote: value.git.remote,
        commit_sha: value.git.commit_sha,
      },
    };
  }
  if (value.curated_registry) {
    return {
      curated_registry: { registry_id: value.curated_registry.registry_id },
    };
  }
  throw new Error("manifest provenance source is unsupported");
}

function constraint(value) {
  return Object.fromEntries(
    [
      ["minimum", value.minimum],
      ["maximum_exclusive", value.maximum_exclusive],
    ].filter(([, child]) => child !== undefined && child !== null),
  );
}

function signingShape(value) {
  const compatibility = {
    minimum_app_version: value.compatibility.minimum_app_version,
    maximum_app_version_exclusive:
      value.compatibility.maximum_app_version_exclusive ?? null,
    platforms: [...value.compatibility.platforms].sort(),
    architectures: [...value.compatibility.architectures].sort(),
  };
  if (value.compatibility.contract != null) {
    compatibility.contract = constraint(value.compatibility.contract);
  }
  return {
    schema_version: value.schema_version,
    extension_id: value.extension_id,
    version: value.version,
    display_name: value.display_name,
    description: value.description,
    host_api: constraint(value.host_api),
    component: {
      path: value.component.path,
      sha256: value.component.sha256,
    },
    capabilities: value.capabilities.map((entry) => ({
      capability_id: entry.capability_id,
      kind: entry.kind,
      display_name: entry.display_name,
      description: entry.description,
      input_schema: sortedJson(entry.input_schema),
    })),
    permissions: value.permissions.map((entry) => ({
      permission_id: entry.permission_id,
      kind: entry.kind,
      scope: entry.scope,
      reason: entry.reason,
    })),
    config_schema: value.config_schema.map((entry) => ({
      key: entry.key,
      label: entry.label,
      description: entry.description,
      kind: entry.kind,
      required: entry.required,
      default: sortedJson(entry.default ?? null),
      options: entry.options,
      minimum: entry.minimum ?? null,
      maximum: entry.maximum ?? null,
    })),
    secret_slots: value.secret_slots.map((entry) => ({
      slot_id: entry.slot_id,
      label: entry.label,
      description: entry.description,
      auth_header: entry.auth_header ?? null,
      auth_scheme: entry.auth_scheme ?? null,
    })),
    dependencies: value.dependencies.map((entry) => ({
      extension_id: entry.extension_id,
      constraint: constraint(entry.constraint),
    })),
    compatibility,
    publisher: value.publisher,
    provenance: {
      publisher: value.provenance.publisher,
      source: provenanceSource(value.provenance.source),
      source_revision: value.provenance.source_revision,
      build_reproducible: value.provenance.build_reproducible,
    },
    signature: null,
    checksums: sortedJson(value.checksums),
  };
}

async function main() {
  const [bundleArgument, privateKeyPath, trustRootId, keyId] = process.argv.slice(2);
  if (!bundleArgument || !privateKeyPath || !trustRootId || !keyId) {
    throw new Error(
      "usage: node extensions-sdk/scripts/sign-manifest.mjs " +
        "<bundle-directory> <ed25519-private-key.pem> <trust-root-id> <key-id>",
    );
  }

  const manifestPath = path.join(path.resolve(bundleArgument), "extension.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  assertRepresentableNumbers(manifest);

  const payload = Buffer.from(JSON.stringify(signingShape(manifest)));
  const privateKey = await readFile(privateKeyPath, "utf8");
  const signature = sign(null, payload, privateKey);
  manifest.signature = {
    trust_root_id: trustRootId,
    key_id: keyId,
    algorithm: "ed25519",
    signature_hex: signature.toString("hex"),
  };
  await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  process.stdout.write(`${manifestPath}\n`);
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  await main();
}

# Executable extension development and publishing

Little Monkey exposes one developer lifecycle through the existing `monkey extensions` command. The commands reuse the production executable-extension runtime, manifest validation, permission model and M4 registry format; there is no separate development-only package format or unsigned network marketplace.

## Create a project

```sh
monkey extensions init ./my-extension \
  --id dev.example.my-extension \
  --name "My Extension" \
  --template tool \
  --publisher "Example Publisher"
```

Supported templates are `tool`, `channel`, `connector`, `model-provider`, `embedding-provider`, `stt`, `tts`, `realtime-voice`, `web-search`, and `device-provider`.

The generated project is standalone. It owns a copy of the current WIT contract and compiles to a WASI Preview 2 component target without relying on paths inside the Little Monkey source tree.

Prerequisites:

```sh
rustup target add wasm32-wasip2
```

## Development loop

```sh
monkey extensions dev ./my-extension
```

Development mode:

1. builds the Rust guest for `wasm32-wasip2`;
2. updates the component checksum in an ephemeral built manifest;
3. validates the manifest with the production `ExtensionManifest` parser;
4. installs it into `<project>/.little-monkey/dev-profile`, never the normal user profile;
5. displays every requested permission and host binding;
6. starts the real Wasmtime extension runtime;
7. streams guest logs;
8. watches project files and cleanly reinstalls/restarts after changes.

An invocation can be included in the loop:

```sh
monkey extensions dev . --capability echo --input '{"text":"hello"}'
```

Use `--once` for CI or a one-shot development launch.

Development mode may approve the project's declared permissions inside its isolated profile. It does **not** add development keys to global trust roots and does not weaken normal-profile installation policy.

## Conformance tests

`extension.tests.json` is an array of cases:

```json
[
  {
    "name": "echoes text",
    "capability_id": "echo",
    "input": { "text": "hello" },
    "expected": { "echoed": "hello" }
  }
]
```

Run:

```sh
monkey extensions test .
monkey extensions test . --json
```

The command builds the component, creates a fresh isolated test profile, discovers and validates through the production runtime, installs/starts the actual component, invokes each case, records guest logs, then removes the temporary installation. A failing case exits non-zero. Runtime-level timeout, cancellation, fuel, memory, permission, upgrade and rollback invariants remain covered by the executable-extension Rust test suites; project conformance exercises the extension's declared capabilities against that runtime.

`monkey extensions validate TARGET` accepts either a project/bundle path or an installed extension id.

## Deterministic `.lmx` packaging

```sh
monkey extensions pack . --output dist/my-extension.lmx --json
```

Packaging is deterministic and uses the same envelope understood by the native marketplace. It rejects traversal, absolute paths, symlinks, case-colliding names, too many files, oversized entries, and oversized decoded payloads. `extension.json` is carried as the typed manifest and the component is carried in `files_base64`.

The command reports both immutable digests:

- package SHA-256 over canonical `.lmx` JSON;
- manifest SHA-256 over canonical manifest JSON.

## Publisher signing

Publisher keys are Ed25519 PKCS#8 PEM files. They are referenced explicitly; private key bytes are never stored in application settings.

```sh
monkey extensions sign dist/my-extension.lmx \
  --private-key ./keys/publisher-private.pem \
  --trust-root-id example.publisher \
  --key-id release-2026
```

Signing calls `ExtensionManifest::signing_payload()` from the production Rust model before writing the signature. This prevents CLI/runtime canonicalization drift.

The corresponding public key must already be present in the user's executable-extension trust store through the normal trust-root management path. Signing a package does not grant trust.

## Publish to a static M4 registry

Publishing requires an existing M4 registry snapshot and separate publisher/registry private keys:

```sh
monkey extensions publish . \
  --snapshot ./registry/index.json \
  --registry-root ./registry \
  --publisher-private-key ./keys/publisher-private.pem \
  --trust-root-id example.publisher \
  --key-id release-2026 \
  --registry-private-key ./keys/registry-private.pem
```

The command is transactional up to the final filesystem writes and performs the release pipeline in this order:

1. run project conformance;
2. build the component;
3. switch release provenance to the target curated registry;
4. validate the final manifest;
5. pack deterministic `.lmx` bytes;
6. sign the extension manifest;
7. calculate package and manifest digests;
8. add/replace that exact version in the M4 snapshot;
9. increment the anti-rollback registry sequence and refresh/expiry timestamps;
10. sign `RegistrySnapshot::signing_payload()` using the registry key;
11. publish `extensions/<extension-id>/<version>.lmx` and the signed snapshot.

The registry owner private key is never copied into the app. A static HTTPS directory, GitHub Pages, or equivalent static host can serve the resulting files.

## Unified catalog

Settings → Ecosystem → **Discover** is one normalized browse/search surface for:

- declarative packages;
- executable WASM releases from verified M4 snapshots;
- MCP requirements declared by packages.

Every catalog row carries a mandatory type and explicitly describes its security boundary. The UI intentionally does **not** unify installation authority:

- declarative packages route to the package preview/install flow;
- WASM releases route to the native marketplace preview, publisher verification and exact permission grants;
- MCP integrations route to MCP setup/OAuth and remain an external process/server trust boundary.

For an uninstalled WASM release the signed M4 index contains immutable package/manifest digests, not unverified descriptive manifest fields. The unified catalog therefore marks publisher/capability/permission metadata as pending until native review downloads and verifies the signed `.lmx`. It never fills those fields from unsigned renderer/network metadata merely to make a richer marketplace card.

## Release CI example

A release job can run:

```sh
monkey extensions test . --json
monkey extensions pack . --output dist/release.lmx --json
monkey extensions sign dist/release.lmx \
  --private-key "$PUBLISHER_KEY_PATH" \
  --trust-root-id "$TRUST_ROOT_ID" \
  --key-id "$PUBLISHER_KEY_ID" --json
```

For registry publication, mount the registry signing key only in the publishing job and use `monkey extensions publish`; do not commit either private key to the repository.

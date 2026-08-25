# Executable extension marketplace and distribution

Little Monkey distributes executable WASM extensions through the **same signed M4 registry snapshots** used by the package ecosystem. There is no second extension-registry format, trust-root database, or Little Monkey-hosted marketplace service.

## Trust model

Distribution and execution are deliberately two independent checks:

1. **M4 registry trust** answers “which immutable bytes did this verified catalog publish?” Rust verifies the registry Ed25519 signature, trust root/key, expiry, monotonic sequence/rollback protection, package/version digests, and signed revocations.
2. **Executable runtime trust** answers “may these bytes execute with these capabilities?” The existing executable-extension manager independently validates `extension.json`, publisher signature/trust, component/checksum integrity, app/platform compatibility, dependency/capability collisions, and the exact permission approval digest before install/update.

A valid registry signature never grants executable permissions. A valid extension publisher signature never bypasses M4 distribution provenance. Network-delivered unsigned extensions are refused by Marketplace even though the local-folder installer retains its explicit unsigned-development flow.

## Native marketplace authority boundary

The renderer is not a distribution authority. It submits only the signed identity the user selected:

```text
registry source id
+ verified snapshot SHA-256
+ extension id
+ version
```

Rust then resolves that identity from the **currently verified M4 state**. The renderer never supplies the artifact URL, raw `.lmx` bytes, expected package digest, or expected manifest digest.

Native preparation performs this sequence:

1. resolve the configured registry source and require the exact reviewed snapshot digest;
2. require an unexpired verified M4 snapshot;
3. resolve `extension.<extension_id>@<version>` from that snapshot;
4. reject an effective signed revocation;
5. fail closed if another currently verified source advertises the same extension/version with different immutable package or manifest digests;
6. derive the artifact URL from the verified registry location;
7. fetch through the hardened native executable-extension HTTP client with DNS/redirect/size protections;
8. verify the exact `.lmx` SHA-256 against native-resolved M4 metadata;
9. canonicalize and verify the embedded manifest digest;
10. verify extension id/version and curated-registry provenance;
11. enforce bounded path/file/component rules;
12. materialize only under the app-owned marketplace cache and return an **opaque staging lease**.

Preview/install/update receive only that opaque lease. Before mutation Rust re-resolves the lease against current M4 state, so source removal, snapshot replacement, expiry, revocation, or changed signed digests invalidates the preview and requires review again. Successful mutations clean the lease, and stale abandoned leases are pruned.

## Reusing the M4 registry schema

Executable releases use the reserved package-id namespace:

```text
extension.<extension_id>
```

For example, `com.example.echo` is indexed under `extension.com.example.echo`.

No executable bytes are passed through the declarative `PackageStore` (which intentionally rejects `.wasm`). The existing `RegistryPackageVersion` fields are reused as an artifact index:

- `version`: executable extension version;
- `bundle_sha256`: SHA-256 of the exact deterministic `.lmx` UTF-8 bytes;
- `manifest_sha256`: SHA-256 of the canonical executable manifest.

The `.lmx` artifact is served beside a remotely configured static registry:

```text
<registry root>/extensions/<extension_id>/<version>.lmx
```

A signed M4 package/version revocation for the reserved id is also an executable marketplace revocation.

The executable catalog is resolved fail-closed across verified sources. Mirrored sources that advertise identical immutable bytes are deduplicated; if verified sources disagree on the package or manifest digest for the newest version, that release is blocked instead of silently choosing one source or falling back to an older version. The bundled first-party M4 catalog participates in the same trust namespace; it currently contains declarative built-ins rather than remotely downloadable executable artifacts, so executable acquisition still requires a verified source with an artifact location.

## `.lmx` format

`.lmx` is deterministic compact JSON:

```json
{
  "schema_version": 1,
  "manifest": { "...": "extension.json contents" },
  "files_base64": { "component.wasm": "..." }
}
```

`extension.json` is not duplicated in `files_base64`; it is reconstructed from the envelope manifest. Packaging and native staging reject absolute paths, `.`/`..`, drive-prefixed paths, non-ASCII/path tricks, case-colliding paths, excessive file counts, excessive per-file size, and excessive decoded size. The declared component must exist.

The manifest's `provenance.source.curated_registry.registry_id` must match the registry that authorized the artifact. Publisher tooling enforces the same relationship before a package can be added to a snapshot, so durable installed provenance cannot drift from distribution provenance.

## User experience

Settings → Ecosystem → **Extensions** has three views:

- **Discover** lists the newest non-revoked executable releases from currently verified M4 registry sources.
- **Registries** shows M4 registry records, verification state, sequence, snapshot digest, expiry and errors. It intentionally has no separate extension keys or trust-root list.
- **Updates** shows installed-version → catalog-version candidates and an update policy.

Every manual install/update opens the executable runtime's real permission preview. Workspace read/write grants require an explicit canonical host path. High-risk and untrusted-publisher acknowledgements remain explicit runtime approvals.

## Update policy

Policies are `off`, `notify`, and `automatic_safe`.

- `off`: the recurring updater performs no marketplace registry refresh or executable download.
- `notify`: native code refreshes and verifies signed registry metadata and surfaces update candidates, but does not download executable artifacts.
- `automatic_safe`: after native metadata refresh, a candidate may be staged and applied only if all safety conditions remain true.

An update is automatic only when all of these remain true immediately before mutation:

- catalog version is newer and not revoked;
- installed and candidate publisher match;
- installed and candidate runtime trust are `verified`;
- trust-root and key lineage are unchanged;
- runtime compatibility passes and there are no blockers;
- the permission diff does not expand authority;
- no unsigned/untrusted/high-risk acknowledgement is newly required;
- no currently granted permission has a host-only binding.

The last rule is intentional. `PermissionView.binding_label` is display-only; the canonical workspace binding stays host-private. Marketplace never reconstructs authority from that label, so such updates pause for manual review.

Automatic-safe stages an immutable artifact once, previews it, re-previews the same opaque native lease immediately before mutation, then Rust revalidates the lease against current signed M4 authority before the runtime update. A changed snapshot/trust/permission state therefore stops mutation without accepting stale approval authority.

The update coordinator is owned by the primary application window and starts from app lifecycle hydration; opening Settings is not required for update discovery. Secondary/session windows do not start duplicate update loops.

## Publisher workflow

The existing SDK still owns extension development/validation and manifest signing. Distribution adds one script:

```bash
# 1. Build/validate the extension and sign extension.json as usual.
node extensions-sdk/scripts/sign-manifest.mjs \
  extensions-sdk/dist/my-extension publisher-private.pem publisher-root publisher-key

# 2. Deterministically pack it.
node extensions-sdk/scripts/marketplace.mjs pack \
  extensions-sdk/dist/my-extension dist/my-extension.lmx

# 3. Add/copy that immutable artifact into an UNSIGNED M4 registry snapshot.
node extensions-sdk/scripts/marketplace.mjs publish \
  dist/my-extension.lmx public/index.json public/

# 4. Sign the SAME M4 snapshot (no extension-specific index is created).
node extensions-sdk/scripts/marketplace.mjs sign-registry \
  public/index.json registry-private.pem

# Optional publisher-side verification.
node extensions-sdk/scripts/marketplace.mjs verify-registry \
  public/index.json registry-public.pem
```

Before `publish`, the manifest must declare the same curated M4 `registry_id` as the target snapshot. `publish` also refuses to mutate a snapshot whose `signature_hex` is already populated. Increment the M4 snapshot sequence/timestamps using the registry's normal release process, add all desired package/extension entries, then sign once.

The resulting directory is static-hostable on HTTPS, including GitHub Pages/object storage. Little Monkey does not require a central marketplace backend.

## Security boundaries

- Registry verification is Rust-owned M4 logic, not renderer-created trust.
- Registry refresh and executable artifact acquisition are native operations; renderer-provided bytes/hashes/URLs cannot authorize an install.
- Registry URLs cannot smuggle credentials; executable artifacts are content-addressed by the signed snapshot.
- A renderer cannot self-authorize arbitrary bytes by supplying matching hashes because expected hashes are resolved from native verified M4 state.
- Opaque marketplace leases are revalidated at preview and mutation boundaries.
- `.lmx` does not weaken the existing Wasmtime sandbox or permission grant model.
- Marketplace cannot install unsigned network executable code.
- Revoked or expired releases are excluded/refused before materialization.
- Conflicting verified registry identities fail closed.
- Update policy cannot widen permissions or synthesize hidden workspace bindings.
- Registry and publisher signing keys are separate trust domains and can be rotated/revoked independently through their existing stores.

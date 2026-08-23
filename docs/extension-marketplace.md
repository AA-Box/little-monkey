# Executable extension marketplace and distribution

Little Monkey distributes executable WASM extensions through the **same signed M4 registry snapshots** used by the package ecosystem. There is no second extension-registry format, trust-root database, or Little Monkey-hosted marketplace service.

## Trust model

Distribution and execution are deliberately two independent checks:

1. **M4 registry trust** answers “which immutable bytes did this verified catalog publish?” The existing Rust M4 verifier checks the registry Ed25519 signature, trust root/key, expiry, monotonic sequence/rollback protection, package/version digests, and signed revocations.
2. **Executable runtime trust** answers “may these bytes execute with these capabilities?” After download, the existing executable-extension manager independently validates `extension.json`, publisher signature/trust, component/checksum integrity, app/platform compatibility, dependency/capability collisions, and the exact permission approval digest before it installs or updates anything.

A valid registry signature never grants executable permissions. A valid extension publisher signature never bypasses the M4 artifact digest. Network-delivered unsigned extensions are refused by Marketplace even though the local-folder installer retains its explicit unsigned-development flow.

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

The `.lmx` artifact is served beside the static registry:

```text
<registry root>/extensions/<extension_id>/<version>.lmx
```

A signed M4 package/version revocation for the reserved id is also an executable marketplace revocation.

## `.lmx` format

`.lmx` is deterministic compact JSON:

```json
{
  "schema_version": 1,
  "manifest": { "...": "extension.json contents" },
  "files_base64": { "component.wasm": "..." }
}
```

`extension.json` is not duplicated in `files_base64`; it is reconstructed from the envelope manifest. Packaging and installation reject absolute paths, `.`/`..`, drive-prefixed paths, case/Unicode-normalization collisions, symlinks in publisher tooling, excessive file counts, excessive per-file size, and excessive decoded size. The declared component must exist.

Marketplace downloads use Little Monkey's existing `tool_web_fetch` path, so normal network permission and egress controls remain in force. The downloaded text is hashed before parsing and must match the signed M4 `bundle_sha256`. The manifest is separately canonicalized and checked against `manifest_sha256` before the executable runtime gets the materialized source.

Temporary materialization is restricted to the app's `$TEMP/**` capability. It is not workspace or home-directory write authority.

## User experience

Settings → Ecosystem → **Extensions** has three views:

- **Discover** lists the newest non-revoked executable releases from currently verified M4 registry sources.
- **Registries** shows the same M4 registry records, verification state, sequence, snapshot digest, expiry and errors. It intentionally has no separate extension keys or source list.
- **Updates** shows installed-version → catalog-version candidates and an update policy.

Every manual install/update opens the executable runtime's real permission preview. Workspace read/write grants require an explicit canonical host path. High-risk and untrusted-publisher acknowledgements remain explicit runtime approvals.

## Update policy

Policies are `off`, `notify`, and `automatic_safe`.

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

Automatic updates re-download, re-hash, and re-preview immediately before calling `extensions_update`, preventing a stale “safe” result from becoming mutation authority.

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

`publish` refuses to mutate a snapshot whose `signature_hex` is already populated. Increment the M4 snapshot sequence/timestamps using the registry's normal release process, add all desired package/extension entries, then sign once.

The resulting directory is static-hostable on HTTPS, including GitHub Pages/object storage. Little Monkey does not require a central marketplace backend.

## Security boundaries

- Registry verification is Rust-owned M4 logic, not renderer-created trust.
- Registry URLs cannot smuggle credentials; executable artifacts are content-addressed by the signed snapshot.
- `.lmx` does not weaken the existing Wasmtime sandbox or permission grant model.
- Marketplace cannot install unsigned network executable code.
- Revoked releases are excluded/refused before materialization.
- Update policy cannot widen permissions or synthesize hidden workspace bindings.
- Registry and publisher signing keys are separate trust domains and can be rotated/revoked independently through their existing stores.

# Studio local-model license jurisdiction

This describes product-level safeguards, **not** legal advice or a substitute
for authorization from the model rights holder.

## Why this is separate from the engine

Little Monkey's Studio video generation is model-agnostic. Reproducing Apache-2.0
licensed inference algorithms does not authorize use of weights under a different
model license. In particular, MiniMax H3, OpenVDN VDN-H3 and weight derivatives
remain subject to the MiniMax H3 Community License Agreement:

https://huggingface.co/OpenVDN/vdn-minimax-h3-edge/blob/main/LICENSE

That agreement excludes the EU, UK, United States and Republic of Korea from
its ordinary grant. The app **cannot** issue written MiniMax authorization.

Wan 2.2's official model family is Apache-2.0 licensed:
https://github.com/Wan-Video/Wan2.2

## Enforcement points

- Users explicitly declare their actual model-usage region in Studio Library or
  Runtime Hub: EU / UK / US / South Korea / outside all four regions. We do
  not infer territory using IP, OS locale, nationality or timezone.
- Both Studio and Runtime Hub read one profile-scoped declaration.
- The backend detects known MiniMax H3/VDN-H3 provenance from model ID, family,
  source repository and model component paths. It canonicalizes the license,
  overriding a user-authored blank or permissive license on model import and
  existing model registry reads.
- With no territory declaration, these restricted models are not eligible.
  In all four excluded regions the community-license weights are not eligible
  merely from **Accept License**; a build-trusted, independently issued signed
  enterprise entitlement after written MiniMax authorization is required.
- Outside the four regions, the user must still accept the community agreement.
- A claim of separate authorization does not create an override. This requires
  a future independently verified rights-holder grant.
- The backend checks eligibility before **weight downloads** and before **every
  local Studio generation run**. Existing local model files do not bypass this.
- Unknown third-party licenses with territorial exclusions fail closed pending
  a policy specific to those terms, rather than interpreting `other` as a
  universal region.
- Studio model list includes `licenseEligible` and `licenseBlockReason`
  returned by Rust; the frontend exposes reasons but is never the gatekeeper.
- Public/open-license models without known exclusions are unaffected; Wan 2.2
  may be installed through the existing user-managed Studio model workflow.

## Separately licensed enterprise deployments

A separate **written authorization from MiniMax** is required before the
community model can be used inside its excluded territories. Little Monkey
cannot issue those rights. The publisher/distributor may enable enterprise
entitlements only after checking rights-holder documentation.

An enterprise build may embed an approved distributor's Ed25519 public key at
compile time in `LITTLE_MONKEY_MINIMAX_GRANT_ED25519_PUBKEY_B64`. It must
be a real 32-byte Ed25519 verification key of a distributor authorized to
issue model entitlements; it must never be supplied by the end user at
runtime.

The distributor signs the **raw JSON bytes** of a payload:

```json
{
  "audience": "little-monkey",
  "modelFamily": "minimax-h3",
  "licenseReference": "reference-to-verified-written-MiniMax-authorization",
  "holderId": "the-authorized-organization",
  "allowedRegions": ["eu"],
  "notBeforeUnix": 1800000000,
  "expiresAtUnix": 1830000000
}
```

Wrap the UTF-8 payload and its Ed25519 signature as base64 in
`{"payloadBase64":"…","signatureBase64":"…"}`. Both are input to the
**Verify & import signed entitlement** control in Studio model settings.

Rust cryptographically validates the signature against the embedded issuer key,
the fixed application audience/model family, license reference, territory,
validity window and expiry. It revalidates on *every* download, model run,
and profile model status refresh, even when the weight files were imported
manually. A changed region or expired receipt closes the gate immediately.
Users can revoke the receipt from the same control.

### Distributor offline issuance

After checking the rights-holder's actual, signed written authorization and
confirming the application's distribution and end-user scope, generate a
dedicated Ed25519 private key in a secure HSM/keystore or an access-controlled
offline location. **Never commit that key.** The distributor's offline CLI
can create the receipt:

```bash
node scripts/issue-minimax-entitlement.mjs \
  --private-key /secure/issuer-ed25519.pem \
  --license-reference "written-MiniMax-authorization-reference" \
  --holder-id "authorized-customer-id" --regions eu \
  --expires 2027-01-01T00:00:00Z \
  --written-minimax-authorization-reviewed
```

The printed `issuerPublicKeyBase64` is embedded as
`LITTLE_MONKEY_MINIMAX_GRANT_ED25519_PUBKEY_B64` at build time, **not**
supplied by the recipient or loaded from a user-editable config. The
`receipt` JSON is given to the licensed customer. The tool performs no
online MiniMax license lookup and cannot decide whether written
authorization is authentic; that verification remains a real prerequisite
for trusted human/enterprise distribution.

**Standard releases have no trusted entitlement issuer key**, and imported
receipts are rejected. This is intentional. A user's own self-signed JSON,
checkbox or unverifiable PDF does not create MiniMax licensing rights. Issuing a
certificate without examining the upstream written authorization is strictly
outside the application's contract.

## Limitations and security boundaries

This is a **local, self-declared territory policy**; it cannot verify a user's
true location. It is intentionally not a geographic or identity surveillance
system, and cannot stop intentional tampering with the executable or renaming
anonymous local weights. The upstream weights would need signed provenance or
verified hashes for stronger identity binding. Users must provide truthful
information and comply with applicable model and component licenses.

This feature does not download, ship, bundle or mirror MiniMax H3 weights. The
community model remains unavailable in excluded territories by default. A
MiniMax-hosted API may have different terms, which need separate verification
before integrating a hosted provider. This patch does not claim to provide
one or to reproduce the separate Video DeltaNet sampler.

## Acceptance tests

Pure Rust tests exercise previously unlicensed MiniMax provenance, the four
excluded regions, undeclared regions, other-region acceptance, non-MiniMax Wan
provenance and unknown third-party restrictions. UI tests/build verify the
shared declaration surface; CI cannot determine true user geography or certify
model licensing rights.

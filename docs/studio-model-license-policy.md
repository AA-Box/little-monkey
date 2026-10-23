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
  In all four excluded regions the community-license weights are not eligible,
  **even when the user has clicked Accept License**.
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

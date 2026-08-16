# Executable extensions

Executable extensions are third-party WebAssembly Components with an explicit,
reviewable authority envelope. They are separate from Little Monkey's
declarative package ecosystem: a declarative package still rejects `.wasm` and
other executable payloads, while an executable extension uses its own manifest,
store, grants, lifecycle and trust decision.

The host contract is
[`little-monkey:extension@1.0.0`](../src-tauri/wit/little-monkey-extension.wit).
A guest exports `guest.run(capability-id, input-json)` and receives only the
imports in `host`. Inputs and outputs are UTF-8 JSON strings; the host validates
the size and JSON syntax before invocation, while guests should strictly parse
their own declared schema. The host always treats guest output, logs and events
as untrusted data.

## Bundle and manifest

A bundle is a real, non-symlink directory containing `extension.json` and every
file named by `checksums`. The component path must be a relative `.wasm` path,
normally `component.wasm`. Paths cannot escape the bundle. The manifest is
limited to 1 MiB; the component and each checksummed file are individually
limited to 32 MiB. Combined accounting starts with the manifest plus component,
then adds each other checksummed file exactly once toward the strict 64 MiB
combined limit. Unknown manifest fields are rejected.

The schema's main fields are:

- `schema_version`: currently `1`.
- `extension_id` and `version`: stable bounded ID and strict `major.minor.patch`.
- `host_api`: half-open API range (`minimum <= host < maximum_exclusive`).
- `component`: relative path and 64-character hexadecimal SHA-256 of the exact
  component. The runtime accepts either hex case; the supplied tooling writes
  lowercase.
- `capabilities`: unique IDs, kinds and JSON input schemas. Supported kinds are
  `tool`, `channel`, `model_provider`, `embedding_provider`, `stt`, `tts`,
  `realtime_voice`, `web_search`, `web_fetch`, `device_provider` and `connector`.
- `permissions`: requested broker authority. A declaration is not a grant.
- `config_schema`: non-secret string, integer, boolean and select fields. A
  guest reads a declared value as JSON with the bounded `config-get` import.
- `secret_slots`: keychain-backed slots; the guest never receives secret bytes.
- `dependencies`: installed extension IDs and compatible version ranges.
- `compatibility`: app range plus optional platform and architecture allowlists.
- `publisher` and `provenance`: signed attribution and immutable source revision.
- `signature`: `null` for an unsigned local bundle, or Ed25519 evidence.
- `checksums`: relative file paths to SHA-256 digests. It must include the exact
  component path and digest. The runtime accepts either hex case.

All listed capability kinds can be declared, collision-checked, displayed and
invoked through the generic extension broker. Healthy `tool` capabilities are
offered to agent turns; healthy `stt` capabilities can be selected by the
Companion speech pipeline; and healthy `web_search` and `web_fetch`
capabilities can be selected in Web settings. Other provider kinds remain
explicit generic capabilities until their native subsystem selects an adapter.
A signed webhook can invoke its exact granted handler, but guest-emitted events
remain invocation result metadata and are not routed into native Channels.

The three complete templates under [`extensions-sdk/examples`](../extensions-sdk/examples)
are the normative starting point. Keep requested authority minimal; installing
one version never silently grants a permission added by an update.

## Permission scopes

| Kind | Exact scope | Broker behavior |
| --- | --- | --- |
| `network_origin` | Canonical `http://` or `https://` origin, no wildcard/path | DNS/IP policy, method, redirect, header and 4 MiB body limits are host enforced. |
| `workspace_read` | Opaque handle ID | User binds the handle to a real absolute directory; guest paths remain relative. |
| `workspace_write` | Opaque handle ID | Same binding rule, with bounded atomic host writes. |
| `artifact_read` | Exact lowercase artifact SHA-256 or `invocation_inputs` | The latter intersects the grant with artifacts attached to this invocation; each read is capped at 32 MiB. |
| `artifact_write` | `content_v1` | Host content-addresses at most 4 MiB and returns an artifact ID. |
| `model_invoke` | Exact `runtime-id:model-id` | Calls only the managed model broker; no provider credential reaches the guest. |
| `secret_use` | Declared secret slot ID | Host may apply the slot to a brokered HTTP auth header. |
| `device` | Exact `device-id:capability-id` | Routes through the paired-device action queue and artifact checks. |
| `webhook_receive` | Exact capability ID | Allows authenticated daemon ingress to invoke only that handler. |

Workspace-write and device requests are critical risk. Network, secret and
webhook requests are high risk. Installation requires a separate high-risk
confirmation, in addition to trust confirmation, and grants can be omitted to
deny optional authority. Attempts outside the manifest and current grants fail
closed and are recorded by Security Doctor.

## Build and package

The supported guest ABI is WASI Preview 2 / the WebAssembly Component Model.
For Rust, install the target and build a `cdylib`:

```sh
rustup target add wasm32-wasip2
cargo build --manifest-path extensions-sdk/Cargo.toml \
  --target wasm32-wasip2 --release
node extensions-sdk/scripts/package-example.mjs simple-tool "$(git rev-parse HEAD)"
```

The packager writes `extensions-sdk/dist/simple-tool/{component.wasm,extension.json}`
and fills the exact component checksum and canonical provenance path. The same
command accepts `mock-channel` and `mock-stt-provider`. Build from a clean,
pinned dependency graph and retain the source revision and toolchain used for
reproducibility.

Inspect before installing. The preview returns the immutable approval digest,
trust evidence, blockers and permission diff:

```sh
monkey extensions discover extensions-sdk/dist/simple-tool --json
monkey extensions install extensions-sdk/dist/simple-tool \
  --approval-digest <digest-from-discover> --allow-unsigned --json
monkey extensions validate dev.little-monkey.examples.simple-tool --json
monkey extensions enable dev.little-monkey.examples.simple-tool --json
monkey extensions start dev.little-monkey.examples.simple-tool --json
monkey extensions invoke dev.little-monkey.examples.simple-tool echo \
  '{"text":"hello"}' --json
```

Pass `--grant <permission-id>` for each approved non-workspace permission and
`--workspace <permission-id>=/absolute/directory` for a workspace handle. Use
`preview-update` before `update`; the new preview has a new approval digest and
an exact added/removed/unchanged permission diff. Stop a running extension
before update, rollback or uninstall. `logs`, `health`, `stop`, `rollback` and
`uninstall` expose the remaining lifecycle operations. In the desktop app the
same operations are under **Settings > Extensions**.

## Publisher signing

Unsigned bundles are useful for local development and always require explicit
unsigned approval. Distribution should use Ed25519 and a trust root already
provisioned by the user or distributor.

Before signing a distributable bundle, replace the packager's `local_folder`
provenance with an immutable `git` source (`remote` and full `commit_sha`) or a
`curated_registry` source, and set `source_revision` to the same reviewed
revision. This avoids publishing a developer-machine path and makes the signed
attribution independently checkable.

The example templates set `build_reproducible` to `false`. Change it only after
independent clean, locked builds produce the same component digest; a pinned
source revision alone is not proof of reproducibility.

```sh
openssl genpkey -algorithm Ed25519 -out publisher-ed25519.pem
node extensions-sdk/scripts/sign-manifest.mjs \
  extensions-sdk/dist/simple-tool publisher-ed25519.pem \
  example-publisher-root release-2026
```

The signer reconstructs the host's compact signing payload with `signature:
null`, signs those exact bytes and writes lowercase signature hex. Never add or
change a manifest field after signing. Keep the private key outside the bundle
and preferably in a hardware-backed signing service.

The bundled signer accepts finite fractional JSON Schema numbers and integers in
JavaScript's safe-integer range. It rejects non-finite values and integral values
outside that range rather than silently signing rounded data. Publishers needing
the full signed 64-bit integer range should serialize and sign with the Rust
manifest types used by the host.

The active app-data directory may add `extensions-trust-v1.json`. It uses trust
store schema `1`, keyed roots, namespace prefixes ending in `.`, bounded key
validity and optional revocation time. `public_key_hex` is the raw 32-byte
Ed25519 public key, not PEM or SPKI. The root's `publisher`, namespace, key ID
and algorithm (`ed25519`) must exactly match the signed manifest. Trust-store
provisioning is intentionally separate from installation: a bundle cannot add
its own trust root.

```json
{
  "schema_version": 1,
  "roots": {
    "example-publisher-root": {
      "trust_root_id": "example-publisher-root",
      "publisher": "Little Monkey Examples",
      "package_namespaces": ["dev.little-monkey.examples."],
      "keys": {
        "release-2026": {
          "key_id": "release-2026",
          "algorithm": "ed25519",
          "public_key_hex": "<replace-with-64-lowercase-hex-characters>",
          "valid_from_unix_ms": 1767225600000,
          "valid_until_unix_ms": 1893456000000,
          "revoked_at_unix_ms": null
        }
      }
    }
  }
}
```

With OpenSSL, the raw public key is the final 32 bytes of its Ed25519 SPKI DER
output: `openssl pkey -in publisher-ed25519.pem -pubout -outform DER | tail -c
32 | xxd -p -c 256`. Replace the example key and validity window before use.

## Signed webhooks

A capability needs a granted `webhook_receive` permission whose scope is that
exact capability ID. Registration pins the active extension version and the
signature-null signing-payload SHA-256 recorded in trust evidence; an update
cannot silently retarget it. Settings stores the HMAC secret in the OS keychain
and registers the trigger with the existing daemon ingress ledger. Registration
checks the declared capability and exact grant, but does not require the
extension to be running. Delivery-time invocation succeeds only while the
extension is enabled, validated and running, with health `healthy` or
`degraded`; stopping, disabling or protective-disabling it leaves the trigger
registered but prevents delivery from executing until runtime state is restored.
The pin is compared byte-for-byte with the runtime-generated lowercase manifest
digest, so headless registration must copy the value from inspection without
changing its case.

The daemon listens on loopback at `POST /v1/triggers/<trigger-id>` when started
with a webhook port. An operator-managed reverse proxy or tunnel is required
for public ingress. Each request supplies:

- `x-little-monkey-delivery-id`: stable provider delivery ID.
- `x-little-monkey-timestamp-ms`: decimal Unix time in milliseconds.
- `x-little-monkey-nonce`: bounded unique nonce.
- `x-little-monkey-signature`: hex HMAC-SHA256, optionally prefixed `sha256=`.

The signed bytes are `timestamp + "\n" + nonce + "\n" + raw JSON body`. The
default clock-skew window is five minutes and the body limit is 1 MiB. Verified
deliveries invoke the handler with `{trigger_id, delivery_id, received_at_ms,
payload}`. Delivery ID/body conflicts are rejected; exact redelivery is deduped.
The invocation ID and result are durable, so a crash between guest completion
and ledger submission replays the cached result instead of repeating effects.

For headless registration, pass the extension target to the daemon trigger
command and read the secret from an environment variable, never argv:

```sh
export EXAMPLE_WEBHOOK_SECRET='<secret>'
monkey daemon trigger add-webhook example-hook \
  --extension-id dev.little-monkey.examples.mock-channel \
  --extension-handler-id receive --extension-version 1.0.0 \
  --extension-manifest-sha256 <manifest-sha256-from-inspect> \
  --secret-env EXAMPLE_WEBHOOK_SECRET
unset EXAMPLE_WEBHOOK_SECRET
```

## Security model and limits

Wasmtime runs components with fuel, epoch interruption, a 30-second wall-clock
timeout, cancellation, 64 MiB linear-memory limit, bounded tables/instances and
restricted WASI Preview 2. The host supplies no preopened directories,
environment variables or arguments and disables raw TCP, UDP and name lookup.
All meaningful effects go through the declared host brokers. Three consecutive
runtime failures trigger protective disable; validation, trust and approval
errors do not count as guest crashes.

Secrets remain in the OS keychain and are applied by the host. Workspace roots
are host-only bindings. Artifact IDs are content digests. HTTP uses exact
origins and refuses a redirect to a different origin. Model and device access
use their existing managed authorization paths. Bounded guest telemetry is
retained in the extension runtime logs. Security Doctor reports trust,
incompatibility, elevated combinations, undeclared attempts, degradation and
protective disable without instantiating guest code.

Current limits are intentionally explicit:

- Components cannot contribute native UI or scripts; use declared capabilities
  and the host Settings surface.
- There is no public extension registry or automatic trust-root distribution.
  Local folders and manually provisioned publisher roots are the supported
  distribution path.
- Host builds are covered on macOS, Linux and Windows. `aarch64` and `x86_64`
  may be declared; Windows ARM64 remains dependent on Wasmtime's Tier 3 host
  support and needs release-hardware validation rather than assuming parity
  from cross-compilation alone.
- Public webhooks require operator-managed TLS/tunneling because the bundled
  listener is deliberately loopback-only.

## Capability contracts

Every capability below is consumed by the subsystem that owns it, chosen the
same way that subsystem's built-in providers are chosen, and subject to the
same lifecycle: a provider disappears the moment its extension is disabled,
stopped, unhealthy, protectively disabled or uninstalled, and every invocation
re-checks that the *same installation* still owns the capability id the
selection recorded.

Two shapes recur throughout, and both are load-bearing:

- **Ownership is recorded, not inferred.** Every persisted selection stores the
  owning extension id alongside the capability id. Resolving by capability id
  alone would let a later install inherit an uninstalled provider's name.
- **An artifact id proves nothing on its own.** The artifact store is
  content-addressed and shared, so a guest that names an id has not shown it
  owns the content. Every consumer that reads audio, a document or an
  attachment out of an extension's answer checks the id against the set of
  artifacts the host recorded *that invocation* writing.
- **Naming an artifact is not being granted it.** The reverse direction has the
  same rule. An id inside the JSON a guest receives — an event, a message, a
  device request, a model reply — tells the guest which of its grants to read
  and confers nothing. Authority is attached separately, in code, by the host
  subsystem that created the bytes: a session step carries the grants its
  trusted call site listed beside the event, and a device asked to play a
  stored clip gets the content the run ledger links to that run, not the id
  the caller wrote. Nothing derives a grant from parsing JSON.

### Tool

Declared `tool` capabilities become agent tools named
`ext__<extension>__<capability>`. The definition is resolved once per turn from
runtime state, the model's arguments are data and never authority, the result
is bounded and treated as untrusted content, and cancelling the turn cancels
the invocation.

### Channel

A channel extension is a real provider account of kind `extension`, configured
with `extension_id` and `capability_id`. The adapter calls it with an `op`:

| `op` | Input | Output |
| --- | --- | --- |
| `probe` | `account_id`, `settings` | `{ok, identity?, error?}` |
| `poll` | `account_id`, `settings`, `cursor` | `{messages, cursor?}` |
| `send` | `account_id`, `settings`, `conversation_id`, `thread_id?`, `text`, `attachments`, `reply_to_provider_id?`, `idempotency_key` | `{status, provider_message_id?, error?, retry_after_ms?}` |

A durable webhook delivery arrives instead as
`{trigger_id, delivery_id, received_at_ms, payload}` and answers with
`{account_id, messages}` — the same message vocabulary, plus which of this
extension's accounts they belong to.

One inbound message is `{provider_event_id, conversation_id, conversation_kind,
thread_id?, conversation_title?, sender_id, sender_label?, sender_is_self?,
sender_is_bot?, text, mentions_self?, reply_to_provider_id?, received_at_ms?,
attachments[]}`. `provider_event_id` must be stable across a redelivery and
never random: the durable dedupe key is the account plus that value.

`status` is exact. `sent` retires the outbox row, `retry` re-attempts it, and
anything else — including a status this build does not recognise — parks the
row for reconciliation. A provider that cannot prove its request never left
must not say `retry`, because retrying a request that did arrive sends the
message twice. An invocation that fails outright is treated the same way, since
a guest can complete its HTTP request and then trap.

Attachments move as artifacts: an outbound attachment is granted to the guest
for exactly that invocation, and an inbound one is declared as an https URL the
host downloads through the same hardened client and size caps every other
provider uses. A guest never hands over attachment bytes directly.

### Model provider

Discovered providers appear in the normal provider list with the id
`extension:<extension-id>:<capability-id>`. They have no base URL and no key:
credentials live in the extension's own secret slots.

A model listing is a one-shot call with `{"query": "models"}` answering
`{models: [{id, vision?, context_length?, tool_calling?}]}`.

A completion is a **session** (see below). The host opens it with
`{model, messages, tools, effort}` and pulls until the guest reports `done`.
Each step returns normalized events, which the host renders into the one stream
shape this app already parses:

- `{kind: "text_delta", payload: {text}}`
- `{kind: "tool_call", payload: {index, id?, name?, arguments?}}`
- `{kind: "usage", payload: {prompt_tokens, completion_tokens, total_tokens?}}`
- `{kind: "finish", payload: {reason}}`
- `{kind: "error", payload: {message}}` — the upstream failed, as distinct from
  the guest failing

No provider-specific JSON crosses into the app: the extension does the
provider-shaped parsing inside its own sandbox.

### Embedding provider

A knowledge stack may record `backend: "extension"` with the owning
`extension_id` and the capability id as its model. Each batch is
`{model, input_kind, dimensions, texts}` and answers `{vectors: [[…]]}`. The
host checks the count, the width against the stack's pinned dimension, and that
every value is finite, then L2-normalizes exactly as it does for a local
server. A width that does not match hard-fails to "reindex required" rather
than mixing two embedding spaces.

### Speech to text

Selected in Companion settings. The host imports the audio as a trusted
artifact and grants the guest that one artifact; the input is
`{artifact_id, language}` and the answer is a bounded transcript with optional
speaker segments. An artifact id appearing in model-generated JSON confers no
access.

### Text to speech

Selected in Companion settings, and used by both the desktop's speak action and
a call's outbound audio. Input is `{text, voice, format: "wav"}`; the answer is
`{artifact_id, media_type}` naming an artifact the guest wrote during that
invocation. The host verifies ownership, the media type, the size and the RIFF
header before anything plays it. A filesystem path is never accepted.

### Realtime voice

A live call is a session, not a pair of one-shot calls, so it gets one. Opened
with `{sample_rate, encoding, first_event}` and driven with:

- `{kind: "caller_audio", artifact_id, byte_len}` → a `transcript` event
- `{kind: "agent_text", text}` → an `audio` event naming an artifact the guest
  wrote this step

The caller's clip is published by the host and granted to that one step, so a
capability declaring `artifact_read` over `invocation_inputs` reads the PCM and
nothing else. The grant is attached beside the event rather than derived from
it: the same event with no grant is refused at `artifact-read`.

The session is closed when the call ends. Updating, disabling or uninstalling
the extension mid-call fails the call rather than handing the rest of the
conversation to different code.

### Web search and fetch

Selected in Web settings. Search takes `{query, count}`; fetch takes
`{url, max_chars, start_index}`. Both run under the extension's exact origin
grants through the app's hardened egress, with the same SSRF protections,
redirect rules and body caps as the built-in providers.

### Device provider

A device provider advertises devices with `{"query": "devices"}` answering
`{devices: [{id, name?, actions: [...]}]}`, where each action is one of the
device vocabulary the `device_action` tool already accepts. Device ids are
namespaced by the host as `ext:<extension-id>:<capability-id>:<device-id>`, so
one extension can never name another's device.

An action is `{"query": "action", device_id, action, arguments}` — with the
action resolved by the host from a validated capability, never taken as a free
string — answering `{result?, artifact_id?, media_type?, error?}`. An
undeclared action or an unadvertised device is refused before the sandbox
starts.

`audio_playback` may name a stored clip. The host resolves it against the run
that owns it — the same link the signed artifact route checks for a paired
phone — and the guest receives that content's id, already granted for this
invocation, with no `run_id` beside it. A clip the ledger does not tie to that
run reaches no sandbox at all.

### Connector

A connector account of provider `extension` records which capability it is
bound to. A refresh asks for
`{"query": "documents", scope, cursor, page}` and receives
`{documents: [{id, artifact_id, canonical_uri?, media_type?, modified_unix_ms?}],
next_page?, cursor?}`. Document bodies are artifacts the guest wrote during the
same invocation; `cursor` is an opaque incremental token stored on the source
exactly like a commit SHA or an ETag map, so a connector that can sync
incrementally does.

## Sessions

Some subsystems are not request/response. A live call and a streaming
completion both run for as long as they run, exchanging chunks in both
directions.

The guest ABI stays one-shot anyway. A session is *host* state: the host owns
the session table, the identity, the sequence number, the deadline and the
guest's own scratch state, and each step is an ordinary sandboxed invocation
carrying the next event. Every property a single invocation has — fuel, memory
ceiling, wall timeout, cancellation, trap isolation, immutable version binding
— therefore applies to every step, and a guest holds no resource across the
gap.

A step arrives as:

```json
{
  "session": { "id": "…", "seq": 3, "phase": "open|event|close", "capability_id": "…" },
  "state": <whatever the guest returned last step, or null>,
  "event": { … }
}
```

and answers with `{"events": [{"kind": "…", "payload": { … }}], "state": …,
"done": false}`.

Bounds: at most 16 open sessions across all extensions, 64 KiB of guest scratch
state, 256 KiB per event, 256 events per step, and a one-hour session lifetime.
A step that fails ends the session — half a call is worse than a clean failure —
and a guest that reports `done` has its session closed by the host. Sessions
are in-memory only: a live call cannot survive a restart, so persisting one
would only leave a row that outlived the thing it described.

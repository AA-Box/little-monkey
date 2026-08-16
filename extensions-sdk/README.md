# Little Monkey executable-extension SDK

This workspace contains guest-side Rust helpers and three deliberately small
WebAssembly Component Model examples. The examples generate bindings from the
canonical [`little-monkey:extension@1.0.0`](../src-tauri/wit/little-monkey-extension.wit)
WIT contract; there is no copied contract that can drift from the host.

## Build and package an example

Install a Rust toolchain that provides the `wasm32-wasip2` standard library,
then run from the repository root:

```sh
rustup target add wasm32-wasip2
cargo build --manifest-path extensions-sdk/Cargo.toml \
  --target wasm32-wasip2 --release
node extensions-sdk/scripts/package-example.mjs simple-tool "$(git rev-parse HEAD)"
```

Replace `simple-tool` with `mock-channel` or `mock-stt-provider`. The packager
copies the component into `extensions-sdk/dist/<example>/`, hashes the exact
bytes, fills both checksum fields and records the canonical bundle path. The
result is an unsigned local-development bundle. Do not edit `component.wasm`
after packaging.

Each example is intentionally standalone:

- `simple-tool` is an agent tool, requesting no authority at all.
- `mock-channel` is a complete messaging provider: it probes, polls, sends and
  normalizes durable webhook deliveries, in the exact shapes the channel core
  consumes. What it returns goes into the same normalized path Telegram and
  Slack go through — durable events, dedupe, access policy, session mapping,
  the outbox and its retry semantics. It talks to no network, so it builds and
  runs offline for anyone; a real provider replaces two marked functions with
  `host::send_http` calls against origins its manifest grants.
- `mock-stt-provider` demonstrates input-artifact delegation, declared
  non-secret configuration and bounded provider output. After installation,
  grant, enablement and start, it can be selected as the Companion STT backend;
  it can also be invoked manually from the CLI.

Every capability kind has a native consumer that selects it the way that
subsystem selects its built-in providers — tools in the agent turn, channels in
Channels, models and embeddings in their provider lists, speech and realtime
voice in Companion, search and fetch in Web, devices in device routing and
connectors in Knowledge sources. Nobody has to invoke an extension by hand to
use it. The per-capability request and response shapes are documented in
[`docs/executable-extensions.md`](../docs/executable-extensions.md#capability-contracts),
along with the host-owned session protocol that streaming completions and
realtime voice run on.

The Settings manual invoke form cannot attach artifact IDs, so a hand-driven
STT call needs `monkey extensions invoke` with `--artifact`.

Use `little-monkey-extension-sdk` for strict JSON parsing, bounded output and
capability dispatch. Generate host bindings directly from a pinned copy of the
canonical WIT when developing outside this monorepo. The complete manifest,
permission, signing and webhook reference is in
[`docs/executable-extensions.md`](../docs/executable-extensions.md).

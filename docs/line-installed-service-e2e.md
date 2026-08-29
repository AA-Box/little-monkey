# LINE installed-service acceptance

Literal path:

`independent LINE user → LINE signed webhook → public HTTPS callback → installed resident daemon → native credential store → durable ingress → real daemon agent → send_message → durable outbox → LINE push API → real sentMessages.id → independent LINE client observes exact reply`

The harness never invokes the LINE adapter directly and never fabricates a webhook delivery.

## Provider setup and observation

LINE's official Messaging API exposes GET/PUT/test operations for the channel webhook endpoint. The harness verifies that the channel has webhook delivery enabled, requires the configured endpoint to equal the generated Little Monkey callback, and asks LINE itself to test that endpoint. With `LINE_E2E_MUTATE_WEBHOOK=1`, it may temporarily replace and later restore the endpoint; use that only on a disposable/test channel.

LINE currently returns `sentMessages[].id` for a successful push, and Little Monkey persists that provider id on its outbound event. That still proves only provider acceptance. LINE exposes no per-user delivery receipt or API for reading a consumer's inbox, so the external user must copy/paste the exact received reply from the real LINE client. The harness compares it byte-for-text (apart from the terminal newline).

## Environment

```text
LITTLE_MONKEY_REQUIRE_LINE_INSTALLED_SERVICE_E2E=1
LINE_E2E_CHANNEL_SECRET=<Messaging API channel secret>
LINE_E2E_CHANNEL_ACCESS_TOKEN=<channel access token>
LINE_E2E_PUBLIC_BASE=https://<public-host-routing-to-this-machine>
LINE_E2E_WEBHOOK_PORT=38444
LINE_E2E_EXTERNAL_USER_ID=<LINE userId expected on the inbound webhook>
```

Optional, test channel only:

```text
LINE_E2E_MUTATE_WEBHOOK=1
```

Build the real CLI first, then run:

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example line_installed_service_e2e
```

A pass requires the installed service PID/heartbeat, LINE's own successful webhook reachability test, a signed inbound marker from the expected user, durable ingress and job ids, the marker crossing the real model interface, a production `send_message` tool dispatch, a provider-named outbound event, and exact observation of the generated reply in the independent LINE client.

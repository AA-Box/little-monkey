# WhatsApp installed-service acceptance

This is the literal WhatsApp Business Cloud API acceptance for the messaging subsystem. It is deliberately stronger than an adapter smoke test:

`real independent WhatsApp user → Meta signed webhook → public HTTPS callback → installed resident daemon → native credential store → durable channel ingress → real daemon task/agent → send_message tool → durable outbox → Meta Cloud API → exact wamid → Meta signed delivered/read receipt`

The deterministic model server is only the model backend. It cannot write the outbox or call WhatsApp. The production agent must see the real inbound marker, choose the production `send_message` tool, and the installed daemon must drain the resulting outbox.

## Why the sender step is interactive

Meta's Cloud API is a business API. It does not expose an API that can impersonate an unrelated consumer WhatsApp account. The acceptance therefore asks a real independent WhatsApp identity to send one unique marker from WhatsApp itself. The remainder is automatic.

Do not replace that boundary with a fabricated webhook POST, a second call to the adapter, or a test-owned `send()` call. Those prove protocol parsing, not the provider path.

## Delivery evidence

A successful `POST /<phone-number-id>/messages` is not treated as delivery. The harness reads the provider message id (`wamid...`) from Little Monkey's durable outbound event and then waits for the production webhook listener to persist either:

- `status:<wamid>:delivered`, or
- `status:<wamid>:read`.

Those rows are produced only after the WhatsApp adapter verifies Meta's real `X-Hub-Signature-256` callback and normalizes its `statuses[]` object. A `sent` receipt is insufficient.

## Public callback

`WA_E2E_PUBLIC_BASE` must be a public HTTPS origin already routed to `127.0.0.1:$WA_E2E_WEBHOOK_PORT` on the test machine. The harness configures that origin through the production `channels set-public-url` command, and the installed daemon binds the loopback webhook port through its real service configuration.

The safe default is to preconfigure the WABA's callback override to the exact URL Little Monkey advertises:

`$WA_E2E_PUBLIC_BASE/v1/channels/<generated-account-id>`

Because the account id is generated for every run, a fully automatic run needs a **disposable WABA**. Set `WA_E2E_MUTATE_WABA_SUBSCRIPTION=1` only for such a WABA. The harness then uses Meta's official `/{WABA-ID}/subscribed_apps` API to create the baseline subscription, install the per-WABA callback override, verify the override, and unsubscribe during cleanup. Do not enable mutation on a production WABA; an interrupted run or provider error could alter its webhook subscription.

## Required environment

```text
LITTLE_MONKEY_REQUIRE_WHATSAPP_INSTALLED_SERVICE_E2E=1
WA_E2E_ACCESS_TOKEN=<long-lived system-user/user access token with the needed WhatsApp scopes>
WA_E2E_APP_SECRET=<Meta app secret used to verify X-Hub-Signature-256>
WA_E2E_VERIFY_TOKEN=<webhook verification token>
WA_E2E_PHONE_NUMBER_ID=<Little Monkey business phone-number id>
WA_E2E_WABA_ID=<WhatsApp Business Account id>
WA_E2E_PUBLIC_BASE=https://<public-host-routing-to-this-machine>
WA_E2E_WEBHOOK_PORT=38443
WA_E2E_EXTERNAL_WA_ID=<digits-only wa_id of the independent sender>
WA_E2E_BUSINESS_DISPLAY_NUMBER=<number/name the human should send the marker to>
```

Optional, disposable WABA only:

```text
WA_E2E_MUTATE_WABA_SUBSCRIPTION=1
```

## Run

Build the same production CLI that the OS service will install, then run the example:

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example whatsapp_installed_service_e2e
```

The harness creates an isolated Little Monkey profile, stores the WhatsApp credential only through `channels set-token` stdin, installs the real user service with the webhook port, proves the service survives a stop/start with a different resident PID, and verifies the WABA subscription points at the generated callback URL. It then prints the one marker the independent WhatsApp identity must send.

A pass requires all of the following in one run:

1. the authenticated inbound event is durable and belongs to the expected external `wa_id`;
2. that event has both a durable ingress id and real daemon job id;
3. the real model request contains the marker and exposes `send_message`;
4. the agent returns after the tool dispatch, proving the tool result crossed the real agent loop;
5. the generated reply becomes a durable outbound event addressed to that same external `wa_id` with a real Meta `wamid`, never `local:*`;
6. Meta sends a signed `delivered` or `read` webhook for that exact `wamid` and the installed daemon persists it.

The harness removes its isolated profile/account/service on exit. In disposable-WABA mutation mode it also attempts to unsubscribe the WABA. Provider-side cleanup is best effort, so the disposable WABA should still be inspected after an interrupted run.

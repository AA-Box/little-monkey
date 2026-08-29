# Google Chat installed-service acceptance

The literal acceptance path is:

`real Google Chat user → Google-signed Chat interaction JWT → public HTTPS callback → installed resident daemon → native credential store → durable channel ingress → real daemon agent/model → send_message → durable outbox → Google Chat API → exact message resource → independent user reads that exact message`

The deterministic model server is only a model backend. It cannot write the outbox or call Chat. The production task/agent must receive the real marker and dispatch the production `send_message` tool.

## Required setup

Use a disposable/test Google Chat app configured with:

- **Connection settings:** HTTP endpoint URL.
- **Authentication audience:** Project Number.
- A service account whose JSON credentials can send as the Chat app.
- A test space containing both the app and the independent observing user.

The harness creates a fresh Little Monkey account id, so its callback path is generated per run. It prints the exact callback URL. Set the test Chat app's HTTP endpoint to that URL before continuing. Do not substitute a locally fabricated request: the inbound JWT must be minted and signed by Google Chat itself.

## Required environment

```text
LITTLE_MONKEY_REQUIRE_GOOGLE_CHAT_INSTALLED_SERVICE_E2E=1
GCHAT_E2E_PROJECT_NUMBER=<Cloud project number used as Chat Authentication Audience>
GCHAT_E2E_BOT_USER_NAME=users/<Chat app user resource>
GCHAT_E2E_SERVICE_ACCOUNT_EMAIL=<service account client_email>
GCHAT_E2E_SERVICE_ACCOUNT_PRIVATE_KEY=<PEM private key>
GCHAT_E2E_PUBLIC_BASE=https://<public-host-routing-to-this-machine>
GCHAT_E2E_WEBHOOK_PORT=38445
GCHAT_E2E_SPACE_NAME=spaces/<test-space>
GCHAT_E2E_EXTERNAL_USER_NAME=users/<independent-user>
GCHAT_E2E_EXTERNAL_USER_ACCESS_TOKEN=<user OAuth token with message-read access>
```

The user access token is only the independent observer. It is never installed into Little Monkey and never used by the production adapter.

## Run

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example google_chat_installed_service_e2e
```

The harness creates an isolated Little Monkey profile, stores the app service-account credential only through `channels set-token` stdin, validates the production credential/JWKS path with `channels probe`, installs the actual user service with its webhook listener, proves the resident PID survives a stop/start as a new process, and prints the exact generated callback URL.

After the test Chat app is configured to that URL, the independent user sends the one marker printed by the harness in the configured space.

A pass requires all of the following in one run:

1. the inbound event is authenticated by the production Google Chat JWT verifier and persists with the expected `users/...` sender;
2. the event becomes a durable ingress and real daemon job;
3. the real agent/model request contains the exact marker and exposes `send_message`;
4. the agent returns to the model after tool execution;
5. the generated text becomes a durable outbound event with a real `spaces/.../messages/...` provider resource name;
6. the independent Workspace user's OAuth token successfully `GET`s that exact message resource and its text exactly matches the generated reply.

A successful service-account send alone is not accepted as provider-side observation.

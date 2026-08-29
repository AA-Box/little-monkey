# Telegram installed-service acceptance

Literal acceptance path:

`operator-owned BotFather bot/token → normal Little Monkey account setup → OS keychain → installed resident daemon → Telegram long poll → previously unknown real Telegram user → pairing challenge → production sender approval → durable accepted ingress → real daemon agent/model → send_message → durable outbox → Telegram Bot API → provider message id → Telegram forwardMessage returns the exact generated reply text`

No Telegram identity belongs to this repository or to Little Monkey. Every run uses a bot the operator created themselves with BotFather, exactly like a normal public installation.

## Required Telegram setup

1. In Telegram, create a bot with `@BotFather` using `/newbot`.
2. Keep that bot token private.
3. From a separate real Telegram user account, open a direct chat with the bot and press Start if Telegram requires it.
4. Do **not** preconfigure that user's numeric Telegram id in the harness. The point of this acceptance is to prove that an arbitrary user is discovered through the real pairing path and can then be approved by the installation owner.

The harness never constructs `TelegramAdapter` and never calls `getUpdates` or `sendMessage` itself. The installed daemon exclusively owns production inbound/outbound transport. The harness uses the Bot API only after Little Monkey has sent the reply, to ask Telegram for independent provider-side evidence: `forwardMessage` returns a copy containing the original message text, which is then deleted best-effort.

## Environment

```text
LITTLE_MONKEY_REQUIRE_TELEGRAM_INSTALLED_SERVICE_E2E=1
TELEGRAM_E2E_BOT_TOKEN=<token from your own BotFather bot>
```

No bot username, Telegram user id, chat id, or destination id is configured. Those values are discovered from real provider traffic during the run.

## Run

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
LITTLE_MONKEY_REQUIRE_TELEGRAM_INSTALLED_SERVICE_E2E=1 \
TELEGRAM_E2E_BOT_TOKEN='123456:replace-with-your-own-token' \
cargo run --locked --manifest-path src-tauri/Cargo.toml --example telegram_installed_service_e2e
```

The harness creates an isolated Little Monkey profile and deterministic model endpoint, then performs the same setup a public installation does:

1. `channels add telegram` creates the account with no bundled identity or secret.
2. `channels set-token` receives the operator's token on stdin and writes it through the production native credential path.
3. Direct messages are set to `pairing`; the route targets the real daemon agent runtime.
4. The actual user service is installed and must report a distinct resident PID plus fresh Telegram-connected health.
5. The service is stopped and started again. The new resident PID must differ and the restarted daemon must independently recover the keychain credential and reconnect to Telegram.
6. The harness prints a unique **pairing marker**. A previously unknown real Telegram user sends it to the bot.
7. The installed daemon must record that exact provider delivery as `challenged`, including the provider-derived sender id. The harness approves that discovered sender using `monkey channels approve` — the same daemon path the desktop UI uses.
8. The harness prints a second unique **execution marker**. The same Telegram user sends it.
9. That exact message must become one accepted durable ingress with an ingress id and job id.
10. The real daemon agent sends the execution marker to the configured model interface, receives a `send_message` tool call, dispatches the production tool, and returns to the model after tool execution.
11. The generated text must become a durable outbound event carrying Telegram's real numeric message id.
12. Finally, the harness calls Telegram's `forwardMessage` only as an observer, forwarding that exact provider message back into the same chat. Telegram's returned forwarded message must contain the exact generated reply text. The temporary forward is deleted best-effort.

A successful `sendMessage` HTTP response by itself is not enough. A local outbox row by itself is not enough. Compilation is not enough. The provider-side text returned by Telegram must exactly match what the production agent generated.

## Public-app invariant

This test is intentionally incompatible with a repository-owned shared bot. If a developer, customer, or downstream distributor can create their own BotFather bot, paste its token into Little Monkey, approve their own Telegram users, restart the machine/service, and complete this same path, the integration satisfies the public self-provisioning model.

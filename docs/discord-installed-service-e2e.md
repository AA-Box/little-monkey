# Discord installed-service acceptance

Literal acceptance path:

`independent operator-owned Discord bot -> Discord REST create-message -> Discord Gateway v10 -> installed Little Monkey user service -> production Discord adapter -> durable access decision -> provider-derived sender approval -> durable ingress/job -> real daemon agent/model -> send_message -> durable outbox -> production Discord REST send -> Discord -> independent bot reads the exact reply through Discord REST`

This is deliberately a two-bot test. It does **not** automate a human Discord account or use a self-bot. Both Discord applications, the guild, and the test channel belong to the operator running the acceptance; Little Monkey owns none of them.

## Why approval, not a fake pairing challenge

Little Monkey intentionally permits pairing only in direct messages. A guild channel has no private place to deliver a pairing code, so an unknown group sender under `pairing` is treated as not allowed. The Discord acceptance therefore tests the real group access model:

1. the account starts with groups on `allow_list` and `mention_only` activation;
2. the independent bot posts a real message mentioning the production bot;
3. the installed daemon must durably record it as `ignored`, with no ingress/job and no model request;
4. the harness takes the sender id from that real Gateway event and approves it through `monkey channels approve`;
5. the same external Discord identity posts the execution marker;
6. only that second message may become a durable ingress/job and reach the agent.

That proves the provider-derived identity and approval boundary without weakening the production policy for the sake of a test.

## Discord setup

Create two bot applications in your own Discord Developer Portal and install both into a dedicated test server/channel.

The **bot under test** needs access to the channel and permission to send messages. Enable the privileged **Message Content Intent** for it; the production adapter requests Gateway intents for guilds, guild messages, direct messages, and message content, and a disallowed intent is a permanent Gateway failure rather than something the daemon retries forever.

The **external bot** needs enough channel permission to:

- view the test channel;
- send messages;
- read message history, because it independently fetches the final production reply by Discord message id.

Use a quiet dedicated text channel. A thread also works: the harness verifies that Little Monkey's normalized thread target resolves back to the exact Discord channel id supplied for the test.

## Environment

```text
LITTLE_MONKEY_REQUIRE_DISCORD_INSTALLED_SERVICE_E2E=1
DISCORD_E2E_BOT_TOKEN=<token for the bot under test>
DISCORD_E2E_EXTERNAL_BOT_TOKEN=<token for the independent sender/observer bot>
DISCORD_E2E_CHANNEL_ID=<shared test channel or thread id>
```

The two tokens must identify different Discord bots. Only `DISCORD_E2E_BOT_TOKEN` is stored through Little Monkey's production `channels set-token` path and then read by the installed daemon from the native credential store. The external token never enters Little Monkey configuration; it belongs only to the independent provider-side actor.

## Run

From the repository root:

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
cargo run --locked --manifest-path src-tauri/Cargo.toml --example discord_installed_service_e2e
```

Run it in a normal logged-in user session where the installed Little Monkey service can access the same native credential store as the CLI (Keychain on macOS, Credential Manager on Windows, or the user's Secret Service on Linux).

## What a pass proves

A pass requires all of the following in one run:

1. both arbitrary operator-supplied bot tokens are independently accepted by Discord `users/@me`, and the identities are different;
2. the production bot token is written through `channels set-token`, never handed to a test-only adapter constructor;
3. the actual OS user service is installed, reports a resident PID, and the production Discord Gateway reaches a fresh `connected` state;
4. the service is stopped and started again, a different resident PID appears, and the restarted production Gateway reconnects successfully;
5. the independent bot posts a real Discord message that mentions the production bot;
6. the production Gateway records exactly one ignored event for that provider message while the sender is unapproved, with no ingress/job and no model traffic;
7. the sender id normalized by the production Gateway matches the independent bot identity returned by Discord;
8. that provider-derived sender id is approved through the normal Little Monkey CLI path;
9. the independent bot posts a second real Discord message and its Discord message id becomes exactly one accepted durable inbound event with ingress and job ids;
10. the production daemon's real agent/model loop sees the marker, receives the real `send_message` tool, executes it, and completes the tool-result turn;
11. the generated reply becomes exactly one durable outbound row carrying a real Discord provider message id rather than a `local:` placeholder;
12. the independent external bot fetches that message from Discord by channel/message id and verifies both the production bot author id and the exact generated reply text.

The deterministic loopback model fixture is the only fake component. It sits behind the ordinary recipe/model interface and cannot inject a provider reply; the reply exists only if the installed daemon runs the normal agent/tool/outbox/provider path.

## Evidence status

The pull-request workflow compiles this harness on every relevant change. That compile is **not** counted as a live Discord acceptance. A demonstrated 5/5 requires running the command above with two real Discord bots and a real Discord channel and recording a successful run.

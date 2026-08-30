# Slack installed-service acceptance

Literal acceptance path:

`independent operator-owned Slack bot → Slack Web API → Slack Events API / Socket Mode → installed resident daemon → target app credential from OS keychain → durable deny/approval/ingress → real daemon agent/model → send_message → durable outbox → target app Web API → Slack → independent bot reads exact generated reply`

This is a public-app acceptance. Little Monkey owns no Slack workspace, app, bot, or token. The operator supplies two apps/bots in a disposable/test workspace:

1. **Target Little Monkey app** — configured in Little Monkey. It needs a bot token (`xoxb-...`) and a Socket Mode app token (`xapp-...`). Both are stored together through `channels set-token` as the production JSON secret and are never used directly by the harness after that write.
2. **External observer bot** — never configured in Little Monkey. Its bot token is used only by the harness to post a real Slack message and later read Slack's copy of the generated reply.

Both bots must be members of the same test channel.

## Target app configuration

Enable **Socket Mode** and subscribe the app to message events appropriate for the chosen channel (for a public channel, `message.channels`; use the matching Slack event for other conversation types). Grant the target bot the scopes required for receiving/posting in that channel, including `chat:write` and the history scope Slack requires for that conversation type. Install/reinstall the app after changing scopes or event subscriptions.

The app-level token must have the permission Slack requires for Socket Mode connections (`connections:write`).

## External observer configuration

The external bot must be able to:

- post into the shared test channel (`chat:write`);
- read that channel's history so the final exact-message assertion can use `conversations.history` (`channels:history` for a public channel, or the equivalent history scope for the chosen channel type).

The external bot may itself be a normal Slack app bot. No self-bot or human user token is required.

## Environment

```text
LITTLE_MONKEY_REQUIRE_SLACK_INSTALLED_SERVICE_E2E=1
SLACK_E2E_BOT_TOKEN=xoxb-...        # target app; stored only through Little Monkey
SLACK_E2E_APP_TOKEN=xapp-...        # target app Socket Mode token
SLACK_E2E_EXTERNAL_BOT_TOKEN=xoxb-... # independent observer app
SLACK_E2E_CHANNEL_ID=C0123456789
```

## Run

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example slack_installed_service_e2e
```

## Assertions

The harness creates a fresh profile and configures Slack only through the same CLI surface the desktop uses. The target credential is written through `channels set-token`, the account is enabled with a conversation-scoped route, and the actual OS user service is installed.

A pass requires all of the following:

1. the installed daemon reaches fresh `connected` Slack health, which requires both a valid bot token and a live Socket Mode transport;
2. the independent bot posts the first unique marker through Slack;
3. that real provider event is durably recorded as `ignored` with `sender_not_allowed`, with no ingress/job and no model call;
4. the sender id normalized from that real Slack event is approved through `channels approve`;
5. the actual daemon service is stopped/started, gets a distinct resident PID, and Socket Mode becomes freshly connected again;
6. the same independent bot posts the execution marker after restart;
7. the second real Slack event becomes a durable accepted ingress/job for the approved provider-derived sender;
8. the production daemon agent sends the exact marker to the model and is offered the real `send_message` tool, then returns to the model after tool execution;
9. the generated reply becomes a durable outbound event with Slack's real message timestamp rather than a `local:*` id;
10. the independent bot calls Slack `conversations.history` for that exact timestamp and Slack returns the exact generated reply text; the observed message must not be the observer bot's own message.

The target app token is never used by the observer side of the harness. Slack provider observation is therefore independent of the production credential boundary.

Compilation alone is not live evidence. A successful run with arbitrary operator-owned Slack apps is the literal acceptance proof.

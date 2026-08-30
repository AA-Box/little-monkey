# Microsoft Teams installed-service acceptance

Literal acceptance path:

`independent Teams user → Microsoft Bot Framework signed Activity → public HTTPS messaging endpoint → installed resident daemon → native credential store → durable conversation reference + durable ingress → real daemon agent/model → send_message → durable outbox → Bot Framework Connector API → real provider activity id → independent Teams client observes exact generated reply`

The harness never constructs `TeamsAdapter`, never fabricates an Activity, and never calls the Bot Framework reply endpoint itself.

## Required provider setup

Use a disposable/test Azure Bot / Teams application. Its messaging endpoint must be the exact callback URL printed by the harness:

`$TEAMS_E2E_PUBLIC_BASE/v1/channels/<generated-account-id>`

The public HTTPS origin must route to `127.0.0.1:$TEAMS_E2E_WEBHOOK_PORT` on the machine running the harness. Because the account id is generated per run, the harness pauses after installation so the operator can update the test bot's messaging endpoint before sending the marker.

The Teams adapter authenticates the inbound Bot Framework JWT against Microsoft's OpenID metadata/JWKS, pins the public Bot Framework issuer/audience, requires a Teams-endorsed signing key, and binds the signed `serviceurl` claim to the Activity's `serviceUrl`. That verified service URL is persisted as durable addressing before the provider is acknowledged and is the only address the production outbound path may use.

## Required environment

```text
LITTLE_MONKEY_REQUIRE_TEAMS_INSTALLED_SERVICE_E2E=1
TEAMS_E2E_APP_ID=<Microsoft App ID / client id>
TEAMS_E2E_TENANT_ID=<Microsoft Entra tenant id>
TEAMS_E2E_APP_PASSWORD=<client secret>
TEAMS_E2E_PUBLIC_BASE=https://<public-host-routing-to-this-machine>
TEAMS_E2E_WEBHOOK_PORT=38446
TEAMS_E2E_EXTERNAL_SENDER_ID=<Bot Framework Activity from.id for the independent Teams user>
TEAMS_E2E_DESTINATION=<human-readable chat/channel where the user will message the bot>
```

`TEAMS_E2E_EXTERNAL_SENDER_ID` is the Bot Framework sender id seen by the bot, not an assumed Entra object id. The harness will accept only a durable inbound event from that exact sender containing its unique marker.

## Run

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example teams_installed_service_e2e
```

The harness creates an isolated profile, writes the app password only through `channels set-token` stdin, enables an open direct/group policy, adds a route to a deterministic model backend behind the real production agent interface, installs the actual user service with the webhook listener, and proves a stop/start produces a distinct resident daemon PID.

After the messaging endpoint is configured, the independent Teams user sends the unique marker printed by the harness. A pass requires:

1. Microsoft's authenticated Activity becomes one accepted durable inbound event for the expected sender;
2. the inbound has durable ingress and daemon job ids;
3. the verified Activity's conversation addressing survives in the production durable store and the later send succeeds through it;
4. the real daemon agent sends the exact marker to the model and is offered the production `send_message` tool;
5. the agent returns to the model after tool execution;
6. the generated reply becomes a durable outbound event with a real Bot Framework activity id rather than `local:*`;
7. the independent Teams user reads the reply in the real Teams client and copy/pastes that exact text back to the harness for byte-for-text comparison apart from the terminal newline.

A Connector API 2xx or provider activity id alone is provider acceptance, not recipient observation, and therefore does not satisfy the final assertion.

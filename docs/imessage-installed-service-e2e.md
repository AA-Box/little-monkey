# iMessage installed-service acceptance

Literal acceptance path:

`independent iMessage identity → Messages/iMessage → real Messages database → little-monkey-imessage-helper with Full Disk Access → installed resident daemon → durable ingress → real daemon agent/model → send_message → durable outbox → helper Automation request to Messages.app → iMessage → independent Messages client observes exact reply`

The daemon itself never reads `chat.db` and never sends Apple events. The production helper is the privileged platform boundary, exactly as in the shipped adapter.

## Required Mac setup

Use a real Mac signed in to Messages. Install the matching `little-monkey-imessage-helper` and grant that helper:

- Full Disk Access, so it can read the Messages database;
- Automation permission for Messages.app, so it can send.

Use an independent iMessage identity/number/address as the sender and observer. Do not send the marker from the same Messages identity the helper controls.

## Environment

```text
LITTLE_MONKEY_REQUIRE_IMESSAGE_INSTALLED_SERVICE_E2E=1
IMESSAGE_E2E_HELPER_PATH=/absolute/path/to/little-monkey-imessage-helper
IMESSAGE_E2E_HANDLE=<Messages handle/account configured for the helper>
IMESSAGE_E2E_EXTERNAL_SENDER=<sender handle exactly as Messages records it>
IMESSAGE_E2E_DESTINATION=<human-readable conversation/identity for instructions>
```

## Run

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo run --locked --manifest-path src-tauri/Cargo.toml --example imessage_installed_service_e2e
```

The harness creates a fresh profile, adds the real helper-backed iMessage account, configures routing/policy, installs the actual user service, waits until the production account health reports connected, restarts the service and requires a distinct resident PID, then asks the independent identity to send one unique marker.

A pass requires:

1. a real helper-reported Messages GUID from the expected external sender becomes a durable accepted inbound with ingress/job ids;
2. the real daemon agent sees that marker through the model interface and is offered `send_message`;
3. the agent returns after tool execution;
4. the exact generated reply becomes a durable accepted outbound event in the same conversation;
5. the helper's real send path completes without a retry/ambiguity failure;
6. the independent iMessage client receives the reply and copy/pastes the exact text back to the harness for exact comparison apart from the terminal newline.

Messages does not provide the helper with a stable identifier for a just-sent message, so this acceptance deliberately does not invent a provider id. Recipient observation is the final delivery proof.

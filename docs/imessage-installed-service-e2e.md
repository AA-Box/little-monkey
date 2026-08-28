# iMessage installed-service acceptance

Literal acceptance path:

`independent iMessage identity → Messages/iMessage → real Messages database → little-monkey-imessage-helper with Full Disk Access → installed resident daemon → real pairing/approval → durable ingress → real daemon agent/model → send_message → durable outbox → helper Automation request to Messages.app → iMessage → independent Messages client observes exact reply`

The daemon itself never reads `chat.db` and never sends Apple events. The production helper is the privileged macOS boundary, exactly as in the shipped adapter. The acceptance harness also never opens `chat.db`, never constructs the adapter, and never runs `osascript` directly.

## Required Mac setup

Use a real Mac signed in to Messages. Install the matching `little-monkey-imessage-helper` and grant that exact helper binary:

- Full Disk Access, so it can read the Messages database;
- Automation permission for Messages.app, so it can send.

Use an independent iMessage identity/number/address as the sender and observer. Do not send the markers from the same Messages identity controlled by the Mac running Little Monkey.

No Apple ID or Apple password is supplied to Little Monkey or to the test. The active Messages account is whichever account the operator already uses in Messages.app.

## Environment

```text
LITTLE_MONKEY_REQUIRE_IMESSAGE_INSTALLED_SERVICE_E2E=1
IMESSAGE_E2E_HELPER_PATH=/absolute/path/to/little-monkey-imessage-helper
IMESSAGE_E2E_HANDLE=<Messages/iMessage handle label configured for this account>
IMESSAGE_E2E_DESTINATION=<optional human-readable conversation/identity for instructions>
```

There is intentionally no `IMESSAGE_E2E_EXTERNAL_SENDER`. The real sender is discovered from the production pairing challenge and approved through `monkey channels approve`, just as a public Little Monkey installation discovers and authorizes its own users.

## Run locally on an authorized Mac

```bash
node scripts/ensure-cli-sidecar-placeholder.mjs
cargo build --locked --manifest-path src-tauri/Cargo.toml --bin monkey-cli
cargo build --locked --manifest-path src-tauri/Cargo.toml --example imessage_installed_service_e2e
src-tauri/target/debug/examples/imessage_installed_service_e2e
```

The harness creates a fresh profile, adds the real helper-backed iMessage account through the normal channel CLI, sets pairing policy and routing, installs the actual macOS user service, waits until the production helper probe reports a readable Messages database + authorized Messages Automation + a usable iMessage account, restarts the service and requires a distinct resident daemon PID, then starts the real pairing flow.

The independent user first sends a unique pairing marker. A pass requires Little Monkey to record a real Messages GUID as `challenged`, expose the provider-derived sender in the normal pending-sender queue, and accept `channels approve` for that sender. The same user then sends a second unique execution marker.

A full pass requires:

1. the real helper-reported Messages GUID from the approved sender becomes a durable accepted inbound with ingress/job ids;
2. the real daemon agent receives that marker through the production model interface and is offered `send_message`;
3. the agent dispatches the production message tool and returns to the model after tool execution;
4. the production helper reports the send successful, causing the exact generated reply to become a durable accepted outbound event in the same conversation;
5. because Messages exposes no stable identifier for a just-sent message, the outbound event correctly keeps its `local:<outbox-id>` identifier rather than inventing a provider id;
6. the independent iMessage client receives the reply and copy/pastes the exact text back to the harness for exact comparison apart from the terminal newline.

A successful helper RPC or a durable outbox row alone is not recipient delivery proof. The independent Messages client observation is required for the final assertion.

## GitHub Actions

Pull-request CI runs the macOS compile job only. A hosted runner cannot honestly execute the live acceptance because it is not signed in to the operator's Messages account and cannot possess that user's Full Disk Access / Automation grants.

The workflow also exposes a manual `workflow_dispatch` live job on a trusted `self-hosted, macOS` runner. Supply the absolute path of the already-authorized helper plus the account handle. That job builds the production CLI/harness, installs the real daemon, and runs the same literal interactive pairing + round-trip acceptance described above.

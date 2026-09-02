# IRC installed-service acceptance

`src-tauri/examples/irc_installed_service_e2e.rs` is the black-box acceptance for the IRC channel path.

It is intentionally stronger than the adapter's opt-in transport smoke. The acceptance configures a fresh Little Monkey profile only through the production CLI, installs the real resident user service, waits for that separate service process to connect the production IRC adapter, restarts the service once, then connects an independent TLS IRC client to the same real network.

The independent client sends one unique marker. That marker must become one durable inbound channel event with an ingress turn and job, reach a real daemon task-run child and the production agent loop, cause the agent to dispatch `send_message`, become one durable outbound event beside the daemon's own one-time notice naming the model (a fresh profile's sender is always a first contact), and arrive back at the independent client as `little-monkey irc installed-service reply <marker>`.

The model endpoint is deterministic by design and is the only non-provider fixture. It is reached through a recipe's ordinary `target.local_url` field; it cannot create channel events, write the outbox, or send IRC traffic. The test additionally asserts the original marker reached that model request and that `send_message` was in the tool schema.

The pull-request workflow compiles this harness but does not contact a public IRC network automatically. The live acceptance is `workflow_dispatch` because the destination is an operator-selected real network and unsolicited CI traffic should never be the default. A successful live workflow run, not merely the presence of the harness, is the evidence required before calling IRC demonstrated end to end.

The initial automated service target is Linux `systemd --user`. macOS is intentionally not claimed by this acceptance, and Windows should receive its own installed-service run before a platform-wide claim is made.

# Home Assistant installed-service acceptance

`src-tauri/examples/home_assistant_installed_service_e2e.rs` is the black-box acceptance for the Home Assistant channel path.

It is intentionally stronger than the adapter's unit tests, which prove the framing and the outcome mapping against recorded frames and a loopback fixture but never touch a real instance. The acceptance configures a fresh Little Monkey profile only through the production CLI, writes the long-lived access token with `channels set-token` so the installed daemon has to recover it from the OS keychain, installs the real resident user service, waits for that separate service process to authenticate and subscribe over `/api/websocket`, restarts the service once, and only then fires the event.

The independent client is the operator's own instance, driven over its REST API with the same token. `POST /api/events/<event_type>` fires the subscribed event with one unique marker — the same path an automation would take — so nothing in the harness hands the daemon an event directly. That marker must become one durable inbound channel event with an ingress turn and job, reach a real daemon task-run child and the production agent loop, cause the agent to dispatch `send_message`, become one durable outbound event beside the daemon's own one-time notice naming the model (a fresh profile's sender is always a first contact), and come back out through `POST /api/services/notify/<service>` as `little-monkey home assistant installed-service reply <marker>`, which the harness reads back from `GET /api/states`.

`notify.persistent_notification` is the service the workflow points the account at, because its result is readable back over the same REST API. A different notify service can be named, but then the reply lands somewhere this harness cannot see and the run has to be checked by hand.

The model endpoint is deterministic by design and is the only non-provider fixture. It is reached through a recipe's ordinary `target.local_url` field; it cannot create channel events, write the outbox, or call a notify service. The test additionally asserts the original marker reached that model request and that `send_message` was in the tool schema.

The pull-request workflow compiles this harness but does not contact a Home Assistant instance automatically. The live acceptance is `workflow_dispatch` because the destination is an operator-owned house, and unsolicited CI traffic into somebody's home automation should never be the default. A successful live workflow run, not merely the presence of the harness, is the evidence required before calling Home Assistant demonstrated end to end.

The initial automated service target is Linux `systemd --user`. macOS is intentionally not claimed by this acceptance, and Windows should receive its own installed-service run before a platform-wide claim is made.

## What it needs

| Input | What it is |
| --- | --- |
| `HA_E2E_BASE_URL` | The instance's bare origin. `https` unless it is loopback — the adapter refuses anything else, because the token rides every request. |
| `HA_E2E_TOKEN` | A long-lived access token from *Profile → Security → Long-lived access tokens*. A repository secret, never a workflow input. |
| `HA_E2E_EVENT_TYPE` | The event type the account subscribes to. Defaults to `little_monkey_message`. |
| `HA_E2E_NOTIFY_SERVICE` | The bare service name under `notify`. Defaults to `persistent_notification`. |
| `LITTLE_MONKEY_REQUIRE_HOME_ASSISTANT_INSTALLED_SERVICE_E2E=1` | The refusal that keeps this from installing an OS service and touching somebody's house by accident. |

# Home Assistant installed-service acceptance

`src-tauri/examples/home_assistant_installed_service_e2e.rs` is the black-box acceptance for the Home Assistant channel path.

It is intentionally stronger than the adapter's unit tests, which prove the framing and the outcome mapping against recorded frames and a loopback fixture but never touch a real instance. The instance is a real one — in CI, an official Home Assistant container the job starts; on a dispatch or a local run, whichever instance the operator names. The acceptance configures a fresh Little Monkey profile only through the production CLI, writes the access token with `channels set-token` so the installed daemon has to recover it from the OS keychain, installs the real resident user service, waits for that separate service process to authenticate and subscribe over `/api/websocket`, restarts the service once, and only then fires the event.

The independent client is that same instance, driven over its REST API with the same token. `POST /api/events/<event_type>` fires the subscribed event with one unique marker — the same path an automation would take — so nothing in the harness hands the daemon an event directly. That marker must become one durable inbound channel event with an ingress turn and job, reach a real daemon task-run child and the production agent loop, cause the agent to dispatch `send_message`, become one durable outbound event beside the daemon's own one-time notice naming the model (a fresh profile's sender is always a first contact), and come back out through `POST /api/services/notify/<service>` as `little-monkey home assistant installed-service reply <marker>`, which the harness reads back from `GET /api/states`.

The workflow points the account at a `command_line` notify service named `little_monkey_e2e`, seeded into the container's `configuration.yaml`. It appends each notification it is sent to a file, and a `command_line` sensor publishes that file's last line as `sensor.little_monkey_e2e_reply` — that state is what the harness reads back. `notify.persistent_notification` cannot be used for this: since 2023.6 a persistent notification is a WebSocket-only object that never becomes a state, so `GET /api/states` can never carry one. A different notify service can be named, but unless it also surfaces as an entity the reply lands somewhere this harness cannot see and the run has to be checked by hand.

The model endpoint is deterministic by design and is the only non-provider fixture. It is reached through a recipe's ordinary `target.local_url` field; it cannot create channel events, write the outbox, or call a notify service. The test additionally asserts the original marker reached that model request and that `send_message` was in the tool schema.

The live acceptance runs on every pull request that touches these paths. It contacts no operator's house: the job starts the official `homeassistant/home-assistant` container, pinned to an exact release, on `127.0.0.1:8123`, then creates the owner and the access token through Home Assistant's own onboarding and token APIs. There is no repository secret and no operator input, so it runs on fork pull requests too. `workflow_dispatch` runs the same job on demand. A successful live workflow run, not merely the presence of the harness, is the evidence required before calling Home Assistant demonstrated end to end.

The initial automated service target is Linux `systemd --user`. macOS is intentionally not claimed by this acceptance, and Windows should receive its own installed-service run before a platform-wide claim is made.

## What it needs

| Input | What it is |
| --- | --- |
| `HA_E2E_BASE_URL` | The instance's bare origin. `https` unless it is loopback — the adapter refuses anything else, because the token rides every request. In CI it is `http://127.0.0.1:8123`, the container's own loopback origin. |
| `HA_E2E_TOKEN` | An access token for that instance. In CI the job mints one by onboarding the owner over `POST /api/onboarding/users` and exchanging the returned code at `POST /auth/token`, so it is not a repository secret and does not come from *Profile → Security*. Running against your own house, that is where you get one. |
| `HA_E2E_EVENT_TYPE` | The event type the account subscribes to. Defaults to `little_monkey_message`. |
| `HA_E2E_NOTIFY_SERVICE` | The bare service name under `notify`. Required, with no default: in CI it is `little_monkey_e2e`. |
| `LITTLE_MONKEY_REQUIRE_HOME_ASSISTANT_INSTALLED_SERVICE_E2E=1` | The refusal that keeps this from installing an OS service and touching somebody's house by accident. |

An operator who wants this run against their own instance sets the same four variables and runs the example locally; nothing about the harness is CI-specific.

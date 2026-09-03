# Web chat installed-service acceptance

`src-tauri/examples/webchat_installed_service_e2e.rs` is the black-box acceptance for the served web chat channel path.

It is intentionally stronger than the other two channels landing beside it, and for one reason: it needs nothing of yours. No account, no credential, no network, no hardware. The acceptance configures a fresh Little Monkey profile only through the production CLI, mints a self-signed `127.0.0.1` certificate with the `openssl` CLI (exactly as `daemon/peer_live.rs` does), points the remote host at a free loopback port with it, installs the real resident user service, waits for that separate service process to report the page's own URL as its health, restarts the service once, and then acts as a browser: an ordinary HTTPS client pinned to that certificate and nothing else.

The visitor leg is the whole claim, in four steps.

1. The client loads `GET /webchat/<account>`, asserts the page arrived under its own content security policy, and calls `POST /webchat/<account>/session` for a visitor identifier. The identifier is minted by the daemon — the client never proposes one — and is 43 characters of unpadded base64url.
2. The client posts a unique marker. The account's policy is `pairing`, so this unknown visitor is answered with a **pairing code** on this account's own outbox, which the client then reads back off `GET /webchat/<account>/messages`. Nothing runs: the acceptance fails if that first marker ever reached the model.
3. The operator approves the waiting sender with `monkey channels approve <account> <sender>`, read out of `monkey channels senders --json`. That sender is the *hashed* visitor — the acceptance fails if the raw identifier is what reached the store.
4. The same visitor posts a second marker. It must become one durable inbound channel event with an ingress turn and job, reach a real daemon task-run child and the production agent loop, cause the agent to dispatch `send_message`, become one durable outbound event beside the pairing code and the daemon's own one-time notice naming the model, and appear on that visitor's own page as `little-monkey webchat installed-service reply <marker>`. A second visitor of the same account, opened at the end, must read none of it.

The model endpoint is deterministic by design and is the only fixture. It is reached through a recipe's ordinary `target.local_url` field; it cannot create channel events, write the outbox, or answer a request on the chat page. The test additionally asserts the original marker reached that model request and that `send_message` was in the tool schema.

Unlike the IRC, email and Home Assistant acceptances, the live leg here runs automatically on every pull request that touches these files, because there is no third party to contact and no operator secret to supply. `workflow_dispatch` runs the same job on demand.

What that run does **not** prove is a browser on another machine reaching a non-loopback bind with a certificate it trusts. That needs a certificate authority and a network you own, and it stays yours to verify. The page is served by the same listener, over the same TLS, on the same address as the remote controller shell and the signed device API, so wherever that listener answers today is where the page answers too — with the account's pairing policy and the route's own rate limit as the gate.

The automated service target is Linux `systemd --user`. macOS is intentionally not claimed by this acceptance, and Windows should receive its own installed-service run before a platform-wide claim is made. The acceptance skips itself, with a printed reason, if the `openssl` CLI is absent.

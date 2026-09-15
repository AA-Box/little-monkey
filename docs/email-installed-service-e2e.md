# Email installed-service acceptance

`src-tauri/examples/email_installed_service_e2e.rs` is the black-box acceptance for the email channel path.

It is intentionally stronger than the adapter's unit tests, which are network-free and prove normalization, threading headers and SMTP outcome mapping against recorded fixtures. The acceptance configures a fresh Little Monkey profile only through the production CLI, stores the mailbox password bundle through `channels set-token` on stdin, installs the real resident user service, waits for that separate service process to report the production email adapter connected to a real IMAP server, restarts the service once, then sends one message from an independent mailbox over a real SMTP relay.

The independent client sends one unique marker. That marker must become one durable inbound channel event with an ingress turn and job, reach a real daemon task-run child and the production agent loop, cause the agent to dispatch `send_message`, become one durable outbound event beside the daemon's own one-time notice naming the model (a fresh profile's sender is always a first contact), and arrive back in the independent mailbox as `little-monkey email installed-service reply <marker>` in a message whose `In-Reply-To` is the marker's own `Message-ID`.

The model endpoint is deterministic by design and is the only non-provider fixture. It is reached through a recipe's ordinary `target.local_url` field; it cannot create channel events, write the outbox, or send mail. The test additionally asserts the original marker reached that model request and that `send_message` was in the tool schema.

On every pull request that touches these paths, the live acceptance runs against a real mail server: an official `mailserver/docker-mailserver` image — Postfix and Dovecot, pinned to an exact tag — started in a container on the runner and published on `127.0.0.1:993` and `127.0.0.1:465`. Both legs are implicit TLS with a certificate the job mints itself: it creates a certificate authority with the openssl CLI, signs one leaf for `mail.lm.test`, mounts only the leaf into the container, and points both the account (through its `tls_ca_file`) and this harness's own mail client (through `EMAIL_E2E_CA_FILE`) at that authority, added to the public web anchors rather than replacing them. Two mailboxes on one local domain are provisioned before the server starts, by writing `postfix-accounts.cf`, so the marker really is sent to Little Monkey from somewhere Little Monkey is not.

Before the harness runs, the workflow speaks IMAP and SMTP to that server itself with `openssl s_client -CAfile ... -verify_hostname ...` and logs both mailboxes in. A container that is listening but not serving, a certificate that does not chain to the job's own authority or does not match the name, and a server that came up with no mailboxes at all each fail there in seconds with the server's own words, rather than three minutes later as a credential-free "the daemon could not reach the mailbox over IMAP".

`workflow_dispatch` remains, and is now the way to run the same acceptance against a mailbox an operator owns — a real provider, a real app password, a certificate that chains to a public root, and no extra authority named. A successful run, not merely the presence of the harness, is still the evidence required before calling any of this demonstrated end to end.

The initial automated service target is Linux `systemd --user`. macOS is intentionally not claimed by this acceptance, and Windows should receive its own installed-service run before a platform-wide claim is made.

## What the live run needs

Two mailboxes, because the acceptance must be able to send *to* Little Monkey from somewhere Little Monkey is not:

| Variable | What it is |
| --- | --- |
| `EMAIL_E2E_IMAP_HOST` / `EMAIL_E2E_IMAP_PORT` | The account's IMAP server. Implicit TLS only; `143` is refused at construction. |
| `EMAIL_E2E_SMTP_HOST` / `EMAIL_E2E_SMTP_PORT` | The account's SMTP relay. Implicit TLS only; `25` is refused at construction. |
| `EMAIL_E2E_USERNAME` / `EMAIL_E2E_FROM` | The login name and the address replies are sent from. |
| `EMAIL_E2E_PASSWORD` | The mailbox password, or the app password where the provider issues one. A repository secret; it is passed to the CLI on stdin and never as an argument. |
| `EMAIL_E2E_PEER_ADDRESS` | The independent mailbox the marker is sent from and the reply is read back in. |
| `EMAIL_E2E_PEER_IMAP_HOST` / `EMAIL_E2E_PEER_IMAP_PORT` | That mailbox's IMAP server. |
| `EMAIL_E2E_PEER_SMTP_HOST` / `EMAIL_E2E_PEER_SMTP_PORT` | That mailbox's SMTP relay. |
| `EMAIL_E2E_PEER_USERNAME` / `EMAIL_E2E_PEER_PASSWORD` | That mailbox's credentials. Also a repository secret. |
| `EMAIL_E2E_CA_FILE` | Optional. A PEM file of extra certificate authorities, trusted *in addition to* the public web anchors by both the account under test and this harness's own mail client. Set only by the container run; a dispatch run against a real provider leaves it unset. A file that is named but cannot be used stops the run rather than falling back. |

The harness refuses to run at all unless `LITTLE_MONKEY_REQUIRE_EMAIL_INSTALLED_SERVICE_E2E=1`, because it installs a real OS service and sends real mail to a real address.

## What this acceptance does not prove

- **IMAP IDLE.** The adapter polls, roughly twice a minute; the harness waits for that cadence rather than for a push. A reply is not instant and this acceptance does not claim it is.
- **OAuth sign-in.** Password and app-password authentication is what ships. A Gmail or Microsoft account that has disabled app passwords cannot be used for this run.
- **Anything but the one folder the account names.** The acceptance watches `INBOX` unless the account configures otherwise.
- **A real provider.** The container run proves Postfix and Dovecot on their implicit-TLS ports; it says nothing about app passwords, a provider's own rate limits and concurrent-connection caps, a certificate chaining to a public root, or any provider that offers OAuth only. Those need the `workflow_dispatch` run against a mailbox somebody owns.
- **The public trust path.** What the container run exercises is the *additive* one: an authority the account names on top of the webpki set. An account naming no `tls_ca_file` still reaches its provider through the public anchors alone, and only the dispatch run proves that.
- **Mailing lists, auto-responders and bounces.** The adapter refuses those before an envelope exists (RFC 3834), so there is nothing here for the acceptance to observe; the unit tests are where that refusal is proven.

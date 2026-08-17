# Reaching an agent from outside this machine

Little Monkey can be reached over a messaging provider, from a paired phone, by
another Little Monkey installation, over the telephone, and by voice. Those five
paths ship different transports and one shared boundary. This page documents
what is built, who has to supply what, and — separately — which of it has been
proven by automated tests and which needs an account only you can provide.

Where a capability is narrower than its name, the boundary is in
[Limitations](limitations.md). The permission model itself is in
[Security](security.md).

## Nothing here is operated by anyone but you

Every one of these paths needs an account, a number, a domain or a device. All
of them are **yours**. This project ships no relay, no bot, no phone number, no
push service, no signing key you borrow and no hosted endpoint that sees your
traffic. There is no vendor account behind any feature on this page, so there is
nothing for a maintainer to revoke and nothing for you to be billed for by us.

Concretely, that means:

- You create the bot, app or integration in the provider's own console, and you
  paste its token into Settings. It goes to the OS keychain; the database keeps
  only the name to look it up under, which is what makes a copied database
  useless.
- You bring your own carrier account for SMS and calls, and you pay that carrier
  directly.
- You bring your own public URL for anything a provider delivers to. See
  [Public callbacks](#public-callbacks-are-yours-to-expose).
- You install helper programs (`signal-cli`, the iMessage helper) yourself and
  point the account at them; this app never downloads or bundles one.
- Push notifications use a provider you configure. Without one, a paired device
  is reachable when it is open and not before.

## Messaging channels

Thirteen providers reach the same durable ingress: Telegram, Discord, Slack,
WhatsApp, Microsoft Teams, Google Chat, LINE, Matrix, Mattermost, Signal,
iMessage, IRC, and SMS. A sandboxed extension can speak for a fourteenth of its
own.

They arrive four different ways — a long poll, a socket the app holds open, a
webhook the provider posts to, or a local helper process — and *converge before
anything is decided*. One gate answers who may talk to this account, one router
decides what recipe runs, and one durable turn is what actually runs. No
provider has its own copy of an access rule.

### Who may talk to it

Per account, and separately for direct and group conversations:

- **Pairing** (the default for direct messages) — an unknown sender gets a
  one-time code and waits for you to approve it in Settings.
- **Approved list** (the default for groups) — only senders you approved.
- **Disabled** — inbound is recorded and dropped.
- **Open** — anyone. Never a default; you have to choose it, and Security Doctor
  reports it as critical for as long as it is on.

Group conversations additionally decide *when* to answer: only when addressed
(the default), always, or never. A blocked sender beats every policy, including
Open.

**Being allowed to send a message is not authority to do anything.** An approved
sender's message becomes a run, and that run is bounded by the same permission
policy as anything you type into the desktop app. A message cannot approve its
own file write, shell command, network call, or reply. Provider metadata — a
display name, a role, a channel topic — is untrusted text and never becomes
permission to do something.

### Files somebody sends

An inbound attachment is size-checked against the account's limit *before* a
connection is opened, streamed under that limit rather than trusted to be the
size it claimed, verified against the declared size afterwards, and stored in
the shared content store. A file whose bytes do not match its description is
refused rather than stored under the wrong name. Nothing is auto-extracted and
nothing is executed. Limits are per account and clamped: you can lower them
freely and can only raise them so far.

### Loops

An account never answers itself: our own echo is recognized and refused before
any policy runs. An exchange between two machines is bounded twice — through the
reply chain when the other side threads its replies, and through a budget of
consecutive machine messages when it does not. Somebody speaking resets the
budget, so a human conversation is never rate-limited by it.

## Public callbacks are yours to expose

WhatsApp, Teams, Google Chat, LINE and the carriers deliver by **posting to a
URL**. That URL has to be reachable from the internet, and reaching it is
something you arrange — a tunnel, a reverse proxy, your own domain. This app
binds loopback and never opens a port to the world on its own.

What it does guarantee about that endpoint:

- Every delivery is authenticated by the provider's own scheme over the exact
  bytes received, before anything is parsed or stored. A body that does not
  verify earns no row.
- Nothing reads `Host` or `X-Forwarded-*`. A provider whose signature covers the
  callback URL is given **your configured public base**, never a value from the
  request, because a header is attacker-controlled.
- A refused delivery is counted against the account with its reason code — never
  the body, never a header, never the signature. That counter is the only
  symptom a rotated secret or a stale console URL has, and Security Doctor reads
  it.
- The callback URL is composed in one place and shown in Settings. Paste that
  exact string into the provider's console.
- Use HTTPS. Security Doctor reports a plaintext callback as critical.

## SMS and calls

Bring a number from Twilio, Plivo or Telnyx. Texts join the same messaging
ingress as everything else — a text is not a special kind of message. Calls are
what telephony adds, and the two authorities are deliberately separate:

- **Answering** is the number's own setting: reject, take a message, or answer
  and talk.
- **Placing a call** is a tool call under the normal approval policy. A number
  that can dial out without asking is a critical Security Doctor finding,
  because a call reaches a person who did not ask to be reached and it bills
  you.

Inbound MMS is downloaded with your carrier credential after the callback has
been acknowledged, never inside it: a slow media host must not push a callback
past the carrier's timeout, because a carrier that times out redelivers.

Recording is off unless you turn it on, and is reported as an informational
finding when it is on, because recording somebody may require telling them
first where you live.

## Paired devices and the mobile companion

A device pairs by scanning a QR code or pasting the same short invitation. From
then on it holds its own key and reaches this machine over your own TLS
identity — there is no account service and no relay.

A physical capability (camera, microphone, screen, location, clipboard,
notifications) is granted per device and is revocable. A command sent to a
device happens **at most once**: the queue is durable, the result is staged
before it is acknowledged, and a restart reconciles rather than re-runs. A
revoked device can neither lease new work nor recover old work.

Offline, a device serves what it already cached and queues what you do. Push
wakes a device that is closed — with your provider, and carrying detail only if
you allow it on a lock screen.

## Talk

A spoken conversation with the agent, on the desktop and on a foregrounded
phone. Speech recognition and synthesis use whatever backend you configured;
a number set to answer calls with no transcription backend is a critical finding
rather than a line that silently drops every word.

An utterance is an ordinary durable turn keyed by a name the *device* gives it,
so re-sending one lands on the run the first attempt made rather than starting a
second. The runner cannot mint that name itself — its session identity is minted
fresh with every ticket — which is why a closing audio frame that carries none is
refused. The shipped client does not re-send: when the socket drops it ends the
session and says so. The name makes a retransmission safe, not automatic. Wake-phrase and
always-listening are off unless you turn them on, and Security Doctor reports
both — and separately reports any path that would send audio off this machine.

## Peers

Another Little Monkey installation can be paired as a peer, to exchange
messages, hand over tasks and pass artifacts. A peer's authority is its own and
is never yours: a peer pairing carries an empty control scope, and a peer cannot
cause anything on this machine that a stranger's message could not.

Loops and replays are bounded by an origin chain and a hop limit rather than by
trust. An artifact a peer references must be one *that peer* uploaded here and
whose bytes verified — a digest is an integrity value, not a capability, so
knowing one buys nothing.

## Executable extensions

A sandboxed WebAssembly extension can add tools, providers and channel
transports. It is confined by the host: declared permissions, declared network
origins, bounded resources, and no ambient authority. Unsigned or untrusted
extensions, broad network origins, high-risk permissions, an incompatible host
API and repeated traps are all Security Doctor findings.

An extension's word about a sender is the extension's word — the host cannot
verify it — which is why an extension-backed channel account is reported as
unable to recognize its own echo.

## Security Doctor

`monkey security audit --json`, and the Security Doctor panel, run the same
checks over the same state. Neither contacts a model. The categories are
storage, network, MCP, extensions, skills, isolation, browser and companion
grants, voice, devices, channels, telephony and peers.

## Support bundles

`monkey security support-bundle` produces a document you can hand to somebody
else: a bounded, ordered trace of what your channels, phone numbers, peers and
devices have been doing — event, party, order and outcome. The desktop's
Diagnostics panel attaches the same trace to its export.

It carries **no** message text, transcript, audio, key, session or credential:
those have no field to travel in, so this is a property of the format rather
than of a scrubbing pass. Every identifier is replaced by a token derived with a
salt generated for that one bundle and never recorded, so a party is consistent
*within* a document and correlates with nothing outside it — including your own
phone number. Each section is capped and reports what it dropped, and a
subsystem that could not be read says so rather than looking idle.

## What is proven how

The distinction below is deliberate. Everything in the first list is exercised
by tests that run on every commit, against protocol fixtures and the production
code paths. Everything in the second needs an account, a device, a number, an
operating-system permission or a public URL that only you can supply, and is
therefore **not** claimed as verified here.

### Implemented and covered by automated tests

- Inbound normalization, access policy, pairing, routing, durable turn creation
  and run submission for every provider, driven through the production ingress.
- Webhook signature verification for WhatsApp, Teams, Google Chat and LINE, and
  carrier callback verification for Twilio, Plivo and Telnyx, over exact bytes,
  including forged and replayed bodies.
- Acknowledgement boundaries: what a provider may be told, and when. Slack
  Socket Mode's parked acknowledgements, and the four webhook providers' own
  success shapes.
- Restart and crash behaviour: generalized ingress, the Telegram poll, the
  Discord gateway, Slack Socket Mode, Matrix replay, outbound sends, webhook
  processing, a leased device command, peer delivery and tasks, inbound SMS, a
  live call, extension invocation and a Talk turn. No leg duplicates an external
  effect across a restart.
- Attachment size validation, streaming caps, declared-versus-actual
  verification, and MMS download through the carrier.
- At-most-once device command execution, durable results, and reconciliation
  after a restart.
- Peer origin chains, hop limits, artifact admission and per-peer scoping.
- Extension sandbox confinement, capability enforcement and cancellation.
- Every Security Doctor check, and the redaction guarantees of the support
  bundle.

### Needs your own account, hardware or exposure to verify

These have production code and protocol-level tests; what has not happened is a
run against the real service. `cargo test --bin monkey-cli -- daemon::live_smoke`
is the opt-in harness — it reads credentials from the environment, passes
silently when they are absent, and never bundles or defaults one.

- Any provider against a real account: that the live service behaves as the
  adapter expects, and that your token has the scopes it needs.
- A publicly reachable callback URL, and the provider console accepting it.
- Real SMS delivery, a real inbound call, real audio in both directions, and
  what your carrier actually charges.
- A physical phone: pairing by camera, the OS permission prompts, push delivery
  through your provider, and background behaviour.
- `signal-cli` and the iMessage helper against a registered account and a real
  Messages database, including macOS Full Disk Access.
- Two installations paired as peers over a real network and a real TLS identity.
- Microphone and speaker behaviour for Talk on your own hardware.

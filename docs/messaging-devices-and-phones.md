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
- You bring your own public URL for anything a provider delivers to — either
  one you publish, or a tunnel this app runs on your own tunnel account. See
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

An account never answers itself, and for the twelve built-in providers that is
the host's own conclusion: the code reading the provider's payload is this app's,
holding the account's credential.

A sandboxed extension speaking for a provider is the case that cannot work that
way — the thing deciding is the thing being checked. So for those accounts the
question is causal instead. Every message this app sends is remembered by the
id the provider gave it, in that conversation; an inbound message carrying one
of those ids is our own echo, whatever its envelope says about its sender. A
guest cannot write that record, and a guest claiming a message is ours grants it
nothing — the claim can only ever cause *fewer* runs.

A transport that cannot supply stable provider message ids says so, and is held
to a narrower reply policy for it: no open inbox, and no answering every message
in a group. Both are the settings that can talk to themselves forever, and an
account that cannot recognise its own voice may not hold either. The ledger is
bounded by age and by rows per account, and holds no message text — it answers
"did we send this", not "what did we say".

An exchange between two machines is bounded twice more — through the reply chain
when the other side threads its replies, and through a budget of consecutive
machine messages when it does not. Somebody speaking resets the budget, so a
human conversation is never rate-limited by it.

## Public callbacks are yours to expose

WhatsApp, Teams, Google Chat, LINE and the carriers deliver by **posting to a
URL**. That URL has to be reachable from the internet, and this app binds
loopback. There are two ways to bridge that, and both of them are yours:

- **You publish it.** A reverse proxy, your own domain, a tunnel you run
  yourself. Paste the address into Settings and this app composes the callback
  URL under it.
- **This app runs your tunnel.** You create a tunnel in your own Cloudflare
  Zero Trust dashboard, add a public hostname, point it at this machine's
  webhook port, and paste the tunnel's token here. The background service then
  starts the `cloudflared` you installed, watches it, restarts it with a bound,
  and reports what it is doing. The token goes to the OS keychain and travels
  to the process in its environment, never on a command line where every other
  process could read it.

There is no relay in either case. No hostname, account, credential or endpoint
here belongs to anybody but you, and a managed tunnel is your tunnel — this app
supplies the supervision, not the address.

A named tunnel specifically: a quick tunnel mints a fresh random hostname on
every start, which cannot be pasted into a provider console and would break
every callback signature the moment the process restarted. A backend that
cannot hold a stable URL is not webhook exposure, so it is not offered.

**The tunnel is transport.** It terminates at the same loopback listener a
`curl` from this machine would reach, and nothing past that point changes.

What it does guarantee about that endpoint:

- Every delivery is authenticated by the provider's own scheme over the exact
  bytes received, before anything is parsed or stored. A body that does not
  verify earns no row.
- Nothing reads `Host` or `X-Forwarded-*`. A provider whose signature covers the
  callback URL is given **your configured public base**, never a value from the
  request, because a header is attacker-controlled — and a tunnel sets all of
  them. This is why the public base comes from the hostname you configured
  rather than from anything the running tunnel reports: a verification URL that
  moved when a process restarted would be one an attacker could move.
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
refused.

The device keeps the recording until the runner says the turn exists durably,
and only then. "We have your audio" and "we transcribed it" are both claims
about a process a crash erases; the acknowledgement is about a row that survives
one. If the connection drops first, the recording is still there and you are
offered **Retry** or **Discard** — retrying sends the audio already captured,
under its original name, so it collapses onto the run the first attempt made.
Nothing re-opens the microphone on its own; resuming is a button. If the turn
*was* accepted and only the answer was lost, it is not re-sent — it is already
running, and the answer arrives in the conversation.

What is held is bounded: eight recordings, 8 MiB each, 32 MiB together, a day.
Nothing about it reaches a log, a support bundle or a diagnostic — there is no
field for audio in any of them. Wake-phrase and always-listening are off unless
you turn them on, and Security Doctor reports both — and separately reports any
path that would send audio off this machine.

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

An extension's word about a sender is the extension's word, and the host does
not rest anything on it. An extension-backed channel account recognises its own
echo through the host's own record of what it sent (see [Loops](#loops)); a
transport that cannot supply stable provider message ids is reported as such and
is held to a narrower reply policy for it.

## Security Doctor

`monkey security audit --json`, and the Security Doctor panel, run the same
checks over the same state. Neither contacts a model. The categories are
storage, network, MCP, extensions, skills, isolation, browser and companion
grants, voice, devices, channels, telephony and peers.

Under channels it also reports how this machine is exposed: a webhook account
waiting for deliveries with nowhere to receive them, a tunnel client that is not
where the account says it is, a missing or rejected tunnel credential, a tunnel
that will not settle, a stopped one while accounts still expect it, and a public
callback base that is not HTTPS.

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
- Host-owned echo suppression for an extension-backed account: the outbound id
  is recorded by the production send path, the inbound match suppresses before
  any policy runs, it survives a restart, a different id or a different
  conversation is unaffected, and the ledger is bounded.
- The Talk pending-utterance journal and its bounds, and the acknowledgement
  that is the only thing a recording is deleted on — including that a refused
  turn is never acknowledged and a re-send lands on the same run.
- Managed callback exposure: which piece is missing is named, the credential is
  never on a command line or in a stored error, a failing tunnel client's own
  words reach the operator, the public base survives a restart, and a spoofed
  `X-Forwarded-*` cannot move the URL a carrier signature is verified against.
- The credential boundary between the two programs that share one: you paste a
  token into Settings, the desktop hands it to the bundled CLI on stdin, and
  the **installed** background service — a launchd agent, a systemd `--user`
  unit or a Scheduled Task — reads it back and builds the account's adapter
  from it. Run against a real installed service on macOS, Windows and Linux.
  The writing program is the point rather than an implementation detail: macOS
  returns a keychain item only to the executable that created it and puts a
  confirmation dialog in front of anybody else, which a background service has
  nobody to answer, so the credential is written by the same binary the daemon
  runs. Two programs share one credential and only one of them ever writes
  it, so the item stays scoped to a single executable rather than being opened
  up to anything running as you.

### Needs your own account, hardware or exposure to verify

These have production code and protocol-level tests; what has not happened is a
run against the real service. Two opt-in harnesses exist, both reading
credentials from the environment, both passing silently when they are absent,
and neither bundling or defaulting one:

- `cargo test --bin monkey-cli -- daemon::live_smoke` proves a real account's
  outbound transport, through the same outbox path every reply takes.
- `cargo test --bin monkey-cli -- daemon::live_agent_e2e` proves the whole
  path: a message you send to your own account becomes a durable turn, a real
  daemon run and a real agent reply, and the provider is then asked over its
  own API whether it holds that reply. Telegram, Discord and Slack today; the
  provider-independent middle is shared, so a further provider supplies only
  its account, its credential and how to read the reply back.

Signal, Mattermost, IRC and iMessage additionally have their own live round
trips (`a_live_signal_round_trip` and its siblings), which prove a real
helper or server connection and a real outbound send — not an inbound message
from somebody else, and not the agent.

- Any provider against a real account: that the live service behaves as the
  adapter expects, and that your token has the scopes it needs.
- A publicly reachable callback URL, and the provider console accepting it.
- A real Cloudflare tunnel account: that `cloudflared` connects with your token,
  that the hostname routes to this machine, and that a provider's delivery
  actually arrives through it. The lifecycle, the readiness probe and the
  refusals are all exercised against real child processes and real sockets; what
  is not is a live tunnel to Cloudflare's edge.
- Real SMS delivery, a real inbound call, real audio in both directions, and
  what your carrier actually charges.
- A physical phone: pairing by camera, the OS permission prompts, push delivery
  through your provider, and background behaviour.
- `signal-cli` and the iMessage helper against a registered account and a real
  Messages database, including macOS Full Disk Access.
- How narrowly macOS scopes that keychain item, and that the scope survives an
  app update. CI runs the path itself against a real launchd service, but with
  unsigned development binaries, whose keychain items are not scoped the way a
  signed build's are — that half is a property of the identity your release is
  signed with. The acceptance run on your own machine: install the release
  build, add a channel and paste its token, start the daemon from Settings, and
  confirm the account leaves `disconnected` on its own with no keychain prompt
  — then install an update and confirm it still does. If a prompt ever appears,
  answering it once with **Always Allow** is enough, and
  `monkey channels probe <account>` from a terminal says whether the credential
  can be read at all.
- Two installations paired as peers over a real network and a real TLS identity.
- Microphone and speaker behaviour for Talk on your own hardware.

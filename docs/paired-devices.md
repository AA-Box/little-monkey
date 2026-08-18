# Paired devices

A paired phone or tablet is already a *controller*: it can watch runs, approve
operations, and chat, within the scope its invitation froze. This document is
about the other direction — letting the runner ask that device's own hardware
for something, once, with the operator's explicit permission.

Nothing here is a new pairing system. It extends the existing remote trust:
the same one-time invitation, the same non-exportable device key, the same
signed, replay-guarded HTTPS identity, the same revocation.

## The four axes, and why they are separate

An action happens only where all four agree:

```
effective =
    granted by the operator
  AND supported by the device build
  AND permission ∈ { granted, not_required }
  AND readiness == ready
```

* **Granted** — what you gave this device at pairing time or afterwards.
* **Supported** — what the device's build can actually do. It reports this
  itself, and a claim here grants nothing: advertising a camera can only ever
  *narrow* what is effective.
* **Permission** — the operating system's answer, and only where an operating
  system has one. Four of the eight capabilities have no OS permission at all,
  and saying otherwise was a real defect: a runner that demanded `granted` for
  `device_info` refused it forever, because no browser or phone has a "may this
  app read its own name" permission to grant.
* **Readiness** — whether the device could act *right now*. Separate from
  permission because the fixes are separate: a permission is granted once in a
  settings screen; readiness is about this moment, and the operator's next step
  is different in each case.

Permission is one of:

| State | Means |
| --- | --- |
| `granted` | the OS permits it |
| `denied` | the OS refuses it — fix in the device's own system settings |
| `promptable` | not asked yet, and askable from the device |
| `not_required` | no such permission exists on this platform |
| `unsupported` | this build has no such facility |

`undetermined` is the older spelling of `promptable` and is treated identically:
**not asked is never permission.** A device build that predates this model, or
one whose browser cannot answer for a permission, therefore grants nothing by
omission.

Readiness is one of:

| State | Means | Fix |
| --- | --- | --- |
| `ready` | it would work now | — |
| `foreground_required` | the controller must be open and in front | bring it forward |
| `interaction_required` | the platform needs a user gesture first | tap the control on the device |
| `armed_required` | a one-time consent must be armed | share the screen once |
| `unavailable` | nothing can be done from here | — |

A surface that says nothing about readiness reads as `unavailable`. That is
deliberate: a device that has upgraded but not yet re-advertised has its
sensitive capabilities refused until it says, in its own words, what it can do
now. **A missing security field is never read as consent.**

Per capability:

| Capability | Permission | Readiness |
| --- | --- | --- |
| `device_info` | `not_required` | `ready` |
| `camera_capture` | the camera permission | `foreground_required` while the page is hidden |
| `microphone_capture` | the microphone permission | as camera |
| `voice_stream` | the microphone permission | as camera |
| `location_read` | the location permission | as camera |
| `notification_post` | the browser's notification permission | `ready` — a notification does not need the page in front |
| `screen_capture` | `not_required` — sharing is a per-session consent, not a stored permission | `armed_required` until the user shares, `ready` while the share is live |
| `audio_playback` | `not_required` — no platform has a "may this page make a sound" permission | `interaction_required` until playback is enabled by a gesture |

They are shown separately everywhere — the desktop card, the phone's own
screen, `device-list --json` — because "why can this phone not take a photo" has
several different answers, and merging them into one list hides which applies.
Each refusal carries the one reason that applies (`not_granted`, `unsupported`,
`permission_required`, `permission_denied`, `foreground_required`,
`interaction_required`, `screen_capture_not_armed`, `unavailable`,
`no_surface`) and a sentence naming the action that fixes it.

A device paired before this existed advertises nothing, so it keeps every
run-facing grant and gains no hardware access from an app update.

## Preparing a capability on the device

The paired-device controller has a **Device readiness** list: one row per
granted hardware capability, showing Granted, Supported, Permission, Readiness
and Effective separately, with the control that fixes whichever one is in the
way — *Allow camera*, *Allow microphone*, *Allow location*, *Allow
notifications*, *Allow screen capture*, *Enable audio playback*.

**A permission prompt only ever comes from a gesture on the device.** An agent
asking for a camera never causes a prompt to appear in somebody's face; the
command is refused with a reason instead, and the person holding the phone
decides. After any answer — granted or refused — the device re-reads the
permission, recalculates readiness, posts a fresh surface and updates its own
screen. It also re-advertises on focus, on visibility change, on a permission
changing in the browser's own settings, when a screen share ends, and on
reconnect.

## Capabilities

`device_info`, `camera_capture`, `microphone_capture`, `location_read`,
`notification_post`, `screen_capture`, `audio_playback`, `voice_stream`.

None implies another. The one dependency is that `voice_stream` also requires
`microphone_capture` — a continuous stream is a superset of one bounded
recording, and without the rule, withdrawing `microphone_capture` would leave
the microphone reachable.

`voice_stream` is not reachable through `device_action`, and that is not a gap:
a discrete command is one request and one answer, and a stream has neither. It
has its own commands — see below.

## Granting

```bash
monkey daemon remote pair-create --output invite.json --run <run-id> \
  --action view-runs --device camera_capture --device location_read --qr
```

`--qr` prints an actual QR code in the terminal, in its own quiet zone and its
own colours so a light theme cannot invert it. It carries the same one-time
token and pins the same certificate by SHA-256 fingerprint; the PEM is left out
because that is what makes it small enough to read from a screen. The same code
appears in **Settings → Background agents → Remote handoff** the moment an
invitation is created, beside the short `littlemonkey://pair/…` string for
anyone without a camera. The JSON invitation, paste, and file flows all still
work, and an old invitation keeps pairing exactly as it did.

`--json` prints the invitation path, the bootstrap URI, its byte count and the
code as an SVG, which is what the desktop panel reads.

After pairing, grants are edited without re-pairing:

```bash
monkey daemon remote device-list
monkey daemon remote device-grant <device-id> --capability notification_post
```

`device-grant` replaces the *physical* grants only; run access stays exactly as
the invitation froze it. Passing no `--capability` withdraws all of them, and
anything already queued under a withdrawn capability is cancelled at once.

## Asking a device to do something

From an agent, the `device_action` tool. From a terminal:

```bash
monkey daemon remote device-action camera_capture --position front --wait-ms 30000
```

Both go through the ordinary permission gate, and both are validated on the
runner — a device build with a lenient parser cannot be talked into a
ten-minute recording.

`audio_playback` takes either text for the device to speak, or a stored run
artifact for it to play:

```bash
monkey daemon remote device-action audio_playback --text "the build is green"
monkey daemon remote device-action audio_playback --run-id <run> --artifact-id <artifact>
```

Playing an artifact means the device fetches it over the ordinary signed
artifact route, under the run scope it was already paired with — so it also
needs the `read_artifacts` grant, and there is no second way to move bytes onto
a device.

## Listening: voice streams

A stream is not a command with a large answer, so it is not one here. The queue
carries the *control* command — "open the microphone for this session" — and the
audio arrives on its own routes, chunk by chunk, while that command runs.

```bash
monkey daemon remote voice-start --duration-ms 30000
monkey daemon remote voice-list
monkey daemon remote voice-stop <session-id>
monkey daemon remote voice-save <session-id> --output room.webm
```

`voice-stop` cancels the control command; the device learns it was stopped from
the answer to the next chunk it posts, closes the microphone, and closes the
session itself. "The runner stopped listening" and "the microphone is closed"
are different statements, and only the device can make the second one.

Chunks carry a sequence number, so a phone on a bad connection that re-sends one
is told the runner already has it rather than having the audio appended twice.
The runner also closes a stream on its own deadline, and fails the control
command with the effect recorded as unproven — a device that walked into a
tunnel leaves a closed session behind, not an open one.

There is no transcription here. The audio is stored as audio.

## Talking: the Talk socket

A voice stream records a room. Talk is a conversation, so it needs the other
half — transcript, thinking, an answer, spoken audio coming back — and a queue of
appends cannot carry that. It gets a dedicated WebSocket, on the same listener,
the same TLS and the same pairing as everything above, gated on the same
`voice_stream` grant.

**How a socket is authenticated.** Every other route here is a signed request:
HMAC over method, path, body, sequence, nonce and key generation. A browser
cannot put any of that on a WebSocket handshake — the API takes no headers — so
the device makes one ordinary signed request for a ticket and spends it
immediately:

```
POST /v1/remote/device/talk/ticket        (signed, gated on voice_stream)
GET  /v1/remote/device/talk/{id}/stream?ticket=…   (Upgrade: websocket)
```

The ticket is random, single-use, lives thirty seconds, and carries the identity
of the signed request that minted it. Grant, key generation and revocation are
re-checked at the moment the socket is admitted, not only when the ticket was
issued — thirty seconds is long enough to revoke a device, and the socket that
follows can stay open for an hour. A plain signed `GET` on the stream route
answers `426` and says how to open one properly.

**Frames.** Both directions carry the protocol version, the session id, a
per-socket random generation and a strictly increasing sequence; audio carries
its own sequence as well. A frame from an earlier socket cannot be replayed into
a later one even with a valid ticket, because the generation will not match. The
device's first frame is always `hello`, which names the container, sample rate
and channel count every later `audio` frame will actually carry; after it the
device sends `audio` (with `last` marking the end of an utterance), `state`,
`interrupt` and `metrics`. The runner sends `ready`, `state`, `transcript`,
`assistant_delta`, `output_audio` and `error`.

An utterance may run to ninety seconds, which is far more than one frame may
carry, so it is uploaded as however many `audio` frames it takes and the runner
reassembles them in order. `metrics` names the utterance it measured and is sent
*before* that utterance's audio: the runner answers the instant an utterance
closes, so spans arriving behind it are dropped rather than credited to whatever
is said next.

**Where the work happens.** Voice activity detection is on the device and stays
there, so silence is never uploaded. The recorder is armed by confirmed speech
rather than by the session, so what is uploaded is an utterance and not the gap
before it — nor, during a barge-in, the answer coming out of the device's own
speaker. The runner transcribes with the operator's own configured backend,
submits the finalized transcript as an ordinary durable turn — same queue, same
session, same tools and approvals as a typed message — streams the answer back,
and speaks it on sentence boundaries. Talking over the answer stops the speech,
drops what has not been said and cancels the run; what a tool already did in the
world is not undone, and nothing claims it was. The audio that interrupted is
kept and becomes the next turn, because it is the next question and nobody
should have to say it twice.

**One conversation.** Talk speaks into the session the operator already has
selected in the controller's own chat surface, rather than minting one of its
own — so a spoken turn and a typed one are the same thread, and the message list
shows both. With no conversation selected there is nothing to speak into, and
Talk says so instead of starting.

**Revocation.** Withdrawing `voice_stream` closes the session in whichever phase
it lands — listening, transcribing, thinking or speaking — because the grant is
re-read on a timer, both while an answer streams and while the socket is simply
waiting for someone to speak, and once more when each turn ends. Never only when
the device happens to send a frame: a listening device has no reason to send
anything at all, so "at the next frame" would have left a withdrawn microphone
open until the socket's idle deadline a quarter of an hour later.

What survives a session is the transcript, the answer and bounded counters: how
many utterances, turns, interruptions and errors, and seven latency spans as
sample counts, means and worst cases. The audio is held for the length of one
utterance and dropped.

## At most once, and delivered at least once

The physical world has no transactions. A photograph cannot be un-taken, and no
protocol can make "the shutter fired" and "the runner knows it fired" happen
together. So the guarantee is stated in two halves, and both halves are real:

* **The physical effect happens at most once.** No disconnect, reload,
  reconnect, daemon restart, duplicate delivery or lost response can cause a
  second one.
* **Its result is delivered at least once.** A result that exists is retried
  until the runner acknowledges it, and the runner recognises a retry as a retry
  rather than as a second answer.

### The lifecycle

```
agent or operator invocation
  → arguments validated on the runner
  → one explicitly eligible device resolved
  → durable, idempotent command row            (queued)
  → the device is woken by push if it is away
  → the device reconciles its own journal first
  → grant / support / permission / readiness re-checked
  → lease                                       (leased)
  → durable start authorization, naming an execution
                                                (running)
  → the physical effect, at most once
  → the result and its bytes staged durably on the device
  → delivery retried until acknowledged
  → the runner persists the artifact, then the terminal row
                                                (succeeded | failed | cancelled | expired)
  → the result reaches the waiting run
```

The `leased` / `running` split is what makes the first half true:

* a lease that lapses **before** the device says it started is requeued, because
  nothing physical has happened;
* a `running` command is **never** requeued, and never handed to another device.
  When its deadline passes with no report it terminates as failed, with the
  effect explicitly recorded as unproven.

### The execution identity

`POST /v1/remote/device/commands/{id}/start` carries an `execution_id` the
device minted and journalled *before* asking. The runner answers:

| Situation | Answer |
| --- | --- |
| `leased` | `started: true` — and only this answer authorizes hardware |
| `running`, same execution | `started: false, recoverable: true` — the same attempt is reconnecting; it may deliver a staged result, and must not act |
| `running`, different execution | `409` — refused. Two executions of one physical command is exactly the failure being prevented |

### Recovering after a reconnect or a restart

`GET /v1/remote/device/commands/recover` returns only the nonterminal commands
this device already started. It is deliberately **not** a lease: handing a
running command back as work would be the second execution. The device answers
each from its own journal:

| Local journal | What happens |
| --- | --- |
| result staged | the exact saved result and bytes are delivered; nothing is re-executed |
| start authorized, no result | terminal failure, `execution_outcome_unknown_after_restart` — the effect **may** have happened, was **not** repeated, and cannot be proven either way |
| no record at all | same: reported unknown, never executed |
| only leased, never started | nothing to recover; the lease expires and the command is safely requeued |

On startup the device takes an exclusive executor lock, flushes its outbox,
reconciles, and only then leases new work. Exactly one browser tab per paired
profile is the executor; the others say so and do nothing.

### Crash and reconnect, step by step

| Crash point | What happens |
| --- | --- |
| before `start` | nothing was authorized; the lease lapses and the command is executed once, later |
| after `start`, before the effect | never re-executed. The outcome is reported unknown |
| after the effect, before the result is staged | the same: unknown, not repeated. This is the unavoidable window |
| after the result is staged | the staged result and its bytes are delivered on reconnect; the effect stays at one |
| after the runner persisted it, before the device saw the acknowledgement | the device retries; the runner recognises the identical report and returns the authoritative record |

### Where the bytes live

An artifact is held in the device's own `device_command_journal` object store —
its own store, not the profile record, because a profile row is rewritten on
every signed request and a multi-megabyte still has no business there. The bytes
are deleted **only** after the runner acknowledges them.

The journal is bounded by entry count, by total artifact bytes, and by a TTL on
acknowledged entries. An unacknowledged result is **never** evicted to satisfy a
bound. Instead, if there is no room to stage the result an artifact-producing
command might produce, the device refuses to start it and reports
`device_storage_full` — the alternative is taking a photograph and then choosing
between losing it and discarding somebody else's undelivered one.

On the runner, the bytes are written to a staging file, flushed, and renamed
atomically onto the command's artifact path *before* the terminal database row
is written. A crash between the two leaves an artifact with no row — recoverable
— rather than a row naming bytes that are not there. Stale staging files are
swept.

### Idempotency, in both directions

**Invocation → command.** A durable tool invocation names itself: the daemon's
job id and the agent loop's tool-call id, both supplied by the runtime and
neither visible to the model. The same invocation delivered twice returns the
command it already created; the same invocation asking for something *different*
is a conflict, not a replacement; a unique index enforces it across processes. A
manual CLI or desktop invocation carries no such identity and is never
deduplicated — two deliberate asks are two asks.

**Result → terminal row.** A terminal report is digested over its outcome,
result, artifact digest and error. The identical digest arriving again is a
retry: accepted, and answered with the authoritative record. A different digest
is refused with `409`, and neither the stored result nor its artifact file is
touched.

### Cancellation

A running command is watched over `GET /v1/remote/device/commands/{id}/control`,
a long poll that returns the moment a cancellation is asked for. What each
action does when it arrives:

| Action | On cancellation |
| --- | --- |
| microphone recording | stops recording promptly, stops the tracks, and reports what it *did* record — the microphone did open |
| voice stream | stops on the answer to its next chunk, closes the microphone and the session |
| audio playback | stops. This is one of the few cancellations that genuinely undoes something |
| location | the pending fix is abandoned; nothing observable happened |
| screen capture | aborted if the frame has not been taken; a taken screenshot is reported, not disowned |
| camera | aborted if the shutter has not fired; a photograph that exists is reported with its bytes |
| notification | a notification already shown cannot be unshown, and is not claimed to be |

The result says which of three things happened, and they are never collapsed:

* `cancelled_before_effect` — nothing happened;
* `cancelled_during_effect` — it started, was cut short, and what happened was
  not undone;
* the effect completed before the cancellation was observed — reported as the
  success it was.

A queued or leased command is cancelled outright on the runner, because nothing
physical has happened yet.

### Authority is re-checked at every boundary

Grant, support, permission, readiness, revocation and expiry are checked when
the command is queued, again when it is leased, and again at `start` — the last
boundary before hardware. A grant withdrawn or a permission revoked in between
stops the command with the reason attached, and no effect occurs. Authority lost
*after* the effect began cannot un-happen it, and is never reported as though it
had.

## What the paired device can do

The bundled controller reaches every capability the runner can grant it, and the
grant is what decides whether a surface appears at all:

| Grant | On the phone |
| --- | --- |
| `view_runs`, `view_events`, `read_artifacts` | the run list, the replayed timeline, artifact fetch by id |
| `approve`, `cancel`, `kill` | digest-bound approvals, cancellation, emergency stop |
| `pause` | pause and resume, offered one at a time by what the run actually is |
| `view_sessions`, `chat` | the paired conversation, and sending a message that becomes a durable run |
| `view_tasks`, `run_workflows` | the runner's declared workflows, and launching one |
| `capture` | filing a note or a file from the phone, digest-checked by the runner |

A device can also revoke *itself* — its key stops working at once and any live
session it owns is force-stopped, exactly as an operator revoke does.

`control_desktop` deliberately has no surface here: this device is the *subject*
of such a session, never its operator. A test asserts that every capability the
runner can grant either reaches a surface or is written down as having none, so
a capability added later cannot quietly become unreachable.

## Offline

The device caches its last view of the runner — runs, the selected run's detail,
its events, its approval metadata, the artifact metadata those events announce,
the sessions and their messages — and marks all of it stale, with the time it
was taken. Every cache is bounded by a count, and pruning happens on write.

Every control whose effect leaves the device is disabled while stale. Nothing is
buffered for replay, because an approval queued against a view the device cannot
refresh would act on a run it can no longer see.

A **draft** is the exception, and it is not an action: text typed into the
composer is kept on the device and restored on the next load, because nothing
has happened until it is sent.

The device-command result outbox is a different thing again, and the distinction
is worth stating plainly because it looks superficially like the queue that is
forbidden:

* a queued **action** would be a request somebody made against a view the device
  could not refresh, replayed later against a runner whose state has moved. That
  is why approvals, cancellations, workflow launches, chat sends, physical
  commands and permission changes are never buffered.
* a staged **result** is not a request at all. The effect already happened. The
  runner is waiting for it and will otherwise record it as unproven. Not
  retrying it would lose the only account of something that really occurred.

So the outbox retries on reconnect, on the network coming back and on focus,
with bounded exponential backoff — and nothing else does.

## Push

Push is optional, provider-neutral, and yours. Little Monkey ships no push
project, no key, and no relay.

The default backend is **Web Push**, and it needs no account anywhere:

```bash
monkey daemon remote push-configure --web-push
monkey daemon remote push-status
monkey daemon remote push-test <device-id>
```

That mints this runner's own VAPID identity, keeps the private half in the
system keychain, and hands the public half to the browser as its
`applicationServerKey`. Each notification is sealed to the device with RFC 8291
before the browser's own push service carries it — that service sees ciphertext
and an address, never a title or a body.

The browser controller offers an **Enable notifications** button once the runner
answers with a key. Turning it off unsubscribes the browser *and* tells the
runner to forget the address; leaving either half would keep a path open that
the user believes is closed.

The same settings are in **Settings → Background agents → Remote handoff**:
which backend is configured, whether it is on, whether a notification would put
specifics on a lock screen, which devices registered an address, and a **Test
push** per device.

A native client that holds its own Firebase registration token can use FCM
instead, against the operator's own project:

```bash
monkey daemon remote push-configure --project-id <your-project> \
  --service-account ./your-service-account.json
```

A notification says what kind of thing happened and which id it happened to —
never what it said — unless you pass `--include-detail`. A security alert is the
one exception, because an alert you cannot identify is not actionable.

A push grants nothing. It is a wake-up hint and nothing else:

```
push notification  ≠  authenticated command
                   ≠  capability grant
                   ≠  permission grant
```

A woken device reconnects, authenticates normally, reconciles whatever it was
already holding, and only then takes work. Nothing in a notification is trusted,
and default notification text carries no command arguments, message contents,
captured media, tool parameters or run output onto a lock screen.

What raises one:

* A run stops for an approval, finishes, or fails. The daemon watches its own
  job table for these, so a transition raises its notification whichever code
  path caused it — the scheduler, a reporting child, or the crash reconciler.
  A finished mobile chat turn says "new response" rather than "run finished".
* A device command is queued, which wakes the target device — without it, a
  phone with its screen off never reconnects to take the command.
* A device is revoked, which tells the devices that still work.

Notifications follow the edge, not the level: the same waiting approval seen on
every poll wakes a phone once, and a daemon restart does not re-send what it
already sent. A machine with no device registered for push decides nothing and
reads no keys.

## What the Security Doctor checks

`monkey security audit` reports which device may hear the room, which holds a
grant it has not used in a month, which is capturing right now, whether a
revoked device can still be woken, whether the transport those grants are
exercised over is pinned HTTPS, and whether push would put run specifics on a
lock screen. An open Talk socket shows up there as a running `voice_stream`
command, like any other capture in flight: the socket registers one when it is
admitted and closes it when it ends, so the audit is reading the socket's actual
lifetime rather than a guess. A runner that dies mid-conversation leaves a row
behind, and the daemon's own expiry sweep terminates it as unproven — a capture
that is reported open forever is an alarm nobody would believe.

## Checking a real device

The automated suite covers the protocol end to end against a simulated
executor. It cannot cover real hardware, so there is a command for that:

```bash
monkey daemon remote device-check                      # safe: no sensor is touched
monkey daemon remote device-check --dangerous          # camera, microphone, location, screen, notification, audio
monkey daemon remote device-check --device-id <id> --json
```

It reads the device's advertised surface, prints Granted / Supported /
Permission / Readiness / Effective per capability, runs `device_info` by
default, validates the shape of every answer — including an artifact's digest,
size and media type — and exits non-zero if anything fails. Capabilities that
are not effective are skipped and reported as skipped rather than hung on.

It needs no credentials, no account and no project: the pairing you already made
is the whole setup. It is deliberately not part of a normal test run, because a
photograph is not something CI should take.

### What CI checks instead

One thing between the simulated executor and a real phone *can* be automated,
and had to be: the response policy the runner serves the controller under. A
permissions policy with an empty allowlist — `camera=()` — disables the feature
for the document, and a CSP without `media-src` falls back to `default-src
'none'`. Both were once true of every response, so the controller was forbidden
the camera, microphone, location, screen capture and audio it implements — and
no frontend test could see it, because jsdom enforces neither header.

`pnpm test:browser-policy` serves the real controller files with the runner's
real header constants (parsed out of `web.rs`, not restated) and loads them in a
real browser engine, asserting that the document permits each API the client
calls and that both audio sources it loads — an artifact's `blob:` URL and the
`data:` silence that unlocks autoplay — are allowed. The same page under the
previous headers must report every one of them blocked, so the test can fail.

It proves what the browser *permits*, never what hardware *does*: nothing there
opens a device, and the permission prompt is never reached. Real capture stays
the manual check above.

## Troubleshooting

| What you see | What it means | What to do |
| --- | --- | --- |
| `not_granted` | the operator never granted it | grant it on the device's card, or at pairing time |
| `no_surface` | the device has never said what it can do | open the paired-device controller on the device once |
| `unsupported` | that build cannot do it at all | nothing here; a different client is needed |
| `permission_required` | the OS has not been asked | open the controller, use the control under Device readiness |
| `permission_denied` | the OS refuses | fix in the device's own system settings, then reopen the controller |
| `foreground_required` | the controller is not in front | bring it forward; it re-advertises on focus |
| `interaction_required` | autoplay policy, usually | tap *Enable audio playback* on the device |
| `screen_capture_not_armed` | nobody has shared a screen | tap *Allow screen capture*; it stays armed until stopped |
| `device_storage_full` | undelivered results are filling the journal | reconnect the device so it can deliver them |
| `outcome unknown — the action was not repeated` | the device was interrupted mid-action | the effect may have happened; check the device, then retry deliberately if needed |

## Limits

These are real platform limits, not things left undone:

* The bundled mobile client is the browser controller the runner serves. It uses
  public web platform APIs, so what it can do is what the browser exposes.
* `screen_capture` cannot be a durable permission: browsers grant screen sharing
  per session and forget it. The armed-stream model is the honest version of
  that — while a share is live, capture is ready; when the user stops it, it is
  not, and the surface says so at once.
* Camera, microphone, voice and location need the controller in the foreground.
  A backgrounded page on a phone is suspended, so a command queued against a
  hidden tab is refused with `foreground_required` rather than silently
  producing a black frame.
* Autoplay policy means remote audio cannot play until someone has interacted
  with the page. There is no way around this from a page, and pretending
  otherwise would make `audio_playback` fail in silence.
* There is one unavoidable uncertainty window: after the runner authorizes a
  start and before the device durably stages a result, a crash leaves an outcome
  nobody can determine. It is reported as unknown and never retried. No protocol
  can close this window for a non-transactional physical effect; what can be
  guaranteed — and is — is that the effect is not repeated.
* Location is a single fix. There is no continuous background tracking, by
  construction — the client never registers a watch.
* An artifact is bounded by the pairing's artifact budget and by the device's
  own advertised bound, whichever is smaller.

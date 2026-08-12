# Paired devices

A paired phone or tablet is already a *controller*: it can watch runs, approve
operations, and chat, within the scope its invitation froze. This document is
about the other direction — letting the runner ask that device's own hardware
for something, once, with the operator's explicit permission.

Nothing here is a new pairing system. It extends the existing remote trust:
the same one-time invitation, the same non-exportable device key, the same
signed, replay-guarded HTTPS identity, the same revocation.

## The three sets, and why they are separate

An action happens only where all three agree:

```
operator grant  ∩  advertised support  ∩  current OS permission
```

* **Granted** — what you gave this device at pairing time or afterwards.
* **Supported** — what the device's build can actually do. It reports this
  itself, and a claim here grants nothing: advertising a camera can only ever
  *narrow* what is effective.
* **OS permitted** — what the device's operating system currently allows.

They are shown separately everywhere — the desktop card, the phone's own
screen, `device-list` — because "why can this phone not take a photo" has four
different answers, and merging them into one list hides which one applies.

A device paired before this existed advertises nothing, so it keeps every
run-facing grant and gains no hardware access from an app update.

## Capabilities

`device_info`, `camera_capture`, `microphone_capture`, `location_read`,
`notification_post`, `screen_capture`, `audio_playback`, `voice_stream`.

None implies another. The one dependency is that `voice_stream` also requires
`microphone_capture` — a continuous stream is a superset of one bounded
recording, and without the rule, withdrawing `microphone_capture` would leave
the microphone reachable.

`voice_stream` is reserved for the Talk surface: it can be granted now, but no
discrete command dispatches it.

## Granting

```bash
monkey daemon remote pair-create --output invite.json --run <run-id> \
  --action view-runs --device camera_capture --device location_read --qr
```

`--qr` also prints a compact pairing code the phone can scan. It carries the
same one-time token and pins the same certificate by SHA-256 fingerprint; the
PEM is left out because that is what makes it small enough to read from a
screen. The JSON invitation, paste, and file flows all still work.

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

## Exactly once

A command's life is `queued → leased → running → succeeded | failed |
cancelled | expired`, and the split between *leased* and *running* is the whole
safety property:

* a lease that lapses **before** the device says it started is requeued, because
  nothing physical has happened;
* a *running* command is **never** requeued. When its deadline passes with no
  report it terminates as failed, with the effect explicitly recorded as
  unproven — a retry could take a second photograph.

A device that reconnects and re-posts `start` is told `started: false` and does
nothing. It also keeps a small local record of what it already did, so a browser
restart reports the old outcome rather than repeating it.

Cancellation is truthful. A queued or leased command is cancelled outright; on a
running one it raises a flag, and anything already captured stays captured.

## Offline

The device caches its last view of the runner and marks it stale, with the time
it was taken. Every control whose effect leaves the device is disabled while
stale — nothing is buffered for replay, because an approval queued against a
view the device cannot refresh would act on a run it can no longer see.

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

A native client that holds its own Firebase registration token can use FCM
instead, against the operator's own project:

```bash
monkey daemon remote push-configure --project-id <your-project> \
  --service-account ./your-service-account.json
```

A notification says what kind of thing happened and which id it happened to —
never what it said — unless you pass `--include-detail`. A security alert is the
one exception, because an alert you cannot identify is not actionable.

A push grants nothing: the woken device still makes an ordinary signed request
to learn or do anything.

Two things raise a notification today: queueing a device command wakes the
target device (without it, a phone with its screen off never reconnects to take
the command), and revoking a device tells the devices that still work. Approval
and run-completion notifications need a daemon-side watcher on the run event
stream, which does not exist yet — the payload kinds are defined and the
delivery path is real, but nothing raises them.

## What the Security Doctor checks

`monkey security audit` reports which device may hear the room, which holds a
grant it has not used in a month, which is capturing right now, whether a
revoked device can still be woken, and whether push would put run specifics on a
lock screen.

## Limits

* The bundled mobile client is the browser controller the runner serves. It uses
  public web platform APIs, so what it can do is what the browser exposes:
  `screen_capture` needs the browser's own consent prompt every time, and
  `audio_playback` speaks text rather than playing an arbitrary audio file.
* Location is a single fix. There is no continuous background tracking, by
  construction — the client never registers a watch.
* An artifact is bounded by the pairing's artifact budget and by the device's own
  advertised bound, whichever is smaller.

# Voice Everywhere

Voice Everywhere lets one Talk session independently choose where speech is captured and where synthesized speech is played. The route belongs to the ordinary Little Monkey conversation; moving a microphone or speaker does not create a second chat, model context, permission system, or pairing identity.

```text
local:input:<media-device-id>          paired:<device-id>:input
             │                                    │
             └────────────────┬───────────────────┘
                              ↓
          VoiceRoute for one conversation, generation N
          input endpoint · output endpoint · engine
                              ↓
            ┌─────────────────┴─────────────────┐
            ↓                                   ↓
      Pipeline engine                     Realtime engine
      host STT                            provider session
      ordinary durable agent turn         (the peer connection
      host TTS                             lives in the desktop
                                           webview; no STT)
            └─────────────────┬─────────────────┘
                              ↓
            ┌─────────────────┴─────────────────┐
            ↓                                   ↓
local:output:<media-device-id>         paired:<device-id>:output
```

Either engine reads the same route. Which endpoints are local and which are
paired changes how the audio travels, not which conversation it lands in.

## Supported routes

Both engines accept the same endpoint combinations:

- local microphone → local speaker
- paired-device microphone → local speaker
- local microphone → paired-device speaker
- paired-device microphone → the same paired-device speaker
- paired-device A microphone → paired-device B speaker

Local endpoint ids are `local:input:<media-device-id>` and `local:output:<media-device-id>`. Paired endpoint ids are `paired:<device-id>:input` and `paired:<device-id>:output`.

A route is conversation-scoped and carries a monotonically increasing generation. Every paired Talk admission is bound to the host-selected conversation, route id, generation, and role (`input`, `output`, or `duplex`). A device cannot replace the host-selected conversation id. A stale generation is rejected both when the one-use Talk ticket is issued and again when it is consumed.

## Media paths

Voice Everywhere adds no new remote-device transport: a paired endpoint is reached
over the Talk WebSocket it already had. What follows is the Pipeline engine; the
Realtime engine reuses the same socket but carries different bytes on it, which is
described under [Realtime Voice](#realtime-voice).

For a paired microphone, the existing authenticated Talk WebSocket carries device-side-VAD utterance audio to the host. The host performs the configured Pipeline STT and injects the resulting transcript into the same `runAgentTurn(..., 'voice')` path used by local Talk.

For a paired speaker, the existing Talk WebSocket is opened in an output-only role. The device does not request microphone access. The host synthesizes with the configured Talk TTS and sends `output_audio` on that socket. The device acknowledges playback only after the chunk finishes or fails, providing bounded backpressure. When the same paired device is both microphone and speaker, a single duplex Talk socket is reused.

Raw or base64 audio is never written to the VoiceRoute coordination ledger. The SQLite route/event tables store only route state and bounded coordination metadata such as transcripts, assistant text deltas, interruption state, readiness, and playback acknowledgements.

## Handoff and stale media

Changing a live route uses generation changes so delayed frames from an old endpoint cannot become current again. Before committing a new microphone owner, the previous paired input command is cancelled and must retire. If destination activation fails, the previous endpoints are restored under a fresh generation rather than rewinding the old one.

Stopping Talk deactivates the route-owned paired roles. Revoking a paired device, withdrawing a required grant, losing the required browser readiness, or changing the route generation invalidates the corresponding Talk authority and closes the role fail-closed.

## Capability and readiness rules

Paired input requires effective `voice_stream`; paired output requires effective `audio_playback`; duplex requires both. “Effective” means the existing operator grant, the device-advertised capability, OS/browser permission where relevant, and current readiness all agree.

Browser restrictions remain real. Microphone permission may require the paired controller to be in the foreground, and audio playback may require a user gesture before the browser can make sound. The desktop selector reports those states instead of pretending a selected endpoint is usable.

## Realtime Voice

Voice Everywhere does **not** pass paired Realtime audio through Whisper. The Realtime engine has no STT stage at all, and routing a paired microphone into it does not quietly reintroduce one.

Local Realtime endpoints keep the existing WebRTC path unchanged: the selected microphone is the peer connection's own input track, and the answer plays through the local audio element on the selected output device. No bridge is created for a route whose endpoints are both local.

A paired endpoint cannot join that peer connection — the provider session is negotiated by the desktop webview, and a phone in the next room has no way to become one of its media tracks. So paired Realtime audio is relayed *to* that webview rather than routed around it. The daemon binds a loopback-only listener on `127.0.0.1:0` and writes its address and a fresh 256-bit token into a permission-restricted `realtime-host-media.json`, rewritten on every daemon start; the desktop reads that descriptor through its own Tauri command and refuses a descriptor that is not loopback, not protocol version 1, or whose token is not 64 hex characters.

**Paired microphone.** The device's Talk socket is admitted in the Realtime dialect: its `hello` must declare mono 24 kHz PCM16 or the socket is refused with `realtime_pcm_required`, and a frame marked `last` is refused too, because Realtime PCM is a continuous stream and not a closed utterance. Each chunk is re-checked against the live route — engine, role, generation, grant, and that this device is still the selected microphone — before it enters a bounded in-memory queue. The desktop polls `GET /v1/host/realtime/input` on the loopback listener with the process-local token, naming the route in `x-little-monkey-route-session` and `x-little-monkey-route-generation` headers rather than in the path — a URL is the part of a request that gets written down, and a conversation id logged beside an audio stream is a record of who was talking and when. The route is validated in the header exactly as strictly as it was in the path, and the queues stay keyed by session and generation and appends what it gets to the provider session over the data channel, so a paired microphone arrives as appended PCM rather than as a media track. Under manual turn detection the desktop closes and opens the device's microphone with an `input_gate` event it waits for the device to acknowledge, rather than shipping audio the provider would hear anyway.

**Paired speaker.** The desktop detaches and mutes its own playback element, taps the remote WebRTC stream, resamples it to 24 kHz PCM16 and `POST`s it to the same loopback path; the daemon delivers it as ordinary `output_audio` frames on the device's Talk socket. A barge-in `DELETE`s the queue, so audio the provider has already produced but the device has not played is dropped rather than spoken over the next question.

Nothing here touches the VoiceRoute ledger. Audio exists only in those bounded queues — 64 KiB per chunk, 128 chunks or 1 MiB per direction, at most 32 route queues with the least recently touched one evicted — and on the loopback socket. Every ingress and egress re-reads the route, so media from a moved, deactivated or revoked route is refused — `409` on the loopback side, an error frame and a closed role on the device side — instead of becoming current again.

**What this costs.** Because the provider peer lives in the desktop webview, a paired Realtime route needs the desktop app open and hosting that session: the daemon alone cannot serve one, and the loopback descriptor is reachable only from the desktop process. Paired Realtime audio therefore makes a round trip through this machine — device → daemon queue → webview → provider, and back the same way — which is a hop local Realtime does not pay, and both directions inherit the queue's backpressure limits. If the bridge fails, the session is closed and the route deactivated as `paired_media_bridge_failed`; it is never degraded to the Pipeline engine behind your back.

## CLI

Inspect endpoints:

```bash
monkey voice endpoints
monkey voice endpoints --json
```

Create or replace a route for an ordinary conversation without opening hardware yet:

```bash
monkey voice route set <session-id> \
  --input local:input:default \
  --output paired:<device-id>:output \
  --engine pipeline
```

`--engine` takes `pipeline` or `realtime` and nothing else. `route set` records a
selection; it opens no hardware, so a route can be prepared before Talk starts.

`monkey voice route activate|deactivate|emit` exist for the desktop's own bridge and
are hidden from `--help` for that reason: Talk drives them itself. They are named
here because they are what turns up in a support session, not because they are
something to run by hand.

Move one or both endpoints while Talk is live:

```bash
monkey voice route move <session-id> --input paired:<device-id>:input
monkey voice route move <session-id> --output local:output:default
```

Inspect or stop the route:

```bash
monkey voice route get <session-id> --json
monkey voice route events <session-id> --json
monkey voice route stop <session-id>
```

## Manual end-to-end matrix

Before testing paired routes, pair the devices, grant the required physical capabilities, allow microphone permission on input devices, and enable browser audio playback on output devices.

For each topology below, use the same existing desktop conversation and verify that a spoken turn appears in that conversation, tools/approvals behave as they do for typed turns, and the answer is heard only at the selected output:

1. local → local
2. paired → local
3. local → paired
4. paired → same paired
5. paired A → paired B

Run 1 and at least one paired topology on `--engine realtime` as well, with the
desktop app open: confirm the answer is spoken only at the selected output, that
no transcript appears from a speech-recognition backend that was never invoked,
and that stopping Talk retires the paired roles. A desktop app killed outright
does not get to run that teardown; what closes the device's microphone then is
the socket's own idle deadline or a withdrawn grant, not the app.

Then move the input during an idle listening period and move the output during/after a response. Verify the previous microphone closes before the new one becomes active, stale speech does not continue on the old speaker, the conversation id does not change, and revoking either required capability immediately makes that endpoint unusable.

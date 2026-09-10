# Voice Everywhere

Voice Everywhere lets one Talk session independently choose where speech is captured and where synthesized speech is played. The route belongs to the ordinary Little Monkey conversation; moving a microphone or speaker does not create a second chat, model context, permission system, or pairing identity.

## Supported Pipeline routes

The classic Pipeline engine supports these endpoint combinations:

- local microphone → local speaker
- paired-device microphone → local speaker
- local microphone → paired-device speaker
- paired-device microphone → the same paired-device speaker
- paired-device A microphone → paired-device B speaker

Local endpoint ids are `local:input:<media-device-id>` and `local:output:<media-device-id>`. Paired endpoint ids are `paired:<device-id>:input` and `paired:<device-id>:output`.

A route is conversation-scoped and carries a monotonically increasing generation. Every paired Talk admission is bound to the host-selected conversation, route id, generation, and role (`input`, `output`, or `duplex`). A device cannot replace the host-selected conversation id. A stale generation is rejected both when the one-use Talk ticket is issued and again when it is consumed.

## Media paths

Voice Everywhere does not add another remote-device transport.

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

Voice Everywhere does **not** silently pass paired Realtime audio through Whisper. The current provider Realtime implementation is WebRTC-bound to the desktop browser media tracks, so paired endpoints fail closed for Realtime until a direct provider media bridge is available. Local Realtime input/output continues to use the existing WebRTC path.

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

Activate/deactivate paired roles when Talk starts or stops:

```bash
monkey voice route activate <session-id>
monkey voice route deactivate <session-id>
```

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

Then move the input during an idle listening period and move the output during/after a response. Verify the previous microphone closes before the new one becomes active, stale speech does not continue on the old speaker, the conversation id does not change, and revoking either required capability immediately makes that endpoint unusable.

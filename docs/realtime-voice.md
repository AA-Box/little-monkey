# Desktop realtime voice

Little Monkey has two explicit desktop Talk engines. **Classic pipeline** is
the default and preserves the existing microphone → local VAD → STT → normal
agent turn → TTS → speaker flow. **Realtime WebRTC** is an opt-in provider
session. Selecting one never silently falls back to the other.

| | Classic pipeline | Realtime WebRTC |
|---|---|---|
| Model path | STT (local/cloud) → ordinary selected local/cloud chat model → TTS | Native provider audio model |
| Privacy | Can remain entirely local | Live microphone audio goes to OpenAI after acknowledgement |
| Requirement | Installed/configured STT, chat model, and TTS | OpenAI key, network, WebRTC, microphone permission |
| Tools | Normal agent-loop tools | Same executor/permissions, bridged from provider function calls |
| Wake phrase | Optional and local-only | Unsupported |
| Devices | Selected microphone and speaker | Selected microphone and speaker |
| Latency | Turn-based stages | Duplex streaming with barge-in |
| Offline | Available when all selected stages are local | Unavailable |

## Architecture and protocol

The frontend talks to a provider-neutral contract: provider, session,
capabilities, normalized events, tool calls, and bounded metrics. The OpenAI
adapter implements that contract with one `RTCPeerConnection`, one microphone
track, one remote audio element, and the `oai-events` data channel.

The browser creates an SDP offer and sends it to a native command. The native
broker reads the ordinary OpenAI key from the existing OS-keychain provider
store and posts a multipart `sdp` + `session` request to the compiled-in
`https://api.openai.com/v1/realtime/calls` endpoint. It returns only the SDP
answer and provider request id. The API key is never returned to JavaScript,
placed in a URL, local/session storage, a transcript, or a metric. Custom
realtime provider origins are not accepted.

HTTP signaling uses the existing hardened native egress client. After the SDP
answer is accepted, encrypted WebRTC media follows provider-negotiated ICE/DTLS
endpoints; browser WebRTC cannot route that media through the HTTP egress
allowlist. Those endpoints come only from the signed-in provider exchange, not
from model output or user-configurable URLs. Reconnect repeats the same fixed
signaling origin and does not broaden provider policy.

The session enables input transcription, audio output, function tools, and
either semantic server VAD or manual push-to-talk. The default model is
`gpt-realtime-2.1`. With semantic VAD, provider
interruption is enabled. A local Stop sends both `response.cancel` and
`output_audio_buffer.clear`. With WebRTC, OpenAI owns the playback buffer and
tracks what was heard: semantic-VAD interruption automatically removes unplayed
audio, and clearing the output buffer also truncates the conversation. No
client-estimated audio clock is substituted for that authoritative playback
position. Speech-start also pauses the local audio element immediately. Manual
turns clear the input buffer before capture and commit it before
`response.create`.

## Durable transcript and context

Final input and output transcripts enter the same saved chat session as typed
turns. Provider event/item ids are stored as local-only provenance and stripped
from ordinary model requests. This makes repeated provider events and reconnect
replay idempotent. Each finalized user transcript creates a normal durable run
with a frozen OpenAI/model target; output and tool lifecycle events are added to
that run.

A new or reconnected provider session receives only the last 12 user/assistant
messages, capped at 8 KB, as a read-only context snapshot. Audio and secret
values are never used for synchronization. The saved Little Monkey transcript,
not the provider conversation, is authoritative.

## Tools and approvals

Tools are frozen when the realtime session starts from the normal base,
workspace, MCP, extension, settings, and permission-mode filters. A provider
function call is normalized to Little Monkey's standard `ToolCall` and executed
by the existing tool executor. That executor owns JSON-schema checks,
permissions, plan-mode refusal, workspace confinement, sandboxing, network
policy, MCP/extension routing, hooks, cancellation, and error formatting.

The provider receives exactly one function result for an allowed call, denied
approval, malformed arguments, unavailable tool, cancellation, or runtime
error. Completed provider item ids are persisted, so the tool is not executed a
second time after reconnect. Realtime voice does not offer nested-agent or
skill-invocation tools because it is already the active agent loop.

## Lifecycle, privacy, and observability

Talk shows a privacy acknowledgement before opening a realtime session. Ending
Talk, closing its surface, page teardown, microphone revocation/removal,
peer failure, and failed negotiation all close the data channel and peer,
remove listeners, stop every microphone track, detach remote audio, and notify
the native broker. A transient disconnect gets one reconnect attempt to the
same provider; an approval/tool call in flight is never replayed. Further
failure is visible and requires an explicit retry.

Realtime metrics are a separate private, atomically written ring of the last 100 sessions:
connection, first recognized speech, first model event, first audio, end-to-end,
interruption, tool round trips, output underruns, reconnect count, and a short
error code, plus provider-reported input/output token totals. They contain no audio, transcripts, tool arguments/results, URLs,
or credentials. Security Doctor separately reports whether realtime voice is
configured and whether a session is active.

The existing daemon/paired-phone Talk WebSocket and the companion
`realtimeBackend` live-call extension setting are different features. They do
not route through, configure, or fall back to this desktop WebRTC engine.

## Verification

Unit tests cover state transitions, duplicate events, interruption, errors,
reconnect state, OpenAI event translation, SDP broker shape, full resource
teardown, and allowed/refused/error/duplicate tool results. Rust tests cover
the fixed-provider validation and session payload. A real-provider smoke test
is present but opt-in because it requires a saved OpenAI key, a desktop-hosted
test runner, network access, and microphone permission:

```text
LITTLE_MONKEY_REALTIME_INTEGRATION=1
```

For manual acceptance, select **Settings → Talk → Realtime WebRTC**, confirm
the provider status, choose devices, accept the privacy warning, start Talk,
interrupt a long answer, approve and refuse tool calls, remove the microphone,
and confirm Security Doctor changes from configured to active and back.

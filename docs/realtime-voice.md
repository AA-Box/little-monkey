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

Because the spoken audio travels on the media track rather than the data
channel, three different facts are reported separately and never conflated,
each at the layer that can actually observe it.

*Generation* is a data-channel signal: an audio delta if one arrives, otherwise
the first transcript delta. It reports that the model began producing output and
drives the responding state, and it says nothing about sound.

*Non-silent audio at the receiver* is an `inbound-rtp` measurement. Accumulated
audio energy is the only figure that separates sound from silence, and it is the
only one accepted: bytes and packets keep flowing for a track carrying silence,
and `totalSamplesReceived` counts samples whether or not they hold any signal,
so treating it as a fallback would let a silent stream certify itself. This is
receive-side, so first-audio latency excludes whatever the output device adds.
It is also standard but not universal — WebKit's `getStats` is narrower than
Chromium's, and this ships to WKWebView and WebKitGTK as well as WebView2 — so
each reading records whether the figure was reported at all. A webview that
reports none is described as unable to tell sound from silence, naming the
webview rather than the application, and never as a silent speaker.

*The local playback path* is the audio element: whether `play()` resolved,
whether it is paused, and whether its position advances. That is the end of what
the application can see. The output device, the OS mixer, and the physical
speaker are past it, so nothing here is ever reported as proof that a person
heard anything. The live run asks the operator two questions instead — whether
the answer was audible, and whether it stopped the moment it was interrupted —
and a run whose every measurable step passes is still only unverified until both
are answered. There is deliberately no flag that can answer them: a
confirmation a script can assert is not a confirmation.

Barge-in is judged at the layer `interrupt()` acts on: the element stops and its
position stops advancing. Inbound RTP is not `interrupt()`'s to stop — packets
already in flight keep arriving and being decoded after playback has ceased — so
judging the stop on receiver energy would condemn a barge-in that did exactly
what was asked. Continued inbound audio is reported alongside the pass as the
expected settling it is.

Interruption is counted the same however it starts. A local Stop and a
provider-VAD barge-in both produce one normalized interruption, so the metric
does not silently omit the automatic case — while the operator's first utterance
of a turn, with nothing in flight, is not counted as interrupting anything.

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
error.

A function result on its own does not make the model speak: after every
`function_call_output` the client must send a fresh `response.create`. That ask
is driven by a per-response ledger rather than by a "tools still running"
counter, because a fast tool can settle before `response.done` is processed.
The ledger records which function calls a response asked for and which are
answered, and answering one settles as `continue`, `wait`, or `closed` — so the
continuation is sent exactly once, by whichever of `response.done` and the last
tool result arrives second, and a response that will never speak again still
closes its durable turn out. A response the provider reports as `cancelled` or
`failed`, and any response abandoned by a barge-in, is never continued, so a
late tool result cannot resurrect an interrupted answer. With provider VAD the
model can open a response of its own while a tool is still running; the
provider refuses a second concurrent response, so a continuation asked for
during one is held until that response finishes.

Each executed call is persisted against two identities. The provider item id
makes a replayed provider event harmless. A host-side identity — the spoken
turn plus a digest of the tool name and its canonical arguments — makes a
*reconnect* harmless: the replacement session is a different provider
conversation with different item ids, so item ids alone cannot tell that the
model is asking again for something the host already did. The turn identity
survives the reconnect, and the stored result is returned instead of executing.
That identity is scoped to a *different* provider session on purpose: inside
one session the model may legitimately call the same tool with the same
arguments twice — read a file it just wrote, re-run a check — and only the item
id may deduplicate there.

A call dispatched by an earlier session of the turn whose outcome was never
recorded is refused with an explanatory result rather than reissued, for every
tool rather than only the ones a read-only marking calls dangerous: the
frontend has no per-tool idempotency contract complete enough to bet a side
effect on. The operator clears that refusal by asking again, which starts a
fresh turn and runs normally. A reconnect is not blocked merely because a tool
is executing — that is the window this identity exists for — only while a
permission prompt is actually waiting for a decision.

Realtime voice does not offer nested-agent or skill-invocation tools because it
is already the active agent loop.

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

Provider-independent tests cover the state machine and the response ledger
(server and manual VAD, interruption during speech and during tool execution,
response cancellation and failure, stale and duplicate provider events, failed
reconnect, credential rejection and retry, device removal, microphone
revocation), OpenAI event translation, response pacing, the separation of
generation from measured playback, the SDP broker shape,
full resource teardown, and the tool boundary across a reconnect: a reissued
operation under a new provider session and item id is not executed twice, while
an ordinary repeat inside one session still runs. Two of those tests are named
for the defects they pin — the tool result that landed before `response.done`,
and the reconnect that executed a tool twice — and both fail against the code
that had them. Rust tests cover the fixed-provider validation and the session
payload.

The real-provider acceptance runs the app itself, because a real microphone,
real WebRTC, a real speaker path, the native keychain broker, and the ordinary
tool executor only exist together in the desktop webview:

```bash
pnpm test:realtime:live --path README.md
```

It needs an OpenAI key saved through Settings, a workspace open on that file,
network access, and microphone permission. The app starts with the acceptance
harness enabled, prints `Speak now`, and the operator asks it once to read the
file. The harness drives the same response ledger and the same tool bridge the
product uses and reports twelve steps — provider configured at the fixed egress
origin, session connected, microphone audio reached the provider, transcript
persisted, tool call bridged to the normal executor, host result returned,
spoken follow-up after that result, non-silent audio measured at the receiver,
the local playback element advancing unpaused, barge-in proven by local playback
stopping, durable conversation rows, clean disconnect. It then asks whether the
answer was audible and whether it stopped the instant it was interrupted,
reporting each as PASS, FAIL, or UNVERIFIED; only two confirmed yeses exit zero,
anything unwitnessed exits 2, and any failure exits 1, so a green exit cannot
close the definition of done on its own. The native side writes the evidence and exits, so the run cannot
pass on a webview that stalled. The report carries step outcomes and bounded
timings only: no transcript, audio, file content, or credential. The same
harness is exercised in CI against a scripted provider, including the case
where the provider never speaks after a tool result — the harness fails that
run rather than hanging.

For manual acceptance, select **Settings → Talk → Realtime WebRTC**, confirm
the provider status, choose devices, accept the privacy warning, start Talk,
interrupt a long answer, approve and refuse tool calls, remove the microphone,
and confirm Security Doctor changes from configured to active and back.

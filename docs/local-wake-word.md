# Local wake-word detection

Desktop Talk uses a real streaming keyword spotter. It does not transcribe
ambient speech and then search the transcript.

```text
getUserMedia (explicit OS permission)
  -> AudioWorklet mono PCM
  -> stateful 16 kHz resampler
  -> bounded ring buffer + bounded drop-oldest queue
  -> native sherpa-onnx keyword spotter
  -> wake event with keyword-end sample
  -> post-keyword Talk/VAD capture on the same microphone
  -> built-in local Whisper
  -> ordinary durable agent turn
  -> existing speech output
  -> re-arm, if Always Listening is still enabled
```

While the state is `armed`, only the local keyword spotter receives PCM.
Whisper, providers, agents, and the run ledger receive nothing. The ring exists
only in renderer memory, has a fixed capacity, and preserves the command that
starts in the same worklet frame as the wake word. A slow native consumer gets
recent audio: the queue drops old frames and increments a bounded counter
instead of accumulating PCM.

The explicit states are `off`, `starting`, `armed`, `wake_detected`,
`capturing_command`, `transcribing`, `thinking`, `speaking`, `interrupted`,
`rearming`, and `error`. The UI cannot show `armed` until the microphone grant
exists, the track and AudioWorklet are live, the model files verify, the runtime
opens, and the native session accepts audio. A stop, closed Talk surface, ended
track, revoked grant, or Always Listening switched off disarms native inference
and closes the microphone (see [Turning it off](#turning-it-off)).

During an active exchange, speech over `thinking` or `speaking` is barge-in: it
stops playback, drops queued speech, requests best-effort turn cancellation,
and captures the interruption as the next turn without requiring another wake
word. The session returns to `armed` only after that turn and its speech queue
finish.

## Backend and assets

The backend is sherpa-onnx's native open-vocabulary keyword spotter. Custom
phrases are tokenized through the bundled GigaSpeech BPE vocabulary; they do
not require one model per phrase. This build accepts 1–128 bytes of English
letters, numbers, spaces, apostrophes, and hyphens. Sensitivity 0–100 maps to a
sherpa score threshold from 0.60 (strict) to 0.10 (sensitive).

Pinned components:

- Rust and native runtime: `sherpa-onnx` / `sherpa-onnx-sys` 1.13.3, static.
- Model: `sherpa-onnx-kws-zipformer-gigaspeech-3.3M-2024-01-01`.
- Tokenizer: pure-Rust `sentencepiece-rs` 0.2.2. There is no Python process or
  sidecar.
- Runtime model payload: 14,129,713 bytes across encoder, decoder, joiner,
  tokens, and BPE. The bundle also carries the archive's 726-byte README.

### Placing the keyword in time

sherpa's `KeywordResult` reports token timestamps relative to the decoding
segment it is in. In 1.13.3's keyword API `start_time` is left at zero, there is
no processed-frame counter, and the decoder starts a new segment on trailing
silence without reporting it. A session armed through a stretch of silence —
which is what Always Listening is — therefore reports a keyword that appears to
have ended near the start of the session.

So the timestamp is treated as a hypothesis, not a fact. The keyword cannot have
ended after the audio that revealed it, and a real decoder lag is bounded by the
model's 16-frame chunk; a hypothesis that fails either test is a segment origin
that drifted, and the command is taken from the end of the frame in hand
instead. Both branches are safe for the boundary this module exists to hold. The
cost of the fall back is precision: up to one decode chunk of the command's
first moments can be lost when the keyword is spotted late, because the position
the ring would need is not one this API can prove. Trigger latency is only
recorded for detections whose position was provable, so the reported average is
never the fall back's zero.

The 1.13.3 runtime is deliberate. sherpa-onnx 1.13.4 and newer currently hit a
native ONNX Runtime abort on SME-capable Apple Silicon; the upstream report is
[k2-fsa/sherpa-onnx#3791](https://github.com/k2-fsa/sherpa-onnx/issues/3791).
The version is exact-pinned until that regression has a verified fix.

`scripts/stage-sherpa-runtime.mjs` has an exact byte count and SHA-256 for the
official static archive for macOS, Linux, and Windows on both arm64 and x86_64.
The vendored build hook authenticates a staged or freshly downloaded archive
again before extraction, records a verification marker beside the extracted
libraries, and refuses any target without an approved digest. This also adds
the official Windows arm64 archive mapping missing from the upstream 1.13.3
Rust build script.

`scripts/stage-wake-word-model.mjs` authenticates the exact official model
archive, extracts into a transaction directory, verifies every runtime file
again, and atomically swaps two verified directories into place: the six
runtime files into `src-tauri/resources/local-wake-word/`, and the two pinned
test WAVs into `src-tauri/resources/local-wake-word-fixtures/`, which nothing
packages. Tauri bundles the model directory whole
(`resources/local-wake-word/**/*`), so "the installer carries no test audio" is
a property of what that directory contains rather than a list of six filenames
somebody has to keep in step with the model. A production install works offline
on first launch. Missing, partial, wrong-size, or wrong-digest files report
`unavailable`; application code never downloads a wake model at runtime.

That directory also carries one tracked file, `PLACEHOLDER.md`, which staging
never overwrites. `tauri-build` resolves packaged resource paths at compile
time, so a directory that matches nothing fails every `cargo build` in a fresh
checkout — including `cargo test`, before a single test runs. The same
arrangement already exists for the bundled Whisper model.

The sherpa-onnx code/runtime is Apache-2.0. Upstream publishes a `no-tts`
static archive that contains no Piper, eSpeak NG or ucd at all, and this build
pins it for five of the six targets — so their separate licensing obligations,
eSpeak NG's GPL-3.0 among them, are not imported rather than merely unlinked.
1.13.3 published no `no-tts` static archive for Linux arm64, so that one target
still carries them; its link list excludes them, and
`vendor/sherpa-onnx-sys/src/little_monkey_tts_stubs.cc` answers for the two
symbols `sherpa-onnx-core` references anyway, aborting if either is ever
reached. Excluding the libraries without answering for their symbols is not
enough on its own: it links on macOS and fails under `lld`. Upstream tracks
removing them from the default archive in
[k2-fsa/sherpa-onnx#3731](https://github.com/k2-fsa/sherpa-onnx/issues/3731).

The Windows archives are the `MD` variants. MSVC refuses to mix C runtime
models, and the rest of this build — Rust and `whisper-rs-sys` included —
compiles against the dynamic CRT, so the `MT` archive fails the link with
`LNK2038` rather than anything to do with wake words.
The model archive's own README
declares `Apache License 2.0`, which is the redistribution metadata used for
bundling. Upstream also has an open request for clearer model-license
provenance, [k2-fsa/sherpa-onnx#3760](https://github.com/k2-fsa/sherpa-onnx/issues/3760);
that ambiguity is recorded here rather than upgraded into a stronger legal
claim.

## Settings, privacy, and measurements

Wake Word and Always Listening are separate, off-by-default controls. Enabling
Always Listening requires a confirmation that the microphone remains open,
detection is local, passive audio is not retained, and full transcription
starts only after a wake event. The Rust configuration refuses Always
Listening when wake gating is off, the backend is not the local KWS backend, or
post-wake transcription is not local Whisper.

### Turning it off

Always Listening is the only reason this application ever opens a microphone
nobody pressed anything for, so switching it off closes that microphone —
whichever surface it is switched off on, and with no further click.

The setting is read once, when a Talk surface builds its engine, and that
surface can be open for hours; a panel armed since breakfast is exactly the
session an operator goes to Settings to turn off. So `m7_config_save` announces
every saved configuration as `m7://config-changed`, and `useTalkSession` — the
hook that owns the devices, shared by the Talk panel and the chat composer's
Talk button — re-reads the configuration when one arrives. If Always Listening
is now off, it stops the engine, closes the native keyword-spotting generation,
stops the media tracks, tears down the worklet and audio context, replaces the
ring and the wake queue, and hands the capture grant back. Reacting inside the
Settings switch or the panel's own **Stop listening** button would only cover
the surface holding that control; every other route to the same save — the
other panel, a second window, an imported configuration — would have left a
live microphone behind.

Two deliberate asymmetries:

- **A press is not the setting.** Talk opened from the composer was asked for
  directly, and revoking a setting the operator did not use to open it does not
  retract that ask. Only the microphone Always Listening opened is closed by
  Always Listening going away.
- **Turning it on does not open one.** The event arrives in the background, and
  the engine on screen was built without wake gating, so acting on it would
  open an *ungated* microphone. Arming waits for the next mount, with the wake
  word compiled in.

A configuration that cannot be re-read is treated as revoked rather than as a
reason to keep listening on the strength of a stale copy.

Settings reports the actual native backend, exact runtime/model identifiers,
verification/load/accepting state, payload bytes, detections, dropped frames,
average native inference time per submitted frame, average trigger latency,
resident model memory, idle CPU while armed, and the number of false triggers
the operator reported.

The last three are measurements, with the scope their labels say. Resident model
memory is the process's resident growth across the one model load — not the
model's file size, and reported as unmeasured where the platform will not answer
or where the process peak was already above the loaded model. Idle CPU is
whole-process CPU across the armed window, which is the number this feature
exists to keep small; below a second the operating system counter's own quantum
dominates the ratio, so there is no reading rather than a loud wrong one. Both
are sampled only when the panel asks, so measuring the idle cost never becomes
the poll that changes it. Inference is driven only by incoming worklet frames;
there is no polling loop anywhere in the path.

**That was not me** records one false wake. The count is all that is kept: not
the audio, not the phrase, not the time. It is the only signal the operator can
give that the threshold is too low, and lowering Sensitivity is the fix it
points at.

Test Wake Word opens the microphone and starts the same Rust manager, tokenizer,
model, threshold, queue, and PCM path used by Talk. Microphone volume cannot
make the test pass.

Passive PCM and decoder tokens have no log, analytics, database, diagnostic,
support-bundle, crash-report, artifact, or run-ledger representation. Native
Whisper logging hooks are installed before inference so the post-wake decoder
does not print token text to stderr. Only aggregate identifiers, counts, and
durations are retained in memory.

Security Doctor emits four independent findings: wake enabled, Always
Listening enabled, wake processing local/non-local, and passive audio network
path absent/present. A passive off-device path is Critical. The only supported
configuration is local and therefore reports the network path absent.

## Build and acceptance

```sh
pnpm stage:sherpa-runtime
pnpm stage:wake-word
pnpm test:wake-assets

LITTLE_MONKEY_KWS_E2E=1 \
  cargo test --manifest-path src-tauri/Cargo.toml --lib \
  local_wake_word::tests::real_model_detects_positive_audio_and_rejects_negative_audio

LITTLE_MONKEY_WAKE_TO_WHISPER_E2E=1 \
  cargo test --manifest-path src-tauri/Cargo.toml --lib \
  local_wake_word::tests::real_wake_event_feeds_only_command_pcm_to_local_whisper

pnpm test:wake-walkthrough
```

The deterministic real-model fixtures cover the configured wake word, no wake
word, a similar but incorrect phrase, an immediate command, an inserted pause,
and deterministic background noise. The second test drives the production
manager, rejects audio from its stopped generation, writes only samples after
the native keyword-end timestamp, and transcribes them with the real bundled
Whisper model while asserting the wake phrase is absent. A third pushes roughly
twenty seconds of silence and unrelated speech through one armed session before
the phrase, which is the only shape in which a drifted segment origin shows
itself.

### The acceptance walkthrough

The fifteen-step acceptance script is executed rather than described.
`src/lib/wakeWordWalkthrough.e2e.test.ts` drives the real `TalkSession` — the
same class the chat window constructs — through the real ring, queue, resampler
and WAV encoder, against the real native keyword spotter and the real bundled
Whisper running in `src-tauri/src/bin/wake-word-e2e.rs`. Configuration steps go
through the operator's own save-time validator. Nothing about detection,
transcription or configuration is mocked:

```sh
pnpm stage:wake-word && pnpm stage:whisper
pnpm test:wake-walkthrough
```

It arms the session, pushes silence and unrelated speech and proves no
transcription and no turn happen — its evidence log prints that as
`before wake — transcriptions: 0, turns: 0` beside the seconds of passive audio
it pushed — says the phrase and its command, proves only the command reaches
Whisper, talks over the answer, proves the second sentence becomes its own turn
without a second wake word, and proves the session re-arms.

What it deliberately does **not** claim is that the microphone closed. The
engine has never held one: `TalkPorts` has no close, because in the product the
devices belong to `useTalkSession`. An earlier version of the last step called
the walkthrough's own microphone stub closed and then asserted it was closed,
which asserted this file's arithmetic. The microphone's lifecycle is asserted
where it lives, against the real hook in `useTalkSession.test.tsx`:

- turning Always Listening off closes it, on the saved configuration alone,
  with nothing in the test calling `stop()` (see
  [Turning it off](#turning-it-off));
- disabling the surface closes it;
- a refused grant is reported rather than dressed up as listening, and no wake
  session is started without one;
- a grant revoked mid-session ends the track and fails the engine closed.

The operating system's own microphone prompt is the one thing no test can
click. Both of its answers are covered above; the click itself is the
operator's.

The dedicated Local Wake Word workflow runs the native runtime on five of the
six supported desktop targets — macOS arm64, Linux arm64 and x86_64, Windows
arm64 and x86_64 — rather than compiling all of them and executing one. Intel
macOS is compiled against its own authenticated archive and not run, because
GitHub's `macos-13` image is retired and is cancelled without executing a step;
that is stated as a compile, never as a verified runtime. Five runtime-verified
targets and one compile-verified target is the claim, and it is not rounded up
to six.
Each host authenticates its own archive, stages the verified model, opens the
real runtime, runs the positive/negative, KWS-to-Whisper and long-armed-session
tests, and then the fifteen-step walkthrough. There is no compile-only matrix:
compiling proved the archive mapping and nothing about whether the model opens.

A second matrix builds a real installer per release target that has a runner
and inspects what it would install:

| Target | Package | Read with |
| --- | --- | --- |
| Linux x86_64 | `.deb` | `dpkg-deb -c` |
| Linux arm64 | `.deb` | `dpkg-deb -c` |
| macOS arm64 | `.dmg`, and the `.app` it wraps | `find` over the bundle |
| Windows x86_64 | `-setup.exe` | `7z l` |
| Windows arm64 | `-setup.exe` | `7z l` |

Each listing goes through one shared rule,
`scripts/verify-packaged-wake-assets.mjs`, which requires every file in
`MODEL_FILES` under the packaged `local-wake-word/` path and fails on any
`.wav` there. The required names come from the staging manifest rather than
from a list repeated in YAML, so a model file added there is required in every
package without anyone remembering; the rule's own cases are asserted in
`scripts/lib/wakeWordAssets.test.mjs`, because a job that first spends three
quarters of an hour building an installer is a bad place to discover that an
unreadable listing passes. Packaging is asked per bundler because it fails per
bundler: a `.deb` proving the resource glob resolved on Debian says nothing
about `Contents/Resources` in a `.app` or `$INSTDIR` in an NSIS installer.
Intel macOS has no packaging job either, for the same missing host.

### What no test performs

Two things, and both are stated rather than implied.

**A physical microphone.** Every automated run reaches the spotter from an
audio fixture. The chain from a real device — `getUserMedia`, a real
AudioWorklet at the host's own sample rate, real room noise, a real model
answer and a real speaker — is an operator procedure, and this repository does
not record it as done:

1. Settings → Voice: enable the wake word, set a phrase, then enable Always
   Listening and confirm it. Talk should show `armed` only once the microphone
   grant, the track, the worklet, the verified model files and the native
   session are all live.
2. Stay silent for a minute, then hold an unrelated conversation near the
   machine. Settings → Voice should report detections `0`; nothing should appear
   in the transcript, and no turn should appear in the run ledger.
3. Say the phrase, then ask something. The transcript should contain the
   question without the phrase, the answer should be spoken, and the run ledger
   should show one ordinary turn.
4. Talk over the answer. Playback should stop, the run should be asked to stop,
   and the interrupting sentence should become the next turn with no second
   wake word.
5. Let it re-arm, then turn Always Listening off **in Settings** while Talk is
   still open. The operating system's microphone indicator should go out
   without touching the Talk panel.

**The microphone prompt itself.** Nobody can click it from a test; both of its
answers are covered against the real hook.

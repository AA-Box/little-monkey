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
track, or revoked grant disarms native inference and closes the microphone.

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
again, copies only the required runtime files plus two pinned test WAVs, and
atomically swaps the verified directory into place. Tauri packages only the six
runtime files, so a production install works offline on first launch. Missing,
partial, wrong-size, or wrong-digest files report `unavailable`; application
code never downloads a wake model at runtime.

The sherpa-onnx code/runtime is Apache-2.0. The upstream 1.x static archive also
contains optional Piper/eSpeak libraries that KWS does not use; the vendored
link list excludes those libraries rather than importing their separate
licensing obligations. Upstream is tracking their removal in
[k2-fsa/sherpa-onnx#3731](https://github.com/k2-fsa/sherpa-onnx/issues/3731).
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

Settings reports the actual native backend, exact runtime/model identifiers,
verification/load/accepting state, payload bytes, detections, dropped frames,
average native inference time per submitted frame, and average trigger latency.
There is no portable resident-set or idle-CPU reading in the selected native
API, so those values are explicitly `unavailable`, not estimates. Inference is
driven only by incoming worklet frames; there is no polling loop.

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
```

The deterministic real-model fixtures cover the configured wake word, no wake
word, a similar but incorrect phrase, an immediate command, an inserted pause,
and deterministic background noise. The second test drives the production
manager, rejects audio from its stopped generation, writes only samples after
the native keyword-end timestamp, and transcribes them with the real bundled
Whisper model while asserting the wake phrase is absent.

The dedicated Local Wake Word workflow stages authenticated archives and
compiles all six desktop targets. A Linux host opens the real runtime and runs
the positive/negative and KWS-to-Whisper tests. A separate bundle job inspects
the generated installer for all six model files. Compilation is not described
as runtime verification: only hosts that execute the native tests are verified.

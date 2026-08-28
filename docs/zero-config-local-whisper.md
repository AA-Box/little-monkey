# Built-in local Whisper

Little Monkey's default speech-to-text backend is part of the application. Users do not install `whisper.cpp`, choose an executable, download a model manually, or configure filesystem paths.

The pinned multilingual `ggml-base-q5_1` model (57MB) ships inside the application. `scripts/stage-whisper-model.mjs` downloads it at build time, verifies its SHA-256, and stages it as a bundled resource, so an installed copy transcribes on a first run with no network at all — and behind a proxy that will not reach `huggingface.co`.

A development tree that has not run `pnpm stage:whisper` has no bundled copy. There, and only there, the app falls back to provisioning the same pinned model into its application-data directory at launch: bounded, fetched through the application's hardened public-download path, SHA-256 verified before activation, and published atomically. A failed background download does not break startup, because the first local transcription retries the same verified path. Talk and the telephony surface both report themselves unready until one of the two has produced a model, rather than claiming readiness they cannot honour.

The Whisper engine itself is compiled into the desktop application. The same implementation is used by Talk, desktop companion transcription, phone-call transcription, and paired-device voice input. Browser `MediaRecorder` WebM/Opus recordings are demuxed and decoded in-process before Whisper inference; WAV and the other formats supported by the bundled audio decoder use the same path.

Legacy `whisperBinary` and `whisperModel` configuration fields remain readable/writable only for compatibility with older persisted config files and clients. The built-in local backend ignores them, and stale legacy paths cannot prevent startup or make Talk report itself unconfigured.

The release targets covered by CI are macOS arm64/x86_64, Linux arm64/x86_64, and Windows arm64/x86_64. The local-speech workflow compiles the application for all six, performs a real end-to-end transcription of both WAV and WebM/Opus audio including checksum verification, and builds an installable package to prove the model is actually inside what a user installs.

# Built-in local Whisper

Little Monkey's default speech-to-text backend is part of the application. Users do not install `whisper.cpp`, choose an executable, download a model manually, or configure filesystem paths.

On launch, Little Monkey starts provisioning a pinned multilingual `ggml-base-q5_1` model into its application-data directory. The download is bounded, fetched through the application's hardened public-download path, SHA-256 verified before activation, and published atomically. A failed background download does not break application startup: the first local transcription retries the same verified provisioning path automatically.

The Whisper engine itself is compiled into the desktop application. The same implementation is used by Talk, desktop companion transcription, phone-call transcription, and paired-device voice input. Browser `MediaRecorder` WebM/Opus recordings are demuxed and decoded in-process before Whisper inference; WAV and the other formats supported by the bundled audio decoder use the same path.

Legacy `whisperBinary` and `whisperModel` configuration fields remain readable/writable only for compatibility with older persisted config files and clients. The built-in local backend ignores them, and stale legacy paths cannot prevent startup or make Talk report itself unconfigured.

The release targets covered by CI are macOS arm64/x86_64, Linux arm64/x86_64, and Windows arm64/x86_64. The local-speech workflow compiles the application for all six and also performs a real end-to-end transcription of both WAV and WebM/Opus audio, including automatic model provisioning and checksum verification.

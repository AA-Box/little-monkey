# Built-in Whisper model staging

`pnpm stage:whisper` downloads the pinned multilingual `ggml-base-q5_1`
Whisper model, verifies its SHA-256, and stages it here before a Tauri
development or release build.

The staged model is ignored by Git. Release assets contain the verified model
so local speech-to-text works on a first run with no network, and users do not
need to install `whisper.cpp` or fetch a model themselves.

This file is committed so the bundle's `resources/local-whisper/**/*` glob
always matches something: Tauri fails the build outright on a resource pattern
that matches no files, and a source build that has not staged the model would
otherwise not compile at all.

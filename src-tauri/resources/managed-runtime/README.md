# Managed local runtime staging

`pnpm stage:runtime` downloads the target-specific, pinned official
`llama.cpp` release, verifies its SHA-256, and stages `llama-server` plus its
adjacent runtime libraries here before a Tauri development or release build.

The staged binaries are ignored by Git. Release assets contain the verified
runtime so Little Monkey users do not need to install Ollama or `llama.cpp`
separately.

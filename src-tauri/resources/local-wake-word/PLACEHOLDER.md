# Local wake-word model

Empty in a fresh checkout, and tracked anyway: `tauri-build` resolves the
packaged resource glob at compile time, so without a file here every `cargo
build` in the repository fails on a machine that has not staged the model.

`pnpm stage:wake-word` authenticates the exact official sherpa-onnx
keyword-spotting archive and writes the six runtime files it ships — including
upstream's own `README.md`, which carries the model's license metadata. Nothing
else lands here: the deterministic test audio is staged to
`../local-wake-word-fixtures/`, outside anything Tauri packages, so "the
installer carries no test audio" is a property of this directory rather than a
list of filenames somebody has to keep in step with the model.

This file has its own name so staging never overwrites a tracked file.

See `docs/local-wake-word.md`.

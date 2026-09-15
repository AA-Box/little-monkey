# Local build-hook patch

This directory is the source of `sherpa-onnx-sys` 1.13.3 from crates.io,
covered by the included upstream Apache-2.0 license. Little Monkey changes only
`build.rs` and its build dependencies:

- authenticate the exact static runtime archive for every supported target by
  byte count and SHA-256 before extraction;
- prefer the already authenticated archive staged by the production build;
- record and require a digest marker for extracted native libraries;
- map the official Windows arm64 archive, which 1.13.3 published but omitted
  from its Rust build script; and
- link only the libraries required by keyword spotting. The unrelated
  `piper_phonemize`, `espeak-ng`, and `ucd` libraries in the upstream
  all-features archive are not linked or shipped.

  `ssentencepiece_core` was in that exclusion list and should not have been:
  it is sherpa's own Apache-2.0 SentencePiece implementation, and
  `sherpa-onnx-core`'s recognizer objects reference it unconditionally, so
  excluding it only moved the failure from the licence audit to the linker.
  macOS tolerated the undefined symbols; `lld` on Linux does not.

  Five of the six targets now pin upstream's `no-tts` archive, which ships no
  `piper_phonemize`, `espeak-ng` or `ucd` at all and therefore asks for none of
  their symbols. 1.13.3 published no `no-tts` static archive for Linux arm64,
  so that target keeps the exclusion and
  `src/little_monkey_tts_stubs.cc` answers for the two symbols
  `sherpa-onnx-core` references anyway, aborting if either is ever called.
  Excluding the libraries without answering for their symbols links on macOS
  and fails under `lld`; the stub is what makes the exclusion real. Delete it
  when upstream publishes a `no-tts` build for Linux arm64.

- pin the `MD` Windows archives. MSVC refuses to mix C runtime models, and this
  build — Rust and `whisper-rs-sys` included — uses the dynamic CRT, so the
  `MT` archive upstream's Rust build script selects fails with `LNK2038`.

The Rust API crate remains the exact upstream 1.13.3 release. Keep this patch
until upstream provides equivalent authenticated staging and the Apple SME
regression in later releases has been verified fixed.

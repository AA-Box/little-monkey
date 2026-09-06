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

  `piper_phonemize` and `espeak-ng` stay excluded — eSpeak NG is GPL-3.0 and
  this is an MIT binary — but their symbols are still demanded, because the
  Rust bindings declare externs across the whole C API and that pulls the
  text-to-speech objects in. `src/little_monkey_tts_stubs.cc` defines the two
  the linker asks for and aborts if either is ever called, which keeps the
  licence boundary without pretending the dependency is not there. Delete it
  when the exclusion goes away.

The Rust API crate remains the exact upstream 1.13.3 release. Keep this patch
until upstream provides equivalent authenticated staging and the Apple SME
regression in later releases has been verified fixed.

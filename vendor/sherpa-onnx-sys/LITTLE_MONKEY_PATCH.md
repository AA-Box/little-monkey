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
  `piper_phonemize`, `espeak-ng`, `ucd`, and `ssentencepiece_core` libraries in
  the upstream all-features archive are not linked or shipped.

The Rust API crate remains the exact upstream 1.13.3 release. Keep this patch
until upstream provides equivalent authenticated staging and the Apple SME
regression in later releases has been verified fixed.

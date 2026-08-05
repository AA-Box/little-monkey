use sha2::{Digest, Sha256};

fn main() {
    emit_managed_runtime_trust();

    #[allow(unused_mut)]
    let mut attributes = tauri_build::Attributes::new();

    #[cfg(windows)]
    {
        // tauri-build embeds the Windows app manifest via tauri-winres /
        // embed-resource's `compile()`, which emits
        // `cargo:rustc-link-arg-bins=...` — that only links the manifest
        // resource into `[[bin]]` targets, never into `cargo test`'s test
        // binaries (see rust-embed-resource#69 and tauri-apps/tauri#13419).
        // Without any manifest, the test binary's PE import table can fail
        // to resolve at Windows loader time (STATUS_ENTRYPOINT_NOT_FOUND,
        // 0xc0000139) even though the same import (bcryptprimitives.dll's
        // ProcessPrng, used unconditionally by Rust std for HashMap's
        // RandomState seed) is well-formed and present — this is exactly
        // the crash this project's Windows CI leg was hitting on
        // `cargo test`, confirmed via `dumpbin /imports` showing a
        // correctly-declared but unresolved-at-runtime import.
        //
        // Fix: opt out of tauri-build's own bins-only embed and embed the
        // identical manifest ourselves via a plain (non "-bins") linker
        // arg, which cargo applies to every target built from this crate —
        // bins, cdylibs, and test binaries alike.
        attributes = attributes
            .windows_attributes(tauri_build::WindowsAttributes::new_without_app_manifest());
        embed_manifest_for_every_target();
    }

    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}

/// Each staged runtime's manifest digest is baked into the binary so
/// `managed_runtime.rs` can authenticate the tree before parsing any name or
/// checksum out of it. The staged directory names must stay in step with
/// `MANAGED_LLAMA_VERSION` / `MANAGED_SD_VERSION` in `src/managed_runtime.rs`;
/// a mismatch emits no digest, which fails discovery closed rather than open.
fn emit_managed_runtime_trust() {
    emit_runtime_digest("llama-b9637", "LITTLE_MONKEY_TRUSTED_RUNTIME_MANIFEST_SHA256");
    emit_runtime_digest(
        "llama-tts-b10278",
        "LITTLE_MONKEY_TRUSTED_TTS_MANIFEST_SHA256",
    );
    emit_runtime_digest(
        "sd-master-812-ea7f0c8",
        "LITTLE_MONKEY_TRUSTED_SD_MANIFEST_SHA256",
    );
}

fn emit_runtime_digest(staged_directory: &str, env_name: &str) {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("resources")
        .join("managed-runtime")
        .join(staged_directory)
        .join("runtime-manifest.json");
    println!("cargo:rerun-if-changed={}", manifest.display());

    // Source-only cargo builds deliberately have no staged runtime. In that
    // case the crate still compiles, but app-owned runtime discovery fails
    // closed until `pnpm stage:runtime` is run and the crate is rebuilt.
    if let Ok(bytes) = std::fs::read(&manifest) {
        println!(
            "cargo:rustc-env={env_name}={:x}",
            Sha256::digest(bytes)
        );
    }
}

#[cfg(windows)]
fn embed_manifest_for_every_target() {
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
}

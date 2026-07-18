fn main() {
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
        attributes = attributes.windows_attributes(
            tauri_build::WindowsAttributes::new_without_app_manifest(),
        );
        embed_manifest_for_every_target();
    }

    tauri_build::try_build(attributes).expect("failed to run tauri-build");
}

#[cfg(windows)]
fn embed_manifest_for_every_target() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("windows-app-manifest.xml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
}

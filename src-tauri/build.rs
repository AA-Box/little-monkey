fn main() {
    tauri_build::build();

    // `tauri_build::build()`'s Windows manifest embedding (via `embed-resource`'s
    // `compile()`) only covers this crate's `bin` target — it emits
    // `cargo:rustc-link-arg-bins`, which Cargo scopes strictly to bin targets.
    // `examples`/`tests` never get the Common-Controls-v6 manifest, so any of
    // them that construct a real window/webview on Windows+MSVC crash on
    // launch with STATUS_ENTRYPOINT_NOT_FOUND (0xc0000139) — see
    // https://github.com/tauri-apps/tauri/issues/13948 (root cause) and
    // https://github.com/orgs/tauri-apps/discussions/11179 (upstream
    // workaround, scoped here to non-bin targets only so the main app
    // binary's existing manifest embedding is left untouched). No `benches`
    // entry: Cargo rejects `rustc-link-arg-benches` outright for a crate with
    // no benchmark target, and this crate has none.
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os == "windows" && target_env == "msvc" {
        let manifest = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
            .join("windows-app-manifest.xml");
        println!("cargo:rerun-if-changed={}", manifest.display());
        for kind in ["examples", "tests"] {
            println!("cargo:rustc-link-arg-{kind}=/MANIFEST:EMBED");
            println!(
                "cargo:rustc-link-arg-{kind}=/MANIFESTINPUT:{}",
                manifest.display()
            );
        }
    }
}

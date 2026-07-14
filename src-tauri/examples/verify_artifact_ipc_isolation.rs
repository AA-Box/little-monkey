//! Real, runnable, exit-code-driven verification that a tier-2 `artifact://`
//! frame has no reachable Tauri IPC bridge — the exact property
//! `src/artifacts.rs`'s module doc calls out as phase 2's explicit security
//! gate, and which the review finding this file addresses correctly pointed
//! out had no automated check, only a manual per-OS checklist.
//!
//! WHY THIS IS AN EXAMPLE, NOT A `#[test]`: creating a real WKWebView (macOS)
//! or WebView2 (Windows) webview requires running on the process's actual
//! main thread. Rust's default `cargo test` harness runs every `#[test]` on a
//! *worker* thread, never the true process main thread, so a real webview
//! can never be constructed inside `#[test]` — this is exactly why
//! `tauri::test` ships a headless `MockRuntime` instead of a real one (see
//! that module's doc comment) and why `artifacts.rs`'s previous "MANUAL
//! VERIFICATION" note existed at all. A `cargo run --example` binary's `main`
//! runs on the real process main thread, so it CAN drive a real webview.
//! `cargo test` therefore can't run this directly, but it IS a genuine
//! automated check: scriptable, exit-code-driven, and runnable in CI on a
//! GUI-capable runner (`cargo run --manifest-path src-tauri/Cargo.toml
//! --example verify_artifact_ipc_isolation`) — upgrading the old "must be
//! verified by hand" note into an actual reproducible tool, which is what
//! the design doc's phase-2 gate asked for.
//!
//! WHAT IT CHECKS: builds a real (non-mock) Tauri app with the exact same
//! `artifact://` protocol handler `lib.rs` registers, publishes a tiny
//! artifact whose inline `<script>` probes `typeof window.__TAURI_INTERNALS__`
//! / `typeof window.__TAURI__` / `window.origin`, and loads it through the
//! exact consuming iframe shape `ArtifactPane.tsx` uses:
//! `sandbox="allow-scripts"` (no `allow-same-origin`), no other capability
//! wiring. The artifact can't call back into Rust by design (that's the
//! whole point), so it reports its findings up via `postMessage` to the
//! host page, which sets its own window title — polled from the real main
//! thread's event loop, no IPC involved anywhere in the reporting path
//! either.
//!
//! Exit codes: `0` isolation confirmed; `1` `__TAURI_INTERNALS__`/`__TAURI__`
//! was reachable inside the artifact frame (the exact regression the design
//! doc's risk section warns about); `2` no result was received before the
//! timeout (treated as a failure, not a pass — see `main`'s timeout branch).
//!
//! KNOWN LIMITATION (found while writing this check — see the code review
//! this addresses): Tauri's own `is_local_url` treats ANY response served by
//! ANY registered custom URI scheme protocol as a "local" origin *on Windows
//! specifically* (custom protocols there share the `http://<scheme>.localhost`
//! address space), and `wry`'s WebView2 backend adds initialization scripts
//! to every subframe regardless of the `for_main_frame_only` flag Tauri sets
//! for its own IPC bridge script (its Windows-specific doc comment says so
//! explicitly). Combined, this means this exact check needs to actually be
//! RUN ON WINDOWS to mean anything for that platform — a pass on macOS/Linux
//! does not establish it also passes on Windows, unlike most of this app's
//! other cross-platform Rust code. Flagged separately as a follow-up; this
//! tool is what to run, on each OS, to find out.
//!
//! REQUIRES A REAL, BOUND GUI SESSION: this creates an actual native window
//! and webview, which on macOS needs a logged-in Aqua/WindowServer session
//! (not just the `WindowServer` daemon, which always runs). In a headless
//! context with no bound interactive session (some CI runners, some sandboxed
//! tool-execution environments), the webview never completes a real
//! navigation/JS pass and this deterministically times out (exit code `2`) —
//! that is a "could not obtain a verdict" result, not a "PASS", and must not
//! be read as confirming isolation. Set `VERIFY_ARTIFACT_DEBUG=1` to print
//! the host window's title on every poll while diagnosing that.
use std::io::Write;
use std::sync::mpsc;
use std::time::Duration;

use little_monkey_lib::{artifacts, AppState};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Serves exactly one path ("" / the window's start URL) with our inline host
/// document — just enough `Assets` impl to avoid depending on the real
/// frontend build (`../dist`) or a running `pnpm dev` server, neither of
/// which this check needs or should require.
struct HostPageAsset(Vec<u8>);

impl<R: tauri::Runtime> tauri::Assets<R> for HostPageAsset {
    fn get(&self, _key: &tauri::utils::assets::AssetKey) -> Option<std::borrow::Cow<'_, [u8]>> {
        Some(std::borrow::Cow::Borrowed(&self.0))
    }

    fn iter(&self) -> Box<tauri::utils::assets::AssetsIter<'_>> {
        Box::new(std::iter::once((
            std::borrow::Cow::Borrowed("index.html"),
            std::borrow::Cow::Borrowed(self.0.as_slice()),
        )))
    }

    fn csp_hashes(
        &self,
        _html_path: &tauri::utils::assets::AssetKey,
    ) -> Box<dyn Iterator<Item = tauri::utils::assets::CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

/// Same platform split `@tauri-apps/api/core`'s `convertFileSrc` performs on
/// the frontend (see `ArtifactPane.tsx`) — Windows serves custom protocols as
/// `http://<scheme>.localhost/...`, every other desktop platform as
/// `<scheme>://localhost/...`.
fn artifact_url(id: &str) -> String {
    #[cfg(windows)]
    {
        format!("http://artifact.localhost/{id}")
    }
    #[cfg(not(windows))]
    {
        format!("artifact://localhost/{id}")
    }
}

fn main() {
    eprintln!("verify_artifact_ipc_isolation: starting");
    let _ = std::io::stderr().flush();
    // The artifact's own content: exactly what a malicious/compromised model
    // could emit in an HTML fence. No `try/catch` needed around the
    // typeof checks themselves (`typeof` never throws for an undeclared
    // identifier), but the surrounding IIFE is wrapped anyway so a failure
    // anywhere in this probe still reports SOMETHING rather than leaving the
    // host waiting for a message that never arrives.
    let artifact_html = r#"<!doctype html><html><body><script>
(function () {
  var result;
  try {
    result = JSON.stringify({
      internals: typeof window.__TAURI_INTERNALS__,
      tauriGlobal: typeof window.__TAURI__,
      origin: window.origin
    });
  } catch (e) {
    result = JSON.stringify({ error: String(e) });
  }
  window.parent.postMessage(result, '*');
})();
</script></body></html>"#;

    let host_html = format!(
        r#"<!doctype html><html><body><iframe id="probe" sandbox="allow-scripts" src="{}"></iframe>
<script>
window.addEventListener('message', function (e) {{
  document.title = 'RESULT:' + e.data;
}});
</script></body></html>"#,
        // The artifact isn't published until `setup`, so this placeholder is
        // swapped in via a second window navigation once the id is known —
        // simpler than threading the id through `Assets` construction order.
        "about:blank"
    );

    let app = tauri::Builder::default()
        .manage(AppState::default())
        .register_uri_scheme_protocol("artifact", |ctx, request| {
            artifacts::handle_request(ctx.app_handle().state::<AppState>().inner(), &request)
        })
        .build(tauri::test::mock_context(HostPageAsset(
            host_html.into_bytes(),
        )))
        .expect("failed to build the verification app");

    {
        let state = app.state::<AppState>();
        let mut store = state.artifacts.lock().unwrap();
        let id = artifacts::publish_impl(&mut store, artifact_html.to_string(), "html")
            .expect("publishing the probe artifact must succeed");
        drop(store);

        let window = WebviewWindowBuilder::new(&app, "probe", WebviewUrl::App("index.html".into()))
            .build()
            .expect("failed to build the probe window");

        // Now that the artifact id is known, point the iframe at it for
        // real via a tiny in-page script (avoids rebuilding the whole host
        // document / `Assets` impl just to splice one URL in).
        let url = artifact_url(&id);
        window
            .eval(format!(
                "document.getElementById('probe').src = {};",
                serde_json::to_string(&url).unwrap()
            ))
            .expect("failed to point the iframe at the published artifact");
    }

    // Poll the host window's title from a background thread. The pass/fail
    // verdict is computed, printed, AND turned into the actual process exit
    // status from inside this thread via `std::process::exit` directly —
    // deliberately NOT `AppHandle::exit(code)`. Two independent reasons:
    // (1) `AppHandle::exit` only *requests* a graceful shutdown by posting a
    // `RunEvent::ExitRequested`/`Exit` pair through the event loop; tao's
    // macOS backend (pinned version, confirmed by reading
    // `tao-0.35.3/src/platform_impl/macos/event_loop.rs` and
    // `tao-runtime-wry`'s `Message::RequestExit` handling while building this
    // check) always resolves that to the bare `ControlFlow::Exit` constant —
    // which `tao`'s own `event_loop.rs` defines as `ExitWithCode(0)` — so the
    // requested code is silently discarded and the real OS exit status is
    // always `0` no matter what's passed in; a hard `process::exit` bypasses
    // that entirely and is the only reliable way to surface a non-zero
    // verdict. (2) on macOS, `app.run()` never returns to its caller at all
    // (the underlying `NSApplication` run loop calls `process::exit` itself
    // once torn down) — code placed after `app.run()` in `main` would
    // silently never execute there, so the diagnostic and exit both have to
    // happen from wherever the decision is actually made, which is here.
    let app_handle = app.handle().clone();
    std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if std::time::Instant::now() > deadline {
                eprintln!(
                    "FAIL: no probe result received within the timeout — the artifact frame's script may not have run at all."
                );
                let _ = std::io::stderr().flush();
                std::process::exit(2);
            }
            let app_handle_for_check = app_handle.clone();
            let (title_tx, title_rx) = mpsc::channel();
            if app_handle
                .run_on_main_thread(move || {
                    let title = app_handle_for_check
                        .get_webview_window("probe")
                        .and_then(|w| w.title().ok())
                        .unwrap_or_default();
                    let _ = title_tx.send(title);
                })
                .is_ok()
            {
                if let Ok(title) = title_rx.recv_timeout(Duration::from_millis(500)) {
                    if std::env::var("VERIFY_ARTIFACT_DEBUG").is_ok() {
                        eprintln!("(debug) current window title: {title:?}");
                    }
                    if let Some(payload) = title.strip_prefix("RESULT:") {
                        let exit_code = evaluate_and_report(payload);
                        std::process::exit(exit_code);
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    app.run(|_, _| {});

    // Only reached on platforms where the native event loop actually returns
    // control here instead of the process having already exited from inside
    // the spawned thread above (see that comment) — a defensive fallback,
    // never the normal path.
    eprintln!("FAIL: the app event loop exited before a verdict was reached.");
    std::process::exit(2);
}

/// Parses the artifact frame's `postMessage` payload, prints a PASS/FAIL
/// diagnostic to stderr, and returns the process exit code that decision
/// maps to (`0` isolated, `1` leaked). Never called for the timeout case —
/// that path reports its own diagnostic and exit code inline above.
fn evaluate_and_report(payload: &str) -> i32 {
    eprintln!("Probe result from inside the artifact:// frame: {payload}");
    let parsed: serde_json::Value =
        serde_json::from_str(payload).unwrap_or(serde_json::Value::Null);
    let internals = parsed
        .get("internals")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tauri_global = parsed
        .get("tauriGlobal")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let _ = std::io::stderr().flush();
    if internals == "undefined" && tauri_global == "undefined" {
        eprintln!("PASS: window.__TAURI_INTERNALS__ and window.__TAURI__ are both undefined inside the artifact:// frame.");
        let _ = std::io::stderr().flush();
        0
    } else {
        eprintln!(
            "FAIL: the Tauri IPC bridge is reachable inside the artifact:// frame (internals={internals:?}, tauriGlobal={tauri_global:?}) — this is a sandbox-escape/IPC-leakage regression."
        );
        let _ = std::io::stderr().flush();
        1
    }
}

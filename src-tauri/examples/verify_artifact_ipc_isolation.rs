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
//! `artifact://` protocol handler `lib.rs` registers PLUS a canary command
//! ([`verify_canary_command`]) standing in for the privileged commands the
//! real app grants its trusted windows. It publishes a tiny artifact whose
//! inline `<script>` doesn't merely *observe* the Tauri bridge — it tries to
//! USE it, calling the canary command through `__TAURI_INTERNALS__.invoke`.
//! The artifact is loaded exactly as `ArtifactPane.tsx` now loads it: the
//! host page fetches the published document from `artifact://` and re-serves
//! it to a `sandbox="allow-scripts"` (no `allow-same-origin`) iframe from a
//! `blob:` URL. The probe reports its outcome up via `postMessage` → the host
//! page's `document.title` → Rust's `on_document_title_changed` handler (no
//! IPC in the reporting path). The VERDICT, though, is not the report: it is
//! whether the canary command actually ran ([`CANARY_FIRED`]). A frame that
//! can run a command has escaped the sandbox; one that cannot, has not —
//! regardless of whether the bridge OBJECT happens to be visible to it.
//!
//! (The title relay is used because Tauri never mirrors `document.title` into
//! the native window title on its own — that mirror is exactly what the opt-in
//! `on_document_title_changed` handler lets apps implement — so a
//! `window.title()` poll would read the configured title forever and never
//! observe the page's report.)
//!
//! Exit codes: `0` the frame could NOT invoke a command (isolation holds);
//! `1` the frame DID get `verify_canary_command` to run (sandbox escape —
//! the exact regression this gate exists to catch); `2` no result was
//! received before the timeout (treated as a failure, not a pass — see
//! `main`'s timeout branch).
//!
//! WHY `blob:` AND WHY WINDOWS IS THE CASE THAT MATTERS: on Windows, `wry`'s
//! WebView2 backend injects Tauri's IPC bridge (invoke key included) into
//! EVERY subframe regardless of the `for_main_frame_only` flag Tauri sets
//! (its own Windows-specific doc comment says so), and Tauri's `is_local_url`
//! treats any registered custom-protocol response (`http://artifact.localhost`
//! there) as a trusted *local* origin. An artifact frame pointed straight at
//! `artifact://` therefore CAN invoke privileged commands on Windows — an
//! earlier version of this check, which only asserted the bridge object was
//! absent, correctly caught exactly that. The fix (see `ArtifactPane.tsx` and
//! `artifacts.rs`) re-serves the document from a `blob:` URL, a *remote*
//! origin Tauri's ACL grants nothing, so the bridge is inert even where the
//! Windows leak still plants it. This check loads via `blob:` to verify that
//! fix on the platform it matters for; a pass on macOS/Linux (where the leak
//! never existed) does not by itself establish the Windows property.
//!
//! REQUIRES A REAL, BOUND GUI SESSION: this creates an actual native window
//! and webview, which on macOS needs a logged-in Aqua/WindowServer session
//! (not just the `WindowServer` daemon, which always runs). In a headless
//! context with no bound interactive session (some CI runners, some sandboxed
//! tool-execution environments), the webview never completes a real
//! navigation/JS pass and this deterministically times out (exit code `2`) —
//! that is a "could not obtain a verdict" result, not a "PASS", and must not
//! be read as confirming isolation. Set `VERIFY_ARTIFACT_DEBUG=1` to print
//! every document-title change while diagnosing that: a `BOOT:` line means
//! the host page's script ran but the iframe never reported; no `BOOT:` line
//! means the host page itself never executed.
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use little_monkey_lib::{artifacts, AppState};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

/// Flipped to `true` iff the [`verify_canary_command`] below is ever actually
/// executed — i.e. iff the sandboxed artifact frame managed to get a Tauri
/// command to run. That is the real sandbox-escape this check exists to
/// detect, so this atomic (not the mere presence of the bridge object) is the
/// authoritative pass/fail signal — see `evaluate_and_report`.
static CANARY_FIRED: AtomicBool = AtomicBool::new(false);

/// A stand-in for any privileged command the real app exposes to its trusted
/// windows. It does nothing but record that it ran: if the untrusted artifact
/// frame can reach it, an attacker-authored artifact could reach the real
/// `shell`/`fs`/`dialog` commands `capabilities/default.json` grants the main
/// window just the same. Registered via `invoke_handler` below.
#[tauri::command]
fn verify_canary_command() -> Result<(), String> {
    CANARY_FIRED.store(true, Ordering::SeqCst);
    Ok(())
}

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
    // could emit in an HTML fence. It doesn't merely *observe* the bridge —
    // it actively tries to USE it, calling `verify_canary_command` through
    // whatever `__TAURI_INTERNALS__.invoke` the frame can see. If that command
    // ever runs (see `CANARY_FIRED`), the sandbox has been escaped. The whole
    // body is wrapped so any failure still reports SOMETHING rather than
    // leaving the host waiting for a message that never arrives; a 3s internal
    // deadline guarantees a report even if the invoke promise never settles.
    let artifact_html = r#"<!doctype html><html><body><script>
(function () {
  function report(o) { window.parent.postMessage(JSON.stringify(o), '*'); }
  var base = {
    internals: typeof window.__TAURI_INTERNALS__,
    tauriGlobal: typeof window.__TAURI__,
    href: location.href
  };
  var internals = window.__TAURI_INTERNALS__;
  if (!internals || typeof internals.invoke !== 'function') {
    base.outcome = 'no-bridge';
    report(base);
    return;
  }
  var done = false;
  function finish(outcome) {
    if (done) return;
    done = true;
    base.outcome = outcome;
    report(base);
  }
  try {
    internals.invoke('verify_canary_command', {})
      .then(function () { finish('invoke-resolved'); })
      .catch(function (e) { finish('invoke-rejected:' + String(e)); });
  } catch (e) {
    finish('invoke-threw:' + String(e));
  }
  setTimeout(function () { finish('invoke-timeout'); }, 3000);
})();
</script></body></html>"#;

    // Publish the probe artifact BEFORE building the host document so the
    // host page's fetch can carry the real artifact URL from the very first
    // parse. `publish_impl` only needs the map, not a built app.
    let state = AppState::default();
    let id = {
        let mut store = state.artifacts.lock().unwrap();
        artifacts::publish_impl(&mut store, artifact_html.to_string(), "html")
            .expect("publishing the probe artifact must succeed")
    };

    // The host page loads the artifact EXACTLY as `ArtifactPane.tsx` now does:
    // it fetches the published document from the `artifact://` protocol and
    // re-serves it to a `sandbox="allow-scripts"` iframe from a `blob:` object
    // URL — not by pointing the iframe straight at `artifact://`. That blob
    // origin is what makes the frame *remote* to Tauri's ACL (see this file's
    // header), so the check exercises the real app's isolation, not a shape
    // the app no longer uses.
    let host_html = format!(
        r#"<!doctype html><html><body><iframe id="probe" sandbox="allow-scripts"></iframe>
<script>
document.title = 'BOOT:host-script-ran';
window.addEventListener('message', function (e) {{
  document.title = 'RESULT:' + e.data;
}});
(function () {{
  fetch({artifact_url})
    .then(function (r) {{ return r.text(); }})
    .then(function (html) {{
      var url = URL.createObjectURL(new Blob([html], {{ type: 'text/html' }}));
      document.getElementById('probe').src = url;
    }})
    .catch(function (e) {{
      document.title = 'RESULT:' + JSON.stringify({{ outcome: 'host-fetch-error:' + String(e) }});
    }});
}})();
</script></body></html>"#,
        artifact_url = serde_json::to_string(&artifact_url(&id)).unwrap()
    );

    let app = tauri::Builder::default()
        .manage(state)
        .invoke_handler(tauri::generate_handler![verify_canary_command])
        .register_uri_scheme_protocol("artifact", |ctx, request| {
            artifacts::handle_request(ctx.app_handle().state::<AppState>().inner(), &request)
        })
        .build(tauri::test::mock_context(HostPageAsset(
            host_html.into_bytes(),
        )))
        .expect("failed to build the verification app");

    // The verdict arrives through `on_document_title_changed` — the opt-in
    // hook wry/Tauri provide precisely because `document.title` is NOT
    // mirrored into the native window title automatically (see this file's
    // header: polling `window.title()` observes only the configured native
    // title, forever). The `BOOT:` title the host page sets immediately also
    // flows through here, giving a debug-visible signal that distinguishes
    // "host page never ran" from "iframe never reported" when diagnosing a
    // timeout on a CI runner.
    //
    // The pass/fail verdict is computed, printed, AND turned into the actual
    // process exit status from inside this handler via `std::process::exit`
    // directly — deliberately NOT `AppHandle::exit(code)`. Two independent
    // reasons:
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
    let debug = std::env::var("VERIFY_ARTIFACT_DEBUG").is_ok();
    let _window = WebviewWindowBuilder::new(&app, "probe", WebviewUrl::App("index.html".into()))
        .on_document_title_changed(move |_window, title| {
            if debug {
                eprintln!("(debug) document title changed: {title:?}");
            }
            if let Some(payload) = title.strip_prefix("RESULT:") {
                std::process::exit(evaluate_and_report(payload));
            }
        })
        .build()
        .expect("failed to build the probe window");

    // Watchdog: if no verdict has arrived (and exited the process) within
    // the timeout, report the inconclusive case as a failure. Overridable
    // via VERIFY_ARTIFACT_TIMEOUT_SECS: a cold CI VM's first WebView2
    // environment initialization (no warm user-data-dir cache) can plausibly
    // take longer than a fixed 10s allows, which would otherwise misreport
    // as the "no bound GUI session" inconclusive case documented in this
    // file's header rather than a real timing issue. Default stays 10s so
    // local/interactive runs are unaffected.
    std::thread::spawn(move || {
        let timeout_secs = std::env::var("VERIFY_ARTIFACT_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        std::thread::sleep(Duration::from_secs(timeout_secs));
        eprintln!(
            "FAIL: no probe result received within the timeout — the artifact frame's script may not have run at all."
        );
        let _ = std::io::stderr().flush();
        std::process::exit(2);
    });

    app.run(|_, _| {});

    // Only reached on platforms where the native event loop actually returns
    // control here instead of the process having already exited from inside
    // the spawned thread above (see that comment) — a defensive fallback,
    // never the normal path.
    eprintln!("FAIL: the app event loop exited before a verdict was reached.");
    std::process::exit(2);
}

/// Prints the artifact frame's `postMessage` diagnostics, then returns the
/// process exit code based on the AUTHORITATIVE signal — whether the frame's
/// invoke actually executed `verify_canary_command` ([`CANARY_FIRED`]). The
/// payload's `outcome`/`internals` fields are printed for diagnosis only:
/// on Windows the bridge OBJECT is expected to be present in the subframe
/// (the WebView2 leak this file documents), so its mere presence is not a
/// failure — only a command actually *running* is. Never called for the
/// timeout case — that path reports its own diagnostic and exit code inline.
fn evaluate_and_report(payload: &str) -> i32 {
    eprintln!("Probe result from inside the artifact frame: {payload}");

    // Small grace so a command that WOULD run has certainly finished before
    // we read the flag: the probe already awaits its invoke (up to its own
    // 3s deadline) before reporting, and `verify_canary_command` is a trivial
    // synchronous handler, so anything that was going to fire has by now —
    // this is belt-and-braces against a late main-thread dispatch.
    std::thread::sleep(Duration::from_millis(250));
    let escaped = CANARY_FIRED.load(Ordering::SeqCst);
    let _ = std::io::stderr().flush();
    if !escaped {
        eprintln!(
            "PASS: the sandboxed artifact frame could not invoke a command (verify_canary_command never ran)."
        );
        let _ = std::io::stderr().flush();
        0
    } else {
        eprintln!(
            "FAIL: the sandboxed artifact frame INVOKED a Tauri command (verify_canary_command ran) — this is a sandbox-escape/IPC-leakage regression."
        );
        let _ = std::io::stderr().flush();
        1
    }
}

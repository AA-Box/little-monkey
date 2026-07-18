//! Tier-2 interactive artifact publishing: an in-memory content store served
//! by the `artifact://` custom protocol so model-generated HTML with inline
//! `<script>` can run inside a fully sandboxed, opaque-origin iframe. Tier 1
//! (the empty-`sandbox=""` `srcdoc` iframe in `ArtifactPane.tsx`) stays the
//! default rendering path for every artifact and categorically blocks script
//! execution; this module exists solely so `artifactScriptsEnabled` can opt
//! a user back into interactive HTML for the artifacts that actually need a
//! live script, without ever giving that script network, storage, or IPC
//! access. See `docs/roadmap/p2-artifacts-rendering.md`'s SANDBOX MODEL
//! section for the full design and threat model this implements.
//!
//! Security posture (phase 2's explicit gate — see this module's tests and
//! the AUTOMATED VERIFICATION note on [`handle_request`]):
//! - `id`: `[a-zA-Z0-9-]` only, mirroring `checkpoints::validate_id` — a
//!   crafted id can never be anything but an exact, harmless map lookup.
//! - Every served document gets `Content-Security-Policy: default-src
//!   'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src
//!   data: blob:; font-src data:; connect-src 'none'; form-action 'none'` —
//!   `connect-src 'none'` alone blocks every network exfiltration channel
//!   (fetch/XHR/WebSocket/beacon) regardless of what the consuming iframe's
//!   `sandbox` attribute allows.
//! - The frontend's iframe uses `sandbox="allow-scripts"` WITHOUT
//!   `allow-same-origin` (see `ArtifactPane.tsx`), which gives the frame an
//!   opaque origin: no cookies, no localStorage, no parent-DOM access, no
//!   popups, no top-window navigation.
//! - The frame is loaded from a `blob:` URL, not straight from `artifact://`.
//!   This is load-bearing on Windows: there `wry`'s WebView2 backend injects
//!   Tauri's IPC bridge (invoke key included) into every subframe regardless
//!   of `for_main_frame_only`, and Tauri's `is_local_url` treats a
//!   custom-protocol frame (`http://artifact.localhost`) as a trusted *local*
//!   origin — so a frame pointed straight at `artifact://` COULD invoke the
//!   privileged commands `capabilities/default.json` grants the `main`/
//!   `session-*` windows. A `blob:` origin is *remote* to Tauri's ACL, and no
//!   `"remote"` entry extends any capability to it, so `invoke`/IPC is inert
//!   there even where the Windows leak still plants the bridge object. See
//!   this module's `capability_config_grants_no_scheme_access` test (the ACL
//!   half) and `examples/verify_artifact_ipc_isolation.rs` (the in-webview
//!   half, which actually attempts an invoke from the frame and fails if it
//!   runs).
//! - Content is served only from the in-memory map below, by id, never from
//!   disk — there is no path-traversal surface.
//!
//! Bounded like `checkpoints.rs`'s `MAX_CHECKPOINTS`: at most
//! [`MAX_ARTIFACTS`] entries are ever kept, oldest-published evicted first,
//! and each entry is capped at [`MAX_ARTIFACT_BYTES`] — enforced here
//! server-side (not merely by whatever the frontend happens to send), since
//! `artifact_publish` is a directly invokable command.

use std::collections::HashMap;

use crate::AppState;

/// Per-artifact size cap — mirrors the design doc's "5 MB per artifact".
pub const MAX_ARTIFACT_BYTES: usize = 5 * 1024 * 1024;

/// How many published artifacts to keep in memory before the oldest
/// (by publish order) are evicted — mirrors `checkpoints.rs`'s
/// `MAX_CHECKPOINTS` bounded-resource pattern.
pub const MAX_ARTIFACTS: usize = 50;

/// The single source of truth for the tier-2 artifact CSP. Emitted BOTH as
/// the `Content-Security-Policy` HTTP response header (below) AND injected
/// into the served document as a leading `<meta http-equiv>` (see
/// [`inject_csp_meta`]). The header alone is no longer sufficient because the
/// frontend loads the served document through a `blob:` URL — see
/// `ArtifactPane.tsx`'s tier-2 doc comment for why (it makes the frame a
/// *remote* origin so Tauri's ACL rejects any IPC the Windows WebView2
/// subframe-script leak might otherwise expose), and a `blob:` document
/// carries no response headers, so the CSP has to travel inside the document.
/// `connect-src 'none'` is the load-bearing directive: it blocks every
/// network exfiltration channel (fetch/XHR/WebSocket/beacon) regardless of
/// the consuming iframe's `sandbox` attribute.
pub const ARTIFACT_CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; font-src data:; connect-src 'none'; form-action 'none'";

/// One published tier-2 artifact, keyed by a server-generated uuid in
/// `AppState::artifacts`.
pub struct PublishedArtifact {
    pub content: String,
    pub mime: &'static str,
    /// Monotonic publish order (a plain incrementing counter, not wall-clock
    /// time — see [`next_seq`]) used only to pick the oldest entries to
    /// evict once [`MAX_ARTIFACTS`] is exceeded. A `HashMap` has no ordering
    /// of its own, so this is this module's equivalent of
    /// `checkpoints::CheckpointManifest::created_at_ms`.
    seq: u64,
}

/// Next value in a process-wide monotonic publish sequence. A plain atomic
/// counter rather than `SystemTime::now()` (contrast
/// `checkpoints.rs::now_ms`): checkpoints are ranked across app restarts so
/// they need a real timestamp, but published artifacts live only in memory
/// for the current process's lifetime, and many can legitimately be
/// published within the same millisecond (e.g. rapid Preview-tab refreshes),
/// which would make a millisecond timestamp an unreliable tiebreaker for
/// "oldest" in eviction tests.
fn next_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
}

/// Reject anything that isn't a plain UUID-shaped id — same rule, and same
/// reasoning, as `checkpoints::validate_id`: this id ends up in a URL
/// (`artifact://localhost/<id>` or `http://artifact.localhost/<id>` on
/// Windows) reachable from a sandboxed frame, so it must never be usable for
/// anything but an exact, harmless map lookup.
fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("Invalid artifact id '{}'", id))
    }
}

/// Maps a frontend-supplied `kind` to the MIME type served for it. `"html"`
/// is the only tier-2-eligible kind today — SVG and Mermaid stay on tier 1
/// (see this module's doc comment: neither needs script execution, so
/// neither needs the interactive protocol) — so anything else is rejected
/// rather than silently guessed at.
fn mime_for_kind(kind: &str) -> Result<&'static str, String> {
    match kind {
        "html" => Ok("text/html; charset=utf-8"),
        other => Err(format!("Unsupported artifact kind '{}'", other)),
    }
}

/// Core publish logic, parameterized by the map directly (mirrors
/// `checkpoints::begin_impl`'s `base_dir` param) so it's testable without a
/// real `AppState`/`AppHandle`. Evicts the oldest entries first if inserting
/// would exceed [`MAX_ARTIFACTS`].
pub fn publish_impl(
    artifacts: &mut HashMap<String, PublishedArtifact>,
    content: String,
    kind: &str,
) -> Result<String, String> {
    if content.len() > MAX_ARTIFACT_BYTES {
        return Err(format!(
            "Artifact content ({} bytes) exceeds the {} byte limit",
            content.len(),
            MAX_ARTIFACT_BYTES
        ));
    }
    let mime = mime_for_kind(kind)?;

    if artifacts.len() >= MAX_ARTIFACTS {
        evict_oldest(artifacts, artifacts.len() - MAX_ARTIFACTS + 1);
    }

    let id = uuid::Uuid::new_v4().to_string();
    artifacts.insert(
        id.clone(),
        PublishedArtifact {
            content,
            mime,
            seq: next_seq(),
        },
    );
    Ok(id)
}

/// Removes the `n` oldest entries (by publish order) from `artifacts`.
fn evict_oldest(artifacts: &mut HashMap<String, PublishedArtifact>, n: usize) {
    let mut ids: Vec<(String, u64)> = artifacts
        .iter()
        .map(|(id, a)| (id.clone(), a.seq))
        .collect();
    ids.sort_by_key(|(_, seq)| *seq);
    for (id, _) in ids.into_iter().take(n) {
        artifacts.remove(&id);
    }
}

/// Core remove logic — a no-op if `id` is unknown, matching
/// `checkpoints::record_original`'s tolerance for an already-gone id. The
/// frontend calls this on both pane close and session switch (see the design
/// doc's LIFECYCLE section), so a double-remove of the same id is an
/// expected, harmless race, not an error.
pub fn remove_impl(artifacts: &mut HashMap<String, PublishedArtifact>, id: &str) {
    artifacts.remove(id);
}

/// Injects a leading `<meta http-equiv="Content-Security-Policy">` carrying
/// [`ARTIFACT_CSP`] into a served HTML document, so the policy still applies
/// once the frontend re-loads the document from a headerless `blob:` URL (see
/// `ArtifactPane.tsx`'s tier-2 doc comment). Placed as early as possible so
/// no resource-triggering markup precedes it: right after `<head>` if one
/// exists, otherwise as a fresh `<head>` right after the `<html …>` open tag,
/// otherwise at the very start (browsers hoist a leading `<meta http-equiv>`
/// into an implicit head). Multiple CSPs on one document combine as an
/// intersection, so a policy the untrusted content itself adds can only ever
/// tighten this one, never loosen it.
fn inject_csp_meta(html: &str) -> String {
    let meta = format!(
        "<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">",
        ARTIFACT_CSP
    );

    // Case-insensitive search for the end of the first `<head …>` open tag,
    // then the first `<html …>` open tag, without pulling in a regex/HTML
    // parser for what is a single anchored insertion.
    let lower = html.to_ascii_lowercase();
    let after_open_tag = |needle: &str| -> Option<usize> {
        lower
            .find(needle)
            .and_then(|start| html[start..].find('>').map(|rel| start + rel + 1))
    };

    if let Some(pos) = after_open_tag("<head") {
        let mut out = String::with_capacity(html.len() + meta.len());
        out.push_str(&html[..pos]);
        out.push_str(&meta);
        out.push_str(&html[pos..]);
        out
    } else if let Some(pos) = after_open_tag("<html") {
        let mut out = String::with_capacity(html.len() + meta.len() + 13);
        out.push_str(&html[..pos]);
        out.push_str("<head>");
        out.push_str(&meta);
        out.push_str("</head>");
        out.push_str(&html[pos..]);
        out
    } else {
        format!("<head>{}</head>{}", meta, html)
    }
}

/// Publish `content` (tagged `kind`, today only `"html"`) so it becomes
/// servable at `artifact://localhost/<id>` (or `http://artifact.localhost/<id>`
/// on Windows — see `convertFileSrc` in `ArtifactPane.tsx`). Every call
/// mints a brand-new id: the frontend republishes on every preview
/// open/refresh (see the design doc's LIFECYCLE section) rather than
/// updating an existing entry in place.
#[tauri::command]
pub fn artifact_publish(
    state: tauri::State<'_, AppState>,
    content: String,
    kind: String,
) -> Result<String, String> {
    let mut artifacts = state
        .artifacts
        .lock()
        .map_err(|_| "Artifact lock poisoned".to_string())?;
    publish_impl(&mut artifacts, content, &kind)
}

/// Removes a published artifact from memory — called on pane close and
/// session switch so the in-memory map stays tiny (see the design doc's
/// LIFECYCLE section). Always succeeds, even for an unknown/already-removed
/// id — see [`remove_impl`]'s doc comment.
#[tauri::command]
pub fn artifact_remove(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let mut artifacts = state
        .artifacts
        .lock()
        .map_err(|_| "Artifact lock poisoned".to_string())?;
    remove_impl(&mut artifacts, &id);
    Ok(())
}

/// Core `artifact://` protocol-handler logic, parameterized directly by
/// `state` and the incoming request so it's testable without spinning up a
/// real webview (mirrors this module's other `_impl` functions). Wired into
/// `lib.rs`'s builder via `.register_uri_scheme_protocol("artifact", |ctx,
/// request| handle_request(ctx.app_handle().state::<AppState>().inner(),
/// &request))`. Serves a previously `artifact_publish`-ed document by id, or
/// a 404 for anything unknown/invalid — never touches disk (see this
/// module's doc comment).
///
/// AUTOMATED VERIFICATION (per the design doc's explicit phase-2 gate):
/// asserting a sandboxed artifact frame cannot reach Tauri IPC *inside a
/// loaded webview frame* isn't something `cargo test` can do — `#[test]`s run
/// on a worker thread, never the process's real main thread, and creating a
/// real WKWebView/WebView2/WebKitGTK requires exactly that (see `tauri::test`'s
/// own doc comment, which is why it ships a headless `MockRuntime` instead of
/// a real one). `examples/verify_artifact_ipc_isolation.rs` is the automated
/// replacement for what used to be a manual per-OS checklist here: it builds
/// a real app using this exact `handle_request`, loads a published artifact
/// exactly as `ArtifactPane.tsx` now does — fetched and re-served through a
/// `blob:` URL into a `sandbox="allow-scripts"` (no `allow-same-origin`)
/// iframe — and asserts the frame cannot actually *invoke* a command: it
/// registers a canary command and fails if the frame ever gets it to run.
/// Run it directly (`cargo run --manifest-path src-tauri/Cargo.toml --example
/// verify_artifact_ipc_isolation`) as part of the design doc's required per-OS
/// release gate; see that file's own doc comment for exit codes and why it's
/// an example rather than a `#[test]`.
///
/// WHY THE FRAME IS LOADED VIA `blob:` AND NOT DIRECTLY FROM `artifact://`:
/// on Windows specifically, `wry`'s WebView2 backend injects Tauri's IPC
/// bridge (including the invoke key) into *every* subframe regardless of the
/// `for_main_frame_only` flag Tauri sets, and Tauri's own `is_local_url`
/// treats any registered custom-protocol response (`http://artifact.localhost`
/// there) as a trusted *local* origin — so a subframe loaded straight from
/// `artifact://` could invoke privileged commands with the host window's full
/// capabilities. Re-serving the same document from a `blob:` URL gives the
/// frame a *remote* origin instead, which Tauri's ACL grants nothing (no
/// `remote` capability is configured), so the bridge is inert even where the
/// Windows subframe-script leak still plants it. That is the property this
/// check now verifies, and why running it ON WINDOWS is the part of the 3-OS
/// pass that matters most.
pub fn handle_request(
    state: &AppState,
    request: &tauri::http::Request<Vec<u8>>,
) -> tauri::http::Response<Vec<u8>> {
    fn not_found() -> tauri::http::Response<Vec<u8>> {
        tauri::http::Response::builder()
            .status(404)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(b"Not found".to_vec())
            .expect("building a static 404 response never fails")
    }

    let id = request.uri().path().trim_start_matches('/');
    if validate_id(id).is_err() {
        return not_found();
    }

    let Ok(artifacts) = state.artifacts.lock() else {
        return not_found();
    };
    let Some(artifact) = artifacts.get(id) else {
        return not_found();
    };

    tauri::http::Response::builder()
        .status(200)
        .header("Content-Type", artifact.mime)
        .header("X-Content-Type-Options", "nosniff")
        .header("Content-Security-Policy", ARTIFACT_CSP)
        // The trusted main frame fetches this document (a cross-origin GET
        // relative to its own origin) to re-serve it to the sandboxed frame
        // from a `blob:` URL — see `ArtifactPane.tsx`. That fetch needs CORS
        // to read the body; `*` is safe here because the content is already
        // CSP-locked (`connect-src 'none'`, no `allow-same-origin`) and keyed
        // by an unguessable per-publish uuid held only in memory.
        .header("Access-Control-Allow-Origin", "*")
        .body(inject_csp_meta(&artifact.content).into_bytes())
        .expect("building a response from an already-validated artifact never fails")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_for(path: &str) -> tauri::http::Request<Vec<u8>> {
        tauri::http::Request::builder()
            .uri(format!("artifact://localhost{}", path))
            .body(Vec::new())
            .unwrap()
    }

    /// Publishes directly against `state.artifacts` (bypassing the
    /// `#[tauri::command]`-wrapped `artifact_publish`, which needs a real
    /// `tauri::State` extractor) — mirrors `checkpoints.rs`'s tests calling
    /// `begin_impl`/`record_original` directly rather than their command
    /// wrappers.
    fn publish(state: &AppState, content: &str, kind: &str) -> String {
        let mut artifacts = state.artifacts.lock().unwrap();
        publish_impl(&mut artifacts, content.to_string(), kind).unwrap()
    }

    #[test]
    fn publish_then_fetch_roundtrips_content_and_headers() {
        let state = AppState::default();
        let id = publish(&state, "<h1>hi</h1>", "html");

        let response = handle_request(&state, &request_for(&format!("/{}", id)));
        assert_eq!(response.status(), 200);
        let body = String::from_utf8(response.body().clone()).unwrap();
        // The original content survives verbatim …
        assert!(body.contains("<h1>hi</h1>"));
        // … preceded by the injected CSP meta so the policy still applies
        // once the frontend re-loads this document from a headerless `blob:`
        // URL (see `ArtifactPane.tsx`'s tier-2 doc comment).
        assert!(body.contains("http-equiv=\"Content-Security-Policy\""));
        assert!(body.find("Content-Security-Policy").unwrap() < body.find("<h1>hi</h1>").unwrap());
        assert_eq!(
            response.headers().get("Content-Type").unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get("X-Content-Type-Options").unwrap(),
            "nosniff"
        );
        let csp = response
            .headers()
            .get("Content-Security-Policy")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(csp.contains("default-src 'none'"));
        assert!(
            csp.contains("connect-src 'none'"),
            "connect-src 'none' is what blocks network exfiltration"
        );
        assert!(csp.contains("script-src 'unsafe-inline'"));
    }

    #[test]
    fn csp_meta_is_injected_inside_head_when_present() {
        let injected = inject_csp_meta(
            "<!doctype html><html><head><title>t</title></head><body>x</body></html>",
        );
        let head_open = injected.find("<head>").unwrap();
        let meta = injected
            .find("http-equiv=\"Content-Security-Policy\"")
            .unwrap();
        let title = injected.find("<title>").unwrap();
        // Meta lands immediately after <head>, before any other head content.
        assert!(head_open < meta && meta < title);
        assert!(injected.contains("connect-src 'none'"));
    }

    #[test]
    fn csp_meta_is_injected_before_body_when_no_head() {
        // The exact shape the probe/most model artifacts use: no <head>.
        let injected =
            inject_csp_meta("<!doctype html><html><body><script>1</script></body></html>");
        let meta = injected
            .find("http-equiv=\"Content-Security-Policy\"")
            .unwrap();
        let script = injected.find("<script>").unwrap();
        assert!(meta < script, "CSP must precede any script in the document");
    }

    #[test]
    fn unknown_id_is_404() {
        let state = AppState::default();
        let response = handle_request(
            &state,
            &request_for("/00000000-0000-4000-8000-000000000000"),
        );
        assert_eq!(response.status(), 404);
    }

    #[test]
    fn traversal_and_malformed_ids_are_rejected_as_404_not_looked_up() {
        let state = AppState::default();
        // Every one of these is a syntactically valid URI path (so
        // constructing the test request itself can't fail) but not a plain
        // UUID-shaped id per `validate_id` — each must 404, not be looked up.
        for path in ["/../etc/passwd", "/id;drop", "/id_with_underscore", "/"] {
            let response = handle_request(&state, &request_for(path));
            assert_eq!(response.status(), 404, "path {path:?} must be rejected");
        }
    }

    #[test]
    fn publish_rejects_content_over_the_size_cap() {
        let mut artifacts = HashMap::new();
        let oversized = "a".repeat(MAX_ARTIFACT_BYTES + 1);
        let err = publish_impl(&mut artifacts, oversized, "html").unwrap_err();
        assert!(err.contains("exceeds"), "unexpected error: {err}");
    }

    #[test]
    fn publish_rejects_an_unsupported_kind() {
        let mut artifacts = HashMap::new();
        let err = publish_impl(&mut artifacts, "<svg/>".to_string(), "svg").unwrap_err();
        assert!(err.contains("Unsupported"), "unexpected error: {err}");
    }

    #[test]
    fn publish_evicts_the_oldest_entry_once_past_the_cap() {
        let mut artifacts = HashMap::new();
        let mut ids = Vec::new();
        for n in 0..MAX_ARTIFACTS {
            ids.push(publish_impl(&mut artifacts, format!("doc {n}"), "html").unwrap());
        }
        assert_eq!(artifacts.len(), MAX_ARTIFACTS);

        let newest = publish_impl(&mut artifacts, "one more".to_string(), "html").unwrap();

        assert_eq!(
            artifacts.len(),
            MAX_ARTIFACTS,
            "total count must stay capped"
        );
        assert!(
            !artifacts.contains_key(&ids[0]),
            "the oldest entry must have been evicted"
        );
        assert!(
            artifacts.contains_key(&ids[MAX_ARTIFACTS - 1]),
            "the second-oldest entry must survive"
        );
        assert!(artifacts.contains_key(&newest));
    }

    #[test]
    fn remove_is_a_noop_for_an_unknown_id() {
        let mut artifacts = HashMap::new();
        remove_impl(&mut artifacts, "00000000-0000-4000-8000-000000000000");
        assert!(artifacts.is_empty());
    }

    #[test]
    fn remove_deletes_a_published_artifact() {
        let mut artifacts = HashMap::new();
        let id = publish_impl(&mut artifacts, "<p>x</p>".to_string(), "html").unwrap();
        assert!(artifacts.contains_key(&id));
        remove_impl(&mut artifacts, &id);
        assert!(!artifacts.contains_key(&id));
    }

    /// The explicit capability-side half of this module's IPC-isolation
    /// guarantee (the other half is the opaque-origin `sandbox` attribute in
    /// `ArtifactPane.tsx`): parses the real on-disk capability file and
    /// asserts it can never extend IPC access to the `artifact` scheme.
    /// Tauri capabilities are scoped to *window labels* (`"windows": [...]`)
    /// — never to a URI scheme — and only gain reach beyond the app's own
    /// origin via an explicit `"remote"` entry, which this capability does
    /// not have. Without both of those being true, a future change to
    /// `capabilities/default.json` could silently make `invoke` reachable
    /// from an `artifact://` frame despite every guarantee this module's
    /// other tests and doc comments describe.
    #[test]
    fn capability_config_grants_no_scheme_access() {
        let raw = include_str!("../capabilities/default.json");
        let parsed: serde_json::Value =
            serde_json::from_str(raw).expect("capabilities/default.json must be valid JSON");

        assert!(
            parsed.get("remote").is_none(),
            "default.json must not have a \"remote\" section — that's the only mechanism that could \
             extend this capability's permissions (including IPC) to a non-app-origin frame like artifact://"
        );

        let windows = parsed["windows"]
            .as_array()
            .expect("default.json must list \"windows\"");
        for w in windows {
            let label = w.as_str().unwrap_or("");
            assert!(
                label == "main" || label.starts_with("session-"),
                "capability windows must be actual window labels, never a URI scheme: got {label:?}"
            );
        }

        // Belt-and-suspenders: the literal scheme name must not appear
        // anywhere in the capability file at all (e.g. smuggled into a
        // permission identifier or scope pattern).
        assert!(
            !raw.to_lowercase().contains("artifact"),
            "the artifact:// scheme must not be referenced anywhere in this capability file"
        );
    }
}

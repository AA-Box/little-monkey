use super::api::ApiResponse;

const INDEX_HTML: &str = include_str!("ui/index.html");
const APP_CSS: &str = include_str!("ui/app.css");
const APP_JS: &str = include_str!("ui/app.js");
const MANIFEST: &str = include_str!("ui/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("ui/sw.js");
const ICON: &str = include_str!("ui/icon.svg");

/// Public, credential-free controller shell. All run data still flows through
/// the signed `/v1/remote` API; the page contains no runner state or secret.
pub fn asset(method: &str, path_and_query: &str) -> Option<ApiResponse> {
    if method != "GET" && method != "HEAD" {
        return None;
    }
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    let (content_type, bytes) = match path {
        "/" | "/remote" | "/remote/" => ("text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/v1/remote/ui/app.css" => ("text/css; charset=utf-8", APP_CSS.as_bytes()),
        "/v1/remote/ui/app.js" => ("text/javascript; charset=utf-8", APP_JS.as_bytes()),
        // At the root, not under `/v1/remote/ui/`: a worker's default scope is
        // its own directory, and the controller it must serve lives at `/`.
        "/sw.js" => ("text/javascript; charset=utf-8", SERVICE_WORKER.as_bytes()),
        "/v1/remote/ui/manifest.webmanifest" => (
            "application/manifest+json; charset=utf-8",
            MANIFEST.as_bytes(),
        ),
        "/v1/remote/ui/icon.svg" | "/favicon.svg" => {
            ("image/svg+xml; charset=utf-8", ICON.as_bytes())
        }
        _ => return None,
    };
    Some(ApiResponse {
        status: 200,
        content_type,
        body: if method == "HEAD" {
            Vec::new()
        } else {
            bytes.to_vec()
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_assets_are_get_only_bounded_and_contain_no_embedded_secret() {
        for path in [
            "/",
            "/remote",
            "/v1/remote/ui/app.css",
            "/v1/remote/ui/app.js",
            "/v1/remote/ui/manifest.webmanifest",
            "/v1/remote/ui/icon.svg",
            "/sw.js",
        ] {
            let response = asset("GET", path).expect(path);
            assert_eq!(response.status, 200);
            assert!(response.body.len() < 1024 * 1024);
            let text = String::from_utf8_lossy(&response.body).to_ascii_lowercase();
            assert!(!text.contains("pairing_token\":"));
            assert!(!text.contains("device_secret\":"));
        }
        assert!(asset("POST", "/").is_none());
        assert!(asset("GET", "/v1/remote/ui/../remote-host.json").is_none());
        assert!(asset("GET", "/unknown").is_none());
    }

    #[test]
    fn controller_html_has_accessibility_and_no_inline_executable_content() {
        let html = String::from_utf8(asset("GET", "/").unwrap().body).unwrap();
        assert!(html.contains("name=\"viewport\""));
        assert!(html.contains("aria-live=\"polite\""));
        assert!(html.contains("<main"));
        assert!(!html.contains("<script>"));
        assert!(!html.contains(" style=\""));
    }

    #[test]
    fn controller_uses_non_exportable_key_replay_headers_and_no_plaintext_storage() {
        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();
        assert!(javascript.contains("crypto.subtle.importKey"));
        assert!(javascript.contains("false,\n    [\"sign\"]"));
        assert!(javascript.contains("indexedDB.open"));
        assert!(javascript.contains("navigator.locks.request"));
        assert!(javascript.contains("x-little-monkey-sequence"));
        assert!(javascript.contains("x-little-monkey-command"));
        assert!(javascript.contains("x-little-monkey-signature"));
        assert!(javascript.contains("after=${encodeURIComponent(String(cursor))}"));
        assert!(!javascript.contains("localStorage"));
        assert!(!javascript.contains("sessionStorage"));
        assert!(!javascript.contains("exportKey"));
        assert!(!javascript.contains("innerHTML"));
    }

    /// **The mobile client's parsers are checked against the Rust enums, not
    /// against a memory of them.**
    ///
    /// The defect this catches already happened: `RemoteAction::Pause` and
    /// `ControlDesktop` existed on the runner while `ALLOWED_ACTIONS` in
    /// `app.js` listed neither, so an invitation granting `pause` was rejected
    /// by this client as "unsupported" — a pairing that looked correct
    /// everywhere except on the phone. The same gap is available every time a
    /// capability is added, and a hand-written client-side allowlist has no way
    /// to notice.
    ///
    /// Scans the shipped script for each wire token. The technique is the same
    /// source-scanning ratchet `api.rs` uses against the published contract and
    /// `egress.rs` uses against bare HTTP clients, for the same reason: the
    /// defect class is "a second copy that drifts".
    #[test]
    fn the_mobile_client_parses_every_action_and_physical_capability_the_runner_grants() {
        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();
        let allowed_actions = javascript
            .split_once("const ALLOWED_ACTIONS = new Set([")
            .and_then(|(_, tail)| tail.split_once("]);"))
            .map(|(list, _)| list.to_string())
            .expect("app.js still declares ALLOWED_ACTIONS");
        for action in [
            super::super::protocol::RemoteAction::ViewRuns,
            super::super::protocol::RemoteAction::ViewEvents,
            super::super::protocol::RemoteAction::ReadArtifacts,
            super::super::protocol::RemoteAction::Approve,
            super::super::protocol::RemoteAction::Cancel,
            super::super::protocol::RemoteAction::Pause,
            super::super::protocol::RemoteAction::Kill,
            super::super::protocol::RemoteAction::ControlDesktop,
        ] {
            let token = serde_json::to_value(action).unwrap();
            let token = token.as_str().unwrap();
            assert!(
                allowed_actions.contains(&format!("\"{token}\"")),
                "the mobile client rejects '{token}', which the runner can grant"
            );
        }

        let capabilities = javascript
            .split_once("const DEVICE_CAPABILITIES = {")
            .and_then(|(_, tail)| tail.split_once("\n};"))
            .map(|(list, _)| list.to_string())
            .expect("app.js still declares DEVICE_CAPABILITIES");
        for capability in super::super::protocol::PHYSICAL_DEVICE_CAPABILITIES {
            let token = serde_json::to_value(capability).unwrap();
            let token = token.as_str().unwrap();
            assert!(
                capabilities.contains(&format!("{token}:")),
                "the mobile client has no entry for physical capability '{token}'"
            );
        }
    }

    /// The compact pairing code the phone scans and the one the runner prints
    /// have to be the same format. Both sides are checked here rather than
    /// only in Rust, because the parser that matters is the one on the phone.
    #[test]
    fn the_mobile_client_reads_the_compact_pairing_code_and_still_pins() {
        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();
        assert!(javascript.contains(super::super::protocol::PAIRING_URI_SCHEME));
        // The fingerprint check must survive the PEM's removal.
        assert!(javascript.contains("validateSha256(invitation.server_certificate_sha256"));
        assert!(javascript.contains("runnerUrl.origin !== location.origin"));
    }

    /// The push path the *bundled* client can actually take.
    ///
    /// An FCM-only push implementation would have been decoration here: this
    /// controller is a browser, its own content security policy forbids loading
    /// a Firebase SDK, and it could therefore never hold a registration token.
    /// These assertions pin the parts that make Web Push work end to end — the
    /// worker at a scope that covers the controller, `userVisibleOnly` so the
    /// browser's permission prompt is honest, and a CSP that permits a worker
    /// at all.
    #[test]
    fn the_controller_can_actually_subscribe_to_the_push_it_is_offered() {
        let worker =
            String::from_utf8(asset("GET", "/sw.js").expect("service worker").body).unwrap();
        assert!(worker.contains("addEventListener(\"push\""));
        assert!(worker.contains("showNotification"));

        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();
        assert!(javascript.contains("navigator.serviceWorker.register(\"/sw.js\")"));
        assert!(javascript.contains("userVisibleOnly: true"));
        assert!(javascript.contains("applicationServerKey"));
        // Unsubscribing has to reach both ends; dropping only one leaves a
        // notification path the user believes is closed.
        assert!(javascript.contains("subscription.unsubscribe()"));
        assert!(javascript.contains("\"DELETE\", \"/v1/remote/device/push\""));

        let html = String::from_utf8(asset("GET", "/").unwrap().body).unwrap();
        assert!(
            html.contains("worker-src 'self'"),
            "without worker-src the CSP's default-src 'none' blocks the service worker"
        );
    }

    /// The three things this client used to only pretend to do.
    ///
    /// Each assertion pins a *capability the runner will now dispatch*, so the
    /// grant and the implementation cannot drift apart again:
    ///
    /// - `voice_stream` was advertised as `() => false`, which meant a grant an
    ///   operator could make and a command no client would ever take. It is a
    ///   real recorder now, and this fails if it goes back to a stub.
    /// - `screen_capture` prompted on every single command, so an unattended
    ///   capture was impossible and the honest OS permission was never
    ///   "granted". One armed display stream replaces both.
    /// - `audio_playback` could only speak a sentence. It plays real audio now,
    ///   fetched over the artifact route that already exists.
    #[test]
    fn the_client_implements_every_physical_capability_it_advertises() {
        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();

        // A live stream: recorder, sequenced chunks, and a close that always
        // runs. `stopTracks` in the `finally` is what closes the microphone
        // when the stream fails rather than ends.
        assert!(javascript.contains("async function streamVoice("));
        assert!(javascript.contains("new MediaRecorder(stream)"));
        assert!(javascript.contains("/voice/${encodeURIComponent(sessionId)}/chunk"));
        assert!(javascript.contains("/voice/${encodeURIComponent(sessionId)}/close"));
        // The runner owns the sequence counter; a client that invented its own
        // would double-append after a dropped reply.
        assert!(javascript.contains("sequence = Number(answer.next_sequence"));
        // The stop signal rides the reply to a chunk, so a cancellation needs
        // no second poll to be seen.
        assert!(javascript.contains("if (answer.stop === true)"));
        assert!(
            !javascript.contains("voice_stream: () => false"),
            "voice_stream is grantable and dispatched; advertising it as unsupported \
             makes the grant a dead letter"
        );

        // One armed screen share, reused, and honestly reported.
        assert!(javascript.contains("function screenShareIsLive()"));
        assert!(javascript.contains("async function armScreenShare()"));
        assert!(javascript.contains("track.addEventListener(\"ended\""));
        assert!(
            javascript.contains("return screenShareIsLive() ? \"granted\" : \"undetermined\""),
            "the OS permission reported for screen capture must follow the armed stream"
        );

        // Real audio, through the artifact route that already exists rather
        // than a second way to move bytes to a device.
        assert!(javascript.contains("async function playArtifact("));
        assert!(javascript.contains("/artifacts/${encodeURIComponent(artifactId)}"));
        assert!(javascript.contains("new Audio(url)"));
        assert!(javascript.contains("URL.revokeObjectURL(url)"));
    }

    /// Offline means read-only. A device showing cached runs must not offer
    /// controls whose effect it cannot see, and must never buffer them for
    /// replay on reconnect.
    #[test]
    fn the_mobile_client_disables_side_effects_while_showing_cached_state() {
        let javascript = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.js")
                .expect("javascript asset")
                .body,
        )
        .unwrap();
        assert!(javascript.contains("function applyStaleState()"));
        assert!(javascript.contains("button.disabled = state.stale"));
        // The command loop only runs while online, so a lease is never taken
        // against a stale view.
        assert!(javascript.contains("while (state.profile && !state.stale)"));
        // `start` before the physical action, and a `started: false` reply
        // performs nothing — the exactly-once contract, on the client side.
        assert!(javascript.contains("if (started.started !== true)"));
    }

    #[test]
    fn controller_head_is_bodyless_and_query_does_not_change_asset_resolution() {
        let response = asset("HEAD", "/remote?ignored=1").expect("head asset");
        assert_eq!(response.status, 200);
        assert!(response.body.is_empty());
        assert_eq!(response.content_type, "text/html; charset=utf-8");
    }

    #[test]
    fn controller_styles_cover_touch_accessibility_and_responsive_breakpoints() {
        let css = String::from_utf8(
            asset("GET", "/v1/remote/ui/app.css")
                .expect("stylesheet asset")
                .body,
        )
        .unwrap();
        assert!(css.contains("min-height: 44px"));
        assert!(css.contains("@media (max-width: 960px)"));
        assert!(css.contains("@media (max-width: 680px)"));
        assert!(css.contains("@media (prefers-reduced-motion: reduce)"));
        assert!(css.contains(":focus-visible"));
    }
}

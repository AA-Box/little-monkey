use super::api::ApiResponse;

const INDEX_HTML: &str = include_str!("ui/index.html");
const APP_CSS: &str = include_str!("ui/app.css");
const APP_JS: &str = include_str!("ui/app.js");
const MANIFEST: &str = include_str!("ui/manifest.webmanifest");
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

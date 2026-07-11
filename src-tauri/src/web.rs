//! Web-research agent tools: `web_fetch` (phase 1, this module) and — in a
//! later phase — `web_search`.
//!
//! Structured exactly like `checkpoints.rs`: an AppHandle-free, directly
//! testable core (`validate_fetch_url`, `fetch_impl`) plus a thin
//! `#[tauri::command]` wrapper (`tool_web_fetch`) that adds the permission
//! gate and Stop-button cancellation. The AppHandle-free split matters for
//! the same reason it does there — `lm-cli` (phase 4) reuses `fetch_impl`
//! directly instead of duplicating the fetch pipeline.
//!
//! SSRF GUARD. [`validate_fetch_url`] is the actual security boundary of this
//! feature: `llama-server` (:8090) and Ollama (:11434) both listen on
//! loopback, so a page or prompt that talks the agent into fetching
//! `http://127.0.0.1:8090/...` (or any other private-range target) could
//! reach a service on the user's own machine that was never meant to be
//! internet-reachable. It is called once on the request's initial URL and
//! again, from inside the custom `reqwest::redirect::Policy`, on every
//! redirect hop — a server can otherwise pass the initial check with a public
//! URL and then 302 the client to a private one. DNS resolution for a
//! hostname (as opposed to a literal IP in the URL) uses the blocking
//! `std::net::ToSocketAddrs`, not an async resolver: `redirect::Policy::custom`
//! takes a *synchronous* closure (`Fn(Attempt<'_>) -> Action`), so there is no
//! way to `.await` inside it — see that closure's construction in
//! `fetch_impl` for why the whole guard is written sync-only rather than
//! having two copies (one async for the entry check, one blocking for
//! redirects). A brief blocking DNS lookup on the async runtime thread is an
//! accepted tradeoff here, same category as the "residual DNS-rebinding risk"
//! the design doc already calls out: this only re-resolves and re-checks
//! per-hop, it doesn't pin the resolved address for the connection itself.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

use tokio::sync::Notify;
use url::Url;

use crate::{permissions, AppState};

/// Total request timeout (connect through full body read) for `tool_web_fetch`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on how many bytes of the response body are read, regardless of
/// `Content-Length` (which may be absent or lie) — protects both memory and
/// the model's context window from an enormous or unbounded response.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Default char-window size when the model doesn't pass `max_chars`. Mirrors
/// the 20k-char cap precedent in `src/lib/mentions.ts`'s `truncateMentionContent`.
/// Settings-driven in phase 3 — hardcoded for now.
const DEFAULT_MAX_CHARS: usize = 20_000;

const USER_AGENT: &str = "LittleMonkey/0.1";

/// Whether fetching a loopback/private/link-local target is allowed.
/// Hardcoded to `false` for phase 1 — settings plumbing (the `allow_local_network`
/// toggle in `web_settings.json`) is phase 3.
const ALLOW_LOCAL_NETWORK: bool = false;

/// Checks whether `ip` falls in any of the ranges the design doc calls out:
/// loopback, RFC1918 private, and link-local. `Ipv4Addr::is_private` (the std
/// method) already covers exactly 10.0.0.0/8, 172.16.0.0/12, and
/// 192.168.0.0/16, so it's used as-is rather than hand-rolling the same
/// three CIDR checks.
fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

/// IPv6 equivalent of [`is_blocked_ipv4`]. `Ipv6Addr::is_loopback` is stable
/// std, but the unique-local (`fc00::/7`) and link-local (`fe80::/10`) checks
/// are hand-rolled here because the corresponding std predicates
/// (`is_unique_local`, `is_unicast_link_local`) are still gated behind the
/// unstable `ip` feature. An IPv4-mapped address (`::ffff:a.b.c.d`) is
/// unwrapped and re-checked against [`is_blocked_ipv4`] so a v4-mapped
/// private address can't slip past a v6-only check.
fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    if ip.is_loopback() {
        return true;
    }
    let octets = ip.octets();
    let unique_local = (octets[0] & 0xfe) == 0xfc; // fc00::/7
    let link_local = octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80; // fe80::/10
    unique_local || link_local
}

fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

/// Validates a candidate fetch URL: http(s) only, no embedded credentials,
/// and (unless `allow_local_network`) no loopback/private/link-local target —
/// checked against a literal IP host directly, or against every address a
/// hostname resolves to. Called both on the initial URL and, from inside the
/// custom redirect policy in [`fetch_impl`], on every redirect hop — see the
/// module doc for why re-checking every hop is the actual security boundary.
pub fn validate_fetch_url(url: &Url, allow_local_network: bool) -> Result<(), String> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(format!("Refusing to fetch '{}': only http/https URLs are allowed", url));
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err("Refusing to fetch a URL with embedded credentials".to_string());
    }

    if allow_local_network {
        return Ok(());
    }

    // `Url::host()` (unlike `host_str()`) gives back an already-parsed
    // `Host` enum — a literal IP is a real `IpAddr` here, not a bracketed
    // string (`"[::1]"`) that would need to be stripped before it could be
    // reparsed. Using this instead of string-parsing `host_str()` avoids
    // that whole bracket-handling class of bug for IPv6 literals.
    match url.host().ok_or_else(|| format!("Refusing to fetch '{}': no host", url))? {
        url::Host::Ipv4(ip) => {
            if is_blocked_ipv4(&ip) {
                return Err(format!("Refusing to fetch '{}': target host is a local/private address", url));
            }
        }
        url::Host::Ipv6(ip) => {
            if is_blocked_ipv6(&ip) {
                return Err(format!("Refusing to fetch '{}': target host is a local/private address", url));
            }
        }
        url::Host::Domain(domain) => {
            // Resolve and check every address the hostname maps to. The port
            // passed to `ToSocketAddrs` is irrelevant to the lookup itself.
            let addrs = (domain, 0u16)
                .to_socket_addrs()
                .map_err(|e| format!("Refusing to fetch '{}': failed to resolve host: {}", url, e))?;
            for addr in addrs {
                if is_blocked_ip(&addr.ip()) {
                    return Err(format!(
                        "Refusing to fetch '{}': host resolves to a local/private address",
                        url
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Result of a successful `web_fetch`, echoed back to the model as JSON.
/// Deliberately plain snake_case field names (no `serde(rename)`), matching
/// `Fact`'s (`memory.rs`) convention for tool results the model itself reads
/// — it's the same snake_case the model's own tool-call arguments use
/// (`max_chars`, `start_index`), not the camelCase Tauri IPC otherwise uses
/// for frontend-facing payloads like `CheckpointSummary`.
#[derive(serde::Serialize, Clone, Debug)]
pub struct FetchResult {
    /// The URL that was requested.
    pub url: String,
    /// The URL actually served, after following any redirects.
    pub final_url: String,
    /// The page's `<title>`, when the content was HTML and one was present.
    pub title: Option<String>,
    pub content_type: String,
    /// The page content — Markdown for HTML, passed through as-is for the
    /// other supported text types — windowed to `[start_index, start_index + max_chars)`.
    pub markdown: String,
    /// Total character count of the full (pre-windowing) content, so the
    /// model can tell whether/how far to page further with `start_index`.
    pub total_chars: usize,
    pub truncated: bool,
}

/// Extracts the content of a `<title>` tag via a simple, case-insensitive
/// regex-free scan — no need to pull in a full HTML parser (`scraper`, added
/// in a later phase for search-result scraping) just for this one tag.
/// Best-effort: returns `None` if there's no `<title>` at all; leaves entity
/// decoding to the caller's judgement since titles are just informational
/// here, not treated as trusted markup.
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find("</title")? + open_end;
    let raw = html[open_end..close].trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Windows `content` to `[start_index, start_index + max_chars)`, working in
/// `char`s (not bytes) so a multi-byte UTF-8 character is never split — the
/// same reasoning `truncateMentionContent` (`src/lib/mentions.ts`) doesn't
/// have to account for, since JS strings there are UTF-16 code units, not
/// UTF-8 bytes. Returns the window plus the full character count and whether
/// there is more content after the window for the model to page into with a
/// larger `start_index` — NOT whether the window itself started past 0, since
/// a window that reaches all the way to the end has nothing left to truncate
/// regardless of where it started.
fn char_window(content: &str, start_index: usize, max_chars: usize) -> (String, usize, bool) {
    let chars: Vec<char> = content.chars().collect();
    let total = chars.len();
    let start = start_index.min(total);
    let end = start.saturating_add(max_chars).min(total);
    let windowed: String = chars[start..end].iter().collect();
    let truncated = end < total;
    (windowed, total, truncated)
}

/// Content-Types `web_fetch` knows how to turn into text. Anything else
/// (images, binaries, video, ...) is rejected with an error naming the type
/// rather than silently returned as garbage.
fn dispatch_content(content_type: &str, body: &str) -> Result<(Option<String>, String), String> {
    // Strip any `; charset=...` parameter before matching.
    let base = content_type.split(';').next().unwrap_or(content_type).trim().to_ascii_lowercase();

    match base.as_str() {
        "text/html" => {
            let title = extract_title(body);
            let markdown = htmd::convert(body).map_err(|e| format!("Failed to convert HTML to Markdown: {}", e))?;
            Ok((title, markdown))
        }
        "text/plain" | "text/markdown" | "application/json" | "application/xml" | "text/xml" => Ok((None, body.to_string())),
        other => Err(format!(
            "Cannot fetch content of type '{}': only HTML, plain text, Markdown, JSON, and XML are supported",
            other
        )),
    }
}

/// Builds the redirect policy [`fetch_impl`] hands to `reqwest`: re-runs
/// [`validate_fetch_url`] against every hop's target (`Attempt::url()` is
/// already a parsed `Url`, so no re-parsing is needed) and errors the whole
/// request out the moment one fails, rather than silently stopping at the
/// last-good URL — a caller must not mistake a blocked redirect for a
/// successful fetch of the pre-redirect page. Factored out of [`fetch_impl`]
/// so a test can drive the exact same policy against a real local server
/// without going through the rest of the fetch pipeline (whose own entry
/// check would otherwise reject a `127.0.0.1` test server before ever
/// reaching the redirect).
fn build_redirect_policy(allow_local_network: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| match validate_fetch_url(attempt.url(), allow_local_network) {
        Ok(()) => attempt.follow(),
        Err(e) => attempt.error(std::io::Error::new(std::io::ErrorKind::PermissionDenied, e)),
    })
}

/// Core `web_fetch` logic: AppHandle-free and directly testable (against a
/// local fixture server) — see the module doc for why. Not itself
/// cancellable; [`tool_web_fetch`] wraps the call in a `tokio::select!` against
/// the turn's cancel `Notify`, the same split `tool_run_shell` uses for its
/// child-process future.
pub async fn fetch_impl(url: String, max_chars: Option<usize>, start_index: Option<usize>) -> Result<FetchResult, String> {
    let max_chars = max_chars.unwrap_or(DEFAULT_MAX_CHARS);
    let start_index = start_index.unwrap_or(0);

    let parsed = Url::parse(&url).map_err(|e| format!("Invalid URL '{}': {}", url, e))?;
    validate_fetch_url(&parsed, ALLOW_LOCAL_NETWORK)?;

    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .redirect(build_redirect_policy(ALLOW_LOCAL_NETWORK))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let mut response = client
        .get(parsed.clone())
        .send()
        .await
        .map_err(|e| format!("Failed to fetch '{}': {}", url, e))?;

    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    // Streamed read capped at MAX_BODY_BYTES regardless of what (if
    // anything) Content-Length claims.
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("Failed to read response body from '{}': {}", url, e))?
    {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= MAX_BODY_BYTES {
            bytes.truncate(MAX_BODY_BYTES);
            break;
        }
    }

    let body = String::from_utf8_lossy(&bytes).into_owned();
    let (title, full_content) = dispatch_content(&content_type, &body)?;
    let (markdown, total_chars, truncated) = char_window(&full_content, start_index, max_chars);

    Ok(FetchResult {
        url,
        final_url,
        title,
        content_type,
        markdown,
        total_chars,
        truncated,
    })
}

/// Fetch a URL and return its content as Markdown (HTML) or as-is (plain
/// text/Markdown/JSON/XML), windowed to `[start_index, start_index + max_chars)`.
/// Permission-gated: prompts with the full URL as detail, exactly like
/// `tool_run_shell` prompts with the command. `turn_id` (injected by the
/// frontend agent loop, never model-supplied) scopes both the permission
/// prompt and Stop-button cancellation to the calling turn — with the split
/// pane, another turn's fetch may be in flight concurrently and must not be
/// killed by this turn's Stop.
///
/// `rename_all = "snake_case"`: matches every other tool command, so the
/// model's snake_case tool-call arguments (`max_chars`, `start_index`) and the
/// agent loop's injected `turn_id` are accepted without translation.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_web_fetch(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
    turn_id: Option<String>,
) -> Result<FetchResult, String> {
    permissions::request_permission(&app, state.inner(), "web_fetch", url.clone(), turn_id.as_deref())
        .await?;

    // Same per-turn cancellation channel `tool_run_shell` uses — see
    // `AppState::tool_cancel`'s doc comment. Callers that don't thread a turn
    // id share the "" channel.
    let cancel_key = turn_id.unwrap_or_default();
    let cancel = state
        .tool_cancel
        .lock()
        .map_err(|_| "Tool-cancel lock poisoned".to_string())?
        .entry(cancel_key.clone())
        .or_insert_with(|| std::sync::Arc::new(Notify::new()))
        .clone();

    let outcome = tokio::select! {
        result = fetch_impl(url, max_chars, start_index) => result,
        _ = cancel.notified() => Err("Fetch cancelled by the user".to_string()),
    };

    // Drop this turn's channel once no other in-flight tool of the same turn
    // still holds it (strong count 2 = the map's Arc + our clone) — mirrors
    // `tool_run_shell`'s own cleanup so the map doesn't grow one entry per
    // turn forever.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard.get(&cancel_key).is_some_and(|n| std::sync::Arc::strong_count(n) <= 2) {
            guard.remove(&cancel_key);
        }
    }

    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn rejects_non_http_schemes() {
        let err = validate_fetch_url(&url("file:///etc/passwd"), false).unwrap_err();
        assert!(err.contains("only http/https"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_embedded_credentials() {
        let err = validate_fetch_url(&url("http://user:pass@example.com/"), false).unwrap_err();
        assert!(err.contains("credentials"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_loopback_ipv4_literal() {
        let err = validate_fetch_url(&url("http://127.0.0.1:8090/v1/chat"), false).unwrap_err();
        assert!(err.contains("local/private"), "unexpected error: {err}");
    }

    #[test]
    fn rejects_ollama_loopback_port() {
        assert!(validate_fetch_url(&url("http://127.0.0.1:11434/api/tags"), false).is_err());
        assert!(validate_fetch_url(&url("http://localhost:11434/api/tags"), false).is_err());
    }

    #[test]
    fn rejects_each_private_ipv4_range() {
        for host in ["10.1.2.3", "172.16.0.1", "172.31.255.255", "192.168.1.1", "169.254.1.1"] {
            let target = format!("http://{host}/");
            assert!(
                validate_fetch_url(&url(&target), false).is_err(),
                "expected {host} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_ipv6_loopback_unique_local_and_link_local() {
        for host in ["[::1]", "[fc00::1]", "[fd12:3456:789a::1]", "[fe80::1]"] {
            let target = format!("http://{host}/");
            assert!(
                validate_fetch_url(&url(&target), false).is_err(),
                "expected {host} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_private_address() {
        let err = validate_fetch_url(&url("http://[::ffff:127.0.0.1]/"), false).unwrap_err();
        assert!(err.contains("local/private"), "unexpected error: {err}");
    }

    #[test]
    fn accepts_public_ipv4_and_ipv6_literals() {
        assert!(validate_fetch_url(&url("http://93.184.216.34/"), false).is_ok());
        assert!(validate_fetch_url(&url("http://[2606:2800:220:1:248:1893:25c8:1946]/"), false).is_ok());
    }

    #[test]
    fn allow_local_network_bypasses_the_private_range_check() {
        assert!(validate_fetch_url(&url("http://127.0.0.1:8090/"), true).is_ok());
    }

    #[test]
    fn char_window_reports_total_and_truncation_correctly() {
        let content = "0123456789";
        let (window, total, truncated) = char_window(content, 0, 5);
        assert_eq!(window, "01234");
        assert_eq!(total, 10);
        assert!(truncated);

        let (window, total, truncated) = char_window(content, 5, 5);
        assert_eq!(window, "56789");
        assert_eq!(total, 10);
        assert!(!truncated, "window covering exactly the tail must not be marked truncated");

        let (window, _total, truncated) = char_window(content, 0, 100);
        assert_eq!(window, content);
        assert!(!truncated, "window covering the whole content must not be marked truncated");
    }

    #[test]
    fn char_window_never_splits_a_multibyte_character() {
        // Each of these is one `char` but multiple UTF-8 bytes — a
        // byte-based slice would panic or produce invalid UTF-8 mid-window.
        let content = "a\u{00e9}b\u{4e2d}c"; // a é b 中 c
        let (window, total, _truncated) = char_window(content, 1, 2);
        assert_eq!(total, 5);
        assert_eq!(window, "\u{00e9}b");
    }

    #[tokio::test]
    async fn html_is_converted_to_markdown_with_title_extracted() {
        let html = "<html><head><title>Hello World</title></head><body><h1>Heading</h1><p>Some text.</p></body></html>";
        let (title, markdown) = dispatch_content("text/html; charset=utf-8", html).unwrap();
        assert_eq!(title.as_deref(), Some("Hello World"));
        assert!(markdown.contains("Heading"));
        assert!(markdown.contains("Some text."));
    }

    #[tokio::test]
    async fn plain_text_content_types_pass_through_unchanged() {
        for ct in ["text/plain", "text/markdown", "application/json", "application/xml"] {
            let (title, content) = dispatch_content(ct, "raw content").unwrap();
            assert!(title.is_none());
            assert_eq!(content, "raw content");
        }
    }

    #[tokio::test]
    async fn unsupported_content_type_is_rejected_by_name() {
        let err = dispatch_content("image/png", "binary").unwrap_err();
        assert!(err.contains("image/png"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn fetch_impl_rejects_a_disallowed_url_without_making_a_request() {
        let err = fetch_impl("http://127.0.0.1:8090/".to_string(), None, None).await.unwrap_err();
        assert!(err.contains("local/private"), "unexpected error: {err}");
    }

    /// Exercises the real `reqwest::redirect::Policy` (not just
    /// `validate_fetch_url` in isolation): a genuine local HTTP server sends a
    /// real 302 pointing at a private-range target, and the request must fail
    /// rather than transparently follow it. This is the redirect-hop guard
    /// the SSRF risk note calls the actual security boundary — a server that
    /// looks public on the entry check can still try to redirect the client
    /// somewhere private after the fact.
    ///
    /// The redirect target (`10.0.0.5`) is never actually connected to: the
    /// policy closure runs (and errors out) before reqwest issues the next
    /// request, so this test needs no second server.
    #[tokio::test]
    async fn redirect_hop_to_a_private_ip_is_blocked() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = "HTTP/1.1 302 Found\r\nLocation: http://10.0.0.5/private\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let client = reqwest::Client::builder()
            .redirect(build_redirect_policy(false))
            .build()
            .unwrap();

        let result = client.get(format!("http://{}/", addr)).send().await;
        assert!(result.is_err(), "expected the redirect to a private IP to be blocked");
    }
}

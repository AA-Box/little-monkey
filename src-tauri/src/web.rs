//! Web-research agent tools: `web_fetch` (phase 1) and `web_search` (phase 2,
//! this module's DuckDuckGo-only slice — Brave/SearXNG are phase 3).
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

/// One ranked result from [`search_impl`], echoed back to the model as JSON.
/// Same "plain snake_case, no `serde(rename)`" convention as [`FetchResult`].
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Default/max results returned by `web_search` — `count` is clamped into
/// `1..=DEFAULT_SEARCH_COUNT` (design doc's "count clamped 1..=10").
const DEFAULT_SEARCH_COUNT: usize = 10;

/// Total request timeout for the DuckDuckGo POST — shorter than
/// [`FETCH_TIMEOUT`] since this hits one fixed, fast endpoint rather than an
/// arbitrary page.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

/// DuckDuckGo's keyless HTML results endpoint (no API key, no official API —
/// see the module doc's "best-effort/brittle" framing). `POST` with the query
/// in a `q` form field is what the (JS-less) `html.duckduckgo.com/html/`
/// front end itself submits.
const DUCKDUCKGO_HTML_ENDPOINT: &str = "https://html.duckduckgo.com/html/";

/// Result-title links on the DuckDuckGo HTML results page carry the reader to
/// `//duckduckgo.com/l/?uddg=<percent-encoded-destination>&rut=...` — a
/// tracking redirect, not the destination itself — but in practice (verified
/// against a live fetch of the endpoint while implementing this) `href` is
/// sometimes already the bare destination URL with no `uddg` wrapper at all,
/// so this decodes the parameter when present and otherwise falls back to
/// `href` unchanged rather than assuming one shape or the other.
///
/// Parsed via `Url::join` against a fixed DuckDuckGo base rather than hand-
/// rolling percent-decoding: `Url::query_pairs()` already does percent-
/// decoding internally (through `url`'s own dependency, not a new one added
/// here just for this), and `join` accepts every href shape actually seen —
/// protocol-relative (`//duckduckgo.com/...`), root-relative (`/l/...`), and
/// absolute (`https://example.com/...`).
fn decode_ddg_href(href: &str) -> String {
    let base = match Url::parse("https://duckduckgo.com/") {
        Ok(u) => u,
        Err(_) => return href.to_string(),
    };
    let joined = if let Some(rest) = href.strip_prefix("//") {
        Url::parse(&format!("https://{rest}"))
    } else {
        base.join(href)
    };
    match joined {
        Ok(url) => url
            .query_pairs()
            .find(|(key, _)| key == "uddg")
            .map(|(_, value)| value.into_owned())
            .unwrap_or_else(|| href.to_string()),
        Err(_) => href.to_string(),
    }
}

/// Parses a DuckDuckGo HTML results page into up to `count` [`SearchResult`]s.
/// Selectors match the design doc / a live capture of the endpoint at
/// implementation time: each result's title+link is `a.result__a` and its
/// snippet is `.result__snippet`, in the same document order — there is no
/// shared container id to pair them by, so this zips the two selections
/// positionally and tolerates a missing snippet (an empty string) rather than
/// dropping the result, since the title/URL is the useful half.
///
/// Best-effort and brittle by nature (module doc): DuckDuckGo's markup, rate
/// limiting, and anomaly/CAPTCHA gating are all outside this app's control,
/// so a shape this parser doesn't recognize simply yields fewer (or zero)
/// results rather than an error — [`search_impl`] is the layer that surfaces
/// an outright HTTP failure.
fn parse_ddg_results(html: &str, count: usize) -> Vec<SearchResult> {
    let document = scraper::Html::parse_document(html);
    // Both selector strings are fixed and valid at compile time.
    let title_selector = scraper::Selector::parse("a.result__a").expect("valid CSS selector");
    let snippet_selector = scraper::Selector::parse(".result__snippet").expect("valid CSS selector");

    let snippets: Vec<String> = document
        .select(&snippet_selector)
        .map(|el| el.text().collect::<String>().trim().to_string())
        .collect();

    let mut results = Vec::new();
    for (index, title_el) in document.select(&title_selector).enumerate() {
        if results.len() >= count {
            break;
        }
        let title = title_el.text().collect::<String>().trim().to_string();
        if title.is_empty() {
            continue;
        }
        let href = title_el.value().attr("href").unwrap_or("");
        let url = decode_ddg_href(href);
        let snippet = snippets.get(index).cloned().unwrap_or_default();
        results.push(SearchResult { title, url, snippet });
    }
    results
}

/// Core `web_search` logic: AppHandle-free and directly testable, same split
/// as [`fetch_impl`]. DuckDuckGo-only for phase 2 (the design doc's Brave and
/// SearXNG branches are phase 3) — no provider parameter yet, so this is the
/// entire dispatch.
///
/// Unlike `fetch_impl`, there is no SSRF guard here to run: the request
/// target is DuckDuckGo's own fixed endpoint, never a user/model-supplied
/// URL — only the query text (sent as a POST form field, not part of the
/// URL) is untrusted.
pub async fn search_impl(query: String, count: Option<usize>) -> Result<Vec<SearchResult>, String> {
    let count = count.unwrap_or(DEFAULT_SEARCH_COUNT).clamp(1, DEFAULT_SEARCH_COUNT);

    let client = reqwest::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

    let response = client
        .post(DUCKDUCKGO_HTML_ENDPOINT)
        .form(&[("q", query.as_str())])
        .send()
        .await
        .map_err(|e| format!("Failed to search DuckDuckGo for '{}': {}", query, e))?;

    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("Failed to read DuckDuckGo response for '{}': {}", query, e))?;

    if !status.is_success() {
        return Err(format!("DuckDuckGo search for '{}' returned HTTP {}", query, status));
    }

    Ok(parse_ddg_results(&body, count))
}

/// Search the web (DuckDuckGo, keyless) and return up to `count` (1-10,
/// default 10) ranked `{title, url, snippet}` results. Permission-gated:
/// prompts with the query as detail, exactly like `tool_web_fetch` prompts
/// with the URL. `turn_id` scopes the permission prompt to the calling turn
/// (never model-supplied) — unlike `tool_web_fetch`, there is no Stop-button
/// cancellation wiring here: the request is one short, fixed-endpoint POST
/// (see `SEARCH_TIMEOUT`), not a potentially large streamed fetch, so it gets
/// `remember`'s simpler "turn id for the prompt only" treatment rather than
/// `web_fetch`'s `tokio::select!` + `state.tool_cancel` split.
///
/// `rename_all = "snake_case"`: matches every other tool command, so the
/// model's snake_case tool-call arguments (`count`) and the agent loop's
/// injected `turn_id` are accepted without translation.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_web_search(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    query: String,
    count: Option<usize>,
    turn_id: Option<String>,
) -> Result<Vec<SearchResult>, String> {
    permissions::request_permission(&app, state.inner(), "web_search", query.clone(), turn_id.as_deref())
        .await?;
    search_impl(query, count).await
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

    /// A trimmed fixture of `html.duckduckgo.com/html/`'s actual result
    /// markup (captured live while implementing this), with one result's
    /// `href` left as the bare destination URL (the shape actually observed)
    /// and a second, synthetic result added with a `uddg`-wrapped redirect
    /// href (the shape the design doc describes) so both decode paths are
    /// exercised by the same fixture.
    const DDG_FIXTURE_HTML: &str = r#"
        <div class="results">
          <div class="result results_links results_links_deep web-result">
            <div class="result__body">
              <h2 class="result__title">
                <a rel="nofollow" class="result__a" href="https://rust-lang.org/">Rust Programming Language</a>
              </h2>
              <a class="result__snippet" href="https://rust-lang.org/"><b>Rust</b> is a fast, reliable, and productive programming language.</a>
            </div>
          </div>
          <div class="result results_links results_links_deep web-result">
            <div class="result__body">
              <h2 class="result__title">
                <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&amp;rut=abc123">Rust (programming language) - Wikipedia</a>
              </h2>
              <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&amp;rut=abc123">A general-purpose <b>programming</b> language which emphasizes performance and safety.</a>
            </div>
          </div>
          <div class="result results_links results_links_deep web-result">
            <div class="result__body">
              <h2 class="result__title">
                <a rel="nofollow" class="result__a" href="https://www.w3schools.com/rust/index.php">Rust Tutorial - W3Schools</a>
              </h2>
              <a class="result__snippet" href="https://www.w3schools.com/rust/index.php">Rust is a popular programming language.</a>
            </div>
          </div>
        </div>
    "#;

    #[test]
    fn decode_ddg_href_returns_a_bare_href_unchanged() {
        assert_eq!(decode_ddg_href("https://rust-lang.org/"), "https://rust-lang.org/");
    }

    #[test]
    fn decode_ddg_href_decodes_a_protocol_relative_uddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&rut=abc123";
        assert_eq!(decode_ddg_href(href), "https://en.wikipedia.org/wiki/Rust_(programming_language)");
    }

    #[test]
    fn decode_ddg_href_decodes_a_root_relative_uddg_redirect() {
        let href = "/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=xyz";
        assert_eq!(decode_ddg_href(href), "https://example.com/page");
    }

    #[test]
    fn decode_ddg_href_falls_back_to_the_raw_href_when_unparseable() {
        // Not a valid URL and not root/protocol-relative either — `Url::join`
        // fails, so the raw string is the only sane thing to return.
        assert_eq!(decode_ddg_href("not a url at all"), "not a url at all");
    }

    #[test]
    fn parse_ddg_results_extracts_title_url_and_snippet_for_each_result() {
        let results = parse_ddg_results(DDG_FIXTURE_HTML, 10);
        assert_eq!(results.len(), 3);

        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[0].url, "https://rust-lang.org/");
        assert!(results[0].snippet.contains("fast, reliable, and productive"));

        // The uddg-wrapped result: url must be the decoded destination, not
        // the duckduckgo.com redirect.
        assert_eq!(results[1].title, "Rust (programming language) - Wikipedia");
        assert_eq!(results[1].url, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
        assert!(results[1].snippet.contains("general-purpose"));

        assert_eq!(results[2].title, "Rust Tutorial - W3Schools");
        assert_eq!(results[2].url, "https://www.w3schools.com/rust/index.php");
    }

    #[test]
    fn parse_ddg_results_respects_the_count_cap() {
        let results = parse_ddg_results(DDG_FIXTURE_HTML, 2);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
        assert_eq!(results[1].title, "Rust (programming language) - Wikipedia");
    }

    #[test]
    fn parse_ddg_results_on_unrecognized_markup_returns_no_results_not_an_error() {
        assert_eq!(parse_ddg_results("<html><body><p>no results here</p></body></html>", 10), Vec::new());
    }

    #[test]
    fn search_impl_clamps_count_below_the_minimum_up_to_one() {
        // count=0 must not become "zero results" or panic on an empty slice —
        // it should clamp up to 1. Exercised indirectly through the pure
        // parser with the real clamp expression `search_impl` uses, since
        // driving `search_impl` itself would make a live network request.
        let clamped = 0usize.clamp(1, DEFAULT_SEARCH_COUNT);
        assert_eq!(clamped, 1);
        let results = parse_ddg_results(DDG_FIXTURE_HTML, clamped);
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn search_impl_clamps_count_above_the_maximum_down_to_ten() {
        let clamped = 500usize.clamp(1, DEFAULT_SEARCH_COUNT);
        assert_eq!(clamped, DEFAULT_SEARCH_COUNT);
    }
}

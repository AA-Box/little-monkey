//! The served half of the web chat channel: the page, and the three small JSON
//! routes it talks to.
//!
//! # Why here and not on the webhook listener
//!
//! There is no provider. Nobody registers a callback, no operator publishes a
//! URL, and nothing signs a body. What authenticates a request on this surface
//! is that it reached *this* listener — the daemon's own, under the operator's
//! own pinned certificate, on the address they configured for the controller
//! shell — carrying an identifier this process minted. That is the same
//! argument the Talk socket makes for its one-use ticket, and it is why
//! `ChannelKind::WebChat` stays in `build_webhook_adapter`'s refusal arm: the
//! public `POST /v1/channels/<account>` endpoint is the one operators are told
//! to expose through a proxy or a tunnel, and it must not be a second way in.
//!
//! Nothing here is a second listener, a second admission plane or a second
//! pairing store. The listener is the remote host's; the acceptance is
//! `webhook::accept_webhook_delivery`, the same function the public listener
//! calls; and who may cause a run is decided where it always is, in
//! `channel_ingress::accept_channel_envelope` under the account's own sender
//! policy. A first-time visitor gets a pairing code queued on this account's
//! outbox and reads it back on this page's next poll.
//!
//! # The visitor identifier
//!
//! Minted here, never accepted from a client on `/session`, and *self*
//! verifying: it is `base64url(nonce16 || HMAC-SHA256(key, account || 0 ||
//! nonce)[..16])` over a 32-byte key kept beside the daemon's other state at
//! `webchat-visitor.key`. So a browser that invents a well-formed-looking
//! identifier is refused rather than opening somebody else's conversation, and
//! nothing has to be stored per visitor to make that true.
//!
//! It grants no authority to run anything — pairing decides that — but it does
//! address one conversation's transcript, which this page reads back. It is
//! therefore that conversation's bearer, which is why it travels in a request
//! header rather than a query string, and why the sender and conversation ids
//! are the *hash* of it: the durable database never holds the value a browser
//! presents, so a copied database is no way to read or send as anybody.
//!
//! # Which peers may reach it
//!
//! Loopback, unless the account says otherwise. The listener itself stays
//! wherever the operator put it — `monkey daemon remote host-configure --listen
//! 0.0.0.0:8443` keeps working, and the controller shell and the signed device
//! API keep answering on it — but those two demand a signed paired device or a
//! one-use ticket, and these three JSON routes demand nothing. They are this
//! listener's only unauthenticated *writing* surface, so widening the bind must
//! not widen them by side effect: a non-loopback peer is answered `404` exactly
//! as an unknown account is, until the account's own `public` flag is set.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE};
use hyper::{Response, StatusCode};

use crate::daemon::adapters::webchat::{visitor_conversation_id, WebChatAdapter};
use crate::daemon::channel_adapter::AdapterConfig;
use crate::daemon::store::{restrict_file, DaemonPaths, DaemonStore};
use crate::daemon::webhook::{accept_webhook_delivery, DeliveryOutcome, WebhookDelivery};
use little_monkey_lib::channels::types::ChannelKind;

const PAGE_HTML: &str = include_str!("ui/webchat.html");
const PAGE_JS: &str = include_str!("ui/webchat.js");
const PAGE_CSS: &str = include_str!("ui/webchat.css");

/// This page's content security policy, sent as a header **and** copied
/// verbatim into the document's own `<meta http-equiv>`. The browser enforces
/// the intersection of the two, so a directive missing from either one is a
/// directive that blocks; a test asserts they are identical.
pub(crate) const WEBCHAT_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; \
     connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; \
     frame-ancestors 'none'; object-src 'none'";

/// Far below the remote plane's own cap: one browser message is text, and the
/// adapter caps the text again at 4 000 characters.
pub(crate) const MAX_BODY_BYTES: usize = 16 * 1024;

/// How many messages one poll reads back.
const TRANSCRIPT_LIMIT: u32 = 50;

/// The visitor's identifier on a read. A header rather than a query parameter:
/// it is a long-lived bearer for one conversation's transcript, and a query
/// string ends up in history, referrers and proxy logs.
pub(crate) const VISITOR_HEADER: &str = "x-webchat-visitor";

// --- Rate limit -------------------------------------------------------------

/// One fixed window per key. Deliberately its own thirty lines rather than a
/// share of `LegacyTokenRateLimiter`: that one is the migration limiter for
/// legacy `lmk-*` tokens, keyed by a persisted token id and shared by every
/// endpoint of the logical server, and a browser visitor is none of those.
const WINDOW_MS: i64 = 60_000;
const MAX_PER_WINDOW: u32 = 60;
/// The whole account's ceiling, checked before any state is opened. Generous
/// next to the per-visitor one because every visitor of one account shares it
/// — a page polls every five seconds while its tab is visible — and it exists
/// to stop an unauthenticated loop making this daemon open SQLite forever,
/// not to ration ordinary use.
const MAX_PER_WINDOW_ACCOUNT: u32 = 600;

#[derive(Default)]
struct RateLimit {
    hits: Mutex<HashMap<String, (i64, u32)>>,
}

impl RateLimit {
    fn allow(&self, key: &str, now_ms: i64) -> bool {
        self.allow_up_to(key, now_ms, MAX_PER_WINDOW)
    }

    fn allow_up_to(&self, key: &str, now_ms: i64, ceiling: u32) -> bool {
        let mut hits = self.hits.lock().unwrap_or_else(|error| error.into_inner());
        // Cheap sweep, so a long-running daemon does not accumulate a row per
        // visitor that ever loaded the page.
        if hits.len() > 1024 {
            hits.retain(|_, (started, _)| now_ms.saturating_sub(*started) < WINDOW_MS);
        }
        let entry = hits.entry(key.to_string()).or_insert((now_ms, 0));
        if now_ms.saturating_sub(entry.0) >= WINDOW_MS {
            *entry = (now_ms, 0);
        }
        entry.1 += 1;
        entry.1 <= ceiling
    }
}

fn limiter() -> &'static RateLimit {
    static LIMITER: std::sync::OnceLock<RateLimit> = std::sync::OnceLock::new();
    LIMITER.get_or_init(RateLimit::default)
}

// --- Routing ----------------------------------------------------------------

/// What one request to `/webchat/...` is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// The page itself.
    Page(String),
    /// One of the two static assets it loads.
    Asset(&'static str),
    /// Mint this browser a visitor identifier.
    Session(String),
    /// One message from the visitor.
    Post(String),
    /// This visitor's own transcript.
    Fetch(String),
}

impl Route {
    /// The account this route names, when it names one. `Asset` does not: it
    /// is the page's own script and stylesheet, the same static bytes for
    /// every account.
    fn account_id(&self) -> Option<&str> {
        match self {
            Route::Asset(_) => None,
            Route::Page(account)
            | Route::Session(account)
            | Route::Post(account)
            | Route::Fetch(account) => Some(account),
        }
    }
}

/// Account ids are minted by `channels add`; anything else is not a route.
fn plausible_account_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// The route this request names, if it is one of ours at all.
pub(crate) fn target(method: &str, path_and_query: &str) -> Option<Route> {
    let path = path_and_query
        .split_once('?')
        .map_or(path_and_query, |(path, _)| path);
    let rest = path.strip_prefix("/webchat/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    match (method, segments.as_slice()) {
        ("GET", ["ui", "webchat.js"]) => Some(Route::Asset("js")),
        ("GET", ["ui", "webchat.css"]) => Some(Route::Asset("css")),
        ("GET", [account]) if plausible_account_id(account) => {
            Some(Route::Page((*account).to_string()))
        }
        ("POST", [account, "session"]) if plausible_account_id(account) => {
            Some(Route::Session((*account).to_string()))
        }
        ("POST", [account, "messages"]) if plausible_account_id(account) => {
            Some(Route::Post((*account).to_string()))
        }
        ("GET", [account, "messages"]) if plausible_account_id(account) => {
            Some(Route::Fetch((*account).to_string()))
        }
        _ => None,
    }
}

// --- The visitor identifier -------------------------------------------------

/// The HMAC key that makes a visitor identifier self-verifying, created on
/// first use beside the daemon's other state and readable only by its owner.
fn visitor_key(paths: &DaemonPaths) -> Result<ring::hmac::Key, String> {
    let path = paths.root.join("webchat-visitor.key");
    match std::fs::read(&path) {
        Ok(bytes) if bytes.len() == 32 => {
            return Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
        }
        Ok(_) => return Err("The web chat visitor key is the wrong length".to_string()),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(format!("Could not read the web chat visitor key: {error}"))
        }
        Err(_) => {}
    }
    paths.ensure()?;
    // Generated as a value rather than filled into a zeroed buffer: the buffer
    // form reads — to a human and to a static analyser alike — as a hard-coded
    // key that something happens to overwrite.
    let fresh = ring::rand::generate::<[u8; 32]>(&ring::rand::SystemRandom::new())
        .map_err(|_| "Could not generate a web chat visitor key".to_string())?
        .expose();
    // `create_new`, so two connections racing on the first request cannot end
    // up with two different keys: the loser re-reads the winner's.
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            use std::io::Write;
            file.write_all(&fresh)
                .map_err(|error| format!("Could not write the web chat visitor key: {error}"))?;
            drop(file);
            restrict_file(&path)?;
            Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &fresh))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // The winner may still be mid-`write_all`, so the loser re-reads
            // through the same length check rather than keying an HMAC over
            // however many bytes had landed — an empty key would verify
            // nothing the file's real key ever will.
            let bytes = std::fs::read(&path)
                .map_err(|error| format!("Could not read the web chat visitor key: {error}"))?;
            if bytes.len() != 32 {
                return Err("The web chat visitor key is the wrong length".to_string());
            }
            Ok(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
        }
        Err(error) => Err(format!(
            "Could not create the web chat visitor key: {error}"
        )),
    }
}

fn tag(key: &ring::hmac::Key, account_id: &str, nonce: &[u8]) -> [u8; 16] {
    let mut context = ring::hmac::Context::with_key(key);
    context.update(account_id.as_bytes());
    context.update(b"\0");
    context.update(nonce);
    let mut out = [0u8; 16];
    out.copy_from_slice(&context.sign().as_ref()[..16]);
    out
}

pub(crate) fn mint_visitor(key: &ring::hmac::Key, account_id: &str) -> Result<String, String> {
    // Same reason as the key above: nothing here is ever a zero nonce, and the
    // shape should not be able to suggest one.
    let nonce = ring::rand::generate::<[u8; 16]>(&ring::rand::SystemRandom::new())
        .map_err(|_| "Could not generate a visitor identifier".to_string())?
        .expose();
    let mut raw = [0u8; 32];
    raw[..16].copy_from_slice(&nonce);
    raw[16..].copy_from_slice(&tag(key, account_id, &nonce));
    Ok(URL_SAFE_NO_PAD.encode(raw))
}

/// Whether this browser is presenting something this daemon minted for this
/// account. Constant-time, and shape-checked before the decode so a hostile
/// string cannot do work.
pub(crate) fn visitor_is_ours(key: &ring::hmac::Key, account_id: &str, visitor: &str) -> bool {
    if visitor.len() != 43 {
        return false;
    }
    let Ok(raw) = URL_SAFE_NO_PAD.decode(visitor) else {
        return false;
    };
    if raw.len() != 32 {
        return false;
    }
    ring::constant_time::verify_slices_are_equal(&raw[16..], &tag(key, account_id, &raw[..16]))
        .is_ok()
}

// --- Responses --------------------------------------------------------------

/// Built here rather than through `server::to_http`, so this surface keeps its
/// own narrow policy: the document gets the deny-everything permissions policy
/// (it uses no camera, microphone, location or screen capture), which the
/// controller's `text/html` branch would have widened for nothing. No CORS
/// header at all — the page is same-origin with everything it calls.
fn respond(status: u16, content_type: &'static str, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR))
        .header(CONTENT_TYPE, content_type)
        .header(CACHE_CONTROL, "no-store")
        .header("pragma", "no-cache")
        .header("x-content-type-options", "nosniff")
        .header("x-frame-options", "DENY")
        .header("referrer-policy", "no-referrer")
        .header("permissions-policy", super::web::API_PERMISSIONS_POLICY)
        .header("cross-origin-opener-policy", "same-origin")
        .header("cross-origin-resource-policy", "same-origin")
        .header("content-security-policy", WEBCHAT_CSP)
        .body(Full::new(Bytes::from(body)))
        .expect("static web chat response is valid")
}

fn json(status: u16, value: serde_json::Value) -> Response<Full<Bytes>> {
    respond(
        status,
        "application/json; charset=utf-8",
        value.to_string().into_bytes(),
    )
}

/// One flat refusal shape. A stranger probing `/webchat/...` learns whether a
/// route exists and nothing about which accounts do.
fn refuse(status: u16, code: &str) -> Response<Full<Bytes>> {
    json(status, serde_json::json!({ "error": code }))
}

// --- The handler ------------------------------------------------------------

/// Whether a connection from this peer may reach this account's page at all.
///
/// `None` is the in-process caller — a test, or a served connection with no
/// socket behind it — and is treated as loopback. Anything else has to be
/// loopback, or the account has to have said `public`.
fn peer_admitted(config: &serde_json::Value, peer: Option<SocketAddr>) -> bool {
    if config.get("public").and_then(serde_json::Value::as_bool) == Some(true) {
        return true;
    }
    peer.is_none_or(|peer| peer.ip().is_loopback())
}

/// The account this route names, if it is an enabled web chat account this
/// peer may reach. A peer that may not is not told the difference: it gets the
/// same `404` an unknown account gets.
fn open_account(
    paths: &DaemonPaths,
    account_id: &str,
    peer: Option<SocketAddr>,
) -> Option<DaemonStore> {
    let store = DaemonStore::open(paths).ok()?;
    let account = match store.channel_account(account_id) {
        Ok(Some(account)) if account.enabled && account.kind == ChannelKind::WebChat => account,
        _ => return None,
    };
    peer_admitted(&account.non_secret_config, peer).then_some(store)
}

/// Serve one web chat request. `visitor_header` is the value of
/// [`VISITOR_HEADER`] and `content_type` the request's declared type; those
/// two are the **only** things taken from the request's headers, nothing else
/// about them is trusted, and none of them reach the adapter.
///
/// `content_type` is not authentication — it is the one header a cross-site
/// form cannot set. Requiring `application/json` on the route that writes
/// means a page on another origin cannot post a well-formed body at all,
/// rather than relying on it being unable to read the visitor id back.
pub(crate) async fn handle(
    paths: &DaemonPaths,
    route: Route,
    peer: Option<SocketAddr>,
    visitor_header: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
    now_ms: i64,
) -> Response<Full<Bytes>> {
    // Before anything opens the store: every one of these routes is reachable
    // without a credential, so the first gate has to be one that costs this
    // daemon nothing.
    if let Some(account_id) = route.account_id() {
        if !limiter().allow_up_to(&format!("account:{account_id}"), now_ms, MAX_PER_WINDOW_ACCOUNT)
        {
            return rate_limited();
        }
    }
    match route {
        Route::Asset("js") => respond(
            200,
            "text/javascript; charset=utf-8",
            PAGE_JS.as_bytes().to_vec(),
        ),
        Route::Asset(_) => respond(200, "text/css; charset=utf-8", PAGE_CSS.as_bytes().to_vec()),
        Route::Page(account_id) => {
            if open_account(paths, &account_id, peer).is_none() {
                return refuse(404, "not_found");
            }
            respond(
                200,
                "text/html; charset=utf-8",
                PAGE_HTML.as_bytes().to_vec(),
            )
        }
        Route::Session(account_id) => {
            if open_account(paths, &account_id, peer).is_none() {
                return refuse(404, "not_found");
            }
            if !limiter().allow(&format!("session:{account_id}"), now_ms) {
                return rate_limited();
            }
            let Ok(key) = visitor_key(paths) else {
                return refuse(500, "state_unavailable");
            };
            match mint_visitor(&key, &account_id) {
                Ok(visitor_id) => json(200, serde_json::json!({ "visitor_id": visitor_id })),
                Err(_) => refuse(500, "state_unavailable"),
            }
        }
        Route::Post(account_id) => {
            if !content_type.is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
            }) {
                return refuse(415, "unsupported_media_type");
            }
            let Some(mut store) = open_account(paths, &account_id, peer) else {
                return refuse(404, "not_found");
            };
            let Ok(key) = visitor_key(paths) else {
                return refuse(500, "state_unavailable");
            };
            // Read the identifier out of the body it is already part of, and
            // refuse before any rate-limit row is keyed on something a stranger
            // chose.
            let Some(visitor) = body_visitor(&body) else {
                return refuse(400, "bad_request");
            };
            if !visitor_is_ours(&key, &account_id, &visitor) {
                return refuse(401, "unknown_visitor");
            }
            if !limiter().allow(&visitor_conversation_id(&account_id, &visitor), now_ms) {
                return rate_limited();
            }
            let Ok(adapter) = adapter_for(&account_id) else {
                return refuse(500, "state_unavailable");
            };
            // The same acceptance the public listener performs, with NO client
            // headers: this surface authenticates nothing from them, and a
            // forwarded one could otherwise claim to be another visitor.
            let outcome = accept_webhook_delivery(
                &mut store,
                &adapter,
                &WebhookDelivery {
                    headers: &[],
                    body: &body,
                    public_base_url: None,
                    now_ms,
                },
            );
            match outcome {
                DeliveryOutcome::Accepted { .. } | DeliveryOutcome::Nothing { .. } => {
                    json(202, serde_json::json!({ "ok": true }))
                }
                DeliveryOutcome::Rejected => refuse(400, "bad_request"),
                DeliveryOutcome::NotAccepted => refuse(503, "not_accepted"),
            }
        }
        Route::Fetch(account_id) => {
            let Some(store) = open_account(paths, &account_id, peer) else {
                return refuse(404, "not_found");
            };
            let Ok(key) = visitor_key(paths) else {
                return refuse(500, "state_unavailable");
            };
            let Some(visitor) = visitor_header else {
                return refuse(400, "bad_request");
            };
            if !visitor_is_ours(&key, &account_id, &visitor) {
                return refuse(401, "unknown_visitor");
            }
            let conversation_id = visitor_conversation_id(&account_id, &visitor);
            if !limiter().allow(&conversation_id, now_ms) {
                return rate_limited();
            }
            match store.messages_in_conversation(
                &account_id,
                &conversation_id,
                None,
                TRANSCRIPT_LIMIT,
            ) {
                Ok(messages) => {
                    let rows: Vec<serde_json::Value> = messages
                        .into_iter()
                        .map(|message| {
                            serde_json::json!({
                                "outbound": message.outbound,
                                "author": message.author,
                                "text": message.text,
                                "at_ms": message.at_ms,
                            })
                        })
                        .collect();
                    json(200, serde_json::json!({ "messages": rows }))
                }
                Err(_) => refuse(500, "state_unavailable"),
            }
        }
    }
}

fn rate_limited() -> Response<Full<Bytes>> {
    let mut response = refuse(429, "rate_limited");
    response
        .headers_mut()
        .insert("retry-after", hyper::header::HeaderValue::from_static("60"));
    response
}

/// The visitor identifier a posted body claims, without trusting anything else
/// in it — the adapter is what validates and normalizes the message.
fn body_visitor(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()?
        .get("visitor_id")?
        .as_str()
        .map(str::to_string)
}

/// This account's adapter. It holds no credential and reads no configuration,
/// so it is built from the id alone rather than through the keychain path a
/// provider account needs.
fn adapter_for(account_id: &str) -> Result<WebChatAdapter, String> {
    let record = crate::daemon::channel_store::ChannelAccountRecord {
        account_id: account_id.to_string(),
        kind: ChannelKind::WebChat,
        label: String::new(),
        enabled: true,
        non_secret_config: serde_json::json!({}),
        credential_ref: None,
        access_policy: Default::default(),
        health: little_monkey_lib::channels::types::ChannelHealth::error(0, "not probed"),
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    WebChatAdapter::new(&AdapterConfig {
        account: &record,
        secret: String::new(),
    })
}

/// The page's own URL, for the adapter's health detail and for `channels list`.
pub(crate) fn page_url(paths: &DaemonPaths, account_id: &str) -> Result<String, String> {
    let config = super::server::load_host_config(paths)?.ok_or_else(|| {
        "No remote host is configured, so there is no listener to serve the chat page on"
            .to_string()
    })?;
    if !config.enabled {
        return Err("The remote host is disabled, so nothing serves the chat page".to_string());
    }
    Ok(format!(
        "{}/webchat/{account_id}",
        config.advertise_url.trim_end_matches('/')
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_restart_tests::temp_daemon_paths;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
    use little_monkey_lib::channels::types::ChannelHealth;

    const NOW: i64 = 1_700_000_000_000;

    fn key() -> ring::hmac::Key {
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &[7u8; 32])
    }

    fn world(kind: ChannelKind, enabled: bool) -> (DaemonPaths, String) {
        let paths = temp_daemon_paths();
        let account_id = format!("chan-{}", uuid::Uuid::new_v4().simple());
        let mut store = DaemonStore::open(&paths).expect("open store");
        store
            .upsert_channel_account(&ChannelAccountRecord {
                account_id: account_id.clone(),
                kind,
                label: "Web chat".into(),
                enabled,
                non_secret_config: serde_json::json!({}),
                credential_ref: None,
                access_policy: ChannelAccessPolicy {
                    direct: AccessPolicy::Pairing,
                    group: AccessPolicy::Pairing,
                    group_activation: GroupActivation::Always,
                },
                health: ChannelHealth::connected(NOW, None),
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("account");
        (paths, account_id)
    }

    async fn body_of(response: Response<Full<Bytes>>) -> (u16, serde_json::Value) {
        let status = response.status().as_u16();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[test]
    fn the_page_csp_header_and_meta_are_identical() {
        // The browser enforces the intersection, so a drift between the two is
        // a directive that silently blocks.
        let meta = PAGE_HTML
            .split("content-security-policy\" content=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .expect("the page carries a CSP meta tag");
        let normalize = |value: &str| value.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(normalize(meta), normalize(WEBCHAT_CSP));
    }

    #[test]
    fn the_page_has_no_inline_script_style_or_inner_html_and_carries_no_secret() {
        assert!(!PAGE_HTML.contains("<script>"));
        assert!(!PAGE_HTML.contains(" style=\""));
        assert!(!PAGE_JS.contains("innerHTML"));
        for asset in [PAGE_HTML, PAGE_JS, PAGE_CSS] {
            let lowered = asset.to_ascii_lowercase();
            assert!(!lowered.contains("visitor_id\":\""));
            assert!(!lowered.contains("device_secret"));
            assert!(!lowered.contains("pairing_token"));
        }
    }

    #[test]
    fn only_the_five_routes_are_routes() {
        assert_eq!(
            target("GET", "/webchat/acc-1"),
            Some(Route::Page("acc-1".into()))
        );
        assert_eq!(
            target("POST", "/webchat/acc-1/session"),
            Some(Route::Session("acc-1".into()))
        );
        assert_eq!(
            target("POST", "/webchat/acc-1/messages"),
            Some(Route::Post("acc-1".into()))
        );
        assert_eq!(
            target("GET", "/webchat/acc-1/messages?ignored=1"),
            Some(Route::Fetch("acc-1".into()))
        );
        assert_eq!(
            target("GET", "/webchat/ui/webchat.js"),
            Some(Route::Asset("js"))
        );
        // Everything else, including the shapes a prober would try.
        for (method, path) in [
            ("GET", "/webchat/"),
            ("GET", "/webchat/acc-1/messages/extra"),
            ("DELETE", "/webchat/acc-1/messages"),
            ("GET", "/webchat/../v1/remote/ui/app.js"),
            ("GET", "/v1/remote/ui/app.js"),
            ("GET", "/"),
        ] {
            assert_eq!(target(method, path), None, "{method} {path}");
        }
    }

    #[test]
    fn a_visitor_identifier_is_minted_here_and_verifies_only_for_its_own_account() {
        let key = key();
        let minted = mint_visitor(&key, "acc-1").expect("mint");
        assert_eq!(minted.len(), 43);
        assert!(visitor_is_ours(&key, "acc-1", &minted));
        // Another account's page cannot be opened with it.
        assert!(!visitor_is_ours(&key, "acc-2", &minted));
        // A client-invented identifier of exactly the right shape is refused,
        // which is the whole reason the tag is there.
        assert!(!visitor_is_ours(&key, "acc-1", &"A".repeat(43)));
        assert!(!visitor_is_ours(&key, "acc-1", "short"));
        // One flipped character is not close enough.
        let mut tampered = minted.clone();
        tampered.replace_range(42..43, if minted.ends_with('A') { "B" } else { "A" });
        assert!(!visitor_is_ours(&key, "acc-1", &tampered));
    }

    /// Every route in these tests is reached the way the page reaches it,
    /// declaring JSON. The one test that does *not* is the one below that
    /// asserts what happens without it.
    async fn serve(
        paths: &DaemonPaths,
        route: Route,
        visitor: Option<String>,
        body: Vec<u8>,
        now_ms: i64,
    ) -> Response<Full<Bytes>> {
        handle(
            paths,
            route,
            None,
            visitor,
            Some("application/json; charset=utf-8".to_string()),
            body,
            now_ms,
        )
        .await
    }

    /// These three routes are unauthenticated, so widening the listener for
    /// the signed device plane must not widen them too. A peer that is not
    /// loopback is answered exactly as an unknown account is, until the
    /// account itself says `public`.
    #[tokio::test]
    async fn a_peer_that_is_not_loopback_is_refused_until_the_account_says_public() {
        let (paths, account_id) = world(ChannelKind::WebChat, true);
        let remote: SocketAddr = "203.0.113.7:52000".parse().expect("peer");
        let local: SocketAddr = "127.0.0.1:52000".parse().expect("peer");
        let page = |peer| {
            handle(
                &paths,
                Route::Page(account_id.clone()),
                Some(peer),
                None,
                None,
                Vec::new(),
                NOW,
            )
        };
        assert_eq!(page(remote).await.status().as_u16(), 404);
        assert_eq!(page(local).await.status().as_u16(), 200);

        let mut store = DaemonStore::open(&paths).expect("open store");
        let mut account = store
            .channel_account(&account_id)
            .expect("read")
            .expect("account");
        account.non_secret_config = serde_json::json!({ "public": true });
        store.upsert_channel_account(&account).expect("update");
        drop(store);
        assert_eq!(page(remote).await.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn a_body_that_does_not_declare_json_writes_nothing() {
        // A cross-site form can post `text/plain` with a well-formed body and
        // cannot set `content-type`, so the route that writes insists on it.
        let (paths, account_id) = world(ChannelKind::WebChat, true);
        let key = visitor_key(&paths).expect("key");
        let minted = mint_visitor(&key, &account_id).expect("mint");
        let body = serde_json::json!({ "visitor_id": minted, "text": "hello" })
            .to_string()
            .into_bytes();
        for declared in [None, Some("text/plain".to_string())] {
            let response = handle(
                &paths,
                Route::Post(account_id.clone()),
                None,
                None,
                declared,
                body.clone(),
                NOW,
            )
            .await;
            assert_eq!(response.status().as_u16(), 415);
        }
        let store = DaemonStore::open(&paths).expect("open store");
        assert!(store
            .recent_channel_events(&account_id, 10)
            .expect("events")
            .is_empty());
    }

    #[tokio::test]
    async fn a_non_webchat_account_and_a_disabled_one_serve_no_page() {
        for (kind, enabled) in [(ChannelKind::Irc, true), (ChannelKind::WebChat, false)] {
            let (paths, account_id) = world(kind, enabled);
            let response = serve(
                &paths,
                Route::Page(account_id.clone()),
                None,
                Vec::new(),
                NOW,
            )
            .await;
            assert_eq!(response.status().as_u16(), 404);
            let response = serve(&paths, Route::Session(account_id), None, Vec::new(), NOW).await;
            assert_eq!(response.status().as_u16(), 404);
        }
    }

    #[tokio::test]
    async fn a_client_invented_visitor_posts_nothing_and_reads_nothing() {
        let (paths, account_id) = world(ChannelKind::WebChat, true);
        let invented = "A".repeat(43);
        let body = serde_json::json!({ "visitor_id": invented, "text": "hello" })
            .to_string()
            .into_bytes();
        let (status, _) =
            body_of(serve(&paths, Route::Post(account_id.clone()), None, body, NOW).await).await;
        assert_eq!(status, 401);
        let (status, _) = body_of(
            serve(
                &paths,
                Route::Fetch(account_id.clone()),
                Some(invented),
                Vec::new(),
                NOW,
            )
            .await,
        )
        .await;
        assert_eq!(status, 401);
        // And nothing durable was written for it.
        let store = DaemonStore::open(&paths).expect("open store");
        assert!(store
            .recent_channel_events(&account_id, 10)
            .expect("events")
            .is_empty());
    }

    #[tokio::test]
    async fn a_flood_is_refused_with_a_retry_after() {
        let (paths, account_id) = world(ChannelKind::WebChat, true);
        let mut last = 200;
        for _ in 0..(MAX_PER_WINDOW + 2) {
            last = serve(
                &paths,
                Route::Session(account_id.clone()),
                None,
                Vec::new(),
                NOW,
            )
            .await
            .status()
            .as_u16();
        }
        assert_eq!(last, 429);
        // The next window opens again.
        let response = serve(
            &paths,
            Route::Session(account_id),
            None,
            Vec::new(),
            NOW + WINDOW_MS,
        )
        .await;
        assert_eq!(response.status().as_u16(), 200);
        assert!(response.headers().get("retry-after").is_none());
    }

    /// The one that matters: a first message becomes a durable event through
    /// the same acceptance the public listener uses, the ordinary pairing
    /// policy answers it with a code on this account's own outbox, and the
    /// page's next poll reads that code back.
    #[tokio::test]
    async fn a_first_message_earns_a_pairing_code_the_page_can_read() {
        use crate::daemon::channel_restart_tests::FakeQueue;
        use crate::daemon::channel_worker::process_pending_channel_ingress;

        let (paths, account_id) = world(ChannelKind::WebChat, true);
        let key = visitor_key(&paths).expect("key");
        let minted = mint_visitor(&key, &account_id).expect("mint");

        let body = serde_json::json!({ "visitor_id": minted, "text": "hello there" })
            .to_string()
            .into_bytes();
        let (status, _) =
            body_of(serve(&paths, Route::Post(account_id.clone()), None, body, NOW).await).await;
        assert_eq!(status, 202);

        let mut store = DaemonStore::open(&paths).expect("open store");
        let queue = FakeQueue::default();
        let report = process_pending_channel_ingress(
            &mut store,
            &queue,
            &std::collections::BTreeMap::new(),
            &crate::daemon::channel_adapter::DaemonBlobs,
            NOW,
        )
        .await
        .expect("one pass");
        // Challenged, not queued: an unknown visitor may not cause a run.
        assert_eq!(report.queued, 0);
        assert_eq!(report.settled, 1);
        assert!(queue
            .submitted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_empty());

        let (status, payload) = body_of(
            serve(
                &paths,
                Route::Fetch(account_id.clone()),
                Some(minted.clone()),
                Vec::new(),
                NOW,
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        let messages = payload["messages"].as_array().expect("messages");
        let reply = messages
            .iter()
            .find(|message| message["outbound"] == serde_json::json!(true))
            .expect("the pairing challenge is queued on this account's outbox");
        assert!(
            reply["text"].as_str().unwrap_or_default().contains("code"),
            "the visitor is given a pairing code: {reply}"
        );

        // And a second visitor of the same account reads none of it.
        let other = mint_visitor(&key, &account_id).expect("mint");
        let (status, payload) = body_of(
            serve(
                &paths,
                Route::Fetch(account_id),
                Some(other),
                Vec::new(),
                NOW,
            )
            .await,
        )
        .await;
        assert_eq!(status, 200);
        assert!(payload["messages"].as_array().expect("messages").is_empty());
    }
}

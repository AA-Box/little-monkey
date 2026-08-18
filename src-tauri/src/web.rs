//! Web-research agent tools: `web_fetch` (phase 1) and `web_search` (phase 2,
//! this module's DuckDuckGo-only slice — Brave/SearXNG are phase 3).
//!
//! Structured exactly like `checkpoints.rs`: an AppHandle-free, directly
//! testable core (`validate_fetch_url`, `fetch_impl`) plus a thin
//! `#[tauri::command]` wrapper (`tool_web_fetch`) that adds the permission
//! gate and Stop-button cancellation. The AppHandle-free split matters for
//! the same reason it does there — `monkey-cli` (phase 4) reuses `fetch_impl`
//! directly instead of duplicating the fetch pipeline.
//!
//! SSRF GUARD. [`validate_fetch_url`] plus [`SsrfGuardedResolver`] together
//! are the actual security boundary of this feature: `llama-server` (:8090)
//! and Ollama (:11434) both listen on loopback, so a page or prompt that
//! talks the agent into fetching `http://127.0.0.1:8090/...` (or any other
//! private-range, link-local, or unspecified (`0.0.0.0`/`::`) target) could
//! reach a service on the user's own machine that was never meant to be
//! internet-reachable. `validate_fetch_url` is called once on the request's
//! initial URL and again, from inside the custom `reqwest::redirect::Policy`,
//! on every redirect hop — a server can otherwise pass the initial check
//! with a public URL and then 302 the client to a private one; that policy
//! also caps the chain at `MAX_REDIRECT_HOPS`, since `Policy::custom` doesn't
//! get `reqwest`'s default loop-cap for free. For a hostname target (as
//! opposed to a literal IP in the URL), `validate_fetch_url`'s own DNS
//! resolution (via the blocking `std::net::ToSocketAddrs` — `redirect::Policy::custom`
//! takes a *synchronous* closure, so there's no way to `.await` inside it) is
//! only ever a fast-reject pre-check, not the actual security boundary for
//! the connection: `fetch_impl` installs [`SsrfGuardedResolver`] as the
//! `reqwest::Client`'s DNS resolver. It delegates to K5's per-run pinned
//! resolver, then filters the pinned answers through the same blocklist, with no
//! separate later lookup for a DNS-rebinding attacker (TTL=0 / a rebinding
//! service answering the check-time and connect-time queries differently) to
//! race against.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use url::Url;

use crate::egress::{EgressDenial, EgressRule};
use crate::executable_extensions::{CapabilityKind, ExtensionManager};
use crate::profiles::ProfileScopedPaths;
use crate::{checkpoints, permissions, AppState};

/// Total request timeout (connect through full body read) for `tool_web_fetch`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard cap on how many bytes of the response body are read, regardless of
/// `Content-Length` (which may be absent or lie) — protects both memory and
/// the model's context window from an enormous or unbounded response.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;

/// Default char-window size when the model doesn't pass `max_chars` AND
/// `web_settings.json` doesn't override it — mirrors the 20k-char cap
/// precedent in `src/lib/mentions.ts`'s `truncateMentionContent`. Phase 1/2
/// hardcoded this; phase 3 makes it `WebSettings::fetch_max_chars`'s serde
/// default (see below) while keeping the same fallback value.
const DEFAULT_MAX_CHARS: usize = 20_000;

const USER_AGENT: &str = "LittleMonkey/0.1";

/// Keychain service name for the Brave API key — same string `providers.rs`
/// and `mcp.rs` use for their own secrets; keychain entries are disambiguated
/// by *account* (see [`BRAVE_KEYCHAIN_ACCOUNT`]), not service, so this is
/// fine to duplicate rather than export from `providers.rs`.
/// Profile-scoped (K23). The default profile keeps this exact service name, so
/// every credential stored before profiles existed still resolves; any other
/// profile's secrets live under `<service>.profile.<id>`, which is a different
/// keychain item that this profile's code never names.
static KEYCHAIN_SERVICE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.app"));

/// Keychain account name the Brave API key is stored under. Namespaced with
/// a `web:` prefix — same reasoning as `mcp.rs::keychain_account`'s `mcp:`
/// prefix (see its doc comment and `keychain_account_namespaces_by_mcp_prefix`
/// test): `providers.rs` stores custom-LLM-provider keys under the *bare*
/// slugified provider id (`providers::slugify`, which only ever produces
/// lowercase alphanumerics and dashes — never a colon), in the very same
/// keychain *service* this module uses. A custom provider labeled e.g.
/// "Search Brave" slugifies to exactly `search-brave`, so an unprefixed
/// account here would silently collide with — and let one feature overwrite
/// or read — the other's secret. The `web:` prefix makes that collision
/// structurally impossible.
const BRAVE_KEYCHAIN_ACCOUNT: &str = "web:search-brave";

/// Filename for the persisted web-tools settings under the app data
/// directory — same file-per-feature pattern as `providers.json`/
/// `mcp_servers.json`.
const SETTINGS_FILE: &str = "web_settings.json";

/// Which search backend `web_search` dispatches to — see [`search_impl`].
/// `#[serde(rename_all = "snake_case")]` on a single-uppercase-run identifier
/// like `Duckduckgo`/`Searxng` just lowercases it (there's no internal
/// camelCase boundary to split on), so this serializes to exactly the design
/// doc's `"duckduckgo"|"brave"|"searxng"` strings with no explicit renames —
/// verified by [`tests::search_provider_serializes_to_the_expected_strings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SearchProvider {
    #[default]
    Duckduckgo,
    Brave,
    Searxng,
    ExecutableExtension,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FetchProvider {
    #[default]
    Builtin,
    ExecutableExtension,
}

/// Serde default for [`WebSettings::fetch_max_chars`] — same fallback value
/// as [`DEFAULT_MAX_CHARS`], just exposed as a `fn` since `#[serde(default)]`
/// needs one rather than a bare const for a non-zero primitive default.
fn default_fetch_max_chars() -> usize {
    DEFAULT_MAX_CHARS
}

/// Persisted at `<app_data>/web_settings.json`, mirrored to the frontend by
/// `src/store/webStore.ts`. Plain snake_case field names, no `serde(rename)`
/// — same hand-editable-file convention as `providers.json`/`mcp_servers.json`.
/// Notably absent: the Brave API key itself, which lives only in the OS
/// keychain (see [`has_brave_key`]/[`read_brave_key`]) and is never part of
/// this struct or this file.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WebSettings {
    #[serde(default)]
    pub search_provider: SearchProvider,
    #[serde(default)]
    pub search_extension_id: Option<String>,
    #[serde(default)]
    pub search_extension_capability_id: Option<String>,
    /// Required when `search_provider == Searxng`; ignored otherwise. `None`
    /// (not an empty string) is the "unset" state — see
    /// `normalize_and_validate_settings`, which turns a blank input back into
    /// `None` before this is ever persisted.
    #[serde(default)]
    pub searxng_base_url: Option<String>,
    /// Whether `web_fetch`/`web_search` may target a loopback/private/
    /// link-local host. Defaults to `false` — see the module doc's SSRF
    /// guard rationale. Real (settings-driven) as of phase 3; phases 1-2
    /// hardcoded this to `false` via a module constant.
    #[serde(default)]
    pub allow_local_network: bool,
    /// Char-window size `tool_web_fetch` uses when the model doesn't pass its
    /// own `max_chars`. Real (settings-driven) as of phase 3.
    #[serde(default = "default_fetch_max_chars")]
    pub fetch_max_chars: usize,
    #[serde(default)]
    pub fetch_provider: FetchProvider,
    #[serde(default)]
    pub fetch_extension_id: Option<String>,
    #[serde(default)]
    pub fetch_extension_capability_id: Option<String>,
}

impl Default for WebSettings {
    fn default() -> Self {
        Self {
            search_provider: SearchProvider::default(),
            search_extension_id: None,
            search_extension_capability_id: None,
            searxng_base_url: None,
            allow_local_network: false,
            fetch_max_chars: DEFAULT_MAX_CHARS,
            fetch_provider: FetchProvider::default(),
            fetch_extension_id: None,
            fetch_extension_capability_id: None,
        }
    }
}

/// Resolves (and creates, if missing) `<app_data_dir>/web_settings.json`'s
/// path — same shape as `providers.rs::providers_file_path`/
/// `mcp.rs::config_file_path`.
fn settings_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base).map_err(|e| {
            format!(
                "Failed to create app data directory {}: {e}",
                base.display()
            )
        })?;
    }
    Ok(base.join(SETTINGS_FILE))
}

/// Core load logic, parameterized by path for testability — a missing file
/// (nothing configured yet, the common case) is simply [`WebSettings::default`],
/// never an error, same stance as `mcp.rs::load_config_impl`. `pub` (rather
/// than private, like most other `*_impl` load fns in this module) because
/// monkey-cli's `web_cli.rs` (phase 4) calls this directly with its own
/// APP_IDENTIFIER-resolved path — same reuse-the-lib-fn-directly shape as
/// `checkpoints_cli.rs` calling `checkpoints::begin_impl`.
pub fn load_settings_impl(path: &Path) -> Result<WebSettings, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("Corrupt web_settings.json: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(WebSettings::default()),
        Err(e) => Err(format!("Failed to read web_settings.json: {e}")),
    }
}

/// The extension-backed web selections currently persisted, for the Security
/// Doctor's orphaned-provider check. Total: unconfigured or unreadable
/// settings mean no selections, which is not a finding.
#[derive(Debug, Default, Clone)]
pub struct PersistedWebSelections {
    pub search: Option<(String, String)>,
    pub fetch: Option<(String, String)>,
}

pub fn persisted_extension_selections(app_data: &Path) -> PersistedWebSelections {
    let Ok(settings) = load_settings_impl(&app_data.join(SETTINGS_FILE)) else {
        return PersistedWebSelections::default();
    };
    PersistedWebSelections {
        search: (settings.search_provider == SearchProvider::ExecutableExtension)
            .then(|| {
                Some((
                    settings.search_extension_id.clone()?,
                    settings.search_extension_capability_id.clone()?,
                ))
            })
            .flatten(),
        fetch: (settings.fetch_provider == FetchProvider::ExecutableExtension)
            .then(|| {
                Some((
                    settings.fetch_extension_id.clone()?,
                    settings.fetch_extension_capability_id.clone()?,
                ))
            })
            .flatten(),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `sessions.rs`'s `save_to` / `mcp.rs`'s `save_config_impl`.
fn save_settings_impl(path: &Path, settings: &WebSettings) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(settings)
        .map_err(|e| format!("Failed to serialize web_settings.json: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload)
        .map_err(|e| format!("Failed to write web_settings.json: {e}"))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Failed to finalize web_settings.json: {e}"))?;
    Ok(())
}

/// Rejects anything that isn't a plain `http(s)://...` base URL and
/// normalizes away a trailing slash — same validation `providers.rs::validate_base_url`
/// applies to custom-provider base URLs, duplicated here (rather than made
/// `pub` there and imported) since that function is private to `providers.rs`
/// and this is a small, self-contained rule.
fn normalize_base_url(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if !(trimmed.starts_with("http://") || trimmed.starts_with("https://")) {
        return Err(format!(
            "Invalid SearXNG base URL '{raw}': must start with http:// or https://"
        ));
    }
    if trimmed.len() <= "https://".len() {
        return Err(format!("Invalid SearXNG base URL '{raw}'"));
    }
    Ok(trimmed.to_string())
}

fn normalize_extension_capability_id(
    label: &str,
    value: Option<String>,
) -> Result<Option<String>, String> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 160
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(format!("{label} must be a bounded ASCII identifier"));
    }
    Ok(Some(value.to_string()))
}

/// Pure validation/normalization core behind [`web_set_settings`]: blanks out
/// a blank/whitespace-only `searxng_base_url` back to `None` (rather than
/// persisting an empty string), otherwise validates+normalizes it, and
/// rejects a zero `fetch_max_chars` (an unbounded-above value is left to the
/// user's judgement — there's no analogous upper nonsense value to guard).
/// Split out from the `#[tauri::command]` wrapper so it's directly testable
/// without an `AppHandle`, same `*_impl` split as everywhere else in this
/// module.
fn normalize_and_validate_settings(mut settings: WebSettings) -> Result<WebSettings, String> {
    if settings.fetch_max_chars == 0 {
        return Err("fetch_max_chars must be greater than 0".to_string());
    }
    if let Some(raw) = settings.searxng_base_url.take() {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            settings.searxng_base_url = Some(normalize_base_url(trimmed)?);
        }
    }
    settings.search_extension_id =
        normalize_extension_capability_id("search_extension_id", settings.search_extension_id)?;
    settings.search_extension_capability_id = normalize_extension_capability_id(
        "search_extension_capability_id",
        settings.search_extension_capability_id,
    )?;
    settings.fetch_extension_id =
        normalize_extension_capability_id("fetch_extension_id", settings.fetch_extension_id)?;
    settings.fetch_extension_capability_id = normalize_extension_capability_id(
        "fetch_extension_capability_id",
        settings.fetch_extension_capability_id,
    )?;
    if settings.search_provider == SearchProvider::ExecutableExtension
        && (settings.search_extension_id.is_none()
            || settings.search_extension_capability_id.is_none())
    {
        return Err(
            "search_extension_id and search_extension_capability_id are required for executable-extension search"
                .to_string(),
        );
    }
    if settings.fetch_provider == FetchProvider::ExecutableExtension
        && (settings.fetch_extension_id.is_none()
            || settings.fetch_extension_capability_id.is_none())
    {
        return Err(
            "fetch_extension_id and fetch_extension_capability_id are required for executable-extension fetch"
                .to_string(),
        );
    }
    Ok(settings)
}

/// Whether a Brave API key is currently saved — always a live keychain
/// probe, never a persisted flag, mirroring `providers::has_key`'s stance
/// exactly (never drifts from reality).
pub fn has_brave_key() -> bool {
    keyring::Entry::new(&KEYCHAIN_SERVICE, BRAVE_KEYCHAIN_ACCOUNT)
        .and_then(|e| e.get_password())
        .is_ok()
}

/// Reads the saved Brave API key, for `search_impl`'s Brave branch (via
/// `tool_web_search`) and monkey-cli's shared `web::read_brave_key()` (phase 4).
pub fn read_brave_key() -> Result<String, String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, BRAVE_KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    entry.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => {
            "No Brave API key saved — add one in Settings > Web, or switch the search provider."
                .to_string()
        }
        other => format!("Failed to read saved Brave API key: {other}"),
    })
}

/// Core remove logic behind [`web_remove_brave_key`] — a missing entry is a
/// no-op success, same stance as `providers::remove_key_impl`.
fn remove_brave_key_impl() -> Result<(), String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, BRAVE_KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove saved key: {e}")),
    }
}

/// Returns the persisted web-tools settings (never the Brave key itself —
/// see [`web_has_brave_key`] for that live probe).
#[tauri::command]
pub fn web_get_settings(app: tauri::AppHandle) -> Result<WebSettings, String> {
    load_settings_impl(&settings_file_path(&app)?)
}

/// Persists the web-tools settings after [`normalize_and_validate_settings`].
/// Does not touch the Brave key — that's [`web_set_brave_key`]/
/// [`web_remove_brave_key`]'s job, kept as separate commands since one lives
/// in the keychain and the other in this JSON file.
///
/// Serialized against every other call via `state.web_settings_lock` (see
/// its doc comment on `AppState`) — this is a synchronous command, so Tauri
/// can dispatch two concurrent calls (e.g. a rapid double-click of Save, or
/// two Settings > Web controls firing close together) onto genuinely
/// concurrent OS threads; without the lock, both could `std::fs::write` the
/// same deterministic `web_settings.json.tmp` path at once and leave a
/// torn/interleaved file for whichever `rename` lands last to publish.
#[tauri::command]
pub fn web_set_settings(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    settings: WebSettings,
) -> Result<(), String> {
    let settings = normalize_and_validate_settings(settings)?;
    let _guard = state
        .web_settings_lock
        .lock()
        .map_err(|_| "Web settings lock poisoned".to_string())?;
    save_settings_impl(&settings_file_path(&app)?, &settings)
}

/// Live keychain probe for the Settings "Web" tab's Brave key field — mirrors
/// `ProviderCard`'s `has_key` pattern (a boolean the UI polls/refreshes,
/// never the secret itself).
#[tauri::command]
pub fn web_has_brave_key() -> bool {
    has_brave_key()
}

/// Validates `api_key` with a live 1-result Brave query *before* touching the
/// keychain — a bad key is never persisted — exactly mirroring
/// `providers::providers_set_key`'s validate-before-store shape (that one
/// fetches the model list; this one runs a throwaway search).
#[tauri::command]
pub async fn web_set_brave_key(api_key: String) -> Result<(), String> {
    let api_key = api_key.trim().to_string();
    if api_key.is_empty() {
        return Err("API key is required".to_string());
    }

    brave_search(&api_key, "test", 1).await?;

    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, BRAVE_KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    entry
        .set_password(&api_key)
        .map_err(|e| format!("Failed to save key to keychain: {e}"))
}

/// Removes the saved Brave API key from the keychain.
#[tauri::command]
pub fn web_remove_brave_key() -> Result<(), String> {
    remove_brave_key_impl()
}

/// Which rule, if any, refuses `ip` — the ranges the design doc calls out:
/// loopback, RFC1918 private, link-local, and unspecified (`0.0.0.0`).
/// `Ipv4Addr::is_private` (the std method) already covers exactly
/// 10.0.0.0/8, 172.16.0.0/12, and 192.168.0.0/16, so it's used as-is rather
/// than hand-rolling the same three CIDR checks.
///
/// # Why this reports a rule instead of a bool
///
/// It used to return `bool`, and all four classes below shared one message:
/// "target host is a local/private address". Ten distinct predicates, counting
/// the v6 side, collapsed into one sentence — so a test could only assert that
/// sentence's substring, and seven of them did. Naming the rule is what lets a
/// test say *loopback* was refused rather than *something* was, and lets a log
/// reader tell an RFC1918 target from a `0.0.0.0` one.
///
/// `is_unspecified` (`0.0.0.0`)
/// is checked explicitly and separately from the other three: it isn't
/// loopback, private, *or* link-local by any of those predicates' own
/// definitions, yet the OS routes an outbound connection to `0.0.0.0` to
/// `127.0.0.1` (verified empirically on macOS) — i.e. it's a real path to a
/// loopback-bound service like `llama-server`/Ollama, not a dead address,
/// so it must be blocked exactly like a literal `127.0.0.1`.
fn blocked_reason_ipv4(ip: &Ipv4Addr) -> Option<EgressRule> {
    if ip.is_loopback() {
        return Some(EgressRule::Loopback);
    }
    if ip.is_private() {
        return Some(EgressRule::PrivateV4);
    }
    if ip.is_link_local() {
        return Some(EgressRule::LinkLocal);
    }
    if ip.is_unspecified() {
        return Some(EgressRule::Unspecified);
    }
    None
}

/// IPv6 equivalent of [`blocked_reason_ipv4`]. `Ipv6Addr::is_loopback` and
/// `is_unspecified` are stable std, but the unique-local (`fc00::/7`) and
/// link-local (`fe80::/10`) checks are hand-rolled here because the
/// corresponding std predicates (`is_unique_local`, `is_unicast_link_local`)
/// are still gated behind the unstable `ip` feature. An IPv4-mapped address
/// (`::ffff:a.b.c.d`) is unwrapped and re-checked against
/// [`blocked_reason_ipv4`] so a v4-mapped private (or unspecified) address
/// can't slip past a v6-only check.
fn blocked_reason_ipv6(ip: &Ipv6Addr) -> Option<EgressRule> {
    // Before the mapped unwrap, because the deprecated IPv4-*compatible* form
    // (`::a.b.c.d`) is not what `to_ipv4_mapped` matches and fell through every
    // branch below. See `egress::is_ipv4_compatible` for why this is a rejection
    // rather than a second unwrap.
    if crate::egress::is_ipv4_compatible(ip) {
        return Some(EgressRule::Ipv4Compatible);
    }
    // A mapped address reports whichever v4 rule its inner address trips, rather
    // than a rule of its own: `::ffff:127.0.0.1` is loopback, and calling it
    // anything else would hide that from whoever reads the denial.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return blocked_reason_ipv4(&v4);
    }
    // The third spelling of a v4 address, and the one this guard was missing:
    // `64:ff9b::7f00:1` *is* `127.0.0.1` wherever a NAT64/CLAT path exists. Delegated
    // to this file's own v4 rule rather than refused outright, because the prefix is
    // live and legitimate — `64:ff9b::` plus a public address is how a v6-only network
    // reaches a v4-only server. See `egress::nat64_embedded_ipv4`.
    if let Some(v4) = crate::egress::nat64_embedded_ipv4(ip) {
        return blocked_reason_ipv4(&v4);
    }
    if ip.is_loopback() {
        return Some(EgressRule::Loopback);
    }
    if ip.is_unspecified() {
        return Some(EgressRule::Unspecified);
    }
    let octets = ip.octets();
    if (octets[0] & 0xfe) == 0xfc {
        return Some(EgressRule::UniqueLocalV6); // fc00::/7
    }
    if octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80 {
        return Some(EgressRule::LinkLocal); // fe80::/10
    }
    None
}

fn blocked_reason_ip(ip: &IpAddr) -> Option<EgressRule> {
    match ip {
        IpAddr::V4(v4) => blocked_reason_ipv4(v4),
        IpAddr::V6(v6) => blocked_reason_ipv6(v6),
    }
}

/// Validates a candidate fetch URL: http(s) only, no embedded credentials,
/// and (unless `allow_local_network`) no loopback/private/link-local target —
/// checked against a literal IP host directly, or against every address a
/// hostname resolves to. Called both on the initial URL and, from inside the
/// custom redirect policy in [`fetch_impl`], on every redirect hop — see the
/// module doc for why re-checking every hop is the actual security boundary.
pub fn validate_fetch_url(url: &Url, allow_local_network: bool) -> Result<(), EgressDenial> {
    let verdict = classify_fetch_url(url, allow_local_network);
    if let Err(denial) = &verdict {
        // Recorded here, at the raise site, rather than at the command boundary —
        // which is the whole reason the sink is reachable without a state handle.
        // By the time this refusal reaches `tool_web_fetch` it is a `String`, and a
        // sink fed from there would be parsing its own rule code back out of a
        // sentence. Fail-soft and never consulted by any decision: see
        // `denial_sink`'s module doc.
        crate::denial_sink::record(GUARD, denial, None);
    }
    verdict
}

/// Names this guard in a denial record, so that two guards disagreeing about the
/// same address class stay distinguishable in the sink.
const GUARD: &str = "web.fetch";

/// Whether a hostname's resolved answers leave anything this app may connect to.
///
/// Its own function purely so the quantifier is testable without the system resolver.
/// The case the change to it is *about* — one public answer and one private one — cannot
/// be produced hermetically through `to_socket_addrs`, and a rule that only the
/// deployment environment can exercise is a rule with no test.
fn classify_resolved_answers(
    domain: &str,
    answers: impl Iterator<Item = IpAddr>,
) -> Result<(), EgressDenial> {
    let mut refused = None;
    let mut survivors = 0usize;
    for ip in answers {
        match blocked_reason_ip(&ip) {
            // The address is named in the detail, not just the class: a hostname with
            // several answers used to be refused by a message that said nothing about
            // which answer tripped it.
            Some(rule) => {
                refused = Some(EgressDenial::about(
                    rule,
                    format!("{domain} resolves to {ip}"),
                ));
            }
            None => survivors += 1,
        }
    }
    if survivors == 0 {
        // `DnsNoAddresses` when the lookup came back empty, which no rule refused;
        // otherwise the rule that accounted for the last answer. The same two-case split
        // the resolver makes, for the same reason: "nothing answered" and "everything
        // that answered is refused" are different facts.
        return Err(refused.unwrap_or_else(|| {
            EgressDenial::about(EgressRule::DnsNoAddresses, domain.to_string())
        }));
    }
    Ok(())
}

/// [`validate_fetch_url`]'s decision, without the recording.
///
/// Split so the guard's logic stays a pure function of its arguments: the tests
/// below drive this one, and a test that recorded into a process-wide sink as a
/// side effect of asserting a verdict would couple every one of them to every
/// other test's expectations.
fn classify_fetch_url(url: &Url, allow_local_network: bool) -> Result<(), EgressDenial> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return Err(EgressDenial::about(
            EgressRule::SchemeNotAllowed,
            format!("only http/https URLs are allowed, not '{}'", url.scheme()),
        ));
    }

    // No detail, deliberately: the URL is what carries the credential, so this is
    // the one refusal that must not quote its target. `EgressRule::redacts_target`
    // says the same thing where every guard can see it.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(EgressDenial::new(EgressRule::EmbeddedCredentials));
    }

    if allow_local_network {
        return Ok(());
    }

    // `Url::host()` (unlike `host_str()`) gives back an already-parsed
    // `Host` enum — a literal IP is a real `IpAddr` here, not a bracketed
    // string (`"[::1]"`) that would need to be stripped before it could be
    // reparsed. Using this instead of string-parsing `host_str()` avoids
    // that whole bracket-handling class of bug for IPv6 literals.
    match url
        .host()
        .ok_or_else(|| EgressDenial::new(EgressRule::HostMissing))?
    {
        url::Host::Ipv4(ip) => {
            if let Some(rule) = blocked_reason_ipv4(&ip) {
                return Err(EgressDenial::about(rule, ip.to_string()));
            }
        }
        url::Host::Ipv6(ip) => {
            if let Some(rule) = blocked_reason_ipv6(&ip) {
                return Err(EgressDenial::about(rule, ip.to_string()));
            }
        }
        url::Host::Domain(domain) => {
            // Resolve and check the addresses the hostname maps to. The port passed to
            // `ToSocketAddrs` is irrelevant to the lookup itself.
            //
            // # Why this refuses only when *no* answer survives
            //
            // This used to refuse if **any** answer was blocked, which disagreed with
            // [`SsrfGuardedResolver`] — the thing that actually enforces — and the
            // disagreement was this gate over-blocking rather than the resolver
            // under-blocking. The resolver prunes blocked answers and hands `reqwest`
            // only the survivors, and since those are exactly what `reqwest` connects
            // to, a pruned private answer is never dialled. So a legitimate host that
            // answers with one public and one private address — ordinary split-horizon
            // or dual-stack DNS — connects safely through the public one, and was being
            // refused outright here for it.
            //
            // Matching the resolver's quantifier keeps this layer without keeping the
            // false refusals. It stays a layer rather than being deleted because it is
            // the only guard for a URL that never reaches a resolver at all: the
            // literal-IP arms above, and any future caller that validates without
            // installing the resolver. Deleting it would make that mistake silent.
            //
            // Note this pre-check's lookup is a *different* lookup from the resolver's,
            // which is why it cannot be the enforcement point — see
            // [`SsrfGuardedResolver`] for the rebinding argument. It is a fast, and now
            // consistent, pre-filter.
            let addrs = (domain, 0u16)
                .to_socket_addrs()
                .map_err(|e| EgressDenial::about(EgressRule::DnsResolutionFailed, e.to_string()))?;
            classify_resolved_answers(domain, addrs.map(|addr| addr.ip()))?;
        }
    }

    Ok(())
}

/// Renders a denial the way this file's callers have always rendered a refusal:
/// named target first, reason second.
///
/// The one exception is the rule that refuses *because* the URL holds a
/// credential, where quoting the target would print the secret into the UI, the
/// model's tool result and the CLI's stdout at once. That exception used to be
/// hand-coded in one branch of [`validate_fetch_url`]; it now comes from
/// [`EgressRule::redacts_target`], so the next guard to be converted inherits it
/// instead of having to remember it.
fn fetch_refusal(url: &Url, denial: &EgressDenial) -> String {
    if denial.rule().redacts_target() {
        format!("Refusing to fetch a URL: {denial}")
    } else {
        format!("Refusing to fetch '{url}': {denial}")
    }
}

const MAX_EXTENSION_RESULT_URL_BYTES: usize = 8 * 1024;
const MAX_EXTENSION_RESULT_TITLE_CHARS: usize = 1_024;
const MAX_EXTENSION_SEARCH_SNIPPET_CHARS: usize = 8 * 1024;
const MAX_EXTENSION_CONTENT_TYPE_BYTES: usize = 512;

#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ExtensionFetchInput<'a> {
    url: &'a str,
    max_chars: usize,
    start_index: usize,
}

#[derive(serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ExtensionSearchInput<'a> {
    query: &'a str,
    count: usize,
}

#[derive(Debug)]
struct WebExtensionCall {
    extension_id: String,
    capability_id: String,
    input_json: String,
    invocation_id: String,
}

struct ExtensionCancellationGuard {
    invocation_id: String,
    armed: bool,
}

/// The desktop Stop path cancels explicitly so it can await runtime cleanup;
/// this guard covers callers such as monkey-cli whose whole tool future is
/// dropped on Ctrl-C.
impl ExtensionCancellationGuard {
    fn new(invocation_id: String) -> Self {
        Self {
            invocation_id,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ExtensionCancellationGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = crate::executable_extensions::cancel(&self.invocation_id);
        }
    }
}

fn deterministic_extension_invocation_id(
    prefix: &str,
    trusted_call_id: &str,
    extension_id: &str,
    capability_id: &str,
    input_json: &str,
) -> String {
    let mut digest = Sha256::new();
    for field in [
        "little-monkey:web-extension:v1",
        prefix,
        trusted_call_id,
        extension_id,
        capability_id,
        input_json,
    ] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    format!("{prefix}-{:x}", digest.finalize())
}

fn extension_fetch_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    url: &str,
    max_chars: Option<usize>,
    start_index: Option<usize>,
) -> Result<Option<WebExtensionCall>, String> {
    if settings.fetch_provider != FetchProvider::ExecutableExtension {
        return Ok(None);
    }
    if trusted_call_id.trim().is_empty() {
        return Err("Executable web-fetch requires a trusted runtime call id".to_string());
    }
    let extension_id = settings
        .fetch_extension_id
        .as_deref()
        .ok_or("Choose a healthy executable web-fetch extension in Settings > Web")?;
    let capability_id = settings
        .fetch_extension_capability_id
        .as_deref()
        .ok_or("Choose a healthy executable web-fetch capability in Settings > Web")?;
    let input_json = serde_json::to_string(&ExtensionFetchInput {
        url,
        max_chars: max_chars.unwrap_or(settings.fetch_max_chars),
        start_index: start_index.unwrap_or(0),
    })
    .map_err(|error| format!("Could not encode executable web-fetch input: {error}"))?;
    Ok(Some(WebExtensionCall {
        extension_id: extension_id.to_string(),
        capability_id: capability_id.to_string(),
        invocation_id: deterministic_extension_invocation_id(
            "web-fetch",
            trusted_call_id,
            extension_id,
            capability_id,
            &input_json,
        ),
        input_json,
    }))
}

fn extension_search_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    query: &str,
    count: Option<usize>,
) -> Result<Option<WebExtensionCall>, String> {
    if settings.search_provider != SearchProvider::ExecutableExtension {
        return Ok(None);
    }
    if trusted_call_id.trim().is_empty() {
        return Err("Executable web-search requires a trusted runtime call id".to_string());
    }
    let extension_id = settings
        .search_extension_id
        .as_deref()
        .ok_or("Choose a healthy executable web-search extension in Settings > Web")?;
    let capability_id = settings
        .search_extension_capability_id
        .as_deref()
        .ok_or("Choose a healthy executable web-search capability in Settings > Web")?;
    let input_json = serde_json::to_string(&ExtensionSearchInput {
        query,
        count: count
            .unwrap_or(DEFAULT_SEARCH_COUNT)
            .clamp(1, DEFAULT_SEARCH_COUNT),
    })
    .map_err(|error| format!("Could not encode executable web-search input: {error}"))?;
    Ok(Some(WebExtensionCall {
        extension_id: extension_id.to_string(),
        capability_id: capability_id.to_string(),
        invocation_id: deterministic_extension_invocation_id(
            "web-search",
            trusted_call_id,
            extension_id,
            capability_id,
            &input_json,
        ),
        input_json,
    }))
}

/// Where web's extension calls look for the installed extension store.
///
/// Production has exactly one answer, the active profile's data directory.
/// Tests need another, because installing a real component into the developer's
/// own profile to prove a search reaches it is not something a test may do.
#[cfg(test)]
static WEB_EXTENSION_APP_DATA: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_web_extension_app_data_for_test(app_data: Option<PathBuf>) {
    *WEB_EXTENSION_APP_DATA.lock().unwrap() = app_data;
}

fn web_extension_app_data() -> Result<PathBuf, String> {
    #[cfg(test)]
    if let Some(app_data) = WEB_EXTENSION_APP_DATA.lock().unwrap().clone() {
        return Ok(app_data);
    }
    crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey app-data directory".to_string())
}

async fn invoke_web_extension(
    kind: CapabilityKind,
    call: WebExtensionCall,
) -> Result<String, String> {
    let app_data = web_extension_app_data()?;
    let mut cancellation = ExtensionCancellationGuard::new(call.invocation_id.clone());
    let result = ExtensionManager::new(app_data)?
        .invoke_owned_active_capability(
            kind,
            &call.extension_id,
            &call.capability_id,
            call.input_json,
            Some(call.invocation_id),
            Vec::new(),
        )
        .await;
    cancellation.disarm();
    result.map(|result| result.output_json)
}

fn parse_extension_result_url(label: &str, value: &str) -> Result<Url, String> {
    if value.len() > MAX_EXTENSION_RESULT_URL_BYTES {
        return Err(format!(
            "Executable extension returned an oversized {label}"
        ));
    }
    let parsed = Url::parse(value)
        .map_err(|error| format!("Executable extension returned an invalid {label}: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(format!(
            "Executable extension returned a non-http(s) or credential-bearing {label}"
        ));
    }
    Ok(parsed)
}

/// Result of a successful `web_fetch`, echoed back to the model as JSON.
/// Deliberately plain snake_case field names (no `serde(rename)`), matching
/// `Fact`'s (`memory.rs`) convention for tool results the model itself reads
/// — it's the same snake_case the model's own tool-call arguments use
/// (`max_chars`, `start_index`), not the camelCase Tauri IPC otherwise uses
/// for frontend-facing payloads like `CheckpointSummary`.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

fn parse_extension_fetch_output(
    output_json: &str,
    requested_url: &str,
    max_chars: usize,
    start_index: usize,
    allow_local_network: bool,
) -> Result<FetchResult, String> {
    if output_json.len() > MAX_BODY_BYTES {
        return Err("Executable web-fetch output exceeds its byte limit".to_string());
    }
    let result: FetchResult = serde_json::from_str(output_json).map_err(|error| {
        format!("Executable web-fetch returned invalid normalized JSON: {error}")
    })?;
    if result.url != requested_url {
        return Err("Executable web-fetch changed the requested URL in its result".to_string());
    }
    parse_extension_result_url("requested URL", &result.url)?;
    let final_url = parse_extension_result_url("final URL", &result.final_url)?;
    crate::egress::check_run_allowlist(&final_url)
        .map_err(|denial| fetch_refusal(&final_url, &denial))?;
    validate_fetch_url(&final_url, allow_local_network)
        .map_err(|denial| fetch_refusal(&final_url, &denial))?;
    if result.title.as_ref().is_some_and(|title| {
        title.trim().is_empty()
            || title.contains('\0')
            || title.chars().count() > MAX_EXTENSION_RESULT_TITLE_CHARS
    }) {
        return Err("Executable web-fetch returned an invalid title".to_string());
    }
    if result.content_type.len() > MAX_EXTENSION_CONTENT_TYPE_BYTES
        || result.content_type.contains(['\r', '\n', '\0'])
    {
        return Err("Executable web-fetch returned an invalid content type".to_string());
    }
    let markdown_chars = result.markdown.chars().count();
    if markdown_chars > max_chars || result.total_chars > MAX_BODY_BYTES {
        return Err(
            "Executable web-fetch returned content outside the requested bounds".to_string(),
        );
    }
    let start = start_index.min(result.total_chars);
    let expected_chars = max_chars.min(result.total_chars.saturating_sub(start));
    let expected_truncated = start.saturating_add(expected_chars) < result.total_chars;
    if markdown_chars != expected_chars || result.truncated != expected_truncated {
        return Err("Executable web-fetch returned inconsistent window metadata".to_string());
    }
    Ok(result)
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

/// Minimum `Article::length` (characters of extracted text content) `dom_smoothie`
/// must produce before its output is trusted over the raw page — below this,
/// a page is more likely something Readability's heuristics simply don't fit
/// (a docs page, a short reference article, a listing page) than a "real"
/// article whose boilerplate was successfully stripped, so falling back to
/// full-page conversion is the safer default. Chosen well below a typical
/// short article's length (a few hundred words) rather than tuned tightly,
/// since a false-fallback (using the full page when readability *would* have
/// worked) only costs some extra boilerplate in the Markdown, while a
/// false-positive (trusting a near-empty extraction) could silently drop the
/// entire page the model asked to read.
const MIN_READABLE_CONTENT_CHARS: usize = 200;

/// Attempts `dom_smoothie::Readability` article extraction on `body` before
/// falling back to the raw page — see the module doc's phase-5 note. Returns
/// `(title, html_to_convert)`: on a successful, non-trivial extraction this is
/// the article's own title and its cleaned inner HTML (nav/ads/footer/sidebar
/// stripped); otherwise it's `extract_title(body)` and `body` itself unchanged,
/// so callers don't need their own fallback branch.
///
/// `document_url` is threaded through as `Readability::new`'s `document_url`
/// so Readability can resolve any relative links/images inside the article
/// content against the actual page URL rather than treating them as
/// document-relative to nothing — passed as `Some` only when `url` parses,
/// since a malformed URL should degrade to "no base" rather than fail the
/// whole extraction.
///
/// Never itself an error: any `Readability::new`/`parse` failure (malformed
/// HTML tendril, no discernible article, etc.) is swallowed and treated the
/// same as "extraction wasn't useful here" — `dispatch_content`'s HTML branch
/// must still succeed via the raw-page fallback so a page Readability doesn't
/// like isn't worse off than before this feature existed.
fn extract_readable_content(body: &str, url: &str) -> (Option<String>, String) {
    let fallback = || (extract_title(body), body.to_string());

    let document_url = Url::parse(url).ok().map(|_| url);
    let mut readability = match dom_smoothie::Readability::new(body, document_url, None) {
        Ok(r) => r,
        Err(_) => return fallback(),
    };
    let article = match readability.parse() {
        Ok(a) => a,
        Err(_) => return fallback(),
    };

    if article.length < MIN_READABLE_CONTENT_CHARS {
        return fallback();
    }

    let title = if article.title.trim().is_empty() {
        extract_title(body)
    } else {
        Some(article.title)
    };
    (title, article.content.to_string())
}

/// Content-Types `web_fetch` knows how to turn into text. Anything else
/// (images, binaries, video, ...) is rejected with an error naming the type
/// rather than silently returned as garbage.
///
/// `url` is the page's (final, post-redirect) URL — only used for the HTML
/// branch's readability extraction (see [`extract_readable_content`]), not
/// otherwise part of content dispatch.
fn dispatch_content(
    content_type: &str,
    body: &str,
    url: &str,
) -> Result<(Option<String>, String), String> {
    // Strip any `; charset=...` parameter before matching.
    let base = content_type
        .split(';')
        .next()
        .unwrap_or(content_type)
        .trim()
        .to_ascii_lowercase();

    match base.as_str() {
        "text/html" => {
            let (title, html_for_conversion) = extract_readable_content(body, url);
            let markdown =
                htmd::convert(&html_for_conversion).map_err(|e| format!("Failed to convert HTML to Markdown: {}", e))?;
            Ok((title, markdown))
        }
        "text/plain" | "text/markdown" | "application/json" | "application/xml" | "text/xml" => Ok((None, body.to_string())),
        other => Err(format!(
            "Cannot fetch content of type '{}': only HTML, plain text, Markdown, JSON, and XML are supported",
            other
        )),
    }
}

/// Hop cap for [`build_redirect_policy`]'s custom policy — same maximum
/// `reqwest::redirect::Policy::default()`/`Policy::limited(10)` already
/// enforce for every *other* client in this app. `Policy::custom` does not
/// get that cap for free (its own doc comment says so explicitly: "the
/// custom variant does not do that for you automatically"), so without this
/// a redirect loop between two hosts that both pass [`validate_fetch_url`]
/// (i.e. both public) would otherwise only ever stop at [`FETCH_TIMEOUT`].
const MAX_REDIRECT_HOPS: usize = 10;

/// Builds the redirect policy [`fetch_impl`] hands to `reqwest`: caps the
/// chain at [`MAX_REDIRECT_HOPS`] hops (mirroring `Policy::default`'s own
/// limit, which a custom policy must reimplement itself — see
/// `MAX_REDIRECT_HOPS`'s doc comment) and re-runs [`validate_fetch_url`]
/// against every hop's target (`Attempt::url()` is already a parsed `Url`,
/// so no re-parsing is needed), erroring the whole request out the moment
/// either check fails rather than silently stopping at the last-good URL —
/// a caller must not mistake a blocked redirect for a successful fetch of
/// the pre-redirect page. Factored out of [`fetch_impl`] so a test can drive
/// the exact same policy against a real local server without going through
/// the rest of the fetch pipeline (whose own entry check would otherwise
/// reject a `127.0.0.1` test server before ever reaching the redirect).
fn build_redirect_policy(allow_local_network: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECT_HOPS {
            return attempt.error(refused(EgressDenial::about(
                EgressRule::RedirectHopLimit,
                format!("refusing to follow more than {MAX_REDIRECT_HOPS} redirects"),
            )));
        }
        // K5 must run before the SSRF pre-check below: hostname validation
        // resolves DNS, so doing it first would leak a disallowed name to the
        // resolver before the run's frozen policy had refused it. `send` checks
        // the initial URL; automatic redirects never pass through `send` again.
        if let Err(denial) = crate::egress::check_run_allowlist(attempt.url()) {
            return attempt.error(refused(denial));
        }
        match validate_fetch_url(attempt.url(), allow_local_network) {
            Ok(()) => {
                crate::egress::note_allowed_redirect_destination(attempt.url());
                attempt.follow()
            }
            // The hop's own rule, not a rule about redirects: a hop refused for
            // pointing at loopback should say `egress.loopback`, so the reason is
            // the same whether the address arrived in the request or in a `302`.
            Err(denial) => attempt.error(refused(denial)),
        }
    })
}

/// Hands a denial to a signature that wants an error.
///
/// The denial is passed as itself rather than as `to_string()`, so the rule
/// survives the trip out through `reqwest` and can be recovered with
/// `downcast_ref` instead of substring-matched. `egress.rs` has the same helper
/// for the same reason; this one is separate only because both are private.
fn refused(denial: EgressDenial) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::PermissionDenied, denial)
}

/// Custom `reqwest` DNS resolver that closes the check-then-connect
/// DNS-rebinding gap the module doc calls out: without this, [`validate_fetch_url`]
/// resolves a hostname once (via blocking `std::net::ToSocketAddrs`) purely to
/// decide whether to allow the request, and `reqwest`'s own default resolver
/// then performs a completely separate, later resolution to actually open the
/// TCP connection — an attacker controlling DNS for the target domain (TTL=0 /
/// a rebinding service) can answer those two lookups differently, passing the
/// check with a public address while the real connection lands on a private
/// one. Installed via `ClientBuilder::dns_resolver` in [`fetch_impl`], this
/// resolver is the *only* connect-time resolution for a hostname target: it
/// resolves through K5's per-run pin (async, so no blocking-DNS call lands on
/// the async runtime thread the way `validate_fetch_url`'s pre-check does) and
/// filters the pinned result through the exact same
/// [`blocked_reason_ip`] classifier `validate_fetch_url` uses, handing `reqwest`
/// only addresses that already passed the filter. Since the addresses handed
/// back are exactly what `reqwest` connects to — not a hint checked against a
/// separate, later lookup — there is no second resolution left for a
/// rebinding attacker to race. Applies on the initial request and every
/// redirect hop alike, since `reqwest` calls the installed resolver fresh for
/// each new host it connects to.
struct SsrfGuardedResolver {
    allow_local_network: bool,
}

impl reqwest::dns::Resolve for SsrfGuardedResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let allow_local_network = self.allow_local_network;
        let host = name.as_str().to_string();
        // Taken while this task's RunScope is active. K5 returns the run's
        // existing pin or resolves and records the first answer set.
        let pinned = crate::egress::resolve_pinned(name);
        Box::pin(async move {
            let resolved = pinned.await?;

            if allow_local_network {
                return Ok(resolved);
            }

            // Classified rather than merely filtered, so the refusal below can
            // name the rule that accounted for the last surviving answer instead
            // of a sentence covering all ten classes at once.
            let mut last_rule = None;
            let allowed: Vec<std::net::SocketAddr> = resolved
                .filter(|addr| match blocked_reason_ip(&addr.ip()) {
                    Some(rule) => {
                        last_rule = Some(rule);
                        false
                    }
                    None => true,
                })
                .collect();
            if allowed.is_empty() {
                // `DnsNoAddresses` when the lookup itself came back empty, which
                // no rule refused; otherwise the rule that refused the answers.
                let denial = match last_rule {
                    Some(rule) => EgressDenial::about(
                        rule,
                        format!("{host} resolves only to addresses this rule refuses"),
                    ),
                    None => EgressDenial::about(EgressRule::DnsNoAddresses, host.clone()),
                };
                return Err(Box::new(denial) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(allowed.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Reads `response`'s body into memory, stopping at `max` bytes.
///
/// Streams rather than calling `.text()`/`.bytes()` for one reason: those read
/// to end-of-stream, so the size of the allocation is the peer's choice. Here
/// the cap is ours, and it holds whether or not `Content-Length` was sent and
/// whether or not it was honest.
///
/// Over-long bodies are truncated rather than refused, which is the same
/// bargain [`fetch_impl`] already made: the callers parse a best-effort
/// document out of what arrived, and a body past these caps is pathological
/// either way.
async fn read_body_capped(mut response: reqwest::Response, max: usize) -> reqwest::Result<Vec<u8>> {
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= max {
            bytes.truncate(max);
            break;
        }
    }
    Ok(bytes)
}

/// Core `web_fetch` logic: AppHandle-free and directly testable (against a
/// local fixture server) — see the module doc for why. Not itself
/// cancellable; [`tool_web_fetch`] wraps the call in a `tokio::select!` against
/// the turn's cancel `Notify`, the same split `tool_run_shell` uses for its
/// child-process future.
///
/// `settings` supplies the real, user-configured `allow_local_network` and
/// (as the fallback when the model omits `max_chars`) `fetch_max_chars` —
/// phases 1-2 hardcoded both via module constants; this is the phase-3
/// settings-driven version `tool_web_fetch` (and monkey-cli, phase 4) call with
/// whatever `web_settings.json` currently holds.
///
/// AppHandle-free callers preserve an ambient run scope when one exists (the
/// durable monkey-cli task path supplies one); otherwise this public core labels
/// their traffic as a user action. [`tool_web_fetch`] enters the injected turn's
/// scope around [`fetch_within_scope`] directly.
pub async fn fetch_impl(
    settings: &WebSettings,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
) -> Result<FetchResult, String> {
    if settings.fetch_provider == FetchProvider::ExecutableExtension {
        return Err(
            "Executable web-fetch requires a trusted runtime call id; use fetch_for_call"
                .to_string(),
        );
    }
    user_action_when_unscoped(fetch_within_scope(settings, url, max_chars, start_index)).await
}

pub async fn fetch_for_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
) -> Result<FetchResult, String> {
    user_action_when_unscoped(fetch_within_scope_for_call(
        settings,
        trusted_call_id,
        url,
        max_chars,
        start_index,
    ))
    .await
}

async fn user_action_when_unscoped<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    if crate::run_scope::current().is_some() {
        future.await
    } else {
        crate::run_scope::scoped(
            crate::run_scope::RunScope::Unattributed(crate::run_scope::Unattributed::UserAction),
            future,
        )
        .await
    }
}

/// [`fetch_impl`]'s body, with the scope already established.
///
/// Split out rather than wrapping the body in an `async` block so that the
/// `scoped` call is one readable frame and this function's own diff stays empty.
/// Every refusal below is raised while this future is being polled, which is what
/// puts it inside the scope: `validate_fetch_url` here, and the same function again
/// from inside the redirect policy, which `reqwest` invokes while polling the
/// request future this task owns.
async fn fetch_within_scope(
    settings: &WebSettings,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
) -> Result<FetchResult, String> {
    fetch_within_scope_for_call(settings, "direct-web-fetch", url, max_chars, start_index).await
}

async fn fetch_within_scope_for_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
) -> Result<FetchResult, String> {
    let max_chars = max_chars.unwrap_or(settings.fetch_max_chars);
    let start_index = start_index.unwrap_or(0);

    let parsed = Url::parse(&url).map_err(|e| format!("Invalid URL '{}': {}", url, e))?;
    // `validate_fetch_url` resolves hostname targets. Enforce the run's frozen
    // host/port/protocol policy first so a denied hostname is not leaked to DNS.
    crate::egress::check_run_allowlist(&parsed)
        .map_err(|denial| fetch_refusal(&parsed, &denial))?;
    validate_fetch_url(&parsed, settings.allow_local_network)
        .map_err(|denial| fetch_refusal(&parsed, &denial))?;

    if let Some(call) = extension_fetch_call(
        settings,
        trusted_call_id,
        &url,
        Some(max_chars),
        Some(start_index),
    )? {
        let output_json = invoke_web_extension(CapabilityKind::WebFetch, call).await?;
        return parse_extension_fetch_output(
            &output_json,
            &url,
            max_chars,
            start_index,
            settings.allow_local_network,
        );
    }

    let client = fetch_client(settings)?;

    let response = crate::egress::send(client.get(parsed.clone()))
        .await
        .map_err(|e| format!("Failed to fetch '{}': {}", url, e))?;

    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let bytes = read_body_capped(response, MAX_BODY_BYTES)
        .await
        .map_err(|e| format!("Failed to read response body from '{}': {}", url, e))?;

    let body = String::from_utf8_lossy(&bytes).into_owned();
    let (title, full_content) = dispatch_content(&content_type, &body, &final_url)?;
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

fn fetch_client(settings: &WebSettings) -> Result<reqwest::Client, String> {
    crate::egress::hardened()
        .timeout(FETCH_TIMEOUT)
        .redirect(build_redirect_policy(settings.allow_local_network))
        .dns_resolver(std::sync::Arc::new(SsrfGuardedResolver {
            allow_local_network: settings.allow_local_network,
        }))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

/// Builds the client used by the executable-extension HTTP broker.
///
/// The caller still validates the request URL with [`validate_fetch_url`] so
/// literal private addresses are refused before any request is built. This
/// client closes the hostname check/connect gap: its resolver filters the exact
/// pinned addresses handed to the connector, while [`crate::egress::hardened`]
/// keeps redirects on the original origin. Proxies are disabled because a
/// proxy would resolve the target outside this guarded resolver.
pub(crate) fn executable_extension_http_client(
    read_budget: Duration,
) -> reqwest::Result<reqwest::Client> {
    crate::egress::hardened_with_read_budget(read_budget)
        .no_proxy()
        .dns_resolver(std::sync::Arc::new(SsrfGuardedResolver {
            allow_local_network: false,
        }))
        .build()
}

fn tool_egress_scope<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    turn_id: Option<&str>,
) -> Result<crate::run_scope::RunScope, String> {
    let (run_id, attribution) = permissions::permission_attribution(app, state, turn_id)?;
    egress_scope_for_attribution(run_id, attribution)
}

fn egress_scope_for_attribution(
    run_id: Option<String>,
    attribution: crate::run_ledger::PermissionAttribution,
) -> Result<crate::run_scope::RunScope, String> {
    use crate::run_ledger::PermissionAttribution;
    use crate::run_scope::{RunScope, Unattributed};

    match attribution {
        PermissionAttribution::LedgerRun | PermissionAttribution::UnregisteredRun => run_id
            .map(RunScope::run)
            .ok_or_else(|| "Network tool attribution named a run without its id".to_string()),
        PermissionAttribution::Unattributed(reason) => Ok(RunScope::Unattributed(reason)),
        PermissionAttribution::Unknown => Ok(RunScope::Unattributed(Unattributed::UserAction)),
    }
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
// Each parameter is an IPC field the frontend sends by name, so folding them
// into a struct would change the tool-call contract rather than simplify it.
#[allow(clippy::too_many_arguments)]
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_web_fetch(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    url: String,
    max_chars: Option<usize>,
    start_index: Option<usize>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
    checkpoint_id: Option<String>,
) -> Result<FetchResult, String> {
    permissions::request_permission(
        &app,
        state.inner(),
        "web_fetch",
        url.clone(),
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        None,
        None,
    )
    .await?;

    // After the gate, so a refused request records nothing, and before the
    // request, because a call that was permitted and then failed may still
    // have reached the network — the same ordering `egress::send` uses for
    // the destination it records.
    checkpoints::record_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Network,
    )?;

    let settings = load_settings_impl(&settings_file_path(&app)?)?;
    let egress_scope = tool_egress_scope(&app, state.inner(), turn_id.as_deref())?;
    let trusted_call_id = if settings.fetch_provider == FetchProvider::ExecutableExtension {
        tool_call_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("Executable web-fetch requires its runtime-owned tool_call_id")?
            .to_string()
    } else {
        "builtin-web-fetch".to_string()
    };
    let extension_invocation_id =
        extension_fetch_call(&settings, &trusted_call_id, &url, max_chars, start_index)?
            .map(|call| call.invocation_id);

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

    let operation = crate::run_commands::scoped_with_egress(
        &app,
        state.inner(),
        egress_scope,
        fetch_within_scope_for_call(&settings, &trusted_call_id, url, max_chars, start_index),
    );
    tokio::pin!(operation);
    let outcome = tokio::select! {
        biased;
        result = &mut operation => result,
        _ = cancel.notified() => {
            if let Some(invocation_id) = extension_invocation_id.as_deref() {
                let _ = crate::executable_extensions::cancel(invocation_id);
                let _ = operation.await;
            }
            Err("Fetch cancelled by the user".to_string())
        },
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
        if guard
            .get(&cancel_key)
            .is_some_and(|n| std::sync::Arc::strong_count(n) <= 2)
        {
            guard.remove(&cancel_key);
        }
    }

    // Committed only when the call came back. A cancelled or errored one keeps
    // the declaration above and nothing more: the request may already have been
    // delivered, and "we didn't see a response" is not "the server saw nothing".
    if outcome.is_ok() {
        checkpoints::commit_external_effect(
            state.inner(),
            checkpoint_id.as_deref(),
            checkpoints::ExternalEffectKind::Network,
        )?;
    }

    outcome
}

/// One ranked result from [`search_impl`], echoed back to the model as JSON.
/// Same "plain snake_case, no `serde(rename)`" convention as [`FetchResult`].
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
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

/// Cap on a search backend's response body, the same protection
/// [`MAX_BODY_BYTES`] gives the fetch path. Smaller than that one because a
/// search response is a results page or a JSON array, not an arbitrary
/// document: DuckDuckGo's HTML runs a few hundred KB and both JSON payloads are
/// far under it, so this only ever bites a body no honest backend sends.
const MAX_SEARCH_BODY_BYTES: usize = 2 * 1024 * 1024;

fn parse_extension_search_output(
    output_json: &str,
    count: usize,
) -> Result<Vec<SearchResult>, String> {
    if output_json.len() > MAX_SEARCH_BODY_BYTES {
        return Err("Executable web-search output exceeds its byte limit".to_string());
    }
    let results: Vec<SearchResult> = serde_json::from_str(output_json).map_err(|error| {
        format!("Executable web-search returned invalid normalized JSON: {error}")
    })?;
    if results.len() > count {
        return Err("Executable web-search returned more results than requested".to_string());
    }
    for result in &results {
        if result.title.trim().is_empty()
            || result.title.contains('\0')
            || result.title.chars().count() > MAX_EXTENSION_RESULT_TITLE_CHARS
            || result.snippet.contains('\0')
            || result.snippet.chars().count() > MAX_EXTENSION_SEARCH_SNIPPET_CHARS
        {
            return Err("Executable web-search returned invalid bounded text".to_string());
        }
        parse_extension_result_url("search-result URL", &result.url)?;
    }
    Ok(results)
}

/// The client all three search backends share.
///
/// Starts from [`crate::egress::hardened_with_read_budget`] rather than a bare
/// `Client::builder()` for one property the three hand-rolled builders it
/// replaces did not have: a redirect that has to stay on the origin the request
/// was aimed at.
///
/// The distinction the previous code missed is between the *request* and the
/// *response*. Each backend's request target is trustworthy — a vendor constant,
/// or a base URL the user typed into Settings themselves — and the old doc
/// comments were right that the untrusted query text never builds a URL. But a
/// `302` is chosen by the response, and reqwest's default `Policy::limited(10)`
/// follows one to any host. Two concrete consequences, both closed here:
///
/// - **Brave's API key could walk to a host the redirect picked.**
///   `X-Subscription-Token` is not one of the four headers reqwest strips when a
///   redirect crosses origins (it strips `Authorization`, `Cookie`,
///   `Proxy-Authorization`, `WWW-Authenticate`) — the same hazard
///   [`crate::egress::hardened`] documents for `x-api-key`.
/// - **A hop could reach unauthenticated loopback services.** This machine's own
///   `llama-server` and `ollama:11434` have no authentication at all.
///   `allow_local_network` was no defence: it is consulted only on the fetch
///   path, so every search client followed a loopback hop with the setting off.
///
/// A SearXNG instance is the sharpest edge of it — that base URL is the one
/// search target a user can point anywhere, and `normalize_base_url` accepts
/// plain `http://` on purpose, because a self-hosted instance on the LAN or on
/// loopback is a supported setup. The rule here is deliberately *relative*
/// (does this hop stay where it was already going?) rather than absolute (is
/// this a public HTTPS host?), for exactly that reason: an absolute rule would
/// refuse the self-hosted instance outright, while the relative one leaves it
/// working and still refuses the `302` off it.
///
/// The total [`SEARCH_TIMEOUT`] deadline stays layered on top of the inherited
/// connect and read budgets. That is safe here, unlike on a download path: these
/// bodies are small and buffered, so a whole-request ceiling bounds a slow
/// backend rather than truncating a legitimate transfer.
fn search_client() -> Result<reqwest::Client, String> {
    crate::egress::hardened_with_read_budget(SEARCH_TIMEOUT)
        .timeout(SEARCH_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {}", e))
}

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
    let snippet_selector =
        scraper::Selector::parse(".result__snippet").expect("valid CSS selector");

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
        results.push(SearchResult {
            title,
            url,
            snippet,
        });
    }
    results
}

/// DuckDuckGo branch of [`search_impl`] (phase 2's entire dispatch, now one
/// arm of three). AppHandle-free and directly testable, same split as
/// [`fetch_impl`].
///
/// Unlike `fetch_impl`, there is no SSRF guard here to run on the request: the
/// target is DuckDuckGo's own fixed endpoint, never a user/model-supplied URL —
/// only the query text (sent as a POST form field, not part of the URL) is
/// untrusted. The *response* is a different matter, and [`search_client`]
/// handles it: a `302` off that endpoint is followed only if it stays on it.
async fn ddg_search(query: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let client = search_client()?;

    let response = crate::egress::send(client.post(DUCKDUCKGO_HTML_ENDPOINT).form(&[("q", query)]))
        .await
        .map_err(|e| format!("Failed to search DuckDuckGo for '{}': {}", query, e))?;

    let status = response.status();
    let body = read_body_capped(response, MAX_SEARCH_BODY_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("Failed to read DuckDuckGo response for '{}': {}", query, e))?;

    if !status.is_success() {
        return Err(format!(
            "DuckDuckGo search for '{}' returned HTTP {}",
            query, status
        ));
    }

    Ok(parse_ddg_results(&body, count))
}

/// Brave Search API's web-search endpoint (design doc / task spec, verified
/// against Brave's own API docs at implementation time): `X-Subscription-Token`
/// header for auth, `q`/`count` query params, `web.results[].{title,url,description}`
/// in the JSON response.
const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";

/// Minimal shape of a Brave web-search JSON response — same defensive,
/// lenient style as `ollama.rs::RawTagEntry`: every field defaults rather
/// than failing the whole parse if Brave's schema grows a field this doesn't
/// know about, or omits `web` entirely (e.g. a query with zero results).
#[derive(serde::Deserialize, Default)]
struct BraveResponse {
    #[serde(default)]
    web: Option<BraveWebResults>,
}

#[derive(serde::Deserialize, Default)]
struct BraveWebResults {
    #[serde(default)]
    results: Vec<BraveResult>,
}

#[derive(serde::Deserialize, Default)]
struct BraveResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    description: String,
}

/// Parses a Brave web-search JSON response body into up to `count`
/// [`SearchResult`]s. Split out from [`brave_search_at`] so a fixture can
/// exercise the parsing on its own, same reasoning as [`parse_ddg_results`].
fn parse_brave_response(body: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let parsed: BraveResponse =
        serde_json::from_str(body).map_err(|e| format!("Failed to parse Brave response: {e}"))?;
    let results = parsed.web.map(|w| w.results).unwrap_or_default();
    Ok(results
        .into_iter()
        .take(count)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            snippet: r.description,
        })
        .collect())
}

/// Brave branch of [`search_impl`], parameterized by `endpoint` so
/// [`tests::brave_search_at_surfaces_an_unauthorized_response_as_an_error`]
/// can point it at a local fixture server instead of the real Brave API —
/// [`brave_search`] is the real-endpoint entry point both `search_impl` and
/// [`web_set_brave_key`]'s validate-before-store call use.
///
/// This is the credentialed search path — [`search_client`]'s origin-pinned
/// redirect policy is what keeps the `X-Subscription-Token` below from riding a
/// `302` to a host of the response's choosing.
async fn brave_search_at(
    endpoint: &str,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchResult>, String> {
    let client = search_client()?;

    let response = crate::egress::send(
        client
            .get(endpoint)
            .header("X-Subscription-Token", api_key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &count.to_string())]),
    )
    .await
    .map_err(|e| format!("Failed to search Brave for '{}': {}", query, e))?;

    let status = response.status();
    let body = read_body_capped(response, MAX_SEARCH_BODY_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("Failed to read Brave response for '{}': {}", query, e))?;

    if !status.is_success() {
        return Err(format!(
            "Brave search for '{}' returned HTTP {}{}",
            query,
            status,
            if body.is_empty() {
                String::new()
            } else {
                format!(": {body}")
            }
        ));
    }

    parse_brave_response(&body, count)
}

async fn brave_search(
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchResult>, String> {
    brave_search_at(BRAVE_SEARCH_ENDPOINT, api_key, query, count).await
}

/// Minimal shape of a SearXNG `format=json` response — same lenient,
/// defaults-everywhere style as [`BraveResponse`].
#[derive(serde::Deserialize, Default)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(serde::Deserialize, Default)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

/// Parses a SearXNG `format=json` response body into up to `count`
/// [`SearchResult`]s. Split out from [`searxng_search`] for the same
/// fixture-testability reason as [`parse_brave_response`].
fn parse_searxng_response(body: &str, count: usize) -> Result<Vec<SearchResult>, String> {
    let parsed: SearxngResponse =
        serde_json::from_str(body).map_err(|e| format!("Failed to parse SearXNG response: {e}"))?;
    Ok(parsed
        .results
        .into_iter()
        .take(count)
        .map(|r| SearchResult {
            title: r.title,
            url: r.url,
            snippet: r.content,
        })
        .collect())
}

/// SearXNG branch of [`search_impl`]: `GET {base_url}/search?q=&format=json`.
/// A `403` is SearXNG's standard response when the instance's `settings.yml`
/// hasn't opted into `formats: [html, json]` (JSON is off by default on most
/// public instances for exactly this reason — it's meant for programmatic
/// use, not casual scraping) — mapped to an explicit, actionable hint rather
/// than a bare "HTTP 403" the user would have to go guess the cause of (the
/// design doc's own risk note: "needs the explicit error hint or users will
/// blame the app").
///
/// `base_url` is the one search target a user can point anywhere, so this is the
/// branch [`search_client`]'s origin-pinned redirect matters most on: the
/// configured host stays reachable, including a self-hosted instance on plain
/// `http://` or on loopback, but a `302` off it does not.
async fn searxng_search(
    base_url: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchResult>, String> {
    let client = search_client()?;

    let url = format!("{}/search", base_url.trim_end_matches('/'));
    let response = crate::egress::send(client.get(&url).query(&[("q", query), ("format", "json")]))
        .await
        .map_err(|e| format!("Failed to search SearXNG at '{}': {}", base_url, e))?;

    let status = response.status();
    if status == reqwest::StatusCode::FORBIDDEN {
        return Err(format!(
            "SearXNG at '{}' returned HTTP 403 — enable `formats: [html, json]` for this instance in its settings.yml",
            base_url
        ));
    }

    let body = read_body_capped(response, MAX_SEARCH_BODY_BYTES)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("Failed to read SearXNG response from '{}': {}", base_url, e))?;

    if !status.is_success() {
        return Err(format!(
            "SearXNG search at '{}' returned HTTP {}",
            base_url, status
        ));
    }

    parse_searxng_response(&body, count)
}

/// Core `web_search` logic: AppHandle-free and directly testable, same split
/// as [`fetch_impl`]. Dispatches on `settings.search_provider` — phase 2's
/// DuckDuckGo-only version was the entire body of this function; Brave and
/// SearXNG are phase 3.
///
/// `brave_key` is passed in rather than read from the keychain here, so this
/// function stays free of any keychain access of its own — [`tool_web_search`]
/// (and monkey-cli's shared call site, phase 4) resolve it via [`read_brave_key`]
/// and pass the result through, keeping `search_impl` trivially testable
/// without touching the real OS keychain.
///
/// Unlike `fetch_impl`, there is no SSRF guard to run on the *request* for any
/// of the three branches: Brave/SearXNG/DuckDuckGo targets are either a fixed
/// vendor endpoint or a user-configured SearXNG base URL the user themselves
/// typed into Settings (not a model-supplied URL) — only the query text is
/// untrusted, and it's sent as a query/form parameter, never used to build a
/// request to an arbitrary host.
///
/// The *response* is guarded, though, and by the client rather than by this
/// function: all three branches build from [`search_client`], whose redirect
/// policy refuses a hop that leaves the origin the request was aimed at. See its
/// doc comment for why a trustworthy request target does not make a `302` off it
/// trustworthy too.
pub async fn search_impl(
    settings: &WebSettings,
    brave_key: Option<String>,
    query: String,
    count: Option<usize>,
) -> Result<Vec<SearchResult>, String> {
    if settings.search_provider == SearchProvider::ExecutableExtension {
        return Err(
            "Executable web-search requires a trusted runtime call id; use search_for_call"
                .to_string(),
        );
    }
    user_action_when_unscoped(search_within_scope(settings, brave_key, query, count)).await
}

pub async fn search_for_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    brave_key: Option<String>,
    query: String,
    count: Option<usize>,
) -> Result<Vec<SearchResult>, String> {
    user_action_when_unscoped(search_within_scope_for_call(
        settings,
        trusted_call_id,
        brave_key,
        query,
        count,
    ))
    .await
}

async fn search_within_scope(
    settings: &WebSettings,
    brave_key: Option<String>,
    query: String,
    count: Option<usize>,
) -> Result<Vec<SearchResult>, String> {
    search_within_scope_for_call(settings, "direct-web-search", brave_key, query, count).await
}

async fn search_within_scope_for_call(
    settings: &WebSettings,
    trusted_call_id: &str,
    brave_key: Option<String>,
    query: String,
    count: Option<usize>,
) -> Result<Vec<SearchResult>, String> {
    let count = count
        .unwrap_or(DEFAULT_SEARCH_COUNT)
        .clamp(1, DEFAULT_SEARCH_COUNT);

    match settings.search_provider {
        SearchProvider::Duckduckgo => ddg_search(&query, count).await,
        SearchProvider::Brave => {
            let key = brave_key.filter(|k| !k.trim().is_empty()).ok_or_else(|| {
                "Brave search requires an API key — add one in Settings > Web.".to_string()
            })?;
            brave_search(&key, &query, count).await
        }
        SearchProvider::Searxng => {
            let base = settings
                .searxng_base_url
                .clone()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    "SearXNG requires a base URL — set one in Settings > Web.".to_string()
                })?;
            searxng_search(&base, &query, count).await
        }
        SearchProvider::ExecutableExtension => {
            let call = extension_search_call(settings, trusted_call_id, &query, Some(count))?
                .ok_or_else(|| "Executable web-search provider is not selected".to_string())?;
            let output_json = invoke_web_extension(CapabilityKind::WebSearch, call).await?;
            parse_extension_search_output(&output_json, count)
        }
    }
}

/// Search the web via the configured provider (DuckDuckGo/Brave/SearXNG) and
/// return up to `count` (1-10, default 10) ranked `{title, url, snippet}`
/// results. Permission-gated: prompts with the query as detail, exactly like
/// `tool_web_fetch` prompts with the URL. `turn_id` scopes both the
/// permission prompt and Stop-button cancellation to the calling turn (never
/// model-supplied) — same `tokio::select!` + `state.tool_cancel` split
/// `tool_web_fetch` uses, rather than the earlier "no cancellation wiring"
/// stance: a user-configured SearXNG instance (`settings.searxng_base_url`)
/// can be slow or hang just as easily as an arbitrary fetched page, and
/// without this a Stop click would silently do nothing on the backend for
/// up to `SEARCH_TIMEOUT` while the frontend already reports the tool call
/// as cancelled.
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
    tool_call_id: Option<String>,
    checkpoint_id: Option<String>,
) -> Result<Vec<SearchResult>, String> {
    permissions::request_permission(
        &app,
        state.inner(),
        "web_search",
        query.clone(),
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        None,
        None,
    )
    .await?;

    // After the gate, so a refused request records nothing, and before the
    // request, because a call that was permitted and then failed may still
    // have reached the network — the same ordering `egress::send` uses for
    // the destination it records.
    checkpoints::record_external_effect(
        state.inner(),
        checkpoint_id.as_deref(),
        checkpoints::ExternalEffectKind::Network,
    )?;

    let settings = load_settings_impl(&settings_file_path(&app)?)?;
    let brave_key = if settings.search_provider == SearchProvider::Brave {
        // Absence just means "dispatch will surface the actionable error" —
        // read_brave_key()'s own message ("add one in Settings") is a fine
        // fallback but search_impl's Brave-branch message is more specific
        // to this call site, so a missing key is folded to `None` here
        // rather than short-circuiting on `read_brave_key`'s own Err.
        read_brave_key().ok()
    } else {
        None
    };
    let egress_scope = tool_egress_scope(&app, state.inner(), turn_id.as_deref())?;
    let trusted_call_id = if settings.search_provider == SearchProvider::ExecutableExtension {
        tool_call_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .ok_or("Executable web-search requires its runtime-owned tool_call_id")?
            .to_string()
    } else {
        "builtin-web-search".to_string()
    };
    let extension_invocation_id =
        extension_search_call(&settings, &trusted_call_id, &query, count)?
            .map(|call| call.invocation_id);

    // Same per-turn cancellation channel `tool_web_fetch`/`tool_run_shell`
    // use — see `AppState::tool_cancel`'s doc comment. Callers that don't
    // thread a turn id share the "" channel.
    let cancel_key = turn_id.unwrap_or_default();
    let cancel = state
        .tool_cancel
        .lock()
        .map_err(|_| "Tool-cancel lock poisoned".to_string())?
        .entry(cancel_key.clone())
        .or_insert_with(|| std::sync::Arc::new(Notify::new()))
        .clone();

    let operation = crate::run_commands::scoped_with_egress(
        &app,
        state.inner(),
        egress_scope,
        search_within_scope_for_call(&settings, &trusted_call_id, brave_key, query, count),
    );
    tokio::pin!(operation);
    let outcome = tokio::select! {
        biased;
        result = &mut operation => result,
        _ = cancel.notified() => {
            if let Some(invocation_id) = extension_invocation_id.as_deref() {
                let _ = crate::executable_extensions::cancel(invocation_id);
                let _ = operation.await;
            }
            Err("Search cancelled by the user".to_string())
        },
    };

    // Drop this turn's channel once no other in-flight tool of the same turn
    // still holds it — mirrors `tool_web_fetch`'s own cleanup so the map
    // doesn't grow one entry per turn forever.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard
            .get(&cancel_key)
            .is_some_and(|n| std::sync::Arc::strong_count(n) <= 2)
        {
            guard.remove(&cancel_key);
        }
    }

    // Committed only when the call came back. A cancelled or errored one keeps
    // the declaration above and nothing more: the request may already have been
    // delivered, and "we didn't see a response" is not "the server saw nothing".
    if outcome.is_ok() {
        checkpoints::commit_external_effect(
            state.inner(),
            checkpoint_id.as_deref(),
            checkpoints::ExternalEffectKind::Network,
        )?;
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
    fn network_tools_keep_registered_and_unregistered_turn_ids_for_k5() {
        use crate::run_ledger::PermissionAttribution;

        for attribution in [
            PermissionAttribution::LedgerRun,
            PermissionAttribution::UnregisteredRun,
        ] {
            let scope = egress_scope_for_attribution(Some("turn:web".to_string()), attribution)
                .expect("turn scope");
            assert_eq!(scope.run_id(), Some("turn:web"));
        }
        let scope = egress_scope_for_attribution(None, PermissionAttribution::Unknown)
            .expect("user action scope");
        assert_eq!(
            scope.unattributed(),
            Some(crate::run_scope::Unattributed::UserAction)
        );
    }

    struct RunPolicyReset;

    impl Drop for RunPolicyReset {
        fn drop(&mut self) {
            crate::egress::clear_run_policy_source();
        }
    }

    fn install_test_run_policy(
        run_id: &str,
        hosts: &[&str],
        ports: &[u16],
        protocols: &[&str],
    ) -> RunPolicyReset {
        let run_id = run_id.to_string();
        let allowlist = crate::run_protocol::EgressAllowlist {
            hosts: hosts.iter().map(|host| (*host).to_string()).collect(),
            ports: ports.to_vec(),
            protocols: protocols
                .iter()
                .map(|protocol| (*protocol).to_string())
                .collect(),
        };
        allowlist.validate().expect("valid test allowlist");
        crate::egress::install_run_policy_source(move |asked| {
            if asked == run_id {
                crate::egress::RunEgressPolicy::Declared(std::sync::Arc::new(allowlist.clone()))
            } else {
                crate::egress::RunEgressPolicy::Unknown
            }
        });
        RunPolicyReset
    }

    #[tokio::test]
    async fn fetch_checks_the_run_allowlist_before_resolving_the_initial_host() {
        let _serialized = crate::denial_sink::test_lock();
        let _policy =
            install_test_run_policy("run:web-initial", &["allowed.example"], &[443], &["https"]);

        let error = crate::run_scope::scoped(
            crate::run_scope::RunScope::run("run:web-initial"),
            fetch_within_scope(
                &WebSettings::default(),
                "https://must-not-resolve.invalid/".to_string(),
                None,
                None,
            ),
        )
        .await
        .expect_err("the undeclared host must be refused");

        assert!(
            error.contains(EgressRule::RunHostNotAllowlisted.code()),
            "unexpected refusal: {error}"
        );
        assert!(
            !error.contains(EgressRule::DnsResolutionFailed.code()),
            "the refused hostname must not reach DNS: {error}"
        );
    }

    #[tokio::test]
    async fn app_handle_free_fetch_preserves_an_ambient_run_for_k5() {
        let _serialized = crate::denial_sink::test_lock();
        let _policy =
            install_test_run_policy("run:cli-fetch", &["allowed.example"], &[443], &["https"]);

        let error = crate::run_scope::scoped(
            crate::run_scope::RunScope::run("run:cli-fetch"),
            fetch_impl(
                &WebSettings::default(),
                "https://must-not-resolve.invalid/".to_string(),
                None,
                None,
            ),
        )
        .await
        .expect_err("the CLI helper must retain the run allowlist");

        assert!(
            error.contains(EgressRule::RunHostNotAllowlisted.code()),
            "unexpected refusal: {error}"
        );
    }

    #[tokio::test]
    async fn app_handle_free_search_preserves_an_ambient_run_for_k5() {
        let _serialized = crate::denial_sink::test_lock();
        let directory =
            std::env::temp_dir().join(format!("lm-web-cli-search-sink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates the sink directory");
        let path = directory.join(crate::denial_sink::SINK_FILE);
        crate::denial_sink::install(
            crate::denial_sink::DenialSink::open(&path).expect("the sink opens"),
        );
        let _policy =
            install_test_run_policy("run:cli-search", &["allowed.example"], &[443], &["https"]);
        let mut settings = WebSettings::default();
        settings.search_provider = SearchProvider::Searxng;
        settings.searxng_base_url = Some("https://must-not-resolve.invalid".to_string());

        crate::run_scope::scoped(
            crate::run_scope::RunScope::run("run:cli-search"),
            search_impl(&settings, None, "query".to_string(), Some(1)),
        )
        .await
        .expect_err("the CLI helper must retain the run allowlist");

        let reader = crate::denial_sink::DenialSink::open(&path).expect("reopens for reading");
        let mine: Vec<_> = reader
            .recent(64)
            .expect("reads")
            .into_iter()
            .filter(|row| row.run_id.as_deref() == Some("run:cli-search"))
            .collect();
        assert_eq!(mine.len(), 1, "exactly one denial belongs to this run");
        assert_eq!(mine[0].rule_code, EgressRule::RunHostNotAllowlisted.code());

        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn fetch_rechecks_the_run_allowlist_before_resolving_a_redirect_host() {
        let _serialized = crate::denial_sink::test_lock();
        let _policy =
            install_test_run_policy("run:web-redirect", &["allowed.example"], &[443], &["https"]);
        let origin =
            spawn_redirecting_server("https://must-not-resolve.invalid/steal".to_string(), "");
        let mut settings = WebSettings::default();
        settings.allow_local_network = true;

        let error =
            crate::run_scope::scoped(crate::run_scope::RunScope::run("run:web-redirect"), async {
                let client = fetch_client(&settings).expect("build fetch client");
                crate::egress::send(client.get(origin))
                    .await
                    .expect_err("the redirect to an undeclared host must be refused")
            })
            .await;

        assert_eq!(
            denied_rule(&error),
            Some(EgressRule::RunHostNotAllowlisted),
            "unexpected refusal: {error}"
        );
    }

    #[tokio::test]
    async fn fetch_accounts_each_allowed_redirect_destination() {
        let _serialized = crate::denial_sink::test_lock();
        crate::egress::clear_run_policy_source();
        let (target, seen) = spawn_recording_server();
        let origin = spawn_redirecting_server(format!("{target}/landed"), "");
        let expected_ports: std::collections::BTreeSet<u16> = [&origin, &target]
            .into_iter()
            .map(|url| {
                Url::parse(url)
                    .expect("fixture url parses")
                    .port_or_known_default()
                    .expect("fixture url has a port")
            })
            .collect();
        let mut settings = WebSettings::default();
        settings.allow_local_network = true;
        let process = crate::run_scope::ProcessScope::new("p-web-redirect-accounting");

        let response = crate::run_scope::scoped_with_process(
            crate::run_scope::RunScope::run("run:web-redirect-accounting"),
            process.clone(),
            async {
                let client = fetch_client(&settings).expect("build fetch client");
                crate::egress::send(client.get(origin))
                    .await
                    .expect("the allowed redirect must be followed")
            },
        )
        .await;
        assert!(response.status().is_success());
        assert!(
            seen.lock().unwrap().is_some(),
            "the redirect target must be contacted"
        );

        let destinations = process.take_destinations();
        let actual_ports: std::collections::BTreeSet<u16> = destinations
            .seen
            .iter()
            .map(|(destination, requests)| {
                assert_eq!(*requests, 1, "each hop is one request");
                destination.port
            })
            .collect();
        assert_eq!(actual_ports, expected_ports);
        assert_eq!(destinations.overflowed, 0);
    }

    /// `::127.0.0.1` walked past this guard entirely: not `::1`, not unspecified,
    /// not `fc00::/7`, not `fe80::/10`, and `to_ipv4_mapped()` returns `None` for
    /// the deprecated compatible form, so it read as an ordinary public address —
    /// a loopback SSRF target on a guard whose whole job is refusing those.
    /// The rule `validate_fetch_url` refused `target` with.
    ///
    /// Every address test below goes through this rather than through
    /// `unwrap_err().contains("…")`. Seven of them used to substring-match the
    /// single string `"local/private"`, which ten different predicates shared —
    /// so a test that meant "loopback was refused" could only prove "one of ten
    /// classes was refused", and a guard that misclassified loopback as private
    /// would have passed every one of them.
    /// Drives [`classify_fetch_url`], the pure half, deliberately. Going through
    /// [`validate_fetch_url`] would make every one of the assertions below write
    /// into whatever process-wide sink another test happened to install.
    fn refusing_rule(target: &str) -> EgressRule {
        classify_fetch_url(&url(target), false)
            .expect_err("expected a refusal")
            .rule()
    }

    /// The recording half, and the claim K5's acceptance actually makes: a blocked
    /// attempt becomes a durable record naming the rule that blocked it.
    ///
    /// Uses a file-backed sink and a second connection to read it, rather than a
    /// test-only accessor on the installed one — which also exercises the path that
    /// ships. The assertion filters by a host unique to this test, so it cannot be
    /// satisfied (or broken) by a denial another test recorded into the same sink.
    #[test]
    fn a_refused_fetch_is_written_down_with_its_rule() {
        let _serialized = crate::denial_sink::test_lock();
        let directory = std::env::temp_dir().join(format!("lm-web-sink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates the directory");
        let path = directory.join(crate::denial_sink::SINK_FILE);
        crate::denial_sink::install(
            crate::denial_sink::DenialSink::open(&path).expect("the sink opens"),
        );

        // A literal, so no DNS is involved and the rule is unambiguous.
        let denial = validate_fetch_url(&url("http://169.254.169.254/latest/meta-data/"), false)
            .expect_err("link-local must be refused");
        assert_eq!(denial.rule(), EgressRule::LinkLocal);

        let reader = crate::denial_sink::DenialSink::open(&path).expect("reopens for reading");
        let mine: Vec<_> = reader
            .recent(64)
            .expect("reads")
            .into_iter()
            .filter(|row| row.detail.as_deref() == Some("169.254.169.254"))
            .collect();

        assert_eq!(mine.len(), 1, "exactly one record for this test's address");
        assert_eq!(mine[0].rule_code, EgressRule::LinkLocal.code());
        assert_eq!(mine[0].guard, GUARD);
        // Both blank, and that is the point of asserting it: this drives the guard
        // directly, outside every scope, so it records the third honest state —
        // "nobody said". The reason column is filled in by `fetch_impl`'s scope and
        // by nothing inside `record` itself, which is what the test below proves and
        // what this row is the counter-example to.
        assert_eq!(mine[0].run_id, None, "no scope was entered, so no run id");
        assert_eq!(
            mine[0].unattributed_reason, None,
            "an uninstrumented site must stay distinguishable from a deliberate one"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The other half: driven through the production entry point, the same refusal
    /// records *why* it has no run instead of a blank.
    ///
    /// A literal address, so no DNS and no listener are involved — the refusal
    /// happens before a socket is opened, which is what makes this a hermetic test of
    /// an egress path. The address is unique to this test so a denial another test
    /// recorded into the same process-wide sink cannot satisfy the filter.
    ///
    /// Sabotage check: delete the `scoped` call in `fetch_impl` and the reason column
    /// goes back to `NULL`, failing the last assertion while the two above it still
    /// pass. That is the whole distinction D3 exists to make.
    #[tokio::test]
    async fn a_refused_tool_fetch_records_the_reason_it_has_no_run() {
        let _serialized = crate::denial_sink::test_lock();
        let directory = std::env::temp_dir().join(format!("lm-web-scope-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates the directory");
        let path = directory.join(crate::denial_sink::SINK_FILE);
        crate::denial_sink::install(
            crate::denial_sink::DenialSink::open(&path).expect("the sink opens"),
        );

        let error = fetch_impl(
            &WebSettings::default(),
            "http://10.83.7.11/private".to_string(),
            None,
            None,
        )
        .await
        .expect_err("a private literal must be refused");
        assert!(
            error.contains(EgressRule::PrivateV4.code()),
            "unexpected error: {error}"
        );

        let reader = crate::denial_sink::DenialSink::open(&path).expect("reopens for reading");
        let mine: Vec<_> = reader
            .recent(64)
            .expect("reads")
            .into_iter()
            .filter(|row| row.detail.as_deref() == Some("10.83.7.11"))
            .collect();

        assert_eq!(mine.len(), 1, "exactly one record for this test's address");
        assert_eq!(mine[0].guard, GUARD);
        assert_eq!(mine[0].rule_code, EgressRule::PrivateV4.code());
        assert_eq!(
            mine[0].run_id, None,
            "a chat-turn fetch has no run id to invent"
        );
        assert_eq!(
            mine[0].unattributed_reason.as_deref(),
            Some(crate::run_scope::Unattributed::UserAction.code()),
            "it must say why it has no run rather than leaving the column blank"
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The counter-test: an allowed fetch must leave no record. Without it, "record
    /// everything unconditionally" would pass the test above.
    #[test]
    fn an_allowed_fetch_records_nothing() {
        let _serialized = crate::denial_sink::test_lock();
        let directory = std::env::temp_dir().join(format!("lm-web-ok-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("creates the directory");
        let path = directory.join(crate::denial_sink::SINK_FILE);
        crate::denial_sink::install(
            crate::denial_sink::DenialSink::open(&path).expect("the sink opens"),
        );

        validate_fetch_url(&url("http://93.184.216.34/"), false).expect("a public literal passes");

        let reader = crate::denial_sink::DenialSink::open(&path).expect("reopens for reading");
        let mine = reader
            .recent(64)
            .expect("reads")
            .into_iter()
            .filter(|row| row.detail.as_deref() == Some("93.184.216.34"))
            .count();
        assert_eq!(mine, 0, "nothing was refused, so nothing may be recorded");

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Proves this guard *delegates* NAT64 to its own v4 rule. `64:ff9b::7f00:1` is
    /// `127.0.0.1` wherever a NAT64/CLAT path exists, and this guard — the one the
    /// agent's own fetch tool goes through — read it as an ordinary public address.
    ///
    /// The public row is the load-bearing counter-test: refusing the whole prefix would
    /// satisfy every other row and break a v6-only network reaching a v4-only host.
    /// The two DNS rules in this file used to disagree on the quantifier, and the
    /// disagreement was this pre-check over-blocking rather than the resolver
    /// under-blocking.
    ///
    /// `SsrfGuardedResolver` prunes blocked answers and hands `reqwest` only the
    /// survivors — which are exactly what it connects to, so a pruned private answer is
    /// never dialled. This gate refused the whole request if *any* answer was blocked,
    /// so an ordinary split-horizon or dual-stack host answering with one public and one
    /// private address was refused here while the resolver would have connected safely.
    ///
    /// Tested through the extracted quantifier rather than the system resolver: the
    /// mixed case is the entire point and no hermetic hostname produces it.
    #[test]
    fn a_hostname_is_refused_only_when_no_answer_survives() {
        use std::str::FromStr;
        let ip = |text: &str| IpAddr::from_str(text).expect("parses");

        // The case that was wrongly refused. Order both ways, because a loop that
        // returns early on the first blocked answer passes one order and fails the
        // other — which is precisely the bug.
        for answers in [
            vec![ip("93.184.216.34"), ip("10.0.0.1")],
            vec![ip("10.0.0.1"), ip("93.184.216.34")],
        ] {
            assert!(
                classify_resolved_answers("dual.example", answers.into_iter()).is_ok(),
                "a host with one reachable answer must not be refused for the others"
            );
        }

        // The counter-test, without which "allow everything" passes: every answer
        // blocked is still refused, and the refusal names a rule rather than a blank.
        let denial = classify_resolved_answers(
            "private.example",
            vec![ip("10.0.0.1"), ip("127.0.0.1")].into_iter(),
        )
        .expect_err("a host whose every answer is refused must be refused");
        assert!(
            matches!(denial.rule(), EgressRule::PrivateV4 | EgressRule::Loopback),
            "the refusal must name one of the rules that fired: {denial}"
        );
        assert!(
            denial
                .detail()
                .is_some_and(|detail| detail.contains("private.example resolves to")),
            "the refusal must name the answer it refused: {denial}"
        );

        // An empty answer list is a different fact from "everything was refused", and
        // the resolver draws the same distinction.
        assert_eq!(
            classify_resolved_answers("empty.example", std::iter::empty())
                .expect_err("no answers is not a pass")
                .rule(),
            EgressRule::DnsNoAddresses
        );
    }

    #[test]
    fn nat64_reaches_this_guards_own_ipv4_rule() {
        use std::str::FromStr;
        for (text, expected) in [
            ("64:ff9b::7f00:1", Some(EgressRule::Loopback)),
            ("64:ff9b::a00:1", Some(EgressRule::PrivateV4)),
            ("64:ff9b::c0a8:101", Some(EgressRule::PrivateV4)),
            ("64:ff9b::a9fe:a9fe", Some(EgressRule::LinkLocal)),
            ("64:ff9b::", Some(EgressRule::Unspecified)),
            // The divergence proof, and the reason this delegates rather than
            // importing a shared blocklist: `100.64.0.1` is CGNAT, which only the
            // broadest guard refuses. This one must still allow it, in either
            // spelling — a shared blocklist would refuse it here and newly break
            // Tailscale users.
            ("64:ff9b::6440:1", None),
            ("64:ff9b::5db8:d822", None),
        ] {
            assert_eq!(
                blocked_reason_ipv6(&Ipv6Addr::from_str(text).expect("parses")),
                expected,
                "{text} must report whichever v4 rule its embedded address trips"
            );
        }
    }

    #[test]
    fn the_deprecated_ipv4_compatible_form_cannot_smuggle_loopback_past_this_guard() {
        use std::str::FromStr;
        // Named as its own rule rather than as loopback/private/link-local: the
        // whole range is refused, so what it wraps never gets consulted.
        for text in ["::127.0.0.1", "::10.0.0.1", "::169.254.1.1"] {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert_eq!(
                blocked_reason_ipv6(&address),
                Some(EgressRule::Ipv4Compatible),
                "{text} must be refused as the deprecated compatible form"
            );
        }
        // Counter-test: a real public v6 is still reachable, so this did not just
        // block everything.
        assert_eq!(
            blocked_reason_ipv6(&Ipv6Addr::from_str("2606:2800:220:1:248:1893:25c8:1946").unwrap()),
            None
        );
        // And the mapped form still works, which is the branch that already
        // existed — reporting the inner address's own rule, not a wrapper rule.
        assert_eq!(
            blocked_reason_ipv6(&Ipv6Addr::from_str("::ffff:127.0.0.1").unwrap()),
            Some(EgressRule::Loopback)
        );
    }

    #[test]
    fn rejects_non_http_schemes() {
        assert_eq!(
            refusing_rule("file:///etc/passwd"),
            EgressRule::SchemeNotAllowed
        );
    }

    /// The one refusal whose message must not quote what it refused, because the
    /// URL is the credential. Asserted on the rendered string as well as the
    /// rule, since the leak would be in the rendering.
    #[test]
    fn rejects_embedded_credentials_without_echoing_them() {
        let target = url("http://user:hunter2@example.com/");
        let denial = validate_fetch_url(&target, false).expect_err("expected a refusal");
        assert_eq!(denial.rule(), EgressRule::EmbeddedCredentials);

        let rendered = fetch_refusal(&target, &denial);
        assert!(
            !rendered.contains("hunter2") && !rendered.contains("user:"),
            "the refusal printed the credential it was refusing: {rendered}"
        );
        assert!(rendered.contains("egress.embedded-credentials"));
    }

    #[test]
    fn rejects_loopback_ipv4_literal() {
        assert_eq!(
            refusing_rule("http://127.0.0.1:8090/v1/chat"),
            EgressRule::Loopback
        );
    }

    /// Both spellings of this machine's own Ollama, and both are refused for
    /// being loopback rather than for the port — nothing here inspects a port.
    #[test]
    fn rejects_ollama_loopback_port() {
        assert_eq!(
            refusing_rule("http://127.0.0.1:11434/api/tags"),
            EgressRule::Loopback
        );
        assert_eq!(
            refusing_rule("http://localhost:11434/api/tags"),
            EgressRule::Loopback
        );
    }

    /// RFC1918 and link-local are different rules, and this now proves which is
    /// which: `169.254.1.1` used to be listed among the "private" ranges here
    /// purely because one message covered both.
    #[test]
    fn rejects_each_private_ipv4_range() {
        for (host, expected) in [
            ("10.1.2.3", EgressRule::PrivateV4),
            ("172.16.0.1", EgressRule::PrivateV4),
            ("172.31.255.255", EgressRule::PrivateV4),
            ("192.168.1.1", EgressRule::PrivateV4),
            ("169.254.1.1", EgressRule::LinkLocal),
        ] {
            assert_eq!(
                refusing_rule(&format!("http://{host}/")),
                expected,
                "wrong rule for {host}"
            );
        }
    }

    #[test]
    fn rejects_ipv6_loopback_unique_local_and_link_local() {
        for (host, expected) in [
            ("[::1]", EgressRule::Loopback),
            ("[fc00::1]", EgressRule::UniqueLocalV6),
            ("[fd12:3456:789a::1]", EgressRule::UniqueLocalV6),
            ("[fe80::1]", EgressRule::LinkLocal),
        ] {
            assert_eq!(
                refusing_rule(&format!("http://{host}/")),
                expected,
                "wrong rule for {host}"
            );
        }
    }

    #[test]
    fn rejects_ipv4_mapped_ipv6_private_address() {
        assert_eq!(
            refusing_rule("http://[::ffff:127.0.0.1]/"),
            EgressRule::Loopback
        );
    }

    /// `0.0.0.0`/`::` are neither loopback, private, nor link-local by any of
    /// std's own predicates, yet the OS routes an outbound connection to
    /// `0.0.0.0` to `127.0.0.1` — a real path to a loopback-bound service
    /// like `llama-server`/Ollama, not a dead address. Must be rejected the
    /// same as a literal `127.0.0.1`, and now says so under its own name rather
    /// than borrowing loopback's.
    #[test]
    fn rejects_unspecified_ipv4_and_ipv6() {
        for target in ["http://0.0.0.0:8090/v1/chat", "http://[::]:11434/api/tags"] {
            assert_eq!(refusing_rule(target), EgressRule::Unspecified, "{target}");
        }
    }

    #[test]
    fn rejects_ipv4_mapped_unspecified_ipv6_address() {
        assert_eq!(
            refusing_rule("http://[::ffff:0.0.0.0]/"),
            EgressRule::Unspecified
        );
    }

    #[tokio::test]
    async fn fetch_impl_rejects_the_unspecified_address() {
        let err = fetch_impl(
            &WebSettings::default(),
            "http://0.0.0.0:8090/".to_string(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains(EgressRule::Unspecified.code()),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn accepts_public_ipv4_and_ipv6_literals() {
        assert!(validate_fetch_url(&url("http://93.184.216.34/"), false).is_ok());
        assert!(
            validate_fetch_url(&url("http://[2606:2800:220:1:248:1893:25c8:1946]/"), false).is_ok()
        );
    }

    /// The whole inventory this guard blocks, one address per rule, in one place.
    ///
    /// Two things it pins that the per-case tests above cannot. First, that these
    /// rules are the *only* ones this guard reports — it is the narrowest of the
    /// four, and a reader should be able to see from here that CGNAT, multicast,
    /// broadcast, TEST-NET and `240/4` are **not** refused here even though
    /// sibling guards refuse them. Second, that no two classes report the same
    /// rule, which is the property that made the old single message unusable.
    #[test]
    fn the_inventory_of_rules_this_guard_reports_is_exactly_this() {
        use std::str::FromStr;

        let cases: &[(&str, EgressRule)] = &[
            ("127.0.0.1", EgressRule::Loopback),
            ("0.0.0.0", EgressRule::Unspecified),
            ("10.0.0.1", EgressRule::PrivateV4),
            ("172.16.0.1", EgressRule::PrivateV4),
            ("192.168.0.1", EgressRule::PrivateV4),
            ("169.254.0.1", EgressRule::LinkLocal),
        ];
        for (text, expected) in cases {
            let address = Ipv4Addr::from_str(text).expect("parses");
            assert_eq!(
                blocked_reason_ipv4(&address),
                Some(*expected),
                "wrong rule for {text}"
            );
        }

        let v6_cases: &[(&str, EgressRule)] = &[
            ("::1", EgressRule::Loopback),
            ("::", EgressRule::Unspecified),
            ("fc00::1", EgressRule::UniqueLocalV6),
            ("fe80::1", EgressRule::LinkLocal),
            ("::127.0.0.1", EgressRule::Ipv4Compatible),
            ("::ffff:10.0.0.1", EgressRule::PrivateV4),
        ];
        for (text, expected) in v6_cases {
            let address = Ipv6Addr::from_str(text).expect("parses");
            assert_eq!(
                blocked_reason_ipv6(&address),
                Some(*expected),
                "wrong rule for {text}"
            );
        }

        // The counter-half: classes the siblings refuse and this guard does not.
        // Listed rather than omitted so that widening this blocklist — which
        // `egress.rs`'s module doc explains was deliberately left undone, because
        // CGNAT is Tailscale's default range — has to change this test and be
        // argued for.
        for text in [
            "100.64.0.1",      // CGNAT
            "224.0.0.1",       // multicast
            "255.255.255.255", // broadcast
            "192.0.2.1",       // TEST-NET-1
            "240.0.0.1",       // reserved
            "93.184.216.34",   // genuinely public
        ] {
            let address = Ipv4Addr::from_str(text).expect("parses");
            assert_eq!(
                blocked_reason_ipv4(&address),
                None,
                "{text} is not refused by this guard today; widening it is a \
                 deliberate change, not a test fix"
            );
        }
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
        assert!(
            !truncated,
            "window covering exactly the tail must not be marked truncated"
        );

        let (window, _total, truncated) = char_window(content, 0, 100);
        assert_eq!(window, content);
        assert!(
            !truncated,
            "window covering the whole content must not be marked truncated"
        );
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
        let (title, markdown) =
            dispatch_content("text/html; charset=utf-8", html, "https://example.com/").unwrap();
        assert_eq!(title.as_deref(), Some("Hello World"));
        assert!(markdown.contains("Heading"));
        assert!(markdown.contains("Some text."));
    }

    #[tokio::test]
    async fn plain_text_content_types_pass_through_unchanged() {
        for ct in [
            "text/plain",
            "text/markdown",
            "application/json",
            "application/xml",
        ] {
            let (title, content) =
                dispatch_content(ct, "raw content", "https://example.com/").unwrap();
            assert!(title.is_none());
            assert_eq!(content, "raw content");
        }
    }

    #[tokio::test]
    async fn unsupported_content_type_is_rejected_by_name() {
        let err = dispatch_content("image/png", "binary", "https://example.com/").unwrap_err();
        assert!(err.contains("image/png"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn fetch_impl_rejects_a_disallowed_url_without_making_a_request() {
        let err = fetch_impl(
            &WebSettings::default(),
            "http://127.0.0.1:8090/".to_string(),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains(EgressRule::Loopback.code()),
            "unexpected error: {err}"
        );
    }

    /// The negative case, and the reason a rule code is worth more here than
    /// anywhere else. With `allow_local_network: true` the guard must let the
    /// target through *validation* and the request must then fail for an ordinary
    /// transport reason — nothing listens on port 1. Told apart from a policy
    /// block by the absence of any rule code at all, which is a claim about the
    /// whole class of refusals. The version this replaces asserted the absence of
    /// the substring `"local/private"`, so a policy block worded any other way —
    /// including every rule code introduced here — would have passed it while the
    /// guard was in fact still refusing.
    #[tokio::test]
    async fn fetch_impl_honors_settings_allow_local_network() {
        let settings = WebSettings {
            allow_local_network: true,
            ..WebSettings::default()
        };
        let err = fetch_impl(&settings, "http://127.0.0.1:1/".to_string(), None, None)
            .await
            .unwrap_err();
        assert!(
            !err.contains("[egress."),
            "no rule may have refused this: {err}"
        );
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

        let error = client
            .get(format!("http://{}/", addr))
            .send()
            .await
            .expect_err("expected the redirect to a private IP to be blocked");

        // `is_err()` alone would also pass on a bind failure, a connection reset,
        // or a redirect refused for the wrong reason. The rule is recovered from
        // the error chain instead, which is what passing the denial through
        // `io::Error` rather than `to_string()` buys.
        assert_eq!(
            denied_rule(&error),
            Some(EgressRule::PrivateV4),
            "the hop must be refused as a private IPv4 target: {error}"
        );
    }

    /// Walks a `reqwest::Error`'s source chain looking for an [`EgressDenial`].
    ///
    /// The chain is what makes this possible at all: the policy hands reqwest an
    /// `io::Error` whose inner error *is* the denial, and `reqwest::Error::source`
    /// hands that `io::Error` straight back, so the rule is still a value on the far
    /// side rather than a substring of `"Failed to fetch '…': …"`.
    ///
    /// # The `io::Error` hop has to be explicit
    ///
    /// `io::Error`'s own `source()` does **not** return its inner error — it
    /// delegates to that inner error's `source()`. So a plain `source()` walk
    /// reaches the `io::Error` and then steps over the payload it is carrying,
    /// arriving at `None`. `get_ref()` is the only way back to it. Worth writing
    /// down, because the first version of this helper was a plain walk and it
    /// silently found nothing on a request that had in fact been refused by rule.
    fn denied_rule(error: &(dyn std::error::Error + 'static)) -> Option<EgressRule> {
        let mut current = Some(error);
        while let Some(step) = current {
            if let Some(denial) = step.downcast_ref::<EgressDenial>() {
                return Some(denial.rule());
            }
            if let Some(denial) = step
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::get_ref)
                .and_then(|inner| inner.downcast_ref::<EgressDenial>())
            {
                return Some(denial.rule());
            }
            current = step.source();
        }
        None
    }

    /// `reqwest::redirect::Policy::custom` does not get a redirect-loop cap
    /// for free (its own doc comment says so explicitly), so without
    /// `MAX_REDIRECT_HOPS` a server that keeps 302ing (to itself, or to a
    /// chain of other otherwise-public hosts) would only ever be stopped by
    /// the whole-request `FETCH_TIMEOUT`. `allow_local_network: true` here is
    /// deliberate: this test is exercising the hop-count cap, not the SSRF
    /// filter (already covered by `redirect_hop_to_a_private_ip_is_blocked`
    /// above), so the SSRF check is opted out of the same way a real user
    /// enabling "allow local network" in Settings would, letting the
    /// loopback test server redirect to itself freely.
    #[tokio::test]
    async fn redirect_policy_caps_an_infinite_redirect_loop() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();
        let self_url = format!("http://{addr}/");

        std::thread::spawn(move || {
            // Keep answering every connection the same way for as long as
            // the (short-lived) test needs it to — each hop is a fresh
            // connection since the response sets `Connection: close`.
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 302 Found\r\nLocation: {self_url}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        let client = reqwest::Client::builder()
            .redirect(build_redirect_policy(true))
            .build()
            .unwrap();

        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            client.get(format!("http://{addr}/")).send(),
        )
        .await
        .expect(
            "the hop cap must stop the loop well within the 5s test timeout, not hang until it",
        );
        let elapsed = started.elapsed();

        let error = result.expect_err("expected the redirect loop to be capped");
        // Which cap stopped it, not merely that something did. The wall-clock
        // assertion below was previously the only evidence that the hop cap fired
        // rather than a timeout, and a proxy measurement is not the claim.
        assert_eq!(
            denied_rule(&error),
            Some(EgressRule::RedirectHopLimit),
            "the loop must be stopped by the hop cap: {error}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "expected the {MAX_REDIRECT_HOPS}-hop cap to stop the loop quickly, took {elapsed:?}"
        );
    }

    /// Exercises [`SsrfGuardedResolver`] directly (not through `fetch_impl`,
    /// which would also reject `localhost` via `validate_fetch_url`'s own
    /// pre-check) — this is the resolver installed as the `reqwest::Client`'s
    /// actual DNS resolver, i.e. the one whose output is what the real TCP
    /// connect uses, closing the check-then-connect DNS-rebinding gap the
    /// module doc describes.
    #[tokio::test]
    async fn ssrf_guarded_resolver_rejects_a_hostname_resolving_to_loopback() {
        use reqwest::dns::Resolve;

        let resolver = SsrfGuardedResolver {
            allow_local_network: false,
        };
        let name: reqwest::dns::Name = "localhost".parse().expect("valid dns name");
        // `Addrs` (the `Ok` type) is a boxed `dyn Iterator`, which isn't
        // `Debug` — so this matches manually rather than using
        // `Result::expect_err`, which requires the `Ok` type to be `Debug`.
        match resolver.resolve(name).await {
            Ok(_) => panic!("localhost must not resolve through the guarded resolver"),
            // Recovered as a value, not matched as prose: the resolver's signature
            // is `Box<dyn Error>`, so the denial travels as itself.
            Err(err) => assert_eq!(
                denied_rule(err.as_ref()),
                Some(EgressRule::Loopback),
                "unexpected error: {err}"
            ),
        }
    }

    #[tokio::test]
    async fn ssrf_guarded_resolver_allows_loopback_when_allow_local_network_is_set() {
        use reqwest::dns::Resolve;

        let resolver = SsrfGuardedResolver {
            allow_local_network: true,
        };
        let name: reqwest::dns::Name = "localhost".parse().expect("valid dns name");
        let addrs = resolver
            .resolve(name)
            .await
            .expect("allow_local_network must let loopback resolve");
        assert!(
            addrs.count() > 0,
            "expected at least one resolved address for localhost"
        );
    }

    #[tokio::test]
    async fn executable_extension_client_refuses_loopback_at_connect_time() {
        let client = executable_extension_http_client(Duration::from_secs(1))
            .expect("build executable-extension client");
        let error = client
            .get("http://localhost:1/")
            .send()
            .await
            .expect_err("guarded resolver must refuse loopback before connecting");

        assert_eq!(
            denied_rule(&error),
            Some(EgressRule::Loopback),
            "unexpected error: {error}"
        );
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
        assert_eq!(
            decode_ddg_href("https://rust-lang.org/"),
            "https://rust-lang.org/"
        );
    }

    #[test]
    fn decode_ddg_href_decodes_a_protocol_relative_uddg_redirect() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust_(programming_language)&rut=abc123";
        assert_eq!(
            decode_ddg_href(href),
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
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
        assert!(results[0]
            .snippet
            .contains("fast, reliable, and productive"));

        // The uddg-wrapped result: url must be the decoded destination, not
        // the duckduckgo.com redirect.
        assert_eq!(results[1].title, "Rust (programming language) - Wikipedia");
        assert_eq!(
            results[1].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
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
        assert_eq!(
            parse_ddg_results("<html><body><p>no results here</p></body></html>", 10),
            Vec::new()
        );
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

    // --- phase 3: settings, Brave, SearXNG -------------------------------

    fn temp_settings_path(name: &str) -> PathBuf {
        // Same unique-path idiom as `sessions.rs`/`mcp.rs`'s own test
        // helpers — an atomic counter plus nanos so parallel test threads
        // (and repeated runs within the same nanosecond) never collide.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_web_settings_test_{}_{}_{}_{}",
            std::process::id(),
            n,
            nanos,
            name
        ))
    }

    #[test]
    fn search_provider_serializes_to_the_expected_strings() {
        assert_eq!(
            serde_json::to_string(&SearchProvider::Duckduckgo).unwrap(),
            "\"duckduckgo\""
        );
        assert_eq!(
            serde_json::to_string(&SearchProvider::Brave).unwrap(),
            "\"brave\""
        );
        assert_eq!(
            serde_json::to_string(&SearchProvider::Searxng).unwrap(),
            "\"searxng\""
        );
        assert_eq!(
            serde_json::to_string(&SearchProvider::ExecutableExtension).unwrap(),
            "\"executable_extension\""
        );

        assert_eq!(
            serde_json::from_str::<SearchProvider>("\"duckduckgo\"").unwrap(),
            SearchProvider::Duckduckgo
        );
        assert_eq!(
            serde_json::from_str::<SearchProvider>("\"brave\"").unwrap(),
            SearchProvider::Brave
        );
        assert_eq!(
            serde_json::from_str::<SearchProvider>("\"searxng\"").unwrap(),
            SearchProvider::Searxng
        );
        assert_eq!(
            serde_json::from_str::<SearchProvider>("\"executable_extension\"").unwrap(),
            SearchProvider::ExecutableExtension
        );
    }

    /// `providers.rs::slugify` only ever produces lowercase alphanumerics and
    /// dashes (never a colon), so any keychain account prefixed with a colon
    /// segment is structurally unreachable from a custom-provider label —
    /// same reasoning as `mcp.rs`'s `keychain_account_namespaces_by_mcp_prefix`
    /// test, applied to this module's own Brave-key account.
    #[test]
    fn brave_keychain_account_is_namespaced_away_from_provider_slugs() {
        assert!(
            BRAVE_KEYCHAIN_ACCOUNT.contains(':'),
            "expected a namespaced account name, got {BRAVE_KEYCHAIN_ACCOUNT}"
        );
        // A custom LLM provider labeled "Search Brave" (or any casing/
        // punctuation variant) slugifies to exactly this — the collision the
        // namespace prefix must rule out.
        assert_ne!(BRAVE_KEYCHAIN_ACCOUNT, "search-brave");
    }

    #[test]
    fn web_settings_default_matches_the_design_docs_defaults() {
        let settings = WebSettings::default();
        assert_eq!(settings.search_provider, SearchProvider::Duckduckgo);
        assert_eq!(settings.search_extension_id, None);
        assert_eq!(settings.search_extension_capability_id, None);
        assert_eq!(settings.searxng_base_url, None);
        assert!(!settings.allow_local_network);
        assert_eq!(settings.fetch_max_chars, DEFAULT_MAX_CHARS);
        assert_eq!(settings.fetch_provider, FetchProvider::Builtin);
        assert_eq!(settings.fetch_extension_id, None);
        assert_eq!(settings.fetch_extension_capability_id, None);
    }

    #[test]
    fn load_settings_returns_default_when_file_missing() {
        let path = temp_settings_path("missing.json");
        assert_eq!(load_settings_impl(&path).unwrap(), WebSettings::default());
    }

    #[test]
    fn settings_save_then_load_roundtrips() {
        let path = temp_settings_path("roundtrip.json");
        let settings = WebSettings {
            search_provider: SearchProvider::Searxng,
            search_extension_id: Some("dev.example.search".to_string()),
            search_extension_capability_id: Some("private-search".to_string()),
            searxng_base_url: Some("https://searx.example.com".to_string()),
            allow_local_network: true,
            fetch_max_chars: 50_000,
            fetch_provider: FetchProvider::ExecutableExtension,
            fetch_extension_id: Some("dev.example.fetch".to_string()),
            fetch_extension_capability_id: Some("private-fetch".to_string()),
        };
        save_settings_impl(&path, &settings).unwrap();
        assert_eq!(load_settings_impl(&path).unwrap(), settings);
        // The temp file must not linger after a successful save (atomic
        // temp+rename, same as `sessions.rs`/`mcp.rs`).
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn legacy_settings_default_the_executable_provider_fields() {
        let settings: WebSettings = serde_json::from_str(
            r#"{"search_provider":"duckduckgo","searxng_base_url":null,"allow_local_network":false,"fetch_max_chars":20000}"#,
        )
        .unwrap();
        assert_eq!(settings, WebSettings::default());
    }

    #[test]
    fn settings_save_overwrites_previous_content() {
        let path = temp_settings_path("overwrite.json");
        save_settings_impl(
            &path,
            &WebSettings {
                fetch_max_chars: 1_000,
                ..WebSettings::default()
            },
        )
        .unwrap();
        save_settings_impl(
            &path,
            &WebSettings {
                fetch_max_chars: 2_000,
                ..WebSettings::default()
            },
        )
        .unwrap();
        assert_eq!(load_settings_impl(&path).unwrap().fetch_max_chars, 2_000);
        let _ = std::fs::remove_file(&path);
    }

    /// Reproduces (and proves fixed) the concurrent-save corruption
    /// `AppState::web_settings_lock` exists to prevent: without a lock
    /// serializing the load-then-write-then-rename cycle, two threads
    /// `std::fs::write`-ing the same deterministic `.json.tmp` path at once
    /// can interleave, leaving a torn/unparseable file. Driven at high
    /// repetition (not just once) since the race is timing-dependent and a
    /// single iteration could pass by luck even with the lock removed.
    #[test]
    fn concurrent_saves_serialized_by_web_settings_lock_never_corrupt_the_file() {
        let path = std::sync::Arc::new(temp_settings_path("concurrent.json"));
        let state = std::sync::Arc::new(crate::AppState::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let make_worker = |fetch_max_chars: usize| {
            let path = path.clone();
            let state = state.clone();
            let barrier = barrier.clone();
            move || {
                barrier.wait();
                for _ in 0..200 {
                    let settings = WebSettings {
                        fetch_max_chars,
                        ..WebSettings::default()
                    };
                    let _guard = state.web_settings_lock.lock().unwrap();
                    save_settings_impl(&path, &settings).unwrap();
                }
            }
        };

        let handle_a = std::thread::spawn(make_worker(1_000));
        let handle_b = std::thread::spawn(make_worker(2_000));
        handle_a.join().unwrap();
        handle_b.join().unwrap();

        // The file must always parse cleanly (never torn/interleaved) and
        // hold exactly one writer's complete settings, never a mix of both.
        let loaded = load_settings_impl(&path)
            .expect("serialized concurrent saves must never corrupt the file");
        assert!(
            loaded.fetch_max_chars == 1_000 || loaded.fetch_max_chars == 2_000,
            "unexpected fetch_max_chars: {}",
            loaded.fetch_max_chars
        );
        let _ = std::fs::remove_file(path.as_path());
    }

    #[test]
    fn normalize_and_validate_settings_rejects_a_zero_fetch_max_chars() {
        let err = normalize_and_validate_settings(WebSettings {
            fetch_max_chars: 0,
            ..WebSettings::default()
        })
        .unwrap_err();
        assert!(err.contains("fetch_max_chars"), "unexpected error: {err}");
    }

    #[test]
    fn normalize_and_validate_settings_blanks_a_whitespace_only_searxng_url_to_none() {
        let settings = WebSettings {
            searxng_base_url: Some("   ".to_string()),
            ..WebSettings::default()
        };
        let normalized = normalize_and_validate_settings(settings).unwrap();
        assert_eq!(normalized.searxng_base_url, None);
    }

    #[test]
    fn normalize_and_validate_settings_normalizes_a_trailing_slash_on_the_searxng_url() {
        let settings = WebSettings {
            searxng_base_url: Some("https://searx.example.com/".to_string()),
            ..WebSettings::default()
        };
        let normalized = normalize_and_validate_settings(settings).unwrap();
        assert_eq!(
            normalized.searxng_base_url.as_deref(),
            Some("https://searx.example.com")
        );
    }

    #[test]
    fn normalize_and_validate_settings_rejects_a_non_http_searxng_url() {
        let settings = WebSettings {
            searxng_base_url: Some("ftp://searx.example.com".to_string()),
            ..WebSettings::default()
        };
        let err = normalize_and_validate_settings(settings).unwrap_err();
        assert!(err.contains("http"), "unexpected error: {err}");
    }

    #[test]
    fn executable_provider_settings_require_bounded_capability_ids() {
        let missing = normalize_and_validate_settings(WebSettings {
            search_provider: SearchProvider::ExecutableExtension,
            ..WebSettings::default()
        })
        .unwrap_err();
        assert!(missing.contains("search_extension_capability_id"));

        let owner_missing = normalize_and_validate_settings(WebSettings {
            search_provider: SearchProvider::ExecutableExtension,
            search_extension_capability_id: Some("private-search".to_string()),
            ..WebSettings::default()
        })
        .unwrap_err();
        assert!(owner_missing.contains("search_extension_id"));

        let normalized = normalize_and_validate_settings(WebSettings {
            search_provider: SearchProvider::ExecutableExtension,
            search_extension_id: Some("  dev.example.search  ".to_string()),
            search_extension_capability_id: Some("  private-search  ".to_string()),
            fetch_provider: FetchProvider::ExecutableExtension,
            fetch_extension_id: Some("dev.example.fetch".to_string()),
            fetch_extension_capability_id: Some("private-fetch:v1".to_string()),
            ..WebSettings::default()
        })
        .unwrap();
        assert_eq!(
            normalized.search_extension_id.as_deref(),
            Some("dev.example.search")
        );
        assert_eq!(
            normalized.search_extension_capability_id.as_deref(),
            Some("private-search")
        );
        assert_eq!(
            normalized.fetch_extension_id.as_deref(),
            Some("dev.example.fetch")
        );
        assert_eq!(
            normalized.fetch_extension_capability_id.as_deref(),
            Some("private-fetch:v1")
        );
    }

    #[test]
    fn extension_calls_use_deterministic_bounded_ids_and_exact_typed_json() {
        let settings = WebSettings {
            fetch_provider: FetchProvider::ExecutableExtension,
            fetch_extension_id: Some("dev.example.fetch".to_string()),
            fetch_extension_capability_id: Some("private-fetch".to_string()),
            search_provider: SearchProvider::ExecutableExtension,
            search_extension_id: Some("dev.example.search".to_string()),
            search_extension_capability_id: Some("private-search".to_string()),
            ..WebSettings::default()
        };
        let first = extension_fetch_call(
            &settings,
            "runtime-call-17",
            "https://example.com/page",
            Some(120),
            Some(40),
        )
        .unwrap()
        .unwrap();
        let repeated = extension_fetch_call(
            &settings,
            "runtime-call-17",
            "https://example.com/page",
            Some(120),
            Some(40),
        )
        .unwrap()
        .unwrap();
        let next_call = extension_fetch_call(
            &settings,
            "runtime-call-18",
            "https://example.com/page",
            Some(120),
            Some(40),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            first.input_json,
            r#"{"url":"https://example.com/page","max_chars":120,"start_index":40}"#
        );
        assert_eq!(first.invocation_id, repeated.invocation_id);
        assert_ne!(first.invocation_id, next_call.invocation_id);
        assert_eq!(first.extension_id, "dev.example.fetch");
        assert!(first.invocation_id.starts_with("web-fetch-"));
        assert!(first.invocation_id.len() <= 160);

        let mut replacement_owner = settings.clone();
        replacement_owner.fetch_extension_id = Some("dev.example.replacement".to_string());
        let replaced = extension_fetch_call(
            &replacement_owner,
            "runtime-call-17",
            "https://example.com/page",
            Some(120),
            Some(40),
        )
        .unwrap()
        .unwrap();
        assert_ne!(first.invocation_id, replaced.invocation_id);

        let search = extension_search_call(&settings, "runtime-call-17", "rust", Some(99))
            .unwrap()
            .unwrap();
        assert_eq!(search.input_json, r#"{"query":"rust","count":10}"#);
        assert!(search.invocation_id.starts_with("web-search-"));
        assert!(
            extension_fetch_call(&settings, " ", "https://example.com/page", None, None,)
                .unwrap_err()
                .contains("trusted runtime call id")
        );
    }

    #[tokio::test]
    async fn executable_providers_reject_legacy_calls_without_runtime_identity() {
        let settings = WebSettings {
            fetch_provider: FetchProvider::ExecutableExtension,
            fetch_extension_id: Some("dev.example.fetch".to_string()),
            fetch_extension_capability_id: Some("private-fetch".to_string()),
            search_provider: SearchProvider::ExecutableExtension,
            search_extension_id: Some("dev.example.search".to_string()),
            search_extension_capability_id: Some("private-search".to_string()),
            ..WebSettings::default()
        };
        assert!(
            fetch_impl(&settings, "https://example.com".to_string(), None, None,)
                .await
                .unwrap_err()
                .contains("trusted runtime call id")
        );
        assert!(search_impl(&settings, None, "rust".to_string(), None)
            .await
            .unwrap_err()
            .contains("trusted runtime call id"));
    }

    #[test]
    fn extension_search_output_is_strict_and_bounded() {
        let valid =
            r#"[{"title":"Rust","url":"https://www.rust-lang.org/","snippet":"A language."}]"#;
        assert_eq!(parse_extension_search_output(valid, 1).unwrap().len(), 1);

        let too_many = format!("[{0},{0}]", &valid[1..valid.len() - 1]);
        assert!(parse_extension_search_output(&too_many, 1)
            .unwrap_err()
            .contains("more results"));
        assert!(parse_extension_search_output(
            r#"[{"title":"Bad","url":"file:///etc/passwd","snippet":"no"}]"#,
            1,
        )
        .unwrap_err()
        .contains("http"));
        assert!(parse_extension_search_output(
            r#"[{"title":"Bad","url":"https://example.com","snippet":"no","extra":true}]"#,
            1,
        )
        .unwrap_err()
        .contains("normalized JSON"));
    }

    #[test]
    fn extension_fetch_output_matches_the_requested_window_exactly() {
        let result = FetchResult {
            url: "https://93.184.216.34/page".to_string(),
            final_url: "https://93.184.216.34/final".to_string(),
            title: Some("Example".to_string()),
            content_type: "text/html".to_string(),
            markdown: "hello".to_string(),
            total_chars: 11,
            truncated: true,
        };
        let output = serde_json::to_string(&result).unwrap();
        assert_eq!(
            parse_extension_fetch_output(&output, &result.url, 5, 0, false).unwrap(),
            result
        );

        let mut local_final_url = result.clone();
        local_final_url.final_url = "http://127.0.0.1/private".to_string();
        assert!(parse_extension_fetch_output(
            &serde_json::to_string(&local_final_url).unwrap(),
            &local_final_url.url,
            5,
            0,
            false,
        )
        .unwrap_err()
        .contains(EgressRule::Loopback.code()));

        let inconsistent = output.replace("\"truncated\":true", "\"truncated\":false");
        assert!(parse_extension_fetch_output(
            &inconsistent,
            "https://93.184.216.34/page",
            5,
            0,
            false,
        )
        .unwrap_err()
        .contains("window metadata"));
        assert!(
            parse_extension_fetch_output(&output, "https://other.example/page", 5, 0, false,)
                .unwrap_err()
                .contains("requested URL")
        );
    }

    /// Brave's actual response shape (`web.results[].{title,url,description}`)
    /// per the design doc / task spec — trimmed to two results.
    const BRAVE_FIXTURE_JSON: &str = r#"{
        "web": {
            "results": [
                { "title": "Rust Programming Language", "url": "https://www.rust-lang.org/", "description": "A language empowering everyone." },
                { "title": "Rust (programming language) - Wikipedia", "url": "https://en.wikipedia.org/wiki/Rust_(programming_language)", "description": "A general-purpose systems programming language." }
            ]
        }
    }"#;

    #[test]
    fn parse_brave_response_extracts_title_url_and_snippet() {
        let results = parse_brave_response(BRAVE_FIXTURE_JSON, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            SearchResult {
                title: "Rust Programming Language".to_string(),
                url: "https://www.rust-lang.org/".to_string(),
                snippet: "A language empowering everyone.".to_string(),
            }
        );
        assert_eq!(results[1].title, "Rust (programming language) - Wikipedia");
    }

    #[test]
    fn parse_brave_response_respects_the_count_cap() {
        let results = parse_brave_response(BRAVE_FIXTURE_JSON, 1).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn parse_brave_response_tolerates_a_missing_web_key() {
        // A query with zero results omits `web` entirely rather than sending
        // an empty `results` array — must yield an empty list, not an error.
        let results = parse_brave_response(r#"{"query": {"original": "asdf"}}"#, 10).unwrap();
        assert_eq!(results, Vec::new());
    }

    #[test]
    fn parse_brave_response_rejects_unparseable_json() {
        assert!(parse_brave_response("not json", 10).is_err());
    }

    /// SearXNG's `format=json` response shape (`results[].{title,url,content}`)
    /// per the design doc.
    const SEARXNG_FIXTURE_JSON: &str = r#"{
        "query": "rust",
        "results": [
            { "title": "Rust Programming Language", "url": "https://www.rust-lang.org/", "content": "A language empowering everyone." },
            { "title": "The Rust Book", "url": "https://doc.rust-lang.org/book/", "content": "The official book on Rust." }
        ]
    }"#;

    #[test]
    fn parse_searxng_response_extracts_title_url_and_snippet() {
        let results = parse_searxng_response(SEARXNG_FIXTURE_JSON, 10).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            SearchResult {
                title: "Rust Programming Language".to_string(),
                url: "https://www.rust-lang.org/".to_string(),
                snippet: "A language empowering everyone.".to_string(),
            }
        );
        assert_eq!(results[1].title, "The Rust Book");
    }

    #[test]
    fn parse_searxng_response_respects_the_count_cap() {
        let results = parse_searxng_response(SEARXNG_FIXTURE_JSON, 1).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn parse_searxng_response_rejects_unparseable_json() {
        assert!(parse_searxng_response("not json", 10).is_err());
    }

    /// Spins up a tiny local HTTP server that always answers with `status`
    /// and `body`, for exercising the HTTP-error paths of `brave_search_at`/
    /// `searxng_search` without a live network call — same one-shot
    /// `TcpListener` idiom `redirect_hop_to_a_private_ip_is_blocked` (above)
    /// already uses for the redirect-policy test.
    fn spawn_fixed_response_server(status_line: &'static str, body: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// Exercises `brave_search_at`'s real HTTP-error handling (not just the
    /// pure parser) — a bad/revoked API key gets a `401` from the real Brave
    /// API, and this must surface as an `Err` naming the status rather than
    /// panicking on the (non-JSON, in this fixture) error body. This is the
    /// "reject a bad key" behavior `web_set_brave_key` relies on to validate
    /// before ever touching the keychain — driven here at the HTTP layer
    /// (not via `web_set_brave_key` itself) so the test never touches the
    /// developer's real OS keychain.
    #[tokio::test]
    async fn brave_search_at_surfaces_an_unauthorized_response_as_an_error() {
        let base =
            spawn_fixed_response_server("401 Unauthorized", "{\"message\":\"Invalid API key\"}");
        let err = brave_search_at(&base, "a-bad-key", "test", 1)
            .await
            .unwrap_err();
        assert!(err.contains("401"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn searxng_search_maps_403_to_an_actionable_hint() {
        let base = spawn_fixed_response_server("403 Forbidden", "");
        let err = searxng_search(&base, "rust", 10).await.unwrap_err();
        assert!(
            err.contains("formats") && err.contains("settings.yml"),
            "unexpected error: {err}"
        );
    }

    /// Spawns a local server that answers its first request with a `302 Found`
    /// pointing at `location`, and any later request with `200 OK` carrying
    /// `body`.
    ///
    /// Serves two requests rather than one because the same-origin counter-test
    /// needs the redirect actually followed to something that answers; the
    /// cross-origin tests only ever reach the first.
    fn spawn_redirecting_server(location: String, body: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            for hop in 0..2 {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = if hop == 0 {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                } else {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// Spawns a local server that records the raw text of the first request it
    /// receives, then answers an empty `200 OK`.
    ///
    /// The recording is the assertion: anything that reached this origin shows up
    /// as `Some(request)`, headers included.
    fn spawn_recording_server() -> (String, std::sync::Arc<std::sync::Mutex<Option<String>>>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let recorder = std::sync::Arc::clone(&seen);

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).unwrap_or(0);
                *recorder.lock().unwrap() =
                    Some(String::from_utf8_lossy(&buf[..read]).into_owned());
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });

        (format!("http://{}", addr), seen)
    }

    /// The property [`search_client`] exists for, driven through the one search
    /// backend that carries a secret: a `302` off Brave's endpoint must not walk
    /// `X-Subscription-Token` to whatever host the redirect names.
    ///
    /// Asserted on the *victim* server rather than on the error message, because
    /// the question is not what the user was told — it is whether the key left
    /// for an address the response picked. reqwest strips `Authorization` across
    /// an origin change but not this header, so before `search_client` the
    /// recording below captured the key verbatim.
    #[tokio::test]
    async fn a_302_does_not_carry_the_brave_key_to_another_origin() {
        let (victim, seen) = spawn_recording_server();
        let attacker = spawn_redirecting_server(format!("{victim}/steal"), "");

        let err = brave_search_at(&attacker, "super-secret-key", "rust", 1)
            .await
            .expect_err("a cross-origin redirect must fail the whole request");

        let captured = seen.lock().unwrap().clone();
        assert!(
            captured.is_none(),
            "the redirect target was contacted at all: {captured:?}"
        );
        assert!(
            !err.contains("super-secret-key"),
            "the key must not reach the error text either: {err}"
        );
    }

    /// The refusal is attributable by rule, not an anonymous transport failure —
    /// which is what puts it in the denial sink under `egress.redirect-cross-origin`
    /// rather than as a nameless "failed to search".
    ///
    /// Driven against the client instead of a backend because the backends return
    /// `Result<_, String>`, and the `format!` at that boundary is where the error
    /// chain carrying the rule ends.
    #[tokio::test]
    async fn the_search_client_names_the_rule_that_refused_a_cross_origin_hop() {
        let (victim, _seen) = spawn_recording_server();
        let attacker = spawn_redirecting_server(format!("{victim}/steal"), "");

        let error = search_client()
            .expect("build the search client")
            .get(format!("{attacker}/search"))
            .send()
            .await
            .expect_err("expected the cross-origin redirect to be refused");

        assert_eq!(
            denied_rule(&error),
            Some(EgressRule::RedirectCrossOrigin),
            "unexpected refusal: {error}"
        );
    }

    /// The counter-test that keeps the two above honest: a policy of "refuse
    /// every redirect" passes both, and would also break the SearXNG instances
    /// that redirect `/search` internally. A hop that stays on the configured
    /// origin has to still be followed all the way to its results.
    #[tokio::test]
    async fn a_302_that_stays_on_the_searxng_origin_is_still_followed() {
        let base = spawn_redirecting_server(
            "/search?q=rust&format=json".to_string(),
            SEARXNG_FIXTURE_JSON,
        );

        let results = searxng_search(&base, "rust", 10)
            .await
            .expect("a same-origin redirect must be followed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust Programming Language");
    }

    /// `read_body_capped` stops at its cap against a peer that keeps writing —
    /// the property `.text()` cannot offer, since it reads to end-of-stream and
    /// so lets the peer size the allocation.
    ///
    /// The declared `Content-Length` is deliberately a lie about a body this
    /// server has no intention of finishing, because an honest length is the case
    /// that never needed a cap.
    #[tokio::test]
    async fn read_body_capped_stops_at_its_cap() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                if stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 104857600\r\nConnection: close\r\n\r\n",
                    )
                    .is_err()
                {
                    return;
                }
                let chunk = vec![b'x'; 64 * 1024];
                // Bounded so a reader that never stops reading cannot leave this
                // thread spinning; 1600 × 64 KiB is the declared length.
                for _ in 0..1600 {
                    if stream.write_all(&chunk).is_err() {
                        return;
                    }
                }
            }
        });

        let response = search_client()
            .expect("build the search client")
            .get(format!("http://{}/", addr))
            .send()
            .await
            .expect("the fixture server answers");

        let bytes = read_body_capped(response, 4096)
            .await
            .expect("read the capped body");

        assert_eq!(bytes.len(), 4096);
    }

    #[tokio::test]
    async fn search_impl_rejects_brave_without_a_key() {
        let settings = WebSettings {
            search_provider: SearchProvider::Brave,
            ..WebSettings::default()
        };
        let err = search_impl(&settings, None, "rust".to_string(), None)
            .await
            .unwrap_err();
        assert!(err.contains("API key"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn search_impl_rejects_searxng_without_a_base_url() {
        let settings = WebSettings {
            search_provider: SearchProvider::Searxng,
            ..WebSettings::default()
        };
        let err = search_impl(&settings, None, "rust".to_string(), None)
            .await
            .unwrap_err();
        assert!(err.contains("base URL"), "unexpected error: {err}");
    }

    // --- phase 5: readability, pagination end-to-end, cancellation wiring ---

    /// A trimmed but structurally realistic news-article page: nav with a
    /// "Subscribe Now" link, a header ad slot, the actual article (multiple
    /// substantive paragraphs, comfortably over [`MIN_READABLE_CONTENT_CHARS`]),
    /// a footer, and a "Trending" sidebar of unrelated headlines — exactly the
    /// boilerplate-vs-content shape `dom_smoothie::Readability` exists to
    /// separate.
    const ARTICLE_FIXTURE_HTML: &str = r#"
        <html>
        <head><title>Local News Site</title></head>
        <body>
        <nav><ul><li><a href="/">Home</a></li><li><a href="/about">About</a></li><li><a href="/subscribe">Subscribe Now</a></li></ul></nav>
        <header><div class="logo">Daily Gazette</div><div class="ad">Advertisement: Buy Now and Save!</div></header>
        <article>
        <h1>Scientists Discover New Method For Sorting Data Efficiently</h1>
        <p>Researchers at a university announced today a novel sorting algorithm that outperforms existing methods on large datasets by a significant margin, according to a paper published this week in a peer-reviewed journal.</p>
        <p>The team, led by several computer scientists, spent three years developing and testing the approach across a wide variety of real-world workloads, finding consistent improvements in both raw speed and peak memory usage compared to the previous state of the art.</p>
        <p>Industry experts who reviewed the work have praised it, noting that such efficiency gains could meaningfully reduce computing costs for companies that process enormous volumes of data on a daily basis, from search engines to financial institutions.</p>
        <p>The researchers plan to release their implementation as open source software within the next few months, along with a detailed technical report describing the algorithm's design and the benchmarks used to evaluate it.</p>
        </article>
        <footer><p>Copyright 2026 Daily Gazette. All rights reserved. <a href="/privacy">Privacy Policy</a></p></footer>
        <div class="sidebar"><h3>Trending</h3><ul><li><a href="/story1">Local team wins championship game</a></li><li><a href="/story2">City council approves new budget</a></li></ul></div>
        </body>
        </html>
    "#;

    #[test]
    fn extract_readable_content_strips_boilerplate_from_an_article_shaped_page() {
        let (title, html) =
            extract_readable_content(ARTICLE_FIXTURE_HTML, "https://example.com/article");

        // Readability reports the page's own `<title>` here (not the `<h1>`) —
        // this fixture's title happens to equal the site name, so the useful
        // assertion is on the stripped *content*, not the title text.
        assert!(title.is_some());
        assert!(
            html.contains("novel sorting algorithm"),
            "article body missing from extracted content: {html}"
        );
        assert!(
            html.contains("open source software"),
            "article body missing from extracted content: {html}"
        );
        assert!(
            !html.contains("Subscribe Now"),
            "nav boilerplate leaked into extracted content: {html}"
        );
        assert!(
            !html.contains("Trending"),
            "sidebar boilerplate leaked into extracted content: {html}"
        );
        assert!(
            !html.contains("Buy Now and Save"),
            "header ad boilerplate leaked into extracted content: {html}"
        );
    }

    #[test]
    fn extract_readable_content_falls_back_to_the_raw_page_when_extraction_is_too_short() {
        // Well under MIN_READABLE_CONTENT_CHARS — Readability may still "succeed"
        // on a page this small, but the length gate must reject it and fall back
        // to the untouched body, same as a hard parse failure would.
        let html = "<html><head><title>Hi</title></head><body><p>Short.</p></body></html>";
        let (title, content) = extract_readable_content(html, "https://example.com/");
        assert_eq!(title.as_deref(), Some("Hi"));
        assert_eq!(content, html);
    }

    /// Spins up a one-shot local HTTP server that answers with a `Content-Type`
    /// header (so `dispatch_content`'s type dispatch is exercised for real,
    /// unlike [`spawn_fixed_response_server`] which never sets one) and the
    /// given `body`. `body` is owned (not `&'static str`) since the pagination
    /// test below generates its fixture content at runtime.
    fn spawn_content_response_server(content_type: &'static str, body: String) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// End-to-end proof that `start_index` pagination actually covers a large
    /// document without gap or overlap: the same content is fetched in two
    /// windowed calls (`[0, 3000)` then `[3000, 6000)` against a 5000-char
    /// fixture) and the two windows, concatenated, must reconstruct the exact
    /// original content — not just that each window's *length* looks right in
    /// isolation (the existing `char_window` unit tests already cover that),
    /// but that the two calls' windows actually line up end-to-end the way a
    /// model paging through a long fetched page would rely on.
    #[tokio::test]
    async fn start_index_pagination_reconstructs_the_full_content_across_two_windowed_fetches() {
        // Deterministic, non-repeating-in-a-way-that-would-mask-a-bug content:
        // cycles through the alphabet so a byte transposed between windows
        // would very likely produce a mismatch rather than accidentally still
        // matching.
        let full_content: String = (0..5000).map(|i| (b'a' + (i % 26) as u8) as char).collect();
        // The local test server is a 127.0.0.1 loopback target, which the SSRF
        // guard rejects by default (correctly) — this test is exercising
        // pagination, not the guard, so it opts in the same way a real user
        // enabling "allow local network" in Settings would.
        let settings = WebSettings {
            allow_local_network: true,
            ..WebSettings::default()
        };

        let base_a = spawn_content_response_server("text/plain", full_content.clone());
        let first = fetch_impl(&settings, format!("{base_a}/"), Some(3000), Some(0))
            .await
            .expect("first windowed fetch should succeed");
        assert_eq!(first.total_chars, 5000);
        assert!(
            first.truncated,
            "a 3000-char window over 5000 chars of content must report truncated"
        );
        assert_eq!(first.markdown.chars().count(), 3000);
        assert_eq!(first.markdown, full_content[0..3000]);

        let base_b = spawn_content_response_server("text/plain", full_content.clone());
        let second = fetch_impl(&settings, format!("{base_b}/"), Some(3000), Some(3000))
            .await
            .expect("second windowed fetch should succeed");
        assert_eq!(second.total_chars, 5000);
        assert!(
            !second.truncated,
            "a window reaching the end of the content must not report truncated"
        );
        assert_eq!(second.markdown.chars().count(), 2000);
        assert_eq!(second.markdown, full_content[3000..5000]);

        // The actual continuity guarantee `start_index` pagination exists for:
        // paging through both windows reconstructs the original byte-for-byte.
        assert_eq!(
            format!("{}{}", first.markdown, second.markdown),
            full_content
        );
    }

    /// Spins up a one-shot local HTTP server that accepts the connection but
    /// deliberately waits `delay` before writing any response — a stand-in for
    /// a slow/hanging page, used to prove Stop-button cancellation actually
    /// abandons an in-flight fetch rather than just returning a "cancelled"
    /// label after quietly waiting for the real response anyway.
    fn spawn_slow_response_server(delay: Duration, body: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                std::thread::sleep(delay);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// Drives the *exact* `tokio::select!` shape [`tool_web_fetch`] wraps
    /// [`fetch_impl`] in (see its body) against a server that would otherwise
    /// take several seconds to respond, with the cancellation `Notify` firing
    /// almost immediately — proving the wiring actually drops the in-flight
    /// request future rather than merely racing a label onto a result that
    /// still shows up later. This can't drive `tool_web_fetch` itself (it
    /// needs a live `AppHandle`/`tauri::State` for the permission gate and
    /// `state.tool_cancel`), so it reconstructs the same `select!` inline —
    /// which is faithful because `tool_web_fetch`'s cancellation branch is
    /// nothing more than this `select!` around `fetch_impl`, with the
    /// `Notify` sourced from `state.tool_cancel` instead of a local one.
    #[tokio::test]
    async fn select_cancellation_drops_the_in_flight_fetch_instead_of_waiting_for_it() {
        let slow_server_delay = Duration::from_secs(3);
        let base = spawn_slow_response_server(slow_server_delay, "too slow");
        // Same opt-in as the pagination test above — a 127.0.0.1 test server
        // is exactly what the SSRF guard exists to block by default.
        let settings = WebSettings {
            allow_local_network: true,
            ..WebSettings::default()
        };

        let cancel = std::sync::Arc::new(Notify::new());
        let cancel_signal = cancel.clone();
        tokio::spawn(async move {
            // Stand-in for the user pressing Stop shortly after the request
            // started — well before the slow server would ever respond.
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_signal.notify_one();
        });

        let started = std::time::Instant::now();
        let outcome: Result<FetchResult, String> = tokio::select! {
            result = fetch_impl(&settings, format!("{base}/"), None, None) => result,
            _ = cancel.notified() => Err("Fetch cancelled by the user".to_string()),
        };
        let elapsed = started.elapsed();

        let err = outcome.expect_err("a cancelled fetch must surface as an error, not a result");
        assert!(err.contains("cancelled"), "unexpected error: {err}");
        assert!(
            elapsed < slow_server_delay / 2,
            "expected cancellation to abandon the in-flight fetch immediately, but the select! took {:?} \
             (close to the server's {:?} artificial delay) as if it had actually waited for the response",
            elapsed,
            slow_server_delay
        );
    }
}

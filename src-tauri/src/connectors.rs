//! Connector Catalog — the foundation for guided Slack/Notion/Jira/S3/GitHub
//! connections used by later Knowledge Sync/Inbox Triage/Issue-to-PR work,
//! built entirely without registering a real OAuth app anywhere.
//!
//! GitHub is bridged through the already-authenticated `gh` CLI (see
//! `m5_delivery::github`) — this module never asks for or stores a GitHub
//! token, only confirms `gh auth status` and records the returned login.
//! Slack, Notion, and Jira connect with a user-pasted bot/integration/API
//! token (Jira additionally needs the account email + site URL for Basic
//! auth), and S3/R2 connect with an access key + secret key. Every one of
//! these is verified with one cheap, live, read-only API call *before* it is
//! ever saved — a bad credential is never persisted.
//!
//! Secrets never touch `connectors.json`. Every credential this module ever
//! saves (Slack/Notion/Jira token, S3 secret key) lives in the OS keychain
//! only, under the same `KEYCHAIN_SERVICE` constant `providers.rs`/`mcp.rs`
//! use, disambiguated by account name `connector:<provider>:<id>` (see
//! [`keychain_account`]) — GitHub gets no keychain entry at all
//! (`credential_ref: None`), since `gh` itself owns that credential.
//! `ConnectorAccount::connection` is the one exception worth calling out: it
//! holds non-secret provider metadata an account needs to re-verify or (in a
//! later ROADMAP stage) actually call its API again — Jira's site URL/email,
//! or S3's endpoint/bucket/region/access key. This is a deliberate, narrow
//! extension beyond a bare id/label/scopes shape, mirroring exactly what
//! `knowledge_service.rs`'s `ConnectorConfig::WebDav` already does (storing
//! `url`/`username` alongside a `credential_ref`, never the password).
//!
//! Non-goals, explicitly: GitHub, Slack, Notion, and Jira all offer a "real"
//! OAuth app flow instead of the token schemes above — none of that is built
//! here, by design (see the top-level task brief). Google Drive, SharePoint/
//! Graph, and anything else that genuinely requires a registered OAuth app
//! with a redirect URI are out of scope for this catalog entirely; they are
//! not faked with a token workaround.
//!
//! Every outbound verification call goes through [`verified_call`]: DNS is
//! resolved once and pinned to the exact socket the TLS/TCP connection uses
//! (closing the classic check-then-connect DNS-rebinding gap), non-public
//! resolved addresses are rejected, redirects are refused outright (none of
//! these API calls legitimately redirect), and the response body is capped
//! well below what an identity-check JSON payload could ever need. Slack and
//! Notion hit fixed, well-known hostnames; Jira's site URL and S3's endpoint
//! are user-supplied, so their *origin* is pinned at add time (from the value
//! the user just typed) and never followed cross-origin after — the same
//! posture `knowledge_service.rs`'s WebDAV connector takes toward its own
//! user-supplied URL.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::Url;

use crate::AppState;

/// Same keychain *service* string `providers.rs`/`mcp.rs` use — entries are
/// disambiguated by *account* name, not service, so this module's
/// `connector:<provider>:<id>` account prefix (see [`keychain_account`]) is
/// what keeps every feature's keychain entries apart within one namespace.
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.app";

const CONFIG_FILE: &str = "connectors.json";
const SCHEMA_VERSION: u8 = 1;

/// Timeout and size cap for one connector-verification HTTP call — these are
/// small identity-check JSON payloads (Slack's `auth.test`, Notion's
/// `/v1/users/me`, Jira's `/myself`), not file content, so both bounds are
/// deliberately much smaller than `knowledge_service.rs`'s file-fetch limits.
const VERIFY_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_VERIFY_BYTES: usize = 64 * 1024;

/// A connector this catalog can hold an account for. `Jira` covers Jira and
/// Confluence (same Atlassian API-token + email scheme); `S3` covers S3 and
/// R2 (same access-key/secret-key scheme) — see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorProvider {
    Github,
    Slack,
    Notion,
    Jira,
    S3,
}

impl ConnectorProvider {
    fn as_str(self) -> &'static str {
        match self {
            ConnectorProvider::Github => "github",
            ConnectorProvider::Slack => "slack",
            ConnectorProvider::Notion => "notion",
            ConnectorProvider::Jira => "jira",
            ConnectorProvider::S3 => "s3",
        }
    }
}

/// One connected account in the catalog, as persisted in `connectors.json`.
/// Never contains a secret: `credential_ref` is a keychain account name, not
/// a credential (`None` for GitHub, which has no stored secret — identity
/// comes from `gh` — see the module doc). `connection` is the one field
/// beyond the task brief's literal shape; see the module doc for why.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectorAccount {
    pub id: String,
    pub provider: ConnectorProvider,
    pub label: String,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub credential_ref: Option<String>,
    #[serde(default)]
    pub identity: Option<String>,
    pub created_at: u64,
    #[serde(default)]
    pub last_verified_at: Option<u64>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// Non-secret provider-specific connection metadata (Jira's `site_url`/
    /// `email`, S3's `endpoint`/`bucket`/`region`/`access_key`) — see the
    /// module doc. `None` for GitHub/Slack/Notion, which need nothing beyond
    /// a fixed hostname plus (for Slack/Notion) the keychain token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<Value>,
}

/// The whole on-disk `connectors.json` document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectorCatalogFile {
    #[serde(default)]
    pub version: u8,
    #[serde(default)]
    pub accounts: Vec<ConnectorAccount>,
}

/// The keychain *account* name under which `id`'s secret (Slack/Notion/Jira
/// token, or S3 secret key) is stored — `connector:<provider>:<id>`,
/// distinguishing it from `providers.rs`'s `<provider_id>`-only accounts and
/// `mcp.rs`'s `mcp:<id>` accounts in the same keychain service.
fn keychain_account(provider: ConnectorProvider, id: &str) -> String {
    format!("connector:{}:{}", provider.as_str(), id)
}

/// Resolves (and creates, if missing) `<app_data_dir>/connectors.json`'s
/// path. AppHandle-free via `app_paths::data_dir()` — unlike `mcp.rs`'s
/// `config_file_path`, this needs no `AppHandle` at all, so every command
/// below (bar the ones that also need `AppState`) is a plain function.
fn config_file_path() -> Result<PathBuf, String> {
    let dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Failed to resolve app data dir".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {e}"))?;
    Ok(dir.join(CONFIG_FILE))
}

/// Core load logic, parameterized by path for testability. A missing file
/// (nothing connected yet — the common case) is simply the empty default,
/// never an error — same stance as `mcp.rs::load_config_impl`.
pub fn load_config_impl(path: &Path) -> Result<ConnectorCatalogFile, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("Corrupt connectors.json: {e}"))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ConnectorCatalogFile::default()),
        Err(e) => Err(format!("Failed to read connectors.json: {e}")),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `mcp.rs::save_config_impl`/`sessions.rs`/`memory.rs`, so a crash mid-write
/// can never leave a truncated/corrupt catalog behind.
pub fn save_config_impl(path: &Path, config: &ConnectorCatalogFile) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize connectors.json: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload).map_err(|e| format!("Failed to write connectors.json: {e}"))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Failed to finalize connectors.json: {e}"))?;
    Ok(())
}

fn validate_label(label: &str) -> Result<(), String> {
    if label.trim().is_empty() || label.len() > 200 {
        return Err("Label must be non-empty and at most 200 characters".to_string());
    }
    Ok(())
}

// --- SSRF-hardened verification HTTP calls ---------------------------------

async fn resolve_host(url: &Url) -> Result<Vec<IpAddr>, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let mut addresses = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();
    addresses.sort();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(format!("DNS returned no addresses for {host}"));
    }
    Ok(addresses)
}

/// `scheme://host[:port]` for `url` — used both as the single allowed origin
/// passed to `UrlSourcePolicy` (pinning a user-supplied Jira site/S3 endpoint
/// to itself) and, for Slack/Notion, as the fixed well-known origin.
fn origin_of(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    match url.port() {
        Some(port) => Ok(format!("{}://{}:{}", url.scheme(), host, port)),
        None => Ok(format!("{}://{}", url.scheme(), host)),
    }
}

/// One SSRF-hardened, non-redirecting HTTP call — every connector's live
/// verification goes through this. Resolves and pins the destination's DNS
/// once (so the TLS/TCP connect can never land on a different, TOCTOU-raced
/// address than the one just validated — same technique as
/// `knowledge_service.rs::fetch_http`), rejects non-public resolved
/// addresses unless `allow_loopback` is set (tests only — every production
/// call site below passes `false`), rejects any redirect outright (safer
/// than following one, and none of these identity-check API calls
/// legitimately redirect), and caps the response body at
/// [`MAX_VERIFY_BYTES`] so a compromised/malicious endpoint can never make
/// this hang or exhaust memory. `allowed_origin` is pinned by the caller —
/// Slack/Notion pass their own fixed hostname; Jira/S3 pass the origin
/// derived from whatever site URL/endpoint the user just typed (see the
/// module doc).
async fn verified_call(
    method: reqwest::Method,
    url: &Url,
    allowed_origin: &str,
    allow_loopback: bool,
    headers: &[(&'static str, String)],
    basic_auth: Option<(&str, &str)>,
) -> Result<Vec<u8>, String> {
    let policy =
        crate::knowledge_pipeline::UrlSourcePolicy::new([allowed_origin], allow_loopback, false)
            .map_err(|e| e.to_string())?;
    let limits = crate::knowledge_pipeline::PipelineLimits::default();
    let addresses = resolve_host(url).await?;
    policy
        .validate(url.as_str(), &addresses, &limits)
        .map_err(|e| e.to_string())?;

    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| "URL has no port".to_string())?;
    let socket = SocketAddr::new(addresses[0], port);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(VERIFY_TIMEOUT)
        .resolve(host, socket)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let mut request = client.request(method, url.clone());
    for (key, value) in headers {
        request = request.header(*key, value);
    }
    if let Some((username, password)) = basic_auth {
        request = request.basic_auth(username, Some(password));
    }

    let response = request
        .send()
        .await
        .map_err(|e| format!("Verification request failed: {e}"))?;
    if response.status().is_redirection() {
        return Err("Verification response was a redirect — refusing to follow".to_string());
    }
    let status = response.status();
    if let Some(length) = response.content_length() {
        if length as usize > MAX_VERIFY_BYTES {
            return Err("Verification response exceeds the size limit".to_string());
        }
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("Verification response stream failed: {e}"))?;
        if body.len().saturating_add(chunk.len()) > MAX_VERIFY_BYTES {
            return Err("Verification response exceeds the size limit".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let snippet: String = String::from_utf8_lossy(&body).chars().take(300).collect();
        return Err(format!("Verification failed with HTTP {status}: {snippet}"));
    }
    Ok(body)
}

async fn verify_slack(token: &str) -> Result<String, String> {
    let url =
        Url::parse("https://slack.com/api/auth.test").expect("hardcoded Slack URL is valid");
    let body = verified_call(
        reqwest::Method::POST,
        &url,
        "https://slack.com",
        false,
        &[("authorization", format!("Bearer {token}"))],
        None,
    )
    .await?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("Slack response was not valid JSON: {e}"))?;
    if json.get("ok").and_then(Value::as_bool) != Some(true) {
        let error = json
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(format!("Slack rejected the token: {error}"));
    }
    let team = json.get("team").and_then(Value::as_str).unwrap_or("workspace");
    let user = json.get("user").and_then(Value::as_str).unwrap_or("bot");
    Ok(format!("{user} @ {team}"))
}

async fn verify_notion(token: &str) -> Result<String, String> {
    let url = Url::parse("https://api.notion.com/v1/users/me")
        .expect("hardcoded Notion URL is valid");
    let body = verified_call(
        reqwest::Method::GET,
        &url,
        "https://api.notion.com",
        false,
        &[
            ("authorization", format!("Bearer {token}")),
            ("notion-version", "2022-06-28".to_string()),
        ],
        None,
    )
    .await?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("Notion response was not valid JSON: {e}"))?;
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            json.get("bot")
                .and_then(|bot| bot.get("owner"))
                .and_then(|owner| owner.get("user"))
                .and_then(|user| user.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("Notion integration");
    Ok(name.to_string())
}

async fn verify_jira(site_url: &str, email: &str, token: &str) -> Result<String, String> {
    let base = Url::parse(site_url).map_err(|e| format!("Invalid Jira site URL: {e}"))?;
    let origin = origin_of(&base)?;
    let url = base
        .join("/rest/api/3/myself")
        .map_err(|e| format!("Invalid Jira site URL: {e}"))?;
    let body = verified_call(
        reqwest::Method::GET,
        &url,
        &origin,
        false,
        &[("accept", "application/json".to_string())],
        Some((email, token)),
    )
    .await?;
    let json: Value = serde_json::from_slice(&body)
        .map_err(|e| format!("Jira response was not valid JSON: {e}"))?;
    let name = json
        .get("displayName")
        .and_then(Value::as_str)
        .unwrap_or(email);
    Ok(name.to_string())
}

// --- S3 SigV4 (used only by `verify_s3`) -----------------------------------

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac =
        <HmacSha256 as Mac>::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn validate_s3_bucket(bucket: &str) -> Result<(), String> {
    let valid = (3..=63).contains(&bucket.len())
        && bucket
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.'));
    if valid {
        Ok(())
    } else {
        Err("Invalid S3 bucket name".to_string())
    }
}

fn validate_s3_region(region: &str) -> Result<(), String> {
    let valid = !region.is_empty()
        && region.len() <= 40
        && region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if valid {
        Ok(())
    } else {
        Err("Invalid S3 region".to_string())
    }
}

fn host_header_value(url: &Url) -> Result<String, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;
    match url.port() {
        Some(port) => Ok(format!("{host}:{port}")),
        None => Ok(host.to_string()),
    }
}

/// AWS Signature Version 4 for a single header-only request (no query
/// string, no body) — the one shape `verify_s3`'s bucket-existence `HEAD`
/// check ever needs. Hand-rolled rather than pulling in an AWS SDK or
/// `aws-sigv4` crate: the algorithm is short, fully deterministic, and this
/// is the only request shape ever signed here. See this file's tests for the
/// well-known empty-payload SHA-256 constant AWS's own docs publish and an
/// RFC 4231 HMAC-SHA-256 test vector, which together anchor the two
/// primitives this builds on.
fn sigv4_authorization(
    method: &str,
    host_header: &str,
    canonical_uri: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    amz_date: &str,
) -> String {
    let date_stamp = &amz_date[..amz_date.len().min(8)];
    let payload_hash = sha256_hex(b"");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_headers = format!(
        "host:{host_header}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
    );
    let canonical_request =
        format!("{method}\n{canonical_uri}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let credential_scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac_sha256(format!("AWS4{secret_key}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    let k_signing = hmac_sha256(&k_service, b"aws4_request");
    let signature = hex_encode(&hmac_sha256(&k_signing, string_to_sign.as_bytes()));
    format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}"
    )
}

/// Verifies S3/R2 credentials with a path-style `HEAD /{bucket}` — enough to
/// confirm the endpoint, bucket, and credentials all agree, without ever
/// listing or reading bucket contents. Path-style (not virtual-hosted-style)
/// addressing is used deliberately: it works identically across AWS S3, R2,
/// MinIO, and any other S3-compatible endpoint a user might type in, with no
/// per-provider special-casing.
async fn verify_s3(
    endpoint: &str,
    bucket: &str,
    region: &str,
    access_key: &str,
    secret_key: &str,
) -> Result<String, String> {
    validate_s3_bucket(bucket)?;
    validate_s3_region(region)?;
    let base = Url::parse(endpoint).map_err(|e| format!("Invalid S3 endpoint: {e}"))?;
    let origin = origin_of(&base)?;
    let host_header = host_header_value(&base)?;
    let mut url = base;
    url.set_path(&format!("/{bucket}"));

    let amz_date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let authorization = sigv4_authorization(
        "HEAD",
        &host_header,
        url.path(),
        access_key,
        secret_key,
        region,
        &amz_date,
    );

    verified_call(
        reqwest::Method::HEAD,
        &url,
        &origin,
        false,
        &[
            ("x-amz-date", amz_date.clone()),
            ("x-amz-content-sha256", sha256_hex(b"")),
            ("authorization", authorization),
        ],
        None,
    )
    .await?;
    Ok(format!("{bucket} @ {region}"))
}

// --- credential + connection-metadata helpers -------------------------------

fn read_credential(account: &ConnectorAccount) -> Result<String, String> {
    let credential_ref = account
        .credential_ref
        .as_deref()
        .ok_or_else(|| format!("Connector '{}' has no stored credential", account.id))?;
    keyring::Entry::new(KEYCHAIN_SERVICE, credential_ref)
        .map_err(|e| format!("Failed to access keychain: {e}"))?
        .get_password()
        .map_err(|e| format!("Failed to read saved credential: {e}"))
}

fn jira_connection(account: &ConnectorAccount) -> Result<(String, String), String> {
    let connection = account
        .connection
        .as_ref()
        .ok_or_else(|| "Missing Jira connection details".to_string())?;
    let site_url = connection
        .get("site_url")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing Jira site URL".to_string())?
        .to_string();
    let email = connection
        .get("email")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing Jira account email".to_string())?
        .to_string();
    Ok((site_url, email))
}

fn s3_connection(account: &ConnectorAccount) -> Result<(String, String, String, String), String> {
    let connection = account
        .connection
        .as_ref()
        .ok_or_else(|| "Missing S3 connection details".to_string())?;
    let get = |key: &str| -> Result<String, String> {
        connection
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("Missing S3 {key}"))
    };
    Ok((get("endpoint")?, get("bucket")?, get("region")?, get("access_key")?))
}

// --- commands ---------------------------------------------------------------

#[tauri::command]
pub fn connectors_list() -> Result<Vec<ConnectorAccount>, String> {
    Ok(load_config_impl(&config_file_path()?)?.accounts)
}

/// Core logic behind [`connectors_add_github`], parameterized by the already
/// -resolved `gh auth status` pieces (rather than the private
/// `m5_delivery::github::GitHubAuthStatus` type, which isn't nameable
/// outside that module) so it's directly unit-testable without a real `gh`
/// process. Upserts by identity: reconnecting the same already-authenticated
/// `gh` login refreshes the existing entry instead of creating a duplicate.
fn add_github_with_status_impl(
    state: &AppState,
    path: &Path,
    label: Option<String>,
    available: bool,
    authenticated: bool,
    account_login: Option<String>,
    detail: &str,
) -> Result<ConnectorAccount, String> {
    if !available {
        return Err("GitHub CLI (`gh`) is not installed".to_string());
    }
    if !authenticated {
        return Err(format!(
            "GitHub CLI authentication is missing or expired: {detail}"
        ));
    }
    let identity = account_login
        .ok_or_else(|| "GitHub CLI did not return an authenticated account".to_string())?;

    let _guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    let now = crate::run_commands::unix_time_ms()?;

    if let Some(existing) = config.accounts.iter_mut().find(|account| {
        account.provider == ConnectorProvider::Github
            && account.identity.as_deref() == Some(identity.as_str())
    }) {
        existing.last_verified_at = Some(now);
        existing.last_error = None;
        let updated = existing.clone();
        save_config_impl(path, &config)?;
        return Ok(updated);
    }

    let account = ConnectorAccount {
        id: uuid::Uuid::new_v4().to_string(),
        provider: ConnectorProvider::Github,
        label: label
            .filter(|l| !l.trim().is_empty())
            .unwrap_or_else(|| format!("GitHub ({identity})")),
        scopes: Vec::new(),
        credential_ref: None,
        identity: Some(identity),
        created_at: now,
        last_verified_at: Some(now),
        last_error: None,
        connection: None,
    };
    config.version = SCHEMA_VERSION;
    config.accounts.push(account.clone());
    save_config_impl(path, &config)?;
    Ok(account)
}

fn add_github_impl(
    state: &AppState,
    path: &Path,
    label: Option<String>,
) -> Result<ConnectorAccount, String> {
    let status = crate::m5_delivery::m5_github_auth_status()?;
    add_github_with_status_impl(
        state,
        path,
        label,
        status.available,
        status.authenticated,
        status.account,
        &status.detail,
    )
}

/// Connects a GitHub account via the already-authenticated `gh` CLI — no
/// pasted token, ever. Fails with a clear message if `gh` isn't installed or
/// isn't logged in; on success, records the login `gh` reports (never a
/// credential — GitHub's `credential_ref` is always `None`).
#[tauri::command]
pub fn connectors_add_github(
    state: tauri::State<'_, AppState>,
    label: Option<String>,
) -> Result<ConnectorAccount, String> {
    add_github_impl(state.inner(), &config_file_path()?, label)
}

async fn verify_token(
    provider: ConnectorProvider,
    token: &str,
    email: Option<&str>,
    site_url: Option<&str>,
) -> Result<(String, Option<Value>), String> {
    match provider {
        ConnectorProvider::Github => Err(
            "GitHub connects via `gh` — use connectors_add_github instead of a pasted token"
                .to_string(),
        ),
        ConnectorProvider::Slack => verify_slack(token).await.map(|identity| (identity, None)),
        ConnectorProvider::Notion => verify_notion(token).await.map(|identity| (identity, None)),
        ConnectorProvider::Jira => {
            let email = email
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .ok_or_else(|| "Jira requires the account email".to_string())?;
            let site_url = site_url
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| "Jira requires the site URL".to_string())?;
            let identity = verify_jira(site_url, email, token).await?;
            Ok((
                identity,
                Some(serde_json::json!({ "site_url": site_url, "email": email })),
            ))
        }
        ConnectorProvider::S3 => {
            Err("S3 connects via connectors_add_s3, not a pasted token".to_string())
        }
    }
}

async fn add_token_impl(
    state: &AppState,
    path: &Path,
    provider: ConnectorProvider,
    label: String,
    token: String,
    scopes: Vec<String>,
    email: Option<String>,
    site_url: Option<String>,
) -> Result<ConnectorAccount, String> {
    validate_label(&label)?;
    let token = token.trim().to_string();
    if token.is_empty() {
        return Err("Token is required".to_string());
    }
    if scopes.is_empty() {
        return Err("At least one scope/capability must be listed before saving".to_string());
    }

    // Verify BEFORE touching the keychain or catalog — a bad credential (or,
    // for Jira/S3, a site URL/endpoint the SSRF policy rejects) is never
    // persisted.
    let (identity, connection) =
        verify_token(provider, &token, email.as_deref(), site_url.as_deref()).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let credential_ref = keychain_account(provider, &id);
    let keychain_entry = keyring::Entry::new(KEYCHAIN_SERVICE, &credential_ref)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    keychain_entry
        .set_password(&token)
        .map_err(|e| format!("Failed to save token to keychain: {e}"))?;

    let now = crate::run_commands::unix_time_ms()?;
    let account = ConnectorAccount {
        id,
        provider,
        label,
        scopes,
        credential_ref: Some(credential_ref),
        identity: Some(identity),
        created_at: now,
        last_verified_at: Some(now),
        last_error: None,
        connection,
    };

    let _guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    config.version = SCHEMA_VERSION;
    config.accounts.push(account.clone());
    if let Err(error) = save_config_impl(path, &config) {
        let _ = keychain_entry.delete_credential();
        return Err(error);
    }
    Ok(account)
}

/// Connects Slack, Notion, or Jira with a user-pasted bot/integration/API
/// token — verified live (Slack `auth.test`, Notion `/v1/users/me`, Jira
/// `/rest/api/3/myself`) before it's ever saved. `email`/`site_url` are
/// required for (and only meaningful for) Jira.
#[tauri::command]
pub async fn connectors_add_token(
    state: tauri::State<'_, AppState>,
    provider: ConnectorProvider,
    label: String,
    token: String,
    scopes: Vec<String>,
    email: Option<String>,
    site_url: Option<String>,
) -> Result<ConnectorAccount, String> {
    add_token_impl(
        state.inner(),
        &config_file_path()?,
        provider,
        label,
        token,
        scopes,
        email,
        site_url,
    )
    .await
}

async fn add_s3_impl(
    state: &AppState,
    path: &Path,
    label: String,
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
) -> Result<ConnectorAccount, String> {
    validate_label(&label)?;
    let endpoint = endpoint.trim().to_string();
    let bucket = bucket.trim().to_string();
    let region = region.trim().to_string();
    let access_key = access_key.trim().to_string();
    let secret_key = secret_key.trim().to_string();
    if access_key.is_empty() || secret_key.is_empty() {
        return Err("Access key and secret key are required".to_string());
    }

    let identity = verify_s3(&endpoint, &bucket, &region, &access_key, &secret_key).await?;

    let id = uuid::Uuid::new_v4().to_string();
    let credential_ref = keychain_account(ConnectorProvider::S3, &id);
    let keychain_entry = keyring::Entry::new(KEYCHAIN_SERVICE, &credential_ref)
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    keychain_entry
        .set_password(&secret_key)
        .map_err(|e| format!("Failed to save secret key to keychain: {e}"))?;

    let now = crate::run_commands::unix_time_ms()?;
    let account = ConnectorAccount {
        id,
        provider: ConnectorProvider::S3,
        label,
        scopes: vec!["read".to_string()],
        credential_ref: Some(credential_ref),
        identity: Some(identity),
        created_at: now,
        last_verified_at: Some(now),
        last_error: None,
        connection: Some(serde_json::json!({
            "endpoint": endpoint,
            "bucket": bucket,
            "region": region,
            "access_key": access_key,
        })),
    };

    let _guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    config.version = SCHEMA_VERSION;
    config.accounts.push(account.clone());
    if let Err(error) = save_config_impl(path, &config) {
        let _ = keychain_entry.delete_credential();
        return Err(error);
    }
    Ok(account)
}

/// Connects an S3/R2 bucket with an access key + secret key — verified live
/// with a minimal SigV4-signed `HEAD /{bucket}` before it's ever saved.
#[tauri::command]
pub async fn connectors_add_s3(
    state: tauri::State<'_, AppState>,
    label: String,
    endpoint: String,
    bucket: String,
    region: String,
    access_key: String,
    secret_key: String,
) -> Result<ConnectorAccount, String> {
    add_s3_impl(
        state.inner(),
        &config_file_path()?,
        label,
        endpoint,
        bucket,
        region,
        access_key,
        secret_key,
    )
    .await
}

fn remove_impl(state: &AppState, path: &Path, id: &str) -> Result<(), String> {
    let _guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    let before = config.accounts.len();
    let removed_credential_ref = config
        .accounts
        .iter()
        .find(|account| account.id == id)
        .and_then(|account| account.credential_ref.clone());
    config.accounts.retain(|account| account.id != id);
    if config.accounts.len() != before {
        save_config_impl(path, &config)?;
    }
    // Best-effort, same stance as `mcp.rs::mcp_remove_server`: an id that
    // never had a keychain secret (GitHub, or one that's already gone) hits
    // the `NoEntry` no-op path — never fails the removal itself over
    // keychain cleanup.
    if let Some(credential_ref) = removed_credential_ref {
        let _ = keyring::Entry::new(KEYCHAIN_SERVICE, &credential_ref)
            .and_then(|entry| entry.delete_credential());
    }
    Ok(())
}

/// Removes a connector: deletes its catalog entry and (best-effort) its
/// keychain secret. Removing an unknown id is a no-op success, not an error
/// — the caller's desired end state (the account is gone) already holds.
#[tauri::command]
pub fn connectors_remove(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    remove_impl(state.inner(), &config_file_path()?, &id)
}

/// Core logic behind `reverify_impl`'s GitHub branch, parameterized by the
/// already-resolved `gh auth status` pieces (same rationale as
/// [`add_github_with_status_impl`]) so the identity-mismatch guard below is
/// directly unit-testable without a real `gh` process.
///
/// The `gh` CLI is a single, global, machine-wide session: it reports
/// whatever account is *currently* logged in, with no notion of "the account
/// this particular catalog entry was created for". If the user has since run
/// `gh auth login`/`gh auth switch` to a different account, a reverify must
/// not silently adopt that different identity into this entry — that would
/// corrupt the entry's identity without any error surfaced (`last_error` is
/// cleared on the success path), and a later consumer of `identity` (a
/// future ROADMAP stage that calls the API again, e.g. Issue-to-PR) would
/// then act as the wrong GitHub account. Instead, treat a mismatch as a
/// verification failure: `last_error` is set and `identity` is left alone.
fn reverify_github_identity_impl(
    expected_identity: Option<&str>,
    status_available: bool,
    status_authenticated: bool,
    status_account: Option<String>,
    status_detail: &str,
) -> Result<String, String> {
    if !status_available || !status_authenticated {
        return Err(format!(
            "GitHub CLI authentication is missing or expired: {status_detail}"
        ));
    }
    let current = status_account
        .ok_or_else(|| "GitHub CLI did not return an authenticated account".to_string())?;
    match expected_identity {
        Some(expected) if expected != current => Err(format!(
            "GitHub CLI is currently authenticated as '{current}', but this connector was added for '{expected}'. Switch the `gh` CLI session back to '{expected}' (`gh auth switch`) or reconnect this entry before reverifying."
        )),
        _ => Ok(current),
    }
}

async fn reverify_impl(
    state: &AppState,
    path: &Path,
    id: &str,
) -> Result<ConnectorAccount, String> {
    let account = load_config_impl(path)?
        .accounts
        .into_iter()
        .find(|account| account.id == id)
        .ok_or_else(|| format!("Unknown connector '{id}'"))?;

    let result: Result<String, String> = async {
        match account.provider {
            ConnectorProvider::Github => {
                let status = crate::m5_delivery::m5_github_auth_status()?;
                reverify_github_identity_impl(
                    account.identity.as_deref(),
                    status.available,
                    status.authenticated,
                    status.account.clone(),
                    &status.detail,
                )
            }
            ConnectorProvider::Slack => {
                let token = read_credential(&account)?;
                verify_slack(&token).await
            }
            ConnectorProvider::Notion => {
                let token = read_credential(&account)?;
                verify_notion(&token).await
            }
            ConnectorProvider::Jira => {
                let token = read_credential(&account)?;
                let (site_url, email) = jira_connection(&account)?;
                verify_jira(&site_url, &email, &token).await
            }
            ConnectorProvider::S3 => {
                let secret_key = read_credential(&account)?;
                let (endpoint, bucket, region, access_key) = s3_connection(&account)?;
                verify_s3(&endpoint, &bucket, &region, &access_key, &secret_key).await
            }
        }
    }
    .await;

    let now = crate::run_commands::unix_time_ms()?;
    let _guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = load_config_impl(path)?;
    let slot = config
        .accounts
        .iter_mut()
        .find(|account| account.id == id)
        .ok_or_else(|| format!("Unknown connector '{id}'"))?;
    match &result {
        Ok(identity) => {
            slot.identity = Some(identity.clone());
            slot.last_verified_at = Some(now);
            slot.last_error = None;
        }
        Err(message) => {
            slot.last_error = Some(message.clone());
        }
    }
    let updated = slot.clone();
    save_config_impl(path, &config)?;
    result.map(|_| updated)
}

/// Re-runs a connector's live verification call and records the outcome
/// (updating `last_verified_at`/`identity` on success, `last_error` on
/// failure) either way — the catalog entry is never deleted by a failed
/// reverify, only marked.
#[tauri::command]
pub async fn connectors_reverify(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<ConnectorAccount, String> {
    reverify_impl(state.inner(), &config_file_path()?, &id).await
}

/// Redacted audit report: id/provider/label/scopes/created_at/
/// last_verified_at/last_error only — never the token, never
/// `credential_ref`, never `identity` (which for Jira/S3 can carry a display
/// name or account identifier), never `connection`.
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorAuditEntry {
    pub id: String,
    pub provider: ConnectorProvider,
    pub label: String,
    pub scopes: Vec<String>,
    pub created_at: u64,
    pub last_verified_at: Option<u64>,
    pub last_error: Option<String>,
}

fn export_audit_impl(path: &Path) -> Result<Vec<ConnectorAuditEntry>, String> {
    Ok(load_config_impl(path)?
        .accounts
        .into_iter()
        .map(|account| ConnectorAuditEntry {
            id: account.id,
            provider: account.provider,
            label: account.label,
            scopes: account.scopes,
            created_at: account.created_at,
            last_verified_at: account.last_verified_at,
            last_error: account.last_error,
        })
        .collect())
}

#[tauri::command]
pub fn connectors_export_audit() -> Result<Vec<ConnectorAuditEntry>, String> {
    export_audit_impl(&config_file_path()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_connectors_test_{}_{}_{}_{}",
            std::process::id(),
            n,
            nanos,
            name
        ))
    }

    // --- catalog load/save/CRUD --------------------------------------------

    #[test]
    fn load_returns_default_when_file_missing() {
        let config = load_config_impl(&temp_path("missing.json")).unwrap();
        assert!(config.accounts.is_empty());
    }

    #[test]
    fn catalog_round_trip_never_persists_a_secret_in_the_json_file() {
        let path = temp_path("no_secret.json");
        let id = "acct-1".to_string();
        let credential_ref = keychain_account(ConnectorProvider::Slack, &id);
        let account = ConnectorAccount {
            id,
            provider: ConnectorProvider::Slack,
            label: "Team Slack".to_string(),
            scopes: vec!["channels:read".to_string()],
            credential_ref: Some(credential_ref.clone()),
            identity: Some("botty @ acme".to_string()),
            created_at: 1_000,
            last_verified_at: Some(1_000),
            last_error: None,
            connection: None,
        };
        save_config_impl(
            &path,
            &ConnectorCatalogFile {
                version: SCHEMA_VERSION,
                accounts: vec![account],
            },
        )
        .unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains(&credential_ref),
            "the credential_ref string itself must be present: {raw}"
        );
        assert!(
            !raw.contains("xoxb-") && !raw.to_lowercase().contains("\"token\""),
            "connectors.json must never contain a raw secret or a token field: {raw}"
        );

        let reloaded = load_config_impl(&path).unwrap();
        assert_eq!(reloaded.accounts.len(), 1);
        assert_eq!(reloaded.accounts[0].credential_ref.as_deref(), Some(credential_ref.as_str()));
    }

    #[test]
    fn keychain_account_is_scoped_by_both_provider_and_id() {
        assert_eq!(keychain_account(ConnectorProvider::Slack, "abc"), "connector:slack:abc");
        assert_eq!(keychain_account(ConnectorProvider::Jira, "abc"), "connector:jira:abc");
        assert_ne!(
            keychain_account(ConnectorProvider::Slack, "abc"),
            keychain_account(ConnectorProvider::Notion, "abc")
        );
    }

    // --- GitHub (gh-status-injected, no real `gh` process) -----------------

    #[test]
    fn add_github_creates_an_entry_with_no_stored_credential() {
        let path = temp_path("add_github.json");
        let state = AppState::default();
        let account = add_github_with_status_impl(
            &state,
            &path,
            None,
            true,
            true,
            Some("octocat".to_string()),
            "ok",
        )
        .unwrap();
        assert_eq!(account.provider, ConnectorProvider::Github);
        assert_eq!(account.credential_ref, None, "GitHub must never get a keychain entry");
        assert_eq!(account.identity.as_deref(), Some("octocat"));
    }

    #[test]
    fn add_github_upserts_by_identity_instead_of_duplicating() {
        let path = temp_path("add_github_upsert.json");
        let state = AppState::default();
        let first = add_github_with_status_impl(
            &state,
            &path,
            None,
            true,
            true,
            Some("octocat".to_string()),
            "ok",
        )
        .unwrap();
        let second = add_github_with_status_impl(
            &state,
            &path,
            None,
            true,
            true,
            Some("octocat".to_string()),
            "ok",
        )
        .unwrap();
        assert_eq!(first.id, second.id, "reconnecting the same gh login must not duplicate");
        let config = load_config_impl(&path).unwrap();
        assert_eq!(config.accounts.len(), 1);
    }

    #[test]
    fn add_github_reports_a_missing_cli_distinctly_from_missing_authentication() {
        let path = temp_path("add_github_missing.json");
        let state = AppState::default();

        let not_installed =
            add_github_with_status_impl(&state, &path, None, false, false, None, "n/a")
                .unwrap_err();
        assert!(not_installed.contains("not installed"), "{not_installed}");

        let not_authenticated = add_github_with_status_impl(
            &state,
            &path,
            None,
            true,
            false,
            None,
            "token expired",
        )
        .unwrap_err();
        assert!(not_authenticated.contains("token expired"), "{not_authenticated}");

        assert!(load_config_impl(&path).unwrap().accounts.is_empty());
    }

    #[test]
    fn concurrent_add_github_calls_under_the_config_lock_do_not_lose_updates() {
        let path = temp_path("concurrent_add_github.json");
        let state = AppState::default();

        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = &path;
                let state = &state;
                scope.spawn(move || {
                    add_github_with_status_impl(
                        state,
                        path,
                        None,
                        true,
                        true,
                        Some(format!("octocat-{i}")),
                        "ok",
                    )
                    .unwrap();
                });
            }
        });

        let config = load_config_impl(&path).unwrap();
        assert_eq!(
            config.accounts.len(),
            8,
            "a concurrent connectors_add_github call's entry was lost"
        );
    }

    // --- reverify: GitHub identity mismatch guard ---------------------------

    #[test]
    fn reverify_github_accepts_a_matching_currently_authenticated_login() {
        let result = reverify_github_identity_impl(
            Some("alice"),
            true,
            true,
            Some("alice".to_string()),
            "ok",
        );
        assert_eq!(result, Ok("alice".to_string()));
    }

    #[test]
    fn reverify_github_rejects_a_currently_authenticated_login_that_does_not_match_the_entry() {
        let error = reverify_github_identity_impl(
            Some("alice"),
            true,
            true,
            Some("bob".to_string()),
            "ok",
        )
        .unwrap_err();
        assert!(
            error.contains("alice") && error.contains("bob"),
            "error should name both the expected and current identity: {error}"
        );
    }

    #[test]
    fn reverify_github_surfaces_missing_or_expired_cli_auth_instead_of_a_stale_identity() {
        let error = reverify_github_identity_impl(Some("alice"), true, false, None, "token expired")
            .unwrap_err();
        assert!(error.contains("token expired"), "{error}");

        let error = reverify_github_identity_impl(Some("alice"), false, false, None, "n/a")
            .unwrap_err();
        assert!(error.contains("missing or expired"), "{error}");
    }

    // --- token connectors: verify-before-save -------------------------------

    #[tokio::test]
    async fn add_token_does_not_persist_an_entry_when_jira_verification_fails() {
        let path = temp_path("add_token_verify_fail.json");
        let state = AppState::default();

        // A loopback site URL is rejected by the SSRF policy before any
        // connection is attempted — deterministic, no live network needed.
        let result = add_token_impl(
            &state,
            &path,
            ConnectorProvider::Jira,
            "My Jira".to_string(),
            "fake-token".to_string(),
            vec!["read".to_string()],
            Some("person@example.com".to_string()),
            Some("http://127.0.0.1:9/wiki".to_string()),
        )
        .await;

        assert!(result.is_err(), "expected a blocked loopback Jira site to fail verification");
        let config = load_config_impl(&path).unwrap();
        assert!(
            config.accounts.is_empty(),
            "a failed verification must never persist a catalog entry"
        );
    }

    #[tokio::test]
    async fn add_token_rejects_github_and_s3_which_have_their_own_dedicated_commands() {
        let path = temp_path("add_token_wrong_provider.json");
        let state = AppState::default();

        let github_result = add_token_impl(
            &state,
            &path,
            ConnectorProvider::Github,
            "Wrong".to_string(),
            "tok".to_string(),
            vec!["read".to_string()],
            None,
            None,
        )
        .await;
        assert!(github_result.is_err());

        let s3_result = add_token_impl(
            &state,
            &path,
            ConnectorProvider::S3,
            "Wrong".to_string(),
            "tok".to_string(),
            vec!["read".to_string()],
            None,
            None,
        )
        .await;
        assert!(s3_result.is_err());
        assert!(load_config_impl(&path).unwrap().accounts.is_empty());
    }

    // --- keychain delete on remove ------------------------------------------

    #[test]
    fn remove_deletes_the_catalog_entry_and_its_keychain_secret() {
        let path = temp_path("remove_deletes_keychain.json");
        let state = AppState::default();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let credential_ref = keychain_account(ConnectorProvider::Slack, &id);
        keyring::Entry::new(KEYCHAIN_SERVICE, &credential_ref)
            .unwrap()
            .set_password("test-secret")
            .unwrap();

        let account = ConnectorAccount {
            id: id.clone(),
            provider: ConnectorProvider::Slack,
            label: "Test Slack".to_string(),
            scopes: vec!["read".to_string()],
            credential_ref: Some(credential_ref.clone()),
            identity: Some("tester".to_string()),
            created_at: 0,
            last_verified_at: None,
            last_error: None,
            connection: None,
        };
        save_config_impl(
            &path,
            &ConnectorCatalogFile {
                version: SCHEMA_VERSION,
                accounts: vec![account],
            },
        )
        .unwrap();

        remove_impl(&state, &path, &id).unwrap();

        assert!(load_config_impl(&path).unwrap().accounts.is_empty());
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &credential_ref).unwrap();
        match entry.get_password() {
            Ok(_) => panic!("expected the keychain secret to be deleted alongside the catalog entry"),
            Err(keyring::Error::NoEntry) => {}
            Err(other) => panic!("unexpected keychain error: {other}"),
        }
    }

    #[test]
    fn remove_of_an_unknown_id_is_a_no_op_success() {
        let path = temp_path("remove_unknown.json");
        let state = AppState::default();
        remove_impl(&state, &path, "does-not-exist").unwrap();
        assert!(load_config_impl(&path).unwrap().accounts.is_empty());
    }

    // --- export audit --------------------------------------------------------

    #[test]
    fn export_audit_redacts_identity_credential_ref_and_connection() {
        let path = temp_path("export_audit.json");
        let account = ConnectorAccount {
            id: "acct-1".to_string(),
            provider: ConnectorProvider::Jira,
            label: "Team Jira".to_string(),
            scopes: vec!["read".to_string()],
            credential_ref: Some(keychain_account(ConnectorProvider::Jira, "acct-1")),
            identity: Some("Jane Doe".to_string()),
            created_at: 42,
            last_verified_at: Some(43),
            last_error: Some("expired".to_string()),
            connection: Some(serde_json::json!({ "site_url": "https://acme.atlassian.net", "email": "jane@acme.example" })),
        };
        save_config_impl(
            &path,
            &ConnectorCatalogFile {
                version: SCHEMA_VERSION,
                accounts: vec![account],
            },
        )
        .unwrap();

        let audit = export_audit_impl(&path).unwrap();
        assert_eq!(audit.len(), 1);
        let entry = &audit[0];
        assert_eq!(entry.id, "acct-1");
        assert_eq!(entry.label, "Team Jira");
        assert_eq!(entry.created_at, 42);
        assert_eq!(entry.last_verified_at, Some(43));
        assert_eq!(entry.last_error.as_deref(), Some("expired"));

        // The audit entry type structurally cannot carry identity/
        // credential_ref/connection — serialize it and double-check none of
        // that redacted data leaks through regardless.
        let serialized = serde_json::to_string(&audit).unwrap();
        assert!(!serialized.contains("Jane Doe"));
        assert!(!serialized.contains("atlassian.net"));
        assert!(!serialized.contains("jane@acme.example"));
    }

    // --- SSRF / origin pinning for the verification HTTP call ---------------

    fn spawn_fixture(status_line: &str, extra_headers: &str, body: &'static str) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let extra_headers = extra_headers.to_string();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let response = format!(
                    "{status_line}\r\nContent-Length: {}\r\n{extra_headers}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    #[tokio::test]
    async fn verified_call_succeeds_against_its_own_pinned_loopback_origin() {
        let addr = spawn_fixture("HTTP/1.1 200 OK", "", "{\"ok\":true}");
        let origin = format!("http://{addr}");
        let url = Url::parse(&format!("http://{addr}/v1/users/me")).unwrap();

        let body = verified_call(reqwest::Method::GET, &url, &origin, true, &[], None)
            .await
            .unwrap();
        assert_eq!(body, b"{\"ok\":true}");
    }

    #[tokio::test]
    async fn verified_call_rejects_an_allowed_origin_that_does_not_match_the_request() {
        let addr = spawn_fixture("HTTP/1.1 200 OK", "", "{}");
        let url = Url::parse(&format!("http://{addr}/rest/api/3/myself")).unwrap();

        let result = verified_call(
            reqwest::Method::GET,
            &url,
            "https://totally-different-origin.example",
            true,
            &[],
            None,
        )
        .await;
        match result {
            Ok(_) => panic!("expected the mismatched allowed_origin to reject the request"),
            Err(message) => assert!(message.contains("origin"), "unexpected error: {message}"),
        }
    }

    #[tokio::test]
    async fn verified_call_refuses_to_follow_a_redirect() {
        let addr = spawn_fixture("HTTP/1.1 302 Found", "Location: http://example.test/\r\n", "");
        let origin = format!("http://{addr}");
        let url = Url::parse(&format!("http://{addr}/")).unwrap();

        let result = verified_call(reqwest::Method::GET, &url, &origin, true, &[], None).await;
        match result {
            Ok(_) => panic!("expected the redirect to be refused"),
            Err(message) => assert!(
                message.to_lowercase().contains("redirect"),
                "unexpected error: {message}"
            ),
        }
    }

    #[tokio::test]
    async fn verified_call_rejects_a_response_over_the_size_cap() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                // Declares a body far larger than MAX_VERIFY_BYTES but never
                // actually sends it — the Content-Length pre-check must
                // reject before any body bytes are read.
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    MAX_VERIFY_BYTES + 1
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        let origin = format!("http://{addr}");
        let url = Url::parse(&format!("http://{addr}/")).unwrap();

        let result = verified_call(reqwest::Method::GET, &url, &origin, true, &[], None).await;
        match result {
            Ok(_) => panic!("expected the oversized response to be rejected"),
            Err(message) => assert!(message.contains("size limit"), "unexpected error: {message}"),
        }
    }

    #[tokio::test]
    async fn verified_call_blocks_a_loopback_destination_when_allow_loopback_is_false() {
        let url = Url::parse("http://127.0.0.1:9/rest/api/3/myself").unwrap();
        let result = verified_call(
            reqwest::Method::GET,
            &url,
            "http://127.0.0.1:9",
            false,
            &[],
            None,
        )
        .await;
        match result {
            Ok(_) => panic!("expected a loopback destination to be blocked by default"),
            Err(message) => assert!(message.contains("blocked"), "unexpected error: {message}"),
        }
    }

    // --- S3 SigV4 primitives --------------------------------------------------

    #[test]
    fn sha256_hex_of_empty_input_matches_the_well_known_constant() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn hmac_sha256_matches_the_rfc_4231_test_case_2_vector() {
        let mac = hex_encode(&hmac_sha256(b"Jefe", b"what do ya want for nothing?"));
        assert_eq!(mac, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    #[test]
    fn sigv4_authorization_has_the_documented_credential_scope_and_a_64_char_hex_signature() {
        let header = sigv4_authorization(
            "HEAD",
            "s3.amazonaws.com",
            "/my-bucket",
            "AKIAIOSFODNN7EXAMPLE",
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "us-east-1",
            "20130524T000000Z",
        );
        assert!(header.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature="
        ));
        let signature = header.rsplit("Signature=").next().unwrap();
        assert_eq!(signature.len(), 64, "signature must be 64 lowercase-hex characters: {signature}");
        assert!(signature.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn validate_s3_bucket_rejects_uppercase_and_too_short_names() {
        assert!(validate_s3_bucket("my-bucket-1").is_ok());
        assert!(validate_s3_bucket("My-Bucket").is_err());
        assert!(validate_s3_bucket("ab").is_err());
    }

    #[test]
    fn validate_s3_region_rejects_empty_and_non_ascii_values() {
        assert!(validate_s3_region("us-east-1").is_ok());
        assert!(validate_s3_region("auto").is_ok());
        assert!(validate_s3_region("").is_err());
        assert!(validate_s3_region("écho").is_err());
    }
}

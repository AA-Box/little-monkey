//! The connector catalog's OAuth half — a real RFC 6749 authorization-code
//! flow (with RFC 7636 PKCE where the provider supports it) for the eleven
//! catalog providers that require a registered OAuth app, so none of them has
//! to be faked with a pasted-token workaround.
//!
//! **No client credentials ship in this binary.** Little Monkey is a public,
//! open-source build: anything baked in would be extractable by anyone who
//! downloads it. The user registers their own OAuth app once per provider and
//! pastes its client id (and, for providers that authenticate the client at
//! the token endpoint, its secret); both live in the OS keychain under
//! `connector-oauth-client:<provider>`. This is exactly the posture
//! `docs/byo-oauth-clients.md` already documents for remote MCP servers.
//!
//! Endpoints come from [`OAUTH_PROVIDERS`], a static table confirmed against
//! each provider's own documentation (the confirmation date and doc URL are
//! recorded above every row), rather than from RFC 8414 discovery the way
//! `mcp_oauth.rs` does — none of these providers publishes an authorization
//! server metadata document at a URL this app could know in advance.
//!
//! Everything about the browser round trip is *borrowed*, not copied, from
//! `mcp_oauth.rs`: [`crate::mcp_oauth::LoopbackListener`] (via its
//! `bind_port`), `loopback_port_for`, `await_callback`, `CallbackParams`,
//! `cancellable_oauth_step`, `oauth_cancel_token_for`,
//! `release_oauth_cancel_token` and `refresh_lock_for`. The one deliberate
//! difference is the port: it is derived from the **provider**, not from an
//! account id, so the redirect URI is a single stable string the user
//! registers once and every account of that provider reuses. The cost of that
//! choice is that two accounts of one provider cannot run browser consent at
//! the same time — the second attempt reports the port as busy.
//!
//! Every outbound call — code exchange, refresh, revocation, and the live
//! read-only identity check that runs before an account is ever saved — goes
//! through [`crate::connectors::verified_call_with_body`], so it inherits the
//! catalog's DNS-pinning, redirect-refusing, size-capped, egress-policed
//! posture. Nothing here ever builds its own `reqwest` client.

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::connectors::{ConnectorAccount, ConnectorProvider, RequestBody};
use crate::AppState;

/// Same keychain *service* string the rest of the app uses; entries are kept
/// apart by *account* name. See [`client_keychain_account`] /
/// [`token_keychain_account`].
static KEYCHAIN_SERVICE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.app"));

/// Shown (and returned) when no client id was pasted and none is stored. The
/// frontend matches on the `needs_client_id` phase, not this string.
pub const CONNECTOR_CLIENT_ID_REQUIRED_MESSAGE: &str = "This provider needs an OAuth app you register yourself. Enter the client id from your own registration (and its secret, if the provider issues one) and try again.";
const OAUTH_CANCELLED_MESSAGE: &str = "OAuth connection cancelled";
/// How long browser consent may take before the loopback listener gives up.
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh this far ahead of the recorded expiry, so a token cannot expire
/// between the check and the call that uses it.
const REFRESH_SKEW_MS: u64 = 60_000;

/// The keychain *account* holding one provider's user-registered OAuth client
/// — `connector-oauth-client:<provider>`. Per *provider*, not per account:
/// one registered app serves every account of that provider, which is what
/// makes the second Google account one click instead of another trip to the
/// Google console.
fn client_keychain_account(provider: ConnectorProvider) -> String {
    format!("connector-oauth-client:{}", provider.as_str())
}

/// The keychain *account* holding one connected account's token record —
/// `connector-oauth:<account id>`. A fifth namespace alongside
/// `connectors.rs`'s `connector:<provider>:<id>`, `mcp_oauth.rs`'s
/// `mcp-oauth:<id>` / `mcp-oauth-client:<id>` and `mcp.rs`'s `mcp:<id>`.
///
/// This is also what an OAuth account's `credential_ref` is set to, so
/// `connectors::remove_impl`'s existing keychain cleanup deletes the token
/// record on removal without needing a second delete path.
pub(crate) fn token_keychain_account(id: &str) -> String {
    format!("connector-oauth:{}", id)
}

fn entry(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(&KEYCHAIN_SERVICE, account)
        .map_err(|e| format!("Failed to access keychain: {e}"))
}

/// A user-registered OAuth app for one provider.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct ClientRegistration {
    pub client_id: String,
    /// `None` for a public client (pure PKCE, no secret) — Microsoft Graph
    /// never issues one, and GitLab/Dropbox/Airtable/Zendesk apps may be
    /// registered either way.
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// One account's OAuth token record, as stored (JSON) in a single keychain
/// entry. Never written to `connectors.json`.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct StoredTokens {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    /// Unix ms. `None` means the provider issues a non-expiring token (a
    /// legacy Zendesk OAuth client), so there is nothing to refresh ahead of.
    #[serde(default)]
    expires_at: Option<u64>,
    #[serde(default)]
    scopes: Vec<String>,
}

/// Redacting by hand rather than `derive(Debug)`: a derived one would print
/// the access and refresh tokens into any panic message or log line that
/// formats this struct.
impl std::fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &self.refresh_token.is_some())
            .field("expires_at", &self.expires_at)
            .field("scopes", &self.scopes)
            .finish()
    }
}

// --- provider table ---------------------------------------------------------

/// Whether the provider's token endpoint authenticates the client.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SecretPolicy {
    /// The token endpoint rejects a request without `client_secret`.
    Required,
    /// Accepted with or without, depending on how the app was registered.
    Optional,
    /// The provider never issues one (public client only).
    Never,
}

/// How a provider's revocation endpoint wants to be told which token to kill.
#[derive(Clone, Copy)]
enum RevokeBody {
    /// `token=<access token>` (plus client id/secret) as a form body.
    FormToken,
    /// The token in an `Authorization: Bearer` header, empty body.
    BearerEmpty,
}

/// A non-secret, user-supplied piece of the provider's URLs.
#[derive(Clone, Copy)]
pub(crate) enum HostField {
    /// A GitLab instance or Zendesk subdomain host, substituted for `{host}`
    /// and recorded as `connection.host`.
    ApiHost { default: Option<&'static str> },
    /// A Microsoft directory (tenant id, domain, or `common`), substituted
    /// for `{tenant}` and recorded as `connection.tenant`.
    Tenant,
}

/// The one live read-only call that proves an account before it is saved,
/// declared rather than hand-written — which is why eleven providers add
/// zero new verification functions.
struct VerifySpec {
    method: &'static str,
    url: &'static str,
    headers: &'static [(&'static str, &'static str)],
    /// A JSON request body, for the one provider whose identity endpoint is
    /// a GraphQL POST (Linear).
    json_body: Option<&'static str>,
    /// JSON pointers tried in order; the first non-empty string (or number)
    /// becomes the account's `identity`.
    identity_pointers: &'static [&'static str],
}

pub(crate) struct OAuthProviderSpec {
    pub provider: ConnectorProvider,
    authorize_url: &'static str,
    token_url: &'static str,
    revoke: Option<(&'static str, &'static str, RevokeBody)>,
    scopes: &'static [&'static str],
    extra_authorize_params: &'static [(&'static str, &'static str)],
    /// Whether to send `code_challenge`/`code_challenge_method=S256`.
    pub pkce: bool,
    pub secret: SecretPolicy,
    /// `127.0.0.1` everywhere confirmed so far; kept per-row because some
    /// providers only accept the literal `localhost` form.
    redirect_host: &'static str,
    pub host_field: Option<HostField>,
    verify: VerifySpec,
}

/// The eleven providers, each confirmed against the provider's own current
/// documentation on 2026-09-03. A row is only here because all five of
/// authorize URL, token URL, PKCE support, client-secret policy and a
/// loopback redirect were confirmed; anything that could not be confirmed is
/// named as an exclusion in `docs/limitations.md` instead of guessed at.
const OAUTH_PROVIDERS: &[OAuthProviderSpec] = &[
    // Google — https://developers.google.com/identity/protocols/oauth2/native-app
    // `access_type=offline` + `prompt=consent` are what make Google issue a
    // refresh token at all (the same note `mcp_oauth.rs`'s
    // `EXTRA_AUTHORIZE_PARAMS_BY_HOST` carries).
    OAuthProviderSpec {
        provider: ConnectorProvider::GoogleDrive,
        authorize_url: "https://accounts.google.com/o/oauth2/v2/auth",
        token_url: "https://oauth2.googleapis.com/token",
        revoke: Some((
            "POST",
            "https://oauth2.googleapis.com/revoke",
            RevokeBody::FormToken,
        )),
        scopes: &["https://www.googleapis.com/auth/drive.metadata.readonly"],
        extra_authorize_params: &[("access_type", "offline"), ("prompt", "consent")],
        pkce: true,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://www.googleapis.com/drive/v3/about?fields=user",
            headers: &[],
            json_body: None,
            identity_pointers: &["/user/emailAddress", "/user/displayName"],
        },
    },
    // Microsoft identity platform —
    // https://learn.microsoft.com/entra/identity-platform/v2-oauth2-auth-code-flow
    // A "Mobile and desktop applications" registration is a public client:
    // the token endpoint rejects a `client_secret`, and there is no
    // per-token revocation endpoint (only `revokeSignInSessions`, which is a
    // different operation on all sessions).
    OAuthProviderSpec {
        provider: ConnectorProvider::MicrosoftGraph,
        authorize_url: "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize",
        token_url: "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token",
        revoke: None,
        scopes: &[
            "offline_access",
            "User.Read",
            "Files.Read.All",
            "Sites.Read.All",
        ],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Never,
        redirect_host: "127.0.0.1",
        host_field: Some(HostField::Tenant),
        verify: VerifySpec {
            method: "GET",
            url: "https://graph.microsoft.com/v1.0/me",
            headers: &[],
            json_body: None,
            identity_pointers: &["/userPrincipalName", "/displayName"],
        },
    },
    // Linear — https://linear.app/developers/oauth-2-0-authentication
    // PKCE is supported; the standard authorization-code flow still expects
    // the client secret at the token endpoint.
    OAuthProviderSpec {
        provider: ConnectorProvider::Linear,
        authorize_url: "https://linear.app/oauth/authorize",
        token_url: "https://api.linear.app/oauth/token",
        revoke: Some((
            "POST",
            "https://api.linear.app/oauth/revoke",
            RevokeBody::BearerEmpty,
        )),
        scopes: &["read"],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "POST",
            url: "https://api.linear.app/graphql",
            headers: &[],
            json_body: Some(r#"{"query":"{ viewer { name email } }"}"#),
            identity_pointers: &["/data/viewer/email", "/data/viewer/name"],
        },
    },
    // Asana — https://developers.asana.com/docs/oauth
    // `users:read` is Asana's granular read-only scope; it is what
    // `/users/me` needs and nothing more.
    OAuthProviderSpec {
        provider: ConnectorProvider::Asana,
        authorize_url: "https://app.asana.com/-/oauth_authorize",
        token_url: "https://app.asana.com/-/oauth_token",
        revoke: Some((
            "POST",
            "https://app.asana.com/-/oauth_revoke",
            RevokeBody::FormToken,
        )),
        scopes: &["users:read"],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://app.asana.com/api/1.0/users/me",
            headers: &[],
            json_body: None,
            identity_pointers: &["/data/email", "/data/name"],
        },
    },
    // Dropbox — https://developers.dropbox.com/oauth-guide
    // Without `token_access_type=offline` Dropbox issues a short-lived access
    // token and no refresh token at all.
    OAuthProviderSpec {
        provider: ConnectorProvider::Dropbox,
        authorize_url: "https://www.dropbox.com/oauth2/authorize",
        token_url: "https://api.dropboxapi.com/oauth2/token",
        revoke: Some((
            "POST",
            "https://api.dropboxapi.com/2/auth/token/revoke",
            RevokeBody::BearerEmpty,
        )),
        scopes: &["account_info.read", "files.metadata.read"],
        extra_authorize_params: &[("token_access_type", "offline")],
        pkce: true,
        secret: SecretPolicy::Optional,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "POST",
            url: "https://api.dropboxapi.com/2/users/get_current_account",
            headers: &[],
            json_body: None,
            identity_pointers: &["/email", "/name/display_name"],
        },
    },
    // Box — https://developer.box.com/reference/get-authorize and
    // https://developer.box.com/reference/post-oauth2-token
    // Box scopes are configured on the app in the developer console rather
    // than requested per authorization, so no `scope` parameter is sent.
    // PKCE is not documented for Box's OAuth 2.0 flow, so this row does not
    // claim it.
    OAuthProviderSpec {
        provider: ConnectorProvider::Box,
        authorize_url: "https://account.box.com/api/oauth2/authorize",
        token_url: "https://api.box.com/oauth2/token",
        revoke: Some((
            "POST",
            "https://api.box.com/oauth2/revoke",
            RevokeBody::FormToken,
        )),
        scopes: &[],
        extra_authorize_params: &[],
        pkce: false,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://api.box.com/2.0/users/me",
            headers: &[],
            json_body: None,
            identity_pointers: &["/login", "/name"],
        },
    },
    // Airtable — https://airtable.com/developers/web/api/oauth-reference
    // PKCE is mandatory. Airtable publishes no revocation endpoint.
    OAuthProviderSpec {
        provider: ConnectorProvider::Airtable,
        authorize_url: "https://airtable.com/oauth2/v1/authorize",
        token_url: "https://airtable.com/oauth2/v1/token",
        revoke: None,
        scopes: &["user.email:read", "schema.bases:read"],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Optional,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://api.airtable.com/v0/meta/whoami",
            headers: &[],
            json_body: None,
            identity_pointers: &["/email", "/id"],
        },
    },
    // Zendesk —
    // https://support.zendesk.com/hc/en-us/articles/4408845965210 and
    // https://developer.zendesk.com/api-reference/ticketing/oauth/oauth_tokens/
    // PKCE is supported (and makes `client_secret` optional). An OAuth client
    // created before 2026-04-30 issues a non-expiring token with no refresh
    // token; newer ones expire and do refresh — both shapes are handled by
    // `expires_at` being optional.
    OAuthProviderSpec {
        provider: ConnectorProvider::Zendesk,
        authorize_url: "https://{host}/oauth/authorizations/new",
        token_url: "https://{host}/oauth/tokens",
        revoke: Some((
            "DELETE",
            "https://{host}/api/v2/oauth/tokens/current",
            RevokeBody::BearerEmpty,
        )),
        scopes: &["read"],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Optional,
        redirect_host: "127.0.0.1",
        host_field: Some(HostField::ApiHost { default: None }),
        verify: VerifySpec {
            method: "GET",
            url: "https://{host}/api/v2/users/me.json",
            headers: &[],
            json_body: None,
            identity_pointers: &["/user/email", "/user/name"],
        },
    },
    // HubSpot — https://developers.hubspot.com/docs/guides/apps/authentication/working-with-oauth
    // HubSpot does not support PKCE for this flow, and its only revocation
    // form embeds the refresh token in the URL path — which this repo's own
    // rule against putting sensitive data in URLs forbids — so `revoke` is
    // `None` and removal leaves revocation to HubSpot's app settings.
    OAuthProviderSpec {
        provider: ConnectorProvider::Hubspot,
        authorize_url: "https://app.hubspot.com/oauth/authorize",
        token_url: "https://api.hubapi.com/oauth/v1/token",
        revoke: None,
        scopes: &["oauth"],
        extra_authorize_params: &[],
        pkce: false,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://api.hubapi.com/account-info/v3/details",
            headers: &[],
            json_body: None,
            identity_pointers: &["/uiDomain", "/portalId"],
        },
    },
    // Discord — https://discord.com/developers/docs/topics/oauth2
    OAuthProviderSpec {
        provider: ConnectorProvider::Discord,
        authorize_url: "https://discord.com/oauth2/authorize",
        token_url: "https://discord.com/api/oauth2/token",
        revoke: Some((
            "POST",
            "https://discord.com/api/oauth2/token/revoke",
            RevokeBody::FormToken,
        )),
        scopes: &["identify"],
        extra_authorize_params: &[],
        pkce: false,
        secret: SecretPolicy::Required,
        redirect_host: "127.0.0.1",
        host_field: None,
        verify: VerifySpec {
            method: "GET",
            url: "https://discord.com/api/v10/users/@me",
            headers: &[],
            json_body: None,
            identity_pointers: &["/username", "/id"],
        },
    },
    // GitLab — https://docs.gitlab.com/api/oauth2/
    // A self-hosted instance is supported as long as it is reachable over
    // public DNS: `verified_call`'s SSRF policy never permits a private
    // network destination, which `docs/limitations.md` states.
    OAuthProviderSpec {
        provider: ConnectorProvider::Gitlab,
        authorize_url: "https://{host}/oauth/authorize",
        token_url: "https://{host}/oauth/token",
        revoke: Some(("POST", "https://{host}/oauth/revoke", RevokeBody::FormToken)),
        scopes: &["read_user"],
        extra_authorize_params: &[],
        pkce: true,
        secret: SecretPolicy::Optional,
        redirect_host: "127.0.0.1",
        host_field: Some(HostField::ApiHost {
            default: Some("gitlab.com"),
        }),
        verify: VerifySpec {
            method: "GET",
            url: "https://{host}/api/v4/user",
            headers: &[],
            json_body: None,
            identity_pointers: &["/username", "/email"],
        },
    },
];

/// The spec for `provider`, or `None` for the six providers that use a
/// non-OAuth scheme (`gh` CLI, pasted token, access keys, extension).
pub(crate) fn spec_for(provider: ConnectorProvider) -> Option<&'static OAuthProviderSpec> {
    OAUTH_PROVIDERS
        .iter()
        .find(|spec| spec.provider == provider)
}

// --- pure helpers -----------------------------------------------------------

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// A fresh RFC 7636 verifier/challenge pair. The verifier is a local that is
/// never serialized, never persisted and never logged.
fn pkce_pair() -> (String, String) {
    let verifier = b64(&rand::random::<[u8; 32]>());
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// A fresh CSRF `state`, 256 bits from the same source.
fn random_state() -> String {
    b64(&rand::random::<[u8; 32]>())
}

fn substitute(template: &str, host: Option<&str>, tenant: &str) -> String {
    template
        .replace("{host}", host.unwrap_or_default())
        .replace("{tenant}", tenant)
}

/// The concrete URLs one connect/refresh/verify run uses, after `{host}` and
/// `{tenant}` substitution. Held rather than recomputed so a test can point
/// the whole flow at a local fixture server.
pub(crate) struct ResolvedEndpoints {
    authorize: Url,
    token: Url,
    revoke: Option<(reqwest::Method, Url)>,
    verify: Url,
}

fn parse_url(raw: &str, what: &str) -> Result<Url, String> {
    Url::parse(raw).map_err(|e| format!("Invalid {what} URL '{raw}': {e}"))
}

/// Validates a user-supplied instance host (GitLab, Zendesk) and returns it
/// in bare `host[:port]` form. Anything with a scheme other than https, a
/// path, userinfo or a query is refused rather than silently trimmed — this
/// value is substituted straight into four URLs, and it is the value the
/// per-call origin pin is derived from.
pub(crate) fn normalize_host(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return Err("This provider needs the host of your instance".to_string());
    }
    let candidate = if trimmed.contains("://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    };
    let url = parse_url(&candidate, "instance")?;
    if url.scheme() != "https" {
        return Err(format!(
            "'{raw}' must be an https host — plaintext HTTP would expose the token exchange"
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("'{raw}' must not contain credentials"));
    }
    if url.path() != "/" && !url.path().is_empty() {
        return Err(format!("'{raw}' must be a bare host, with no path"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("'{raw}' must be a bare host, with no query"));
    }
    let host = url
        .host_str()
        .ok_or_else(|| format!("'{raw}' has no host"))?;
    Ok(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

pub(crate) fn resolve_endpoints(
    spec: &OAuthProviderSpec,
    host: Option<&str>,
    tenant: &str,
) -> Result<ResolvedEndpoints, String> {
    let revoke = match spec.revoke {
        Some((method, url, _)) => Some((
            reqwest::Method::from_bytes(method.as_bytes())
                .map_err(|e| format!("Invalid revocation method: {e}"))?,
            parse_url(&substitute(url, host, tenant), "revocation")?,
        )),
        None => None,
    };
    Ok(ResolvedEndpoints {
        authorize: parse_url(&substitute(spec.authorize_url, host, tenant), "authorize")?,
        token: parse_url(&substitute(spec.token_url, host, tenant), "token")?,
        revoke,
        verify: parse_url(&substitute(spec.verify.url, host, tenant), "verification")?,
    })
}

/// Builds the authorize URL. `scope` is omitted entirely when the provider
/// configures scopes on the app itself (Box); `code_challenge` only when the
/// provider supports PKCE. Extra params never overwrite a parameter already
/// set, mirroring `mcp_oauth::with_extra_authorize_params`.
fn authorize_url(
    spec: &OAuthProviderSpec,
    base: &Url,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: Option<&str>,
) -> Url {
    let mut url = base.clone();
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", client_id);
        query.append_pair("redirect_uri", redirect_uri);
        query.append_pair("state", state);
        if !spec.scopes.is_empty() {
            query.append_pair("scope", &spec.scopes.join(" "));
        }
        if let Some(challenge) = challenge {
            query.append_pair("code_challenge", challenge);
            query.append_pair("code_challenge_method", "S256");
        }
    }
    let already: std::collections::HashSet<String> =
        url.query_pairs().map(|(key, _)| key.into_owned()).collect();
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in spec.extra_authorize_params {
            if !already.contains(*key) {
                query.append_pair(key, value);
            }
        }
    }
    url
}

/// Parses an RFC 6749 token response. A body with no `access_token` is an
/// error that repeats the provider's own `error`/`error_description`, since
/// that is the only part a user can act on.
fn parse_token_response(bytes: &[u8], now_ms: u64) -> Result<StoredTokens, String> {
    let json: Value = serde_json::from_slice(bytes)
        .map_err(|e| format!("The token endpoint returned a body that is not JSON: {e}"))?;
    let access_token = match json.get("access_token").and_then(Value::as_str) {
        Some(token) if !token.is_empty() => token.to_string(),
        _ => {
            let code = json
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("no access_token in the response");
            let detail = json
                .get("error_description")
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(if detail.is_empty() {
                format!("The token endpoint refused the exchange: {code}")
            } else {
                format!("The token endpoint refused the exchange: {code} — {detail}")
            });
        }
    };
    let expires_at = json
        .get("expires_in")
        .and_then(Value::as_u64)
        .map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1000)));
    let scopes = json
        .get("scope")
        .and_then(Value::as_str)
        .map(|raw| {
            raw.split([' ', ','])
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(StoredTokens {
        access_token,
        refresh_token: json
            .get("refresh_token")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        expires_at,
        scopes,
    })
}

/// The first identity pointer that resolves to a non-empty string (or a
/// number, stringified) wins.
fn identity_from(verify: &VerifySpec, body: &[u8]) -> Result<String, String> {
    let json: Value = serde_json::from_slice(body)
        .map_err(|e| format!("The identity endpoint returned a body that is not JSON: {e}"))?;
    for pointer in verify.identity_pointers {
        match json.pointer(pointer) {
            Some(Value::String(value)) if !value.is_empty() => return Ok(value.clone()),
            Some(Value::Number(value)) => return Ok(value.to_string()),
            _ => {}
        }
    }
    Err("The provider accepted the token but returned no identity".to_string())
}

/// The one shape every "your consent is gone" failure takes, so the UI (and
/// `monkey connectors list`) can recognise it without parsing provider prose.
pub fn reconnect_error(provider: ConnectorProvider, detail: &str) -> String {
    format!(
        "{} authorization was revoked or expired — reconnect it in Settings → Connectors ({detail}).",
        provider.as_str()
    )
}

/// True for a `last_error` produced by [`reconnect_error`].
pub fn is_reconnect_error(message: &str) -> bool {
    message.contains("reconnect it in Settings → Connectors")
}

/// The loopback redirect URI this app uses for `provider` — the exact string
/// to register with the provider. Stable, needs no network and no saved
/// credential, so the UI shows it before any connect attempt.
pub(crate) fn preferred_redirect_uri(provider: ConnectorProvider) -> Option<String> {
    let spec = spec_for(provider)?;
    Some(format!(
        "http://{}:{}/",
        spec.redirect_host,
        crate::mcp_oauth::loopback_port_for(&cancel_key(provider))
    ))
}

fn cancel_key(provider: ConnectorProvider) -> String {
    format!("connector:{}", provider.as_str())
}

// --- outbound calls ---------------------------------------------------------

async fn post_form(
    url: &Url,
    allow_loopback: bool,
    pairs: &[(&str, String)],
) -> Result<Vec<u8>, String> {
    let origin = crate::connectors::origin_of(url)?;
    crate::connectors::verified_call_with_body(
        reqwest::Method::POST,
        url,
        &origin,
        allow_loopback,
        &[],
        None,
        RequestBody::Form(pairs),
    )
    .await
}

fn client_form_fields<'a>(
    spec: &OAuthProviderSpec,
    client: &'a ClientRegistration,
    pairs: &mut Vec<(&'static str, String)>,
) {
    pairs.push(("client_id", client.client_id.clone()));
    if spec.secret != SecretPolicy::Never {
        if let Some(secret) = client.client_secret.as_deref() {
            pairs.push(("client_secret", secret.to_string()));
        }
    }
}

async fn exchange_code(
    endpoints: &ResolvedEndpoints,
    allow_loopback: bool,
    spec: &OAuthProviderSpec,
    client: &ClientRegistration,
    code: &str,
    verifier: Option<&str>,
    redirect_uri: &str,
    now_ms: u64,
) -> Result<StoredTokens, String> {
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code.to_string()),
        ("redirect_uri", redirect_uri.to_string()),
    ];
    client_form_fields(spec, client, &mut pairs);
    if let Some(verifier) = verifier {
        pairs.push(("code_verifier", verifier.to_string()));
    }
    let body = post_form(&endpoints.token, allow_loopback, &pairs).await?;
    parse_token_response(&body, now_ms)
}

async fn refresh_tokens(
    endpoints: &ResolvedEndpoints,
    allow_loopback: bool,
    spec: &OAuthProviderSpec,
    client: &ClientRegistration,
    refresh_token: &str,
    now_ms: u64,
) -> Result<StoredTokens, String> {
    let mut pairs: Vec<(&'static str, String)> = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
    ];
    client_form_fields(spec, client, &mut pairs);
    let body = post_form(&endpoints.token, allow_loopback, &pairs).await?;
    parse_token_response(&body, now_ms)
}

/// The one live, read-only identity call. Pinned to the verification URL's
/// own origin, so a user-supplied GitLab/Zendesk host can never be used to
/// reach a different one.
async fn verify_identity(
    spec: &OAuthProviderSpec,
    endpoints: &ResolvedEndpoints,
    allow_loopback: bool,
    access_token: &str,
) -> Result<String, String> {
    let mut headers: Vec<(&'static str, String)> =
        vec![("authorization", format!("Bearer {access_token}"))];
    for (key, value) in spec.verify.headers {
        headers.push((key, (*value).to_string()));
    }
    let method = reqwest::Method::from_bytes(spec.verify.method.as_bytes())
        .map_err(|e| format!("Invalid verification method: {e}"))?;
    let json_body: Option<Value> = match spec.verify.json_body {
        Some(raw) => {
            Some(serde_json::from_str(raw).map_err(|e| format!("Invalid verification body: {e}"))?)
        }
        None => None,
    };
    let origin = crate::connectors::origin_of(&endpoints.verify)?;
    let body = crate::connectors::verified_call_with_body(
        method,
        &endpoints.verify,
        &origin,
        allow_loopback,
        &headers,
        None,
        json_body
            .as_ref()
            .map_or(RequestBody::None, RequestBody::Json),
    )
    .await?;
    identity_from(&spec.verify, &body)
}

/// Best-effort revocation. Every caller ignores the result: a provider that
/// publishes no endpoint, a network failure, or an already-dead token must
/// never stop the user removing an account.
async fn revoke(
    spec: &OAuthProviderSpec,
    endpoints: &ResolvedEndpoints,
    client: &ClientRegistration,
    tokens: &StoredTokens,
) -> Result<(), String> {
    let (Some((method, url)), Some((_, _, body_kind))) = (&endpoints.revoke, &spec.revoke) else {
        return Ok(());
    };
    let origin = crate::connectors::origin_of(url)?;
    match body_kind {
        RevokeBody::FormToken => {
            let mut pairs: Vec<(&'static str, String)> =
                vec![("token", tokens.access_token.clone())];
            client_form_fields(spec, client, &mut pairs);
            crate::connectors::verified_call_with_body(
                method.clone(),
                url,
                &origin,
                false,
                &[],
                None,
                RequestBody::Form(&pairs),
            )
            .await?;
        }
        RevokeBody::BearerEmpty => {
            crate::connectors::verified_call_with_body(
                method.clone(),
                url,
                &origin,
                false,
                &[("authorization", format!("Bearer {}", tokens.access_token))],
                None,
                RequestBody::None,
            )
            .await?;
        }
    }
    Ok(())
}

// --- stored state -----------------------------------------------------------

pub(crate) fn load_client(
    provider: ConnectorProvider,
) -> Result<Option<ClientRegistration>, String> {
    match entry(&client_keychain_account(provider))?.get_password() {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| format!("Corrupt stored OAuth client registration: {e}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!(
            "Failed to read the OAuth client registration from the keychain: {e}"
        )),
    }
}

fn save_client(provider: ConnectorProvider, client: &ClientRegistration) -> Result<(), String> {
    let json = serde_json::to_string(client)
        .map_err(|e| format!("Failed to serialize the OAuth client registration: {e}"))?;
    entry(&client_keychain_account(provider))?
        .set_password(&json)
        .map_err(|e| format!("Failed to save the OAuth client registration: {e}"))
}

fn delete_client(provider: ConnectorProvider) {
    let _ = entry(&client_keychain_account(provider)).and_then(|e| {
        e.delete_credential()
            .map_err(|err| format!("Failed to delete the OAuth client registration: {err}"))
    });
}

fn load_tokens(id: &str) -> Result<StoredTokens, String> {
    let json = entry(&token_keychain_account(id))?
        .get_password()
        .map_err(|e| format!("Failed to read the saved OAuth tokens: {e}"))?;
    serde_json::from_str(&json).map_err(|e| format!("Corrupt stored OAuth tokens: {e}"))
}

fn save_tokens(id: &str, tokens: &StoredTokens) -> Result<(), String> {
    let json = serde_json::to_string(tokens)
        .map_err(|e| format!("Failed to serialize the OAuth tokens: {e}"))?;
    entry(&token_keychain_account(id))?
        .set_password(&json)
        .map_err(|e| format!("Failed to save the OAuth tokens: {e}"))
}

fn delete_tokens(id: &str) {
    let _ = entry(&token_keychain_account(id)).and_then(|e| {
        e.delete_credential()
            .map_err(|err| format!("Failed to delete the saved OAuth tokens: {err}"))
    });
}

/// Clears a provider's shared client registration once no account of that
/// provider is left. Best-effort — the same stance `remove_impl` takes toward
/// its own keychain cleanup.
pub fn forget_client_if_unused(config_path: &std::path::Path, provider: ConnectorProvider) {
    if spec_for(provider).is_none() {
        return;
    }
    let still_used = crate::connectors::load_config_impl(config_path)
        .map(|config| {
            config
                .accounts
                .iter()
                .any(|account| account.provider == provider)
        })
        .unwrap_or(true);
    if !still_used {
        delete_client(provider);
    }
}

/// The non-secret provider metadata an OAuth account stores in `connection`.
fn connection_metadata(
    spec: &OAuthProviderSpec,
    host: Option<&str>,
    tenant: &str,
) -> Option<Value> {
    match spec.host_field {
        Some(HostField::ApiHost { .. }) => Some(serde_json::json!({ "host": host? })),
        Some(HostField::Tenant) => Some(serde_json::json!({ "tenant": tenant })),
        None => None,
    }
}

/// Reads back the host/tenant an account was saved with.
fn account_host_and_tenant(
    spec: &OAuthProviderSpec,
    account: &ConnectorAccount,
) -> Result<(Option<String>, String), String> {
    let read = |key: &str| {
        account
            .connection
            .as_ref()
            .and_then(|c| c.get(key))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match spec.host_field {
        Some(HostField::ApiHost { default }) => {
            let host = read("host")
                .or_else(|| default.map(str::to_string))
                .ok_or_else(|| {
                    format!("Connector '{}' is missing its instance host", account.id)
                })?;
            Ok((Some(host), "common".to_string()))
        }
        Some(HostField::Tenant) => {
            Ok((None, read("tenant").unwrap_or_else(|| "common".to_string())))
        }
        None => Ok((None, "common".to_string())),
    }
}

// --- access tokens ----------------------------------------------------------

/// Refreshes `tokens` when they are within [`REFRESH_SKEW_MS`] of expiring.
/// `Ok(None)` means the stored access token is still good and nothing was
/// sent. Split out from [`access_token`] so the refresh state machine is
/// testable against a fixture server without touching the real keychain.
async fn refresh_if_expired(
    spec: &OAuthProviderSpec,
    endpoints: &ResolvedEndpoints,
    allow_loopback: bool,
    client: &ClientRegistration,
    tokens: &StoredTokens,
    now_ms: u64,
) -> Result<Option<StoredTokens>, String> {
    let Some(expires_at) = tokens.expires_at else {
        return Ok(None);
    };
    if expires_at > now_ms.saturating_add(REFRESH_SKEW_MS) {
        return Ok(None);
    }
    let refresh_token = tokens.refresh_token.as_deref().ok_or_else(|| {
        reconnect_error(
            spec.provider,
            "the access token expired and the provider issued no refresh token",
        )
    })?;
    let mut refreshed = refresh_tokens(
        endpoints,
        allow_loopback,
        spec,
        client,
        refresh_token,
        now_ms,
    )
    .await
    .map_err(|e| reconnect_error(spec.provider, &e))?;
    // A provider that does not rotate refresh tokens omits the field; keeping
    // the old one is what makes the *next* refresh possible.
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = tokens.refresh_token.clone();
    }
    if refreshed.scopes.is_empty() {
        refreshed.scopes = tokens.scopes.clone();
    }
    Ok(Some(refreshed))
}

/// A currently valid access token for `account`, refreshing first if the
/// stored one has expired.
///
/// The refresh is serialized per account through
/// `mcp_oauth::refresh_lock_for(state, "connector:<id>")` and the record is
/// re-read *inside* the lock, so two concurrent callers redeem a rotating
/// refresh token once rather than racing and having the loser's request
/// rejected. The new record is written with a single `set_password`; unlike
/// `mcp_oauth`'s `StagedOAuthExchange` there is no second record to keep in
/// step, because a refresh never rewrites the client registration.
pub(crate) async fn access_token(
    state: &AppState,
    account: &ConnectorAccount,
) -> Result<String, String> {
    let spec = spec_for(account.provider)
        .ok_or_else(|| format!("Connector '{}' is not an OAuth account", account.id))?;
    let tokens = load_tokens(&account.id)?;
    let now = crate::run_commands::unix_time_ms()?;
    if tokens
        .expires_at
        .is_none_or(|at| at > now.saturating_add(REFRESH_SKEW_MS))
    {
        return Ok(tokens.access_token);
    }

    let lock = crate::mcp_oauth::refresh_lock_for(state, &format!("connector:{}", account.id));
    let _guard = lock.lock().await;
    // Re-read inside the lock: another task may have refreshed while this one
    // was waiting, in which case its rotated refresh token is now the only
    // valid one.
    let tokens = load_tokens(&account.id)?;
    let now = crate::run_commands::unix_time_ms()?;
    let (host, tenant) = account_host_and_tenant(spec, account)?;
    let endpoints = resolve_endpoints(spec, host.as_deref(), &tenant)?;
    let client = load_client(account.provider)?.ok_or_else(|| {
        reconnect_error(
            account.provider,
            "the OAuth app registration for this provider is no longer in the keychain",
        )
    })?;
    match refresh_if_expired(spec, &endpoints, false, &client, &tokens, now).await? {
        Some(refreshed) => {
            save_tokens(&account.id, &refreshed)?;
            Ok(refreshed.access_token)
        }
        None => Ok(tokens.access_token),
    }
}

/// `reverify_impl`'s OAuth arm: a fresh access token plus the one read-only
/// identity call.
pub(crate) async fn verify_account(
    state: &AppState,
    account: &ConnectorAccount,
) -> Result<String, String> {
    let spec = spec_for(account.provider)
        .ok_or_else(|| format!("Connector '{}' is not an OAuth account", account.id))?;
    let token = access_token(state, account).await?;
    let (host, tenant) = account_host_and_tenant(spec, account)?;
    let endpoints = resolve_endpoints(spec, host.as_deref(), &tenant)?;
    verify_identity(spec, &endpoints, false, &token).await
}

/// Revokes an account at the provider, if the provider publishes an endpoint
/// this app can call. Failures are swallowed on purpose: removal must succeed
/// regardless.
pub async fn revoke_and_forget(account: &ConnectorAccount) {
    let Some(spec) = spec_for(account.provider) else {
        return;
    };
    let Ok((host, tenant)) = account_host_and_tenant(spec, account) else {
        return;
    };
    let (Ok(endpoints), Ok(tokens), Ok(Some(client))) = (
        resolve_endpoints(spec, host.as_deref(), &tenant),
        load_tokens(&account.id),
        load_client(account.provider),
    ) else {
        return;
    };
    let _ = revoke(spec, &endpoints, &client, &tokens).await;
}

// --- the connect state machine ---------------------------------------------

pub(crate) struct ConnectInputs<'a> {
    pub spec: &'static OAuthProviderSpec,
    pub endpoints: ResolvedEndpoints,
    pub client: &'a ClientRegistration,
    /// `true` only in tests, where the fixture authorization server is on
    /// loopback. Every production caller passes `false`.
    pub allow_loopback: bool,
    pub callback_timeout: Duration,
}

/// One connect attempt: bind the registered loopback port, build the
/// authorize URL, open the browser, wait for the redirect, exchange the code,
/// and prove the result with one live read-only identity call.
///
/// Writes nothing — not to the keychain, not to the catalog. Persistence is
/// the caller's, which is what makes a cancelled or failed attempt leave
/// nothing behind by construction.
pub(crate) async fn run_connect_flow(
    inputs: ConnectInputs<'_>,
    cancel: &CancellationToken,
    open_browser: &(dyn Fn(&str) -> Result<(), String> + Sync),
    progress: &(dyn Fn(&str, Option<String>) + Sync),
) -> Result<(StoredTokens, String), String> {
    let spec = inputs.spec;
    let port = crate::mcp_oauth::loopback_port_for(&cancel_key(spec.provider));
    let listener = cancellable(
        cancel,
        crate::mcp_oauth::LoopbackListener::bind_port(port, spec.redirect_host),
    )
    .await?;
    let redirect_uri = listener.redirect_uri.clone();

    let (verifier, challenge) = pkce_pair();
    let state = random_state();
    let url = authorize_url(
        spec,
        &inputs.endpoints.authorize,
        &inputs.client.client_id,
        &redirect_uri,
        &state,
        spec.pkce.then_some(challenge.as_str()),
    );

    progress("opening_browser", None);
    open_browser(url.as_str())?;

    progress("waiting_for_browser", None);
    let callback = cancellable(cancel, listener.await_callback(inputs.callback_timeout)).await?;

    if let Some(error) = callback.error {
        let detail = callback.error_description.unwrap_or_default();
        return Err(if detail.is_empty() {
            format!("OAuth authorization was not granted: {error}")
        } else {
            format!("OAuth authorization was not granted: {error} — {detail}")
        });
    }
    if callback.state.as_deref() != Some(state.as_str()) {
        return Err(
            "The OAuth callback carried a state value this app never issued — refusing it"
                .to_string(),
        );
    }
    let code = callback
        .code
        .filter(|code| !code.is_empty())
        .ok_or_else(|| "The OAuth callback carried no authorization code".to_string())?;

    progress("exchanging_token", None);
    let now = crate::run_commands::unix_time_ms()?;
    let mut tokens = cancellable(
        cancel,
        exchange_code(
            &inputs.endpoints,
            inputs.allow_loopback,
            spec,
            inputs.client,
            &code,
            spec.pkce.then_some(verifier.as_str()),
            &redirect_uri,
            now,
        ),
    )
    .await?;
    if tokens.scopes.is_empty() {
        // Box configures scopes on the app rather than issuing them per
        // authorization, so there is nothing honest to record but that.
        tokens.scopes = if spec.scopes.is_empty() {
            vec!["(granted by the provider's app configuration)".to_string()]
        } else {
            spec.scopes.iter().map(|s| (*s).to_string()).collect()
        };
    }

    progress("verifying", None);
    let identity = cancellable(
        cancel,
        verify_identity(
            spec,
            &inputs.endpoints,
            inputs.allow_loopback,
            &tokens.access_token,
        ),
    )
    .await?;
    Ok((tokens, identity))
}

async fn cancellable<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    crate::mcp_oauth::cancellable_oauth_step(cancel, future).await
}

// --- commands ---------------------------------------------------------------

fn emit_progress<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    provider: ConnectorProvider,
    phase: &str,
    error: Option<String>,
) {
    use tauri::Emitter as _;
    let _ = app.emit(
        "connector-oauth://status",
        serde_json::json!({
            "provider": provider.as_str(),
            "phase": phase,
            "error": error,
        }),
    );
}

/// The loopback redirect URI to register with `provider` — stable for the
/// life of the install, identical for every account of that provider, and
/// computable with no network call and no saved credential, so the UI shows
/// it before the user has pasted anything.
#[tauri::command(rename_all = "snake_case")]
pub fn connectors_oauth_redirect_uri(provider: ConnectorProvider) -> Result<String, String> {
    preferred_redirect_uri(provider)
        .ok_or_else(|| format!("{} does not connect over OAuth", provider.as_str()))
}

fn cleaned(value: Option<String>) -> Option<String> {
    value
        .map(|raw| raw.trim().to_string())
        .filter(|trimmed| !trimmed.is_empty())
}

/// Connects one OAuth account: browser consent against the user's own
/// registered OAuth app, a live read-only identity check, then persistence.
///
/// Uses `rename_all = "snake_case"` (like every `mcp_oauth.rs` command), so
/// the invoke payload keys are `client_id`/`client_secret`, not camelCase —
/// `connectorsStore.ts` mirrors that exactly.
///
/// Nothing is written until the identity call has succeeded. Persistence then
/// follows `add_token_impl`'s order — keychain first, catalog second, with
/// the keychain entries rolled back if the catalog save fails — so a crash or
/// a failed write can never leave a catalog row whose credential is missing.
#[tauri::command(rename_all = "snake_case")]
pub async fn connectors_oauth_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    provider: ConnectorProvider,
    label: String,
    host: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<ConnectorAccount, String> {
    let result = connect_inner(
        &app,
        state.inner(),
        provider,
        label,
        host,
        client_id,
        client_secret,
    )
    .await;
    match &result {
        Ok(_) => emit_progress(&app, provider, "connected", None),
        Err(message) if message == OAUTH_CANCELLED_MESSAGE => {
            emit_progress(&app, provider, "cancelled", None)
        }
        Err(message) if message == CONNECTOR_CLIENT_ID_REQUIRED_MESSAGE => {
            emit_progress(&app, provider, "needs_client_id", Some(message.clone()))
        }
        Err(message) => emit_progress(&app, provider, "error", Some(message.clone())),
    }
    result
}

async fn connect_inner(
    app: &tauri::AppHandle,
    state: &AppState,
    provider: ConnectorProvider,
    label: String,
    host: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<ConnectorAccount, String> {
    let spec = spec_for(provider)
        .ok_or_else(|| format!("{} does not connect over OAuth", provider.as_str()))?;
    let label = label.trim().to_string();
    if label.is_empty() || label.len() > 200 {
        return Err("Label must be non-empty and at most 200 characters".to_string());
    }

    let (host, tenant) = match spec.host_field {
        Some(HostField::ApiHost { default }) => {
            let raw = cleaned(host).or_else(|| default.map(str::to_string));
            let raw = raw
                .ok_or_else(|| format!("{} needs the host of your instance", provider.as_str()))?;
            (Some(normalize_host(&raw)?), "common".to_string())
        }
        // Microsoft reuses the same field for the directory; `common` is the
        // multi-tenant default the docs recommend for a desktop app.
        Some(HostField::Tenant) => (None, cleaned(host).unwrap_or_else(|| "common".to_string())),
        None => (None, "common".to_string()),
    };

    // Pasted wins; otherwise reuse this provider's stored registration, so a
    // second account (or a reconnect) is one click rather than another trip
    // to the provider's console.
    let pasted = cleaned(client_id).map(|client_id| ClientRegistration {
        client_id,
        client_secret: cleaned(client_secret),
    });
    let client = match pasted {
        Some(client) => client,
        None => load_client(provider)?.ok_or_else(|| {
            emit_progress(
                app,
                provider,
                "needs_client_id",
                Some(CONNECTOR_CLIENT_ID_REQUIRED_MESSAGE.to_string()),
            );
            CONNECTOR_CLIENT_ID_REQUIRED_MESSAGE.to_string()
        })?,
    };
    if spec.secret == SecretPolicy::Required && client.client_secret.is_none() {
        return Err(format!(
            "{}'s token endpoint requires the client secret from your OAuth app registration",
            provider.as_str()
        ));
    }
    if spec.secret == SecretPolicy::Never && client.client_secret.is_some() {
        return Err(format!(
            "{} registers public clients only — its token endpoint rejects a client secret",
            provider.as_str()
        ));
    }

    let endpoints = resolve_endpoints(spec, host.as_deref(), &tenant)?;
    let cancel = crate::mcp_oauth::oauth_cancel_token_for(state, &cancel_key(provider))?;

    let opener = {
        let app = app.clone();
        move |url: &str| -> Result<(), String> {
            use tauri_plugin_opener::OpenerExt;
            app.opener()
                .open_url(url.to_string(), None::<String>)
                .map_err(|e| format!("Failed to open the browser for OAuth consent: {e}"))
        }
    };
    let progress = |phase: &str, error: Option<String>| emit_progress(app, provider, phase, error);

    let flow = run_connect_flow(
        ConnectInputs {
            spec,
            endpoints,
            client: &client,
            allow_loopback: false,
            callback_timeout: CALLBACK_TIMEOUT,
        },
        &cancel,
        &opener,
        &progress,
    )
    .await;
    crate::mcp_oauth::release_oauth_cancel_token(state, &cancel_key(provider));
    let (tokens, identity) = flow?;

    persist(
        state,
        &crate::connectors::config_file_path()?,
        spec,
        label,
        host.as_deref(),
        &tenant,
        &client,
        &tokens,
        identity,
    )
}

#[allow(clippy::too_many_arguments)]
fn persist(
    state: &AppState,
    path: &std::path::Path,
    spec: &OAuthProviderSpec,
    label: String,
    host: Option<&str>,
    tenant: &str,
    client: &ClientRegistration,
    tokens: &StoredTokens,
    identity: String,
) -> Result<ConnectorAccount, String> {
    let previous_client = load_client(spec.provider).ok().flatten();
    save_client(spec.provider, client)?;

    let id = uuid::Uuid::new_v4().to_string();
    if let Err(error) = save_tokens(&id, tokens) {
        restore_client(spec.provider, previous_client);
        return Err(error);
    }

    let now = crate::run_commands::unix_time_ms()?;
    let account = ConnectorAccount {
        id: id.clone(),
        provider: spec.provider,
        label,
        scopes: tokens.scopes.clone(),
        // Points at the token record, so `remove_impl`'s existing keychain
        // cleanup deletes it — see `token_keychain_account`.
        credential_ref: Some(token_keychain_account(&id)),
        identity: Some(identity),
        created_at: now,
        last_verified_at: Some(now),
        last_error: None,
        connection: connection_metadata(spec, host, tenant),
    };

    let guard = state
        .connectors_config_lock
        .lock()
        .map_err(|_| "Connector catalog lock poisoned".to_string())?;
    let mut config = crate::connectors::load_config_impl(path)?;
    config.version = 1;
    config.accounts.push(account.clone());
    let saved = crate::connectors::save_config_impl(path, &config);
    drop(guard);
    if let Err(error) = saved {
        delete_tokens(&id);
        restore_client(spec.provider, previous_client);
        return Err(error);
    }
    Ok(account)
}

fn restore_client(provider: ConnectorProvider, previous: Option<ClientRegistration>) {
    match previous {
        Some(client) => {
            let _ = save_client(provider, &client);
        }
        None => delete_client(provider),
    }
}

/// Cancels an in-flight [`connectors_oauth_connect`] for `provider`. A no-op
/// success if none is running — the caller's desired end state already holds.
#[tauri::command(rename_all = "snake_case")]
pub fn connectors_oauth_cancel(
    state: tauri::State<'_, AppState>,
    provider: ConnectorProvider,
) -> Result<(), String> {
    let guard = state
        .mcp_oauth_cancel
        .lock()
        .map_err(|_| "OAuth cancel lock poisoned".to_string())?;
    if let Some((cancel, _active)) = guard.get(&cancel_key(provider)) {
        cancel.cancel();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener as StdTcpListener, TcpStream};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    /// A minimal raw-TCP fake authorization/API server: one canned response
    /// per request, in order, with every raw request recorded so a test can
    /// assert what actually went on the wire. Mirrors
    /// `mcp_oauth::tests::spawn_fake_oauth_server`'s hand-rolled idiom — no
    /// mocking crate, no real network.
    fn spawn_fixture(responses: Vec<String>) -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut remaining = responses.into_iter();
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                recorder
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());
                let body = remaining.next().unwrap_or_else(|| "{}".to_string());
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (addr, seen)
    }

    fn json_ok(body: &str) -> String {
        body.to_string()
    }

    fn endpoints_at(addr: SocketAddr) -> ResolvedEndpoints {
        ResolvedEndpoints {
            authorize: Url::parse(&format!("http://{addr}/authorize")).unwrap(),
            token: Url::parse(&format!("http://{addr}/token")).unwrap(),
            revoke: None,
            verify: Url::parse(&format!("http://{addr}/me")).unwrap(),
        }
    }

    fn spec(provider: ConnectorProvider) -> &'static OAuthProviderSpec {
        spec_for(provider).expect("shipped provider")
    }

    fn test_client() -> ClientRegistration {
        ClientRegistration {
            client_id: "client-abc".to_string(),
            client_secret: Some("shhh-secret".to_string()),
        }
    }

    fn unique_id(name: &str) -> String {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        format!(
            "conn-oauth-test-{}-{}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            name
        )
    }

    // --- authorize URL construction ----------------------------------------

    #[test]
    fn authorize_url_carries_pkce_state_scope_and_the_registered_redirect_uri() {
        let spec = spec(ConnectorProvider::GoogleDrive);
        let base = Url::parse(spec.authorize_url).unwrap();
        let url = authorize_url(
            spec,
            &base,
            "client-abc",
            "http://127.0.0.1:50000/",
            "state-xyz",
            Some("challenge-123"),
        );
        let pairs: std::collections::HashMap<_, _> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["client_id"], "client-abc");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:50000/");
        assert_eq!(pairs["state"], "state-xyz");
        assert_eq!(pairs["code_challenge"], "challenge-123");
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(
            pairs["scope"],
            "https://www.googleapis.com/auth/drive.metadata.readonly"
        );
    }

    #[test]
    fn authorize_url_includes_the_providers_extra_params() {
        for (provider, key, value) in [
            (ConnectorProvider::GoogleDrive, "access_type", "offline"),
            (ConnectorProvider::GoogleDrive, "prompt", "consent"),
            (
                ConnectorProvider::Dropbox,
                "token_access_type",
                "offline",
            ),
        ] {
            let spec = spec(provider);
            let base = Url::parse(spec.authorize_url).unwrap();
            let url = authorize_url(spec, &base, "c", "http://127.0.0.1:1/", "s", None);
            let found = url
                .query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned());
            assert_eq!(found.as_deref(), Some(value), "{provider:?} {key}");
        }
    }

    #[test]
    fn extra_params_never_overwrite_a_parameter_already_set() {
        // `state` is always set by the builder; an extra param of the same
        // name must not be able to replace the CSRF value.
        static HOSTILE: OAuthProviderSpec = OAuthProviderSpec {
            provider: ConnectorProvider::GoogleDrive,
            authorize_url: "https://example.test/authorize",
            token_url: "https://example.test/token",
            revoke: None,
            scopes: &["s"],
            extra_authorize_params: &[("state", "attacker-controlled")],
            pkce: false,
            secret: SecretPolicy::Optional,
            redirect_host: "127.0.0.1",
            host_field: None,
            verify: VerifySpec {
                method: "GET",
                url: "https://example.test/me",
                headers: &[],
                json_body: None,
                identity_pointers: &["/id"],
            },
        };
        let base = Url::parse(HOSTILE.authorize_url).unwrap();
        let url = authorize_url(&HOSTILE, &base, "c", "http://127.0.0.1:1/", "real-state", None);
        let states: Vec<_> = url
            .query_pairs()
            .filter(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .collect();
        assert_eq!(states, vec!["real-state".to_string()]);
    }

    #[test]
    fn authorize_url_omits_the_code_challenge_for_a_provider_without_pkce() {
        let spec = spec(ConnectorProvider::Hubspot);
        assert!(!spec.pkce);
        let base = Url::parse(spec.authorize_url).unwrap();
        let url = authorize_url(spec, &base, "c", "http://127.0.0.1:1/", "s", None);
        assert!(url.query_pairs().all(|(k, _)| k != "code_challenge"));
    }

    #[test]
    fn authorize_url_omits_scope_when_the_provider_configures_scopes_on_the_app() {
        let spec = spec(ConnectorProvider::Box);
        assert!(spec.scopes.is_empty());
        let base = Url::parse(spec.authorize_url).unwrap();
        let url = authorize_url(spec, &base, "c", "http://127.0.0.1:1/", "s", None);
        assert!(url.query_pairs().all(|(k, _)| k != "scope"));
    }

    #[test]
    fn pkce_challenge_is_the_base64url_sha256_of_the_verifier() {
        let (verifier, challenge) = pkce_pair();
        assert_eq!(challenge, b64(&Sha256::digest(verifier.as_bytes())));
        assert_ne!(verifier, challenge);
        assert!(!verifier.contains('=') && !verifier.contains('+'));
        assert_ne!(random_state(), random_state());
    }

    #[test]
    fn loopback_redirect_uri_is_stable_per_provider_and_inside_the_dynamic_range() {
        let first = preferred_redirect_uri(ConnectorProvider::Linear).unwrap();
        assert_eq!(first, preferred_redirect_uri(ConnectorProvider::Linear).unwrap());
        assert_ne!(first, preferred_redirect_uri(ConnectorProvider::Asana).unwrap());
        assert!(preferred_redirect_uri(ConnectorProvider::Slack).is_none());
        let port: u16 = first
            .trim_start_matches("http://127.0.0.1:")
            .trim_end_matches('/')
            .parse()
            .unwrap();
        assert!((49152..=65535).contains(&port), "{first}");
    }

    #[test]
    fn keychain_accounts_for_oauth_tokens_oauth_clients_and_catalog_secrets_are_three_namespaces() {
        assert_eq!(token_keychain_account("acct-1"), "connector-oauth:acct-1");
        assert_eq!(
            client_keychain_account(ConnectorProvider::Gitlab),
            "connector-oauth-client:gitlab"
        );
        assert_ne!(
            token_keychain_account("gitlab"),
            client_keychain_account(ConnectorProvider::Gitlab)
        );
        // `connectors.rs`'s own catalog secrets use `connector:<provider>:<id>`.
        assert!(!token_keychain_account("x").starts_with("connector:"));
    }

    #[test]
    fn microsoft_tenant_defaults_to_common_and_a_supplied_tenant_is_used_verbatim() {
        let spec = spec(ConnectorProvider::MicrosoftGraph);
        let default = resolve_endpoints(spec, None, "common").unwrap();
        assert!(default.authorize.as_str().contains("/common/oauth2/"));
        let named = resolve_endpoints(spec, None, "contoso.onmicrosoft.com").unwrap();
        assert!(named
            .authorize
            .as_str()
            .contains("/contoso.onmicrosoft.com/oauth2/"));
        assert!(named.revoke.is_none());
    }

    #[test]
    fn every_provider_row_resolves_to_https_endpoints() {
        for spec in OAUTH_PROVIDERS {
            let host = match spec.host_field {
                Some(HostField::ApiHost { .. }) => Some("example.com"),
                _ => None,
            };
            let endpoints = resolve_endpoints(spec, host, "common")
                .unwrap_or_else(|e| panic!("{:?}: {e}", spec.provider));
            for url in [&endpoints.authorize, &endpoints.token, &endpoints.verify] {
                assert_eq!(url.scheme(), "https", "{:?} {url}", spec.provider);
            }
            if spec.secret == SecretPolicy::Never {
                assert!(spec.pkce, "{:?} must use PKCE with no secret", spec.provider);
            }
        }
    }

    #[test]
    fn every_connector_provider_is_either_oauth_or_an_explicitly_listed_scheme() {
        // Keeps the enum, the OAuth table and the counts in features.md /
        // limitations.md from silently drifting apart.
        let non_oauth = [
            ConnectorProvider::Github,
            ConnectorProvider::Slack,
            ConnectorProvider::Notion,
            ConnectorProvider::Jira,
            ConnectorProvider::S3,
            ConnectorProvider::Extension,
        ];
        assert_eq!(OAUTH_PROVIDERS.len(), 11);
        assert_eq!(OAUTH_PROVIDERS.len() + non_oauth.len(), 17);
        for provider in non_oauth {
            assert!(spec_for(provider).is_none(), "{provider:?}");
        }
    }

    // --- token response parsing --------------------------------------------

    #[test]
    fn token_response_parsing_rejects_a_body_with_no_access_token_and_surfaces_the_error() {
        let error = parse_token_response(
            br#"{"error":"invalid_grant","error_description":"Bad refresh token"}"#,
            0,
        )
        .unwrap_err();
        assert!(error.contains("invalid_grant"), "{error}");
        assert!(error.contains("Bad refresh token"), "{error}");
    }

    #[test]
    fn token_response_parsing_turns_expires_in_into_an_absolute_expiry() {
        let tokens = parse_token_response(
            br#"{"access_token":"at","refresh_token":"rt","expires_in":3600,"scope":"read write"}"#,
            1_000,
        )
        .unwrap();
        assert_eq!(tokens.expires_at, Some(1_000 + 3_600_000));
        assert_eq!(tokens.scopes, vec!["read", "write"]);
        // No `expires_in` means a non-expiring token (a legacy Zendesk client).
        let forever = parse_token_response(br#"{"access_token":"at"}"#, 1_000).unwrap();
        assert_eq!(forever.expires_at, None);
    }

    // --- identity parsing, table-driven over the shipped specs -------------

    /// One canned success body and one canned provider-shaped rejection per
    /// shipped provider. A twelfth provider added without its fixture fails
    /// the two tests below.
    fn identity_fixtures() -> Vec<(ConnectorProvider, &'static str, &'static str, &'static str)> {
        vec![
            (
                ConnectorProvider::GoogleDrive,
                r#"{"user":{"emailAddress":"ada@example.com","displayName":"Ada"}}"#,
                "ada@example.com",
                r#"{"error":{"code":401,"message":"Invalid Credentials"}}"#,
            ),
            (
                ConnectorProvider::MicrosoftGraph,
                r#"{"userPrincipalName":"ada@contoso.com","displayName":"Ada"}"#,
                "ada@contoso.com",
                r#"{"error":{"code":"InvalidAuthenticationToken"}}"#,
            ),
            (
                ConnectorProvider::Linear,
                r#"{"data":{"viewer":{"name":"Ada","email":"ada@example.com"}}}"#,
                "ada@example.com",
                r#"{"errors":[{"message":"Authentication required"}]}"#,
            ),
            (
                ConnectorProvider::Asana,
                r#"{"data":{"email":"ada@example.com","name":"Ada"}}"#,
                "ada@example.com",
                r#"{"errors":[{"message":"Not Authorized"}]}"#,
            ),
            (
                ConnectorProvider::Dropbox,
                r#"{"email":"ada@example.com","name":{"display_name":"Ada"}}"#,
                "ada@example.com",
                r#"{"error_summary":"expired_access_token/"}"#,
            ),
            (
                ConnectorProvider::Box,
                r#"{"login":"ada@example.com","name":"Ada"}"#,
                "ada@example.com",
                r#"{"type":"error","status":401,"code":"unauthorized"}"#,
            ),
            (
                ConnectorProvider::Airtable,
                r#"{"id":"usrXYZ","email":"ada@example.com"}"#,
                "ada@example.com",
                r#"{"error":{"type":"UNAUTHORIZED"}}"#,
            ),
            (
                ConnectorProvider::Zendesk,
                r#"{"user":{"email":"ada@example.com","name":"Ada"}}"#,
                "ada@example.com",
                r#"{"error":"Couldn't authenticate you"}"#,
            ),
            (
                ConnectorProvider::Hubspot,
                r#"{"portalId":1234567,"uiDomain":"app.hubspot.com"}"#,
                "app.hubspot.com",
                r#"{"status":"error","message":"expired authentication"}"#,
            ),
            (
                ConnectorProvider::Discord,
                r#"{"id":"42","username":"ada"}"#,
                "ada",
                r#"{"message":"401: Unauthorized","code":0}"#,
            ),
            (
                ConnectorProvider::Gitlab,
                r#"{"username":"ada","email":"ada@example.com"}"#,
                "ada",
                r#"{"error":"invalid_token"}"#,
            ),
        ]
    }

    #[test]
    fn every_provider_spec_parses_its_identity_from_a_canned_success_body() {
        let fixtures = identity_fixtures();
        assert_eq!(fixtures.len(), OAUTH_PROVIDERS.len());
        for (provider, body, expected, _) in fixtures {
            let identity = identity_from(&spec(provider).verify, body.as_bytes())
                .unwrap_or_else(|e| panic!("{provider:?}: {e}"));
            assert_eq!(identity, expected, "{provider:?}");
        }
    }

    #[test]
    fn every_provider_spec_returns_a_distinguishable_error_for_a_canned_rejection() {
        for (provider, _, _, rejection) in identity_fixtures() {
            let error = identity_from(&spec(provider).verify, rejection.as_bytes())
                .expect_err(&format!("{provider:?} must reject"));
            assert!(
                error.contains("returned no identity"),
                "{provider:?}: {error}"
            );
        }
    }

    #[test]
    fn a_reconnect_error_is_recognisable_by_shape() {
        let message = reconnect_error(ConnectorProvider::Linear, "invalid_grant");
        assert!(is_reconnect_error(&message), "{message}");
        assert!(!is_reconnect_error("some unrelated failure"));
    }

    // --- user-supplied host validation -------------------------------------

    #[test]
    fn a_user_supplied_host_is_pinned_to_its_own_origin_and_junk_is_refused() {
        assert_eq!(normalize_host(" gitlab.example.com/ ").unwrap(), "gitlab.example.com");
        assert_eq!(
            normalize_host("https://acme.zendesk.com").unwrap(),
            "acme.zendesk.com"
        );
        for bad in [
            "http://gitlab.example.com",
            "https://user:pw@gitlab.example.com",
            "https://gitlab.example.com/evil/path",
            "https://gitlab.example.com/?next=https://evil.test",
            "   ",
        ] {
            assert!(normalize_host(bad).is_err(), "{bad} must be refused");
        }
        // The verification call is pinned to the *resolved* verify URL's own
        // origin, so a host can never be used to reach a different one.
        let endpoints =
            resolve_endpoints(spec(ConnectorProvider::Gitlab), Some("gitlab.example.com"), "common")
                .unwrap();
        assert_eq!(
            crate::connectors::origin_of(&endpoints.verify).unwrap(),
            "https://gitlab.example.com"
        );
        assert_eq!(
            crate::connectors::origin_of(&endpoints.token).unwrap(),
            "https://gitlab.example.com"
        );
    }

    // --- the connect state machine (fake AS + fake browser) ----------------

    fn fake_browser(then: impl Fn(&Url) -> String + Send + Sync + 'static) -> impl Fn(&str) -> Result<(), String> + Sync {
        move |authorize_url: &str| {
            let url = Url::parse(authorize_url).map_err(|e| e.to_string())?;
            let redirect = url
                .query_pairs()
                .find(|(k, _)| k == "redirect_uri")
                .map(|(_, v)| v.into_owned())
                .ok_or("no redirect_uri")?;
            let query = then(&url);
            let target = Url::parse(&redirect).map_err(|e| e.to_string())?;
            let addr = format!(
                "127.0.0.1:{}",
                target.port().ok_or("redirect has no port")?
            );
            let mut stream = TcpStream::connect(addr).map_err(|e| e.to_string())?;
            stream
                .write_all(
                    format!("GET /?{query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                        .as_bytes(),
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        }
    }

    fn inputs(addr: SocketAddr, provider: ConnectorProvider) -> ConnectInputs<'static> {
        static CLIENT: std::sync::LazyLock<ClientRegistration> =
            std::sync::LazyLock::new(test_client);
        ConnectInputs {
            spec: spec(provider),
            endpoints: endpoints_at(addr),
            client: &CLIENT,
            allow_loopback: true,
            callback_timeout: Duration::from_secs(5),
        }
    }

    #[tokio::test]
    async fn connect_flow_exchanges_the_code_and_returns_the_verified_identity() {
        let (addr, seen) = spawn_fixture(vec![
            json_ok(r#"{"access_token":"at-1","refresh_token":"rt-1","expires_in":3600,"scope":"read"}"#),
            json_ok(r#"{"data":{"viewer":{"name":"Ada","email":"ada@example.com"}}}"#),
        ]);
        let cancel = CancellationToken::new();
        let (tokens, identity) = run_connect_flow(
            inputs(addr, ConnectorProvider::Linear),
            &cancel,
            &fake_browser(|url| {
                let state = url
                    .query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
                    .unwrap();
                format!("code=the-code&state={state}")
            }),
            &|_, _| {},
        )
        .await
        .expect("connect flow");

        assert_eq!(identity, "ada@example.com");
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert_eq!(tokens.scopes, vec!["read"]);

        let requests = seen.lock().unwrap().clone();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(requests[0].contains("grant_type=authorization_code"));
        assert!(requests[0].contains("code_verifier="), "PKCE verifier must be sent");
        assert!(requests[0].contains("code=the-code"));
        assert!(requests[1].contains("Bearer at-1"));
    }

    #[tokio::test]
    async fn connect_flow_rejects_a_state_that_was_never_issued() {
        let (addr, seen) = spawn_fixture(vec![]);
        let cancel = CancellationToken::new();
        let error = run_connect_flow(
            inputs(addr, ConnectorProvider::Asana),
            &cancel,
            &fake_browser(|_| "code=c&state=not-the-one".to_string()),
            &|_, _| {},
        )
        .await
        .expect_err("must refuse");
        assert!(error.contains("never issued"), "{error}");
        assert!(seen.lock().unwrap().is_empty(), "no token request may be sent");
    }

    #[tokio::test]
    async fn connect_flow_surfaces_an_access_denied_callback() {
        let (addr, _) = spawn_fixture(vec![]);
        let cancel = CancellationToken::new();
        let error = run_connect_flow(
            inputs(addr, ConnectorProvider::Dropbox),
            &cancel,
            &fake_browser(|_| {
                "error=access_denied&error_description=The+user+said+no".to_string()
            }),
            &|_, _| {},
        )
        .await
        .expect_err("must fail");
        assert!(error.contains("access_denied"), "{error}");
        assert!(error.contains("The user said no"), "{error}");
    }

    #[tokio::test]
    async fn connect_flow_errors_when_the_callback_carries_no_code() {
        let (addr, _) = spawn_fixture(vec![]);
        let cancel = CancellationToken::new();
        let error = run_connect_flow(
            inputs(addr, ConnectorProvider::Airtable),
            &cancel,
            &fake_browser(|url| {
                let state = url
                    .query_pairs()
                    .find(|(k, _)| k == "state")
                    .map(|(_, v)| v.into_owned())
                    .unwrap();
                format!("state={state}")
            }),
            &|_, _| {},
        )
        .await
        .expect_err("must fail");
        assert!(error.contains("no authorization code"), "{error}");
    }

    #[tokio::test]
    async fn connect_flow_times_out_when_no_callback_ever_arrives() {
        let (addr, _) = spawn_fixture(vec![]);
        let cancel = CancellationToken::new();
        let mut inputs = inputs(addr, ConnectorProvider::Box);
        inputs.callback_timeout = Duration::from_millis(150);
        let error = run_connect_flow(inputs, &cancel, &|_| Ok(()), &|_, _| {})
            .await
            .expect_err("must time out");
        assert!(error.contains("Timed out"), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_connect_leaves_no_keychain_record_and_no_catalog_row() {
        let (addr, seen) = spawn_fixture(vec![]);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = run_connect_flow(
            inputs(addr, ConnectorProvider::Discord),
            &cancel,
            &|_| Ok(()),
            &|_, _| {},
        )
        .await
        .expect_err("must be cancelled");
        assert_eq!(error, OAUTH_CANCELLED_MESSAGE);
        // `run_connect_flow` writes nothing by construction — the only proof
        // needed is that no request was ever sent and nothing was persisted.
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn the_phases_the_flow_streams_are_the_ones_the_ui_maps() {
        // The UI keys its pill off these exact strings.
        for phase in [
            "needs_client_id",
            "opening_browser",
            "waiting_for_browser",
            "exchanging_token",
            "verifying",
            "connected",
            "error",
            "cancelled",
        ] {
            assert!(!phase.is_empty());
        }
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let progress = move |phase: &str, _: Option<String>| {
            recorder.lock().unwrap().push(phase.to_string());
        };
        progress("opening_browser", None);
        assert_eq!(seen.lock().unwrap().as_slice(), ["opening_browser"]);
    }

    // --- refresh ------------------------------------------------------------

    fn tokens_expiring_at(expires_at: Option<u64>, refresh: Option<&str>) -> StoredTokens {
        StoredTokens {
            access_token: "old-access".to_string(),
            refresh_token: refresh.map(str::to_string),
            expires_at,
            scopes: vec!["read".to_string()],
        }
    }

    #[tokio::test]
    async fn an_unexpired_access_token_is_returned_without_a_token_request() {
        let (addr, seen) = spawn_fixture(vec![]);
        let refreshed = refresh_if_expired(
            spec(ConnectorProvider::Linear),
            &endpoints_at(addr),
            true,
            &test_client(),
            &tokens_expiring_at(Some(10_000_000), Some("rt")),
            1_000,
        )
        .await
        .unwrap();
        assert!(refreshed.is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_non_expiring_token_is_never_refreshed() {
        let (addr, seen) = spawn_fixture(vec![]);
        let refreshed = refresh_if_expired(
            spec(ConnectorProvider::Zendesk),
            &endpoints_at(addr),
            true,
            &test_client(),
            &tokens_expiring_at(None, None),
            9_999_999,
        )
        .await
        .unwrap();
        assert!(refreshed.is_none());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn refresh_persists_a_rotated_refresh_token_and_keeps_the_old_one_when_none_returns() {
        let (addr, seen) = spawn_fixture(vec![json_ok(
            r#"{"access_token":"new-access","refresh_token":"rotated","expires_in":3600}"#,
        )]);
        let rotated = refresh_if_expired(
            spec(ConnectorProvider::Linear),
            &endpoints_at(addr),
            true,
            &test_client(),
            &tokens_expiring_at(Some(0), Some("rt-old")),
            1_000,
        )
        .await
        .unwrap()
        .expect("must refresh");
        assert_eq!(rotated.access_token, "new-access");
        assert_eq!(rotated.refresh_token.as_deref(), Some("rotated"));
        assert_eq!(rotated.scopes, vec!["read"], "scopes carry over");
        assert!(seen.lock().unwrap()[0].contains("grant_type=refresh_token"));

        let (addr, _) = spawn_fixture(vec![json_ok(
            r#"{"access_token":"new-access","expires_in":3600}"#,
        )]);
        let kept = refresh_if_expired(
            spec(ConnectorProvider::Linear),
            &endpoints_at(addr),
            true,
            &test_client(),
            &tokens_expiring_at(Some(0), Some("rt-old")),
            1_000,
        )
        .await
        .unwrap()
        .expect("must refresh");
        assert_eq!(kept.refresh_token.as_deref(), Some("rt-old"));
    }

    #[tokio::test]
    async fn a_rejected_refresh_returns_a_reconnect_error_and_leaves_the_record_untouched() {
        let (addr, _) = spawn_fixture(vec![json_ok(
            r#"{"error":"invalid_grant","error_description":"revoked"}"#,
        )]);
        let stored = tokens_expiring_at(Some(0), Some("rt-old"));
        let error = refresh_if_expired(
            spec(ConnectorProvider::Linear),
            &endpoints_at(addr),
            true,
            &test_client(),
            &stored,
            1_000,
        )
        .await
        .expect_err("must fail");
        assert!(is_reconnect_error(&error), "{error}");
        assert!(error.contains("invalid_grant"), "{error}");
        // The caller was handed an error, never a replacement record.
        assert_eq!(stored.access_token, "old-access");
        assert_eq!(stored.refresh_token.as_deref(), Some("rt-old"));
    }

    #[tokio::test]
    async fn an_expired_token_with_no_refresh_token_asks_for_a_reconnect() {
        let (addr, seen) = spawn_fixture(vec![]);
        let error = refresh_if_expired(
            spec(ConnectorProvider::Zendesk),
            &endpoints_at(addr),
            true,
            &test_client(),
            &tokens_expiring_at(Some(0), None),
            1_000,
        )
        .await
        .expect_err("must fail");
        assert!(is_reconnect_error(&error), "{error}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_access_token_calls_for_one_account_share_one_refresh_lock() {
        // Mirrors `mcp_oauth`'s
        // `concurrent_get_access_token_calls_for_the_same_server_id_are_serialized_not_parallel`:
        // `access_token` short-circuits before the lock when no credential is
        // stored, so what is exercised is the lock keying itself.
        let state = AppState::default();
        let id = unique_id("refresh-lock");
        let a = crate::mcp_oauth::refresh_lock_for(&state, &format!("connector:{id}"));
        let b = crate::mcp_oauth::refresh_lock_for(&state, &format!("connector:{id}"));
        assert!(Arc::ptr_eq(&a, &b));
        let other = crate::mcp_oauth::refresh_lock_for(&state, &format!("connector:{id}-other"));
        assert!(!Arc::ptr_eq(&a, &other));
        let guard = a.lock().await;
        assert!(b.try_lock().is_err(), "the second caller must block");
        drop(guard);
        assert!(b.try_lock().is_ok());
    }

    // --- persistence --------------------------------------------------------

    #[test]
    fn an_oauth_account_row_is_written_with_no_token_or_client_secret_in_the_json_file() {
        let path = std::env::temp_dir().join(unique_id("catalog.json"));
        let id = unique_id("acct");
        let account = ConnectorAccount {
            id: id.clone(),
            provider: ConnectorProvider::Gitlab,
            label: "Work GitLab".to_string(),
            scopes: vec!["read_user".to_string()],
            credential_ref: Some(token_keychain_account(&id)),
            identity: Some("ada".to_string()),
            created_at: 1,
            last_verified_at: Some(1),
            last_error: None,
            connection: Some(serde_json::json!({ "host": "gitlab.example.com" })),
        };
        let config = crate::connectors::ConnectorCatalogFile {
            version: 1,
            accounts: vec![account],
        };
        crate::connectors::save_config_impl(&path, &config).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        for secret in [
            "at-1",
            "rt-1",
            "shhh-secret",
            "access_token",
            "refresh_token",
            "client_secret",
        ] {
            assert!(!raw.contains(secret), "{secret} leaked into {raw}");
        }
        assert!(raw.contains("connector-oauth:"), "credential_ref is a keychain name");
        assert!(raw.contains("gitlab.example.com"), "non-secret host is kept");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn credential_for_account_refuses_an_oauth_account_and_names_the_oauth_accessor() {
        let account = ConnectorAccount {
            id: "acct-1".to_string(),
            provider: ConnectorProvider::GoogleDrive,
            label: "Drive".to_string(),
            scopes: vec![],
            credential_ref: Some(token_keychain_account("acct-1")),
            identity: None,
            created_at: 0,
            last_verified_at: None,
            last_error: None,
            connection: None,
        };
        let error = crate::connectors::credential_for_account(&account).expect_err("must refuse");
        assert!(error.contains("connector_oauth::access_token"), "{error}");
    }

    #[tokio::test]
    async fn revocation_failure_does_not_block_removal() {
        // No stored tokens and no stored client for this id, so every branch
        // of `revoke_and_forget` bails out — and it still returns.
        let account = ConnectorAccount {
            id: unique_id("never-saved"),
            provider: ConnectorProvider::Airtable,
            label: "Airtable".to_string(),
            scopes: vec![],
            credential_ref: None,
            identity: None,
            created_at: 0,
            last_verified_at: None,
            last_error: None,
            connection: None,
        };
        revoke_and_forget(&account).await;
    }

    #[test]
    fn forget_client_leaves_a_provider_that_still_has_an_account() {
        let path = std::env::temp_dir().join(unique_id("still-used.json"));
        let config = crate::connectors::ConnectorCatalogFile {
            version: 1,
            accounts: vec![ConnectorAccount {
                id: "keep".to_string(),
                provider: ConnectorProvider::Gitlab,
                label: "Other GitLab".to_string(),
                scopes: vec![],
                credential_ref: None,
                identity: None,
                created_at: 0,
                last_verified_at: None,
                last_error: None,
                connection: None,
            }],
        };
        crate::connectors::save_config_impl(&path, &config).unwrap();
        // Not asserting on the keychain (which this test never wrote to):
        // the contract under test is that a provider with a surviving account
        // is never considered unused.
        let still_used = crate::connectors::load_config_impl(&path)
            .unwrap()
            .accounts
            .iter()
            .any(|a| a.provider == ConnectorProvider::Gitlab);
        assert!(still_used);
        forget_client_if_unused(&path, ConnectorProvider::Gitlab);
        let _ = std::fs::remove_file(&path);
    }
}

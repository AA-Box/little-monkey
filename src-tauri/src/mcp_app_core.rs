//! MCP OAuth, structured content, deterministic tool routing, and opaque UI
//! host contracts. This module performs no network, keychain, or UI work;
//! transports, secret vaults, cryptographic material, and approval gates are
//! injected so callers cannot receive a fake success.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Host, Url};

pub const MCP_OAUTH_CONTRACT_VERSION: u32 = 1;
pub const MCP_CONTENT_CONTRACT_VERSION: u32 = 1;
pub const MCP_ROUTER_CONTRACT_VERSION: u32 = 1;
pub const MCP_UI_HOST_CONTRACT_VERSION: u32 = 1;

pub type McpCoreResult<T> = Result<T, McpCoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCoreError {
    InvalidOAuth(String),
    OAuthState(String),
    Transport(String),
    Vault(String),
    InvalidContent(String),
    InvalidTool(String),
    Router(String),
    UiPolicy(String),
    ApprovalDenied(String),
    LimitExceeded(String),
    Json(String),
}

impl fmt::Display for McpCoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOAuth(message) => write!(formatter, "invalid MCP OAuth data: {message}"),
            Self::OAuthState(message) => write!(formatter, "MCP OAuth state error: {message}"),
            Self::Transport(message) => write!(formatter, "MCP OAuth transport error: {message}"),
            Self::Vault(message) => write!(formatter, "MCP OAuth vault error: {message}"),
            Self::InvalidContent(message) => write!(formatter, "invalid MCP content: {message}"),
            Self::InvalidTool(message) => write!(formatter, "invalid MCP tool: {message}"),
            Self::Router(message) => write!(formatter, "MCP router error: {message}"),
            Self::UiPolicy(message) => write!(formatter, "MCP UI policy error: {message}"),
            Self::ApprovalDenied(message) => write!(formatter, "MCP UI approval denied: {message}"),
            Self::LimitExceeded(message) => write!(formatter, "MCP limit exceeded: {message}"),
            Self::Json(message) => write!(formatter, "MCP JSON error: {message}"),
        }
    }
}

impl std::error::Error for McpCoreError {}

impl From<serde_json::Error> for McpCoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn validate_id(label: &str, value: &str) -> McpCoreResult<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(McpCoreError::InvalidOAuth(format!(
            "{label} must be a bounded ASCII identifier"
        )));
    }
    Ok(())
}

fn validate_https_url(label: &str, value: &str) -> McpCoreResult<Url> {
    let url = Url::parse(value)
        .map_err(|error| McpCoreError::InvalidOAuth(format!("{label}: {error}")))?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(McpCoreError::InvalidOAuth(format!(
            "{label} must be credential-free HTTPS"
        )));
    }
    match url.host() {
        Some(Host::Domain(host)) if host != "localhost" && !host.ends_with(".localhost") => Ok(url),
        _ => Err(McpCoreError::InvalidOAuth(format!(
            "{label} cannot use a local or literal host"
        ))),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OAuthServerMetadata {
    pub contract_version: u32,
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub revocation_endpoint: Option<String>,
    pub supported_scopes: BTreeSet<String>,
    pub supports_pkce_s256: bool,
}

impl OAuthServerMetadata {
    pub fn validate(&self) -> McpCoreResult<()> {
        if self.contract_version != MCP_OAUTH_CONTRACT_VERSION || !self.supports_pkce_s256 {
            return Err(McpCoreError::InvalidOAuth(
                "OAuth server must support contract v1 and PKCE S256".to_string(),
            ));
        }
        let issuer = validate_https_url("issuer", &self.issuer)?;
        if issuer.query().is_some() || issuer.path() != "/" {
            return Err(McpCoreError::InvalidOAuth(
                "issuer must be an HTTPS origin".to_string(),
            ));
        }
        validate_https_url("authorization_endpoint", &self.authorization_endpoint)?;
        validate_https_url("token_endpoint", &self.token_endpoint)?;
        if let Some(endpoint) = &self.revocation_endpoint {
            validate_https_url("revocation_endpoint", endpoint)?;
        }
        if self.supported_scopes.is_empty()
            || self.supported_scopes.iter().any(|scope| {
                scope.is_empty() || scope.len() > 128 || scope.contains(char::is_whitespace)
            })
        {
            return Err(McpCoreError::InvalidOAuth(
                "OAuth scope catalog is empty or malformed".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OAuthClientConfig {
    pub server_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub requested_scopes: BTreeSet<String>,
}

impl OAuthClientConfig {
    pub fn validate(&self, server: &OAuthServerMetadata) -> McpCoreResult<()> {
        validate_id("server_id", &self.server_id)?;
        if self.client_id.trim().is_empty() || self.client_id.len() > 512 {
            return Err(McpCoreError::InvalidOAuth(
                "invalid OAuth client id".to_string(),
            ));
        }
        validate_redirect_uri(&self.redirect_uri)?;
        if self.requested_scopes.is_empty()
            || !self.requested_scopes.is_subset(&server.supported_scopes)
        {
            return Err(McpCoreError::InvalidOAuth(
                "requested OAuth scopes are empty or unsupported".to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_redirect_uri(value: &str) -> McpCoreResult<()> {
    let url = Url::parse(value)
        .map_err(|error| McpCoreError::InvalidOAuth(format!("redirect URI: {error}")))?;
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(McpCoreError::InvalidOAuth(
            "redirect URI cannot contain credentials or a fragment".to_string(),
        ));
    }
    let valid_loopback = url.scheme() == "http"
        && matches!(url.host(), Some(Host::Ipv4(address)) if address.is_loopback())
        && url.port().is_some();
    let valid_custom = url.scheme() == "littlemonkey"
        && url.host_str() == Some("oauth")
        && url.path() == "/callback";
    if !valid_loopback && !valid_custom {
        return Err(McpCoreError::InvalidOAuth(
            "redirect URI must be a fixed-port IPv4 loopback or littlemonkey://oauth/callback"
                .to_string(),
        ));
    }
    Ok(())
}

/// Sensitive material has no Serialize implementation and a redacted Debug.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretMaterial(String);

impl SecretMaterial {
    pub fn new(value: String) -> McpCoreResult<Self> {
        if value.is_empty() || value.len() > 16_384 || value.contains('\0') {
            return Err(McpCoreError::InvalidOAuth(
                "secret material is empty or over limit".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretMaterial([REDACTED])")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretReference {
    pub vault_id: String,
    pub reference_id: String,
}

impl SecretReference {
    fn validate(&self) -> McpCoreResult<()> {
        validate_id("vault_id", &self.vault_id)?;
        validate_id("secret reference_id", &self.reference_id)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthTokenSet {
    access_token: SecretMaterial,
    refresh_token: Option<SecretMaterial>,
    pub token_type: String,
    pub granted_scopes: BTreeSet<String>,
    pub issued_unix_ms: u64,
    pub expires_unix_ms: u64,
}

impl fmt::Debug for OAuthTokenSet {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthTokenSet")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("token_type", &self.token_type)
            .field("granted_scopes", &self.granted_scopes)
            .field("issued_unix_ms", &self.issued_unix_ms)
            .field("expires_unix_ms", &self.expires_unix_ms)
            .finish()
    }
}

impl OAuthTokenSet {
    pub fn new(
        access_token: SecretMaterial,
        refresh_token: Option<SecretMaterial>,
        token_type: String,
        granted_scopes: BTreeSet<String>,
        issued_unix_ms: u64,
        expires_unix_ms: u64,
    ) -> McpCoreResult<Self> {
        if !token_type.eq_ignore_ascii_case("bearer")
            || granted_scopes.is_empty()
            || expires_unix_ms <= issued_unix_ms
        {
            return Err(McpCoreError::InvalidOAuth(
                "invalid token type, scopes, or expiry".to_string(),
            ));
        }
        Ok(Self {
            access_token,
            refresh_token,
            token_type,
            granted_scopes,
            issued_unix_ms,
            expires_unix_ms,
        })
    }

    pub fn access_token(&self) -> &SecretMaterial {
        &self.access_token
    }

    pub fn refresh_token(&self) -> Option<&SecretMaterial> {
        self.refresh_token.as_ref()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthTokenMetadata {
    pub token_reference: SecretReference,
    pub token_type: String,
    pub granted_scopes: BTreeSet<String>,
    pub issued_unix_ms: u64,
    pub expires_unix_ms: u64,
}

pub trait OAuthSecretVault: Send + Sync {
    fn put_ephemeral(&self, label: &str, secret: SecretMaterial)
        -> Result<SecretReference, String>;
    fn get_ephemeral(&self, reference: &SecretReference) -> Result<SecretMaterial, String>;
    fn delete_ephemeral(&self, reference: &SecretReference) -> Result<(), String>;
    fn put_tokens(&self, server_id: &str, tokens: OAuthTokenSet)
        -> Result<SecretReference, String>;
    fn get_tokens(&self, reference: &SecretReference) -> Result<OAuthTokenSet, String>;
    fn replace_tokens(
        &self,
        reference: &SecretReference,
        tokens: OAuthTokenSet,
    ) -> Result<(), String>;
    fn delete_tokens(&self, reference: &SecretReference) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct PkceMaterial {
    pub state: String,
    pub verifier: SecretMaterial,
    pub challenge_s256: String,
}

pub trait OAuthSecurityProvider: Send + Sync {
    fn generate_pkce(&self) -> Result<PkceMaterial, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingOAuthFlow {
    pub flow_id: String,
    pub server_id: String,
    pub state_sha256: String,
    pub verifier_reference: SecretReference,
    pub redirect_uri: String,
    pub requested_scopes: BTreeSet<String>,
    pub created_unix_ms: u64,
    pub expires_unix_ms: u64,
}

pub trait OAuthFlowStore: Send + Sync {
    fn put(&self, state: PendingOAuthFlow) -> Result<(), String>;
    fn take_by_state_hash(&self, state_sha256: &str) -> Result<Option<PendingOAuthFlow>, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OAuthAuthorizationPlan {
    pub flow_id: String,
    pub authorization_url: String,
    pub expires_unix_ms: u64,
}

#[derive(Debug, Clone)]
pub struct OAuthCodeExchangeRequest {
    pub token_endpoint: String,
    pub client_id: String,
    pub code: SecretMaterial,
    pub redirect_uri: String,
    pub pkce_verifier: SecretMaterial,
    pub requested_scopes: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct OAuthRefreshRequest {
    pub token_endpoint: String,
    pub client_id: String,
    pub refresh_token: SecretMaterial,
    pub scopes: BTreeSet<String>,
}

pub trait OAuthTransport: Send + Sync {
    fn exchange_code(&self, request: OAuthCodeExchangeRequest) -> Result<OAuthTokenSet, String>;
    fn refresh(&self, request: OAuthRefreshRequest) -> Result<OAuthTokenSet, String>;
    fn revoke(&self, endpoint: &str, client_id: &str, token: SecretMaterial) -> Result<(), String>;
}

#[derive(Clone, PartialEq, Eq)]
pub struct OAuthCallback {
    pub state: String,
    pub code: SecretMaterial,
    pub error: Option<String>,
}

impl fmt::Debug for OAuthCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OAuthCallback")
            .field("state", &self.state)
            .field("code", &"[REDACTED]")
            .field("error", &self.error)
            .finish()
    }
}

pub fn begin_oauth(
    server: &OAuthServerMetadata,
    client: &OAuthClientConfig,
    security: &dyn OAuthSecurityProvider,
    vault: &dyn OAuthSecretVault,
    flows: &dyn OAuthFlowStore,
    now_unix_ms: u64,
    lifetime_ms: u64,
) -> McpCoreResult<OAuthAuthorizationPlan> {
    server.validate()?;
    client.validate(server)?;
    if !(60_000..=15 * 60_000).contains(&lifetime_ms) {
        return Err(McpCoreError::InvalidOAuth(
            "OAuth flow lifetime must be between one and fifteen minutes".to_string(),
        ));
    }
    let pkce = security.generate_pkce().map_err(McpCoreError::OAuthState)?;
    if pkce.state.len() < 32
        || pkce.state.len() > 256
        || pkce.challenge_s256.len() < 32
        || pkce.challenge_s256.len() > 256
        || pkce
            .state
            .bytes()
            .chain(pkce.challenge_s256.bytes())
            .any(|byte| {
                !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
            })
    {
        return Err(McpCoreError::OAuthState(
            "security provider returned malformed state/PKCE data".to_string(),
        ));
    }
    let verifier_reference = vault
        .put_ephemeral("mcp-oauth-pkce", pkce.verifier)
        .map_err(McpCoreError::Vault)?;
    verifier_reference.validate()?;
    let flow_id = sha256(format!("{}:{}:{}", client.server_id, pkce.state, now_unix_ms).as_bytes());
    let expires_unix_ms = now_unix_ms.saturating_add(lifetime_ms);
    let pending = PendingOAuthFlow {
        flow_id: flow_id.clone(),
        server_id: client.server_id.clone(),
        state_sha256: sha256(pkce.state.as_bytes()),
        verifier_reference,
        redirect_uri: client.redirect_uri.clone(),
        requested_scopes: client.requested_scopes.clone(),
        created_unix_ms: now_unix_ms,
        expires_unix_ms,
    };
    flows.put(pending).map_err(McpCoreError::OAuthState)?;
    let mut authorization = Url::parse(&server.authorization_endpoint)
        .map_err(|error| McpCoreError::InvalidOAuth(error.to_string()))?;
    authorization
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client.client_id)
        .append_pair("redirect_uri", &client.redirect_uri)
        .append_pair(
            "scope",
            &client
                .requested_scopes
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(" "),
        )
        .append_pair("state", &pkce.state)
        .append_pair("code_challenge", &pkce.challenge_s256)
        .append_pair("code_challenge_method", "S256");
    Ok(OAuthAuthorizationPlan {
        flow_id,
        authorization_url: authorization.to_string(),
        expires_unix_ms,
    })
}

pub fn complete_oauth(
    server: &OAuthServerMetadata,
    client: &OAuthClientConfig,
    callback: OAuthCallback,
    vault: &dyn OAuthSecretVault,
    flows: &dyn OAuthFlowStore,
    transport: &dyn OAuthTransport,
    now_unix_ms: u64,
) -> McpCoreResult<OAuthTokenMetadata> {
    server.validate()?;
    client.validate(server)?;
    if let Some(error) = callback.error {
        return Err(McpCoreError::Transport(format!(
            "authorization server returned: {error}"
        )));
    }
    if callback.state.len() > 256 || callback.code.expose().len() > 8_192 {
        return Err(McpCoreError::OAuthState(
            "callback state/code is malformed".to_string(),
        ));
    }
    let state_hash = sha256(callback.state.as_bytes());
    let pending = flows
        .take_by_state_hash(&state_hash)
        .map_err(McpCoreError::OAuthState)?
        .ok_or_else(|| McpCoreError::OAuthState("unknown or replayed OAuth state".to_string()))?;
    if pending.state_sha256 != state_hash
        || pending.server_id != client.server_id
        || pending.redirect_uri != client.redirect_uri
        || pending.requested_scopes != client.requested_scopes
        || now_unix_ms > pending.expires_unix_ms
    {
        let _ = vault.delete_ephemeral(&pending.verifier_reference);
        return Err(McpCoreError::OAuthState(
            "OAuth flow expired or changed after authorization began".to_string(),
        ));
    }
    let verifier = vault
        .get_ephemeral(&pending.verifier_reference)
        .map_err(McpCoreError::Vault)?;
    let exchange_result = transport.exchange_code(OAuthCodeExchangeRequest {
        token_endpoint: server.token_endpoint.clone(),
        client_id: client.client_id.clone(),
        code: callback.code,
        redirect_uri: client.redirect_uri.clone(),
        pkce_verifier: verifier,
        requested_scopes: pending.requested_scopes.clone(),
    });
    let cleanup_result = vault.delete_ephemeral(&pending.verifier_reference);
    let tokens = exchange_result.map_err(McpCoreError::Transport)?;
    cleanup_result.map_err(McpCoreError::Vault)?;
    validate_returned_tokens(&tokens, &pending.requested_scopes, now_unix_ms)?;
    let token_type = tokens.token_type.clone();
    let granted_scopes = tokens.granted_scopes.clone();
    let issued_unix_ms = tokens.issued_unix_ms;
    let expires_unix_ms = tokens.expires_unix_ms;
    let token_reference = vault
        .put_tokens(&client.server_id, tokens)
        .map_err(McpCoreError::Vault)?;
    token_reference.validate()?;
    Ok(OAuthTokenMetadata {
        token_reference,
        token_type,
        granted_scopes,
        issued_unix_ms,
        expires_unix_ms,
    })
}

pub fn refresh_oauth(
    server: &OAuthServerMetadata,
    client: &OAuthClientConfig,
    metadata: &OAuthTokenMetadata,
    vault: &dyn OAuthSecretVault,
    transport: &dyn OAuthTransport,
    now_unix_ms: u64,
) -> McpCoreResult<OAuthTokenMetadata> {
    server.validate()?;
    client.validate(server)?;
    metadata.token_reference.validate()?;
    let old = vault
        .get_tokens(&metadata.token_reference)
        .map_err(McpCoreError::Vault)?;
    let refresh_token = old.refresh_token().cloned().ok_or_else(|| {
        McpCoreError::InvalidOAuth("server did not issue a refresh token".to_string())
    })?;
    let mut next = transport
        .refresh(OAuthRefreshRequest {
            token_endpoint: server.token_endpoint.clone(),
            client_id: client.client_id.clone(),
            refresh_token,
            scopes: old.granted_scopes.clone(),
        })
        .map_err(McpCoreError::Transport)?;
    validate_returned_tokens(&next, &old.granted_scopes, now_unix_ms)?;
    if next.refresh_token.is_none() {
        next.refresh_token = old.refresh_token;
    }
    let updated = OAuthTokenMetadata {
        token_reference: metadata.token_reference.clone(),
        token_type: next.token_type.clone(),
        granted_scopes: next.granted_scopes.clone(),
        issued_unix_ms: next.issued_unix_ms,
        expires_unix_ms: next.expires_unix_ms,
    };
    vault
        .replace_tokens(&metadata.token_reference, next)
        .map_err(McpCoreError::Vault)?;
    Ok(updated)
}

pub fn revoke_oauth(
    server: &OAuthServerMetadata,
    client: &OAuthClientConfig,
    metadata: &OAuthTokenMetadata,
    vault: &dyn OAuthSecretVault,
    transport: &dyn OAuthTransport,
) -> McpCoreResult<()> {
    let endpoint = server.revocation_endpoint.as_deref().ok_or_else(|| {
        McpCoreError::InvalidOAuth("server has no revocation endpoint".to_string())
    })?;
    let tokens = vault
        .get_tokens(&metadata.token_reference)
        .map_err(McpCoreError::Vault)?;
    transport
        .revoke(endpoint, &client.client_id, tokens.access_token)
        .map_err(McpCoreError::Transport)?;
    vault
        .delete_tokens(&metadata.token_reference)
        .map_err(McpCoreError::Vault)
}

fn validate_returned_tokens(
    tokens: &OAuthTokenSet,
    requested_scopes: &BTreeSet<String>,
    now_unix_ms: u64,
) -> McpCoreResult<()> {
    if tokens.issued_unix_ms > now_unix_ms.saturating_add(60_000)
        || tokens.expires_unix_ms <= now_unix_ms
        || !tokens.granted_scopes.is_subset(requested_scopes)
        || tokens.granted_scopes.is_empty()
        || !tokens.token_type.eq_ignore_ascii_case("bearer")
    {
        return Err(McpCoreError::InvalidOAuth(
            "token response has invalid timing, type, or scope expansion".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Structured MCP content and useful deterministic text fallback
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum McpContentBlock {
    Text {
        text: String,
        annotations: BTreeMap<String, String>,
    },
    Image {
        mime_type: String,
        data: Vec<u8>,
        alt_text: Option<String>,
    },
    Audio {
        mime_type: String,
        data: Vec<u8>,
        transcript: Option<String>,
    },
    Resource {
        uri: String,
        name: Option<String>,
        mime_type: Option<String>,
        content: ResourceContent,
        annotations: BTreeMap<String, String>,
    },
    Json {
        value: Value,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum ResourceContent {
    Text { text: String },
    Blob { data: Vec<u8> },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpStructuredResult {
    pub contract_version: u32,
    pub blocks: Vec<McpContentBlock>,
    pub structured_content: Option<Value>,
    pub metadata: BTreeMap<String, Value>,
    pub is_error: bool,
}

impl McpStructuredResult {
    pub fn validate(&self, limits: &McpContentLimits) -> McpCoreResult<()> {
        limits.validate()?;
        if self.contract_version != MCP_CONTENT_CONTRACT_VERSION
            || self.blocks.len() > limits.max_blocks
            || self.metadata.len() > limits.max_metadata_entries
        {
            return Err(McpCoreError::InvalidContent(
                "content contract version or collection limits are invalid".to_string(),
            ));
        }
        let mut total = 0_usize;
        for block in &self.blocks {
            let size = match block {
                McpContentBlock::Text { text, annotations } => {
                    validate_annotations(annotations)?;
                    text.len()
                }
                McpContentBlock::Image {
                    mime_type,
                    data,
                    alt_text,
                } => {
                    validate_binary_mime(mime_type, "image/")?;
                    if alt_text.as_ref().is_some_and(|text| text.len() > 8_192) {
                        return Err(McpCoreError::LimitExceeded(
                            "image alt text exceeds limit".to_string(),
                        ));
                    }
                    data.len()
                }
                McpContentBlock::Audio {
                    mime_type,
                    data,
                    transcript,
                } => {
                    validate_binary_mime(mime_type, "audio/")?;
                    if transcript
                        .as_ref()
                        .is_some_and(|text| text.len() > limits.max_text_bytes)
                    {
                        return Err(McpCoreError::LimitExceeded(
                            "audio transcript exceeds limit".to_string(),
                        ));
                    }
                    data.len()
                }
                McpContentBlock::Resource {
                    uri,
                    name,
                    mime_type,
                    content,
                    annotations,
                } => {
                    validate_resource_uri(uri)?;
                    validate_annotations(annotations)?;
                    if name.as_ref().is_some_and(|name| name.len() > 512)
                        || mime_type.as_ref().is_some_and(|mime| mime.len() > 160)
                    {
                        return Err(McpCoreError::LimitExceeded(
                            "resource metadata exceeds limit".to_string(),
                        ));
                    }
                    match content {
                        ResourceContent::Text { text } => text.len(),
                        ResourceContent::Blob { data } => data.len(),
                    }
                }
                McpContentBlock::Json { value } => serde_json::to_vec(value)?.len(),
            };
            if size > limits.max_block_bytes {
                return Err(McpCoreError::LimitExceeded(
                    "MCP content block exceeds per-block limit".to_string(),
                ));
            }
            total = total.checked_add(size).ok_or_else(|| {
                McpCoreError::LimitExceeded("MCP content size overflow".to_string())
            })?;
            if total > limits.max_total_bytes {
                return Err(McpCoreError::LimitExceeded(
                    "MCP result exceeds total content limit".to_string(),
                ));
            }
        }
        if self.structured_content.as_ref().is_some_and(|value| {
            serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > limits.max_block_bytes)
        }) {
            return Err(McpCoreError::LimitExceeded(
                "structured_content exceeds limit".to_string(),
            ));
        }
        Ok(())
    }

    pub fn text_fallback(&self, limits: &McpContentLimits) -> McpCoreResult<String> {
        self.validate(limits)?;
        let mut sections = Vec::new();
        for block in &self.blocks {
            let text = match block {
                McpContentBlock::Text { text, .. } => text.clone(),
                McpContentBlock::Image {
                    mime_type,
                    data,
                    alt_text,
                } => format!(
                    "Image ({mime_type}, {} bytes, sha256 {}){}",
                    data.len(),
                    sha256(data),
                    alt_text
                        .as_ref()
                        .map_or_else(String::new, |text| format!("\nAlt: {text}"))
                ),
                McpContentBlock::Audio {
                    mime_type,
                    data,
                    transcript,
                } => format!(
                    "Audio ({mime_type}, {} bytes, sha256 {}){}",
                    data.len(),
                    sha256(data),
                    transcript
                        .as_ref()
                        .map_or_else(String::new, |text| format!("\nTranscript: {text}"))
                ),
                McpContentBlock::Resource {
                    uri,
                    name,
                    mime_type,
                    content,
                    ..
                } => {
                    let heading = format!(
                        "Resource: {} ({}, {})",
                        name.as_deref().unwrap_or("unnamed"),
                        mime_type.as_deref().unwrap_or("unknown type"),
                        uri
                    );
                    match content {
                        ResourceContent::Text { text } => format!("{heading}\n{text}"),
                        ResourceContent::Blob { data } => format!(
                            "{heading}\nBinary: {} bytes, sha256 {}",
                            data.len(),
                            sha256(data)
                        ),
                    }
                }
                McpContentBlock::Json { value } => {
                    format!("Structured JSON:\n{}", serde_json::to_string_pretty(value)?)
                }
            };
            sections.push(text);
        }
        if let Some(value) = &self.structured_content {
            sections.push(format!(
                "Result structured_content:\n{}",
                serde_json::to_string_pretty(value)?
            ));
        }
        let mut output = sections.join("\n\n");
        if output.len() > limits.max_text_bytes {
            let boundary = floor_char_boundary(&output, limits.max_text_bytes.saturating_sub(24));
            output.truncate(boundary);
            output.push_str("\n[Fallback truncated]");
        }
        Ok(output)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct McpContentLimits {
    pub max_blocks: usize,
    pub max_block_bytes: usize,
    pub max_total_bytes: usize,
    pub max_text_bytes: usize,
    pub max_metadata_entries: usize,
}

impl Default for McpContentLimits {
    fn default() -> Self {
        Self {
            max_blocks: 256,
            max_block_bytes: 32 * 1024 * 1024,
            max_total_bytes: 128 * 1024 * 1024,
            max_text_bytes: 2 * 1024 * 1024,
            max_metadata_entries: 256,
        }
    }
}

impl McpContentLimits {
    fn validate(&self) -> McpCoreResult<()> {
        if self.max_blocks == 0
            || self.max_block_bytes == 0
            || self.max_total_bytes < self.max_block_bytes
            || self.max_text_bytes == 0
            || self.max_metadata_entries == 0
        {
            return Err(McpCoreError::LimitExceeded(
                "MCP content limits are inconsistent".to_string(),
            ));
        }
        Ok(())
    }
}

fn validate_annotations(annotations: &BTreeMap<String, String>) -> McpCoreResult<()> {
    if annotations.len() > 64
        || annotations
            .iter()
            .any(|(key, value)| key.len() > 128 || value.len() > 2_048)
    {
        return Err(McpCoreError::LimitExceeded(
            "MCP annotations exceed limits".to_string(),
        ));
    }
    Ok(())
}

fn validate_binary_mime(value: &str, prefix: &str) -> McpCoreResult<()> {
    if !value.starts_with(prefix)
        || value.len() > 160
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(McpCoreError::InvalidContent(
            "invalid binary MIME type".to_string(),
        ));
    }
    Ok(())
}

fn validate_resource_uri(value: &str) -> McpCoreResult<()> {
    let url = Url::parse(value)
        .map_err(|error| McpCoreError::InvalidContent(format!("resource URI: {error}")))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || value.len() > 4_096
        || !matches!(url.scheme(), "https" | "file" | "mcp" | "artifact" | "ui")
    {
        return Err(McpCoreError::InvalidContent(
            "resource URI scheme/credentials are invalid".to_string(),
        ));
    }
    Ok(())
}

fn floor_char_boundary(value: &str, index: usize) -> usize {
    let mut index = index.min(value.len());
    while index > 0 && !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

// ---------------------------------------------------------------------------
// Deterministic relevant-tool routing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpToolDescriptor {
    pub server_id: String,
    pub tool_name: String,
    pub title: String,
    pub description: String,
    pub tags: BTreeSet<String>,
    pub input_schema: Value,
    pub required_permissions: BTreeSet<String>,
    pub enabled: bool,
    pub allowlisted: bool,
}

impl McpToolDescriptor {
    pub fn qualified_id(&self) -> String {
        format!("mcp__{}__{}", self.server_id, self.tool_name)
    }

    fn validate(&self) -> McpCoreResult<()> {
        validate_tool_component("server_id", &self.server_id)?;
        validate_tool_component("tool_name", &self.tool_name)?;
        if self.title.trim().is_empty()
            || self.title.len() > 256
            || self.description.len() > 8_192
            || self.tags.len() > 64
            || self.tags.iter().any(|tag| tag.is_empty() || tag.len() > 80)
            || !self.input_schema.is_object()
            || serde_json::to_vec(&self.input_schema)?.len() > 256 * 1024
        {
            return Err(McpCoreError::InvalidTool(format!(
                "invalid descriptor: {}",
                self.qualified_id()
            )));
        }
        for permission in &self.required_permissions {
            validate_tool_component("permission", permission)?;
        }
        Ok(())
    }
}

fn validate_tool_component(label: &str, value: &str) -> McpCoreResult<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(McpCoreError::InvalidTool(format!(
            "invalid {label}: {value}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolRoutingPolicy {
    pub allowed_servers: BTreeSet<String>,
    pub allowed_tool_ids: Option<BTreeSet<String>>,
    pub granted_permissions: BTreeSet<String>,
    pub maximum_tools: usize,
    pub explicitly_selected_router_model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoutedTool {
    pub qualified_id: String,
    pub deterministic_score: i64,
    pub reasons: Vec<String>,
    pub router_model_rank: Option<u32>,
}

pub trait ToolRouterModel: Send + Sync {
    fn model_id(&self) -> &str;
    /// May only reorder the supplied candidate ids.
    fn rank(&self, query: &str, candidate_ids: &[String]) -> Result<Vec<String>, String>;
}

pub fn route_tools(
    query: &str,
    catalog: &[McpToolDescriptor],
    policy: &ToolRoutingPolicy,
    router_model: Option<&dyn ToolRouterModel>,
) -> McpCoreResult<Vec<RoutedTool>> {
    if query.trim().is_empty() || query.len() > 32_768 || policy.maximum_tools == 0 {
        return Err(McpCoreError::Router(
            "routing query or maximum tool count is invalid".to_string(),
        ));
    }
    if router_model.is_some() != policy.explicitly_selected_router_model.is_some() {
        return Err(McpCoreError::Router(
            "router model must be explicitly selected, and selected models must be provided"
                .to_string(),
        ));
    }
    if let (Some(expected), Some(model)) = (
        policy.explicitly_selected_router_model.as_deref(),
        router_model,
    ) {
        if expected != model.model_id() {
            return Err(McpCoreError::Router(
                "router model identity differs from explicit selection".to_string(),
            ));
        }
    }
    let query_tokens = tokenize(query);
    let query_lower = query.to_ascii_lowercase();
    let mut routed = Vec::new();
    let mut seen_ids = HashSet::new();
    for tool in catalog {
        tool.validate()?;
        let id = tool.qualified_id();
        if !seen_ids.insert(id.clone()) {
            return Err(McpCoreError::InvalidTool(format!(
                "duplicate tool id: {id}"
            )));
        }
        if !tool.enabled
            || !tool.allowlisted
            || !policy.allowed_servers.contains(&tool.server_id)
            || policy
                .allowed_tool_ids
                .as_ref()
                .is_some_and(|allowed| !allowed.contains(&id))
            || !tool
                .required_permissions
                .is_subset(&policy.granted_permissions)
        {
            continue;
        }
        let mut score = 0_i64;
        let mut reasons = Vec::new();
        let name_lower = tool.tool_name.to_ascii_lowercase();
        let title_lower = tool.title.to_ascii_lowercase();
        let description_tokens = tokenize(&tool.description);
        if query_lower == name_lower || query_lower == id.to_ascii_lowercase() {
            score += 10_000;
            reasons.push("exact_name".to_string());
        }
        for token in &query_tokens {
            if name_lower.split(['-', '_']).any(|part| part == token) {
                score += 1_000;
                reasons.push(format!("name:{token}"));
            }
            if title_lower
                .split(|character: char| !character.is_alphanumeric())
                .any(|part| part == token)
            {
                score += 600;
                reasons.push(format!("title:{token}"));
            }
            if tool.tags.iter().any(|tag| tag.eq_ignore_ascii_case(token)) {
                score += 800;
                reasons.push(format!("tag:{token}"));
            }
            if description_tokens.contains(token) {
                score += 100;
                reasons.push(format!("description:{token}"));
            }
        }
        if score > 0 {
            reasons.sort();
            reasons.dedup();
            routed.push(RoutedTool {
                qualified_id: id,
                deterministic_score: score,
                reasons,
                router_model_rank: None,
            });
        }
    }
    routed.sort_by(|left, right| {
        right
            .deterministic_score
            .cmp(&left.deterministic_score)
            .then_with(|| left.qualified_id.cmp(&right.qualified_id))
    });
    if let Some(model) = router_model {
        let ids = routed
            .iter()
            .map(|tool| tool.qualified_id.clone())
            .collect::<Vec<_>>();
        let ranked = model.rank(query, &ids).map_err(McpCoreError::Router)?;
        let expected = ids.iter().collect::<HashSet<_>>();
        let actual = ranked.iter().collect::<HashSet<_>>();
        if ranked.len() != ids.len() || actual.len() != ranked.len() || expected != actual {
            return Err(McpCoreError::Router(
                "router model attempted to add, omit, or duplicate a candidate".to_string(),
            ));
        }
        let rank = ranked
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index as u32 + 1))
            .collect::<HashMap<_, _>>();
        for tool in &mut routed {
            tool.router_model_rank = rank.get(tool.qualified_id.as_str()).copied();
        }
        routed.sort_by(|left, right| {
            left.router_model_rank
                .cmp(&right.router_model_rank)
                .then_with(|| right.deterministic_score.cmp(&left.deterministic_score))
                .then_with(|| left.qualified_id.cmp(&right.qualified_id))
        });
    }
    routed.truncate(policy.maximum_tools);
    Ok(routed)
}

fn tokenize(value: &str) -> BTreeSet<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.len() >= 2)
        .take(256)
        .map(str::to_ascii_lowercase)
        .collect()
}

// ---------------------------------------------------------------------------
// Opaque-origin MCP UI host and narrow bridge authorization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum HostActionKind {
    InvokeTool,
    OpenExternalUrl,
    WriteClipboardText,
    PublishArtifact,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeclaredHostAction {
    pub action_id: String,
    pub kind: HostActionKind,
    pub target: String,
    pub required_permission: String,
    pub always_requires_approval: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpUiManifest {
    pub contract_version: u32,
    pub server_id: String,
    pub resource_uri: String,
    pub resource_sha256: String,
    pub entry_media_type: String,
    pub network_origins: BTreeSet<String>,
    pub host_actions: BTreeMap<String, DeclaredHostAction>,
    pub text_fallback: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpUiHostPlan {
    pub opaque_origin_required: bool,
    pub iframe_sandbox_tokens: BTreeSet<String>,
    pub content_security_policy: String,
    pub bridge_action_ids: BTreeSet<String>,
    pub tauri_ipc_exposed: bool,
    pub filesystem_exposed: bool,
    pub keychain_exposed: bool,
    pub text_fallback: String,
}

pub fn build_ui_host_plan(manifest: &McpUiManifest) -> McpCoreResult<McpUiHostPlan> {
    validate_ui_manifest(manifest)?;
    let connect_src = if manifest.network_origins.is_empty() {
        "'none'".to_string()
    } else {
        manifest
            .network_origins
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ")
    };
    Ok(McpUiHostPlan {
        opaque_origin_required: true,
        iframe_sandbox_tokens: ["allow-scripts".to_string()].into_iter().collect(),
        content_security_policy: format!(
            "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data: blob:; media-src data: blob:; connect-src {connect_src}; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'"
        ),
        bridge_action_ids: manifest.host_actions.keys().cloned().collect(),
        tauri_ipc_exposed: false,
        filesystem_exposed: false,
        keychain_exposed: false,
        text_fallback: manifest.text_fallback.clone(),
    })
}

fn validate_ui_manifest(manifest: &McpUiManifest) -> McpCoreResult<()> {
    if manifest.contract_version != MCP_UI_HOST_CONTRACT_VERSION {
        return Err(McpCoreError::UiPolicy(
            "unsupported UI host contract version".to_string(),
        ));
    }
    validate_tool_component("server_id", &manifest.server_id)
        .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?;
    validate_resource_uri(&manifest.resource_uri)
        .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?;
    let resource_scheme = Url::parse(&manifest.resource_uri)
        .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?
        .scheme()
        .to_string();
    if manifest.resource_sha256.len() != 64
        || !manifest
            .resource_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        || !matches!(resource_scheme.as_str(), "mcp" | "artifact" | "ui")
        || !matches!(
            manifest.entry_media_type.as_str(),
            "text/html" | "image/svg+xml"
        )
        || manifest.text_fallback.trim().is_empty()
        || manifest.text_fallback.len() > 256 * 1024
        || manifest.host_actions.len() > 64
    {
        return Err(McpCoreError::UiPolicy(
            "UI resource hash/media/fallback/actions are invalid".to_string(),
        ));
    }
    for origin in &manifest.network_origins {
        let url = validate_https_url("UI network origin", origin)
            .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?;
        if url.path() != "/"
            || url.query().is_some()
            || url.origin().ascii_serialization() != *origin
        {
            return Err(McpCoreError::UiPolicy(
                "UI network targets must be canonical HTTPS origins".to_string(),
            ));
        }
    }
    for (action_id, action) in &manifest.host_actions {
        validate_tool_component("action_id", action_id)
            .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?;
        if action.action_id != *action_id
            || action.target.is_empty()
            || action.target.len() > 2_048
            || action.required_permission.is_empty()
        {
            return Err(McpCoreError::UiPolicy(format!(
                "invalid host action: {action_id}"
            )));
        }
        match action.kind {
            HostActionKind::InvokeTool => {
                if !action
                    .target
                    .starts_with(&format!("mcp__{}__", manifest.server_id))
                    || !action.always_requires_approval
                {
                    return Err(McpCoreError::UiPolicy(
                        "tool bridge actions must target this server and always require approval"
                            .to_string(),
                    ));
                }
            }
            HostActionKind::OpenExternalUrl => {
                let url = validate_https_url("external URL", &action.target)
                    .map_err(|error| McpCoreError::UiPolicy(error.to_string()))?;
                let origin = url.origin().ascii_serialization();
                if !manifest.network_origins.contains(&origin) || !action.always_requires_approval {
                    return Err(McpCoreError::UiPolicy(
                        "external URL action target is undeclared or lacks approval".to_string(),
                    ));
                }
            }
            HostActionKind::WriteClipboardText | HostActionKind::PublishArtifact => {
                if !action.always_requires_approval {
                    return Err(McpCoreError::UiPolicy(
                        "side-effecting host action must always require approval".to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

pub fn verify_ui_resource_bytes(manifest: &McpUiManifest, bytes: &[u8]) -> McpCoreResult<()> {
    validate_ui_manifest(manifest)?;
    if bytes.len() > 16 * 1024 * 1024 || sha256(bytes) != manifest.resource_sha256 {
        return Err(McpCoreError::UiPolicy(
            "UI resource bytes exceed limits or fail their checksum".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq)]
pub struct BridgeCapability(String);

impl BridgeCapability {
    pub fn new(value: String) -> McpCoreResult<Self> {
        if value.len() < 32 || value.len() > 512 {
            return Err(McpCoreError::UiPolicy(
                "bridge capability has invalid entropy/length".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn hash(&self) -> String {
        sha256(self.0.as_bytes())
    }

    /// Deliberate one-time handoff to the trusted host/bootstrap layer. The
    /// type intentionally remains non-serializable so it cannot leak through
    /// unrelated logs, histories, or generic IPC payloads by accident.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BridgeCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BridgeCapability([REDACTED])")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiBridgeRequest {
    pub session_id: String,
    pub server_id: String,
    pub resource_sha256: String,
    pub action_id: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthorizedBridgeAction {
    pub session_id: String,
    pub action: DeclaredHostAction,
    pub payload: Value,
    pub approval_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PreparedBridgeAction {
    pub session_id: String,
    pub action: DeclaredHostAction,
    pub payload: Value,
    pub payload_summary_sha256: String,
}

pub trait UiActionApprovalGate: Send + Sync {
    fn approve(
        &self,
        session_id: &str,
        action: &DeclaredHostAction,
        payload_summary_sha256: &str,
    ) -> Result<Option<String>, String>;
}

pub fn prepare_ui_bridge_action(
    manifest: &McpUiManifest,
    expected_session_id: &str,
    expected_capability_hash: &str,
    presented_capability: &BridgeCapability,
    granted_permissions: &BTreeSet<String>,
    request: UiBridgeRequest,
) -> McpCoreResult<PreparedBridgeAction> {
    validate_ui_manifest(manifest)?;
    if presented_capability.hash() != expected_capability_hash
        || request.session_id != expected_session_id
        || request.server_id != manifest.server_id
        || request.resource_sha256 != manifest.resource_sha256
        || serde_json::to_vec(&request.payload)?.len() > 256 * 1024
    {
        return Err(McpCoreError::UiPolicy(
            "bridge request is not bound to this opaque UI session/resource".to_string(),
        ));
    }
    let action = manifest
        .host_actions
        .get(&request.action_id)
        .ok_or_else(|| {
            McpCoreError::UiPolicy("UI requested an undeclared host action".to_string())
        })?;
    if !granted_permissions.contains(&action.required_permission) {
        return Err(McpCoreError::ApprovalDenied(
            "required Little Monkey permission is not granted".to_string(),
        ));
    }
    let payload_summary_sha256 = sha256(&serde_json::to_vec(&request.payload)?);
    Ok(PreparedBridgeAction {
        session_id: request.session_id,
        action: action.clone(),
        payload: request.payload,
        payload_summary_sha256,
    })
}

pub fn authorize_ui_bridge_action(
    manifest: &McpUiManifest,
    expected_session_id: &str,
    expected_capability_hash: &str,
    presented_capability: &BridgeCapability,
    granted_permissions: &BTreeSet<String>,
    request: UiBridgeRequest,
    approval_gate: &dyn UiActionApprovalGate,
) -> McpCoreResult<AuthorizedBridgeAction> {
    let prepared = prepare_ui_bridge_action(
        manifest,
        expected_session_id,
        expected_capability_hash,
        presented_capability,
        granted_permissions,
        request,
    )?;
    let approval_id = approval_gate
        .approve(
            &prepared.session_id,
            &prepared.action,
            &prepared.payload_summary_sha256,
        )
        .map_err(McpCoreError::ApprovalDenied)?
        .ok_or_else(|| McpCoreError::ApprovalDenied("user denied host action".to_string()))?;
    validate_id("approval_id", &approval_id)
        .map_err(|error| McpCoreError::ApprovalDenied(error.to_string()))?;
    Ok(AuthorizedBridgeAction {
        session_id: prepared.session_id,
        action: prepared.action,
        payload: prepared.payload,
        approval_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn scopes(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn server() -> OAuthServerMetadata {
        OAuthServerMetadata {
            contract_version: MCP_OAUTH_CONTRACT_VERSION,
            issuer: "https://auth.example.com/".to_string(),
            authorization_endpoint: "https://auth.example.com/authorize".to_string(),
            token_endpoint: "https://auth.example.com/token".to_string(),
            revocation_endpoint: Some("https://auth.example.com/revoke".to_string()),
            supported_scopes: scopes(&["read", "write"]),
            supports_pkce_s256: true,
        }
    }

    fn client() -> OAuthClientConfig {
        OAuthClientConfig {
            server_id: "fixture-server".to_string(),
            client_id: "fixture-client".to_string(),
            redirect_uri: "http://127.0.0.1:43127/callback".to_string(),
            requested_scopes: scopes(&["read"]),
        }
    }

    fn tokens(
        access: &str,
        refresh: Option<&str>,
        granted_scopes: BTreeSet<String>,
        issued_unix_ms: u64,
    ) -> OAuthTokenSet {
        OAuthTokenSet::new(
            SecretMaterial::new(access.to_string()).unwrap(),
            refresh.map(|value| SecretMaterial::new(value.to_string()).unwrap()),
            "Bearer".to_string(),
            granted_scopes,
            issued_unix_ms,
            issued_unix_ms + 3_600_000,
        )
        .unwrap()
    }

    struct FixedSecurity;

    impl OAuthSecurityProvider for FixedSecurity {
        fn generate_pkce(&self) -> Result<PkceMaterial, String> {
            Ok(PkceMaterial {
                state: "state_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                verifier: SecretMaterial::new(
                    "verifier_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                )
                .unwrap(),
                challenge_s256: "challenge_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
            })
        }
    }

    #[derive(Default)]
    struct MemoryFlowStore {
        flows: Mutex<BTreeMap<String, PendingOAuthFlow>>,
    }

    impl OAuthFlowStore for MemoryFlowStore {
        fn put(&self, state: PendingOAuthFlow) -> Result<(), String> {
            let previous = self
                .flows
                .lock()
                .map_err(|_| "flow lock poisoned".to_string())?
                .insert(state.state_sha256.clone(), state);
            if previous.is_some() {
                return Err("duplicate OAuth state".to_string());
            }
            Ok(())
        }

        fn take_by_state_hash(
            &self,
            state_sha256: &str,
        ) -> Result<Option<PendingOAuthFlow>, String> {
            Ok(self
                .flows
                .lock()
                .map_err(|_| "flow lock poisoned".to_string())?
                .remove(state_sha256))
        }
    }

    #[derive(Default)]
    struct MemoryVault {
        ephemeral: Mutex<BTreeMap<String, SecretMaterial>>,
        tokens: Mutex<BTreeMap<String, OAuthTokenSet>>,
        deleted_ephemeral: Mutex<usize>,
        next: Mutex<u64>,
    }

    impl MemoryVault {
        fn reference(&self, prefix: &str) -> Result<SecretReference, String> {
            let mut next = self
                .next
                .lock()
                .map_err(|_| "counter lock poisoned".to_string())?;
            *next += 1;
            Ok(SecretReference {
                vault_id: "test-vault".to_string(),
                reference_id: format!("{prefix}-{next}"),
            })
        }
    }

    impl OAuthSecretVault for MemoryVault {
        fn put_ephemeral(
            &self,
            _label: &str,
            secret: SecretMaterial,
        ) -> Result<SecretReference, String> {
            let reference = self.reference("ephemeral")?;
            self.ephemeral
                .lock()
                .map_err(|_| "ephemeral lock poisoned".to_string())?
                .insert(reference.reference_id.clone(), secret);
            Ok(reference)
        }

        fn get_ephemeral(&self, reference: &SecretReference) -> Result<SecretMaterial, String> {
            self.ephemeral
                .lock()
                .map_err(|_| "ephemeral lock poisoned".to_string())?
                .get(&reference.reference_id)
                .cloned()
                .ok_or_else(|| "ephemeral secret not found".to_string())
        }

        fn delete_ephemeral(&self, reference: &SecretReference) -> Result<(), String> {
            let removed = self
                .ephemeral
                .lock()
                .map_err(|_| "ephemeral lock poisoned".to_string())?
                .remove(&reference.reference_id);
            if removed.is_none() {
                return Err("ephemeral secret not found".to_string());
            }
            *self
                .deleted_ephemeral
                .lock()
                .map_err(|_| "delete counter lock poisoned".to_string())? += 1;
            Ok(())
        }

        fn put_tokens(
            &self,
            _server_id: &str,
            tokens: OAuthTokenSet,
        ) -> Result<SecretReference, String> {
            let reference = self.reference("tokens")?;
            self.tokens
                .lock()
                .map_err(|_| "token lock poisoned".to_string())?
                .insert(reference.reference_id.clone(), tokens);
            Ok(reference)
        }

        fn get_tokens(&self, reference: &SecretReference) -> Result<OAuthTokenSet, String> {
            self.tokens
                .lock()
                .map_err(|_| "token lock poisoned".to_string())?
                .get(&reference.reference_id)
                .cloned()
                .ok_or_else(|| "token set not found".to_string())
        }

        fn replace_tokens(
            &self,
            reference: &SecretReference,
            tokens: OAuthTokenSet,
        ) -> Result<(), String> {
            let mut stored = self
                .tokens
                .lock()
                .map_err(|_| "token lock poisoned".to_string())?;
            if !stored.contains_key(&reference.reference_id) {
                return Err("token set not found".to_string());
            }
            stored.insert(reference.reference_id.clone(), tokens);
            Ok(())
        }

        fn delete_tokens(&self, reference: &SecretReference) -> Result<(), String> {
            if self
                .tokens
                .lock()
                .map_err(|_| "token lock poisoned".to_string())?
                .remove(&reference.reference_id)
                .is_none()
            {
                return Err("token set not found".to_string());
            }
            Ok(())
        }
    }

    struct TestTransport {
        exchange_result: Mutex<Option<Result<OAuthTokenSet, String>>>,
        refresh_result: Mutex<Option<Result<OAuthTokenSet, String>>>,
        exchanges: Mutex<Vec<(String, String)>>,
        revoked: Mutex<Vec<String>>,
    }

    impl TestTransport {
        fn new(exchange: Result<OAuthTokenSet, String>) -> Self {
            Self {
                exchange_result: Mutex::new(Some(exchange)),
                refresh_result: Mutex::new(None),
                exchanges: Mutex::new(Vec::new()),
                revoked: Mutex::new(Vec::new()),
            }
        }

        fn set_refresh(&self, result: Result<OAuthTokenSet, String>) {
            *self.refresh_result.lock().unwrap() = Some(result);
        }
    }

    impl OAuthTransport for TestTransport {
        fn exchange_code(
            &self,
            request: OAuthCodeExchangeRequest,
        ) -> Result<OAuthTokenSet, String> {
            self.exchanges.lock().unwrap().push((
                request.code.expose().to_string(),
                request.pkce_verifier.expose().to_string(),
            ));
            self.exchange_result
                .lock()
                .map_err(|_| "exchange lock poisoned".to_string())?
                .take()
                .ok_or_else(|| "exchange called more than once".to_string())?
        }

        fn refresh(&self, request: OAuthRefreshRequest) -> Result<OAuthTokenSet, String> {
            if request.refresh_token.expose().is_empty() {
                return Err("empty refresh token".to_string());
            }
            self.refresh_result
                .lock()
                .map_err(|_| "refresh lock poisoned".to_string())?
                .take()
                .ok_or_else(|| "refresh response not configured".to_string())?
        }

        fn revoke(
            &self,
            endpoint: &str,
            _client_id: &str,
            token: SecretMaterial,
        ) -> Result<(), String> {
            self.revoked
                .lock()
                .map_err(|_| "revoke lock poisoned".to_string())?
                .push(format!("{endpoint}:{}", token.expose()));
            Ok(())
        }
    }

    #[test]
    fn oauth_pkce_flow_persists_only_hashes_and_rejects_replay() {
        let vault = MemoryVault::default();
        let flows = MemoryFlowStore::default();
        let transport = TestTransport::new(Ok(tokens(
            "access-one",
            Some("refresh-one"),
            scopes(&["read"]),
            1_000,
        )));
        let plan = begin_oauth(
            &server(),
            &client(),
            &FixedSecurity,
            &vault,
            &flows,
            1_000,
            60_000,
        )
        .unwrap();
        let state = "state_abcdefghijklmnopqrstuvwxyz0123456789";
        assert!(plan
            .authorization_url
            .contains("code_challenge_method=S256"));
        let pending = flows.flows.lock().unwrap().values().next().unwrap().clone();
        let persisted = serde_json::to_string(&pending).unwrap();
        assert!(!persisted.contains(state));
        assert!(!persisted.contains("verifier_abcdefghijklmnopqrstuvwxyz0123456789"));
        assert_eq!(
            format!(
                "{:?}",
                SecretMaterial::new("raw-secret".to_string()).unwrap()
            ),
            "SecretMaterial([REDACTED])"
        );

        let metadata = complete_oauth(
            &server(),
            &client(),
            OAuthCallback {
                state: state.to_string(),
                code: SecretMaterial::new("authorization-code".to_string()).unwrap(),
                error: None,
            },
            &vault,
            &flows,
            &transport,
            2_000,
        )
        .unwrap();
        assert_eq!(metadata.granted_scopes, scopes(&["read"]));
        assert_eq!(*vault.deleted_ephemeral.lock().unwrap(), 1);
        assert_eq!(
            transport.exchanges.lock().unwrap()[0].1,
            "verifier_abcdefghijklmnopqrstuvwxyz0123456789"
        );
        assert!(matches!(
            complete_oauth(
                &server(),
                &client(),
                OAuthCallback {
                    state: state.to_string(),
                    code: SecretMaterial::new("second-code".to_string()).unwrap(),
                    error: None,
                },
                &vault,
                &flows,
                &transport,
                2_001,
            ),
            Err(McpCoreError::OAuthState(_))
        ));
    }

    #[test]
    fn oauth_refresh_preserves_rotating_secret_and_rejects_scope_expansion() {
        let vault = MemoryVault::default();
        let original = tokens("access-one", Some("refresh-one"), scopes(&["read"]), 1_000);
        let reference = vault.put_tokens("fixture-server", original).unwrap();
        let metadata = OAuthTokenMetadata {
            token_reference: reference.clone(),
            token_type: "Bearer".to_string(),
            granted_scopes: scopes(&["read"]),
            issued_unix_ms: 1_000,
            expires_unix_ms: 3_601_000,
        };
        let transport = TestTransport::new(Err("unused".to_string()));
        transport.set_refresh(Ok(tokens("access-two", None, scopes(&["read"]), 2_000)));
        let updated =
            refresh_oauth(&server(), &client(), &metadata, &vault, &transport, 2_000).unwrap();
        assert_eq!(updated.issued_unix_ms, 2_000);
        let stored = vault.get_tokens(&reference).unwrap();
        assert_eq!(stored.access_token().expose(), "access-two");
        assert_eq!(stored.refresh_token().unwrap().expose(), "refresh-one");

        transport.set_refresh(Ok(tokens(
            "access-three",
            None,
            scopes(&["read", "write"]),
            3_000,
        )));
        assert!(matches!(
            refresh_oauth(&server(), &client(), &updated, &vault, &transport, 3_000),
            Err(McpCoreError::InvalidOAuth(_))
        ));
        assert_eq!(
            vault
                .get_tokens(&reference)
                .unwrap()
                .access_token()
                .expose(),
            "access-two"
        );
    }

    #[test]
    fn failed_exchange_consumes_state_and_deletes_ephemeral_verifier() {
        let vault = MemoryVault::default();
        let flows = MemoryFlowStore::default();
        let transport = TestTransport::new(Err("network unavailable".to_string()));
        begin_oauth(
            &server(),
            &client(),
            &FixedSecurity,
            &vault,
            &flows,
            10,
            60_000,
        )
        .unwrap();
        let result = complete_oauth(
            &server(),
            &client(),
            OAuthCallback {
                state: "state_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
                code: SecretMaterial::new("authorization-code".to_string()).unwrap(),
                error: None,
            },
            &vault,
            &flows,
            &transport,
            20,
        );
        assert_eq!(
            result.unwrap_err(),
            McpCoreError::Transport("network unavailable".to_string())
        );
        assert!(vault.ephemeral.lock().unwrap().is_empty());
        assert!(flows.flows.lock().unwrap().is_empty());
    }

    #[test]
    fn structured_content_round_trips_and_text_fallback_keeps_useful_data() {
        let result = McpStructuredResult {
            contract_version: MCP_CONTENT_CONTRACT_VERSION,
            blocks: vec![
                McpContentBlock::Text {
                    text: "plain text".to_string(),
                    annotations: BTreeMap::from([("audience".to_string(), "user".to_string())]),
                },
                McpContentBlock::Image {
                    mime_type: "image/png".to_string(),
                    data: vec![1, 2, 3],
                    alt_text: Some("chart".to_string()),
                },
                McpContentBlock::Audio {
                    mime_type: "audio/wav".to_string(),
                    data: vec![4, 5, 6],
                    transcript: Some("spoken result".to_string()),
                },
                McpContentBlock::Resource {
                    uri: "mcp://fixture-server/report".to_string(),
                    name: Some("report".to_string()),
                    mime_type: Some("text/markdown".to_string()),
                    content: ResourceContent::Text {
                        text: "resource body".to_string(),
                    },
                    annotations: BTreeMap::new(),
                },
                McpContentBlock::Json {
                    value: serde_json::json!({"answer": 42}),
                },
            ],
            structured_content: Some(serde_json::json!({"rows": [1, 2]})),
            metadata: BTreeMap::from([("request_id".to_string(), serde_json::json!("r-1"))]),
            is_error: false,
        };
        let encoded = serde_json::to_vec(&result).unwrap();
        let decoded: McpStructuredResult = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, result);
        let fallback = result.text_fallback(&McpContentLimits::default()).unwrap();
        for expected in [
            "plain text",
            "sha256",
            "spoken result",
            "resource body",
            "answer",
            "structured_content",
        ] {
            assert!(fallback.contains(expected), "fallback missed {expected}");
        }
        let limits = McpContentLimits {
            max_blocks: 1,
            ..McpContentLimits::default()
        };
        assert!(matches!(
            result.validate(&limits),
            Err(McpCoreError::InvalidContent(_))
        ));
    }

    fn tool(server_id: &str, tool_name: &str, tag: &str, permission: &str) -> McpToolDescriptor {
        McpToolDescriptor {
            server_id: server_id.to_string(),
            tool_name: tool_name.to_string(),
            title: tool_name.replace('_', " "),
            description: format!("Use this tool for {tag} work"),
            tags: scopes(&[tag]),
            input_schema: serde_json::json!({"type": "object"}),
            required_permissions: scopes(&[permission]),
            enabled: true,
            allowlisted: true,
        }
    }

    struct ReverseRouter;

    impl ToolRouterModel for ReverseRouter {
        fn model_id(&self) -> &str {
            "explicit-router"
        }

        fn rank(&self, _query: &str, candidate_ids: &[String]) -> Result<Vec<String>, String> {
            Ok(candidate_ids.iter().rev().cloned().collect())
        }
    }

    struct ExpandingRouter;

    impl ToolRouterModel for ExpandingRouter {
        fn model_id(&self) -> &str {
            "explicit-router"
        }

        fn rank(&self, _query: &str, candidate_ids: &[String]) -> Result<Vec<String>, String> {
            let mut result = candidate_ids.to_vec();
            result.push("mcp__evil__exfiltrate".to_string());
            Ok(result)
        }
    }

    #[test]
    fn relevant_tool_routing_is_deterministic_and_model_cannot_expand_authority() {
        let catalog = vec![
            tool("github", "search_issues", "issues", "repo-read"),
            tool("github", "create_issue", "issues", "repo-write"),
            tool("gitlab", "search_issues", "issues", "repo-read"),
        ];
        let mut policy = ToolRoutingPolicy {
            allowed_servers: scopes(&["github"]),
            allowed_tool_ids: None,
            granted_permissions: scopes(&["repo-read"]),
            maximum_tools: 5,
            explicitly_selected_router_model: None,
        };
        let first = route_tools("search issues", &catalog, &policy, None).unwrap();
        let second = route_tools("search issues", &catalog, &policy, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            first
                .iter()
                .map(|item| item.qualified_id.as_str())
                .collect::<Vec<_>>(),
            vec!["mcp__github__search_issues"]
        );

        policy.explicitly_selected_router_model = Some("explicit-router".to_string());
        assert_eq!(
            route_tools("search issues", &catalog, &policy, Some(&ReverseRouter))
                .unwrap()
                .len(),
            1
        );
        assert!(matches!(
            route_tools("search issues", &catalog, &policy, Some(&ExpandingRouter)),
            Err(McpCoreError::Router(_))
        ));
    }

    fn ui_manifest(bytes: &[u8]) -> McpUiManifest {
        McpUiManifest {
            contract_version: MCP_UI_HOST_CONTRACT_VERSION,
            server_id: "fixture-server".to_string(),
            resource_uri: "ui://fixture-server/dashboard".to_string(),
            resource_sha256: sha256(bytes),
            entry_media_type: "text/html".to_string(),
            network_origins: scopes(&["https://api.example.com"]),
            host_actions: BTreeMap::from([(
                "search".to_string(),
                DeclaredHostAction {
                    action_id: "search".to_string(),
                    kind: HostActionKind::InvokeTool,
                    target: "mcp__fixture-server__search".to_string(),
                    required_permission: "search-read".to_string(),
                    always_requires_approval: true,
                },
            )]),
            text_fallback: "Interactive search dashboard unavailable; use the search tool."
                .to_string(),
        }
    }

    struct ApprovalGate(bool);

    impl UiActionApprovalGate for ApprovalGate {
        fn approve(
            &self,
            _session_id: &str,
            _action: &DeclaredHostAction,
            _payload_summary_sha256: &str,
        ) -> Result<Option<String>, String> {
            Ok(self.0.then(|| "approval-1".to_string()))
        }
    }

    #[test]
    fn opaque_ui_host_exposes_no_ambient_privileges_and_checks_resource_bytes() {
        let bytes = b"<html><script>render()</script></html>";
        let manifest = ui_manifest(bytes);
        let plan = build_ui_host_plan(&manifest).unwrap();
        assert!(plan.opaque_origin_required);
        assert_eq!(plan.iframe_sandbox_tokens, scopes(&["allow-scripts"]));
        assert!(!plan.iframe_sandbox_tokens.contains("allow-same-origin"));
        assert!(!plan.tauri_ipc_exposed && !plan.filesystem_exposed && !plan.keychain_exposed);
        assert!(plan.content_security_policy.contains("default-src 'none'"));
        verify_ui_resource_bytes(&manifest, bytes).unwrap();
        assert!(verify_ui_resource_bytes(&manifest, b"tampered").is_err());
    }

    #[test]
    fn ui_bridge_is_session_bound_declared_permissioned_and_approved() {
        let bytes = b"<html></html>";
        let manifest = ui_manifest(bytes);
        let capability =
            BridgeCapability::new("capability_abcdefghijklmnopqrstuvwxyz0123456789".to_string())
                .unwrap();
        let request = UiBridgeRequest {
            session_id: "session-1".to_string(),
            server_id: manifest.server_id.clone(),
            resource_sha256: manifest.resource_sha256.clone(),
            action_id: "search".to_string(),
            payload: serde_json::json!({"query": "rust"}),
        };
        let expected_hash = capability.hash();
        assert!(matches!(
            authorize_ui_bridge_action(
                &manifest,
                "session-1",
                &expected_hash,
                &capability,
                &BTreeSet::new(),
                request.clone(),
                &ApprovalGate(true),
            ),
            Err(McpCoreError::ApprovalDenied(_))
        ));
        assert!(matches!(
            authorize_ui_bridge_action(
                &manifest,
                "session-1",
                &expected_hash,
                &capability,
                &scopes(&["search-read"]),
                request.clone(),
                &ApprovalGate(false),
            ),
            Err(McpCoreError::ApprovalDenied(_))
        ));
        let authorized = authorize_ui_bridge_action(
            &manifest,
            "session-1",
            &expected_hash,
            &capability,
            &scopes(&["search-read"]),
            request.clone(),
            &ApprovalGate(true),
        )
        .unwrap();
        assert_eq!(authorized.approval_id, "approval-1");

        let wrong = BridgeCapability::new(
            "wrong_capability_abcdefghijklmnopqrstuvwxyz0123456789".to_string(),
        )
        .unwrap();
        assert!(matches!(
            authorize_ui_bridge_action(
                &manifest,
                "session-1",
                &expected_hash,
                &wrong,
                &scopes(&["search-read"]),
                request,
                &ApprovalGate(true),
            ),
            Err(McpCoreError::UiPolicy(_))
        ));
    }
}

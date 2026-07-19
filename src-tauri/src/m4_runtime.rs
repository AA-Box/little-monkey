//! Production adapters for the M4 package, MCP App, OAuth, workflow, and
//! daemon-trigger contracts. This module contains no Tauri commands so the
//! same adapters can be used by the desktop app and the resident CLI daemon.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use futures_util::StreamExt;
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::browser_worker::BrowserWorkflowAdapter;
use crate::m4_services::{
    M4ServiceError, McpAppService, McpUiSessionIssuer, PackageRegistryService,
    PersistentWorkflowTriggerRegistrar, UiActionApprovalBroker, UiActionApprovalChallenge,
    WorkflowHumanApprovalBroker, WorkflowHumanApprovalChallenge, WorkflowService,
    WorkflowTriggerBatch, M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
};
use crate::m5_delivery::{DeliveryMutation, ReviewPullRequestRequest};
use crate::mcp;
use crate::mcp_app_core::{
    BridgeCapability, DeclaredHostAction, OAuthCodeExchangeRequest, OAuthFlowStore,
    OAuthRefreshRequest, OAuthSecretVault, OAuthSecurityProvider, OAuthTokenSet, OAuthTransport,
    PendingOAuthFlow, PkceMaterial, PreparedBridgeAction, SecretMaterial, SecretReference,
    UiActionApprovalGate,
};
use crate::package_ecosystem::{
    signed_first_party_catalog, InstallEnvironment, InstallTrustPolicy, PackageLimits,
    RingEd25519SignatureVerifier, SemanticVersion, FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS,
};
use crate::workflow_core::{
    ArtifactReference, DaemonCapability, EffectClass, FailureClass, LegacyRecipeV1,
    NodeAdapterResult, NodeExecutionRequest, ResourceUsage, SecretBinding,
    WorkflowCapabilityCatalog, WorkflowClock, WorkflowNodeExecutor, WorkflowNodeKind,
    WorkflowRunHistory, WorkflowRunRequest, WorkflowTrigger, WorkflowValue, WorkflowValueType,
};

const OAUTH_VAULT_ID: &str = "os-keychain";
const OAUTH_KEYCHAIN_SERVICE: &str = "com.littlemonkey.m4-oauth";
const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;
const UI_APPROVAL_TTL_MS: u64 = 10 * 60 * 1_000;
const WORKFLOW_APPROVAL_TTL_MS: u64 = 30 * 60 * 1_000;

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock was poisoned"))
}

pub fn system_now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn random_urlsafe(byte_len: usize) -> Result<String, String> {
    let mut bytes = vec![0_u8; byte_len];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "operating-system random source failed".to_string())?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    if path.exists()
        && fs::symlink_metadata(path)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
    {
        return Err(format!(
            "private directory cannot be a symlink: {}",
            path.display()
        ));
    }
    fs::create_dir_all(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("protect {}: {e}", path.display()))?;
    }
    Ok(())
}

fn protect_file(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("protect {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("sync {}: {e}", path.display()))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8], replace: bool) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    ensure_private_directory(parent)?;
    let temporary = parent.join(format!(".tmp-{}", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    let mut file = options
        .open(&temporary)
        .map_err(|e| format!("create {}: {e}", temporary.display()))?;
    protect_file(&temporary)?;
    file.write_all(bytes)
        .and_then(|_| file.sync_all())
        .map_err(|e| format!("write {}: {e}", temporary.display()))?;
    drop(file);
    commit_private_temp(&temporary, path, replace).map_err(|e| {
        let _ = fs::remove_file(&temporary);
        format!("commit {}: {e}", path.display())
    })?;
    sync_directory(parent)
}

#[cfg(unix)]
fn commit_private_temp(temporary: &Path, path: &Path, replace: bool) -> std::io::Result<()> {
    if replace {
        fs::rename(temporary, path)
    } else {
        // Linking the already-fsynced inode publishes a create-new record
        // atomically. Unlike an exists-then-rename sequence, a concurrent
        // writer can never be overwritten between the check and commit.
        fs::hard_link(temporary, path)?;
        fs::remove_file(temporary)
    }
}

#[cfg(windows)]
fn commit_private_temp(temporary: &Path, path: &Path, replace: bool) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    // SAFETY: both buffers are owned, NUL-terminated UTF-16 strings and
    // remain alive for the duration of the synchronous Win32 call.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
fn commit_private_temp(temporary: &Path, path: &Path, replace: bool) -> std::io::Result<()> {
    if !replace && path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "destination already exists",
        ));
    }
    fs::rename(temporary, path)
}

// -------------------------------------------------------------------------
// OAuth PKCE, OS-keychain vault, replay-resistant flow store, and transport
// -------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ProductionPkceSecurity;

impl OAuthSecurityProvider for ProductionPkceSecurity {
    fn generate_pkce(&self) -> Result<PkceMaterial, String> {
        let state = random_urlsafe(32)?;
        let verifier_text = random_urlsafe(64)?;
        let challenge_s256 = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(digest(&SHA256, verifier_text.as_bytes()).as_ref());
        Ok(PkceMaterial {
            state,
            verifier: SecretMaterial::new(verifier_text).map_err(|e| e.to_string())?,
            challenge_s256,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TokenEnvelope {
    contract_version: u32,
    access_token: String,
    refresh_token: Option<String>,
    token_type: String,
    granted_scopes: BTreeSet<String>,
    issued_unix_ms: u64,
    expires_unix_ms: u64,
}

#[derive(Debug, Default)]
pub struct KeychainOAuthVault;

impl KeychainOAuthVault {
    fn entry(reference_id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(OAUTH_KEYCHAIN_SERVICE, reference_id)
            .map_err(|e| format!("open OAuth keychain entry: {e}"))
    }

    fn reference(prefix: &str) -> SecretReference {
        SecretReference {
            vault_id: OAUTH_VAULT_ID.to_string(),
            reference_id: format!("{prefix}-{}", Uuid::new_v4().simple()),
        }
    }

    fn checked_reference<'a>(
        reference: &'a SecretReference,
        prefix: &str,
    ) -> Result<&'a str, String> {
        if reference.vault_id != OAUTH_VAULT_ID
            || !reference.reference_id.starts_with(prefix)
            || reference.reference_id.len() > 160
        {
            return Err(
                "OAuth secret reference is outside the expected keychain namespace".to_string(),
            );
        }
        Ok(&reference.reference_id)
    }

    fn envelope(tokens: OAuthTokenSet) -> TokenEnvelope {
        TokenEnvelope {
            contract_version: 1,
            access_token: tokens.access_token().expose().to_string(),
            refresh_token: tokens
                .refresh_token()
                .map(|secret| secret.expose().to_string()),
            token_type: tokens.token_type,
            granted_scopes: tokens.granted_scopes,
            issued_unix_ms: tokens.issued_unix_ms,
            expires_unix_ms: tokens.expires_unix_ms,
        }
    }

    fn tokens(envelope: TokenEnvelope) -> Result<OAuthTokenSet, String> {
        if envelope.contract_version != 1 {
            return Err("unsupported OAuth keychain envelope version".to_string());
        }
        OAuthTokenSet::new(
            SecretMaterial::new(envelope.access_token).map_err(|e| e.to_string())?,
            envelope
                .refresh_token
                .map(SecretMaterial::new)
                .transpose()
                .map_err(|e| e.to_string())?,
            envelope.token_type,
            envelope.granted_scopes,
            envelope.issued_unix_ms,
            envelope.expires_unix_ms,
        )
        .map_err(|e| e.to_string())
    }

    fn delete(reference_id: &str) -> Result<(), String> {
        match Self::entry(reference_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!("delete OAuth keychain entry: {error}")),
        }
    }
}

impl OAuthSecretVault for KeychainOAuthVault {
    fn put_ephemeral(
        &self,
        _label: &str,
        secret: SecretMaterial,
    ) -> Result<SecretReference, String> {
        let reference = Self::reference("ephemeral");
        Self::entry(&reference.reference_id)?
            .set_password(secret.expose())
            .map_err(|e| format!("save OAuth PKCE verifier: {e}"))?;
        Ok(reference)
    }

    fn get_ephemeral(&self, reference: &SecretReference) -> Result<SecretMaterial, String> {
        let reference_id = Self::checked_reference(reference, "ephemeral-")?;
        let value = Self::entry(reference_id)?
            .get_password()
            .map_err(|e| format!("read OAuth PKCE verifier: {e}"))?;
        SecretMaterial::new(value).map_err(|e| e.to_string())
    }

    fn delete_ephemeral(&self, reference: &SecretReference) -> Result<(), String> {
        Self::delete(Self::checked_reference(reference, "ephemeral-")?)
    }

    fn put_tokens(
        &self,
        _server_id: &str,
        tokens: OAuthTokenSet,
    ) -> Result<SecretReference, String> {
        let reference = Self::reference("tokens");
        let encoded = serde_json::to_string(&Self::envelope(tokens)).map_err(|e| e.to_string())?;
        Self::entry(&reference.reference_id)?
            .set_password(&encoded)
            .map_err(|e| format!("save OAuth token set: {e}"))?;
        Ok(reference)
    }

    fn get_tokens(&self, reference: &SecretReference) -> Result<OAuthTokenSet, String> {
        let reference_id = Self::checked_reference(reference, "tokens-")?;
        let encoded = Self::entry(reference_id)?
            .get_password()
            .map_err(|e| format!("read OAuth token set: {e}"))?;
        let envelope =
            serde_json::from_str(&encoded).map_err(|e| format!("decode OAuth token set: {e}"))?;
        Self::tokens(envelope)
    }

    fn replace_tokens(
        &self,
        reference: &SecretReference,
        tokens: OAuthTokenSet,
    ) -> Result<(), String> {
        let reference_id = Self::checked_reference(reference, "tokens-")?;
        let encoded = serde_json::to_string(&Self::envelope(tokens)).map_err(|e| e.to_string())?;
        Self::entry(reference_id)?
            .set_password(&encoded)
            .map_err(|e| format!("replace OAuth token set: {e}"))
    }

    fn delete_tokens(&self, reference: &SecretReference) -> Result<(), String> {
        Self::delete(Self::checked_reference(reference, "tokens-")?)
    }
}

#[derive(Debug)]
pub struct FilesystemOAuthFlowStore {
    root: PathBuf,
    gate: Mutex<()>,
}

impl FilesystemOAuthFlowStore {
    pub fn new(root: impl AsRef<Path>) -> Result<Self, String> {
        let root = root.as_ref();
        ensure_private_directory(root)?;
        Ok(Self {
            root: fs::canonicalize(root).map_err(|e| e.to_string())?,
            gate: Mutex::new(()),
        })
    }

    fn validate_hash(hash: &str) -> Result<(), String> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("OAuth state hash must be 64 hexadecimal characters".to_string());
        }
        Ok(())
    }
}

impl OAuthFlowStore for FilesystemOAuthFlowStore {
    fn put(&self, state: PendingOAuthFlow) -> Result<(), String> {
        Self::validate_hash(&state.state_sha256)?;
        let _guard = lock(&self.gate, "OAuth flow store")?;
        let path = self.root.join(format!("{}.json", state.state_sha256));
        atomic_write_private(
            &path,
            &serde_json::to_vec(&state).map_err(|e| e.to_string())?,
            false,
        )
    }

    fn take_by_state_hash(&self, state_sha256: &str) -> Result<Option<PendingOAuthFlow>, String> {
        Self::validate_hash(state_sha256)?;
        let _guard = lock(&self.gate, "OAuth flow store")?;
        let path = self.root.join(format!("{state_sha256}.json"));
        let claimed = self
            .root
            .join(format!(".claimed-{}", Uuid::new_v4().simple()));
        match fs::rename(&path, &claimed) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("claim OAuth flow: {error}")),
        }
        sync_directory(&self.root)?;
        let result = fs::read(&claimed)
            .map_err(|e| format!("read claimed OAuth flow: {e}"))
            .and_then(|bytes| {
                serde_json::from_slice::<PendingOAuthFlow>(&bytes)
                    .map_err(|e| format!("decode claimed OAuth flow: {e}"))
            });
        let _ = fs::remove_file(&claimed);
        sync_directory(&self.root)?;
        let state = result?;
        if state.state_sha256 != state_sha256 {
            return Err("claimed OAuth flow failed its state-hash integrity check".to_string());
        }
        Ok(Some(state))
    }
}

#[derive(Debug, Deserialize)]
struct OAuthWireTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default = "default_bearer")]
    token_type: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default = "default_expires_in")]
    expires_in: u64,
}

fn default_bearer() -> String {
    "Bearer".to_string()
}

fn default_expires_in() -> u64 {
    3_600
}

async fn bounded_response_bytes(response: reqwest::Response) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("read OAuth response: {e}"))?;
        if bytes.len().saturating_add(chunk.len()) > MAX_HTTP_RESPONSE_BYTES {
            return Err("OAuth response exceeded 1 MiB".to_string());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn run_async_worker<T, F>(label: &str, future: F) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = Result<T, String>> + Send + 'static,
{
    let label = label.to_string();
    let thread_label = label.clone();
    std::thread::Builder::new()
        .name(format!("m4-{label}"))
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| format!("create {thread_label} runtime: {e}"))?;
            runtime.block_on(future)
        })
        .map_err(|e| format!("start {label} worker: {e}"))?
        .join()
        .map_err(|_| format!("{label} worker panicked"))?
}

#[derive(Clone)]
pub struct ReqwestOAuthTransport {
    client: reqwest::Client,
}

impl ReqwestOAuthTransport {
    pub fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(format!(
                "LittleMonkey/{} M4-OAuth",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| format!("build OAuth HTTP client: {e}"))?;
        Ok(Self { client })
    }

    fn parse_tokens(
        bytes: &[u8],
        fallback_scopes: BTreeSet<String>,
    ) -> Result<OAuthTokenSet, String> {
        let response: OAuthWireTokenResponse = serde_json::from_slice(bytes)
            .map_err(|e| format!("decode OAuth token response: {e}"))?;
        let scopes = response.scope.map_or(fallback_scopes, |scope| {
            scope
                .split_whitespace()
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        });
        let issued = system_now_unix_ms();
        let expires = issued.saturating_add(response.expires_in.clamp(1, 31_536_000) * 1_000);
        OAuthTokenSet::new(
            SecretMaterial::new(response.access_token).map_err(|e| e.to_string())?,
            response
                .refresh_token
                .map(SecretMaterial::new)
                .transpose()
                .map_err(|e| e.to_string())?,
            response.token_type,
            scopes,
            issued,
            expires,
        )
        .map_err(|e| e.to_string())
    }
}

impl OAuthTransport for ReqwestOAuthTransport {
    fn exchange_code(&self, request: OAuthCodeExchangeRequest) -> Result<OAuthTokenSet, String> {
        let client = self.client.clone();
        let scopes = request.requested_scopes.clone();
        let form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("client_id", request.client_id),
            ("code", request.code.expose().to_string()),
            ("redirect_uri", request.redirect_uri),
            ("code_verifier", request.pkce_verifier.expose().to_string()),
        ];
        run_async_worker("oauth-code-exchange", async move {
            let response = client
                .post(request.token_endpoint)
                .form(&form)
                .send()
                .await
                .map_err(|e| format!("OAuth code exchange failed: {e}"))?;
            let status = response.status();
            let bytes = bounded_response_bytes(response).await?;
            if !status.is_success() {
                return Err(format!(
                    "OAuth code exchange returned {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            Self::parse_tokens(&bytes, scopes)
        })
    }

    fn refresh(&self, request: OAuthRefreshRequest) -> Result<OAuthTokenSet, String> {
        let client = self.client.clone();
        let scopes = request.scopes.clone();
        let form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("client_id", request.client_id),
            ("refresh_token", request.refresh_token.expose().to_string()),
            (
                "scope",
                scopes.iter().cloned().collect::<Vec<_>>().join(" "),
            ),
        ];
        run_async_worker("oauth-refresh", async move {
            let response = client
                .post(request.token_endpoint)
                .form(&form)
                .send()
                .await
                .map_err(|e| format!("OAuth refresh failed: {e}"))?;
            let status = response.status();
            let bytes = bounded_response_bytes(response).await?;
            if !status.is_success() {
                return Err(format!(
                    "OAuth refresh returned {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            Self::parse_tokens(&bytes, scopes)
        })
    }

    fn revoke(&self, endpoint: &str, client_id: &str, token: SecretMaterial) -> Result<(), String> {
        let client = self.client.clone();
        let endpoint = endpoint.to_string();
        let form = vec![
            ("client_id", client_id.to_string()),
            ("token", token.expose().to_string()),
        ];
        run_async_worker("oauth-revoke", async move {
            let response = client
                .post(endpoint)
                .form(&form)
                .send()
                .await
                .map_err(|e| format!("OAuth revocation failed: {e}"))?;
            let status = response.status();
            let bytes = bounded_response_bytes(response).await?;
            if !status.is_success() {
                return Err(format!(
                    "OAuth revocation returned {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            Ok(())
        })
    }
}

// -------------------------------------------------------------------------
// Opaque MCP App sessions and explicit, single-use approval brokers
// -------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct ProductionMcpUiSessionIssuer;

impl McpUiSessionIssuer for ProductionMcpUiSessionIssuer {
    fn issue(&self) -> Result<(String, BridgeCapability), String> {
        let session_id = format!("mcp-ui-{}", Uuid::new_v4().simple());
        let capability =
            BridgeCapability::new(random_urlsafe(48)?).map_err(|error| error.to_string())?;
        Ok((session_id, capability))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApprovalDecision {
    Pending,
    Approved,
    Denied,
}

#[derive(Debug, Clone)]
struct UiApprovalRecord {
    challenge: UiActionApprovalChallenge,
    decision: ApprovalDecision,
    expires_unix_ms: u64,
}

#[derive(Debug, Default)]
pub struct InMemoryUiActionApprovalBroker {
    records: Mutex<HashMap<String, UiApprovalRecord>>,
}

impl InMemoryUiActionApprovalBroker {
    fn purge_expired(records: &mut HashMap<String, UiApprovalRecord>, now: u64) {
        records.retain(|_, record| record.expires_unix_ms > now);
    }
}

impl UiActionApprovalBroker for InMemoryUiActionApprovalBroker {
    fn prepare(&self, action: &PreparedBridgeAction) -> Result<UiActionApprovalChallenge, String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "MCP UI approval broker")?;
        Self::purge_expired(&mut records, now);
        records.retain(|_, record| {
            !(record.decision == ApprovalDecision::Denied
                && record.challenge.session_id == action.session_id
                && record.challenge.action_id == action.action.action_id
                && record.challenge.payload_summary_sha256 == action.payload_summary_sha256)
        });
        if records.values().any(|record| {
            record.challenge.session_id == action.session_id
                && record.challenge.action_id == action.action.action_id
                && record.challenge.payload_summary_sha256 == action.payload_summary_sha256
        }) {
            return Err("an equivalent MCP UI action approval is already pending".to_string());
        }
        let challenge = UiActionApprovalChallenge {
            challenge_id: format!("ui-challenge-{}", Uuid::new_v4().simple()),
            session_id: action.session_id.clone(),
            action_id: action.action.action_id.clone(),
            action_target: action.action.target.clone(),
            required_permission: action.action.required_permission.clone(),
            payload_summary_sha256: action.payload_summary_sha256.clone(),
        };
        records.insert(
            challenge.challenge_id.clone(),
            UiApprovalRecord {
                challenge: challenge.clone(),
                decision: ApprovalDecision::Pending,
                expires_unix_ms: now.saturating_add(UI_APPROVAL_TTL_MS),
            },
        );
        Ok(challenge)
    }

    fn decide(&self, challenge_id: &str, approved: bool) -> Result<(), String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "MCP UI approval broker")?;
        Self::purge_expired(&mut records, now);
        let decision = records
            .get(challenge_id)
            .ok_or_else(|| "MCP UI approval challenge is unknown or expired".to_string())?;
        if decision.decision != ApprovalDecision::Pending {
            return Err("MCP UI approval challenge was already decided".to_string());
        }
        if approved {
            records
                .get_mut(challenge_id)
                .expect("challenge existence was checked")
                .decision = ApprovalDecision::Approved;
        } else {
            records
                .get_mut(challenge_id)
                .expect("challenge existence was checked")
                .decision = ApprovalDecision::Denied;
        }
        Ok(())
    }
}

impl UiActionApprovalGate for InMemoryUiActionApprovalBroker {
    fn approve(
        &self,
        session_id: &str,
        action: &DeclaredHostAction,
        payload_summary_sha256: &str,
    ) -> Result<Option<String>, String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "MCP UI approval broker")?;
        Self::purge_expired(&mut records, now);
        let matching = records
            .iter()
            .filter(|(_, record)| {
                record.challenge.session_id == session_id
                    && record.challenge.action_id == action.action_id
                    && record.challenge.action_target == action.target
                    && record.challenge.required_permission == action.required_permission
                    && record.challenge.payload_summary_sha256 == payload_summary_sha256
            })
            .map(|(id, record)| (id.clone(), record.decision))
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            return Err("ambiguous duplicate MCP UI approvals were rejected".to_string());
        }
        let Some((challenge_id, decision)) = matching.into_iter().next() else {
            return Ok(None);
        };
        match decision {
            ApprovalDecision::Approved => {
                records.remove(&challenge_id);
                Ok(Some(format!("ui-approval-{}", Uuid::new_v4().simple())))
            }
            ApprovalDecision::Denied => {
                records.remove(&challenge_id);
                Ok(None)
            }
            ApprovalDecision::Pending => Ok(None),
        }
    }
}

#[derive(Debug, Clone)]
struct WorkflowApprovalRecord {
    challenge: WorkflowHumanApprovalChallenge,
    decision: ApprovalDecision,
    expires_unix_ms: u64,
}

#[derive(Debug, Default)]
pub struct InMemoryWorkflowApprovalBroker {
    records: Mutex<HashMap<String, WorkflowApprovalRecord>>,
}

impl InMemoryWorkflowApprovalBroker {
    fn purge_expired(records: &mut HashMap<String, WorkflowApprovalRecord>, now: u64) {
        records.retain(|_, record| record.expires_unix_ms > now);
    }
}

impl WorkflowHumanApprovalBroker for InMemoryWorkflowApprovalBroker {
    fn prepare(
        &self,
        workflow_id: &str,
        run_id: &str,
        node_id: &str,
        approval_policy_id: &str,
        summary_sha256: &str,
    ) -> Result<WorkflowHumanApprovalChallenge, String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "workflow approval broker")?;
        Self::purge_expired(&mut records, now);
        records.retain(|_, record| {
            !(record.decision == ApprovalDecision::Denied
                && record.challenge.workflow_id == workflow_id
                && record.challenge.run_id == run_id
                && record.challenge.node_id == node_id
                && record.challenge.approval_policy_id == approval_policy_id
                && record.challenge.summary_sha256 == summary_sha256)
        });
        if records.values().any(|record| {
            record.challenge.workflow_id == workflow_id
                && record.challenge.run_id == run_id
                && record.challenge.node_id == node_id
                && record.challenge.approval_policy_id == approval_policy_id
                && record.challenge.summary_sha256 == summary_sha256
        }) {
            return Err("an equivalent workflow approval is already pending".to_string());
        }
        let challenge = WorkflowHumanApprovalChallenge {
            challenge_id: format!("workflow-challenge-{}", Uuid::new_v4().simple()),
            workflow_id: workflow_id.to_string(),
            run_id: run_id.to_string(),
            node_id: node_id.to_string(),
            approval_policy_id: approval_policy_id.to_string(),
            summary_sha256: summary_sha256.to_string(),
        };
        records.insert(
            challenge.challenge_id.clone(),
            WorkflowApprovalRecord {
                challenge: challenge.clone(),
                decision: ApprovalDecision::Pending,
                expires_unix_ms: now.saturating_add(WORKFLOW_APPROVAL_TTL_MS),
            },
        );
        Ok(challenge)
    }

    fn decide(&self, challenge_id: &str, approved: bool) -> Result<(), String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "workflow approval broker")?;
        Self::purge_expired(&mut records, now);
        let decision = records
            .get(challenge_id)
            .ok_or_else(|| "workflow approval challenge is unknown or expired".to_string())?;
        if decision.decision != ApprovalDecision::Pending {
            return Err("workflow approval challenge was already decided".to_string());
        }
        if approved {
            records
                .get_mut(challenge_id)
                .expect("challenge existence was checked")
                .decision = ApprovalDecision::Approved;
        } else {
            records
                .get_mut(challenge_id)
                .expect("challenge existence was checked")
                .decision = ApprovalDecision::Denied;
        }
        Ok(())
    }

    fn consume(
        &self,
        workflow_id: &str,
        run_id: &str,
        node_id: &str,
        approval_policy_id: &str,
        summary_sha256: &str,
    ) -> Result<Option<bool>, String> {
        let now = system_now_unix_ms();
        let mut records = lock(&self.records, "workflow approval broker")?;
        Self::purge_expired(&mut records, now);
        let matching = records
            .iter()
            .filter(|(_, record)| {
                record.challenge.workflow_id == workflow_id
                    && record.challenge.run_id == run_id
                    && record.challenge.node_id == node_id
                    && record.challenge.approval_policy_id == approval_policy_id
                    && record.challenge.summary_sha256 == summary_sha256
            })
            .map(|(id, record)| (id.clone(), record.decision))
            .collect::<Vec<_>>();
        if matching.len() > 1 {
            return Err("ambiguous duplicate workflow approvals were rejected".to_string());
        }
        let Some((id, decision)) = matching.into_iter().next() else {
            return Ok(None);
        };
        if decision == ApprovalDecision::Pending {
            return Ok(None);
        }
        records.remove(&id);
        Ok(Some(decision == ApprovalDecision::Approved))
    }
}

// -------------------------------------------------------------------------
// Production workflow execution adapters
// -------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct SystemWorkflowClock;

impl WorkflowClock for SystemWorkflowClock {
    fn now_unix_ms(&self) -> u64 {
        system_now_unix_ms()
    }

    fn sleep_ms(&self, duration_ms: u64, cancel: &CancellationToken) -> Result<(), String> {
        let mut remaining = duration_ms;
        while remaining > 0 {
            if cancel.is_cancelled() {
                return Err("workflow sleep cancelled".to_string());
            }
            let slice = remaining.min(50);
            std::thread::sleep(Duration::from_millis(slice));
            remaining -= slice;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct ProductionWorkflowNodeExecutor {
    workspace_root: PathBuf,
    artifact_root: PathBuf,
    http: reqwest::Client,
    approvals: Arc<dyn WorkflowHumanApprovalBroker>,
    browser: Arc<BrowserWorkflowAdapter>,
    mcp_state: Arc<crate::AppState>,
    mcp_config_path: PathBuf,
    mcp_gate: Arc<Mutex<()>>,
}

impl ProductionWorkflowNodeExecutor {
    pub fn new(
        app_data_dir: &Path,
        approvals: Arc<dyn WorkflowHumanApprovalBroker>,
    ) -> Result<Self, String> {
        Self::new_with_browser(
            app_data_dir,
            approvals,
            Arc::new(BrowserWorkflowAdapter::production(app_data_dir)?),
        )
    }

    fn new_with_browser(
        app_data_dir: &Path,
        approvals: Arc<dyn WorkflowHumanApprovalBroker>,
        browser: Arc<BrowserWorkflowAdapter>,
    ) -> Result<Self, String> {
        let workspace_root = app_data_dir.join("m4/workflow-files-v1");
        let artifact_root = app_data_dir.join("m4/workflow-artifacts-v1");
        ensure_private_directory(&workspace_root)?;
        ensure_private_directory(&artifact_root)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .user_agent(format!(
                "LittleMonkey/{} M4-Workflow",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|e| format!("build workflow HTTP client: {e}"))?;
        Ok(Self {
            workspace_root: fs::canonicalize(workspace_root).map_err(|e| e.to_string())?,
            artifact_root: fs::canonicalize(artifact_root).map_err(|e| e.to_string())?,
            http,
            approvals,
            browser,
            mcp_state: Arc::new(crate::AppState::default()),
            mcp_config_path: app_data_dir.join("mcp_servers.json"),
            mcp_gate: Arc::new(Mutex::new(())),
        })
    }

    fn success(output: WorkflowValue, usage: ResourceUsage) -> NodeAdapterResult {
        NodeAdapterResult::Succeeded {
            outputs: BTreeMap::from([("out".to_string(), output)]),
            usage,
        }
    }

    fn failure(
        class: FailureClass,
        message: impl Into<String>,
        retryable: bool,
    ) -> NodeAdapterResult {
        NodeAdapterResult::Failed {
            class,
            message: message.into(),
            retryable,
            usage: ResourceUsage::default(),
        }
    }

    fn require_string<'a>(
        inputs: &'a BTreeMap<String, WorkflowValue>,
        name: &str,
    ) -> Result<&'a str, String> {
        match inputs.get(name) {
            Some(WorkflowValue::String(value)) => Ok(value),
            _ => Err(format!("workflow node input {name} is not a string")),
        }
    }

    fn require_json<'a>(
        inputs: &'a BTreeMap<String, WorkflowValue>,
        name: &str,
    ) -> Result<&'a Value, String> {
        match inputs.get(name) {
            Some(WorkflowValue::Json(value)) => Ok(value),
            _ => Err(format!("workflow node input {name} is not JSON")),
        }
    }

    fn scoped_path(&self, value: &str, must_exist: bool) -> Result<PathBuf, String> {
        let relative = Path::new(value);
        if value.is_empty()
            || value.len() > 1_024
            || relative.is_absolute()
            || relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err("workflow file path escapes its private workspace".to_string());
        }
        let candidate = self.workspace_root.join(relative);
        if must_exist {
            let canonical = fs::canonicalize(&candidate)
                .map_err(|e| format!("open workflow file {}: {e}", candidate.display()))?;
            if !canonical.starts_with(&self.workspace_root) {
                return Err("workflow file symlink escapes its private workspace".to_string());
            }
            Ok(canonical)
        } else {
            let parent = candidate
                .parent()
                .ok_or_else(|| "workflow file path has no parent".to_string())?;
            ensure_private_directory(parent)?;
            let canonical_parent = fs::canonicalize(parent).map_err(|e| e.to_string())?;
            if !canonical_parent.starts_with(&self.workspace_root) {
                return Err("workflow file parent escapes its private workspace".to_string());
            }
            Ok(canonical_parent.join(
                candidate
                    .file_name()
                    .ok_or_else(|| "workflow file path has no name".to_string())?,
            ))
        }
    }

    fn execute_tool(
        &self,
        tool_id: &str,
        inputs: &BTreeMap<String, WorkflowValue>,
    ) -> Result<NodeAdapterResult, String> {
        let arguments = Self::require_json(inputs, "arguments")?;
        match tool_id {
            "builtin.read_file" => {
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "builtin.read_file requires string path".to_string())?;
                let path = self.scoped_path(path, true)?;
                let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
                if metadata.len() > 4 * 1024 * 1024 {
                    return Err("workflow file exceeds the 4 MiB read limit".to_string());
                }
                let content = fs::read_to_string(&path)
                    .map_err(|e| format!("read workflow file {}: {e}", path.display()))?;
                Ok(Self::success(
                    WorkflowValue::Json(json!({"path": path.file_name(), "content": content})),
                    ResourceUsage::default(),
                ))
            }
            "builtin.write_file" => {
                let path = arguments
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "builtin.write_file requires string path".to_string())?;
                let content = arguments
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "builtin.write_file requires string content".to_string())?;
                if content.len() > 4 * 1024 * 1024 {
                    return Err("workflow file exceeds the 4 MiB write limit".to_string());
                }
                let path = self.scoped_path(path, false)?;
                atomic_write_private(&path, content.as_bytes(), true)?;
                Ok(Self::success(
                    WorkflowValue::Json(json!({
                        "path": path.file_name(),
                        "bytes": content.len(),
                        "sha256": sha256_hex(content.as_bytes())
                    })),
                    ResourceUsage::default(),
                ))
            }
            _ => Err(format!("unavailable production workflow tool: {tool_id}")),
        }
    }

    fn run_model(
        &self,
        endpoint: String,
        model: String,
        prompt: String,
        bearer: Option<String>,
    ) -> Result<(String, ResourceUsage), String> {
        let client = self.http.clone();
        run_async_worker("workflow-model", async move {
            let ollama = endpoint.ends_with(":11434") || endpoint.contains(":11434/");
            let url = if ollama {
                format!("{}/api/chat", endpoint.trim_end_matches('/'))
            } else {
                format!("{}/chat/completions", endpoint.trim_end_matches('/'))
            };
            let body = if ollama {
                json!({"model": model, "stream": false, "messages": [{"role": "user", "content": prompt}]})
            } else {
                json!({"model": model, "stream": false, "messages": [{"role": "user", "content": prompt}]})
            };
            let mut request = client.post(url).json(&body);
            if let Some(token) = bearer {
                request = request.bearer_auth(token);
            }
            let response = request
                .send()
                .await
                .map_err(|e| format!("model request failed: {e}"))?;
            let status = response.status();
            let bytes = bounded_response_bytes(response).await?;
            if !status.is_success() {
                return Err(format!(
                    "model request returned {status}: {}",
                    String::from_utf8_lossy(&bytes)
                ));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| format!("decode model response: {e}"))?;
            let content = if ollama {
                value.pointer("/message/content").and_then(Value::as_str)
            } else {
                value
                    .pointer("/choices/0/message/content")
                    .and_then(Value::as_str)
            }
            .ok_or_else(|| "model response omitted assistant content".to_string())?
            .to_string();
            let usage = ResourceUsage {
                model_calls: 1,
                input_tokens: value
                    .get(if ollama { "prompt_eval_count" } else { "usage" })
                    .and_then(|usage| {
                        if ollama {
                            usage.as_u64()
                        } else {
                            usage.get("prompt_tokens").and_then(Value::as_u64)
                        }
                    })
                    .unwrap_or((prompt.len() as u64).div_ceil(4)),
                output_tokens: value
                    .get(if ollama { "eval_count" } else { "usage" })
                    .and_then(|usage| {
                        if ollama {
                            usage.as_u64()
                        } else {
                            usage.get("completion_tokens").and_then(Value::as_u64)
                        }
                    })
                    .unwrap_or((content.len() as u64).div_ceil(4)),
                ..ResourceUsage::default()
            };
            Ok((content, usage))
        })
    }

    fn default_ollama_model(&self) -> Result<String, String> {
        if let Ok(model) = std::env::var("LITTLE_MONKEY_WORKFLOW_MODEL") {
            if !model.trim().is_empty() {
                return Ok(model);
            }
        }
        let client = self.http.clone();
        run_async_worker("workflow-model-discovery", async move {
            let response = client
                .get(format!("{}/api/tags", crate::ollama::OLLAMA_BASE_URL))
                .send()
                .await
                .map_err(|error| format!("discover Ollama models: {error}"))?;
            let status = response.status();
            let bytes = bounded_response_bytes(response).await?;
            if !status.is_success() {
                return Err(format!("Ollama model discovery returned {status}"));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("decode Ollama model inventory: {error}"))?;
            value
                .pointer("/models/0/name")
                .and_then(Value::as_str)
                .filter(|model| !model.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    "No Ollama model is installed; pull one or set LITTLE_MONKEY_WORKFLOW_MODEL"
                        .to_string()
                })
        })
    }

    fn execute_agent(
        &self,
        profile: &str,
        prompt: &str,
        subagent: bool,
    ) -> Result<NodeAdapterResult, String> {
        let model = self.default_ollama_model()?;
        let role = if subagent {
            "bounded subagent"
        } else {
            "workflow agent"
        };
        let profile_instruction = match profile {
            "default" => "Complete the requested task and return a concise, verifiable result.",
            "explore" => "Investigate the request read-only. Cite concrete evidence and do not propose unverified facts.",
            "review" => "Review the supplied material for correctness, risk, and actionable improvements.",
            other => return Err(format!("unavailable {role} profile: {other}")),
        };
        let prompt = format!(
            "You are a Little Monkey {role} using the {profile} profile.\n{profile_instruction}\n\nTask:\n{prompt}"
        );
        match self.run_model(
            crate::ollama::OLLAMA_BASE_URL.to_string(),
            model,
            prompt,
            None,
        ) {
            Ok((content, usage)) => Ok(Self::success(WorkflowValue::String(content), usage)),
            Err(error) => Ok(Self::failure(FailureClass::Transient, error, true)),
        }
    }

    fn execute_mcp(
        &self,
        server_id: &str,
        tool_name: &str,
        effect: EffectClass,
        request: &NodeExecutionRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeAdapterResult, String> {
        let arguments = Self::require_json(&request.inputs, "arguments")?.clone();
        let entry = mcp::load_config_impl(&self.mcp_config_path)?
            .servers
            .into_iter()
            .find(|entry| entry.enabled && entry.id == server_id)
            .ok_or_else(|| format!("MCP server is not enabled: {server_id}"))?;
        let state = self.mcp_state.clone();
        let cancel = cancel.clone();
        let tool = tool_name.to_string();
        let timeout = Duration::from_secs(entry.timeout_secs.unwrap_or(60).clamp(1, 600));
        let _gate = lock(&self.mcp_gate, "workflow MCP adapter")?;
        let outcome = run_async_worker("workflow-mcp", async move {
            tokio::time::timeout(
                Duration::from_secs(mcp::CONNECT_TIMEOUT_SECS),
                mcp::connect_impl(&state, &entry),
            )
            .await
            .map_err(|_| format!("MCP server '{}' connection timed out", entry.id))??;
            let timeout_tool = tool.clone();
            let call = mcp::call_tool_with_cancel_impl(
                &state,
                &entry,
                &tool,
                arguments,
                async move {
                    tokio::select! {
                        _ = cancel.cancelled() => "workflow MCP call cancelled".to_string(),
                        _ = tokio::time::sleep(timeout) => format!("MCP tool {timeout_tool} timed out"),
                    }
                },
            )
            .await;
            mcp::disconnect_all(&state).await;
            call.and_then(|result| serde_json::to_value(result).map_err(|error| error.to_string()))
        });
        match outcome {
            Ok(value) => Ok(Self::success(
                WorkflowValue::Json(value),
                ResourceUsage::default(),
            )),
            Err(error) if effect == EffectClass::ExternalMutation => {
                let receipt = sha256_hex(
                    format!(
                        "{}\0{}\0{}\0{}\0{}",
                        request.workflow_id,
                        request.run_id,
                        request.node.node_id,
                        request.attempt,
                        error
                    )
                    .as_bytes(),
                );
                Ok(NodeAdapterResult::AmbiguousExternalEffect {
                    receipt: format!("mcp-{receipt}"),
                    pending_outputs: BTreeMap::new(),
                    usage: ResourceUsage::default(),
                })
            }
            Err(error) => Ok(Self::failure(FailureClass::Transient, error, true)),
        }
    }

    fn execute_browser(
        &self,
        action: &str,
        effect: EffectClass,
        request: &NodeExecutionRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeAdapterResult, String> {
        if cancel.is_cancelled() {
            let _ = self.browser.shutdown_run(&request.run_id);
            return Ok(Self::failure(
                FailureClass::Permanent,
                "workflow browser action cancelled",
                false,
            ));
        }
        let arguments = Self::require_json(&request.inputs, "arguments")?.clone();
        match self.browser.execute(&request.run_id, action, arguments) {
            Ok(_) if cancel.is_cancelled() => {
                let _ = self.browser.shutdown_run(&request.run_id);
                Ok(Self::failure(
                    FailureClass::Permanent,
                    "workflow browser action cancelled",
                    false,
                ))
            }
            Ok(value) => Ok(Self::success(
                WorkflowValue::Json(value),
                ResourceUsage::default(),
            )),
            Err(error) if effect == EffectClass::ExternalMutation => {
                let _ = self.browser.shutdown_run(&request.run_id);
                let receipt = sha256_hex(
                    format!(
                        "{}\0{}\0{}\0{}\0{}",
                        request.workflow_id,
                        request.run_id,
                        request.node.node_id,
                        request.attempt,
                        error
                    )
                    .as_bytes(),
                );
                Ok(NodeAdapterResult::AmbiguousExternalEffect {
                    receipt: format!("browser-{receipt}"),
                    pending_outputs: BTreeMap::new(),
                    usage: ResourceUsage::default(),
                })
            }
            Err(error) => {
                let _ = self.browser.shutdown_run(&request.run_id);
                Ok(Self::failure(FailureClass::Permanent, error, false))
            }
        }
    }

    fn json_string<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
        arguments
            .get(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("workflow action requires string {name}"))
    }

    fn json_u32(arguments: &Value, name: &str) -> Result<u32, String> {
        arguments
            .get(name)
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| format!("workflow action requires positive integer {name}"))
    }

    fn delivery_mutation(arguments: &Value) -> Result<DeliveryMutation, String> {
        serde_json::from_value(
            arguments
                .get("mutation")
                .cloned()
                .ok_or_else(|| "delivery action requires mutation".to_string())?,
        )
        .map_err(|error| format!("invalid delivery mutation: {error}"))
    }

    fn is_git_mutation(mutation: &DeliveryMutation) -> bool {
        matches!(
            mutation,
            DeliveryMutation::SetLock { .. }
                | DeliveryMutation::Stage { .. }
                | DeliveryMutation::Commit { .. }
                | DeliveryMutation::Push { .. }
                | DeliveryMutation::ArchiveWorktree { .. }
                | DeliveryMutation::CleanupWorktree { .. }
        )
    }

    fn is_pull_request_mutation(mutation: &DeliveryMutation) -> bool {
        matches!(
            mutation,
            DeliveryMutation::CreateDraftPr { .. }
                | DeliveryMutation::UpdateDraftPr { .. }
                | DeliveryMutation::PublishReview { .. }
                | DeliveryMutation::QueuePatchTask { .. }
        )
    }

    fn prepare_delivery_mutation(
        &self,
        arguments: &Value,
        git: bool,
    ) -> Result<NodeAdapterResult, String> {
        let mutation = Self::delivery_mutation(arguments)?;
        let valid = if git {
            Self::is_git_mutation(&mutation)
        } else {
            Self::is_pull_request_mutation(&mutation)
        };
        if !valid {
            return Err("delivery mutation does not belong to this workflow node".to_string());
        }
        let preview = crate::m5_delivery::prepare_mutation_impl(mutation.clone(), &self.mcp_state)?;
        let mut value = serde_json::to_value(preview).map_err(|error| error.to_string())?;
        value
            .as_object_mut()
            .ok_or_else(|| "delivery preview did not serialize as an object".to_string())?
            .insert(
                "mutation".to_string(),
                serde_json::to_value(mutation).map_err(|error| error.to_string())?,
            );
        Ok(Self::success(
            WorkflowValue::Json(value),
            ResourceUsage::default(),
        ))
    }

    fn execute_delivery_mutation(
        &self,
        arguments: &Value,
        git: bool,
        expected_external: bool,
    ) -> Result<NodeAdapterResult, String> {
        let mutation = Self::delivery_mutation(arguments)?;
        let valid_family = if git {
            Self::is_git_mutation(&mutation)
        } else {
            Self::is_pull_request_mutation(&mutation)
        };
        let actual_external = matches!(
            mutation,
            DeliveryMutation::Push { .. }
                | DeliveryMutation::CreateDraftPr { .. }
                | DeliveryMutation::UpdateDraftPr { .. }
                | DeliveryMutation::PublishReview { .. }
        );
        if !valid_family || actual_external != expected_external {
            return Err(
                "delivery mutation family/effect does not match the workflow action".to_string(),
            );
        }
        let digest = Self::json_string(arguments, "digest")?.to_string();
        let confirmation = arguments
            .get("confirmation")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "delivery action requires user-supplied confirmation".to_string())?
            .to_string();
        let state = self.mcp_state.clone();
        let call_digest = digest.clone();
        let outcome = run_async_worker("workflow-delivery", async move {
            crate::m5_delivery::execute_mutation_impl(mutation, call_digest, confirmation, &state)
                .await
        });
        match outcome {
            Ok(value) => Ok(Self::success(
                WorkflowValue::Json(value),
                ResourceUsage::default(),
            )),
            Err(error)
                if expected_external
                    && (error.contains("requires reconciliation")
                        || error.contains("outcome is ambiguous")
                        || error.contains("may have changed state")) =>
            {
                Ok(NodeAdapterResult::AmbiguousExternalEffect {
                    receipt: format!("m5-delivery-{digest}"),
                    pending_outputs: BTreeMap::new(),
                    usage: ResourceUsage::default(),
                })
            }
            Err(error) => Ok(Self::failure(FailureClass::Permanent, error, false)),
        }
    }

    fn execute_git(
        &self,
        action: &str,
        inputs: &BTreeMap<String, WorkflowValue>,
    ) -> Result<NodeAdapterResult, String> {
        let arguments = Self::require_json(inputs, "arguments")?;
        let value = match action {
            "list_worktrees" => {
                serde_json::to_value(crate::m5_delivery::m5_delivery_list_worktrees()?)
                    .map_err(|error| error.to_string())?
            }
            "inspect_worktree" => {
                serde_json::to_value(crate::m5_delivery::m5_delivery_inspect_worktree(
                    Self::json_string(arguments, "worktreeId")?.to_string(),
                )?)
                .map_err(|error| error.to_string())?
            }
            "prepare_mutation" => return self.prepare_delivery_mutation(arguments, true),
            "execute_local_mutation" => {
                return self.execute_delivery_mutation(arguments, true, false)
            }
            "execute_push" => return self.execute_delivery_mutation(arguments, true, true),
            _ => return Err(format!("unsupported Git workflow action: {action}")),
        };
        Ok(Self::success(
            WorkflowValue::Json(value),
            ResourceUsage::default(),
        ))
    }

    fn execute_pull_request(
        &self,
        action: &str,
        inputs: &BTreeMap<String, WorkflowValue>,
    ) -> Result<NodeAdapterResult, String> {
        let arguments = Self::require_json(inputs, "arguments")?;
        let worktree_id = || Self::json_string(arguments, "worktreeId").map(str::to_string);
        let number = || Self::json_u32(arguments, "number");
        let value = match action {
            "auth_status" => serde_json::to_value(crate::m5_delivery::m5_github_auth_status()?)
                .map_err(|error| error.to_string())?,
            "read_issue" => crate::m5_delivery::m5_github_issue(worktree_id()?, number()?)?,
            "read_pull_request" => {
                crate::m5_delivery::m5_github_pull_request(worktree_id()?, number()?)?
            }
            "read_review_threads" => {
                crate::m5_delivery::m5_github_review_threads(worktree_id()?, number()?)?
            }
            "read_checks" => crate::m5_delivery::m5_github_checks(worktree_id()?, number()?)?,
            "review_pull_request" => {
                let request = ReviewPullRequestRequest {
                    worktree_id: worktree_id()?,
                    pr_number: number()?,
                    model: Self::json_string(arguments, "model")?.to_string(),
                };
                return run_async_worker("workflow-pr-review", async move {
                    crate::m5_delivery::m5_review_pull_request(request)
                        .await
                        .and_then(|report| {
                            serde_json::to_value(report).map_err(|error| error.to_string())
                        })
                })
                .map(|value| Self::success(WorkflowValue::Json(value), ResourceUsage::default()));
            }
            "review_reports" => serde_json::to_value(crate::m5_delivery::m5_review_reports(
                worktree_id()?,
                number()?,
            )?)
            .map_err(|error| error.to_string())?,
            "prepare_mutation" => return self.prepare_delivery_mutation(arguments, false),
            "execute_external_mutation" => {
                return self.execute_delivery_mutation(arguments, false, true)
            }
            "execute_patch_task" => return self.execute_delivery_mutation(arguments, false, false),
            _ => {
                return Err(format!(
                    "unsupported pull-request workflow action: {action}"
                ))
            }
        };
        Ok(Self::success(
            WorkflowValue::Json(value),
            ResourceUsage::default(),
        ))
    }

    fn execute_legacy(
        &self,
        recipe: &LegacyRecipeV1,
        inputs: &BTreeMap<String, WorkflowValue>,
    ) -> Result<NodeAdapterResult, String> {
        let mut prompt = recipe.prompt.clone();
        for (name, value) in inputs {
            let WorkflowValue::String(value) = value else {
                return Err(format!("legacy recipe parameter {name} is not a string"));
            };
            prompt = prompt.replace(&format!("{{{{{name}}}}}"), value);
        }
        if let Some(system) = &recipe.system {
            prompt = format!("{system}\n\n{prompt}");
        }
        let (endpoint, model, bearer) = if let Some(model) = &recipe.target.ollama {
            (
                crate::ollama::OLLAMA_BASE_URL.to_string(),
                model.clone(),
                None,
            )
        } else if let Some(base) = &recipe.target.local_url {
            let parsed =
                url::Url::parse(base).map_err(|e| format!("invalid local recipe URL: {e}"))?;
            let loopback = matches!(parsed.host(), Some(url::Host::Ipv4(address)) if address.is_loopback())
                || parsed.host_str() == Some("localhost");
            if parsed.scheme() != "http" || !loopback || parsed.port().is_none() {
                return Err(
                    "local recipe URL must be a fixed-port HTTP loopback endpoint".to_string(),
                );
            }
            (
                base.clone(),
                recipe
                    .target
                    .model
                    .clone()
                    .unwrap_or_else(|| "local".to_string()),
                None,
            )
        } else if let (Some(provider), Some(model)) =
            (&recipe.target.provider, &recipe.target.model)
        {
            let base = crate::providers::resolve_base_url(provider, &[])?;
            let key = crate::providers::read_key(provider)?;
            (base, model.clone(), Some(key))
        } else {
            return Err("legacy recipe target is incomplete".to_string());
        };
        let (content, usage) = self.run_model(endpoint, model, prompt, bearer)?;
        Ok(Self::success(WorkflowValue::String(content), usage))
    }

    fn execute_shell(
        &self,
        profile: &str,
        command: &str,
        request: &NodeExecutionRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeAdapterResult, String> {
        let stdout_path = self
            .workspace_root
            .join(format!(".stdout-{}", Uuid::new_v4().simple()));
        let stderr_path = self
            .workspace_root
            .join(format!(".stderr-{}", Uuid::new_v4().simple()));
        let stdout = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stdout_path)
            .map_err(|e| e.to_string())?;
        let stderr = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&stderr_path)
            .map_err(|e| e.to_string())?;
        protect_file(&stdout_path)?;
        protect_file(&stderr_path)?;
        #[cfg(unix)]
        let mut child = {
            if profile != "posix-sh" {
                return Err(format!(
                    "unsupported shell profile on this platform: {profile}"
                ));
            }
            Command::new("/bin/sh")
                .args(["-c", command])
                .current_dir(&self.workspace_root)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|e| format!("start workflow shell: {e}"))?
        };
        #[cfg(windows)]
        let mut child = {
            if profile != "powershell" {
                return Err(format!(
                    "unsupported shell profile on this platform: {profile}"
                ));
            }
            Command::new("powershell.exe")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    command,
                ])
                .current_dir(&self.workspace_root)
                .stdin(Stdio::null())
                .stdout(Stdio::from(stdout))
                .stderr(Stdio::from(stderr))
                .spawn()
                .map_err(|e| format!("start workflow shell: {e}"))?
        };
        loop {
            if cancel.is_cancelled() || system_now_unix_ms() > request.deadline_unix_ms {
                let _ = child.kill();
                let _ = child.wait();
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                return Ok(Self::failure(
                    FailureClass::Timeout,
                    "workflow shell was cancelled or timed out",
                    false,
                ));
            }
            if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
                let read_bounded = |path: &Path| -> Result<String, String> {
                    let file = File::open(path).map_err(|e| e.to_string())?;
                    let mut bytes = Vec::new();
                    file.take(512 * 1024)
                        .read_to_end(&mut bytes)
                        .map_err(|e| e.to_string())?;
                    Ok(String::from_utf8_lossy(&bytes).to_string())
                };
                let stdout = read_bounded(&stdout_path)?;
                let stderr = read_bounded(&stderr_path)?;
                let _ = fs::remove_file(&stdout_path);
                let _ = fs::remove_file(&stderr_path);
                if !status.success() {
                    return Ok(Self::failure(
                        FailureClass::Permanent,
                        format!("shell exited with {status}: {stderr}"),
                        false,
                    ));
                }
                return Ok(Self::success(
                    WorkflowValue::Json(
                        json!({"status": status.code(), "stdout": stdout, "stderr": stderr}),
                    ),
                    ResourceUsage::default(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl WorkflowNodeExecutor for ProductionWorkflowNodeExecutor {
    fn execute(
        &self,
        request: NodeExecutionRequest,
        cancel: &CancellationToken,
    ) -> Result<NodeAdapterResult, String> {
        if cancel.is_cancelled() {
            return Ok(Self::failure(
                FailureClass::Permanent,
                "workflow node cancelled",
                false,
            ));
        }
        match &request.node.kind {
            WorkflowNodeKind::PromptModel { model_selector } => {
                let (backend, model) = model_selector
                    .split_once(':')
                    .ok_or_else(|| "model selector must be backend:model".to_string())?;
                if backend != "ollama" || model.is_empty() {
                    return Ok(Self::failure(
                        FailureClass::Validation,
                        format!("unsupported model backend: {backend}"),
                        false,
                    ));
                }
                let prompt = Self::require_string(&request.inputs, "prompt")?.to_string();
                match self.run_model(
                    crate::ollama::OLLAMA_BASE_URL.to_string(),
                    model.to_string(),
                    prompt,
                    None,
                ) {
                    Ok((content, usage)) => {
                        Ok(Self::success(WorkflowValue::String(content), usage))
                    }
                    Err(error) => Ok(Self::failure(FailureClass::Transient, error, true)),
                }
            }
            WorkflowNodeKind::Agent { agent_profile, .. } => {
                let prompt = Self::require_string(&request.inputs, "prompt")?;
                self.execute_agent(agent_profile, prompt, false)
            }
            WorkflowNodeKind::Subagent { agent_profile, .. } => {
                let prompt = Self::require_string(&request.inputs, "prompt")?;
                self.execute_agent(agent_profile, prompt, true)
            }
            WorkflowNodeKind::Tool { tool_id, .. } => self.execute_tool(tool_id, &request.inputs),
            WorkflowNodeKind::Mcp {
                server_id,
                tool_name,
                effect,
            } => self.execute_mcp(server_id, tool_name, *effect, &request, cancel),
            WorkflowNodeKind::Browser { action, effect } => {
                self.execute_browser(action, *effect, &request, cancel)
            }
            WorkflowNodeKind::Git { action, .. } => self.execute_git(action, &request.inputs),
            WorkflowNodeKind::PullRequest { action, .. } => {
                self.execute_pull_request(action, &request.inputs)
            }
            WorkflowNodeKind::Shell { shell_profile } => {
                let command = Self::require_string(&request.inputs, "command")?;
                self.execute_shell(shell_profile, command, &request, cancel)
            }
            WorkflowNodeKind::Verify { verifier_id } if verifier_id == "sha256" => {
                let input = Self::require_json(&request.inputs, "input")?.clone();
                let canonical = serde_json::to_vec(&input).map_err(|e| e.to_string())?;
                Ok(Self::success(
                    WorkflowValue::Json(
                        json!({"valid": true, "sha256": sha256_hex(&canonical), "value": input}),
                    ),
                    ResourceUsage::default(),
                ))
            }
            WorkflowNodeKind::Transform { transform_id } if transform_id == "identity" => {
                Ok(Self::success(
                    WorkflowValue::Json(Self::require_json(&request.inputs, "input")?.clone()),
                    ResourceUsage::default(),
                ))
            }
            WorkflowNodeKind::Condition => match request.inputs.get("condition") {
                Some(WorkflowValue::Boolean(value)) => Ok(Self::success(
                    WorkflowValue::Boolean(*value),
                    ResourceUsage::default(),
                )),
                _ => Err("condition input is not Boolean".to_string()),
            },
            WorkflowNodeKind::BoundedLoop { maximum_iterations } => Ok(Self::success(
                WorkflowValue::Json(Self::require_json(&request.inputs, "input")?.clone()),
                ResourceUsage {
                    loop_iterations: u64::from(*maximum_iterations),
                    ..ResourceUsage::default()
                },
            )),
            WorkflowNodeKind::HumanApproval { approval_policy_id } => {
                let summary = Self::require_string(&request.inputs, "summary")?;
                let decision = self.approvals.consume(
                    &request.workflow_id,
                    &request.run_id,
                    &request.node.node_id,
                    approval_policy_id,
                    &sha256_hex(summary.as_bytes()),
                )?;
                match decision {
                    Some(value) => Ok(Self::success(
                        WorkflowValue::Boolean(value),
                        ResourceUsage::default(),
                    )),
                    None => Ok(Self::failure(
                        FailureClass::Permission,
                        "explicit workflow approval is missing or pending",
                        false,
                    )),
                }
            }
            WorkflowNodeKind::Artifact { media_type } => {
                let content = Self::require_string(&request.inputs, "content")?;
                if content.len() > 16 * 1024 * 1024 {
                    return Ok(Self::failure(
                        FailureClass::Validation,
                        "artifact exceeds 16 MiB",
                        false,
                    ));
                }
                let digest = sha256_hex(content.as_bytes());
                let artifact_id = format!("artifact-{}", &digest[..24]);
                let path = self.artifact_root.join(format!("{artifact_id}.bin"));
                atomic_write_private(&path, content.as_bytes(), true)?;
                Ok(Self::success(
                    WorkflowValue::Artifact(ArtifactReference {
                        artifact_id,
                        sha256: digest,
                        media_type: media_type.clone(),
                    }),
                    ResourceUsage::default(),
                ))
            }
            WorkflowNodeKind::Output => Ok(Self::success(
                WorkflowValue::Json(Self::require_json(&request.inputs, "value")?.clone()),
                ResourceUsage::default(),
            )),
            WorkflowNodeKind::LegacyRecipe { recipe } => {
                self.execute_legacy(recipe, &request.inputs)
            }
            WorkflowNodeKind::Verify { verifier_id } => Ok(Self::failure(
                FailureClass::Validation,
                format!("unknown verifier: {verifier_id}"),
                false,
            )),
            WorkflowNodeKind::Transform { transform_id } => Ok(Self::failure(
                FailureClass::Validation,
                format!("unknown transform: {transform_id}"),
                false,
            )),
        }
    }

    fn finish_run(&self, run_id: &str) {
        let _ = self.browser.shutdown_run(run_id);
    }
}

// -------------------------------------------------------------------------
// Resident-daemon trigger contract and production service composition
// -------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTriggerRegistrationEnvelope {
    pub contract_version: u32,
    pub workflow_id: String,
    pub workflow_version: u32,
    pub definition_sha256: String,
    pub triggers: Vec<WorkflowTrigger>,
    pub updated_unix_ms: u64,
    pub enabled: bool,
}

pub fn workflow_trigger_registration_id(
    workflow_id: &str,
    workflow_version: u32,
    trigger: &WorkflowTrigger,
) -> Result<String, String> {
    let trigger_bytes = serde_json::to_vec(trigger).map_err(|e| e.to_string())?;
    Ok(format!(
        "m4w-{}-v{}-{}",
        &sha256_hex(workflow_id.as_bytes())[..16],
        workflow_version,
        &sha256_hex(&trigger_bytes)[..16]
    ))
}

#[derive(Debug)]
pub struct FilesystemWorkflowTriggerRegistrar {
    root: PathBuf,
    gate: Mutex<()>,
}

impl FilesystemWorkflowTriggerRegistrar {
    pub fn new(app_data_dir: &Path) -> Result<Self, String> {
        let root = app_data_dir.join("daemon/workflow-triggers-v1");
        ensure_private_directory(&root)?;
        Ok(Self {
            root: fs::canonicalize(root).map_err(|e| e.to_string())?,
            gate: Mutex::new(()),
        })
    }

    fn path(&self, workflow_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", sha256_hex(workflow_id.as_bytes())))
    }

    fn read(
        &self,
        workflow_id: &str,
    ) -> Result<Option<WorkflowTriggerRegistrationEnvelope>, String> {
        let path = self.path(workflow_id);
        match fs::read(&path) {
            Ok(bytes) => {
                let envelope: WorkflowTriggerRegistrationEnvelope = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("decode {}: {e}", path.display()))?;
                if envelope.workflow_id != workflow_id
                    || envelope.contract_version != M4_TRIGGER_ADAPTER_CONTRACT_VERSION
                {
                    return Err("workflow trigger envelope identity/version mismatch".to_string());
                }
                Ok(Some(envelope))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("read {}: {error}", path.display())),
        }
    }
}

impl PersistentWorkflowTriggerRegistrar for FilesystemWorkflowTriggerRegistrar {
    fn replace_batch(&self, batch: &WorkflowTriggerBatch) -> Result<Vec<String>, String> {
        if batch.contract_version != M4_TRIGGER_ADAPTER_CONTRACT_VERSION
            || batch.workflow_version == 0
            || batch.definition_sha256.len() != 64
            || !batch
                .definition_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("workflow trigger batch has invalid contract/version/digest".to_string());
        }
        let _guard = lock(&self.gate, "workflow trigger registrar")?;
        if let Some(existing) = self.read(&batch.workflow_id)? {
            if batch.workflow_version < existing.workflow_version
                || (batch.workflow_version == existing.workflow_version
                    && batch.definition_sha256 != existing.definition_sha256)
            {
                return Err(
                    "workflow trigger batch would roll back or rewrite a released definition"
                        .to_string(),
                );
            }
        }
        let mut canonical = batch
            .triggers
            .iter()
            .map(|trigger| {
                serde_json::to_vec(trigger)
                    .map(|bytes| (bytes, trigger.clone()))
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        canonical.sort_by(|left, right| left.0.cmp(&right.0));
        canonical.dedup_by(|left, right| left.0 == right.0);
        let triggers = canonical
            .into_iter()
            .map(|(_, trigger)| trigger)
            .collect::<Vec<_>>();
        let mut ids = triggers
            .iter()
            .map(|trigger| {
                workflow_trigger_registration_id(
                    &batch.workflow_id,
                    batch.workflow_version,
                    trigger,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        ids.sort();
        let envelope = WorkflowTriggerRegistrationEnvelope {
            contract_version: M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
            workflow_id: batch.workflow_id.clone(),
            workflow_version: batch.workflow_version,
            definition_sha256: batch.definition_sha256.clone(),
            triggers,
            updated_unix_ms: system_now_unix_ms(),
            enabled: true,
        };
        atomic_write_private(
            &self.path(&batch.workflow_id),
            &serde_json::to_vec(&envelope).map_err(|e| e.to_string())?,
            true,
        )?;
        Ok(ids)
    }

    fn remove_workflow(&self, workflow_id: &str) -> Result<(), String> {
        let _guard = lock(&self.gate, "workflow trigger registrar")?;
        let Some(mut envelope) = self.read(workflow_id)? else {
            return Ok(());
        };
        envelope.enabled = false;
        envelope.updated_unix_ms = system_now_unix_ms();
        atomic_write_private(
            &self.path(workflow_id),
            &serde_json::to_vec(&envelope).map_err(|e| e.to_string())?,
            true,
        )
    }
}

pub struct ProductionM4Services {
    pub packages: Arc<PackageRegistryService>,
    pub mcp_apps: Arc<McpAppService>,
    pub workflows: Arc<WorkflowService>,
    pub workflow_browser: Arc<BrowserWorkflowAdapter>,
}

pub fn production_workflow_capabilities(
    app_data_dir: &Path,
) -> Result<WorkflowCapabilityCatalog, String> {
    let mut catalog = WorkflowCapabilityCatalog {
        model_backends: BTreeSet::from(["ollama".to_string()]),
        agents: BTreeMap::from([
            ("default".to_string(), EffectClass::ReadOnly),
            ("explore".to_string(), EffectClass::ReadOnly),
            ("review".to_string(), EffectClass::ReadOnly),
        ]),
        subagents: BTreeMap::from([
            ("default".to_string(), EffectClass::ReadOnly),
            ("explore".to_string(), EffectClass::ReadOnly),
            ("review".to_string(), EffectClass::ReadOnly),
        ]),
        tools: BTreeMap::from([
            ("builtin.read_file".to_string(), EffectClass::ReadOnly),
            ("builtin.write_file".to_string(), EffectClass::LocalMutation),
        ]),
        browser_actions: BTreeMap::from([
            ("start".to_string(), EffectClass::LocalMutation),
            ("list".to_string(), EffectClass::ReadOnly),
            ("navigate".to_string(), EffectClass::ReadOnly),
            ("inspect".to_string(), EffectClass::ReadOnly),
            ("click".to_string(), EffectClass::ExternalMutation),
            ("type_text".to_string(), EffectClass::ExternalMutation),
            ("scroll".to_string(), EffectClass::ReadOnly),
            ("screenshot".to_string(), EffectClass::ReadOnly),
            ("capture_evidence".to_string(), EffectClass::ReadOnly),
            ("stop".to_string(), EffectClass::LocalMutation),
        ]),
        git_actions: BTreeMap::from([
            ("list_worktrees".to_string(), EffectClass::ReadOnly),
            ("inspect_worktree".to_string(), EffectClass::ReadOnly),
            ("prepare_mutation".to_string(), EffectClass::ReadOnly),
            (
                "execute_local_mutation".to_string(),
                EffectClass::LocalMutation,
            ),
            ("execute_push".to_string(), EffectClass::ExternalMutation),
        ]),
        pull_request_actions: BTreeMap::from([
            ("auth_status".to_string(), EffectClass::ReadOnly),
            ("read_issue".to_string(), EffectClass::ReadOnly),
            ("read_pull_request".to_string(), EffectClass::ReadOnly),
            ("read_review_threads".to_string(), EffectClass::ReadOnly),
            ("read_checks".to_string(), EffectClass::ReadOnly),
            ("review_pull_request".to_string(), EffectClass::ReadOnly),
            ("review_reports".to_string(), EffectClass::ReadOnly),
            ("prepare_mutation".to_string(), EffectClass::ReadOnly),
            (
                "execute_external_mutation".to_string(),
                EffectClass::ExternalMutation,
            ),
            ("execute_patch_task".to_string(), EffectClass::LocalMutation),
        ]),
        transforms: BTreeSet::from(["identity".to_string()]),
        verifiers: BTreeSet::from(["sha256".to_string()]),
        ..WorkflowCapabilityCatalog::default()
    };
    for entry in mcp::load_config_impl(&app_data_dir.join("mcp_servers.json"))?
        .servers
        .into_iter()
        .filter(|entry| entry.enabled)
    {
        // MCP does not currently persist server-provided read/write
        // annotations. Only explicitly allowlisted tools are exposed and
        // they are conservatively treated as external mutations.
        for tool in entry.tool_allowlist.unwrap_or_default() {
            catalog.mcp_tools.insert(
                format!("{}:{tool}", entry.id),
                EffectClass::ExternalMutation,
            );
        }
    }
    #[cfg(unix)]
    catalog.shell_profiles.insert("posix-sh".to_string());
    #[cfg(windows)]
    catalog.shell_profiles.insert("powershell".to_string());
    Ok(catalog)
}

fn production_workflow_daemon_capabilities() -> BTreeSet<DaemonCapability> {
    BTreeSet::from([
        DaemonCapability::PersistentCron,
        DaemonCapability::FilesystemWatch,
        DaemonCapability::SignedWebhook,
        DaemonCapability::EventIngestion,
    ])
}

pub fn refresh_production_workflow_capabilities(
    service: &WorkflowService,
    app_data_dir: &Path,
) -> Result<(), String> {
    service
        .set_runtime_capabilities(
            production_workflow_daemon_capabilities(),
            production_workflow_capabilities(app_data_dir)?,
        )
        .map_err(|error| error.to_string())
}

fn production_workflow_service_with_browser(
    app_data_dir: &Path,
) -> Result<(Arc<WorkflowService>, Arc<BrowserWorkflowAdapter>), String> {
    ensure_private_directory(app_data_dir)?;
    let approvals: Arc<dyn WorkflowHumanApprovalBroker> =
        Arc::new(InMemoryWorkflowApprovalBroker::default());
    let browser = Arc::new(BrowserWorkflowAdapter::production(app_data_dir)?);
    let executor = Arc::new(ProductionWorkflowNodeExecutor::new_with_browser(
        app_data_dir,
        approvals.clone(),
        browser.clone(),
    )?);
    let registrar = Arc::new(FilesystemWorkflowTriggerRegistrar::new(app_data_dir)?);
    let service = WorkflowService::new_with_approval_broker(
        app_data_dir.join("m4/workflow-store-v1"),
        production_workflow_daemon_capabilities(),
        production_workflow_capabilities(app_data_dir)?,
        executor,
        Arc::new(SystemWorkflowClock),
        Some(registrar),
        approvals,
    )
    .map(Arc::new)
    .map_err(|e| e.to_string())?;
    Ok((service, browser))
}

pub fn production_workflow_service(app_data_dir: &Path) -> Result<Arc<WorkflowService>, String> {
    production_workflow_service_with_browser(app_data_dir).map(|(service, _)| service)
}

pub fn production_m4_services(app_data_dir: &Path) -> Result<ProductionM4Services, String> {
    ensure_private_directory(app_data_dir)?;
    let (trust_store, _, _) = signed_first_party_catalog().map_err(|e| e.to_string())?;
    let app_version =
        SemanticVersion::parse(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())?;
    let packages = Arc::new(
        PackageRegistryService::new(
            app_data_dir.join("m4/package-store-v1"),
            trust_store,
            InstallEnvironment {
                app_version,
                platform: std::env::consts::OS.to_string(),
                architecture: std::env::consts::ARCH.to_string(),
            },
            InstallTrustPolicy {
                // Local developer packages remain data-only, checksum-bound,
                // and require the same explicit permission preview as signed
                // catalog packages. Git/registry acquisition still requires
                // a trusted signature, so this does not create an executable
                // plugin or remote supply-chain bypass.
                allow_unsigned_local_folders: true,
                allow_unsigned_git: false,
                require_registry_catalog_match: true,
                permit_expired_offline_registry: true,
            },
            PackageLimits::default(),
            Arc::new(RingEd25519SignatureVerifier),
        )
        .map_err(|e| e.to_string())?,
    );
    packages
        .seed_first_party(system_now_unix_ms().max(FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS))
        .map_err(|e| e.to_string())?;

    let ui_approvals: Arc<dyn UiActionApprovalBroker> =
        Arc::new(InMemoryUiActionApprovalBroker::default());
    let mcp_apps = Arc::new(
        McpAppService::new_persistent(
            app_data_dir.join("m4/mcp-oauth-state-v1"),
            Arc::new(ProductionPkceSecurity),
            Arc::new(KeychainOAuthVault),
            Arc::new(FilesystemOAuthFlowStore::new(
                app_data_dir.join("m4/oauth-flows-v1"),
            )?),
            Arc::new(ReqwestOAuthTransport::new()?),
            Arc::new(ProductionMcpUiSessionIssuer),
            ui_approvals,
        )
        .map_err(|error| error.to_string())?,
    );
    let (workflows, workflow_browser) = production_workflow_service_with_browser(app_data_dir)?;
    Ok(ProductionM4Services {
        packages,
        mcp_apps,
        workflows,
        workflow_browser,
    })
}

/// Resident-daemon entry point. It reloads and recompiles the append-only
/// definition, verifies the exact digest and declared trigger, maps only the
/// one supported daemon payload shape, and persists an ordinary run history.
pub fn run_daemon_workflow_delivery(
    app_data_dir: &Path,
    workflow_id: &str,
    expected_definition_sha256: &str,
    run_id: &str,
    trigger: WorkflowTrigger,
    payload_json: Value,
) -> Result<WorkflowRunHistory, String> {
    let service = production_workflow_service(app_data_dir)?;
    let definition = service.load(workflow_id).map_err(|e| e.to_string())?;
    let ir = service.validate(&definition).map_err(|e| e.to_string())?;
    if ir.definition_sha256 != expected_definition_sha256 {
        return Err("daemon workflow definition digest no longer matches registration".to_string());
    }
    if !ir.triggers.contains(&trigger) {
        return Err("daemon delivery trigger is not declared by the workflow".to_string());
    }
    let inputs = if definition.inputs.is_empty() {
        // The daemon always owns an event body. A zero-input definition has
        // deliberately chosen not to bind it, so preserve an empty input
        // snapshot without guessing a coercion or rejecting the delivery.
        BTreeMap::new()
    } else if definition.inputs.len() == 1
        && definition.inputs.get("trigger_payload") == Some(&WorkflowValueType::Json)
    {
        BTreeMap::from([(
            "trigger_payload".to_string(),
            WorkflowValue::Json(payload_json),
        )])
    } else {
        return Err(
            "daemon workflow inputs must be empty or exactly trigger_payload: Json".to_string(),
        );
    };
    let secret_bindings = definition
        .secrets
        .keys()
        .map(|secret_id| {
            let digest = sha256_hex(format!("{workflow_id}\0{secret_id}").as_bytes());
            (
                secret_id.clone(),
                SecretBinding {
                    secret_id: secret_id.clone(),
                    vault_reference: format!("keychain:m4-workflow-{}", &digest[..32]),
                },
            )
        })
        .collect();
    match service.history(run_id) {
        Ok(existing) => {
            if existing.workflow_id == workflow_id
                && existing.definition_sha256 == expected_definition_sha256
                && existing.trigger == trigger
                && existing.input_snapshot == inputs
                && existing.secret_reference_snapshot == secret_bindings
            {
                return Ok(existing);
            }
            return Err(
                "daemon run id already exists for a different workflow delivery".to_string(),
            );
        }
        Err(M4ServiceError::NotFound(_)) => {}
        Err(error) => return Err(error.to_string()),
    }
    service
        .run_workflow(
            workflow_id,
            WorkflowRunRequest {
                run_id: run_id.to_string(),
                inputs,
                secret_bindings,
                trigger,
            },
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp_app_core::{HostActionKind, OAuthSecurityProvider};
    use crate::workflow_core::{workflow_core_fixtures, WorkflowRunStatus, WorkflowTrigger};

    struct TempDirectory(PathBuf);

    impl TempDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-m4-runtime-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).expect("create temp directory");
            Self(path)
        }
    }

    impl Drop for TempDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn private_atomic_write_never_overwrites_create_new_records() {
        let directory = TempDirectory::new("atomic-write");
        let path = directory.0.join("record.json");
        atomic_write_private(&path, b"first", false).expect("create record");
        assert!(atomic_write_private(&path, b"second", false).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"first");
        atomic_write_private(&path, b"replacement", true).expect("replace record");
        assert_eq!(fs::read(&path).unwrap(), b"replacement");
    }

    #[test]
    fn production_workflow_catalog_advertises_only_effect_classed_adapters() {
        let directory = TempDirectory::new("workflow-capabilities");
        let config = crate::mcp::McpConfigFile {
            version: 1,
            servers: vec![crate::mcp::McpServerEntry {
                id: "fixture".to_string(),
                label: "Fixture".to_string(),
                transport: crate::mcp::McpTransport::Stdio {
                    command: "fixture-mcp".to_string(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                },
                enabled: true,
                tool_allowlist: Some(vec!["lookup".to_string()]),
                timeout_secs: Some(30),
            }],
        };
        crate::mcp::save_config_impl(&directory.0.join("mcp_servers.json"), &config).unwrap();
        let catalog = production_workflow_capabilities(&directory.0).unwrap();
        assert_eq!(
            catalog.mcp_tools.get("fixture:lookup"),
            Some(&EffectClass::ExternalMutation)
        );
        assert_eq!(
            catalog.browser_actions.get("click"),
            Some(&EffectClass::ExternalMutation)
        );
        assert_eq!(
            catalog.git_actions.get("execute_push"),
            Some(&EffectClass::ExternalMutation)
        );
        assert_eq!(
            catalog
                .pull_request_actions
                .get("execute_external_mutation"),
            Some(&EffectClass::ExternalMutation)
        );
    }

    #[test]
    fn workflow_delivery_adapter_keeps_preview_and_confirmation_separate() {
        let directory = TempDirectory::new("workflow-delivery-confirmation");
        let executor = ProductionWorkflowNodeExecutor::new(
            &directory.0,
            Arc::new(InMemoryWorkflowApprovalBroker::default()),
        )
        .unwrap();
        let stage = DeliveryMutation::Stage {
            worktree_id: "owned-fixture".to_string(),
            paths: vec!["src/lib.rs".to_string()],
        };
        assert!(ProductionWorkflowNodeExecutor::is_git_mutation(&stage));
        assert!(!ProductionWorkflowNodeExecutor::is_pull_request_mutation(
            &stage
        ));
        let error = executor
            .execute_delivery_mutation(
                &json!({
                    "mutation": stage,
                    "digest": "a".repeat(64),
                    "confirmationPhrase": "CONFIRM aaaaaaaaaaaa"
                }),
                true,
                false,
            )
            .unwrap_err();
        assert!(error.contains("user-supplied confirmation"));
    }

    #[test]
    fn pkce_and_filesystem_flow_store_are_replay_resistant() {
        let pkce = ProductionPkceSecurity.generate_pkce().expect("PKCE");
        assert!(pkce.state.len() >= 32);
        assert!(pkce.verifier.expose().len() >= 43);
        assert_eq!(pkce.challenge_s256.len(), 43);

        let directory = TempDirectory::new("oauth-flow");
        let store = FilesystemOAuthFlowStore::new(&directory.0).expect("flow store");
        let state_sha256 = sha256_hex(pkce.state.as_bytes());
        let pending = PendingOAuthFlow {
            flow_id: "flow-1".to_string(),
            server_id: "server-1".to_string(),
            state_sha256: state_sha256.clone(),
            verifier_reference: SecretReference {
                vault_id: OAUTH_VAULT_ID.to_string(),
                reference_id: "ephemeral-fixture".to_string(),
            },
            redirect_uri: "littlemonkey://oauth/callback".to_string(),
            requested_scopes: BTreeSet::from(["read".to_string()]),
            created_unix_ms: 1,
            expires_unix_ms: 61_000,
        };
        store.put(pending.clone()).expect("put flow");
        assert_eq!(
            store.take_by_state_hash(&state_sha256).expect("take flow"),
            Some(pending)
        );
        assert_eq!(
            store
                .take_by_state_hash(&state_sha256)
                .expect("replay lookup"),
            None
        );
    }

    #[test]
    fn ui_approval_is_explicit_bound_and_single_use() {
        let broker = InMemoryUiActionApprovalBroker::default();
        let action = DeclaredHostAction {
            action_id: "search".to_string(),
            kind: HostActionKind::InvokeTool,
            target: "mcp__fixture__search".to_string(),
            required_permission: "read".to_string(),
            always_requires_approval: true,
        };
        let prepared = PreparedBridgeAction {
            session_id: "session-1".to_string(),
            action: action.clone(),
            payload: json!({"query": "rust"}),
            payload_summary_sha256: sha256_hex(br#"{"query":"rust"}"#),
        };
        let challenge = broker.prepare(&prepared).expect("prepare");
        assert_eq!(
            UiActionApprovalGate::approve(
                &broker,
                &prepared.session_id,
                &action,
                &prepared.payload_summary_sha256,
            )
            .expect("pending gate"),
            None
        );
        // Pending authorization does not consume the pending challenge.
        broker
            .decide(&challenge.challenge_id, true)
            .expect("approve challenge");
        assert!(UiActionApprovalGate::approve(
            &broker,
            &prepared.session_id,
            &action,
            &prepared.payload_summary_sha256,
        )
        .expect("approved gate")
        .is_some());
        assert_eq!(
            UiActionApprovalGate::approve(
                &broker,
                &prepared.session_id,
                &action,
                &prepared.payload_summary_sha256,
            )
            .expect("single use"),
            None
        );
        let denied = broker.prepare(&prepared).expect("prepare denial");
        broker
            .decide(&denied.challenge_id, false)
            .expect("deny challenge");
        assert!(
            broker.prepare(&prepared).is_ok(),
            "a denial must be retryable"
        );
    }

    #[test]
    fn workflow_denial_can_be_reviewed_again_without_ambiguous_records() {
        let broker = InMemoryWorkflowApprovalBroker::default();
        let first = broker
            .prepare(
                "workflow-1",
                "run-1",
                "approval-1",
                "explicit-user",
                &sha256_hex(b"Allow mutation?"),
            )
            .expect("prepare workflow approval");
        broker
            .decide(&first.challenge_id, false)
            .expect("deny workflow approval");
        assert!(broker
            .prepare(
                "workflow-1",
                "run-1",
                "approval-1",
                "explicit-user",
                &sha256_hex(b"Allow mutation?"),
            )
            .is_ok());
    }

    #[test]
    fn registrar_is_atomic_deterministic_and_rejects_rollback() {
        let directory = TempDirectory::new("registrar");
        let registrar = FilesystemWorkflowTriggerRegistrar::new(&directory.0).expect("registrar");
        let trigger = WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".to_string(),
        };
        let batch = WorkflowTriggerBatch {
            contract_version: M4_TRIGGER_ADAPTER_CONTRACT_VERSION,
            workflow_id: "workflow-1".to_string(),
            workflow_version: 2,
            definition_sha256: "a".repeat(64),
            triggers: vec![trigger.clone(), trigger.clone()],
        };
        let ids = registrar.replace_batch(&batch).expect("replace");
        assert_eq!(ids.len(), 1);
        assert_eq!(
            ids[0],
            workflow_trigger_registration_id("workflow-1", 2, &trigger).unwrap()
        );
        let envelope = registrar.read("workflow-1").unwrap().unwrap();
        assert!(envelope.enabled);
        assert_eq!(envelope.triggers.len(), 1);
        let rollback = WorkflowTriggerBatch {
            workflow_version: 1,
            ..batch.clone()
        };
        assert!(registrar.replace_batch(&rollback).is_err());
        registrar.remove_workflow("workflow-1").expect("disable");
        assert!(!registrar.read("workflow-1").unwrap().unwrap().enabled);
    }

    #[test]
    fn daemon_delivery_runs_persists_and_is_idempotent() {
        let directory = TempDirectory::new("delivery");
        let service = production_workflow_service(&directory.0).expect("workflow service");
        let mut definition = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .expect("fixture")
            .workflow;
        let trigger = WorkflowTrigger::PersistentCron {
            expression: "*/5 * * * *".to_string(),
        };
        definition.triggers = vec![trigger.clone()];
        let ir = service.create(definition.clone()).expect("create workflow");
        let ids = service
            .register_persistent_triggers(&definition.workflow_id)
            .expect("register trigger");
        assert_eq!(ids.len(), 1);
        drop(service);

        let first = run_daemon_workflow_delivery(
            &directory.0,
            &definition.workflow_id,
            &ir.definition_sha256,
            "daemon-run-1",
            trigger.clone(),
            json!({"event": "ignored by zero-input definition"}),
        )
        .expect("first delivery");
        assert_eq!(first.status, WorkflowRunStatus::Succeeded);
        let repeated = run_daemon_workflow_delivery(
            &directory.0,
            &definition.workflow_id,
            &ir.definition_sha256,
            "daemon-run-1",
            trigger,
            json!({"event": "different body remains unbound"}),
        )
        .expect("idempotent reconnect");
        assert_eq!(repeated, first);
    }
}

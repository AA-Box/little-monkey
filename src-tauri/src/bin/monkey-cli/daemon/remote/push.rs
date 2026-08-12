//! Waking a paired device up.
//!
//! **Provider-neutral by construction.** Everything above [`PushBackend`] knows
//! only that a device has an address and that something happened; which service
//! carries it is one implementation of one trait.
//!
//! **Two backends, and the default is the one the bundled client can use.**
//! The mobile controller this runner serves is a browser. It has no Firebase
//! SDK and its own content security policy forbids loading one, so it could
//! never hold an FCM token — a push path it cannot register for would be
//! decoration. [`WebPushBackend`] is therefore the default: the browser's own
//! `PushManager` gives the address, this runner mints its own VAPID identity,
//! and the notification is sealed to the subscriber with RFC 8291 before the
//! push service ever sees it. [`FcmBackend`] remains for a native client that
//! does hold a registration token.
//!
//! **The configuration is the end user's.** Little Monkey ships no Firebase
//! project, no service account, no VAPID key and no relay. Web Push needs no
//! account anywhere: the keypair is generated on this machine and the only
//! third party involved is the push service the user's own browser already
//! chose. FCM, if an operator picks it, uses *their* project. There is nowhere
//! in this file for a maintainer-owned credential to live, which is the point.
//!
//! **A push grants nothing.** It is a notification, never authority: the device
//! still has to make an ordinary signed request to learn anything or do
//! anything, and this module never sends message text unless the operator has
//! explicitly turned that on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::protocol::{first_pem_block, REMOTE_PROTOCOL_VERSION};
use crate::daemon::store::{restrict_file, DaemonPaths};

/// What happened. Deliberately a closed set: a notification's whole payload is
/// derived from one of these, so there is no path by which arbitrary text
/// reaches a lock screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushKind {
    ApprovalRequested,
    RunCompleted,
    RunFailed,
    NewResponse,
    /// A device was revoked, or a key rotated. Always delivered with detail,
    /// because a security event the user cannot identify is not actionable.
    SecurityAlert,
    /// A queued device command needs someone to be holding the phone — a screen
    /// capture's consent prompt, or a camera that must be pointed somewhere.
    DeviceActionAwaiting,
}

impl PushKind {
    /// The generic line. No run text, no message body, no participant names:
    /// a lock screen is the least private surface either end of this system
    /// has, and the device can read the detail through a signed request the
    /// moment someone unlocks it.
    fn title(self) -> &'static str {
        match self {
            Self::ApprovalRequested => "Approval needed",
            Self::RunCompleted => "Run finished",
            Self::RunFailed => "Run failed",
            Self::NewResponse => "New response",
            Self::SecurityAlert => "Security alert",
            Self::DeviceActionAwaiting => "Little Monkey needs this device",
        }
    }

    fn body(self) -> &'static str {
        match self {
            Self::ApprovalRequested => "A run is waiting on your decision.",
            Self::RunCompleted => "Open Little Monkey to see the result.",
            Self::RunFailed => "Open Little Monkey to see what happened.",
            Self::NewResponse => "There is something new to read.",
            Self::SecurityAlert => "Open Little Monkey to review this.",
            Self::DeviceActionAwaiting => "An action is waiting for you to allow it.",
        }
    }

    /// Whether the operator's `include_detail` setting may add specifics.
    /// A security alert always carries them; nothing else ever does without
    /// the setting.
    fn always_detailed(self) -> bool {
        matches!(self, Self::SecurityAlert)
    }
}

/// One thing worth waking a device for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushNotification {
    pub kind: PushKind,
    /// The run, command or device this is about. Travels in the data payload
    /// (which the app reads after unlock), never in the visible text unless
    /// [`PushKind::always_detailed`].
    pub target_id: Option<String>,
    /// Specifics the operator has opted into showing.
    pub detail: Option<String>,
}

/// What a backend actually transmits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushPayload {
    pub title: String,
    pub body: String,
    pub data: BTreeMap<String, String>,
}

impl PushNotification {
    pub fn payload(&self, include_detail: bool) -> PushPayload {
        let mut data = BTreeMap::new();
        data.insert(
            "kind".to_string(),
            serde_json::to_value(self.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
        );
        if let Some(target_id) = &self.target_id {
            data.insert("target_id".to_string(), bounded(target_id, 256));
        }
        // The data payload names *what* happened and *which* thing it happened
        // to. It never carries content: an id is useless to anyone who
        // intercepts it and sufficient for the app to fetch the rest over the
        // signed channel.
        let show_detail = include_detail || self.kind.always_detailed();
        let body = match (&self.detail, show_detail) {
            (Some(detail), true) => bounded(detail, 256),
            _ => self.kind.body().to_string(),
        };
        PushPayload {
            title: self.kind.title().to_string(),
            body,
            data,
        }
    }
}

/// One way of delivering a notification.
///
/// Async because every real backend is a network call and this process is
/// already inside a Tokio runtime; a synchronous trait would have forced each
/// implementation to block a runtime thread.
#[async_trait::async_trait]
pub trait PushBackend: Send + Sync {
    fn name(&self) -> &'static str;
    /// Delivers to one registration token. Returns the provider's own message
    /// id, so a delivery is traceable in the provider's console without this
    /// process keeping a second copy of it.
    async fn send(&self, token: &str, payload: &PushPayload) -> Result<String, String>;
}

/// The operator's push configuration. Every field is theirs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PushConfig {
    pub protocol_version: u32,
    /// `web_push`, `fcm` or `none`. A closed set rather than a URL: a backend
    /// is code in this file, and "point push at an arbitrary host" is not a
    /// feature.
    ///
    /// `web_push` is the default and the one the bundled browser client uses;
    /// it needs no account anywhere. `fcm` exists for a native client that
    /// holds a Firebase registration token, and then the project is the
    /// operator's own.
    pub backend: String,
    /// The operator's own Firebase project. Empty under `web_push`.
    #[serde(default)]
    pub project_id: String,
    /// Where the service account JSON was copied to inside app-private state.
    /// Empty under `web_push`.
    #[serde(default)]
    pub service_account_path: String,
    /// The VAPID `sub` claim: how a push service would contact whoever is
    /// sending. A self-hosted runner has no support address, so this is its own
    /// advertised URL.
    #[serde(default = "default_vapid_subject")]
    pub vapid_subject: String,
    /// Whether notifications may carry specifics. Off by default: the visible
    /// text of a push is the least private thing this system produces.
    pub include_detail: bool,
    pub enabled: bool,
}

fn default_vapid_subject() -> String {
    "https://localhost".to_string()
}

impl PushConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err("Unsupported push configuration version".to_string());
        }
        if !matches!(self.backend.as_str(), "web_push" | "fcm" | "none") {
            return Err("Push backend must be 'web_push', 'fcm' or 'none'".to_string());
        }
        if self.backend == "web_push" {
            let subject = url::Url::parse(&self.vapid_subject)
                .map_err(|_| "The VAPID subject must be an https: or mailto: URL".to_string())?;
            if !matches!(subject.scheme(), "https" | "mailto") {
                return Err("The VAPID subject must be an https: or mailto: URL".to_string());
            }
        }
        if self.backend == "fcm" {
            if self.project_id.trim().is_empty() || self.project_id.len() > 128 {
                return Err("FCM requires your own Firebase project id".to_string());
            }
            if self.service_account_path.trim().is_empty() {
                return Err("FCM requires your own service account key".to_string());
            }
        }
        Ok(())
    }
}

pub fn config_path(paths: &DaemonPaths) -> PathBuf {
    paths.root.join("remote-push.json")
}

pub fn load_config(paths: &DaemonPaths) -> Result<Option<PushConfig>, String> {
    match std::fs::read(config_path(paths)) {
        Ok(bytes) => {
            let config: PushConfig = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Push configuration is invalid: {error}"))?;
            config.validate()?;
            Ok(Some(config))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read push configuration: {error}")),
    }
}

/// Copies the operator's service account into app-private state and records the
/// configuration.
///
/// The key is copied rather than referenced in place for the same reason
/// `configure_host` copies the TLS key: a path in the user's Downloads folder
/// is not somewhere a daemon should be reading a credential from months later.
/// Mints this runner's own VAPID identity and enables Web Push.
///
/// No account, no project, no third-party console: the keypair is generated
/// here and the private half goes straight into the platform keychain, beside
/// every other device secret. Idempotent — an existing key is kept, because
/// replacing it would silently invalidate every subscription already made
/// against it.
pub fn configure_web_push(
    paths: &DaemonPaths,
    subject: &str,
    include_detail: bool,
    secrets: &dyn super::store::RemoteSecretStore,
) -> Result<PushConfig, String> {
    paths.ensure()?;
    if secrets.get(VAPID_SECRET_SLOT).is_err() {
        secrets.set(VAPID_SECRET_SLOT, &VapidIdentity::generate()?)?;
    }
    let config = PushConfig {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        backend: "web_push".to_string(),
        project_id: String::new(),
        service_account_path: String::new(),
        vapid_subject: subject.to_string(),
        include_detail,
        enabled: true,
    };
    save_config(paths, &config)?;
    Ok(config)
}

pub fn configure(
    paths: &DaemonPaths,
    project_id: &str,
    service_account_source: &Path,
    include_detail: bool,
) -> Result<PushConfig, String> {
    paths.ensure()?;
    let bytes = std::fs::read(service_account_source)
        .map_err(|error| format!("Could not read the service account key: {error}"))?;
    if bytes.len() > 64 * 1024 {
        return Err("The service account key is implausibly large".to_string());
    }
    // Parsed before it is stored, so a wrong file fails now rather than at the
    // first notification.
    let account = ServiceAccount::parse(&bytes)?;
    if account.project_id != project_id {
        return Err(format!(
            "The service account belongs to project '{}', not '{project_id}'",
            account.project_id
        ));
    }
    let destination = paths.root.join("remote-push-service-account.json");
    std::fs::write(&destination, &bytes)
        .map_err(|error| format!("Could not store the service account key: {error}"))?;
    restrict_file(&destination)?;
    let config = PushConfig {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        backend: "fcm".to_string(),
        project_id: project_id.to_string(),
        service_account_path: destination.to_string_lossy().to_string(),
        vapid_subject: default_vapid_subject(),
        include_detail,
        enabled: true,
    };
    save_config(paths, &config)?;
    Ok(config)
}

pub fn save_config(paths: &DaemonPaths, config: &PushConfig) -> Result<(), String> {
    config.validate()?;
    paths.ensure()?;
    let path = config_path(paths);
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(config).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("Could not write push configuration: {error}"))?;
    restrict_file(&path)
}

/// The operator's service account, as FCM HTTP v1 needs it.
#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub client_email: String,
    pub private_key: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    "https://oauth2.googleapis.com/token".to_string()
}

impl ServiceAccount {
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let account: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("The service account key is not valid JSON: {error}"))?;
        if account.project_id.trim().is_empty()
            || account.client_email.trim().is_empty()
            || !account.private_key.contains("PRIVATE KEY")
        {
            return Err(
                "The service account key is missing its project, client email or private key"
                    .to_string(),
            );
        }
        if !account.token_uri.starts_with("https://") {
            return Err("The service account token endpoint must be HTTPS".to_string());
        }
        Ok(account)
    }

    /// The signed assertion exchanged for an access token.
    ///
    /// RS256 with the algorithm pinned by name. A verifier is not involved here
    /// — Google is — but the header is written rather than echoed for the same
    /// reason a verifier must pin: `alg` is not a field anything should take
    /// from elsewhere.
    pub fn assertion(&self, now_s: u64) -> Result<String, String> {
        let header = base64url(
            serde_json::json!({ "alg": "RS256", "typ": "JWT" })
                .to_string()
                .as_bytes(),
        );
        let claims = base64url(
            serde_json::json!({
                "iss": self.client_email,
                "scope": "https://www.googleapis.com/auth/firebase.messaging",
                "aud": self.token_uri,
                "iat": now_s,
                "exp": now_s + 3_600,
            })
            .to_string()
            .as_bytes(),
        );
        let signing_input = format!("{header}.{claims}");
        let der = first_pem_block(self.private_key.as_bytes(), "PRIVATE KEY")?;
        let key = ring::signature::RsaKeyPair::from_pkcs8(&der)
            .map_err(|error| format!("The service account private key is unusable: {error}"))?;
        let mut signature = vec![0u8; key.public().modulus_len()];
        key.sign(
            &ring::signature::RSA_PKCS1_SHA256,
            &ring::rand::SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| "The service account assertion could not be signed".to_string())?;
        Ok(format!("{signing_input}.{}", base64url(&signature)))
    }
}

/// The exact `messages:send` body for one token.
///
/// A free function so the request this process would put on the wire is
/// assertable without one.
pub fn fcm_message(token: &str, payload: &PushPayload) -> serde_json::Value {
    serde_json::json!({
        "message": {
            "token": token,
            "notification": { "title": payload.title, "body": payload.body },
            "data": payload.data,
            // High priority only wakes the app; it still has to authenticate to
            // learn anything. Android's own delivery hint, not an authority.
            "android": { "priority": "high" },
            "apns": { "headers": { "apns-priority": "10" } },
        }
    })
}

pub fn fcm_endpoint(project_id: &str) -> String {
    format!("https://fcm.googleapis.com/v1/projects/{project_id}/messages:send")
}

/// FCM HTTP v1, against the operator's own project.
pub struct FcmBackend {
    account: ServiceAccount,
    project_id: String,
}

impl FcmBackend {
    pub fn open(config: &PushConfig) -> Result<Self, String> {
        let bytes = std::fs::read(&config.service_account_path).map_err(|error| {
            format!("Could not read the configured service account key: {error}")
        })?;
        Ok(Self {
            account: ServiceAccount::parse(&bytes)?,
            project_id: config.project_id.clone(),
        })
    }

    async fn access_token(&self) -> Result<String, String> {
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "The system clock is before the epoch".to_string())?
            .as_secs();
        let assertion = self.account.assertion(now_s)?;
        // `egress::hardened` rather than a client built here: it supplies the
        // connect/read budgets and the redirect policy that will not carry this
        // credential to a host the response chose.
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Could not build the push client: {error}"))?;
        let response = async {
            client
                .post(&self.account.token_uri)
                .form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", assertion.as_str()),
                ])
                .send()
                .await?
                .text()
                .await
        }
        .await
        .map_err(|error| format!("The push token exchange failed: {error}"))?;
        serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| {
                value
                    .get("access_token")
                    .and_then(|token| token.as_str().map(str::to_string))
            })
            .ok_or_else(|| "The push token exchange returned no access token".to_string())
    }
}

#[async_trait::async_trait]
impl PushBackend for FcmBackend {
    fn name(&self) -> &'static str {
        "fcm"
    }

    async fn send(&self, token: &str, payload: &PushPayload) -> Result<String, String> {
        let access_token = self.access_token().await?;
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Could not build the push client: {error}"))?;
        let body = fcm_message(token, payload);
        let endpoint = fcm_endpoint(&self.project_id);
        let response = async {
            client
                .post(&endpoint)
                .bearer_auth(access_token)
                .json(&body)
                .send()
                .await?
                .text()
                .await
        }
        .await
        .map_err(|error| format!("The push delivery failed: {error}"))?;
        serde_json::from_str::<serde_json::Value>(&response)
            .ok()
            .and_then(|value| {
                value
                    .get("name")
                    .and_then(|name| name.as_str().map(str::to_string))
            })
            .ok_or_else(|| {
                format!(
                    "The push provider refused the message: {}",
                    bounded(&response, 512)
                )
            })
    }
}

/// Resolves the operator's configured backend, or `None` when this machine has
/// no push configured — which is the ordinary case, not an error.
pub fn backend(
    paths: &DaemonPaths,
    secrets: &dyn super::store::RemoteSecretStore,
) -> Result<Option<Box<dyn PushBackend>>, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(None);
    };
    if !config.enabled || config.backend == "none" {
        return Ok(None);
    }
    Ok(Some(match config.backend.as_str() {
        "web_push" => Box::new(WebPushBackend::open(&config, secrets)?) as Box<dyn PushBackend>,
        _ => Box::new(FcmBackend::open(&config)?),
    }))
}

/// The `applicationServerKey` a browser needs before it can subscribe, or
/// `None` when this runner does not do Web Push.
pub fn application_server_key(
    paths: &DaemonPaths,
    secrets: &dyn super::store::RemoteSecretStore,
) -> Result<Option<String>, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(None);
    };
    if !config.enabled || config.backend != "web_push" {
        return Ok(None);
    }
    Ok(Some(
        WebPushBackend::open(&config, secrets)?.application_server_key(),
    ))
}

/// Wakes one device, if it has registered and this machine has push
/// configured.
///
/// Every failure here is soft: a notification is a courtesy, and a run must
/// never fail because a phone could not be reached. Returns whether anything
/// was actually delivered, so a caller that wants to say so can.
pub async fn notify_device(
    paths: &DaemonPaths,
    device_id: &str,
    notification: &PushNotification,
    secrets: &dyn super::store::RemoteSecretStore,
) -> Result<bool, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(false);
    };
    let Some(backend) = backend(paths, secrets)? else {
        return Ok(false);
    };
    let registration =
        super::store::RemoteStore::open(&paths.root)?.push_registration(device_id)?;
    let Some((registered_backend, token)) = registration else {
        return Ok(false);
    };
    if registered_backend != backend.name() {
        return Err(format!(
            "This device registered for '{registered_backend}' and push is configured for '{}'",
            backend.name()
        ));
    }
    backend
        .send(&token, &notification.payload(config.include_detail))
        .await?;
    Ok(true)
}

/// Wakes every reachable device. Used for events that belong to the operator
/// rather than to one phone — an approval request, a security alert.
pub async fn notify_all(
    paths: &DaemonPaths,
    notification: &PushNotification,
    secrets: &dyn super::store::RemoteSecretStore,
) -> Result<usize, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(0);
    };
    let Some(backend) = backend(paths, secrets)? else {
        return Ok(0);
    };
    let registrations = super::store::RemoteStore::open(&paths.root)?.push_registrations()?;
    let payload = notification.payload(config.include_detail);
    let mut delivered = 0;
    for (_, registered_backend, token) in registrations {
        if registered_backend != backend.name() {
            continue;
        }
        // One unreachable device must not stop the others.
        if backend.send(&token, &payload).await.is_ok() {
            delivered += 1;
        }
    }
    Ok(delivered)
}

// --- Web Push (RFC 8030 / 8188 / 8291 / 8292) ------------------------------
//
// The backend the *bundled* client can actually use. The mobile controller this
// runner serves is a browser: it has no Firebase SDK and its own content
// security policy forbids loading one, so it can never hold an FCM token. What
// it does have is `PushManager.subscribe`, which yields an endpoint on whatever
// push service the browser vendor runs, plus the two keys needed to encrypt to
// it.
//
// That makes this the *more* self-service of the two backends, not the lesser
// one: there is no account to create anywhere. The operator's runner mints its
// own VAPID identity, and the only third party involved is the push service the
// user's own browser already chose.
//
// Implemented directly on `ring` rather than pulled in as a crate: every
// primitive RFC 8291 needs — ECDH on P-256, HKDF-SHA256, AES-128-GCM — is
// already in this binary's dependency graph, and the whole construction is the
// hundred lines below.

/// Where the VAPID private key lives in the runner's keychain.
pub const VAPID_SECRET_SLOT: &str = "vapid:web-push:1";

/// The record size this sender advertises. A single record is always used —
/// notifications are bounded to a few hundred bytes by `PushKind` — so this is
/// only ever the "everything fits" value.
const RECORD_SIZE: u32 = 4_096;

/// How long a push service should hold an undelivered notification. Four hours:
/// long enough for a phone that is asleep, short enough that nobody is woken by
/// an approval request that was answered yesterday.
const PUSH_TTL_SECONDS: u32 = 4 * 60 * 60;

/// One browser's subscription, exactly as `PushManager.subscribe` produced it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebPushSubscription {
    pub endpoint: String,
    /// The subscriber's uncompressed P-256 public key, base64url.
    pub p256dh: String,
    /// The subscriber's 16-byte authentication secret, base64url.
    pub auth: String,
}

impl WebPushSubscription {
    pub fn validate(&self) -> Result<(), String> {
        let endpoint = url::Url::parse(&self.endpoint)
            .map_err(|error| format!("Push endpoint is not a URL: {error}"))?;
        // HTTPS only, and no credentials: this URL is stored and later posted to
        // by a background process.
        if endpoint.scheme() != "https"
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err("Push endpoint must be a credential-free HTTPS URL".to_string());
        }
        if self.endpoint.len() > 2_048 {
            return Err("Push endpoint is too long".to_string());
        }
        if decode_base64url(&self.p256dh)?.len() != 65 {
            return Err("Push subscription key must be an uncompressed P-256 point".to_string());
        }
        if decode_base64url(&self.auth)?.len() != 16 {
            return Err("Push subscription auth secret must be 16 bytes".to_string());
        }
        Ok(())
    }
}

/// HKDF output length, as `ring`'s `KeyType` wants it.
#[derive(Debug, Clone, Copy)]
struct HkdfLength(usize);

impl ring::hkdf::KeyType for HkdfLength {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], length: usize) -> Result<Vec<u8>, String> {
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, salt).extract(ikm);
    // `info` is bound to a local first: `expand` borrows the slice-of-slices,
    // and a temporary built inline would be dropped before `fill` reads it.
    let info = [info];
    let okm = prk
        .expand(&info, HkdfLength(length))
        .map_err(|_| "HKDF expansion failed".to_string())?;
    let mut out = vec![0u8; length];
    okm.fill(&mut out)
        .map_err(|_| "HKDF output could not be read".to_string())?;
    Ok(out)
}

/// Encrypts one notification to one subscription, producing an `aes128gcm`
/// body (RFC 8188) keyed as RFC 8291 requires.
///
/// The ephemeral key is generated per message, which is what makes each
/// delivery independently sealed: the push service that carries this body — a
/// third party, by design — sees only ciphertext.
pub fn encrypt_web_push(
    subscription: &WebPushSubscription,
    plaintext: &[u8],
    salt: [u8; 16],
    ephemeral: ring::agreement::EphemeralPrivateKey,
) -> Result<Vec<u8>, String> {
    let ua_public = decode_base64url(&subscription.p256dh)?;
    let auth_secret = decode_base64url(&subscription.auth)?;
    let as_public = ephemeral
        .compute_public_key()
        .map_err(|_| "The ephemeral public key could not be computed".to_string())?
        .as_ref()
        .to_vec();

    let peer = ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, &ua_public);
    let shared = ring::agreement::agree_ephemeral(ephemeral, &peer, |secret| secret.to_vec())
        .map_err(|_| "The push key agreement failed".to_string())?;

    // RFC 8291 §3.4. The key info binds the derived secret to *both* public
    // keys, so a shared secret cannot be replayed against a different
    // subscriber.
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(&ua_public);
    key_info.extend_from_slice(&as_public);
    let ikm = hkdf(&auth_secret, &shared, &key_info, 32)?;

    // RFC 8188 §2.
    let content_encryption_key = hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0", 12)?;

    let key = ring::aead::LessSafeKey::new(
        ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &content_encryption_key)
            .map_err(|_| "The push content key is invalid".to_string())?,
    );
    // 0x02 is the last-record delimiter; there is only ever one record here.
    let mut sealed = plaintext.to_vec();
    sealed.push(0x02);
    key.seal_in_place_append_tag(
        ring::aead::Nonce::assume_unique_for_key(
            nonce
                .as_slice()
                .try_into()
                .map_err(|_| "The push nonce is the wrong length".to_string())?,
        ),
        ring::aead::Aad::empty(),
        &mut sealed,
    )
    .map_err(|_| "The push payload could not be encrypted".to_string())?;

    let mut body = Vec::with_capacity(21 + as_public.len() + sealed.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(&as_public);
    body.extend_from_slice(&sealed);
    Ok(body)
}

/// The runner's own VAPID identity (RFC 8292).
///
/// Minted here and kept in the platform keychain — never a maintainer's key,
/// and never shared between machines. The public half is what the browser is
/// given as `applicationServerKey`, which is how a push service knows a
/// notification for that subscription really came from this runner.
pub struct VapidIdentity {
    key_pair: ring::signature::EcdsaKeyPair,
    public_key: Vec<u8>,
    /// The `sub` claim. A push service uses it to contact whoever is sending;
    /// for a self-hosted runner there is no support address, so this is the
    /// runner's own advertised URL.
    subject: String,
}

impl VapidIdentity {
    /// Generates a new identity, returning the PKCS#8 bytes to store.
    pub fn generate() -> Result<Vec<u8>, String> {
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &rng,
        )
        .map_err(|_| "A VAPID key pair could not be generated".to_string())?;
        Ok(document.as_ref().to_vec())
    }

    pub fn from_pkcs8(pkcs8: &[u8], subject: &str) -> Result<Self, String> {
        let rng = ring::rand::SystemRandom::new();
        let key_pair = ring::signature::EcdsaKeyPair::from_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            pkcs8,
            &rng,
        )
        .map_err(|_| "The stored VAPID key is unusable".to_string())?;
        let public_key = ring::signature::KeyPair::public_key(&key_pair)
            .as_ref()
            .to_vec();
        Ok(Self {
            key_pair,
            public_key,
            subject: subject.to_string(),
        })
    }

    /// What the browser passes to `PushManager.subscribe`.
    pub fn application_server_key(&self) -> String {
        base64url(&self.public_key)
    }

    /// The `Authorization: vapid t=…, k=…` header value for one endpoint.
    ///
    /// The audience is the endpoint's *origin*, not the endpoint: a token
    /// scoped to the full URL would leak which subscription it was for to any
    /// push service that logs headers, and RFC 8292 asks for the origin anyway.
    pub fn authorization(&self, endpoint: &str, now_s: u64) -> Result<String, String> {
        let url = url::Url::parse(endpoint)
            .map_err(|error| format!("Push endpoint is not a URL: {error}"))?;
        let audience = url.origin().ascii_serialization();
        let header = base64url(
            serde_json::json!({ "typ": "JWT", "alg": "ES256" })
                .to_string()
                .as_bytes(),
        );
        let claims = base64url(
            serde_json::json!({
                "aud": audience,
                // Twelve hours. RFC 8292 caps this at 24; half of that leaves
                // room for a slow clock at either end without minting a token
                // that stays valid for a day.
                "exp": now_s + 12 * 60 * 60,
                "sub": self.subject,
            })
            .to_string()
            .as_bytes(),
        );
        let signing_input = format!("{header}.{claims}");
        let rng = ring::rand::SystemRandom::new();
        let signature = self
            .key_pair
            .sign(&rng, signing_input.as_bytes())
            .map_err(|_| "The VAPID assertion could not be signed".to_string())?;
        Ok(format!(
            "vapid t={signing_input}.{}, k={}",
            base64url(signature.as_ref()),
            self.application_server_key()
        ))
    }
}

/// Delivers to a browser's own push service.
pub struct WebPushBackend {
    identity: VapidIdentity,
}

impl WebPushBackend {
    pub fn open(
        config: &PushConfig,
        secrets: &dyn super::store::RemoteSecretStore,
    ) -> Result<Self, String> {
        let pkcs8 = secrets.get(VAPID_SECRET_SLOT).map_err(|error| {
            format!("This runner has no VAPID key yet ({error}); run `remote push-configure --web-push`")
        })?;
        Ok(Self {
            identity: VapidIdentity::from_pkcs8(&pkcs8, &config.vapid_subject)?,
        })
    }

    pub fn application_server_key(&self) -> String {
        self.identity.application_server_key()
    }

    /// The exact request this sender would put on the wire, minus the network.
    /// Split out so the encryption, the headers and the framing are all
    /// assertable without a push service.
    pub fn build_request(
        &self,
        subscription: &WebPushSubscription,
        payload: &PushPayload,
        now_s: u64,
    ) -> Result<(String, Vec<(String, String)>, Vec<u8>), String> {
        subscription.validate()?;
        let plaintext = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
        let mut salt = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut salt)
            .map_err(|_| "The operating system random generator failed".to_string())?;
        let ephemeral = ring::agreement::EphemeralPrivateKey::generate(
            &ring::agreement::ECDH_P256,
            &ring::rand::SystemRandom::new(),
        )
        .map_err(|_| "An ephemeral push key could not be generated".to_string())?;
        let body = encrypt_web_push(subscription, &plaintext, salt, ephemeral)?;
        let headers = vec![
            (
                "Authorization".to_string(),
                self.identity.authorization(&subscription.endpoint, now_s)?,
            ),
            ("Content-Encoding".to_string(), "aes128gcm".to_string()),
            (
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            ),
            ("TTL".to_string(), PUSH_TTL_SECONDS.to_string()),
            // An approval that nobody sees is the failure this whole path
            // exists to prevent, so these are worth waking a screen for.
            ("Urgency".to_string(), "high".to_string()),
        ];
        Ok((subscription.endpoint.clone(), headers, body))
    }
}

#[async_trait::async_trait]
impl PushBackend for WebPushBackend {
    fn name(&self) -> &'static str {
        "web_push"
    }

    async fn send(&self, token: &str, payload: &PushPayload) -> Result<String, String> {
        let subscription: WebPushSubscription = serde_json::from_str(token)
            .map_err(|error| format!("The stored push subscription is unreadable: {error}"))?;
        let now_s = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "The system clock is before the epoch".to_string())?
            .as_secs();
        let (endpoint, headers, body) = self.build_request(&subscription, payload, now_s)?;
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Could not build the push client: {error}"))?;
        let mut request = client.post(&endpoint).body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("The push delivery failed: {error}"))?;
        let status = response.status();
        if !status.is_success() {
            // 404/410 is the push service saying this subscription is dead. The
            // caller deletes it rather than retrying forever.
            return Err(format!(
                "The push service refused the message ({}){}",
                status,
                if status.as_u16() == 404 || status.as_u16() == 410 {
                    ": the subscription has expired"
                } else {
                    ""
                }
            ));
        }
        Ok(status.to_string())
    }
}

fn decode_base64url(value: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    // Browsers emit unpadded base64url; some emit padded. Accept both rather
    // than rejecting a perfectly good subscription over a trailing '='.
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.trim_end_matches('='))
        .map_err(|error| format!("Push subscription value is not base64url: {error}"))
}

fn base64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use base64::Engine;

    use super::*;

    #[derive(Default)]
    struct FakeBackend(Mutex<Vec<(String, PushPayload)>>);

    #[async_trait::async_trait]
    impl PushBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }
        async fn send(&self, token: &str, payload: &PushPayload) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .push((token.to_string(), payload.clone()));
            Ok("projects/test/messages/1".to_string())
        }
    }

    /// **The Web Push payload really is decryptable by a real subscriber.**
    ///
    /// Encryption that only ever runs forwards proves nothing: a wrong info
    /// string, a swapped salt, or a mis-ordered header would all still produce
    /// plausible-looking ciphertext, and the failure would surface as silence on
    /// someone's phone. So this test *is* the browser: it generates a
    /// subscriber key pair, hands over the public half exactly as
    /// `PushManager.subscribe` would, then performs the RFC 8291 receiver side
    /// — parse the header, agree, derive, decrypt — and asserts the plaintext
    /// comes back.
    #[test]
    fn a_web_push_body_decrypts_back_to_the_notification_for_a_real_subscriber() {
        let rng = ring::rand::SystemRandom::new();
        // The subscriber. `ring`'s ephemeral keys cannot be reused, so the
        // receiver side derives the shared secret from its own private key —
        // exactly as a browser does.
        let ua_private =
            ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
                .unwrap();
        let ua_public = ua_private.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth_secret = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut auth_secret).unwrap();

        let subscription = WebPushSubscription {
            endpoint: "https://push.example.invalid/subscription/abc".into(),
            p256dh: base64url(&ua_public),
            auth: base64url(&auth_secret),
        };
        subscription.validate().unwrap();

        let payload = PushNotification {
            kind: PushKind::ApprovalRequested,
            target_id: Some("run-seven".into()),
            detail: None,
        }
        .payload(false);
        let plaintext = serde_json::to_vec(&payload).unwrap();

        let mut salt = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut salt).unwrap();
        let as_private =
            ring::agreement::EphemeralPrivateKey::generate(&ring::agreement::ECDH_P256, &rng)
                .unwrap();
        let body = encrypt_web_push(&subscription, &plaintext, salt, as_private).unwrap();

        // --- the receiver, from here down ---
        assert_eq!(&body[..16], &salt, "the header must carry the salt");
        assert_eq!(
            u32::from_be_bytes(body[16..20].try_into().unwrap()),
            RECORD_SIZE
        );
        let key_length = body[20] as usize;
        assert_eq!(key_length, 65, "an uncompressed P-256 point is 65 bytes");
        let as_public = &body[21..21 + key_length];
        let ciphertext = &body[21 + key_length..];

        let peer = ring::agreement::UnparsedPublicKey::new(&ring::agreement::ECDH_P256, as_public);
        let shared =
            ring::agreement::agree_ephemeral(ua_private, &peer, |secret| secret.to_vec()).unwrap();
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(&ua_public);
        key_info.extend_from_slice(as_public);
        let ikm = hkdf(&auth_secret, &shared, &key_info, 32).unwrap();
        let content_encryption_key =
            hkdf(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16).unwrap();
        let nonce = hkdf(&salt, &ikm, b"Content-Encoding: nonce\0", 12).unwrap();
        let key = ring::aead::LessSafeKey::new(
            ring::aead::UnboundKey::new(&ring::aead::AES_128_GCM, &content_encryption_key).unwrap(),
        );
        let mut opened = ciphertext.to_vec();
        let decrypted = key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce.as_slice().try_into().unwrap()),
                ring::aead::Aad::empty(),
                &mut opened,
            )
            .expect("a real subscriber must be able to decrypt this");
        assert_eq!(
            decrypted.last(),
            Some(&0x02),
            "the record must end with the last-record delimiter"
        );
        let received: PushPayload =
            serde_json::from_slice(&decrypted[..decrypted.len() - 1]).unwrap();
        assert_eq!(received, payload);

        // And the seal is real: one flipped ciphertext byte must not open.
        let mut tampered = ciphertext.to_vec();
        tampered[0] ^= 0x01;
        assert!(key
            .open_in_place(
                ring::aead::Nonce::assume_unique_for_key(nonce.as_slice().try_into().unwrap()),
                ring::aead::Aad::empty(),
                &mut tampered,
            )
            .is_err());
    }

    /// The VAPID half: a real ES256 signature, over the claims RFC 8292 asks
    /// for, verifiable with the key the header advertises.
    #[test]
    fn the_vapid_header_is_a_verifiable_es256_token_scoped_to_the_endpoint_origin() {
        let identity = VapidIdentity::from_pkcs8(
            &VapidIdentity::generate().unwrap(),
            "https://runner.example.invalid",
        )
        .unwrap();
        let header = identity
            .authorization(
                "https://push.example.invalid/subscription/abc?query=1",
                1_000,
            )
            .unwrap();
        let (token, advertised_key) = header
            .strip_prefix("vapid t=")
            .and_then(|rest| rest.split_once(", k="))
            .expect("the header must carry both the token and the key");
        assert_eq!(advertised_key, identity.application_server_key());

        let parts: Vec<&str> = token.split('.').collect();
        assert_eq!(parts.len(), 3);
        let claims: serde_json::Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(parts[1])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            claims["aud"], "https://push.example.invalid",
            "the audience is the origin, never the full subscription URL"
        );
        assert_eq!(claims["sub"], "https://runner.example.invalid");
        assert_eq!(claims["exp"].as_u64().unwrap(), 1_000 + 12 * 60 * 60);

        // The signature verifies against the advertised public key, which is
        // the only thing a push service actually checks.
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(parts[2])
            .unwrap();
        let public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(advertised_key)
            .unwrap();
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ECDSA_P256_SHA256_FIXED,
            &public_key,
        )
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &signature)
        .expect("a push service must be able to verify this token");
    }

    /// A subscription is checked before it is stored, so an unusable one is
    /// refused while the device is still there to be told.
    #[test]
    fn a_web_push_subscription_must_be_a_credential_free_https_endpoint_with_real_keys() {
        let good = WebPushSubscription {
            endpoint: "https://push.example.invalid/s/1".into(),
            p256dh: base64url(&[4u8; 65]),
            auth: base64url(&[7u8; 16]),
        };
        assert!(good.validate().is_ok());
        for broken in [
            WebPushSubscription {
                endpoint: "http://push.example.invalid/s/1".into(),
                ..good.clone()
            },
            WebPushSubscription {
                endpoint: "https://user:pass@push.example.invalid/s/1".into(),
                ..good.clone()
            },
            WebPushSubscription {
                p256dh: base64url(&[4u8; 32]),
                ..good.clone()
            },
            WebPushSubscription {
                auth: base64url(&[7u8; 8]),
                ..good.clone()
            },
        ] {
            assert!(
                broken.validate().is_err(),
                "an invalid subscription must not reach storage: {broken:?}"
            );
        }
    }

    /// The whole point of choosing Web Push for the bundled client: it needs no
    /// account anywhere, and the configuration says so.
    #[test]
    fn web_push_needs_no_third_party_project() {
        let config = PushConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            backend: "web_push".into(),
            project_id: String::new(),
            service_account_path: String::new(),
            vapid_subject: "https://runner.example.invalid".into(),
            include_detail: false,
            enabled: true,
        };
        assert!(config.validate().is_ok());
        let unusable_subject = PushConfig {
            vapid_subject: "not a url".into(),
            ..config
        };
        assert!(unusable_subject.validate().is_err());
    }

    /// The privacy claim, tested rather than asserted in a doc comment: what
    /// reaches a lock screen says what kind of thing happened and nothing about
    /// its content, unless the operator turned that on.
    #[test]
    fn a_default_notification_carries_no_content() {
        let notification = PushNotification {
            kind: PushKind::NewResponse,
            target_id: Some("run-one".into()),
            detail: Some("The model said something private".into()),
        };
        let default = notification.payload(false);
        assert_eq!(default.title, "New response");
        assert!(!default.body.contains("private"));
        assert_eq!(
            default.data.get("target_id").map(String::as_str),
            Some("run-one")
        );

        let opted_in = notification.payload(true);
        assert!(opted_in.body.contains("private"));

        // A security alert is the exception, and only in the direction of
        // telling the user what happened to them.
        let alert = PushNotification {
            kind: PushKind::SecurityAlert,
            target_id: Some("device-one".into()),
            detail: Some("device-one was revoked".into()),
        };
        assert!(alert.payload(false).body.contains("revoked"));
    }

    /// The FCM request path, without a network: the exact body and URL this
    /// process would put on the wire, so CI proves the shape without a project,
    /// a service account or a socket.
    #[tokio::test]
    async fn the_fcm_request_is_addressed_to_the_operators_own_project() {
        let payload = PushNotification {
            kind: PushKind::ApprovalRequested,
            target_id: Some("run-two".into()),
            detail: None,
        }
        .payload(false);
        let message = fcm_message("device-token", &payload);
        assert_eq!(message["message"]["token"], "device-token");
        assert_eq!(
            message["message"]["notification"]["title"],
            "Approval needed"
        );
        assert_eq!(message["message"]["data"]["kind"], "approval_requested");
        assert_eq!(message["message"]["data"]["target_id"], "run-two");
        assert_eq!(
            fcm_endpoint("my-own-project"),
            "https://fcm.googleapis.com/v1/projects/my-own-project/messages:send",
            "the endpoint must name the operator's project and nothing else"
        );
        // A fake backend proves the fan-out reaches a backend with exactly the
        // payload that was built.
        let backend = FakeBackend::default();
        backend.send("device-token", &payload).await.unwrap();
        let sent = backend.0.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].1, payload);
    }

    #[test]
    fn push_configuration_belongs_to_the_operator_and_is_validated() {
        let mut config = PushConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            backend: "fcm".into(),
            project_id: String::new(),
            service_account_path: "/tmp/key.json".into(),
            vapid_subject: default_vapid_subject(),
            include_detail: false,
            enabled: true,
        };
        assert!(
            config.validate().is_err(),
            "FCM without the operator's own project id must be refused"
        );
        config.project_id = "their-project".into();
        assert!(config.validate().is_ok());
        config.backend = "littlemonkey-relay".into();
        assert!(
            config.validate().is_err(),
            "there is no first-party relay to point at"
        );

        assert!(ServiceAccount::parse(b"{}").is_err());
        assert!(ServiceAccount::parse(
            br#"{"project_id":"p","client_email":"a@b","private_key":"x","token_uri":"http://insecure"}"#
        )
        .is_err());
    }
}

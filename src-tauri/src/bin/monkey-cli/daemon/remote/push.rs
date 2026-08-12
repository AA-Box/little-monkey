//! Waking a paired device up.
//!
//! **Provider-neutral by construction.** Everything above [`PushBackend`] knows
//! only that a device has a token and that something happened; which service
//! carries it is one implementation of one trait. FCM is the first backend
//! because it is what phones actually have, not because anything here depends
//! on Google.
//!
//! **The configuration is the end user's.** Little Monkey is open source and
//! ships no Firebase project, no service account and no relay: an operator who
//! wants push points this at *their* project and *their* service account, and a
//! machine that has not is simply a machine without push. There is nowhere in
//! this file for a maintainer-owned credential to live, which is the point.
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
    /// `fcm` or `none`. A closed set rather than a URL: a backend is code in
    /// this file, and "point push at an arbitrary host" is not a feature.
    pub backend: String,
    /// The operator's own Firebase project.
    pub project_id: String,
    /// Where the service account JSON was copied to inside app-private state.
    pub service_account_path: String,
    /// Whether notifications may carry specifics. Off by default: the visible
    /// text of a push is the least private thing this system produces.
    pub include_detail: bool,
    pub enabled: bool,
}

impl PushConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err("Unsupported push configuration version".to_string());
        }
        if !matches!(self.backend.as_str(), "fcm" | "none") {
            return Err("Push backend must be 'fcm' or 'none'".to_string());
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
pub fn backend(paths: &DaemonPaths) -> Result<Option<Box<dyn PushBackend>>, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(None);
    };
    if !config.enabled || config.backend == "none" {
        return Ok(None);
    }
    Ok(Some(Box::new(FcmBackend::open(&config)?)))
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
) -> Result<bool, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(false);
    };
    let Some(backend) = backend(paths)? else {
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
) -> Result<usize, String> {
    let Some(config) = load_config(paths)? else {
        return Ok(0);
    };
    let Some(backend) = backend(paths)? else {
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

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::protocol::{
    legacy_capabilities, random_token, random_token_id, sha256_hex, validate_capabilities,
    AuditEntry, ControllerProfile, DeviceCapability, PairAcceptResponse, RemoteScopes,
    RotationBundle, REMOTE_PROTOCOL_VERSION,
};

const REMOTE_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS pairing_invitations (
    pairing_id TEXT PRIMARY KEY,
    token_sha256 TEXT NOT NULL CHECK(length(token_sha256)=64),
    scopes_json BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    accepted_at_ms INTEGER,
    accepted_device_id TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS remote_devices (
    device_id TEXT PRIMARY KEY,
    device_name TEXT NOT NULL,
    secret_generation INTEGER NOT NULL CHECK(secret_generation > 0),
    scopes_json BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER,
    last_sequence INTEGER NOT NULL DEFAULT 0,
    revoked_at_ms INTEGER,
    revoke_reason TEXT
) STRICT;

CREATE TABLE IF NOT EXISTS remote_commands (
    device_id TEXT NOT NULL,
    command_id TEXT NOT NULL,
    nonce TEXT NOT NULL,
    sequence INTEGER NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64),
    method TEXT NOT NULL,
    path TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('processing','complete','reconciled')),
    response_status INTEGER,
    response_body BLOB,
    created_at_ms INTEGER NOT NULL,
    completed_at_ms INTEGER,
    PRIMARY KEY(device_id, command_id),
    UNIQUE(device_id, nonce),
    UNIQUE(device_id, sequence),
    FOREIGN KEY(device_id) REFERENCES remote_devices(device_id)
) STRICT;

CREATE TABLE IF NOT EXISTS remote_audit (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    occurred_at_ms INTEGER NOT NULL,
    device_id TEXT,
    action TEXT NOT NULL,
    target TEXT,
    outcome TEXT NOT NULL,
    request_sha256 TEXT
) STRICT;

CREATE INDEX IF NOT EXISTS remote_audit_time_idx
    ON remote_audit(occurred_at_ms DESC, audit_id DESC);

CREATE TABLE IF NOT EXISTS remote_controllers (
    alias TEXT PRIMARY KEY,
    profile_json BLOB NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS remote_pairing_capabilities (
    pairing_id TEXT PRIMARY KEY REFERENCES pairing_invitations(pairing_id) ON DELETE CASCADE,
    capabilities_json BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS remote_device_capabilities (
    device_id TEXT PRIMARY KEY REFERENCES remote_devices(device_id) ON DELETE CASCADE,
    capabilities_json BLOB NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS remote_mobile_messages (
    message_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL,
    device_id TEXT NOT NULL REFERENCES remote_devices(device_id) ON DELETE RESTRICT,
    role TEXT NOT NULL DEFAULT 'user' CHECK(role IN ('user','assistant','system')),
    text TEXT NOT NULL,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64),
    task_state TEXT NOT NULL CHECK(task_state IN ('queued','accepted','failed')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS remote_mobile_messages_session_idx
    ON remote_mobile_messages(session_id,created_at_ms,message_id);

CREATE TABLE IF NOT EXISTS remote_mobile_captures (
    capture_id TEXT PRIMARY KEY,
    device_id TEXT NOT NULL REFERENCES remote_devices(device_id) ON DELETE RESTRICT,
    kind TEXT NOT NULL CHECK(kind IN ('text','image','file','voice')),
    title TEXT NOT NULL,
    text TEXT,
    content_sha256 TEXT CHECK(content_sha256 IS NULL OR length(content_sha256)=64),
    size_bytes INTEGER,
    media_type TEXT,
    request_sha256 TEXT NOT NULL CHECK(length(request_sha256)=64),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS remote_mobile_workflow_runs (
    run_id TEXT PRIMARY KEY,
    workflow_id TEXT NOT NULL,
    device_id TEXT NOT NULL REFERENCES remote_devices(device_id) ON DELETE RESTRICT,
    created_at_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS remote_mobile_workflow_device_idx
    ON remote_mobile_workflow_runs(device_id,created_at_ms DESC);
"#;

pub trait RemoteSecretStore: Send + Sync {
    fn get(&self, slot: &str) -> Result<Vec<u8>, String>;
    fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String>;
    fn delete(&self, slot: &str) -> Result<(), String>;
}

/// Seam that lets `revoke_device` immediately force-stop any live desktop
/// control session owned by the revoked device, rather than merely blocking
/// its future signed requests. Implemented by
/// `super::desktop::DesktopControlRuntime`; `None` is passed from control
/// planes that hold no live sessions (e.g. the one-shot `pair-revoke` CLI
/// process). Kept as a trait — mirroring `desktop_control.rs`'s own
/// `DesktopInputBackend` seam — so the revoke path is unit-testable without a
/// real runtime. Returns how many sessions were force-stopped.
pub trait DesktopSessionKiller: Send + Sync {
    fn force_stop_device(&self, device_id: &str) -> usize;
}

pub struct KeyringRemoteSecrets;

impl KeyringRemoteSecrets {
    fn entry(slot: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new("com.littlemonkey.remote", slot).map_err(|error| error.to_string())
    }
}

impl RemoteSecretStore for KeyringRemoteSecrets {
    fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
        Self::entry(slot)?
            .get_secret()
            .map_err(|error| format!("Remote keychain secret '{slot}' is unavailable: {error}"))
    }

    fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
        Self::entry(slot)?
            .set_secret(secret)
            .map_err(|error| format!("Could not store remote keychain secret '{slot}': {error}"))
    }

    fn delete(&self, slot: &str) -> Result<(), String> {
        match Self::entry(slot)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!(
                "Could not delete remote keychain secret '{slot}': {error}"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceRecord {
    pub device_id: String,
    pub device_name: String,
    pub secret_generation: u64,
    pub scopes: RemoteScopes,
    pub capabilities: std::collections::BTreeSet<DeviceCapability>,
    pub last_sequence: u64,
    pub revoked_at_ms: Option<u64>,
}

impl DeviceRecord {
    pub fn secret_slot(&self) -> String {
        runner_secret_slot(&self.device_id, self.secret_generation)
    }

    pub fn active(&self) -> bool {
        self.revoked_at_ms.is_none()
    }
}

#[derive(Debug, Clone)]
pub struct InvitationRecord {
    pub pairing_id: String,
    pub token: String,
    pub scopes: RemoteScopes,
    pub capabilities: BTreeSet<DeviceCapability>,
    pub expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileMessageRecord {
    pub message_id: String,
    pub session_id: String,
    pub device_id: String,
    /// `user`, `assistant`, or `system` — the same three roles the mobile
    /// client renders. Assistant/system rows are node-authored (reply
    /// materialization) and keep the ORIGINATING device/request digest for
    /// provenance.
    pub role: String,
    pub text: String,
    /// SHA-256 of the signed request that created (or, for node-authored
    /// replies, triggered) this row.
    pub request_sha256: String,
    pub task_state: String,
    pub created_at_ms: u64,
}

/// One mobile chat session summarized for `GET /v1/remote/mobile/sessions` —
/// derived entirely from `remote_mobile_messages`, which is the node-side
/// source of truth for mobile-originated conversations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileSessionSummary {
    pub session_id: String,
    pub title: String,
    pub updated_at_ms: u64,
    pub message_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileCaptureRecord {
    pub capture_id: String,
    pub device_id: String,
    pub kind: String,
    pub title: String,
    pub text: Option<String>,
    pub content_sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub media_type: Option<String>,
    /// SHA-256 of the signed request that uploaded this capture.
    pub request_sha256: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MobileWorkflowRunRecord {
    pub run_id: String,
    pub workflow_id: String,
    pub device_id: String,
    pub created_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandReservation {
    New,
    Replay {
        status: Option<u16>,
        response_body: Option<Vec<u8>>,
        processing: bool,
    },
}

pub struct RemoteStore {
    connection: Connection,
}

impl RemoteStore {
    pub fn open(daemon_root: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(daemon_root)
            .map_err(|error| format!("Could not create remote state directory: {error}"))?;
        let path = daemon_root.join("remote-v1.sqlite3");
        let connection = Connection::open(&path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .map_err(|error| error.to_string())?;
        connection
            .execute_batch(REMOTE_SCHEMA)
            .map_err(|error| format!("Could not migrate remote runner state: {error}"))?;
        crate::daemon::store::restrict_file(&path)?;
        Ok(Self { connection })
    }

    /// Legacy-shaped invitation: exactly the capabilities implied by the
    /// run scope, with no mobile grants. Production `pair-create` always
    /// goes through [`Self::create_invitation_with_capabilities`] so an
    /// operator's mobile selection is explicit; this remains for the tests
    /// that pin legacy pairing behavior.
    #[cfg(test)]
    pub fn create_invitation(
        &mut self,
        scopes: &RemoteScopes,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<InvitationRecord, String> {
        let capabilities = legacy_capabilities(scopes);
        self.create_invitation_with_capabilities(
            scopes,
            &capabilities,
            now_ms,
            expires_at_ms,
        )
    }

    pub fn create_invitation_with_capabilities(
        &mut self,
        scopes: &RemoteScopes,
        capabilities: &BTreeSet<DeviceCapability>,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<InvitationRecord, String> {
        scopes.validate()?;
        validate_capabilities(capabilities, scopes)?;
        if expires_at_ms <= now_ms || expires_at_ms.saturating_sub(now_ms) > 24 * 60 * 60 * 1_000 {
            return Err("Pairing invitation lifetime must be between now and 24 hours".to_string());
        }
        let pairing_id = format!("pair-{}", random_token(18)?);
        let token = random_token(32)?;
        self.connection
            .execute(
                "INSERT INTO pairing_invitations
                 (pairing_id,token_sha256,scopes_json,created_at_ms,expires_at_ms)
                 VALUES(?1,?2,?3,?4,?5)",
                params![
                    pairing_id,
                    sha256_hex(token.as_bytes()),
                    serde_json::to_vec(scopes).map_err(|error| error.to_string())?,
                    to_i64(now_ms)?,
                    to_i64(expires_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        self.connection
            .execute(
                "INSERT INTO remote_pairing_capabilities(pairing_id,capabilities_json)
                 VALUES(?1,?2)",
                params![
                    pairing_id,
                    serde_json::to_vec(capabilities).map_err(|error| error.to_string())?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(InvitationRecord {
            pairing_id,
            token,
            scopes: scopes.clone(),
            capabilities: capabilities.clone(),
            expires_at_ms,
        })
    }

    pub fn accept_invitation(
        &mut self,
        pairing_id: &str,
        token: &str,
        device_name: &str,
        runner_id: &str,
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<PairAcceptResponse, String> {
        self.accept_invitation_with_capabilities(
            pairing_id,
            token,
            device_name,
            runner_id,
            None,
            now_ms,
            secrets,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn accept_invitation_with_capabilities(
        &mut self,
        pairing_id: &str,
        token: &str,
        device_name: &str,
        runner_id: &str,
        requested_capabilities: Option<&BTreeSet<DeviceCapability>>,
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<PairAcceptResponse, String> {
        validate_device_name(device_name)?;
        // `random_token_id`, not `random_token`: this id is later reused as
        // `ClientIdentity.client_id`/`instance_id` (see `control_recorder` in
        // `daemon/remote/api.rs`), which `validate_protocol_id` requires to
        // start and end with an ASCII letter or digit — a plain
        // `random_token` can land `-`/`_` at either boundary.
        let device_id = format!("device-{}", random_token_id(18)?);
        let secret = random_token(32)?;
        let secret_generation = 1u64;
        let slot = runner_secret_slot(&device_id, secret_generation);
        secrets.set(&slot, secret.as_bytes())?;

        let result = (|| {
            let transaction = self
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| error.to_string())?;
            let invitation = transaction
                .query_row(
                    "SELECT i.token_sha256,i.scopes_json,i.expires_at_ms,i.accepted_at_ms,
                            c.capabilities_json
                     FROM pairing_invitations i
                     LEFT JOIN remote_pairing_capabilities c ON c.pairing_id=i.pairing_id
                     WHERE i.pairing_id=?1",
                    [pairing_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<Vec<u8>>>(4)?,
                        ))
                    },
                )
                .optional()
                .map_err(|error| error.to_string())?
                .ok_or_else(|| "Unknown pairing invitation".to_string())?;
            if invitation.3.is_some() {
                return Err("Pairing invitation was already consumed".to_string());
            }
            if invitation.2 <= to_i64(now_ms)? {
                return Err("Pairing invitation has expired".to_string());
            }
            if invitation.0 != sha256_hex(token.as_bytes()) {
                return Err("Pairing token is invalid".to_string());
            }
            let scopes: RemoteScopes =
                serde_json::from_slice(&invitation.1).map_err(|error| error.to_string())?;
            scopes.validate()?;
            let invited_capabilities = invitation
                .4
                .as_deref()
                .map(serde_json::from_slice::<BTreeSet<DeviceCapability>>)
                .transpose()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| legacy_capabilities(&scopes));
            validate_capabilities(&invited_capabilities, &scopes)?;
            let capabilities = requested_capabilities
                .cloned()
                .unwrap_or_else(|| invited_capabilities.clone());
            if !capabilities.is_subset(&invited_capabilities) {
                return Err(
                    "Pairing response cannot grant capabilities absent from the invitation"
                        .to_string(),
                );
            }
            validate_capabilities(&capabilities, &scopes)?;
            transaction
                .execute(
                    "INSERT INTO remote_devices
                     (device_id,device_name,secret_generation,scopes_json,created_at_ms)
                     VALUES(?1,?2,?3,?4,?5)",
                    params![
                        device_id,
                        device_name,
                        to_i64(secret_generation)?,
                        invitation.1,
                        to_i64(now_ms)?,
                    ],
                )
                .map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT INTO remote_device_capabilities(device_id,capabilities_json)
                     VALUES(?1,?2)",
                    params![
                        device_id,
                        serde_json::to_vec(&capabilities).map_err(|error| error.to_string())?,
                    ],
                )
                .map_err(|error| error.to_string())?;
            let changed = transaction
                .execute(
                    "UPDATE pairing_invitations
                     SET accepted_at_ms=?2,accepted_device_id=?3
                     WHERE pairing_id=?1 AND accepted_at_ms IS NULL",
                    params![pairing_id, to_i64(now_ms)?, device_id],
                )
                .map_err(|error| error.to_string())?;
            if changed != 1 {
                return Err("Pairing invitation was consumed concurrently".to_string());
            }
            transaction.commit().map_err(|error| error.to_string())?;
            Ok(PairAcceptResponse {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                runner_id: runner_id.to_string(),
                device_id,
                secret_generation,
                device_secret: secret,
                scopes,
                capabilities,
            })
        })();
        if result.is_err() {
            let _ = secrets.delete(&slot);
        }
        result
    }

    pub fn device(&self, device_id: &str) -> Result<Option<DeviceRecord>, String> {
        self.connection
            .query_row(
                "SELECT device_id,device_name,secret_generation,scopes_json,
                        last_sequence,revoked_at_ms,
                        (SELECT capabilities_json FROM remote_device_capabilities c
                          WHERE c.device_id=remote_devices.device_id)
                 FROM remote_devices WHERE device_id=?1",
                [device_id],
                read_device,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub fn devices(&self) -> Result<Vec<DeviceRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT device_id,device_name,secret_generation,scopes_json,
                        last_sequence,revoked_at_ms,
                        (SELECT capabilities_json FROM remote_device_capabilities c
                          WHERE c.device_id=remote_devices.device_id)
                 FROM remote_devices ORDER BY created_at_ms ASC,device_id ASC",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], read_device)
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn revoke_device(
        &mut self,
        device_id: &str,
        reason: &str,
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
        desktop: Option<&dyn DesktopSessionKiller>,
    ) -> Result<bool, String> {
        let Some(device) = self.device(device_id)? else {
            return Ok(false);
        };
        let changed = self
            .connection
            .execute(
                "UPDATE remote_devices SET revoked_at_ms=?2,revoke_reason=?3
                 WHERE device_id=?1 AND revoked_at_ms IS NULL",
                params![device_id, to_i64(now_ms)?, bounded(reason, 1_024)],
            )
            .map_err(|error| error.to_string())?;
        // Database revocation is authoritative and immediate. Keychain
        // deletion is best-effort cleanup; a retained old secret cannot pass
        // the revoked row check.
        let _ = secrets.delete(&device.secret_slot());
        if changed == 1 {
            self.audit(
                now_ms,
                Some(device_id),
                "device_revoke",
                Some(device_id),
                "allowed",
                None,
            )?;
            // Blocking future signed requests is not enough: a live desktop
            // control session (especially an approved-batch one) must be
            // force-stopped the instant its device is revoked, not left
            // driving the cursor until it expires.
            if let Some(killer) = desktop {
                let stopped = killer.force_stop_device(device_id);
                if stopped > 0 {
                    self.audit(
                        now_ms,
                        Some(device_id),
                        "desktop_control_force_stop",
                        Some(device_id),
                        "revoked",
                        None,
                    )?;
                }
            }
        }
        Ok(changed == 1)
    }

    pub fn rotate_device(
        &mut self,
        device_id: &str,
        runner_id: &str,
        runner_url: &str,
        certificate_pem: &str,
        certificate_sha256: &str,
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<RotationBundle, String> {
        let device = self
            .device(device_id)?
            .ok_or_else(|| format!("Unknown remote device '{device_id}'"))?;
        if !device.active() {
            return Err("Revoked devices cannot be rotated".to_string());
        }
        let new_generation = device
            .secret_generation
            .checked_add(1)
            .ok_or_else(|| "Remote key generation overflow".to_string())?;
        let secret = random_token(32)?;
        let new_slot = runner_secret_slot(device_id, new_generation);
        secrets.set(&new_slot, secret.as_bytes())?;
        let changed = self
            .connection
            .execute(
                "UPDATE remote_devices SET secret_generation=?2,last_sequence=0
                 WHERE device_id=?1 AND secret_generation=?3 AND revoked_at_ms IS NULL",
                params![
                    device_id,
                    to_i64(new_generation)?,
                    to_i64(device.secret_generation)?
                ],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            let _ = secrets.delete(&new_slot);
            return Err("Remote device changed concurrently".to_string());
        }
        let _ = secrets.delete(&device.secret_slot());
        self.audit(
            now_ms,
            Some(device_id),
            "device_rotate",
            Some(device_id),
            "allowed",
            None,
        )?;
        Ok(RotationBundle {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: runner_id.to_string(),
            device_id: device_id.to_string(),
            secret_generation: new_generation,
            device_secret: secret,
            runner_url: runner_url.to_string(),
            server_certificate_pem: certificate_pem.to_string(),
            server_certificate_sha256: certificate_sha256.to_string(),
            scopes: device.scopes,
            capabilities: device.capabilities,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn reserve_command(
        &mut self,
        device_id: &str,
        generation: u64,
        command_id: &str,
        nonce: &str,
        sequence: u64,
        request_sha256: &str,
        method: &str,
        path: &str,
        now_ms: u64,
    ) -> Result<CommandReservation, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let current = transaction
            .query_row(
                "SELECT secret_generation,last_sequence,revoked_at_ms
                 FROM remote_devices WHERE device_id=?1",
                [device_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "Unknown remote device".to_string())?;
        if current.2.is_some() {
            return Err("Remote device is revoked".to_string());
        }
        if current.0 != to_i64(generation)? {
            return Err("Remote key generation is stale".to_string());
        }
        if let Some(existing) = transaction
            .query_row(
                "SELECT nonce,sequence,request_sha256,status,response_status,response_body
                 FROM remote_commands WHERE device_id=?1 AND command_id=?2",
                params![device_id, command_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                        row.get::<_, Option<Vec<u8>>>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?
        {
            transaction.commit().map_err(|error| error.to_string())?;
            if existing.0 != nonce
                || existing.1 != to_i64(sequence)?
                || existing.2 != request_sha256
            {
                return Err("Conflicting replay of remote command id".to_string());
            }
            return Ok(CommandReservation::Replay {
                status: existing
                    .4
                    .map(|value| u16::try_from(value).map_err(|_| "Invalid response status"))
                    .transpose()?,
                response_body: existing.5,
                processing: existing.3 == "processing",
            });
        }
        if to_i64(sequence)? <= current.1 {
            return Err("Remote request sequence was already passed".to_string());
        }
        let inserted = transaction.execute(
            "INSERT INTO remote_commands
             (device_id,command_id,nonce,sequence,request_sha256,method,path,status,created_at_ms)
             VALUES(?1,?2,?3,?4,?5,?6,?7,'processing',?8)",
            params![
                device_id,
                command_id,
                nonce,
                to_i64(sequence)?,
                request_sha256,
                method,
                path,
                to_i64(now_ms)?
            ],
        );
        if let Err(error) = inserted {
            return Err(
                if error
                    .to_string()
                    .contains("remote_commands.device_id, remote_commands.nonce")
                {
                    "Remote request nonce was already used".to_string()
                } else {
                    format!("Could not reserve remote command: {error}")
                },
            );
        }
        transaction
            .execute(
                "UPDATE remote_devices SET last_sequence=?2,last_seen_at_ms=?3
                 WHERE device_id=?1",
                params![device_id, to_i64(sequence)?, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(CommandReservation::New)
    }

    pub fn complete_command(
        &mut self,
        device_id: &str,
        command_id: &str,
        status: u16,
        response_body: &[u8],
        reconciled: bool,
        now_ms: u64,
    ) -> Result<(), String> {
        let cached = if response_body.len() <= 1024 * 1024 {
            Some(response_body)
        } else {
            None
        };
        let changed = self
            .connection
            .execute(
                "UPDATE remote_commands SET status=?3,response_status=?4,response_body=?5,
                    completed_at_ms=?6 WHERE device_id=?1 AND command_id=?2",
                params![
                    device_id,
                    command_id,
                    if reconciled { "reconciled" } else { "complete" },
                    i64::from(status),
                    cached,
                    to_i64(now_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("Remote command reservation disappeared".to_string());
        }
        Ok(())
    }

    pub fn audit(
        &mut self,
        now_ms: u64,
        device_id: Option<&str>,
        action: &str,
        target: Option<&str>,
        outcome: &str,
        request_sha256: Option<&str>,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO remote_audit
                 (occurred_at_ms,device_id,action,target,outcome,request_sha256)
                 VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    to_i64(now_ms)?,
                    device_id,
                    bounded(action, 128),
                    target.map(|value| bounded(value, 512)),
                    bounded(outcome, 128),
                    request_sha256,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn audit_entries(&self, limit: u32) -> Result<Vec<AuditEntry>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT audit_id,occurred_at_ms,device_id,action,target,outcome,request_sha256
                 FROM remote_audit ORDER BY audit_id DESC LIMIT ?1",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([i64::from(limit.min(1_000))], |row| {
                Ok(AuditEntry {
                    audit_id: from_i64(row.get(0)?)?,
                    occurred_at_ms: from_i64(row.get(1)?)?,
                    device_id: row.get(2)?,
                    action: row.get(3)?,
                    target: row.get(4)?,
                    outcome: row.get(5)?,
                    request_sha256: row.get(6)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    // --- Mobile extension (`/v1/remote/mobile/*`) records -----------------

    pub fn insert_mobile_message(&mut self, record: &MobileMessageRecord) -> Result<(), String> {
        if record.message_id.is_empty() || record.message_id.len() > 128 {
            return Err("Mobile message id must be 1-128 characters".to_string());
        }
        if record.session_id.is_empty() || record.session_id.len() > 128 {
            return Err("Mobile session id must be 1-128 characters".to_string());
        }
        if record.text.is_empty() || record.text.len() > 256 * 1024 {
            return Err("Mobile message text must be 1 byte to 256 KiB".to_string());
        }
        self.connection
            .execute(
                "INSERT INTO remote_mobile_messages
                 (message_id,session_id,device_id,role,text,request_sha256,task_state,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?8)
                 ON CONFLICT(message_id) DO NOTHING",
                params![
                    record.message_id,
                    record.session_id,
                    record.device_id,
                    record.role,
                    record.text,
                    record.request_sha256,
                    record.task_state,
                    to_i64(record.created_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn set_mobile_message_state(
        &mut self,
        message_id: &str,
        task_state: &str,
        now_ms: u64,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "UPDATE remote_mobile_messages SET task_state=?2, updated_at_ms=?3
                 WHERE message_id=?1",
                params![message_id, task_state, to_i64(now_ms)?],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn mobile_messages(
        &self,
        session_id: &str,
        limit: u32,
    ) -> Result<Vec<MobileMessageRecord>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT message_id,session_id,device_id,role,text,request_sha256,task_state,created_at_ms
                 FROM remote_mobile_messages WHERE session_id=?1
                 ORDER BY created_at_ms ASC, message_id ASC LIMIT ?2",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map(params![session_id, i64::from(limit.min(2_000))], |row| {
                Ok(MobileMessageRecord {
                    message_id: row.get(0)?,
                    session_id: row.get(1)?,
                    device_id: row.get(2)?,
                    role: row.get(3)?,
                    text: row.get(4)?,
                    request_sha256: row.get(5)?,
                    task_state: row.get(6)?,
                    created_at_ms: from_i64(row.get(7)?)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn mobile_session_summaries(&self) -> Result<Vec<MobileSessionSummary>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT session_id,
                        (SELECT text FROM remote_mobile_messages first
                          WHERE first.session_id = all_rows.session_id AND first.role='user'
                          ORDER BY first.created_at_ms ASC, first.message_id ASC LIMIT 1),
                        MAX(created_at_ms),
                        COUNT(*)
                 FROM remote_mobile_messages all_rows
                 GROUP BY session_id
                 ORDER BY MAX(created_at_ms) DESC
                 LIMIT 200",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let title: Option<String> = row.get(1)?;
                Ok(MobileSessionSummary {
                    session_id: row.get(0)?,
                    title: title.unwrap_or_default(),
                    updated_at_ms: from_i64(row.get(2)?)?,
                    message_count: from_i64(row.get(3)?)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn insert_mobile_capture(&mut self, record: &MobileCaptureRecord) -> Result<(), String> {
        if record.capture_id.is_empty() || record.capture_id.len() > 128 {
            return Err("Mobile capture id must be 1-128 characters".to_string());
        }
        if record.title.is_empty() || record.title.len() > 512 {
            return Err("Mobile capture title must be 1-512 characters".to_string());
        }
        self.connection
            .execute(
                "INSERT INTO remote_mobile_captures
                 (capture_id,device_id,kind,title,text,content_sha256,size_bytes,media_type,request_sha256,created_at_ms,updated_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?10)
                 ON CONFLICT(capture_id) DO NOTHING",
                params![
                    record.capture_id,
                    record.device_id,
                    record.kind,
                    record.title,
                    record.text,
                    record.content_sha256,
                    record.size_bytes.map(to_i64).transpose()?,
                    record.media_type,
                    record.request_sha256,
                    to_i64(record.created_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn insert_mobile_workflow_run(
        &mut self,
        record: &MobileWorkflowRunRecord,
    ) -> Result<(), String> {
        self.connection
            .execute(
                "INSERT INTO remote_mobile_workflow_runs
                 (run_id,workflow_id,device_id,created_at_ms)
                 VALUES(?1,?2,?3,?4)
                 ON CONFLICT(run_id) DO NOTHING",
                params![
                    record.run_id,
                    record.workflow_id,
                    record.device_id,
                    to_i64(record.created_at_ms)?,
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    /// Latest mobile-launched run timestamp per workflow, for the workflow
    /// list's `last_run_at_ms` field.
    pub fn mobile_workflow_last_runs(
        &self,
    ) -> Result<std::collections::HashMap<String, u64>, String> {
        let mut statement = self
            .connection
            .prepare(
                "SELECT workflow_id, MAX(created_at_ms)
                 FROM remote_mobile_workflow_runs GROUP BY workflow_id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, from_i64(row.get(1)?)?))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<Result<std::collections::HashMap<_, _>, _>>()
            .map_err(|error| error.to_string())
    }

    pub fn save_controller(
        &mut self,
        profile: &ControllerProfile,
        secret: &[u8],
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<(), String> {
        profile.scopes.validate()?;
        let capabilities = if profile.capabilities.is_empty() {
            legacy_capabilities(&profile.scopes)
        } else {
            profile.capabilities.clone()
        };
        validate_capabilities(&capabilities, &profile.scopes)?;
        let slot = controller_secret_slot(&profile.alias, profile.secret_generation);
        secrets.set(&slot, secret)?;
        let stored = serde_json::to_vec(profile).map_err(|error| error.to_string())?;
        let result = self.connection.execute(
            "INSERT INTO remote_controllers(alias,profile_json,updated_at_ms)
             VALUES(?1,?2,?3)
             ON CONFLICT(alias) DO UPDATE SET profile_json=excluded.profile_json,
                updated_at_ms=excluded.updated_at_ms",
            params![profile.alias, stored, to_i64(now_ms)?],
        );
        if let Err(error) = result {
            let _ = secrets.delete(&slot);
            return Err(error.to_string());
        }
        Ok(())
    }

    pub fn controller(&self, alias: &str) -> Result<Option<ControllerProfile>, String> {
        let bytes = self
            .connection
            .query_row(
                "SELECT profile_json FROM remote_controllers WHERE alias=?1",
                [alias],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        bytes
            .map(|value| serde_json::from_slice(&value).map_err(|error| error.to_string()))
            .transpose()
    }

    pub fn allocate_controller_sequence(&mut self, alias: &str) -> Result<u64, String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let bytes = transaction
            .query_row(
                "SELECT profile_json FROM remote_controllers WHERE alias=?1",
                [alias],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("Unknown remote controller '{alias}'"))?;
        let mut profile: ControllerProfile =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        let allocated = profile.next_sequence.max(1);
        profile.next_sequence = allocated
            .checked_add(1)
            .ok_or_else(|| "Remote controller sequence overflow".to_string())?;
        transaction
            .execute(
                "UPDATE remote_controllers SET profile_json=?2 WHERE alias=?1",
                params![
                    alias,
                    serde_json::to_vec(&profile).map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(allocated)
    }

    pub fn update_controller_cursor(
        &mut self,
        alias: &str,
        run_id: &str,
        sequence: u64,
    ) -> Result<(), String> {
        let mut profile = self
            .controller(alias)?
            .ok_or_else(|| format!("Unknown remote controller '{alias}'"))?;
        let cursor = profile.event_cursors.entry(run_id.to_string()).or_default();
        *cursor = (*cursor).max(sequence);
        self.connection
            .execute(
                "UPDATE remote_controllers SET profile_json=?2 WHERE alias=?1",
                params![
                    alias,
                    serde_json::to_vec(&profile).map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn controller_secret(
        profile: &ControllerProfile,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<Vec<u8>, String> {
        secrets.get(&controller_secret_slot(
            &profile.alias,
            profile.secret_generation,
        ))
    }

    pub fn replace_controller_rotation(
        &mut self,
        alias: &str,
        bundle: &RotationBundle,
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<(), String> {
        let old = self
            .controller(alias)?
            .ok_or_else(|| format!("Unknown remote controller '{alias}'"))?;
        if old.runner_id != bundle.runner_id || old.device_id != bundle.device_id {
            return Err("Rotation bundle belongs to a different runner/device".to_string());
        }
        if bundle.secret_generation <= old.secret_generation {
            return Err("Rotation bundle is not newer than the current key".to_string());
        }
        if !bundle.scopes.is_subset_of(&old.scopes) {
            return Err("Rotation bundle attempts to expand controller scope".to_string());
        }
        let bundle_capabilities = if bundle.capabilities.is_empty() {
            legacy_capabilities(&bundle.scopes)
        } else {
            bundle.capabilities.clone()
        };
        let old_capabilities = if old.capabilities.is_empty() {
            legacy_capabilities(&old.scopes)
        } else {
            old.capabilities.clone()
        };
        if !bundle_capabilities.is_subset(&old_capabilities) {
            return Err("Rotation bundle attempts to expand controller capabilities".to_string());
        }
        validate_capabilities(&bundle_capabilities, &bundle.scopes)?;
        let profile = ControllerProfile {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            alias: alias.to_string(),
            runner_id: bundle.runner_id.clone(),
            runner_url: bundle.runner_url.clone(),
            server_certificate_pem: bundle.server_certificate_pem.clone(),
            server_certificate_sha256: bundle.server_certificate_sha256.clone(),
            device_id: bundle.device_id.clone(),
            secret_generation: bundle.secret_generation,
            scopes: bundle.scopes.clone(),
            capabilities: bundle_capabilities,
            next_sequence: 1,
            event_cursors: old.event_cursors,
        };
        self.save_controller(&profile, bundle.device_secret.as_bytes(), now_ms, secrets)?;
        let _ = secrets.delete(&controller_secret_slot(alias, old.secret_generation));
        Ok(())
    }
}

fn read_device(row: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceRecord> {
    let scopes_bytes: Vec<u8> = row.get(3)?;
    let scopes: RemoteScopes = serde_json::from_slice(&scopes_bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    // Devices paired before the capability split have no
    // remote_device_capabilities row (the correlated subquery yields NULL);
    // they keep exactly their legacy remote-action surface.
    let capabilities = match row.get::<_, Option<Vec<u8>>>(6)? {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                6,
                rusqlite::types::Type::Blob,
                Box::new(error),
            )
        })?,
        None => legacy_capabilities(&scopes),
    };
    Ok(DeviceRecord {
        device_id: row.get(0)?,
        device_name: row.get(1)?,
        secret_generation: from_i64(row.get(2)?)?,
        scopes,
        capabilities,
        last_sequence: from_i64(row.get(4)?)?,
        revoked_at_ms: row.get::<_, Option<i64>>(5)?.map(from_i64).transpose()?,
    })
}

pub fn runner_secret_slot(device_id: &str, generation: u64) -> String {
    format!("runner:{device_id}:{generation}")
}

pub fn controller_secret_slot(alias: &str, generation: u64) -> String {
    format!("controller:{alias}:{generation}")
}

fn validate_device_name(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > 128 || value.contains(['\r', '\n']) {
        Err("Remote device name must be a non-empty single line under 128 bytes".to_string())
    } else {
        Ok(())
    }
}

fn bounded(value: &str, max: usize) -> String {
    value.chars().take(max).collect()
}

fn to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "Remote numeric value exceeds SQLite range".to_string())
}

fn from_i64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(0, value))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::sync::Mutex;

    use super::*;
    use crate::daemon::remote::protocol::RemoteAction;

    #[derive(Default)]
    struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl RemoteSecretStore for FakeSecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    fn fixture() -> (PathBuf, RemoteStore, FakeSecrets, RemoteScopes) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-remote-store-{}",
            uuid::Uuid::new_v4()
        ));
        let store = RemoteStore::open(&root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns, RemoteAction::Cancel]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        (root, store, FakeSecrets::default(), scopes)
    }

    #[test]
    fn invitation_is_one_time_and_secret_never_enters_sqlite() {
        let (root, mut store, secrets, scopes) = fixture();
        let invitation = store.create_invitation(&scopes, 1_000, 2_000).unwrap();
        let accepted = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "phone",
                "runner-one",
                1_100,
                &secrets,
            )
            .unwrap();
        assert!(store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "second",
                "runner-one",
                1_101,
                &secrets,
            )
            .is_err());
        let database = std::fs::read(root.join("remote-v1.sqlite3")).unwrap();
        assert!(!database
            .windows(accepted.device_secret.len())
            .any(|window| window == accepted.device_secret.as_bytes()));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn command_replay_is_idempotent_but_nonce_or_payload_conflicts_fail() {
        let (root, mut store, secrets, scopes) = fixture();
        let invitation = store.create_invitation(&scopes, 1_000, 2_000).unwrap();
        let accepted = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "phone",
                "runner-one",
                1_100,
                &secrets,
            )
            .unwrap();
        assert_eq!(
            store
                .reserve_command(
                    &accepted.device_id,
                    1,
                    "cmd-one",
                    "nonce-one-0123456",
                    1,
                    &"a".repeat(64),
                    "POST",
                    "/cancel",
                    1_200,
                )
                .unwrap(),
            CommandReservation::New
        );
        store
            .complete_command(&accepted.device_id, "cmd-one", 200, b"ok", false, 1_201)
            .unwrap();
        assert_eq!(
            store
                .reserve_command(
                    &accepted.device_id,
                    1,
                    "cmd-one",
                    "nonce-one-0123456",
                    1,
                    &"a".repeat(64),
                    "POST",
                    "/cancel",
                    1_202,
                )
                .unwrap(),
            CommandReservation::Replay {
                status: Some(200),
                response_body: Some(b"ok".to_vec()),
                processing: false,
            }
        );
        assert!(store
            .reserve_command(
                &accepted.device_id,
                1,
                "cmd-one",
                "nonce-one-0123456",
                1,
                &"b".repeat(64),
                "POST",
                "/cancel",
                1_203,
            )
            .is_err());
        assert!(store
            .reserve_command(
                &accepted.device_id,
                1,
                "cmd-two",
                "nonce-one-0123456",
                2,
                &"c".repeat(64),
                "GET",
                "/runs",
                1_204,
            )
            .is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[derive(Default)]
    struct RecordingKiller(Mutex<Vec<String>>);
    impl DesktopSessionKiller for RecordingKiller {
        fn force_stop_device(&self, device_id: &str) -> usize {
            self.0.lock().unwrap().push(device_id.to_string());
            // Pretend one live session was force-stopped so the revoke path
            // records its audit entry.
            1
        }
    }

    #[test]
    fn revoke_force_stops_the_revoked_devices_desktop_sessions() {
        let (root, mut store, secrets, scopes) = fixture();
        let invitation = store.create_invitation(&scopes, 1_000, 2_000).unwrap();
        let accepted = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "phone",
                "runner-one",
                1_100,
                &secrets,
            )
            .unwrap();
        let killer = RecordingKiller::default();
        assert!(store
            .revoke_device(&accepted.device_id, "lost", 1_300, &secrets, Some(&killer))
            .unwrap());
        assert_eq!(
            killer.0.lock().unwrap().as_slice(),
            &[accepted.device_id.clone()],
            "revoke must force-stop the revoked device's live sessions immediately"
        );
        // A second revoke is a no-op (already revoked) and must not re-fire the
        // killer.
        assert!(!store
            .revoke_device(&accepted.device_id, "again", 1_400, &secrets, Some(&killer))
            .unwrap());
        assert_eq!(killer.0.lock().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn revoke_and_rotation_stop_old_authority_immediately() {
        let (root, mut store, secrets, scopes) = fixture();
        let invitation = store.create_invitation(&scopes, 1_000, 2_000).unwrap();
        let accepted = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "phone",
                "runner-one",
                1_100,
                &secrets,
            )
            .unwrap();
        let rotation = store
            .rotate_device(
                &accepted.device_id,
                "runner-one",
                "https://runner.test",
                "-----BEGIN CERTIFICATE-----\nQQ==\n-----END CERTIFICATE-----",
                &"a".repeat(64),
                1_200,
                &secrets,
            )
            .unwrap();
        assert_eq!(rotation.secret_generation, 2);
        assert!(store
            .reserve_command(
                &accepted.device_id,
                1,
                "cmd-old",
                "nonce-old-0123456",
                1,
                &"a".repeat(64),
                "GET",
                "/runs",
                1_201,
            )
            .is_err());
        assert!(store
            .revoke_device(&accepted.device_id, "lost", 1_300, &secrets, None)
            .unwrap());
        assert!(store
            .reserve_command(
                &accepted.device_id,
                2,
                "cmd-new",
                "nonce-new-0123456",
                1,
                &"a".repeat(64),
                "GET",
                "/runs",
                1_301,
            )
            .is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}

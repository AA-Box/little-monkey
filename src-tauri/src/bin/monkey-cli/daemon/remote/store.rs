use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use super::protocol::{
    random_token, random_token_id, sha256_hex, AuditEntry, ControllerProfile, PairAcceptResponse,
    RemoteScopes, RotationBundle, REMOTE_PROTOCOL_VERSION,
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
"#;

pub trait RemoteSecretStore: Send + Sync {
    fn get(&self, slot: &str) -> Result<Vec<u8>, String>;
    fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String>;
    fn delete(&self, slot: &str) -> Result<(), String>;
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
    pub expires_at_ms: u64,
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

    pub fn create_invitation(
        &mut self,
        scopes: &RemoteScopes,
        now_ms: u64,
        expires_at_ms: u64,
    ) -> Result<InvitationRecord, String> {
        scopes.validate()?;
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
        Ok(InvitationRecord {
            pairing_id,
            token,
            scopes: scopes.clone(),
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
                    "SELECT token_sha256,scopes_json,expires_at_ms,accepted_at_ms
                     FROM pairing_invitations WHERE pairing_id=?1",
                    [pairing_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Vec<u8>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
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
                        last_sequence,revoked_at_ms
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
                        last_sequence,revoked_at_ms
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

    pub fn save_controller(
        &mut self,
        profile: &ControllerProfile,
        secret: &[u8],
        now_ms: u64,
        secrets: &dyn RemoteSecretStore,
    ) -> Result<(), String> {
        profile.scopes.validate()?;
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
    let scopes = serde_json::from_slice(&scopes_bytes).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Blob, Box::new(error))
    })?;
    Ok(DeviceRecord {
        device_id: row.get(0)?,
        device_name: row.get(1)?,
        secret_generation: from_i64(row.get(2)?)?,
        scopes,
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
            .revoke_device(&accepted.device_id, "lost", 1_300, &secrets)
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

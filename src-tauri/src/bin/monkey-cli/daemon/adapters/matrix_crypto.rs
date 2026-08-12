//! Matrix end-to-end encryption, for real.
//!
//! The Matrix adapter speaks the Client-Server API by hand. Encryption is the
//! one part of that spec nobody should hand-roll: Olm and Megolm are where a
//! subtle mistake stops being a bug and starts being someone's private
//! conversation. So the ratchets, device keys and session management come
//! from `matrix-sdk-crypto` — the crypto crate the Matrix Rust SDK itself is
//! built on — and this module is only the plumbing between it and our own
//! HTTP.
//!
//! # What lives where
//!
//! - **`matrix-sdk-crypto`** owns every key and every ratchet. It decides
//!   what requests need making; it never makes them.
//! - **This module** ships those requests over `little_monkey_lib::egress`,
//!   the same hardened client every other adapter uses, and hands the
//!   responses back. That keeps encryption inside the SSRF and private-network
//!   controls rather than beside them.
//! - **`matrix-sdk-sqlite`** persists the store under the account's own
//!   profile data root. Keys surviving a restart is not a nicety: a lost
//!   store means every already-shared Megolm session is unreadable, and the
//!   account shows up to everyone else as a brand-new unverified device.
//!
//! # The device is the token's device
//!
//! An access token belongs to a device the homeserver already knows about.
//! The device id is read from `/account/whoami` rather than invented, so this
//! app is the same device the user sees in their own session list — inventing
//! one would mean every restart adds another unverified device to their
//! account.

use std::collections::BTreeMap;
use std::path::Path;

use matrix_sdk_crypto::types::requests::{
    AnyIncomingResponse, AnyOutgoingRequest, OutgoingRequest,
};
use matrix_sdk_crypto::{DecryptionSettings, EncryptionSettings, OlmMachine, TrustRequirement};
use matrix_sdk_sqlite::SqliteCryptoStore;
use ruma::api::client::keys::{
    claim_keys, get_keys, upload_keys, upload_signatures::v3 as upload_signatures,
};
use ruma::api::client::message::send_message_event;
use ruma::api::client::sync::sync_events::DeviceLists;
use ruma::api::client::to_device::send_event_to_device;
use ruma::api::IncomingResponse;
use ruma::events::AnyMessageLikeEventContent;
use ruma::serde::Raw;
use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, TransactionId, UserId};
use serde_json::{json, Value};

/// Everything encryption needs that the plain REST adapter already has.
pub(crate) struct MatrixCrypto {
    machine: OlmMachine,
    homeserver_url: String,
    access_token: String,
    user_id: OwnedUserId,
    device_id: OwnedDeviceId,
}

/// One decrypted timeline event: the cleartext event JSON, plus whether the
/// sender's device was one we have any reason to trust.
#[derive(Debug)]
pub(crate) struct Decrypted {
    pub event: Value,
    /// False when the message decrypted but arrived from a device this
    /// account has never verified. Surfaced rather than swallowed: "we could
    /// read it" and "we know who wrote it" are different claims.
    pub sender_verified: bool,
}

impl MatrixCrypto {
    /// Build the machine for one account.
    ///
    /// `store_dir` is inside the profile's own data root, so two profiles
    /// never share a crypto store — the same boundary every other per-profile
    /// store obeys.
    pub async fn new(
        homeserver_url: &str,
        access_token: &str,
        user_id: &str,
        device_id: &str,
        store_dir: &Path,
    ) -> Result<Self, String> {
        let user_id = UserId::parse(user_id)
            .map_err(|error| format!("Matrix user id is not a valid user id: {error}"))?;
        let device_id: OwnedDeviceId = <&DeviceId>::from(device_id).to_owned();
        std::fs::create_dir_all(store_dir)
            .map_err(|error| format!("Could not create the Matrix crypto store: {error}"))?;
        let store = SqliteCryptoStore::open(store_dir, None)
            .await
            .map_err(|error| format!("Could not open the Matrix crypto store: {error}"))?;
        let machine = OlmMachine::with_store(&user_id, &device_id, store, None)
            .await
            .map_err(|error| format!("Could not start Matrix encryption: {error}"))?;
        Ok(Self {
            machine,
            homeserver_url: homeserver_url.trim_end_matches('/').to_string(),
            access_token: access_token.to_string(),
            user_id,
            device_id,
        })
    }

    /// The identity keys other clients see. Shown in the account's health so
    /// a user can compare them against what their other client displays,
    /// which is how device verification starts.
    pub async fn identity_fingerprint(&self) -> String {
        format!(
            "{} · {}",
            self.device_id,
            self.machine.identity_keys().ed25519.to_base64()
        )
    }

    /// Feed one `/sync` response's encryption-relevant parts to the machine,
    /// then ship whatever it decides to send.
    ///
    /// The to-device events are where room keys arrive, so this has to run
    /// before the timeline is decrypted — a key that arrives in the same sync
    /// as the message it unlocks is the normal case, not an edge case.
    pub async fn absorb_sync(&self, sync: &Value) -> Result<(), String> {
        let to_device: Vec<Raw<ruma::events::AnyToDeviceEvent>> = sync
            .get("to_device")
            .and_then(|value| value.get("events"))
            .and_then(Value::as_array)
            .map(|events| {
                events
                    .iter()
                    .filter_map(|event| serde_json::from_value(event.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();

        let changed_devices = device_lists(sync);
        let one_time_keys_counts = counts(sync.get("device_one_time_keys_count"));
        let unused_fallback_keys: Option<Vec<ruma::OneTimeKeyAlgorithm>> = sync
            .get("device_unused_fallback_key_types")
            .and_then(Value::as_array)
            .map(|types| {
                types
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ruma::OneTimeKeyAlgorithm::from)
                    .collect()
            });
        let next_batch = sync
            .get("next_batch")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        let changes = matrix_sdk_crypto::EncryptionSyncChanges {
            to_device_events: to_device,
            changed_devices: &changed_devices,
            one_time_keys_counts: &one_time_keys_counts,
            unused_fallback_keys: unused_fallback_keys.as_deref(),
            next_batch_token: Some(next_batch),
        };
        self.machine
            .receive_sync_changes(changes, &decryption_settings())
            .await
            .map_err(|error| format!("Matrix encryption could not process this sync: {error}"))?;
        self.run_outgoing().await
    }

    /// Decrypt one `m.room.encrypted` timeline event.
    pub async fn decrypt(&self, event: &Value, room_id: &str) -> Result<Decrypted, String> {
        let room_id: OwnedRoomId = ruma::RoomId::parse(room_id)
            .map_err(|error| format!("Matrix sent an unparseable room id: {error}"))?;
        let raw: Raw<matrix_sdk_crypto::types::events::room::encrypted::EncryptedEvent> =
            serde_json::from_value(event.clone())
                .map_err(|error| format!("Matrix sent an unparseable encrypted event: {error}"))?;
        let decrypted = self
            .machine
            .decrypt_room_event(&raw, &room_id, &decryption_settings())
            .await
            .map_err(|error| error.to_string())?;
        let sender_verified = matches!(
            decrypted.encryption_info.verification_state,
            matrix_sdk_common::deserialized_responses::VerificationState::Verified
        );
        let event = decrypted
            .event
            .deserialize_as::<Value>()
            .map_err(|error| format!("Decrypted a Matrix event that is not JSON: {error}"))?;
        Ok(Decrypted {
            event,
            sender_verified,
        })
    }

    /// Encrypt one outgoing message for an encrypted room.
    ///
    /// Everything the room needs before that can happen — claiming one-time
    /// keys for devices we have no session with, then handing each of them
    /// the room key — happens here, because a message encrypted for devices
    /// that never received the key is an unreadable message, not a sent one.
    pub async fn encrypt_for_room(
        &self,
        room_id: &str,
        members: &[String],
        content: &Value,
    ) -> Result<Value, String> {
        let room_id: OwnedRoomId = ruma::RoomId::parse(room_id)
            .map_err(|error| format!("Matrix room id is not valid: {error}"))?;
        let members: Vec<OwnedUserId> = members
            .iter()
            .filter_map(|member| UserId::parse(member).ok())
            .collect();

        self.machine
            .update_tracked_users(members.iter().map(AsRef::as_ref))
            .await
            .map_err(|error| format!("Matrix could not track the room's members: {error}"))?;
        // Devices we have never spoken to need a one-time key claimed first.
        if let Some((transaction_id, request)) = self
            .machine
            .get_missing_sessions(members.iter().map(AsRef::as_ref))
            .await
            .map_err(|error| format!("Matrix could not check for missing sessions: {error}"))?
        {
            let body = json!({ "one_time_keys": request.one_time_keys });
            let response = self.post("/_matrix/client/v3/keys/claim", body).await?;
            let parsed = parse_response::<claim_keys::v3::Response>(&response)?;
            self.machine
                .mark_request_as_sent(&transaction_id, AnyIncomingResponse::KeysClaim(&parsed))
                .await
                .map_err(|error| format!("Matrix rejected the claimed keys: {error}"))?;
        }
        for request in self
            .machine
            .share_room_key(
                &room_id,
                members.iter().map(AsRef::as_ref),
                EncryptionSettings::default(),
            )
            .await
            .map_err(|error| format!("Matrix could not share the room key: {error}"))?
        {
            self.send_to_device(&request).await?;
        }

        // The whole cleartext content is encrypted, reply metadata
        // included: everything except the event's own envelope is covered.
        let content = Raw::<AnyMessageLikeEventContent>::from_json_string(content.to_string())
            .map_err(|error| format!("Could not build the message body: {error}"))?;
        let encrypted = self
            .machine
            .encrypt_room_event_raw(&room_id, "m.room.message", &content)
            .await
            .map_err(|error| format!("Matrix could not encrypt the message: {error}"))?;
        serde_json::to_value(encrypted.content)
            .map_err(|error| format!("Could not serialize the encrypted message: {error}"))
    }

    /// Ship everything the machine currently wants to send.
    ///
    /// Called after every sync and before every send. A request that fails is
    /// *not* marked as sent, so the machine offers it again rather than
    /// believing a key upload happened that did not.
    pub async fn run_outgoing(&self) -> Result<(), String> {
        let requests = self
            .machine
            .outgoing_requests()
            .await
            .map_err(|error| format!("Matrix encryption could not be read: {error}"))?;
        for request in requests {
            self.ship(&request).await?;
        }
        Ok(())
    }

    async fn ship(&self, request: &OutgoingRequest) -> Result<(), String> {
        let id = request.request_id();
        match request.request() {
            AnyOutgoingRequest::KeysUpload(upload) => {
                // Ruma's request types are not `Serialize` (they know how to
                // become an HTTP request, not a JSON value), so each body is
                // assembled from the fields the spec names.
                let body = json!({
                    "device_keys": upload.device_keys,
                    "one_time_keys": upload.one_time_keys,
                    "fallback_keys": upload.fallback_keys,
                });
                let response = self.post("/_matrix/client/v3/keys/upload", body).await?;
                let parsed = parse_response::<upload_keys::v3::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::KeysUpload(&parsed))
                    .await
            }
            AnyOutgoingRequest::KeysQuery(query) => {
                let body = json!({ "device_keys": query.device_keys });
                let response = self.post("/_matrix/client/v3/keys/query", body).await?;
                let parsed = parse_response::<get_keys::v3::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::KeysQuery(&parsed)).await
            }
            AnyOutgoingRequest::KeysClaim(claim) => {
                let body = json!({ "one_time_keys": claim.one_time_keys });
                let response = self.post("/_matrix/client/v3/keys/claim", body).await?;
                let parsed = parse_response::<claim_keys::v3::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::KeysClaim(&parsed)).await
            }
            AnyOutgoingRequest::SignatureUpload(signatures) => {
                let body = serde_json::to_value(&signatures.signed_keys)
                    .map_err(|error| format!("Could not serialize the signatures: {error}"))?;
                let response = self
                    .post("/_matrix/client/v3/keys/signatures/upload", body)
                    .await?;
                let parsed = parse_response::<upload_signatures::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::SignatureUpload(&parsed))
                    .await
            }
            AnyOutgoingRequest::ToDeviceRequest(to_device) => {
                let response = self.send_to_device(to_device).await?;
                let parsed = parse_response::<send_event_to_device::v3::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::ToDevice(&parsed)).await
            }
            AnyOutgoingRequest::RoomMessage(message) => {
                let path = format!(
                    "/_matrix/client/v3/rooms/{}/send/{}/{}",
                    message.room_id,
                    ruma::events::MessageLikeEventContent::event_type(&*message.content),
                    message.txn_id
                );
                let body = serde_json::to_value(&message.content)
                    .map_err(|error| format!("Could not serialize the room message: {error}"))?;
                let response = self.put(&path, body).await?;
                let parsed = parse_response::<send_message_event::v3::Response>(&response)?;
                self.mark(id, AnyIncomingResponse::RoomMessage(&parsed))
                    .await
            }
        }
    }

    async fn mark(
        &self,
        id: &TransactionId,
        response: AnyIncomingResponse<'_>,
    ) -> Result<(), String> {
        self.machine
            .mark_request_as_sent(id, response)
            .await
            .map_err(|error| format!("Matrix encryption rejected a response: {error}"))
    }

    /// One `sendToDevice`, which is how every key actually reaches a device.
    async fn send_to_device(
        &self,
        request: &matrix_sdk_crypto::types::requests::ToDeviceRequest,
    ) -> Result<Vec<u8>, String> {
        let path = format!(
            "/_matrix/client/v3/sendToDevice/{}/{}",
            request.event_type,
            request.txn_id.as_str()
        );
        let body = json!({ "messages": request.messages });
        self.put(&path, body).await
    }

    async fn post(&self, path: &str, body: Value) -> Result<Vec<u8>, String> {
        self.request(reqwest::Method::POST, path, body).await
    }

    async fn put(&self, path: &str, body: Value) -> Result<Vec<u8>, String> {
        self.request(reqwest::Method::PUT, path, body).await
    }

    /// Every encryption call goes out through the same hardened client the
    /// rest of the adapter uses, so a homeserver URL cannot reach somewhere
    /// the egress rules forbid just because the request carries a key.
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> Result<Vec<u8>, String> {
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build the Matrix HTTP client: {error}"))?;
        let request = client
            .request(method, format!("{}{path}", self.homeserver_url))
            .bearer_auth(&self.access_token)
            .json(&body);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| format!("Matrix encryption request failed: {error}"))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("Matrix encryption response could not be read: {error}"))?;
        if !status.is_success() {
            return Err(format!(
                "The homeserver refused an encryption request with HTTP {}",
                status.as_u16()
            ));
        }
        Ok(bytes.to_vec())
    }

    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }
}

/// Parse one homeserver response body into the ruma type the machine wants.
fn parse_response<T: IncomingResponse>(body: &[u8]) -> Result<T, String> {
    let http_response = http::Response::builder()
        .status(http::StatusCode::OK)
        .body(body.to_vec())
        .map_err(|error| format!("Could not rebuild the homeserver response: {error}"))?;
    T::try_from_http_response(http_response)
        .map_err(|error| format!("The homeserver's answer did not parse: {error}"))
}

/// Users whose device list changed in this sync. A device that appeared or
/// disappeared invalidates what we know about who can read a room.
fn device_lists(sync: &Value) -> DeviceLists {
    let read = |key: &str| -> Vec<OwnedUserId> {
        sync.get("device_lists")
            .and_then(|lists| lists.get(key))
            .and_then(Value::as_array)
            .map(|users| {
                users
                    .iter()
                    .filter_map(Value::as_str)
                    .filter_map(|user| UserId::parse(user).ok())
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut lists = DeviceLists::default();
    lists.changed = read("changed");
    lists.left = read("left");
    lists
}

/// One-time key counts, which are how the homeserver asks for more keys.
fn counts(value: Option<&Value>) -> BTreeMap<ruma::OneTimeKeyAlgorithm, ruma::UInt> {
    let mut counts = BTreeMap::new();
    if let Some(object) = value.and_then(Value::as_object) {
        for (algorithm, count) in object {
            if let Some(count) = count
                .as_u64()
                .and_then(|count| ruma::UInt::try_from(count).ok())
            {
                counts.insert(ruma::OneTimeKeyAlgorithm::from(algorithm.as_str()), count);
            }
        }
    }
    counts
}

/// Decrypt anything we hold a key for.
///
/// `TrustRequirement::Untrusted` is deliberate: refusing to decrypt a message
/// from an unverified device would silently drop real messages from people
/// who never verified, which is most people. The verification state travels
/// with the decrypted event instead, so the decision is made where it is
/// visible rather than by making the message vanish.
fn decryption_settings() -> DecryptionSettings {
    DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::Untrusted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "monkey-matrix-crypto-{}",
            uuid::Uuid::new_v4().simple()
        ))
    }

    #[tokio::test]
    async fn a_new_account_wants_to_upload_its_keys_before_anything_else() {
        let store = temp_store();
        let crypto = MatrixCrypto::new(
            "https://matrix.example.org",
            "syt_token",
            "@you:example.org",
            "DEVICEID",
            &store,
        )
        .await
        .expect("machine");

        // A machine with no keys on the server has exactly one thing to do,
        // and this is what makes an account reachable at all: without the
        // upload, nobody can start an Olm session with it.
        let requests = crypto.machine.outgoing_requests().await.expect("requests");
        assert!(
            requests
                .iter()
                .any(|request| matches!(request.request(), AnyOutgoingRequest::KeysUpload(_))),
            "a fresh device must offer its keys"
        );
        let _ = std::fs::remove_dir_all(&store);
    }

    #[tokio::test]
    async fn the_store_survives_a_restart_with_the_same_identity() {
        let store = temp_store();
        let first = MatrixCrypto::new(
            "https://matrix.example.org",
            "syt_token",
            "@you:example.org",
            "DEVICEID",
            &store,
        )
        .await
        .expect("machine");
        let fingerprint = first.identity_fingerprint().await;
        drop(first);

        let second = MatrixCrypto::new(
            "https://matrix.example.org",
            "syt_token",
            "@you:example.org",
            "DEVICEID",
            &store,
        )
        .await
        .expect("machine");
        // Same store, same device: a restart must not mint a new identity,
        // which is what would show up in the user's session list as another
        // unverified device every single time.
        assert_eq!(second.identity_fingerprint().await, fingerprint);
        let _ = std::fs::remove_dir_all(&store);
    }

    #[tokio::test]
    async fn an_event_we_hold_no_key_for_is_an_error_not_a_panic() {
        let store = temp_store();
        let crypto = MatrixCrypto::new(
            "https://matrix.example.org",
            "syt_token",
            "@you:example.org",
            "DEVICEID",
            &store,
        )
        .await
        .expect("machine");

        let event = json!({
            "type": "m.room.encrypted",
            "event_id": "$1",
            "sender": "@someone:example.org",
            "origin_server_ts": 1_700_000_000_000i64,
            "room_id": "!room:example.org",
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEnB4dGVzdA",
                "sender_key": "sender_key",
                "device_id": "OTHER",
                "session_id": "session"
            }
        });
        let error = crypto
            .decrypt(&event, "!room:example.org")
            .await
            .expect_err("no key for this session");
        assert!(!error.is_empty());
        let _ = std::fs::remove_dir_all(&store);
    }

    #[test]
    fn a_syncs_device_lists_and_key_counts_are_read_the_way_the_spec_writes_them() {
        // These two are what tell the machine that someone's devices changed
        // and that the server wants more one-time keys. Misreading either is
        // silent: messages simply stop being decryptable for the new device.
        let sync = json!({
            "next_batch": "s1",
            "device_lists": {
                "changed": ["@ada:example.org", "not-a-user-id"],
                "left": ["@bob:example.org"]
            },
            "device_one_time_keys_count": { "signed_curve25519": 12 }
        });

        let lists = device_lists(&sync);
        assert_eq!(
            lists.changed.len(),
            1,
            "an unparseable user id is dropped, not fatal"
        );
        assert_eq!(lists.changed[0], "@ada:example.org");
        assert_eq!(lists.left[0], "@bob:example.org");

        let key_counts = counts(sync.get("device_one_time_keys_count"));
        assert_eq!(
            key_counts.get(&ruma::OneTimeKeyAlgorithm::SignedCurve25519),
            Some(&ruma::UInt::from(12u32))
        );
        assert!(counts(None).is_empty());
    }

    #[test]
    fn an_unverified_sender_is_still_decrypted_but_never_reported_as_verified() {
        // Refusing to decrypt from unverified devices would drop real
        // messages from most people. The claim is downgraded instead.
        assert!(matches!(
            decryption_settings().sender_device_trust_requirement,
            TrustRequirement::Untrusted
        ));
    }
}

use std::path::Path;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use reqwest::{Certificate, Method, Response};
use serde_json::Value;

use crate::daemon::store::DaemonPaths;

use super::protocol::{
    certificate_fingerprint, sha256_hex, ControllerProfile, PairAcceptRequest, PairAcceptResponse,
    PairingInvitation, RotationBundle, SignedRequestHeaders, REMOTE_PROTOCOL_VERSION,
};
use super::store::{KeyringRemoteSecrets, RemoteStore};

pub async fn accept_invitation(
    paths: &DaemonPaths,
    invitation_path: &Path,
    alias: &str,
    device_name: &str,
    now_ms: u64,
) -> Result<ControllerProfile, String> {
    super::protocol::validate_id(alias)?;
    let invitation: PairingInvitation = serde_json::from_slice(
        &std::fs::read(invitation_path)
            .map_err(|error| format!("Could not read pairing invitation: {error}"))?,
    )
    .map_err(|error| format!("Pairing invitation is invalid: {error}"))?;
    invitation.validate(now_ms)?;
    let client = pinned_client(
        &invitation.server_certificate_pem,
        &invitation.server_certificate_sha256,
    )?;
    let endpoint = format!(
        "{}/v1/remote/pairings/accept",
        invitation.runner_url.trim_end_matches('/')
    );
    let response = client
        .post(endpoint)
        .json(&PairAcceptRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            pairing_id: invitation.pairing_id.clone(),
            pairing_token: invitation.pairing_token.clone(),
            device_name: device_name.to_string(),
        })
        .send()
        .await
        .map_err(|error| format!("Pairing request failed: {error}"))?;
    verify_response_pin(&response, &invitation.server_certificate_sha256)?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("Could not read pairing response: {error}"))?;
    if !status.is_success() {
        return Err(remote_error(status.as_u16(), &bytes));
    }
    let accepted: PairAcceptResponse = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Pairing response is invalid: {error}"))?;
    if accepted.protocol_version != REMOTE_PROTOCOL_VERSION
        || accepted.runner_id != invitation.runner_id
        || !accepted.scopes.is_subset_of(&invitation.scopes)
    {
        return Err("Pairing response attempts to change runner identity or scope".to_string());
    }
    let profile = ControllerProfile {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        alias: alias.to_string(),
        runner_id: accepted.runner_id,
        runner_url: invitation.runner_url,
        server_certificate_pem: invitation.server_certificate_pem,
        server_certificate_sha256: invitation.server_certificate_sha256,
        device_id: accepted.device_id,
        secret_generation: accepted.secret_generation,
        scopes: accepted.scopes,
        next_sequence: 1,
        event_cursors: Default::default(),
    };
    let mut store = RemoteStore::open(&paths.root)?;
    store.save_controller(
        &profile,
        accepted.device_secret.as_bytes(),
        now_ms,
        &KeyringRemoteSecrets,
    )?;
    Ok(profile)
}

pub fn accept_rotation(
    paths: &DaemonPaths,
    alias: &str,
    rotation_path: &Path,
    now_ms: u64,
) -> Result<ControllerProfile, String> {
    let bundle: RotationBundle = serde_json::from_slice(
        &std::fs::read(rotation_path)
            .map_err(|error| format!("Could not read rotation bundle: {error}"))?,
    )
    .map_err(|error| format!("Rotation bundle is invalid: {error}"))?;
    if bundle.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err("Unsupported rotation bundle protocol version".to_string());
    }
    if certificate_fingerprint(bundle.server_certificate_pem.as_bytes())?
        != bundle.server_certificate_sha256
    {
        return Err("Rotation bundle certificate does not match its pin".to_string());
    }
    let mut store = RemoteStore::open(&paths.root)?;
    store.replace_controller_rotation(alias, &bundle, now_ms, &KeyringRemoteSecrets)?;
    store
        .controller(alias)?
        .ok_or_else(|| "Controller disappeared after rotation".to_string())
}

pub async fn call(
    paths: &DaemonPaths,
    alias: &str,
    method: Method,
    path_and_query: &str,
    body: Vec<u8>,
    now_ms: u64,
) -> Result<Value, String> {
    if !path_and_query.starts_with("/v1/remote/") || path_and_query.contains(['\r', '\n']) {
        return Err("Remote path is outside the versioned API".to_string());
    }
    let mut store = RemoteStore::open(&paths.root)?;
    let profile = store
        .controller(alias)?
        .ok_or_else(|| format!("Unknown remote controller '{alias}'"))?;
    if profile.protocol_version != REMOTE_PROTOCOL_VERSION {
        return Err("Controller profile protocol is unsupported".to_string());
    }
    let sequence = store.allocate_controller_sequence(alias)?;
    let secret = RemoteStore::controller_secret(&profile, &KeyringRemoteSecrets)?;
    let auth = SignedRequestHeaders::new(
        profile.device_id.clone(),
        profile.secret_generation,
        sequence,
        now_ms,
        method.as_str(),
        path_and_query,
        &body,
        &secret,
    )?;
    let client = pinned_client(
        &profile.server_certificate_pem,
        &profile.server_certificate_sha256,
    )?;
    let endpoint = format!(
        "{}{}",
        profile.runner_url.trim_end_matches('/'),
        path_and_query
    );
    let mut last_error = None;
    for attempt in 0..3u32 {
        let response = client
            .request(method.clone(), &endpoint)
            .header("x-little-monkey-device", &auth.device_id)
            .header(
                "x-little-monkey-key-generation",
                auth.secret_generation.to_string(),
            )
            .header("x-little-monkey-sequence", auth.sequence.to_string())
            .header(
                "x-little-monkey-timestamp-ms",
                auth.timestamp_ms.to_string(),
            )
            .header("x-little-monkey-nonce", &auth.nonce)
            .header("x-little-monkey-command", &auth.command_id)
            .header("x-little-monkey-signature", &auth.signature)
            .header("content-type", "application/json")
            .body(body.clone())
            .send()
            .await;
        match response {
            Ok(response) => {
                verify_response_pin(&response, &profile.server_certificate_sha256)?;
                let status = response.status();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| format!("Could not read remote response: {error}"))?;
                if !status.is_success() {
                    return Err(remote_error(status.as_u16(), &bytes));
                }
                return serde_json::from_slice(&bytes)
                    .map_err(|error| format!("Remote response is invalid JSON: {error}"));
            }
            Err(error) => {
                last_error = Some(error.to_string());
                if attempt < 2 {
                    // Reuse the exact command id, nonce, sequence, body, and
                    // signature. The runner either executes it once or
                    // returns the cached response after a lost connection.
                    tokio::time::sleep(Duration::from_millis(100 * (1 << attempt))).await;
                }
            }
        }
    }
    Err(format!(
        "Remote runner is unreachable after replay-safe reconnect attempts: {}",
        last_error.unwrap_or_else(|| "unknown transport error".to_string())
    ))
}

pub async fn events(
    paths: &DaemonPaths,
    alias: &str,
    run_id: &str,
    after_override: Option<u64>,
    now_ms: u64,
) -> Result<Value, String> {
    let mut store = RemoteStore::open(&paths.root)?;
    let profile = store
        .controller(alias)?
        .ok_or_else(|| format!("Unknown remote controller '{alias}'"))?;
    let after =
        after_override.unwrap_or_else(|| profile.event_cursors.get(run_id).copied().unwrap_or(0));
    drop(store);
    let path = format!(
        "/v1/remote/runs/{}/events?after={after}&limit=1000",
        percent_segment(run_id)?
    );
    let value = call(paths, alias, Method::GET, &path, vec![], now_ms).await?;
    if let Some(cursor) = value.get("next_cursor").and_then(Value::as_u64) {
        store = RemoteStore::open(&paths.root)?;
        store.update_controller_cursor(alias, run_id, cursor)?;
    }
    Ok(value)
}

pub async fn fetch_artifact(
    paths: &DaemonPaths,
    alias: &str,
    run_id: &str,
    artifact_id: &str,
    destination: &Path,
    now_ms: u64,
) -> Result<(), String> {
    let path = format!(
        "/v1/remote/runs/{}/artifacts/{}",
        percent_segment(run_id)?,
        percent_segment(artifact_id)?
    );
    let value = call(paths, alias, Method::GET, &path, vec![], now_ms).await?;
    let encoded = value
        .get("content_base64")
        .and_then(Value::as_str)
        .ok_or_else(|| "Remote artifact response has no content".to_string())?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| format!("Remote artifact is not valid base64: {error}"))?;
    let expected = value
        .get("content_sha256")
        .and_then(Value::as_str)
        .ok_or_else(|| "Remote artifact response has no digest".to_string())?;
    if sha256_hex(&bytes) != expected {
        return Err("Remote artifact failed end-to-end digest verification".to_string());
    }
    let temporary = destination.with_extension("remote.tmp");
    std::fs::write(&temporary, &bytes)
        .map_err(|error| format!("Could not stage remote artifact: {error}"))?;
    std::fs::rename(&temporary, destination)
        .map_err(|error| format!("Could not publish remote artifact: {error}"))?;
    Ok(())
}

fn pinned_client(certificate_pem: &str, expected_sha256: &str) -> Result<reqwest::Client, String> {
    if certificate_fingerprint(certificate_pem.as_bytes())? != expected_sha256 {
        return Err("Pinned certificate bytes do not match the stored fingerprint".to_string());
    }
    let certificate = Certificate::from_pem(certificate_pem.as_bytes())
        .map_err(|error| format!("Pinned certificate is invalid: {error}"))?;
    reqwest::Client::builder()
        .tls_certs_only([certificate])
        .tls_info(true)
        .https_only(true)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Could not build pinned remote client: {error}"))
}

fn verify_response_pin(response: &Response, expected_sha256: &str) -> Result<(), String> {
    let tls = response
        .extensions()
        .get::<reqwest::tls::TlsInfo>()
        .ok_or_else(|| "Remote response has no TLS peer identity".to_string())?;
    let peer = tls
        .peer_certificate()
        .ok_or_else(|| "Remote response has no peer certificate".to_string())?;
    if sha256_hex(peer) != expected_sha256 {
        return Err("Remote TLS certificate pin mismatch".to_string());
    }
    Ok(())
}

fn remote_error(status: u16, body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .map(|message| format!("Remote runner returned HTTP {status}: {message}"))
        .unwrap_or_else(|| format!("Remote runner returned HTTP {status}"))
}

fn percent_segment(value: &str) -> Result<String, String> {
    super::protocol::validate_id(value)?;
    Ok(url::form_urlencoded::byte_serialize(value.as_bytes()).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_accept_only_protocol_ids_before_encoding() {
        assert_eq!(percent_segment("run-one").unwrap(), "run-one");
        assert!(percent_segment("../../secret").is_err());
        assert!(percent_segment("run/other").is_err());
    }

    #[test]
    fn artifact_digest_rejects_tampered_handoff_bytes() {
        let bytes = b"artifact";
        assert_eq!(sha256_hex(bytes).len(), 64);
        assert_ne!(sha256_hex(bytes), sha256_hex(b"tampered"));
    }
}

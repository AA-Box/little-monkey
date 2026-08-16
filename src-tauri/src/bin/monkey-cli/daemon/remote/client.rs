use std::path::Path;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use little_monkey_lib::egress;
use reqwest::{Certificate, Method, Response};
use serde_json::Value;

use crate::daemon::store::DaemonPaths;

use super::protocol::{
    certificate_fingerprint, legacy_capabilities, sha256_hex, ControllerProfile, PairAcceptRequest,
    PairAcceptResponse, PairingInvitation, RotationBundle, SignedRequestHeaders,
    REMOTE_PROTOCOL_VERSION,
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
    // Metered like every other outbound request. The TLS pin below still works:
    // the meter rebuilds the response from its own parts, extensions included, so
    // `reqwest::tls::TlsInfo` survives.
    let response = egress::send(client.post(endpoint).json(&PairAcceptRequest {
        protocol_version: REMOTE_PROTOCOL_VERSION,
        pairing_id: invitation.pairing_id.clone(),
        pairing_token: invitation.pairing_token.clone(),
        device_name: device_name.to_string(),
        // The CLI controller never down-selects: omitting the subset
        // requests the invitation's complete capability grant.
        requested_capabilities: None,
    }))
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
    // Resolve the empty-set legacy convention on both sides before comparing,
    // mirroring the rotation path: a runner must not be able to hand the
    // controller a wider capability grant than the invitation carried.
    let invited_capabilities = if invitation.capabilities.is_empty() {
        legacy_capabilities(&invitation.scopes)
    } else {
        invitation.capabilities.clone()
    };
    let granted_capabilities = if accepted.capabilities.is_empty() {
        legacy_capabilities(&accepted.scopes)
    } else {
        accepted.capabilities.clone()
    };
    if accepted.protocol_version != REMOTE_PROTOCOL_VERSION
        || accepted.runner_id != invitation.runner_id
        || !accepted.scopes.is_subset_of(&invitation.scopes)
        || !granted_capabilities.is_subset(&invited_capabilities)
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
        capabilities: granted_capabilities,
        next_sequence: 1,
        event_cursors: Default::default(),
        last_seen_at_ms: None,
        peer_advertised: Default::default(),
        peer_requested: Default::default(),
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
        let response = egress::send(
            client
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
                // Cloned per attempt because a retry re-sends it, which the meter
                // charges again — the bytes really do leave the machine twice.
                .body(body.clone()),
        )
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

// --- Roadmap K17: the placement plane, from the asking side ----------------

/// Asks one node to describe itself and stores the answer.
///
/// The store write is what makes `last_seen_at_ms` meaningful: it happens only
/// on a successful answer, so a failed probe leaves the previous timestamp
/// alone and `node_placement::liveness` sees the silence grow rather than
/// resetting on every attempt.
pub async fn refresh_node(
    paths: &DaemonPaths,
    alias: &str,
    now_ms: u64,
) -> Result<little_monkey_lib::node_placement::NodeDescriptor, String> {
    let value = call(paths, alias, Method::GET, "/v1/remote/node", vec![], now_ms).await?;
    let descriptor: little_monkey_lib::node_placement::NodeDescriptor =
        serde_json::from_value(value)
            .map_err(|error| format!("Node descriptor is invalid: {error}"))?;
    descriptor.validate()?;
    RemoteStore::open(&paths.root)?.save_node(alias, &descriptor, now_ms)?;
    Ok(descriptor)
}

/// The cheap liveness probe. Refreshes `last_seen_at_ms` without making the node
/// re-measure its hardware.
///
/// The node's queue state *is* refreshed here, because that is the part that
/// actually moves between probes — and a placer ranking on a two-minute-old
/// "accepting" flag would keep choosing a node that has since filled up.
pub async fn probe_node(
    paths: &DaemonPaths,
    alias: &str,
    now_ms: u64,
) -> Result<little_monkey_lib::node_placement::NodeHealth, String> {
    let value = call(
        paths,
        alias,
        Method::GET,
        "/v1/remote/node/health",
        vec![],
        now_ms,
    )
    .await?;
    let health: little_monkey_lib::node_placement::NodeHealth = serde_json::from_value(value)
        .map_err(|error| format!("Node health is invalid: {error}"))?;
    let mut store = RemoteStore::open(&paths.root)?;
    if let Some((_, descriptor, _)) = store
        .nodes()?
        .into_iter()
        .find(|(stored_alias, _, _)| stored_alias == alias)
    {
        if descriptor.runner_id != health.runner_id {
            // The alias now points at a different machine. Refusing here rather
            // than quietly re-pointing is the whole reason the runner id is on
            // the health response: a residency rule proved against one host must
            // not silently carry over to another.
            return Err(format!(
                "Node '{alias}' answered as runner '{}' but is recorded as '{}'; re-pair it",
                health.runner_id, descriptor.runner_id
            ));
        }
        store.save_node(
            alias,
            &little_monkey_lib::node_placement::NodeDescriptor {
                accepting: health.accepting,
                queue_depth: health.queue_depth,
                queue_capacity: health.queue_capacity,
                ..descriptor
            },
            now_ms,
        )?;
    }
    Ok(health)
}

/// Ships one frozen `RunSpec` to a node.
pub async fn place_run(
    paths: &DaemonPaths,
    alias: &str,
    request: &little_monkey_lib::node_placement::PlaceRunRequest,
    now_ms: u64,
) -> Result<little_monkey_lib::node_placement::PlaceRunResponse, String> {
    request.validate()?;
    let body = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    let value = call(
        paths,
        alias,
        Method::POST,
        "/v1/remote/node/runs",
        body,
        now_ms,
    )
    .await?;
    serde_json::from_value(value).map_err(|error| format!("Placement response is invalid: {error}"))
}

/// Reads one placement back from the node that holds it.
pub async fn placed_status(
    paths: &DaemonPaths,
    alias: &str,
    submitted_run_id: &str,
    now_ms: u64,
) -> Result<little_monkey_lib::node_placement::PlacedRunStatus, String> {
    let path = format!(
        "/v1/remote/node/runs/{}",
        percent_segment(submitted_run_id)?
    );
    let value = call(paths, alias, Method::GET, &path, vec![], now_ms).await?;
    serde_json::from_value(value).map_err(|error| format!("Placed run status is invalid: {error}"))
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
        // A silence budget, not a deadline for the whole request. The 30 seconds
        // this replaces covered the body, and `fetch_artifact` reads a whole
        // artifact out of one JSON response: the runner inlines it as
        // `content_base64`, so a `max_artifact_bytes` at its 32 MiB ceiling
        // arrives as ~43 MiB of JSON through `call`'s single `bytes()` read.
        // Thirty seconds for that is 1.4 MB/s sustained, so an artifact fetch over
        // anything slower failed mid-body and reported a transport error. Reset on
        // every read instead, a peer that stops sending is still cut off while one
        // still sending is not.
        .read_timeout(Duration::from_secs(30))
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

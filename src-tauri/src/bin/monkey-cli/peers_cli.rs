//! `monkey peers` — pair with another Little Monkey installation, say something
//! to it, and read what came back.
//!
//! Two directions, deliberately separate. *Inbound* peers are installations
//! paired **into** this one: they hold a signing secret for this node and their
//! grants decide what they may ask for here. *Outbound* peers are installations
//! this one is paired **with**, reached by alias. Pairing in one direction does
//! not create the other; an operator who wants both runs the flow twice, once
//! from each side, which is exactly what "no central federation" costs.
//!
//! Every subcommand here is what the desktop's Peers settings calls through the
//! typed bridge, so the rules live in one place.

use std::collections::BTreeSet;
use std::path::PathBuf;

use little_monkey_lib::peers::{
    PeerCapability, PeerEnvelope, PeerMessageKind, DEFAULT_HOP_LIMIT, MAX_BODY_BYTES,
};

use crate::daemon::peer_ingress::PEER_TASK_RECIPE;
use crate::daemon::remote::protocol::{is_peer_only, DeviceCapability};
use crate::daemon::remote::store::{KeyringRemoteSecrets, RemoteStore};
use crate::daemon::store::{DaemonPaths, DaemonStore};

/// How long a peer envelope stays worth acting on, unless the caller says
/// otherwise. Ten minutes: long enough to survive a restart on the far side,
/// short enough that nothing runs hours after it was asked for.
const DEFAULT_TTL_MS: i64 = 10 * 60 * 1_000;

/// `monkey peers <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum PeersCmd {
    /// Write a one-time invitation that lets another installation pair with
    /// this one as a peer. The invitation grants peer standing and nothing
    /// else — no runs, no approvals, no desktop.
    Invite {
        /// Name for the peer in listings on this side.
        label: String,
        /// Grants to offer: message, task, artifact (comma-separated).
        #[arg(long, default_value = "message")]
        allow: String,
        #[arg(long, default_value_t = 60)]
        expires_minutes: u64,
        /// Where to write the invitation file.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Accept another installation's invitation, so this one can talk to it.
    Accept {
        invitation: PathBuf,
        /// Local name for the peer.
        alias: String,
        #[arg(long)]
        json: bool,
    },
    /// Peers in both directions, with their grants and their state.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Change what an inbound peer may ask for. Replaces its peer grants;
    /// an empty list leaves it paired but unable to ask for anything.
    Grant {
        device_id: String,
        /// message, task, artifact (comma-separated, empty for none).
        #[arg(long, default_value = "")]
        allow: String,
        #[arg(long)]
        json: bool,
    },
    /// Revoke an inbound peer and delete the threads it left behind.
    Revoke {
        device_id: String,
        #[arg(long, default_value = "Revoked by the operator")]
        reason: String,
    },
    /// Send a message or a task request to a peer this installation is paired
    /// with.
    Send {
        /// The alias given at `accept` time.
        alias: String,
        text: String,
        /// Conversation to continue. A new one is minted when absent.
        #[arg(long)]
        thread: Option<String>,
        /// Ask for work rather than just saying something. The peer decides
        /// whether to run it, under its own recipe and permissions.
        #[arg(long)]
        task: bool,
        /// Your own handle for correlating the result.
        #[arg(long)]
        correlation: Option<String>,
        /// Artifact ids from this installation's content store to hand over.
        /// The bytes are uploaded first, then referenced; the peer never
        /// receives a path and never reaches into this machine.
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Read a thread on the peer that owns it, including results.
    Thread {
        alias: String,
        thread_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Threads inbound peers opened here, and what happened to them.
    Threads {
        #[arg(long)]
        peer: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// Replace an inbound peer's signing key. The old one stops working
    /// immediately; hand the bundle to that peer out of band.
    Rotate {
        device_id: String,
        /// Where to write the replacement bundle.
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Take up the bundle a peer produced when it rotated this pairing.
    AcceptRotation {
        bundle: PathBuf,
        alias: String,
        #[arg(long)]
        json: bool,
    },
    /// Ask a peer whether it is there, and refresh what each side knows about
    /// the other. Signed and certificate-pinned like every other peer call.
    Status {
        alias: String,
        /// Peer grants to ask this peer for (comma-separated). Recorded on both
        /// sides for the operator to act on; asking grants nothing.
        #[arg(long, default_value = "")]
        request: String,
        #[arg(long)]
        json: bool,
    },
    /// Clear one peer's traffic here. A revoked pairing also loses its retained
    /// peer grants, so it stops occupying the Peers screen.
    Clear {
        device_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Forget an outbound peer: its profile and the key used to reach it.
    Forget { alias: String },
}

pub async fn dispatch(command: &PeersCmd) -> Result<(), String> {
    match command {
        PeersCmd::Invite {
            label,
            allow,
            expires_minutes,
            output,
            json,
        } => invite(label, allow, *expires_minutes, output, *json),
        PeersCmd::Accept {
            invitation,
            alias,
            json,
        } => accept(invitation, alias, *json).await,
        PeersCmd::List { json } => list(*json),
        PeersCmd::Grant {
            device_id,
            allow,
            json,
        } => grant(device_id, allow, *json),
        PeersCmd::Revoke { device_id, reason } => revoke(device_id, reason),
        PeersCmd::Send {
            alias,
            text,
            thread,
            task,
            correlation,
            artifacts,
            json,
        } => {
            send(
                alias,
                text,
                thread.as_deref(),
                *task,
                correlation.as_deref(),
                artifacts,
                *json,
            )
            .await
        }
        PeersCmd::Thread {
            alias,
            thread_id,
            json,
        } => thread(alias, thread_id, *json).await,
        PeersCmd::Threads { peer, limit, json } => threads(peer.as_deref(), *limit, *json),
        PeersCmd::Rotate {
            device_id,
            output,
            json,
        } => rotate(device_id, output, *json),
        PeersCmd::AcceptRotation {
            bundle,
            alias,
            json,
        } => accept_rotation(bundle, alias, *json),
        PeersCmd::Status {
            alias,
            request,
            json,
        } => status(alias, request, *json).await,
        PeersCmd::Clear { device_id, json } => clear(device_id, *json),
        PeersCmd::Forget { alias } => forget(alias),
    }
}

/// Parse `message,task,artifact` into grants.
fn parse_grants(allow: &str) -> Result<BTreeSet<DeviceCapability>, String> {
    let mut grants = BTreeSet::new();
    for token in allow.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let capability = match token {
            "message" => PeerCapability::Message,
            "task" | "task_request" => PeerCapability::TaskRequest,
            "artifact" => PeerCapability::Artifact,
            other => {
                return Err(format!(
                    "Unknown peer grant '{other}' (expected message, task or artifact)"
                ))
            }
        };
        grants.insert(match capability {
            PeerCapability::Message => DeviceCapability::PeerMessage,
            PeerCapability::TaskRequest => DeviceCapability::PeerTaskRequest,
            PeerCapability::Artifact => DeviceCapability::PeerArtifact,
        });
    }
    Ok(grants)
}

fn grant_tokens(capabilities: &BTreeSet<DeviceCapability>) -> Vec<&'static str> {
    let mut tokens = Vec::new();
    if capabilities.contains(&DeviceCapability::PeerMessage) {
        tokens.push("message");
    }
    if capabilities.contains(&DeviceCapability::PeerTaskRequest) {
        tokens.push("task");
    }
    if capabilities.contains(&DeviceCapability::PeerArtifact) {
        tokens.push("artifact");
    }
    tokens
}

fn paths() -> Result<DaemonPaths, String> {
    DaemonPaths::resolve()
}

fn invite(
    label: &str,
    allow: &str,
    expires_minutes: u64,
    output: &PathBuf,
    json: bool,
) -> Result<(), String> {
    if label.trim().is_empty() || label.len() > 120 {
        return Err("A peer label must be 1-120 characters".to_string());
    }
    if !(1..=24 * 60).contains(&expires_minutes) {
        return Err("Pairing invitation expiry must be between 1 and 1440 minutes".to_string());
    }
    let grants = parse_grants(allow)?;
    if grants.is_empty() {
        return Err(
            "A peer invitation must grant at least one of message, task or artifact".to_string(),
        );
    }
    let paths = paths()?;
    let invitation =
        crate::daemon::remote::create_peer_invitation(&paths, label, &grants, expires_minutes)?;
    crate::daemon::remote::write_invitation_file(output, &invitation)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "pairing_id": invitation.pairing_id,
                "expires_at_ms": invitation.expires_at_ms,
                "grants": grant_tokens(&grants),
                "output": output.display().to_string(),
            })
        );
        return Ok(());
    }
    println!(
        "One-time peer invitation for {label} written to {} (expires at {}). Transfer it securely; the other installation accepts it with `monkey peers accept`.",
        output.display(),
        invitation.expires_at_ms
    );
    println!("Grants: {}", grant_tokens(&grants).join(", "));
    Ok(())
}

async fn accept(invitation: &PathBuf, alias: &str, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let profile = crate::daemon::remote::accept_peer_invitation(&paths, invitation, alias).await?;
    let grants = grant_tokens(&profile.capabilities);
    if json {
        println!(
            "{}",
            serde_json::json!({
                "alias": profile.alias,
                "peer_id": profile.runner_id,
                "peer_url": profile.runner_url,
                "grants": grants,
                "certificate_sha256": profile.server_certificate_sha256,
            })
        );
        return Ok(());
    }
    println!(
        "Paired with {} as '{}'. Certificate fingerprint {}.",
        profile.runner_id, profile.alias, profile.server_certificate_sha256
    );
    println!("This installation may: {}", grants.join(", "));
    Ok(())
}

/// How long after its last answer a peer still counts as reachable.
///
/// Five minutes rather than a live socket: peers are polled, not connected, so
/// "online" here honestly means "answered recently", and saying otherwise would
/// be a status light that lies.
const PRESENCE_FRESH_MS: u64 = 5 * 60 * 1_000;

/// Presence from the last time a peer actually answered.
///
/// `unknown` is a real answer, not a fallback: a peer that has never been in
/// touch is not offline — nothing has been tried.
fn presence(last_seen_at_ms: Option<u64>, now_ms: u64) -> &'static str {
    match last_seen_at_ms {
        None => "unknown",
        Some(seen) if now_ms.saturating_sub(seen) <= PRESENCE_FRESH_MS => "online",
        Some(_) => "offline",
    }
}

fn list(json: bool) -> Result<(), String> {
    let paths = paths()?;
    let store = RemoteStore::open(&paths.root)?;
    let now = crate::daemon::remote::now_ms_public()?;
    let mut inbound = Vec::new();
    for device in store.devices()? {
        if grant_tokens(&device.capabilities).is_empty() {
            continue;
        }
        // What the peer *claims*, read from its own table and never merged into
        // the grant list. The Peers screen shows the two side by side precisely
        // so an ask cannot be mistaken for an entitlement.
        let advertisement = store.peer_advertisement(&device.device_id)?;
        inbound.push(serde_json::json!({
            "device_id": device.device_id,
            "label": device.device_name,
            "grants": grant_tokens(&device.capabilities),
            "advertised_grants": advertisement
                .as_ref()
                .map(|claim| grant_tokens(&claim.advertised))
                .unwrap_or_default(),
            "requested_grants": advertisement
                .as_ref()
                .map(|claim| grant_tokens(&claim.requested))
                .unwrap_or_default(),
            "state": if device.active() { "active" } else { "revoked" },
            // A pairing that carries only peer grants can reach nothing on
            // the control plane. Saying so explicitly is the difference
            // between "cryptographically paired" and "trusted".
            "peer_only": is_peer_only(&device.capabilities),
            "last_sequence": device.last_sequence,
            "last_seen_at_ms": device.last_seen_at_ms,
            "presence": presence(device.last_seen_at_ms, now),
            "secret_generation": device.secret_generation,
        }));
    }
    let mut outbound = Vec::new();
    for alias in store.controller_aliases()? {
        let Some(profile) = store.controller(&alias)? else {
            continue;
        };
        let grants = grant_tokens(&profile.capabilities);
        // Peer grants are how a peer profile is told apart from a controller
        // one — but a peer the far side has since revoked has none, and
        // dropping it here would take its Forget button with it. Anything that
        // has ever introduced itself as a peer stays listed as one.
        if grants.is_empty() && profile.peer_advertised.is_empty() {
            continue;
        }
        outbound.push(serde_json::json!({
            "alias": profile.alias,
            "peer_id": profile.runner_id,
            "peer_url": profile.runner_url,
            "grants": grants,
            "advertised_grants": grant_tokens(&profile.peer_advertised),
            "requested_grants": grant_tokens(&profile.peer_requested),
            "certificate_sha256": profile.server_certificate_sha256,
            "last_seen_at_ms": profile.last_seen_at_ms,
            "presence": presence(profile.last_seen_at_ms, now),
            "secret_generation": profile.secret_generation,
        }));
    }

    if json {
        println!(
            "{}",
            serde_json::json!({ "inbound": inbound, "outbound": outbound })
        );
        return Ok(());
    }
    if inbound.is_empty() && outbound.is_empty() {
        println!(
            "No peers. `monkey peers invite` offers pairing; `monkey peers accept` takes one up."
        );
        return Ok(());
    }
    for peer in &inbound {
        println!(
            "inbound  {}  {}  grants={}  {}",
            peer["device_id"].as_str().unwrap_or_default(),
            peer["label"].as_str().unwrap_or_default(),
            peer["grants"]
                .as_array()
                .map(|grants| grants
                    .iter()
                    .filter_map(|g| g.as_str())
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default(),
            peer["state"].as_str().unwrap_or_default(),
        );
    }
    for peer in &outbound {
        println!(
            "outbound {}  {}  grants={}",
            peer["alias"].as_str().unwrap_or_default(),
            peer["peer_url"].as_str().unwrap_or_default(),
            peer["grants"]
                .as_array()
                .map(|grants| grants
                    .iter()
                    .filter_map(|g| g.as_str())
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default(),
        );
    }
    Ok(())
}

fn grant(device_id: &str, allow: &str, json: bool) -> Result<(), String> {
    let grants = parse_grants(allow)?;
    let paths = paths()?;
    let now = crate::daemon::remote::now_ms_public()?;
    let device = RemoteStore::open(&paths.root)?.set_peer_capabilities(device_id, &grants, now)?;
    let tokens = grant_tokens(&device.capabilities);
    if json {
        println!(
            "{}",
            serde_json::json!({ "device_id": device.device_id, "grants": tokens })
        );
        return Ok(());
    }
    if tokens.is_empty() {
        println!("{device_id} is still paired but may no longer ask for anything.");
    } else {
        println!("{device_id} may now: {}", tokens.join(", "));
    }
    Ok(())
}

fn revoke(device_id: &str, reason: &str) -> Result<(), String> {
    let paths = paths()?;
    let now = crate::daemon::remote::now_ms_public()?;
    RemoteStore::open(&paths.root)?.revoke_device(
        device_id,
        reason,
        now,
        &KeyringRemoteSecrets,
        None,
    )?;
    // The pairing is gone; the traffic it produced goes with it, so a revoked
    // peer does not keep occupying the Peers screen.
    let removed = DaemonStore::open(&paths)?.delete_peer_traffic(device_id)?;
    println!("Revoked {device_id} and removed {removed} thread(s).");
    Ok(())
}

/// Replace an inbound peer's key. The bundle it produces is the only copy of
/// the replacement, which is why it goes to a private file and never to stdout.
fn rotate(device_id: &str, output: &PathBuf, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let config = crate::daemon::remote::enabled_host(&paths)?;
    let certificate = std::fs::read_to_string(&config.certificate_path)
        .map_err(|error| format!("Could not read this installation's certificate: {error}"))?;
    let bundle = RemoteStore::open(&paths.root)?.rotate_device(
        device_id,
        &config.runner_id,
        &config.advertise_url,
        &certificate,
        &config.certificate_sha256,
        crate::daemon::remote::now_ms_public()?,
        &KeyringRemoteSecrets,
    )?;
    crate::daemon::remote::protected_json(output, &bundle)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "device_id": device_id,
                "secret_generation": bundle.secret_generation,
                "output": output.display().to_string(),
            })
        );
        return Ok(());
    }
    println!(
        "Rotated {device_id} to key generation {}; its previous key is invalid immediately.",
        bundle.secret_generation
    );
    println!("Transfer {} to that peer securely.", output.display());
    Ok(())
}

fn accept_rotation(bundle: &PathBuf, alias: &str, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let profile = crate::daemon::remote::accept_peer_rotation(&paths, alias, bundle)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "alias": profile.alias,
                "secret_generation": profile.secret_generation,
                "certificate_sha256": profile.server_certificate_sha256,
            })
        );
        return Ok(());
    }
    println!(
        "Peer '{}' now uses key generation {}. Certificate fingerprint {}.",
        profile.alias, profile.secret_generation, profile.server_certificate_sha256
    );
    Ok(())
}

async fn status(alias: &str, request: &str, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let requested = parse_grants(request)?;
    let response = crate::daemon::remote::peer_hello(&paths, alias, &requested).await?;
    let now = crate::daemon::remote::now_ms_public()?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "alias": alias,
                "peer_id": response.instance_id,
                "last_seen_at_ms": now,
                // A hello that returned *is* contact, so this is always
                // `online`; the field is here so the caller reads one shape
                // whether it probed or listed.
                "presence": presence(Some(now), now),
                "advertised_grants": grant_tokens(&response.advertised),
                "granted": grant_tokens(&response.granted),
            })
        );
        return Ok(());
    }
    println!(
        "{alias} answered as {} and allows this installation to: {}",
        response.instance_id,
        if response.granted.is_empty() {
            "nothing".to_string()
        } else {
            grant_tokens(&response.granted).join(", ")
        }
    );
    Ok(())
}

/// Clear what a peer left behind here.
///
/// Two different things depending on the pairing's state, and deliberately so:
/// an active peer loses its traffic and keeps its standing, a revoked one also
/// loses the grants it can no longer use — which is the finding Security Doctor
/// raises about a revoked pairing whose grant list was never cleared.
fn clear(device_id: &str, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let mut remote = RemoteStore::open(&paths.root)?;
    let device = remote
        .device(device_id)?
        .ok_or_else(|| format!("Unknown paired device '{device_id}'"))?;
    let grants_cleared = if device.active() {
        false
    } else {
        remote.set_peer_capabilities(
            device_id,
            &BTreeSet::new(),
            crate::daemon::remote::now_ms_public()?,
        )?;
        true
    };
    let threads_removed = DaemonStore::open(&paths)?.delete_peer_traffic(device_id)?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "device_id": device_id,
                "threads_removed": threads_removed,
                "grants_cleared": grants_cleared,
            })
        );
        return Ok(());
    }
    println!("Removed {threads_removed} thread(s) for {device_id}.");
    if grants_cleared {
        println!("Its retained peer grants were cleared; the pairing stays revoked.");
    }
    Ok(())
}

fn forget(alias: &str) -> Result<(), String> {
    let paths = paths()?;
    if !RemoteStore::open(&paths.root)?.forget_controller(alias, &KeyringRemoteSecrets)? {
        return Err(format!("Unknown peer '{alias}'"));
    }
    println!(
        "Forgot '{alias}'. This installation can no longer reach it; revoking this installation \
         over there is that peer's own decision."
    );
    Ok(())
}

async fn send(
    alias: &str,
    text: &str,
    thread: Option<&str>,
    task: bool,
    correlation: Option<&str>,
    artifacts: &[String],
    json: bool,
) -> Result<(), String> {
    if text.len() > MAX_BODY_BYTES {
        return Err(format!(
            "A peer message may carry at most {MAX_BODY_BYTES} bytes"
        ));
    }
    let paths = paths()?;
    let sender_instance_id = crate::daemon::remote::local_instance_id(&paths, alias)?;
    let now = i64::try_from(crate::daemon::remote::now_ms_public()?).unwrap_or(i64::MAX);
    let thread_id = thread
        .map(str::to_string)
        .unwrap_or_else(|| format!("thread-{}", uuid::Uuid::new_v4().simple()));
    let mut envelope = PeerEnvelope::new(
        format!("pmsg-{}", uuid::Uuid::new_v4().simple()),
        thread_id.clone(),
        if task {
            PeerMessageKind::TaskRequest
        } else {
            PeerMessageKind::Message
        },
        sender_instance_id,
        text,
        now,
        DEFAULT_TTL_MS,
    );
    envelope.correlation_id = correlation.map(str::to_string);
    envelope.hop_limit = DEFAULT_HOP_LIMIT;
    envelope.artifacts =
        crate::daemon::peer_tool::upload_artifacts(&paths, alias, artifacts).await?;
    if !envelope.artifacts.is_empty()
        && envelope.kind == PeerMessageKind::Message
        && text.is_empty()
    {
        envelope.kind = PeerMessageKind::Artifact;
    }
    // Refused here as well as on the far side: a malformed envelope should cost
    // nothing and travel nowhere.
    envelope
        .validate_for_send(now)
        .map_err(|rejection| rejection.message().to_string())?;

    let response = crate::daemon::remote::peer_call(
        &paths,
        alias,
        reqwest::Method::POST,
        "/v1/remote/peer/messages",
        serde_json::to_vec(&envelope).map_err(|error| error.to_string())?,
    )
    .await?;
    if json {
        println!("{response}");
        return Ok(());
    }
    println!(
        "Sent {} to {alias} in thread {thread_id} ({}).",
        envelope.message_id,
        response["state"].as_str().unwrap_or("accepted")
    );
    println!("Read the answer with `monkey peers thread {alias} {thread_id}`.");
    Ok(())
}

async fn thread(alias: &str, thread_id: &str, json: bool) -> Result<(), String> {
    let paths = paths()?;
    let response = crate::daemon::remote::peer_call(
        &paths,
        alias,
        reqwest::Method::GET,
        &format!("/v1/remote/peer/threads/{thread_id}"),
        Vec::new(),
    )
    .await?;
    if json {
        println!("{response}");
        return Ok(());
    }
    let empty = Vec::new();
    let messages = response["messages"].as_array().unwrap_or(&empty);
    if messages.is_empty() {
        println!("Nothing in {thread_id} yet.");
        return Ok(());
    }
    for message in messages {
        let payload = &message["payload"];
        let body = payload["text"]
            .as_str()
            .or_else(|| payload["body"].as_str())
            .unwrap_or("");
        println!(
            "{}  {}  {}  {}",
            message["created_at_ms"].as_i64().unwrap_or_default(),
            message["direction"].as_str().unwrap_or_default(),
            message["disposition"].as_str().unwrap_or_default(),
            body.lines().next().unwrap_or_default()
        );
    }
    Ok(())
}

fn threads(peer: Option<&str>, limit: u32, json: bool) -> Result<(), String> {
    let store = DaemonStore::open(&paths()?)?;
    let threads = store.peer_threads(peer, limit.clamp(1, 200))?;
    if json {
        let mut rows = Vec::new();
        for thread in &threads {
            let messages = store.peer_messages(&thread.peer_device_id, &thread.thread_id, 200)?;
            rows.push(serde_json::json!({
                "thread_id": thread.thread_id,
                "peer_device_id": thread.peer_device_id,
                "peer_instance_id": thread.peer_instance_id,
                "session_key": thread.session_key,
                "created_at_ms": thread.created_at_ms,
                "last_activity_at_ms": thread.last_activity_at_ms,
                "message_count": messages.len(),
                "recent": messages.iter().rev().take(10).map(|message| serde_json::json!({
                    "message_id": message.message_id,
                    "direction": message.direction.as_str(),
                    "kind": message.kind,
                    "disposition": message.disposition.as_str(),
                    "rejection": message.rejection,
                    "job_id": message.job_id,
                    "created_at_ms": message.created_at_ms,
                })).collect::<Vec<_>>(),
            }));
        }
        println!(
            "{}",
            serde_json::json!({ "threads": rows, "recipe": PEER_TASK_RECIPE })
        );
        return Ok(());
    }
    if threads.is_empty() {
        println!("No peer has opened a thread here yet.");
        return Ok(());
    }
    for thread in &threads {
        println!(
            "{}  {}  {}  {}",
            thread.last_activity_at_ms, thread.thread_id, thread.peer_device_id, thread.session_key
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_parse_from_what_an_operator_would_type() {
        assert_eq!(
            parse_grants("message,task").unwrap(),
            BTreeSet::from([
                DeviceCapability::PeerMessage,
                DeviceCapability::PeerTaskRequest
            ])
        );
        assert_eq!(
            parse_grants(" artifact , task_request ").unwrap(),
            BTreeSet::from([
                DeviceCapability::PeerArtifact,
                DeviceCapability::PeerTaskRequest
            ])
        );
        assert!(parse_grants("").unwrap().is_empty());
    }

    #[test]
    fn a_grant_this_command_must_not_hand_out_is_refused() {
        // Nothing an operator can type here reaches the control plane: the
        // parser knows three words, and "admin" is not one of them.
        assert!(parse_grants("admin").unwrap_err().contains("admin"));
        assert!(parse_grants("place_runs").is_err());
        assert!(parse_grants("view_runs").is_err());
    }

    /// Presence is a claim about the last time a peer *answered*, and the three
    /// states are genuinely different: "never in touch" is not "offline", and
    /// treating it as one would tell an operator a pairing had failed when
    /// nothing had ever been tried.
    #[test]
    fn presence_distinguishes_never_asked_from_not_answering() {
        const NOW: u64 = 1_700_000_000_000;
        assert_eq!(presence(None, NOW), "unknown");
        assert_eq!(presence(Some(NOW), NOW), "online");
        assert_eq!(presence(Some(NOW - PRESENCE_FRESH_MS), NOW), "online");
        assert_eq!(presence(Some(NOW - PRESENCE_FRESH_MS - 1), NOW), "offline");
        // A peer whose clock ran ahead of ours is reachable, not from the
        // future: saturating here keeps a skewed timestamp from reading as
        // stale by an enormous margin.
        assert_eq!(presence(Some(NOW + 60_000), NOW), "online");
    }

    #[test]
    fn grant_tokens_round_trip_through_the_parser() {
        let grants = parse_grants("message,task,artifact").unwrap();
        assert_eq!(grant_tokens(&grants), vec!["message", "task", "artifact"]);
        assert_eq!(
            parse_grants(&grant_tokens(&grants).join(",")).unwrap(),
            grants
        );
    }
}

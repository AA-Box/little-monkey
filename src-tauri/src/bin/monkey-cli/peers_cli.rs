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
            json,
        } => {
            send(
                alias,
                text,
                thread.as_deref(),
                *task,
                correlation.as_deref(),
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

fn list(json: bool) -> Result<(), String> {
    let paths = paths()?;
    let store = RemoteStore::open(&paths.root)?;
    let inbound: Vec<serde_json::Value> = store
        .devices()?
        .into_iter()
        .filter(|device| !grant_tokens(&device.capabilities).is_empty())
        .map(|device| {
            serde_json::json!({
                "device_id": device.device_id,
                "label": device.device_name,
                "grants": grant_tokens(&device.capabilities),
                "state": if device.active() { "active" } else { "revoked" },
                // A pairing that carries only peer grants can reach nothing on
                // the control plane. Saying so explicitly is the difference
                // between "cryptographically paired" and "trusted".
                "peer_only": is_peer_only(&device.capabilities),
                "last_sequence": device.last_sequence,
            })
        })
        .collect();
    let mut outbound = Vec::new();
    for alias in store.controller_aliases()? {
        let Some(profile) = store.controller(&alias)? else {
            continue;
        };
        let grants = grant_tokens(&profile.capabilities);
        if grants.is_empty() {
            continue;
        }
        outbound.push(serde_json::json!({
            "alias": profile.alias,
            "peer_id": profile.runner_id,
            "peer_url": profile.runner_url,
            "grants": grants,
            "certificate_sha256": profile.server_certificate_sha256,
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

async fn send(
    alias: &str,
    text: &str,
    thread: Option<&str>,
    task: bool,
    correlation: Option<&str>,
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
            let messages = store.peer_messages(&thread.thread_id, 200)?;
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

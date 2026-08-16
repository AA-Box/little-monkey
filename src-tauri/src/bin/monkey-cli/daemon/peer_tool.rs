//! The `peer_message` agent tool.
//!
//! A run can say something to an installation the operator already paired with,
//! and that is the whole capability. What keeps it that small:
//!
//! - The destination is an alias the operator chose at pairing time. There is
//!   no parameter for a URL, a host, a token or a route, so a model cannot be
//!   talked into contacting somewhere new by the text it is reading.
//! - A peer this installation is not paired with, or one with no grant to
//!   accept what is being sent, is refused here rather than contacted and
//!   rejected there.
//! - What happens on the far side is the far side's decision: it runs the
//!   request under its own recipe, its own permissions and its own approvals.
//!   Nothing in the envelope can change that.

use std::collections::BTreeSet;

use little_monkey_lib::peers::{
    PeerArtifactRef, PeerEnvelope, PeerMessageKind, MAX_ARTIFACT_REFS, MAX_BODY_BYTES,
};

use super::remote::protocol::DeviceCapability;
use super::remote::store::RemoteStore;
use super::store::DaemonPaths;

/// Longest text this tool will send. Well under the envelope's own byte bound,
/// because a model writing more than this to a peer is not having a
/// conversation.
const MAX_TEXT_CHARS: usize = 8_000;

/// How long the request stays worth acting on: ten minutes, matching the CLI.
const TTL_MS: i64 = 10 * 60 * 1_000;

/// Peers this installation can currently send to, with what each accepts.
///
/// Used to decide whether the tool is offered at all — an operator with no
/// peers never sees it, rather than being offered one whose only possible
/// answer is a refusal.
pub(crate) fn reachable_peers() -> Vec<(String, BTreeSet<DeviceCapability>)> {
    let Ok(paths) = DaemonPaths::resolve() else {
        return Vec::new();
    };
    let Ok(store) = RemoteStore::open(&paths.root) else {
        return Vec::new();
    };
    let Ok(aliases) = store.controller_aliases() else {
        return Vec::new();
    };
    aliases
        .into_iter()
        .filter_map(|alias| {
            let profile = store.controller(&alias).ok().flatten()?;
            let grants: BTreeSet<DeviceCapability> = profile
                .capabilities
                .iter()
                .copied()
                .filter(|capability| {
                    matches!(
                        capability,
                        DeviceCapability::PeerMessage
                            | DeviceCapability::PeerTaskRequest
                            | DeviceCapability::PeerArtifact
                    )
                })
                .collect();
            (!grants.is_empty()).then_some((profile.alias, grants))
        })
        .collect()
}

pub(crate) fn any_peer_is_reachable() -> bool {
    !reachable_peers().is_empty()
}

/// Send one message or task request to a paired peer.
///
/// Returns the JSON the tool loop hands back to the model: the thread and the
/// message id, so a later turn can ask for the result, and nothing about the
/// peer's address, certificate or grants.
pub(crate) async fn send_peer_message(
    alias: &str,
    text: &str,
    thread: Option<&str>,
    task: bool,
    correlation: Option<&str>,
    artifact_ids: &[String],
) -> Result<serde_json::Value, String> {
    let text = text.trim();
    if text.is_empty() && artifact_ids.is_empty() {
        return Err("A peer message must contain some text.".to_string());
    }
    if text.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "A peer message must be at most {MAX_TEXT_CHARS} characters; this one is {}.",
            text.chars().count()
        ));
    }
    if text.len() > MAX_BODY_BYTES {
        return Err("A peer message is larger than the envelope allows.".to_string());
    }
    if artifact_ids.len() > MAX_ARTIFACT_REFS {
        return Err(format!(
            "A peer message may carry at most {MAX_ARTIFACT_REFS} artifacts."
        ));
    }

    let grants = reachable_peers()
        .into_iter()
        .find(|(name, _)| name == alias)
        .map(|(_, grants)| grants)
        .ok_or_else(|| {
            format!("'{alias}' is not a peer this installation is paired with as a peer.")
        })?;
    let needed = if task {
        DeviceCapability::PeerTaskRequest
    } else {
        DeviceCapability::PeerMessage
    };
    if !grants.contains(&needed) {
        return Err(format!(
            "'{alias}' did not grant this installation permission to {}.",
            if task {
                "ask it to do work"
            } else {
                "send it messages"
            }
        ));
    }
    // Checked here rather than only on the far side, because failing after the
    // upload would leave the peer holding bytes for a message it then refuses.
    if !artifact_ids.is_empty() && !grants.contains(&DeviceCapability::PeerArtifact) {
        return Err(format!(
            "'{alias}' did not grant this installation permission to hand over files."
        ));
    }

    let paths = DaemonPaths::resolve()?;
    let sender_instance_id = super::remote::local_instance_id(&paths, alias)?;
    let now = i64::try_from(super::remote::now_ms_public()?).unwrap_or(i64::MAX);
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
        TTL_MS,
    );
    envelope.correlation_id = correlation.map(str::to_string);
    envelope.artifacts = upload_artifacts(&paths, alias, artifact_ids).await?;

    // Validated here as well as on the far side: a malformed envelope should
    // cost nothing and travel nowhere.
    envelope
        .validate_for_send(now)
        .map_err(|rejection| rejection.message().to_string())?;

    let response = super::remote::peer_call(
        &paths,
        alias,
        reqwest::Method::POST,
        "/v1/remote/peer/messages",
        serde_json::to_vec(&envelope).map_err(|error| error.to_string())?,
    )
    .await?;

    Ok(serde_json::json!({
        "sent": true,
        "peer": alias,
        "thread_id": thread_id,
        "message_id": envelope.message_id,
        "correlation_id": envelope.correlation_id,
        "artifacts": envelope.artifacts.len(),
        "state": response["state"].as_str().unwrap_or("accepted"),
        "note": if task {
            "The peer decides whether to run this, under its own permissions. Ask again later for the result."
        } else {
            "Delivered to the peer's thread."
        },
    }))
}

/// Hand each artifact's bytes over, then describe what the peer stored.
///
/// Ids only. There is no parameter anywhere on this path for a path, so neither
/// a model nor a CLI caller can turn a filename it read somewhere into a file
/// that leaves this machine — only content the content store already holds.
///
/// Uploading before referencing is what makes the reference resolvable: the
/// receiver refuses an envelope naming content it was never given, so a
/// half-done exchange fails here rather than queueing an unreadable attachment
/// over there.
pub(crate) async fn upload_artifacts(
    paths: &DaemonPaths,
    alias: &str,
    artifact_ids: &[String],
) -> Result<Vec<PeerArtifactRef>, String> {
    if artifact_ids.is_empty() {
        return Ok(Vec::new());
    }
    let store = super::peer_ingress::peer_content_store(paths)?;
    let mut refs = Vec::with_capacity(artifact_ids.len());
    for id in artifact_ids {
        // `read` validates the id before touching the filesystem, so an id
        // shaped like a path is refused rather than followed.
        let bytes = store
            .read(id)
            .map_err(|error| format!("Artifact '{id}': {error}"))?;
        let stored = super::remote::peer_put_artifact(paths, alias, &bytes, None, None)
            .await
            .map_err(|error| format!("Artifact '{id}': {error}"))?;
        refs.push(PeerArtifactRef {
            artifact_id: stored.artifact_id,
            sha256: stored.sha256,
            filename: None,
            media_type: None,
            size_bytes: Some(stored.size_bytes),
        });
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_unknown_peer_is_refused_without_contacting_anything() {
        let error = send_peer_message("nobody", "hello", None, false, None, &[])
            .await
            .expect_err("unknown peer");
        assert!(error.contains("not a peer"));
    }

    #[tokio::test]
    async fn empty_and_oversized_text_are_refused_before_any_lookup() {
        assert!(send_peer_message("nobody", "   ", None, false, None, &[])
            .await
            .expect_err("empty")
            .contains("must contain some text"));

        let long = "x".repeat(MAX_TEXT_CHARS + 1);
        assert!(send_peer_message("nobody", &long, None, false, None, &[])
            .await
            .expect_err("too long")
            .contains("at most"));
    }
}

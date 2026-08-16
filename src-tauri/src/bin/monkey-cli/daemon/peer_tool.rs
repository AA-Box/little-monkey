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

use super::peer_store::PeerOutboundMessage;
use super::remote::protocol::DeviceCapability;
use super::remote::store::RemoteStore;
use super::store::{DaemonPaths, DaemonStore};

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
    remember_send(&paths, alias, &envelope, &response, now);

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

/// Remember one thing this installation sent, so it can be followed later.
///
/// Best-effort on purpose: the message *was* delivered, and failing the send
/// afterwards because a local bookkeeping row could not be written would turn a
/// success into a retry the far side would then refuse as a duplicate.
///
/// This row is also the reason there is no remote "list every thread" route and
/// no need for one. The only installation that legitimately knows which threads
/// exist on a peer is the one that opened them, so it keeps the list itself
/// rather than asking the peer to enumerate its conversations to whoever calls.
pub(crate) fn remember_send(
    paths: &DaemonPaths,
    alias: &str,
    envelope: &PeerEnvelope,
    response: &serde_json::Value,
    now_ms: i64,
) {
    let state = response["state"].as_str().unwrap_or("accepted");
    // The far side answers `accepted:false` for a duplicate it had already
    // refused; taking its word is the whole point of asking.
    let state = if response["accepted"] == serde_json::Value::Bool(false) {
        "rejected"
    } else {
        state
    };
    let _ = DaemonStore::open(paths).and_then(|mut store| {
        store.record_outbound_peer_message(
            alias,
            &envelope.message_id,
            &envelope.thread_id,
            envelope.correlation_id.as_deref(),
            envelope.kind.as_str(),
            state,
            now_ms,
        )
    });
}

/// Ask one peer about one thread this installation opened, and fold what came
/// back into the local record.
///
/// Deliberately narrow. There is no route that enumerates a node's peer threads
/// and there should not be — that would let any paired peer discover the shape
/// of every conversation a node is having. The thread id here comes from this
/// installation's own outbound record, so it asks only about conversations it
/// started.
pub(crate) async fn refresh_remote_thread(
    paths: &DaemonPaths,
    alias: &str,
    thread_id: &str,
) -> Result<Vec<PeerOutboundMessage>, String> {
    check_peer_id(thread_id)?;
    let response = super::remote::peer_call(
        paths,
        alias,
        reqwest::Method::GET,
        &format!("/v1/remote/peer/threads/{thread_id}"),
        Vec::new(),
    )
    .await?;
    let now = i64::try_from(super::remote::now_ms_public()?).unwrap_or(i64::MAX);
    let mut store = DaemonStore::open(paths)?;
    let empty = Vec::new();
    for message in response["messages"].as_array().unwrap_or(&empty) {
        let payload = &message["payload"];
        match message["kind"].as_str() {
            // A result the peer produced for something this installation asked
            // for. `in_reply_to` is the message id this side minted.
            Some("result") => {
                let Some(in_reply_to) = payload["in_reply_to"].as_str() else {
                    continue;
                };
                store.record_outbound_peer_result(
                    alias,
                    in_reply_to,
                    payload["state"].as_str().unwrap_or("succeeded"),
                    payload["text"].as_str(),
                    now,
                )?;
            }
            // The peer's echo of what this installation sent, which is where a
            // refusal shows up.
            Some(_) if message["disposition"] == "rejected" => {
                let Some(message_id) = message["message_id"].as_str() else {
                    continue;
                };
                store.record_outbound_peer_result(
                    alias,
                    message_id,
                    "rejected",
                    message["rejection"].as_str(),
                    now,
                )?;
            }
            _ => {}
        }
    }
    Ok(store
        .outbound_peer_messages(Some(alias), 200)?
        .into_iter()
        .filter(|message| message.thread_id == thread_id)
        .collect())
}

/// The identifier alphabet the peer plane uses everywhere else.
///
/// Checked before a thread id reaches a URL: an id is the operator's or this
/// installation's own, but it still must not be able to become a different
/// route by carrying a slash or a query string.
fn check_peer_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > little_monkey_lib::peers::MAX_ID_LEN
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(format!("'{value}' is not a peer thread id"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_id_cannot_turn_into_a_different_route() {
        for forged in [
            "../../v1/remote/runs",
            "thread-1/../node",
            "thread 1",
            "thread-1?all=true",
            "",
        ] {
            assert!(
                check_peer_id(forged).is_err(),
                "'{forged}' must not be usable as a thread id"
            );
        }
        assert!(check_peer_id("thread-abc.1:2_3").is_ok());
    }

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

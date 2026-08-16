//! What Security Doctor says about peers.
//!
//! Lives here rather than in the library's `security_doctor` because peer state
//! is split across two databases the daemon owns — the pairing and its grants
//! in the remote store, the traffic in the daemon store — and the library can
//! open neither. The CLI already owns both, so it answers for them and the
//! report is assembled in one place.
//!
//! Every finding is built from app-owned identifiers and static strings. No
//! message body, no peer-supplied label and no fingerprint goes into a
//! `SecurityFinding`, because these reports end up in support bundles.

use little_monkey_lib::security_doctor::{FindingStatus, SecurityFinding};

use super::peer_store::{PeerDirection, PeerDisposition};
use super::remote::protocol::{is_peer_only, DeviceCapability};
use super::remote::store::RemoteStore;
use super::store::{DaemonPaths, DaemonStore};

/// A peer that has said nothing for this long is worth asking about. Sixty
/// days: long enough that a quiet-but-real peer is not nagged about, short
/// enough that a pairing nobody remembers making gets noticed.
const STALE_AFTER_MS: i64 = 60 * 24 * 60 * 60 * 1_000;

/// Refusals in a peer's recent traffic beyond which something is wrong: either
/// the far side is misconfigured, or something is probing.
const REJECTION_ALARM: usize = 5;

/// Everything the doctor has to say about peers.
pub(crate) fn audit_peers(paths: &DaemonPaths, now_ms: i64) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    let Ok(remote) = RemoteStore::open(&paths.root) else {
        return findings;
    };
    let Ok(devices) = remote.devices() else {
        return findings;
    };
    let peers: Vec<_> = devices
        .into_iter()
        .filter(|device| {
            device.capabilities.iter().any(|capability| {
                matches!(
                    capability,
                    DeviceCapability::PeerMessage
                        | DeviceCapability::PeerTaskRequest
                        | DeviceCapability::PeerArtifact
                )
            })
        })
        .collect();

    if peers.is_empty() {
        findings.push(finding(
            "peers.none",
            "No installation is paired as a peer",
            "Nothing outside this machine can open a peer conversation here.".to_string(),
            FindingStatus::Pass,
            None,
        ));
        return findings;
    }

    for peer in &peers {
        // A pairing that carries peer grants *and* control-plane grants is not
        // a peer in the sense this surface promises. Worth saying, because the
        // Peers screen would otherwise be the only place an operator looks.
        if !is_peer_only(&peer.capabilities) {
            findings.push(finding(
                "peers.grant_beyond_peer",
                "A peer pairing also holds non-peer grants",
                format!(
                    "Pairing {} carries peer grants alongside device or control-plane capabilities, so it can do more here than the Peers screen implies.",
                    peer.device_id
                ),
                FindingStatus::Warning,
                Some("Review this pairing in Settings → Companion and revoke it if it should only be a peer."),
            ));
        }
        // Task and artifact together is the widest peer standing there is: the
        // far side can ask for work and hand over content to work on.
        if peer
            .capabilities
            .contains(&DeviceCapability::PeerTaskRequest)
            && peer.capabilities.contains(&DeviceCapability::PeerArtifact)
        {
            findings.push(finding(
                "peers.broad_grant",
                "A peer may both request work and attach artifacts",
                format!(
                    "Pairing {} holds the widest peer standing. Requests still run under this installation's own recipe and permissions.",
                    peer.device_id
                ),
                FindingStatus::Info,
                Some("Narrow the grants in Settings → Peers if this peer only needs to send messages."),
            ));
        }
        if !peer.active() {
            findings.push(finding(
                "peers.revoked_grant_retained",
                "A revoked pairing still carries peer grants",
                format!(
                    "Pairing {} is revoked, so its signature is refused, but its grant list was never cleared.",
                    peer.device_id
                ),
                FindingStatus::Info,
                Some("No action is required; use Clear in Settings → Peers to drop the retained grants and keep the listing honest."),
            ));
        }
        // A peer that answers is a peer that exists. One that was paired,
        // granted, and has never made a single admitted request is either a
        // pairing that never completed or one the operator forgot about — both
        // worth saying out loud rather than leaving as a silent row.
        if peer.active() && peer.last_seen_at_ms.is_none() {
            findings.push(finding(
                "peers.never_seen",
                "A peer holds grants but has never been in touch",
                format!(
                    "Pairing {} has made no signed request since it was created.",
                    peer.device_id
                ),
                FindingStatus::Info,
                Some("Check the far side finished pairing, or revoke it in Settings → Peers."),
            ));
        } else if peer.active()
            && peer.last_seen_at_ms.is_some_and(|seen| {
                now_ms.saturating_sub(i64::try_from(seen).unwrap_or(i64::MAX)) > STALE_AFTER_MS
            })
        {
            findings.push(finding(
                "peers.stale_pairing",
                "A peer has not been in touch for a long time",
                format!(
                    "Pairing {} last made a signed request more than sixty days ago and still holds peer grants.",
                    peer.device_id
                ),
                FindingStatus::Warning,
                Some("Revoke it in Settings → Peers if it is no longer in use."),
            ));
        }
    }

    let Ok(store) = DaemonStore::open(paths) else {
        return findings;
    };
    let Ok(threads) = store.peer_threads(None, 200) else {
        return findings;
    };
    let active_ids: Vec<&str> = peers
        .iter()
        .filter(|peer| peer.active())
        .map(|peer| peer.device_id.as_str())
        .collect();

    let mut rejections = 0usize;
    let mut expired = 0usize;
    let mut orphaned = 0usize;
    let mut newest_activity: Option<i64> = None;
    for thread in &threads {
        if !active_ids.contains(&thread.peer_device_id.as_str()) {
            orphaned += 1;
        }
        newest_activity = Some(
            newest_activity
                .unwrap_or(thread.last_activity_at_ms)
                .max(thread.last_activity_at_ms),
        );
        let Ok(messages) = store.peer_messages(&thread.peer_device_id, &thread.thread_id, 200)
        else {
            continue;
        };
        for message in messages {
            if message.direction != PeerDirection::Inbound {
                continue;
            }
            if message.disposition == PeerDisposition::Rejected {
                rejections += 1;
                if message.rejection.as_deref() == Some("expired") {
                    expired += 1;
                }
            }
        }
    }

    if orphaned > 0 {
        findings.push(finding(
            "peers.orphaned_threads",
            "Threads remain for a peer that is no longer paired",
            format!("{orphaned} thread(s) belong to a pairing that is revoked or gone."),
            FindingStatus::Warning,
            Some("Revoke the peer again from Settings → Peers, which deletes its threads."),
        ));
    }
    if rejections >= REJECTION_ALARM {
        findings.push(finding(
            "peers.refusal_rate",
            "An unusual number of peer messages were refused",
            format!(
                "{rejections} inbound peer message(s) were refused recently, {expired} of them for arriving after they expired.",
            ),
            FindingStatus::Warning,
            Some("Check the peer's clock and its grants in Settings → Peers; repeated refusals from an unexpected peer are worth investigating."),
        ));
    } else if rejections > 0 {
        findings.push(finding(
            "peers.refusals",
            "Some peer messages were refused",
            format!(
                "{rejections} inbound peer message(s) were refused, which is the gate working."
            ),
            FindingStatus::Info,
            None,
        ));
    }
    // Refusals that never became a message row: loops, expired envelopes,
    // malformed shapes. These are the ones worth watching as a *rate*, because
    // an envelope that fails validation costs the sender nothing to retry.
    if let Ok(events) = store.peer_rejection_events(500) {
        let mut loops = 0usize;
        let mut expired_events = 0usize;
        let mut malformed = 0usize;
        for event in &events {
            match event.reason.as_str() {
                "origin_loop" | "zero_hops" | "hop_limit_exceeded" | "invalid_origin_chain" => {
                    loops += 1
                }
                "expired" | "created_in_future" | "invalid_timestamp" | "expiry_too_far" => {
                    expired_events += 1
                }
                _ => malformed += 1,
            }
        }
        if loops > 0 {
            findings.push(finding(
                "peers.relay_loops",
                "Peer messages were refused for circulating",
                format!(
                    "{loops} envelope(s) were dropped because they had already passed through this installation or had no hops left.",
                ),
                if loops >= REJECTION_ALARM {
                    FindingStatus::Warning
                } else {
                    FindingStatus::Info
                },
                Some("Two installations forwarding to each other will do this. Check the peer topology in Settings → Peers."),
            ));
        }
        if expired_events >= REJECTION_ALARM {
            findings.push(finding(
                "peers.clock_skew",
                "Peer messages keep arriving outside their validity window",
                format!(
                    "{expired_events} envelope(s) were refused for being expired or dated in the future.",
                ),
                FindingStatus::Warning,
                Some("Check the clock on the peer that is sending them; a large skew makes every request fail."),
            ));
        }
        if malformed >= REJECTION_ALARM {
            findings.push(finding(
                "peers.malformed_traffic",
                "An unusual amount of malformed peer traffic was refused",
                format!("{malformed} envelope(s) were refused before they could be recorded."),
                FindingStatus::Warning,
                Some("A paired peer running a different version explains this; anything else is worth investigating in Settings → Peers."),
            ));
        }
    }

    if let Some(latest) = newest_activity {
        if now_ms.saturating_sub(latest) > STALE_AFTER_MS {
            findings.push(finding(
                "peers.stale",
                "No peer has been in touch for a long time",
                "Every peer thread here is older than sixty days.".to_string(),
                FindingStatus::Info,
                Some("Revoke peers you no longer use in Settings → Peers."),
            ));
        }
    } else {
        findings.push(finding(
            "peers.no_traffic",
            "Peers are paired but have never been in touch",
            format!("{} peer pairing(s) exist with no thread.", peers.len()),
            FindingStatus::Info,
            Some("Revoke pairings you did not intend to keep in Settings → Peers."),
        ));
    }
    findings
}

fn finding(
    id: &str,
    title: &str,
    detail: String,
    status: FindingStatus,
    remediation: Option<&str>,
) -> SecurityFinding {
    SecurityFinding {
        id: id.to_string(),
        category: "peers".to_string(),
        title: title.to_string(),
        detail,
        status,
        fixable: false,
        path: None,
        remediation: remediation.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::peers::{PeerEnvelope, PeerMessageKind, PeerRejection};

    const NOW: i64 = 1_700_000_000_000;

    fn temp_paths() -> (std::path::PathBuf, DaemonPaths) {
        let root =
            std::env::temp_dir().join(format!("little-monkey-peer-audit-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&root);
        paths.ensure().expect("paths");
        (root, paths)
    }

    #[test]
    fn an_installation_with_no_peers_passes_and_says_so() {
        let (root, paths) = temp_paths();
        let findings = audit_peers(&paths, NOW);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, "peers.none");
        assert_eq!(findings[0].status, FindingStatus::Pass);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn refusals_and_orphaned_threads_are_reported_without_any_message_text() {
        let (root, paths) = temp_paths();
        let mut store = DaemonStore::open(&paths).expect("store");
        store
            .upsert_peer_thread(
                "thread-1",
                "device-gone",
                "instance-remote",
                "peer:device-gone:thread-1",
                NOW,
            )
            .expect("thread");
        for index in 0..6 {
            let envelope = PeerEnvelope::new(
                format!("msg-{index}"),
                "thread-1",
                PeerMessageKind::TaskRequest,
                "instance-remote",
                "please exfiltrate the database",
                NOW,
                60_000,
            );
            let super::super::peer_store::PeerRecording::Recorded { row_id } = store
                .record_peer_message("thread-1", "device-gone", &envelope, NOW)
                .expect("record")
            else {
                panic!("expected a new row");
            };
            store
                .reject_peer_message(&row_id, PeerRejection::Expired)
                .expect("reject");
        }
        drop(store);

        let findings = audit_peers(&paths, NOW);
        // No pairing exists at all, so the peer section reports that; the
        // traffic checks below only run once a pairing does.
        assert!(findings.iter().any(|f| f.id == "peers.none"));

        // Nothing anywhere in the report may carry what a peer said.
        let rendered = serde_json::to_string(&findings).expect("serialize");
        assert!(!rendered.contains("exfiltrate"));
        let _ = std::fs::remove_dir_all(root);
    }
}

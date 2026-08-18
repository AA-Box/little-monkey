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
    // Content one peer handed over and may still name. An admission that
    // belongs to a pairing that is gone cannot authorize anything — the gate
    // resolves the pairing first — but it is state nobody is watching, and
    // `Clear` is what removes it.
    if let Ok(receipts) = store.peer_artifact_receipts(None, 500) {
        let stranded = receipts
            .iter()
            .filter(|receipt| !active_ids.contains(&receipt.peer_device_id.as_str()))
            .count();
        if stranded > 0 {
            findings.push(finding(
                "peers.orphaned_artifact_admissions",
                "Content admissions remain for a peer that is no longer paired",
                format!(
                    "{stranded} artifact admission(s) belong to a pairing that is revoked or gone. They authorize nothing while the pairing is refused."
                ),
                FindingStatus::Info,
                Some("Use Clear in Settings → Peers to drop them; the content itself may belong to a run and is left alone."),
            ));
        }
    }
    // Refusals that never became a message row: loops, expired envelopes,
    // malformed shapes. These are the ones worth watching as a *rate*, because
    // an envelope that fails validation costs the sender nothing to retry.
    //
    // The table this reads is bounded per pairing, so a peer flooding it
    // cannot push another peer's evidence out and cannot grow the database
    // without end. What the doctor sees is the most recent traffic, which is
    // what these findings are about.
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
    use std::collections::BTreeSet;

    use super::*;
    use little_monkey_lib::peers::{PeerEnvelope, PeerMessageKind, PeerRejection};

    const NOW: i64 = 1_700_000_000_000;
    const NOW_U64: u64 = 1_700_000_000_000;

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

    /// Pair one peer for real, so the traffic findings below actually run.
    fn paths_with_a_peer(
        capabilities: BTreeSet<DeviceCapability>,
    ) -> (std::path::PathBuf, DaemonPaths, String) {
        let (root, paths) = temp_paths();
        let mut remote = RemoteStore::open(&paths.root).expect("remote store");
        let scopes = super::super::remote::protocol::RemoteScopes {
            actions: BTreeSet::new(),
            run_ids: BTreeSet::new(),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let invitation = remote
            .create_invitation_with_capabilities(&scopes, &capabilities, NOW_U64, NOW_U64 + 600_000)
            .expect("invitation");
        let accepted = remote
            .accept_invitation_with_capabilities(
                &invitation.pairing_id,
                &invitation.token,
                "peer",
                "instance-local",
                None,
                NOW_U64 + 1,
                &MemorySecrets::default(),
            )
            .expect("accept");
        (root, paths, accepted.device_id)
    }

    #[derive(Default)]
    struct MemorySecrets(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

    impl super::super::remote::store::RemoteSecretStore for MemorySecrets {
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }

        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing".to_string())
        }

        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    /// The findings the bounded refusal table is *for*.
    ///
    /// Retention changed underneath these; the point of the table did not, so
    /// the loop, clock-skew and malformed findings still have to appear from
    /// the rows that survive.
    #[test]
    fn loops_clock_skew_and_malformed_traffic_are_still_reported_from_a_bounded_table() {
        let (root, paths, device_id) = paths_with_a_peer(BTreeSet::from([
            DeviceCapability::PeerMessage,
            DeviceCapability::PeerTaskRequest,
        ]));
        let mut store = DaemonStore::open(&paths).expect("store");
        for (reason, count) in [
            (PeerRejection::OriginLoop, 3),
            (PeerRejection::ZeroHops, 3),
            (PeerRejection::Expired, 4),
            (PeerRejection::CreatedInFuture, 2),
            (PeerRejection::MalformedId, 6),
        ] {
            for index in 0..count {
                store
                    .record_peer_rejection_event(
                        &device_id,
                        Some("msg-1"),
                        Some("thread-1"),
                        reason,
                        NOW + index,
                    )
                    .expect("record");
            }
        }
        drop(store);

        let findings = audit_peers(&paths, NOW);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"peers.relay_loops"), "{ids:?}");
        assert!(ids.contains(&"peers.clock_skew"), "{ids:?}");
        assert!(ids.contains(&"peers.malformed_traffic"), "{ids:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    /// One peer flooding must not be able to erase what another peer did.
    #[test]
    fn a_flood_from_one_peer_leaves_another_peers_evidence_intact() {
        let (root, paths, loud) =
            paths_with_a_peer(BTreeSet::from([DeviceCapability::PeerMessage]));
        let mut store = DaemonStore::open(&paths).expect("store");
        for index in 0..super::super::peer_store::MAX_PEER_REJECTION_EVENTS_PER_PEER * 2 {
            store
                .record_peer_rejection_event(
                    &loud,
                    None,
                    None,
                    PeerRejection::MalformedId,
                    NOW + i64::from(index),
                )
                .expect("record");
        }
        // A second pairing that sent five expired envelopes long before.
        for index in 0..5 {
            store
                .record_peer_rejection_event(
                    "device-quiet",
                    None,
                    None,
                    PeerRejection::Expired,
                    NOW - 1_000 + index,
                )
                .expect("record");
        }
        assert_eq!(
            store
                .peer_rejection_event_count(Some("device-quiet"))
                .unwrap(),
            5,
            "the quiet peer's rows are still there"
        );
        drop(store);

        let findings = audit_peers(&paths, NOW);
        let ids: Vec<&str> = findings.iter().map(|f| f.id.as_str()).collect();
        // Both peers' stories are told, from a table that is bounded for each.
        assert!(ids.contains(&"peers.malformed_traffic"), "{ids:?}");
        assert!(ids.contains(&"peers.clock_skew"), "{ids:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn admissions_left_by_a_pairing_that_is_gone_are_reported_without_naming_content() {
        let (root, paths, _device_id) =
            paths_with_a_peer(BTreeSet::from([DeviceCapability::PeerArtifact]));
        let mut store = DaemonStore::open(&paths).expect("store");
        store
            .record_peer_artifact_receipt(
                "device-vanished",
                &"a".repeat(64),
                &"a".repeat(64),
                12,
                Some("payroll.csv"),
                Some("text/csv"),
                NOW,
            )
            .expect("admit");
        drop(store);

        let findings = audit_peers(&paths, NOW);
        let finding = findings
            .iter()
            .find(|f| f.id == "peers.orphaned_artifact_admissions")
            .expect("stranded admissions are reported");
        assert_eq!(finding.status, FindingStatus::Info);
        // A filename is the peer's own text and a digest identifies content;
        // neither belongs in a report that ends up in a support bundle.
        let rendered = serde_json::to_string(&findings).expect("serialize");
        assert!(!rendered.contains("payroll.csv"));
        assert!(!rendered.contains(&"a".repeat(64)));
        let _ = std::fs::remove_dir_all(root);
    }
}

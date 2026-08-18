//! What Security Doctor says about the operator's phone numbers.
//!
//! This lives in the CLI rather than in `little_monkey_lib::security_doctor`
//! for a boundary reason worth stating: the doctor runs inside the library, and
//! telephony state lives in the daemon's own database, which the library cannot
//! open. Teaching the library a second store to reach it would be worse than
//! appending findings from the one process that already has the store open.
//!
//! Everything audited here is a way to spend the operator's money or to let
//! somebody else spend it: a line that answers anyone, a number that can dial
//! out without being asked, callbacks that cannot be verified, a call that has
//! been running far too long, and a carrier that keeps failing.

use little_monkey_lib::security_doctor::{FindingStatus, SecurityFinding};

use crate::daemon::store::{DaemonPaths, DaemonStore};
use crate::daemon::telecom_store::{
    CallbackRejections, InboundCallPolicy, OutboundCallApproval, TelecomAccountRecord,
};
use crate::daemon::telephony::{CallState, TelecomKind};

/// A call still open this long after it started is reported whatever the
/// account's own limit says: at this point the limit itself is not working.
const STALE_CALL_MS: i64 = 4 * 60 * 60 * 1_000;

/// How many refused callbacks in a row stop being noise and start being a
/// misconfiguration. One is a probe or a stray request; several in a row with
/// none succeeding in between is a carrier that cannot reach this machine.
const REJECTED_CALLBACK_THRESHOLD: u32 = 3;

/// Audit every configured telephony account.
///
/// Returns an empty list when there is no daemon state at all, which is the
/// normal case for an operator who has never configured a number — an audit
/// finding about a subsystem nobody uses is noise.
pub(crate) fn telecom_findings(now_ms: i64) -> Vec<SecurityFinding> {
    let Ok(paths) = DaemonPaths::resolve() else {
        return Vec::new();
    };
    let Ok(store) = DaemonStore::open(&paths) else {
        return Vec::new();
    };
    let Ok(accounts) = store.telecom_accounts() else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    // A number set to answer with no transcription backend is a feature that
    // looks on and does nothing: the caller talks and every turn is dropped.
    if accounts.iter().any(|account| {
        account.enabled && !matches!(account.inbound_policy, InboundCallPolicy::Reject)
    }) {
        if let Err(error) = little_monkey_lib::m7_companion::call_speech_readiness(&paths.root) {
            findings.push(f(
                "telephony.no_speech_backend",
                "A number answers calls this machine cannot understand",
                &error,
                FindingStatus::Critical,
                Some("Configure transcription in Settings > Companion, or set the number to reject calls."),
            ));
        }
    }
    for account in accounts.iter().filter(|account| account.enabled) {
        findings.extend(audit_account(account));
        // A carrier posting to a URL whose signature never verifies is the one
        // failure with no other symptom: texts and calls simply never arrive,
        // and every other check on this page passes. It is nearly always the
        // callback URL — a tunnel that moved, a console pointed at the wrong
        // path — or a credential rotated on one side only.
        if let Ok(rejections) = store.callback_rejections(&account.account_id) {
            findings.extend(rejected_callbacks_finding(account, &rejections));
        }
        if let Ok(calls) = store.recent_calls(&account.account_id, 50) {
            for call in calls {
                if matches!(call.state, CallState::InProgress)
                    && now_ms - call.started_at_ms.unwrap_or(call.created_at_ms) > STALE_CALL_MS
                {
                    findings.push(f(
                        &format!("telephony.stale_call.{}", call.call_id),
                        "A call has been open for hours",
                        &format!(
                            "The call with {} on {} is still marked in progress. It may still be billing.",
                            call.peer_number, account.from_number
                        ),
                        FindingStatus::Warning,
                        Some("Check the call in your carrier's console and end it there if it is still up."),
                    ));
                }
                if matches!(call.state, CallState::NeedsReconciliation) {
                    findings.push(f(
                        &format!("telephony.unreconciled_call.{}", call.call_id),
                        "A call was never confirmed either way",
                        &format!(
                            "The call with {} on {} could not be confirmed with the carrier: {}",
                            call.peer_number,
                            account.from_number,
                            call.last_error.as_deref().unwrap_or("no detail recorded")
                        ),
                        FindingStatus::Warning,
                        Some("Reconcile it against your carrier's call log; nothing is retried automatically."),
                    ));
                }
            }
        }
    }
    if findings.is_empty() && accounts.iter().any(|account| account.enabled) {
        findings.push(f(
            "telephony.posture",
            "Phone numbers are configured conservatively",
            "Every enabled number verifies its callbacks, answers only who it is meant to, and needs approval before dialing out.",
            FindingStatus::Pass,
            None,
        ));
    }
    findings
}

/// A carrier whose callbacks never verify.
///
/// The one failure with no other symptom: texts and calls simply never arrive,
/// and every other check on this page passes. It is nearly always the callback
/// URL — a tunnel that moved, a console pointed at the wrong path — or a
/// credential rotated on one side only.
fn rejected_callbacks_finding(
    account: &TelecomAccountRecord,
    rejections: &CallbackRejections,
) -> Option<SecurityFinding> {
    if rejections.count < REJECTED_CALLBACK_THRESHOLD {
        return None;
    }
    Some(f(
        &format!("telephony.rejected_callbacks.{}", account.account_id),
        "A carrier's callbacks are all being refused",
        &format!(
            "{} callback(s) to {} have failed verification since one last succeeded: {}",
            rejections.count,
            account.from_number,
            rejections
                .last_reason
                .as_deref()
                .unwrap_or("no reason recorded")
        ),
        FindingStatus::Critical,
        Some("Check that the URL in your carrier's console is exactly the callback URL shown in Settings > Phone and SMS, and that the credential there matches the one saved here."),
    ))
}

fn audit_account(account: &TelecomAccountRecord) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    let number = &account.from_number;

    if matches!(account.inbound_policy, InboundCallPolicy::Answer) {
        findings.push(f(
            &format!("telephony.open_inbound.{}", account.account_id),
            "A number answers calls from anyone",
            &format!(
                "{number} answers every caller and hands what they say to an agent. Anyone who learns the number can start a conversation with it."
            ),
            FindingStatus::Warning,
            Some("Set it to take a message instead, or keep the number private."),
        ));
    }

    if matches!(account.outbound_approval, OutboundCallApproval::Allow) {
        findings.push(f(
            &format!("telephony.unattended_outbound.{}", account.account_id),
            "A number can call out without asking",
            &format!(
                "{number} places calls with no approval prompt. A call reaches a person who did not ask to be reached, and it bills you."
            ),
            FindingStatus::Critical,
            Some("Set calling out to ask every time."),
        ));
    }

    if account.public_base_url.is_none() {
        findings.push(f(
            &format!("telephony.no_callback_url.{}", account.account_id),
            "A number has no callback URL",
            &format!(
                "{number} is enabled but has nowhere for its carrier to deliver texts or calls."
            ),
            FindingStatus::Warning,
            Some("Add your own public URL in Settings > Phone and SMS."),
        ));
    } else if account
        .public_base_url
        .as_deref()
        .is_some_and(|url| url.starts_with("http://"))
    {
        findings.push(f(
            &format!("telephony.plaintext_callback.{}", account.account_id),
            "A callback URL is not HTTPS",
            &format!(
                "{number} receives carrier callbacks over plain HTTP, so message contents and signatures cross the network in the clear."
            ),
            FindingStatus::Critical,
            Some("Point the carrier at an https:// URL."),
        ));
    }

    // Telnyx verifies callbacks with a key published in its portal. Without
    // that key the account cannot be built at all, which is a misconfiguration
    // an operator should hear about here rather than discover from silence.
    if matches!(account.kind, TelecomKind::Telnyx)
        && account
            .non_secret_config
            .get("webhook_public_key")
            .and_then(|value| value.as_str())
            .is_none_or(str::is_empty)
    {
        findings.push(f(
            &format!("telephony.unverifiable_callbacks.{}", account.account_id),
            "Callbacks for a number cannot be verified",
            &format!(
                "{number} has no webhook public key, so nothing this carrier sends can be checked."
            ),
            FindingStatus::Critical,
            Some("Copy the public key from your carrier's portal into the account's settings."),
        ));
    }

    if account.limits.recording_enabled {
        findings.push(f(
            &format!("telephony.recording.{}", account.account_id),
            "A number records its calls",
            &format!("{number} records calls. Where you live, recording somebody may require telling them or asking them first."),
            FindingStatus::Info,
            Some("Turn recording off unless you need it and are allowed to."),
        ));
    }

    if matches!(
        account.health.state,
        little_monkey_lib::channels::types::HealthState::Error
    ) {
        findings.push(f(
            &format!("telephony.carrier_failing.{}", account.account_id),
            "A carrier keeps refusing this number",
            &format!(
                "{number} last failed with: {}",
                account
                    .health
                    .last_error
                    .as_deref()
                    .unwrap_or("no detail recorded")
            ),
            FindingStatus::Warning,
            Some("Test the connection in Settings > Phone and SMS; the credential may have been rotated."),
        ));
    }

    findings
}

fn f(
    id: &str,
    title: &str,
    detail: &str,
    status: FindingStatus,
    remediation: Option<&str>,
) -> SecurityFinding {
    SecurityFinding {
        id: id.to_string(),
        category: "telephony".to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status,
        // Nothing here is safe to fix automatically: every remedy changes what
        // an operator's own phone number does.
        fixable: false,
        path: None,
        remediation: remediation.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::telecom_store::{CallLimits, CallbackRejections};
    use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

    const NOW: i64 = 1_700_000_000_000;

    fn account() -> TelecomAccountRecord {
        TelecomAccountRecord {
            account_id: "tel-1".into(),
            kind: TelecomKind::Twilio,
            label: "Support line".into(),
            enabled: true,
            carrier_account_id: "AC1".into(),
            from_number: "+15550000000".into(),
            credential_ref: Some("telecom:tel-1".into()),
            public_base_url: Some("https://calls.example.test".into()),
            non_secret_config: serde_json::json!({}),
            inbound_policy: InboundCallPolicy::Voicemail,
            outbound_approval: OutboundCallApproval::Approval,
            limits: CallLimits::default(),
            health: ChannelHealth {
                state: HealthState::Connected,
                detail: None,
                last_error: None,
                probed_at_ms: NOW,
            },
            created_at_ms: NOW,
            updated_at_ms: NOW,
        }
    }

    fn ids(findings: &[SecurityFinding]) -> Vec<String> {
        findings.iter().map(|finding| finding.id.clone()).collect()
    }

    #[test]
    fn a_conservatively_configured_number_raises_nothing() {
        assert!(audit_account(&account()).is_empty());
    }

    #[test]
    fn dialing_out_without_approval_is_critical() {
        let mut account = account();
        account.outbound_approval = OutboundCallApproval::Allow;

        let findings = audit_account(&account);

        let finding = findings
            .iter()
            .find(|finding| finding.id.starts_with("telephony.unattended_outbound"))
            .expect("reported");
        assert_eq!(finding.status, FindingStatus::Critical);
        assert!(
            !finding.fixable,
            "nobody's phone settings change themselves"
        );
    }

    #[test]
    fn answering_anyone_is_reported_but_is_not_by_itself_critical() {
        let mut account = account();
        account.inbound_policy = InboundCallPolicy::Answer;

        let findings = audit_account(&account);

        let finding = findings
            .iter()
            .find(|finding| finding.id.starts_with("telephony.open_inbound"))
            .expect("reported");
        assert_eq!(finding.status, FindingStatus::Warning);
    }

    #[test]
    fn a_plaintext_callback_url_is_critical_and_a_missing_one_is_not() {
        let mut plaintext = account();
        plaintext.public_base_url = Some("http://calls.example.test".into());
        let mut missing = account();
        missing.public_base_url = None;

        assert_eq!(
            audit_account(&plaintext)
                .iter()
                .find(|finding| finding.id.starts_with("telephony.plaintext_callback"))
                .map(|finding| finding.status),
            Some(FindingStatus::Critical)
        );
        assert_eq!(
            audit_account(&missing)
                .iter()
                .find(|finding| finding.id.starts_with("telephony.no_callback_url"))
                .map(|finding| finding.status),
            Some(FindingStatus::Warning)
        );
    }

    #[test]
    fn a_telnyx_account_with_no_published_key_cannot_verify_anything() {
        let mut account = account();
        account.kind = TelecomKind::Telnyx;

        let findings = audit_account(&account);

        assert!(ids(&findings)
            .iter()
            .any(|id| id.starts_with("telephony.unverifiable_callbacks")));
    }

    #[test]
    fn recording_is_reported_as_something_the_operator_should_know_about() {
        let mut account = account();
        account.limits.recording_enabled = true;

        let findings = audit_account(&account);

        let finding = findings
            .iter()
            .find(|finding| finding.id.starts_with("telephony.recording"))
            .expect("reported");
        assert_eq!(finding.status, FindingStatus::Info);
        assert!(finding.detail.contains("require"));
    }

    #[test]
    fn callbacks_that_never_verify_are_reported_once_they_stop_being_noise() {
        let account = account();
        // One stray unsigned request is not a misconfiguration.
        assert_eq!(
            rejected_callbacks_finding(
                &account,
                &CallbackRejections {
                    count: 1,
                    last_reason: Some("missing X-Twilio-Signature header".into()),
                    last_at_ms: Some(1),
                }
            ),
            None
        );

        let finding = rejected_callbacks_finding(
            &account,
            &CallbackRejections {
                count: 9,
                last_reason: Some("Twilio signature verification failed".into()),
                last_at_ms: Some(1),
            },
        )
        .expect("a carrier that cannot reach this machine is worth saying out loud");

        assert_eq!(finding.status, FindingStatus::Critical);
        assert!(finding.detail.contains("9 callback"));
        // The reason names what to fix; without it the operator sees a number
        // that looks configured and simply never rings.
        assert!(finding.detail.contains("signature verification failed"));
        assert!(finding
            .remediation
            .as_deref()
            .unwrap_or_default()
            .contains("console"));
    }

    #[test]
    fn a_carrier_that_keeps_refusing_is_surfaced_with_its_own_reason() {
        let mut account = account();
        account.health.state = HealthState::Error;
        account.health.last_error = Some("401 from the carrier".into());

        let findings = audit_account(&account);

        let finding = findings
            .iter()
            .find(|finding| finding.id.starts_with("telephony.carrier_failing"))
            .expect("reported");
        assert!(finding.detail.contains("401 from the carrier"));
    }
}

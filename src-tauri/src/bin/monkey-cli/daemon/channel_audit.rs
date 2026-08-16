//! What Security Doctor says about the operator's messaging accounts.
//!
//! The messaging counterpart to [`crate::telecom_audit`] and
//! [`super::peer_audit`], and it lives here for the same boundary reason both of
//! those do: the doctor runs inside `little_monkey_lib`, and channel accounts
//! live in the daemon's own SQLite database, which the library cannot open.
//! Teaching the library a second reader for that schema would make the audit
//! quietly wrong at the first migration.
//!
//! # What these checks are about
//!
//! Every one of them is a way for somebody who is not the operator to reach an
//! agent, or for the operator to stop hearing about it when that path breaks:
//!
//! - a conversation anybody can start;
//! - a provider whose deliveries cannot be authenticated, or whose callbacks
//!   have stopped authenticating;
//! - a transport that is failing and has been for a while;
//! - an attachment ceiling raised to where one message can cost real money;
//! - a transport that cannot recognize its own echo, paired with an activation
//!   policy that would answer it;
//! - a helper binary that is not where the account says it is.
//!
//! # What they are deliberately not about
//!
//! Nothing here reads a message, a sender's name, or a credential. A finding
//! names an account, its provider, and the setting at fault. The account label
//! is the operator's own words and is quoted; nothing that arrived over the wire
//! ever is.

use little_monkey_lib::channels::policy::{AccessPolicy, GroupActivation};
use little_monkey_lib::channels::types::{ChannelKind, HealthState, InboundTransport};
use little_monkey_lib::security_doctor::{FindingStatus, SecurityFinding};

use super::adapters::inbound_transport_for;
use super::channel_adapter::AttachmentLimits;
use super::channel_store::ChannelAccountRecord;
use super::store::{DaemonPaths, DaemonStore};

/// How many refused deliveries in a row stop being noise and start being a
/// misconfiguration.
///
/// One is a probe, a scanner, or a provider retrying something stale. Three in a
/// row with none verifying in between is a secret that was rotated on one side
/// or a console pointed somewhere this machine is not.
const REJECTED_CALLBACK_THRESHOLD: u32 = 3;

/// A transport that has been down this long is reported.
///
/// Long enough that a reconnect, a rate-limit backoff, or a provider's own brief
/// outage has finished on its own; short enough that an operator hears about a
/// dead account the same day. A socket adapter reconnects with jitter and a
/// bounded policy, so anything still failing after an hour is failing for a
/// reason no retry will fix.
const STALE_HEALTH_MS: i64 = 60 * 60 * 1_000;

/// The attachment ceiling above which one inbound message is worth mentioning.
///
/// Not the hard cap — [`AttachmentLimits::for_account`] clamps to 64 MiB and no
/// account may exceed it. This is the point at which a stranger's message can
/// make this machine spend enough bandwidth and disk to be worth an operator
/// knowing they chose it.
const LARGE_ATTACHMENT_BYTES: u64 = 32 * 1024 * 1024;

/// Audit every configured messaging account.
///
/// Returns an empty list when there is no daemon state at all, which is the
/// normal case for somebody who has never connected a provider — a finding about
/// a subsystem nobody uses is noise.
pub(crate) fn channel_findings(paths: &DaemonPaths, now_ms: i64) -> Vec<SecurityFinding> {
    let Ok(store) = DaemonStore::open(paths) else {
        return Vec::new();
    };
    let Ok(accounts) = store.channel_accounts() else {
        return Vec::new();
    };
    // The SMS kind is telephony's messaging face: it is created and governed by
    // a telecom account, and `telecom_audit` already reports on that number's
    // inbound policy, its callback URL and its signature verification. Auditing
    // it again here would tell the operator the same thing twice in different
    // words.
    let accounts: Vec<&ChannelAccountRecord> = accounts
        .iter()
        .filter(|account| account.kind != ChannelKind::Sms)
        .collect();
    if accounts.is_empty() {
        return vec![f(
            "channels.none",
            "No messaging accounts are connected",
            "Nothing can reach an agent over a messaging provider.",
            FindingStatus::Pass,
            None,
        )];
    }

    let public_base_url = store.channel_public_base_url().ok().flatten();
    let mut findings = Vec::new();
    for account in accounts.iter().filter(|account| account.enabled) {
        findings.extend(audit_account(account, public_base_url.as_deref(), now_ms));
        if let Ok(rejections) = store.channel_callback_rejections(&account.account_id) {
            if rejections.count >= REJECTED_CALLBACK_THRESHOLD {
                findings.push(f(
                    &format!("channels.rejected_callbacks.{}", account.account_id),
                    "A provider's deliveries are all being refused",
                    &format!(
                        "{} delivery attempt(s) to {} have failed verification since one last \
                         succeeded: {}",
                        rejections.count,
                        describe(account),
                        rejections
                            .last_reason
                            .as_deref()
                            .unwrap_or("no reason recorded")
                    ),
                    FindingStatus::Critical,
                    Some(
                        "Check that the callback URL in the provider's console is exactly the one \
                         shown in Settings > Channels, and that the signing secret there matches \
                         the one saved here.",
                    ),
                ));
            }
        }
    }

    if findings.is_empty() && accounts.iter().any(|account| account.enabled) {
        findings.push(f(
            "channels.posture",
            "Messaging accounts are configured conservatively",
            "Every connected account decides who may talk to it, can authenticate what its \
             provider sends, and is bounded in what one message may cost.",
            FindingStatus::Pass,
            None,
        ));
    }
    findings
}

fn audit_account(
    account: &ChannelAccountRecord,
    public_base_url: Option<&str>,
    now_ms: i64,
) -> Vec<SecurityFinding> {
    let mut findings = Vec::new();
    let transport = inbound_transport_for(account.kind);
    let id = &account.account_id;
    let label = describe(account);

    // -- Who may start a conversation ------------------------------------

    if account.access_policy.direct == AccessPolicy::Open {
        findings.push(f(
            &format!("channels.open_direct.{id}"),
            "Anyone who finds this account can talk to the agent",
            &format!(
                "{label} accepts a direct message from any sender with no pairing and no \
                 approval. Whoever learns the handle gets an agent."
            ),
            FindingStatus::Critical,
            Some("Set direct messages to ask for pairing, or to an approved list, in Settings > Channels."),
        ));
    }
    // An open group policy is a smaller hole than an open direct one — a group
    // has to be joined first — but paired with `Always` it is the same hole
    // through a different door, so the two are reported at different weights
    // rather than one being left out.
    if account.access_policy.group == AccessPolicy::Open {
        let always = account.access_policy.group_activation == GroupActivation::Always;
        findings.push(f(
            &format!("channels.open_group.{id}"),
            "Any member of a shared conversation can drive the agent",
            &format!(
                "{label} accepts messages from every member of a group or channel it is in{}.",
                if always {
                    ", and answers every message rather than waiting to be addressed"
                } else {
                    ""
                }
            ),
            if always {
                FindingStatus::Critical
            } else {
                FindingStatus::Warning
            },
            Some("Restrict group senders to an approved list in Settings > Channels."),
        ));
    }

    // -- Can what arrives be authenticated at all -------------------------

    if account.credential_ref.is_none() {
        findings.push(f(
            &format!("channels.no_credential.{id}"),
            "An account is enabled with no credential",
            &format!(
                "{label} is switched on but has no saved credential, so it can neither \
                 authenticate what arrives nor send anything."
            ),
            FindingStatus::Warning,
            Some("Add the provider's token or signing secret in Settings > Channels, or switch the account off."),
        ));
    }

    // -- Where the provider is told to deliver ----------------------------

    if transport == InboundTransport::Webhook {
        match public_base_url {
            None => findings.push(f(
                &format!("channels.no_callback_url.{id}"),
                "A provider has nowhere to deliver messages",
                &format!(
                    "{label} receives by webhook, but no public URL is configured, so the \
                     callback address in its console cannot match this machine."
                ),
                FindingStatus::Warning,
                Some("Add your own public URL in Settings > Channels, then paste the callback address it shows into the provider's console."),
            )),
            Some(base) if base.starts_with("http://") => findings.push(f(
                &format!("channels.plaintext_callback.{id}"),
                "A callback URL is not HTTPS",
                &format!(
                    "{label} receives deliveries over plain HTTP, so message contents and the \
                     signatures over them cross the network in the clear."
                ),
                FindingStatus::Critical,
                Some("Point the provider at an https:// URL."),
            )),
            Some(_) => {}
        }
    }

    // -- A transport that is not working ----------------------------------

    let down_for = now_ms.saturating_sub(account.health.probed_at_ms);
    match account.health.state {
        HealthState::Error if down_for >= STALE_HEALTH_MS => findings.push(f(
            &format!("channels.reconnect_failing.{id}"),
            "An account has been unable to connect for hours",
            &format!(
                "{label} has been failing since its last probe and has not recovered: {}",
                account
                    .health
                    .last_error
                    .as_deref()
                    .unwrap_or("no detail recorded")
            ),
            FindingStatus::Warning,
            Some("Test the connection in Settings > Channels; the credential may have been rotated or revoked."),
        )),
        HealthState::Degraded if down_for >= STALE_HEALTH_MS => findings.push(f(
            &format!("channels.degraded.{id}"),
            "An account has been degraded for hours",
            &format!(
                "{label} is connected but impaired, and has been for a while: {}",
                account
                    .health
                    .detail
                    .as_deref()
                    .or(account.health.last_error.as_deref())
                    .unwrap_or("no detail recorded")
            ),
            FindingStatus::Info,
            Some("Check the provider's status and the account's rate limits in Settings > Channels."),
        )),
        _ => {}
    }

    // -- What one inbound message may cost --------------------------------

    let limits = AttachmentLimits::for_account(&account.non_secret_config);
    if limits.max_bytes >= LARGE_ATTACHMENT_BYTES {
        findings.push(f(
            &format!("channels.large_attachments.{id}"),
            "One message may download a very large file",
            &format!(
                "{label} will fetch attachments up to {} MiB each, up to {} per message. A sender \
                 chooses when that is spent.",
                limits.max_bytes / (1024 * 1024),
                limits.max_listed
            ),
            FindingStatus::Info,
            Some("Lower this account's attachment size limit in Settings > Channels if it does not need files that large."),
        ));
    }

    // -- Could it answer itself -------------------------------------------

    findings.extend(own_echo_finding(account, &label));

    // -- Is the helper it needs actually there ----------------------------

    if transport == InboundTransport::Helper {
        findings.extend(helper_path_finding(account, &label));
    }

    findings
}

/// Whether this account could end up in a conversation with a machine.
///
/// Two different risks share one finding because the remedy is the same. The
/// first is our own echo: a transport that cannot tell its own outbound message
/// apart from somebody else's inbound one will answer itself forever. The second
/// is another bot: `sender.is_bot` bounds an automated exchange at
/// `MAX_AUTOMATED_REPLY_DEPTH`, but only where the provider reports it, and only
/// a group set to answer everything can get into one unprompted.
///
/// Reported only when the activation policy could actually produce the loop. An
/// account that answers only when addressed cannot start one, whatever it can or
/// cannot recognize.
fn own_echo_finding(account: &ChannelAccountRecord, label: &str) -> Option<SecurityFinding> {
    if !recognizes_own_echo(account.kind) {
        let exposed = account.access_policy.group_activation == GroupActivation::Always
            || account.access_policy.direct == AccessPolicy::Open;
        return Some(f(
            &format!("channels.own_echo_blind.{}", account.account_id),
            "An account cannot always recognize its own messages",
            &format!(
                "{label} cannot tell every message it sent apart from one it received, so a \
                 message of its own coming back could be answered as if somebody had sent it.{}",
                if exposed {
                    " Its current settings would answer such a message."
                } else {
                    " Its current settings answer only when it is addressed, which prevents this \
                      today."
                }
            ),
            if exposed {
                FindingStatus::Warning
            } else {
                FindingStatus::Info
            },
            Some("Keep this account answering only when it is addressed, and keep direct messages behind pairing."),
        ));
    }
    None
}

/// Whether this provider's adapter can identify a message as our own.
///
/// True where the adapter resolves its own identity and compares it, or where
/// the source filters our own messages before they are ever parsed. False where
/// the wire format this build reads carries no such signal — which is a real
/// property of the integration, not an oversight, and is why the audit reports
/// it rather than the code pretending otherwise.
fn recognizes_own_echo(kind: ChannelKind) -> bool {
    match kind {
        // Each of these resolves its own account/bot/nick at connect time and
        // compares the sender against it.
        ChannelKind::Telegram
        | ChannelKind::Discord
        | ChannelKind::Slack
        | ChannelKind::Mattermost
        | ChannelKind::Irc
        | ChannelKind::Matrix => true,
        // The helper filters on `is_from_me` in the query itself, so our own
        // messages never reach the parser.
        ChannelKind::IMessage => true,
        // A webhook delivery is inbound by construction: these providers do not
        // post our own outbound messages back to the callback.
        ChannelKind::WhatsApp
        | ChannelKind::Teams
        | ChannelKind::GoogleChat
        | ChannelKind::Line => true,
        // A carrier does not deliver our own outbound text to the inbound
        // webhook either, and telephony's own audit covers the number.
        ChannelKind::Sms => true,
        // signal-cli reports a linked device's own sends as a sync
        // notification rather than a `dataMessage`, and the parser reads both.
        ChannelKind::Signal => true,
        // The guest decides what it reports and the host cannot verify it.
        ChannelKind::Extension => false,
    }
}

/// A helper transport whose binary is not where the account says it is.
///
/// Checked as a path rather than by asking the adapter, because the adapter
/// cannot be built without the credential and this is precisely the failure of
/// an account that looks configured and receives nothing.
fn helper_path_finding(account: &ChannelAccountRecord, label: &str) -> Option<SecurityFinding> {
    let configured = account
        .non_secret_config
        .get("helper_path")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    if configured.is_empty() {
        return Some(f(
            &format!("channels.helper_missing.{}", account.account_id),
            "An account has no helper program configured",
            &format!("{label} needs a local helper program to receive anything, and none is set."),
            FindingStatus::Warning,
            Some("Set the helper path in Settings > Channels, or switch the account off."),
        ));
    }
    let path = std::path::Path::new(configured);
    if !path.is_absolute() {
        return Some(f(
            &format!("channels.helper_relative.{}", account.account_id),
            "A helper program is named by a relative path",
            &format!(
                "{label} names its helper as '{configured}', which is resolved against whatever \
                 directory the daemon happens to be started in — so which program runs depends on \
                 how the daemon was launched."
            ),
            FindingStatus::Critical,
            Some("Give the full path to the helper program in Settings > Channels."),
        ));
    }
    if !path.is_file() {
        return Some(f(
            &format!("channels.helper_absent.{}", account.account_id),
            "A helper program is not where the account says it is",
            &format!("{label} points at '{configured}', and there is no file there."),
            FindingStatus::Warning,
            Some("Correct the helper path in Settings > Channels, or reinstall the helper."),
        ));
    }
    None
}

/// How a finding names an account: the operator's own label, and the provider.
///
/// The label is theirs and is safe to quote. Nothing from the wire is.
fn describe(account: &ChannelAccountRecord) -> String {
    format!("{} ({})", account.label, account.kind.label())
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
        category: "channels".to_string(),
        title: title.to_string(),
        detail: detail.to_string(),
        status,
        fixable: false,
        path: None,
        remediation: remediation.map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::ChannelHealth;

    const NOW: i64 = 1_800_000_000_000;

    /// A private daemon root, so nothing here touches the machine's real state.
    /// The `PathBuf` is returned only so a test can put a file beside it.
    fn store() -> (std::path::PathBuf, DaemonPaths) {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-channel-audit-{}",
            uuid::Uuid::new_v4()
        ));
        let paths = DaemonPaths::under(&root);
        paths.ensure().expect("paths");
        DaemonStore::open(&paths).expect("open");
        (root, paths)
    }

    fn account(id: &str, kind: ChannelKind) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: id.to_string(),
            kind,
            label: "Work".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some(format!("test:{id}")),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth::connected(NOW, None),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn seed(paths: &DaemonPaths, account: &ChannelAccountRecord) {
        let mut store = DaemonStore::open(paths).expect("open");
        store.upsert_channel_account(account).expect("upsert");
    }

    fn ids(findings: &[SecurityFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.id.as_str()).collect()
    }

    #[test]
    fn no_accounts_reports_a_pass_rather_than_nothing() {
        let (_root, paths) = store();
        let findings = channel_findings(&paths, NOW);
        assert_eq!(ids(&findings), vec!["channels.none"]);
    }

    #[test]
    fn a_conservative_account_reports_only_its_posture() {
        let (_root, paths) = store();
        seed(&paths, &account("acct-1", ChannelKind::Telegram));
        let findings = channel_findings(&paths, NOW);
        assert_eq!(ids(&findings), vec!["channels.posture"]);
    }

    #[test]
    fn an_open_direct_policy_is_critical() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Telegram);
        account.access_policy.direct = AccessPolicy::Open;
        seed(&paths, &account);
        let findings = channel_findings(&paths, NOW);
        let open = findings
            .iter()
            .find(|f| f.id == "channels.open_direct.acct-1")
            .expect("open direct finding");
        assert_eq!(open.status, FindingStatus::Critical);
    }

    /// An open group that answers everything is the same exposure as an open
    /// direct policy, and is reported at the same weight.
    #[test]
    fn an_open_group_that_answers_everything_is_critical() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Discord);
        account.access_policy.group = AccessPolicy::Open;
        account.access_policy.group_activation = GroupActivation::Always;
        seed(&paths, &account);
        let findings = channel_findings(&paths, NOW);
        let open = findings
            .iter()
            .find(|f| f.id == "channels.open_group.acct-1")
            .expect("open group finding");
        assert_eq!(open.status, FindingStatus::Critical);
        assert!(open.detail.contains("rather than waiting to be addressed"));
    }

    #[test]
    fn a_webhook_account_with_no_public_url_is_reported() {
        let (_root, paths) = store();
        seed(&paths, &account("acct-1", ChannelKind::WhatsApp));
        let findings = channel_findings(&paths, NOW);
        assert!(ids(&findings).contains(&"channels.no_callback_url.acct-1"));
    }

    #[test]
    fn a_plaintext_public_url_is_critical() {
        let (_root, paths) = store();
        seed(&paths, &account("acct-1", ChannelKind::Line));
        DaemonStore::open(&paths)
            .expect("open")
            .set_channel_public_base_url(Some("http://tunnel.example.test"))
            .expect("set base");
        let findings = channel_findings(&paths, NOW);
        let plaintext = findings
            .iter()
            .find(|f| f.id == "channels.plaintext_callback.acct-1")
            .expect("plaintext finding");
        assert_eq!(plaintext.status, FindingStatus::Critical);
    }

    /// A socket provider is not asked about a callback URL it never uses.
    #[test]
    fn a_socket_account_is_not_asked_about_a_callback_url() {
        let (_root, paths) = store();
        seed(&paths, &account("acct-1", ChannelKind::Slack));
        let findings = channel_findings(&paths, NOW);
        assert!(!ids(&findings)
            .iter()
            .any(|id| id.starts_with("channels.no_callback_url")));
    }

    #[test]
    fn refused_deliveries_are_reported_once_they_are_a_streak() {
        let (_root, paths) = store();
        seed(&paths, &account("acct-1", ChannelKind::WhatsApp));
        let mut store = DaemonStore::open(&paths).expect("open");
        store
            .record_channel_callback_rejection("acct-1", "signature mismatch", NOW)
            .expect("record");
        drop(store);
        // One is a probe, not a finding.
        assert!(!ids(&channel_findings(&paths, NOW))
            .iter()
            .any(|id| id.starts_with("channels.rejected_callbacks")));

        let mut store = DaemonStore::open(&paths).expect("open");
        for _ in 0..2 {
            store
                .record_channel_callback_rejection("acct-1", "signature mismatch", NOW)
                .expect("record");
        }
        drop(store);
        let findings = channel_findings(&paths, NOW);
        let refused = findings
            .iter()
            .find(|f| f.id == "channels.rejected_callbacks.acct-1")
            .expect("rejected finding");
        assert_eq!(refused.status, FindingStatus::Critical);
        assert!(refused.detail.contains("signature mismatch"));

        // And one that verifies clears the streak.
        DaemonStore::open(&paths)
            .expect("open")
            .clear_channel_callback_rejections("acct-1")
            .expect("clear");
        assert!(!ids(&channel_findings(&paths, NOW))
            .iter()
            .any(|id| id.starts_with("channels.rejected_callbacks")));
    }

    #[test]
    fn a_transport_down_for_hours_is_reported_and_a_fresh_failure_is_not() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Discord);
        account.health = ChannelHealth {
            state: HealthState::Error,
            detail: None,
            last_error: Some("gateway refused the token".to_string()),
            probed_at_ms: NOW - STALE_HEALTH_MS - 1,
        };
        seed(&paths, &account);
        let findings = channel_findings(&paths, NOW);
        let failing = findings
            .iter()
            .find(|f| f.id == "channels.reconnect_failing.acct-1")
            .expect("reconnect finding");
        assert!(failing.detail.contains("gateway refused the token"));

        // The same failure a minute old is a reconnect in progress, not a
        // finding: reporting it would fire on every transient blip.
        let mut fresh = account.clone();
        fresh.health.probed_at_ms = NOW - 60_000;
        seed(&paths, &fresh);
        assert!(!ids(&channel_findings(&paths, NOW))
            .iter()
            .any(|id| id.starts_with("channels.reconnect_failing")));
    }

    #[test]
    fn a_raised_attachment_ceiling_is_reported() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Telegram);
        account.non_secret_config = serde_json::json!({ "max_attachment_bytes": 64 * 1024 * 1024 });
        seed(&paths, &account);
        let findings = channel_findings(&paths, NOW);
        let large = findings
            .iter()
            .find(|f| f.id == "channels.large_attachments.acct-1")
            .expect("attachment finding");
        assert!(large.detail.contains("64 MiB"));
    }

    /// The two halves of the echo finding: a provider this build cannot filter,
    /// weighted by whether the account's own settings would answer the echo.
    ///
    /// An extension provider is the case: what a guest reports about a sender
    /// is the guest's word, and the host has no way to check it.
    #[test]
    fn an_echo_blind_provider_is_weighted_by_what_it_would_answer() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Extension);
        account.non_secret_config = serde_json::json!({
            "extension_id": "ext-1",
            "capability_id": "cap-1",
        });
        seed(&paths, &account);
        let quiet = channel_findings(&paths, NOW);
        let finding = quiet
            .iter()
            .find(|f| f.id == "channels.own_echo_blind.acct-1")
            .expect("echo finding");
        assert_eq!(finding.status, FindingStatus::Info);

        account.access_policy.group_activation = GroupActivation::Always;
        seed(&paths, &account);
        let loud = channel_findings(&paths, NOW);
        let finding = loud
            .iter()
            .find(|f| f.id == "channels.own_echo_blind.acct-1")
            .expect("echo finding");
        assert_eq!(finding.status, FindingStatus::Warning);
    }

    #[test]
    fn a_provider_that_filters_its_own_echo_is_not_reported() {
        // Signal is here deliberately: its parser reads the sync notification a
        // linked device's own send arrives as, so it belongs on this side of
        // the line rather than the other.
        for kind in [ChannelKind::Telegram, ChannelKind::Signal] {
            let (root, paths) = store();
            let helper = root.join("signal-cli");
            std::fs::write(&helper, b"#!/bin/sh\n").expect("write helper");
            let mut account = account("acct-1", kind);
            account.access_policy.group_activation = GroupActivation::Always;
            if kind == ChannelKind::Signal {
                account.non_secret_config = serde_json::json!({
                    "helper_path": helper.to_string_lossy(),
                    "account": "+15550000000",
                });
            }
            seed(&paths, &account);
            assert!(
                !ids(&channel_findings(&paths, NOW))
                    .iter()
                    .any(|id| id.starts_with("channels.own_echo_blind")),
                "{kind:?}"
            );
        }
    }

    /// A relative helper path is worse than a missing one: which program runs
    /// depends on the daemon's working directory.
    #[test]
    fn a_relative_helper_path_is_critical_and_a_missing_file_is_a_warning() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Signal);
        account.non_secret_config = serde_json::json!({
            "helper_path": "signal-cli",
            "account": "+15550000000",
        });
        seed(&paths, &account);
        let relative = channel_findings(&paths, NOW);
        let finding = relative
            .iter()
            .find(|f| f.id == "channels.helper_relative.acct-1")
            .expect("relative finding");
        assert_eq!(finding.status, FindingStatus::Critical);

        account.non_secret_config = serde_json::json!({
            "helper_path": "/definitely/not/here/signal-cli",
            "account": "+15550000000",
        });
        seed(&paths, &account);
        let absent = channel_findings(&paths, NOW);
        let finding = absent
            .iter()
            .find(|f| f.id == "channels.helper_absent.acct-1")
            .expect("absent finding");
        assert_eq!(finding.status, FindingStatus::Warning);
    }

    #[test]
    fn a_helper_that_exists_is_not_reported() {
        let (root, paths) = store();
        let helper = root.join("signal-cli");
        std::fs::write(&helper, b"#!/bin/sh\n").expect("write helper");
        let mut account = account("acct-1", ChannelKind::Signal);
        account.non_secret_config = serde_json::json!({
            "helper_path": helper.to_string_lossy(),
            "account": "+15550000000",
        });
        seed(&paths, &account);
        assert!(!ids(&channel_findings(&paths, NOW))
            .iter()
            .any(|id| id.starts_with("channels.helper_")));
    }

    /// An account switched off is not audited: it cannot receive anything, and
    /// a finding about it is a finding about a setting nobody is using.
    #[test]
    fn a_disabled_account_is_not_audited() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Telegram);
        account.enabled = false;
        account.access_policy.direct = AccessPolicy::Open;
        seed(&paths, &account);
        assert!(!ids(&channel_findings(&paths, NOW))
            .iter()
            .any(|id| id.starts_with("channels.open_direct")));
    }

    /// SMS is telephony's messaging face and `telecom_audit` already reports on
    /// the number behind it. Saying the same thing twice in different words is
    /// how an operator learns to skip the page.
    #[test]
    fn the_sms_kind_is_left_to_the_telephony_audit() {
        let (_root, paths) = store();
        let mut account = account("acct-sms", ChannelKind::Sms);
        account.access_policy.direct = AccessPolicy::Open;
        seed(&paths, &account);
        assert_eq!(ids(&channel_findings(&paths, NOW)), vec!["channels.none"]);
    }

    #[test]
    fn an_enabled_account_with_no_credential_is_reported() {
        let (_root, paths) = store();
        let mut account = account("acct-1", ChannelKind::Telegram);
        account.credential_ref = None;
        seed(&paths, &account);
        assert!(ids(&channel_findings(&paths, NOW)).contains(&"channels.no_credential.acct-1"));
    }
}

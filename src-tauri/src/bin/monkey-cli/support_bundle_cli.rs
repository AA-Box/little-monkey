//! Assembles the redacted support bundle from the stores that hold the truth.
//!
//! The library owns the *shape* — see `little_monkey_lib::support_bundle`, and
//! in particular why an identifier can only ever be a `Pseudonym`. This owns the
//! *reading*, for the same boundary reason `channel_audit`, `telecom_audit` and
//! `peer_audit` live here: these schemas belong to this binary, and a second
//! reader in the library would be a second copy of them.
//!
//! # The rule every reader in this file follows
//!
//! Take the row's shape, never its contents. A message row contributes its
//! direction, its state and its timing; its `text` is not read at all. A call
//! contributes its state transitions; its `opening_line` is not read. A device
//! command contributes its capability and outcome; its `arguments` and `result`
//! are not read.
//!
//! That is not a redaction pass over a fuller extract — the fuller extract is
//! never built. A pass that runs afterwards is one forgotten field away from
//! shipping somebody's messages, and the forgotten field is always the one added
//! last.

use little_monkey_lib::support_bundle::{
    bounded_reason, Redactor, SupportBundle, TraceEvent, TraceSection,
    SUPPORT_BUNDLE_SCHEMA_VERSION,
};

use crate::daemon::store::{DaemonPaths, DaemonStore};

/// How many rows each reader asks its store for.
///
/// Above `MAX_SECTION_EVENTS`, so the "omitted" count in a capped section is a
/// real number rather than always zero — a reader needs to know their window is
/// only part of the story.
const READ_LIMIT: u32 = 150;

/// Build the bundle.
///
/// A subsystem whose store cannot be opened contributes an *unavailable*
/// section naming the reason, because "this could not be read" and "this had
/// nothing in it" are different answers and a bundle that blurs them sends
/// somebody looking in the wrong place. The one whole-bundle failure is a
/// machine that cannot produce a salt — see below.
pub(crate) fn collect(app_version: &str) -> Result<SupportBundle, String> {
    // No redactor, no bundle. A fixed or zeroed fallback salt would turn every
    // pseudonym into a stable global identifier for the number behind it, while
    // the document went on claiming its identifiers were pseudonymized — which
    // is worse than producing nothing.
    let redactor = Redactor::new()?;
    let mut sections = std::collections::BTreeMap::new();

    match DaemonPaths::resolve() {
        Err(error) => {
            let reason = bounded_reason(&error);
            for name in ["channels", "telephony", "peers", "callback_exposure"] {
                sections.insert(name.to_string(), TraceSection::unavailable(reason.clone()));
            }
            sections.insert(
                "devices".to_string(),
                TraceSection::unavailable(reason.clone()),
            );
        }
        Ok(paths) => {
            match DaemonStore::open(&paths) {
                Err(error) => {
                    let reason = bounded_reason(&error);
                    for name in ["channels", "telephony", "peers", "callback_exposure"] {
                        sections
                            .insert(name.to_string(), TraceSection::unavailable(reason.clone()));
                    }
                }
                Ok(store) => {
                    sections.insert("channels".to_string(), channel_section(&store, &redactor));
                    sections.insert(
                        "telephony".to_string(),
                        telephony_section(&store, &redactor),
                    );
                    sections.insert("peers".to_string(), peer_section(&store, &redactor));
                    sections.insert("callback_exposure".to_string(), exposure_section(&store));
                }
            }
            sections.insert("devices".to_string(), device_section(&paths, &redactor));
        }
    }

    Ok(SupportBundle {
        schema_version: SUPPORT_BUNDLE_SCHEMA_VERSION,
        generated_at_ms: u64::try_from(now_ms()).unwrap_or_default(),
        app_version: app_version.to_string(),
        platform: std::env::consts::OS.to_string(),
        redaction: Default::default(),
        sections,
    })
}

/// How this machine is reachable from the internet, and what its tunnel is
/// doing.
///
/// Takes no redactor, and that is not an oversight: there is nothing here to
/// pseudonymize. The hostname is the operator's own published address — it is
/// already in a provider console and on the settings page — and everything else
/// is a state name, a count and a bounded failure reason the supervisor already
/// stripped the credential out of. The token itself has no field to travel in.
fn exposure_section(store: &DaemonStore) -> TraceSection {
    let status = crate::daemon::callback_exposure::status(
        store,
        &crate::daemon::channel_adapter::KeyringChannelSecrets,
    );
    TraceSection::from_events(vec![TraceEvent {
        at_ms: status.since_ms.unwrap_or(0),
        event: match status.state {
            crate::daemon::callback_exposure::ExposureState::Connected => {
                "callback_exposure_connected"
            }
            crate::daemon::callback_exposure::ExposureState::Stopped
            | crate::daemon::callback_exposure::ExposureState::NotConfigured => {
                "callback_exposure_stopped"
            }
            crate::daemon::callback_exposure::ExposureState::Connecting => {
                "callback_exposure_starting"
            }
            _ => "callback_exposure_failed",
        }
        .to_string(),
        // No subject and no context: both are `Pseudonym`, which is the type
        // system enforcing that anything identifying has been through the
        // redactor. There is nothing identifying here to put in one -- the
        // mode, the state and the restart count are this codebase's own
        // vocabulary, so they belong in `outcome` and `reason`.
        subject: None,
        context: None,
        outcome: Some(format!(
            "{}/{} after {} restart(s)",
            match status.mode {
                crate::daemon::callback_exposure::ExposureMode::Manual => "manual",
                crate::daemon::callback_exposure::ExposureMode::ManagedTunnel => status
                    .provider
                    .map(|provider| provider.as_str())
                    .unwrap_or("managed"),
            },
            status.state.as_str(),
            status.restarts,
        )),
        reason: status.last_error.as_deref().map(bounded_reason),
    }])
}

/// Inbound normalization, access, routing and the outbox, per account.
///
/// The four questions this answers are the four that go wrong: did the message
/// arrive, was the sender allowed, did it become a run, and did the reply leave.
/// `envelope_json` is *not* parsed for its text — only the disposition columns
/// the gate already wrote are read.
fn channel_section(store: &DaemonStore, redactor: &Redactor) -> TraceSection {
    let accounts = match store.channel_accounts() {
        Ok(accounts) => accounts,
        Err(error) => return TraceSection::unavailable(bounded_reason(&error)),
    };
    let mut events = Vec::new();
    for account in &accounts {
        let account_token = redactor.pseudonym("account", &account.account_id);
        events.push(TraceEvent {
            at_ms: account.health.probed_at_ms,
            event: format!("account.{}", account.kind.as_str()),
            subject: Some(account_token.clone()),
            context: None,
            outcome: Some(account.health.state.as_str().to_string()),
            reason: account.health.last_error.as_deref().map(bounded_reason),
        });
        // How this account decides one of its own messages coming back, and
        // how many ids it is holding to do it with. A *count*, never an id: a
        // provider message id names a specific message in a specific
        // conversation, which is the kind of thing this format has no field
        // for on purpose.
        let correlation = crate::daemon::channel_adapter::echo_correlation_for(account);
        events.push(TraceEvent {
            at_ms: account.updated_at_ms,
            event: "extension_echo_correlation".to_string(),
            subject: Some(account_token.clone()),
            // A count, never an id: a provider message id names one specific
            // message in one specific conversation, and this format has no
            // field it could travel in.
            context: None,
            outcome: Some(format!(
                "{} ({} id(s) recorded)",
                correlation.as_str(),
                store
                    .outbound_echo_count(&account.account_id)
                    .unwrap_or_default()
            )),
            reason: (!correlation.is_host_verifiable())
                .then(|| "reply policy is restricted".to_string()),
        });
        let Ok(rows) = store.recent_channel_events(&account.account_id, READ_LIMIT) else {
            continue;
        };
        for row in rows {
            events.push(TraceEvent {
                at_ms: row.received_at_ms,
                event: format!("{}.{}", row.direction.as_str(), row.disposition.as_str()),
                // The sender is a handle somebody else chose and is exactly the
                // sort of thing a bundle must not carry in the clear.
                subject: redactor.optional("sender", row.sender_id.as_deref()),
                context: Some(redactor.pseudonym("conversation", &row.conversation_id)),
                // Whether it became a run at all -- the join a reader is
                // usually looking for, without naming the run.
                outcome: Some(
                    match (row.job_id.is_some(), row.ingress_id.is_some()) {
                        (true, _) => "queued",
                        (false, true) => "accepted_not_queued",
                        (false, false) => "no_turn",
                    }
                    .to_string(),
                ),
                reason: row.ignore_reason.as_deref().map(bounded_reason),
            });
        }
    }
    TraceSection::from_events(events)
}

/// Calls and texts, as state and timing only.
///
/// A phone number is the single most identifying thing this app touches, so it
/// is pseudonymized everywhere it appears — including the operator's own. The
/// message `text` column and a call's `opening_line` are never read.
fn telephony_section(store: &DaemonStore, redactor: &Redactor) -> TraceSection {
    let accounts = match store.telecom_accounts() {
        Ok(accounts) => accounts,
        Err(error) => return TraceSection::unavailable(bounded_reason(&error)),
    };
    let mut events = Vec::new();
    for account in &accounts {
        let number = redactor.pseudonym("phone", &account.from_number);
        events.push(TraceEvent {
            at_ms: account.health.probed_at_ms,
            event: "number.health".to_string(),
            subject: Some(number.clone()),
            context: None,
            outcome: Some(account.health.state.as_str().to_string()),
            reason: account.health.last_error.as_deref().map(bounded_reason),
        });
        if let Ok(calls) = store.recent_calls(&account.account_id, READ_LIMIT) {
            for call in calls {
                events.push(TraceEvent {
                    at_ms: call.updated_at_ms,
                    event: format!("call.{}", call.direction.as_str()),
                    subject: Some(redactor.pseudonym("phone", &call.peer_number)),
                    // The call itself is the context, so its own progress and
                    // the texts around it read as one sequence.
                    context: Some(redactor.pseudonym("call", &call.call_id)),
                    outcome: Some(call.state.as_str().to_string()),
                    reason: call.last_error.as_deref().map(bounded_reason),
                });
            }
        }
        if let Ok(messages) = store.recent_telecom_messages(&account.account_id, READ_LIMIT) {
            for message in messages {
                events.push(TraceEvent {
                    at_ms: message.at_ms,
                    event: format!("sms.{}", message.direction.as_str()),
                    subject: Some(redactor.pseudonym("phone", &message.peer_number)),
                    context: Some(number.clone()),
                    // Both halves: what this machine did with it, and what the
                    // carrier later said happened to it. A send that succeeded
                    // and was never delivered is the case worth seeing.
                    outcome: Some(match message.delivery_state.as_deref() {
                        Some(delivery) => format!("{}/{delivery}", message.state),
                        None => message.state.clone(),
                    }),
                    reason: message.error.as_deref().map(bounded_reason),
                });
            }
        }
    }
    TraceSection::from_events(events)
}

/// Peer threads and what this installation sent to them.
///
/// `result_text` — a peer's answer to a task — is not read. Neither is any
/// message body.
fn peer_section(store: &DaemonStore, redactor: &Redactor) -> TraceSection {
    let mut events = Vec::new();
    match store.peer_threads(None, READ_LIMIT) {
        Ok(threads) => {
            for thread in threads {
                events.push(TraceEvent {
                    at_ms: thread.last_activity_at_ms,
                    event: "peer.thread".to_string(),
                    subject: Some(redactor.pseudonym("peer", &thread.peer_device_id)),
                    context: Some(redactor.pseudonym("thread", &thread.thread_id)),
                    outcome: None,
                    reason: None,
                });
            }
        }
        Err(error) => return TraceSection::unavailable(bounded_reason(&error)),
    }
    if let Ok(sent) = store.outbound_peer_messages(None, READ_LIMIT) {
        for message in sent {
            events.push(TraceEvent {
                at_ms: message.checked_at_ms.unwrap_or(message.sent_at_ms),
                event: format!("peer.sent.{}", message.kind),
                subject: Some(redactor.pseudonym("peer", &message.alias)),
                context: Some(redactor.pseudonym("thread", &message.thread_id)),
                outcome: Some(message.state.clone()),
                reason: None,
            });
        }
    }
    TraceSection::from_events(events)
}

/// Physical commands sent to a paired device.
///
/// `arguments` and `result` are never read: a command's arguments are where a
/// text-to-send or a location lives, and its result is where a photo's artifact
/// and a clipboard's contents do.
fn device_section(paths: &DaemonPaths, redactor: &Redactor) -> TraceSection {
    let store = match crate::daemon::remote::store::RemoteStore::open(&paths.root) {
        Ok(store) => store,
        Err(error) => return TraceSection::unavailable(bounded_reason(&error)),
    };
    // Recent commands whatever state they reached, not the active set. A
    // postmortem is almost always about a command that already finished or
    // failed, and reading only what is still in flight produces a trace that is
    // empty exactly when somebody needs it.
    let commands = match store.recent_device_commands(READ_LIMIT) {
        Ok(commands) => commands,
        Err(error) => return TraceSection::unavailable(bounded_reason(&error)),
    };
    let events = commands
        .into_iter()
        .map(|command| TraceEvent {
            at_ms: i64::try_from(command.updated_at_ms).unwrap_or(i64::MAX),
            event: format!(
                "device.{}",
                serde_json::to_value(command.capability)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_else(|| "unknown".to_string())
            ),
            subject: Some(redactor.pseudonym("device", &command.device_id)),
            context: Some(redactor.pseudonym("command", &command.command_id)),
            outcome: Some(command.state.as_str().to_string()),
            reason: command.error.as_deref().map(bounded_reason),
        })
        .collect();
    TraceSection::from_events(events)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::types::{ChannelHealth, ChannelKind};

    use crate::daemon::channel_store::{
        ChannelAccountRecord, EventDirection, EventDisposition, NewChannelEvent,
    };
    use crate::daemon::telecom_store::{
        CallLimits, InboundCallPolicy, OutboundCallApproval, TelecomAccountRecord,
    };

    const NOW: i64 = 1_800_000_000_000;
    const NUMBER: &str = "+15550001111";
    const PEER: &str = "+15559998888";
    const BODY: &str = "the secret plan is at 3pm";
    /// What a notification command carries -- the words shown on somebody's
    /// lock screen, which is exactly what a bundle must never carry.
    const SECRET_PAYLOAD: &str = "hunter2-the-actual-clipboard-contents";

    fn channel_account(id: &str, kind: ChannelKind) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: id.into(),
            kind,
            label: "Work".into(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some(format!("test:{id}")),
            access_policy: Default::default(),
            health: ChannelHealth::connected(NOW, None),
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn inbound_event(
        account_id: &str,
        provider_event_id: &str,
        sender: &str,
        conversation: &str,
    ) -> NewChannelEvent {
        NewChannelEvent {
            account_id: account_id.into(),
            source: little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
            direction: EventDirection::Inbound,
            provider_event_id: provider_event_id.into(),
            conversation_id: conversation.into(),
            thread_id: None,
            sender_id: Some(sender.into()),
            // Carries the body, exactly as production does: the point of the
            // test is that the reader does not go looking for it.
            envelope_json: serde_json::json!({ "text": BODY }).to_string(),
            disposition: EventDisposition::Accepted,
            received_at_ms: NOW,
        }
    }

    fn seeded() -> DaemonStore {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .upsert_channel_account(&channel_account("acct-1", ChannelKind::Telegram))
            .expect("account");
        store
            .record_channel_event(&inbound_event("acct-1", "evt-1", "user-ada", "chat-42"))
            .expect("event");

        // A phone number, and a text on it. `recent_telecom_messages` is a
        // read-side join over the messaging tables, so the text is seeded the
        // way a carrier callback would leave it.
        store
            .upsert_telecom_account(&TelecomAccountRecord {
                account_id: "tel-1".into(),
                kind: crate::daemon::telephony::TelecomKind::Mock,
                label: "Line".into(),
                enabled: true,
                carrier_account_id: "carrier-1".into(),
                from_number: NUMBER.into(),
                credential_ref: None,
                public_base_url: None,
                non_secret_config: serde_json::json!({}),
                inbound_policy: InboundCallPolicy::Reject,
                outbound_approval: OutboundCallApproval::Approval,
                limits: CallLimits::default(),
                health: ChannelHealth::connected(NOW, None),
                created_at_ms: 1,
                updated_at_ms: 1,
            })
            .expect("telecom account");
        store
            .upsert_channel_account(&channel_account("tel-1", ChannelKind::Sms))
            .expect("sms account");
        store
            .record_channel_event(&inbound_event("tel-1", "sms-1", PEER, PEER))
            .expect("sms event");
        store
    }

    fn bundle_json(store: &DaemonStore) -> String {
        let redactor = Redactor::from_seed_for_tests("support-bundle-cli-tests");
        let sections = serde_json::json!({
            "channels": channel_section(store, &redactor),
            "telephony": telephony_section(store, &redactor),
        });
        serde_json::to_string(&sections).expect("json")
    }

    /// The one assertion this whole module exists to make: nothing anybody
    /// wrote, and no phone number, survives into the document.
    ///
    /// Written as a search over the *whole serialized bundle* rather than over
    /// the fields the readers were meant to fill, because the failure this
    /// guards against is a field somebody adds later without thinking about it.
    #[test]
    fn no_message_text_and_no_phone_number_reaches_the_bundle() {
        let store = seeded();
        let json = bundle_json(&store);
        for forbidden in [
            BODY,
            "secret plan",
            NUMBER,
            PEER,
            "5550001111",
            "user-ada",
            "chat-42",
        ] {
            assert!(
                !json.contains(forbidden),
                "'{forbidden}' survived into the bundle: {json}"
            );
        }
    }

    /// And it is still useful: the shape of what happened is all there.
    #[test]
    fn what_happened_survives_even_though_who_and_what_do_not() {
        let store = seeded();
        let json = bundle_json(&store);
        for expected in [
            "inbound.accepted",
            "sms.inbound",
            "account.telegram",
            "conversation:",
            "phone:",
        ] {
            assert!(json.contains(expected), "missing '{expected}' in {json}");
        }
    }

    /// Two events about the same party read as the same party, which is what
    /// makes a trace followable at all.
    #[test]
    fn one_party_reads_as_one_party_within_a_bundle() {
        let store = seeded();
        let redactor = Redactor::from_seed_for_tests("support-bundle-cli-tests");
        let telephony = telephony_section(&store, &redactor);
        let health = telephony
            .events
            .iter()
            .find(|event| event.event == "number.health")
            .expect("health event");
        let text = telephony
            .events
            .iter()
            .find(|event| event.event == "sms.inbound")
            .expect("sms event");
        assert_eq!(
            health.subject, text.context,
            "the operator's own number is one token throughout"
        );
    }

    /// A finished command is in the trace, because a finished command is what a
    /// postmortem is about.
    ///
    /// The Security Doctor's reader deliberately shows only what is still in
    /// flight — the question there is "is a microphone open right now". Reusing
    /// it here produced a device section that was empty exactly when somebody
    /// needed it, which is the failure this test exists to prevent recurring.
    #[test]
    fn a_completed_device_command_is_in_the_trace_and_its_payload_is_not() {
        use crate::daemon::remote::protocol::{
            DeviceCapability, DeviceCommandState, RemoteAction, RemoteScopes,
        };
        use crate::daemon::remote::store::{DeviceCommandRequest, RemoteSecretStore, RemoteStore};

        #[derive(Default)]
        struct Secrets(std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);
        impl RemoteSecretStore for Secrets {
            fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
                self.0
                    .lock()
                    .unwrap()
                    .get(slot)
                    .cloned()
                    .ok_or_else(|| "missing".to_string())
            }
            fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
                self.0
                    .lock()
                    .unwrap()
                    .insert(slot.to_string(), secret.to_vec());
                Ok(())
            }
            fn delete(&self, slot: &str) -> Result<(), String> {
                self.0.lock().unwrap().remove(slot);
                Ok(())
            }
        }

        let root = std::env::temp_dir().join(format!(
            "little-monkey-bundle-devices-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let paths = DaemonPaths::under(&root);
        paths.ensure().expect("paths");
        let secrets = Secrets::default();
        let mut store = RemoteStore::open(&paths.root).expect("remote store");
        let invitation = store
            .create_invitation(
                &RemoteScopes {
                    actions: std::collections::BTreeSet::from([RemoteAction::ViewRuns]),
                    run_ids: std::collections::BTreeSet::from(["run-one".to_string()]),
                    workspace_ids: std::collections::BTreeSet::new(),
                    max_artifact_bytes: 1024,
                },
                1,
                1_000_000,
            )
            .expect("invitation");
        let device = store
            .accept_invitation(
                &invitation.pairing_id,
                &invitation.token,
                "Ada's phone",
                "runner-one",
                1,
                &secrets,
            )
            .expect("pair")
            .device_id;
        let command = store
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: device.clone(),
                    capability: DeviceCapability::NotificationPost,
                    // The kind of payload that must never reach a bundle: the
                    // words that would be shown on somebody's lock screen.
                    arguments: serde_json::json!({ "body": SECRET_PAYLOAD }),
                    source_run_id: None,
                    source_session_id: None,
                    source_tool_call_id: None,
                    invocation_id: None,
                    expires_at_ms: 1_000_000,
                },
                1,
            )
            .expect("enqueue");
        store.lease_device_command(&device, 30_000, 2).ok();
        store
            .complete_device_command(
                &device,
                &command.command_id,
                DeviceCommandState::Succeeded,
                Some(&serde_json::json!({ "body": SECRET_PAYLOAD })),
                None,
                None,
                None,
                3,
            )
            .expect("complete");
        drop(store);

        let section = device_section(
            &paths,
            &Redactor::from_seed_for_tests("support-bundle-cli-tests"),
        );
        let json = serde_json::to_string(&section).expect("json");
        assert!(
            json.contains("device.notification_post"),
            "a finished command has to be in the trace: {json}"
        );
        assert!(json.contains("succeeded"), "{json}");
        // And what it carried is not.
        for forbidden in [SECRET_PAYLOAD, &device] {
            assert!(!json.contains(forbidden), "'{forbidden}' leaked: {json}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A store that cannot be read says so, rather than contributing an empty
    /// section somebody reads as "this subsystem did nothing".
    #[test]
    fn a_subsystem_that_cannot_be_read_is_distinguishable_from_a_quiet_one() {
        // A daemon root that is a *file*. A merely-absent directory is not
        // unopenable — SQLite will happily create one, and a Unix-shaped
        // absent path is a relative path on Windows and gets created next to
        // the test binary. A regular file cannot have a database placed inside
        // it on any platform, which is the failure this test needs.
        let root = std::env::temp_dir().join(format!(
            "little-monkey-unreadable-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&root, b"not a directory").expect("write blocker file");
        let paths = DaemonPaths::under(&root);
        let section = device_section(
            &paths,
            &Redactor::from_seed_for_tests("support-bundle-cli-tests"),
        );
        assert!(section.unavailable.is_some(), "{section:?}");

        let quiet = peer_section(
            &seeded(),
            &Redactor::from_seed_for_tests("support-bundle-cli-tests"),
        );
        assert!(quiet.unavailable.is_none());
        assert!(quiet.events.is_empty());
        let _ = std::fs::remove_file(&root);
    }
}

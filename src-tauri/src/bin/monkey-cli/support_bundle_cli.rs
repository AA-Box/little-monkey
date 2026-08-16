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
            for name in ["channels", "telephony", "peers"] {
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
                    for name in ["channels", "telephony", "peers"] {
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
    let commands = match store.active_device_commands() {
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

    /// A store that cannot be read says so, rather than contributing an empty
    /// section somebody reads as "this subsystem did nothing".
    #[test]
    fn a_subsystem_that_cannot_be_read_is_distinguishable_from_a_quiet_one() {
        let paths = DaemonPaths::under(std::path::Path::new("/definitely/not/a/directory"));
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
    }
}

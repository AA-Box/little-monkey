//! What happens to a verified carrier callback.
//!
//! The shape mirrors the messaging side deliberately. A text is not special:
//! it becomes a `ChannelEnvelope` and goes through `channel_ingress`, which is
//! where access policy, pairing, routing and untrusted-text wrapping already
//! live. Nothing about SMS gets its own copy of those rules.
//!
//! A call is what telephony adds. Two decisions govern it and they are kept
//! apart on purpose: whether Little Monkey answers the phone, and whether it
//! may dial out. This file only ever implements the first. Placing a call is
//! the `place_call` tool's job, behind the normal approval policy.

use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::types::{ChannelEnvelope, ChannelHealth, ChannelKind};

use super::channel_store::ChannelAccountRecord;
use super::channel_worker::{ingest_batch, RunQueue};
use super::store::DaemonStore;
use super::telecom_store::{
    CallDirection, InboundCallPolicy, TelecomAccountRecord, TelecomCallRecord,
};
use super::telephony::{CallState, TelecomEvent};

/// What one verified callback did. Returned rather than logged so the webhook
/// route can answer the carrier accurately and a test can assert on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CarrierOutcome {
    /// A text was handed to the messaging gate. The counts are that gate's.
    Message { accepted: u32, ignored: u32 },
    /// A call was recorded. `answered` is false when the account's policy is
    /// to reject, which is also when nothing further happens.
    Call { call_id: String, answered: bool },
    /// Progress was applied to a call we already knew about.
    Progress { call_id: String },
    /// Verified and understood, and nothing followed from it.
    Nothing,
}

/// Handle one verified event from a carrier.
pub(crate) fn handle_carrier_event(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    account: &TelecomAccountRecord,
    event: TelecomEvent,
    now_ms: i64,
) -> Result<CarrierOutcome, String> {
    match event {
        TelecomEvent::InboundSms(envelope) => {
            ensure_sms_channel_account(store, account, now_ms)?;
            let report = ingest_batch(store, queue, std::slice::from_ref(&*envelope), now_ms);
            Ok(CarrierOutcome::Message {
                accepted: report.accepted,
                ignored: report.ignored + report.challenged + report.duplicates,
            })
        }
        TelecomEvent::InboundCall {
            provider_call_id,
            from_number,
            received_at_ms,
            ..
        } => {
            // The row is written whatever the policy says: an operator who has
            // calls switched off still wants to see that the phone rang.
            let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
            let answered = matches!(
                account.inbound_policy,
                InboundCallPolicy::Answer | InboundCallPolicy::Voicemail
            );
            let record = TelecomCallRecord {
                call_id: call_id.clone(),
                account_id: account.account_id.clone(),
                provider_call_id: Some(provider_call_id.clone()),
                direction: CallDirection::Inbound,
                peer_number: from_number,
                state: if answered {
                    CallState::Ringing
                } else {
                    CallState::Completed
                },
                session_key: answered.then(|| format!("call:{}:{call_id}", account.account_id)),
                job_id: None,
                // The carrier's own call id is the natural idempotency key for
                // an inbound call: a redelivered callback finds this row rather
                // than creating a second one.
                idempotency_key: format!("inbound:{provider_call_id}"),
                last_error: (!answered)
                    .then(|| "Inbound calls are turned off for this number".to_string()),
                started_at_ms: None,
                ended_at_ms: (!answered).then_some(received_at_ms.max(now_ms)),
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
            };
            let recorded = store.start_call(&record)?;
            Ok(CarrierOutcome::Call {
                call_id: match recorded {
                    super::telecom_store::CallRecording::Recorded { call_id }
                    | super::telecom_store::CallRecording::Duplicate { call_id } => call_id,
                },
                answered,
            })
        }
        TelecomEvent::CallProgress {
            provider_call_id,
            state,
            detail,
        } => {
            let Some(call) = store.call_by_provider_id(&account.account_id, &provider_call_id)?
            else {
                // A call this machine never placed or answered. Recorded as an
                // event by the caller and otherwise ignored — inventing a row
                // for it would let a carrier create call history at will.
                return Ok(CarrierOutcome::Nothing);
            };
            store.advance_call(&call.call_id, state, detail.as_deref(), now_ms)?;
            Ok(CarrierOutcome::Progress {
                call_id: call.call_id,
            })
        }
        // Delivery receipts and carrier heartbeats: the event row the caller
        // already wrote is the whole record.
        TelecomEvent::SmsStatus { .. } | TelecomEvent::Ignored => Ok(CarrierOutcome::Nothing),
    }
}

/// Make sure the messaging side has an account row for this number's texts.
///
/// SMS reuses the channel subsystem, and that subsystem keys everything —
/// access policy, sender authorization, routes, the event log — on a channel
/// account. Rather than asking an operator to configure the same number twice,
/// the telephony account lends its id to a channel account created on first
/// use.
///
/// The default policy is the conservative one messaging already ships: a text
/// from a stranger starts a pairing handshake rather than running.
fn ensure_sms_channel_account(
    store: &mut DaemonStore,
    account: &TelecomAccountRecord,
    now_ms: i64,
) -> Result<(), String> {
    if store.channel_account(&account.account_id)?.is_some() {
        return Ok(());
    }
    store.upsert_channel_account(&ChannelAccountRecord {
        account_id: account.account_id.clone(),
        kind: ChannelKind::Sms,
        label: account.label.clone(),
        enabled: account.enabled,
        non_secret_config: serde_json::json!({ "from_number": account.from_number }),
        // The carrier credential lives on the telephony account. The messaging
        // side never needs it: SMS is sent through the carrier provider, not
        // through a channel adapter.
        credential_ref: None,
        access_policy: ChannelAccessPolicy {
            direct: AccessPolicy::Pairing,
            group: AccessPolicy::Disabled,
            group_activation: GroupActivation::Disabled,
        },
        health: ChannelHealth {
            state: account.health.state,
            detail: account.health.detail.clone(),
            last_error: account.health.last_error.clone(),
            probed_at_ms: account.health.probed_at_ms,
        },
        created_at_ms: now_ms,
        updated_at_ms: now_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::ingress::ConversationIngress;
    use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
    use little_monkey_lib::channels::types::{ChannelConversation, ChannelSender, HealthState};
    use std::sync::Mutex;

    use super::super::telecom_store::{OutboundCallApproval, TelecomAccountRecord};
    use super::super::telephony::TelecomKind;

    const NOW: i64 = 1_700_000_000_000;

    #[derive(Default)]
    struct FakeQueue {
        submitted: Mutex<Vec<String>>,
    }

    impl RunQueue for FakeQueue {
        fn submit(
            &self,
            ingress: &ConversationIngress,
            _params: Vec<String>,
        ) -> Result<String, String> {
            let id = ingress.deterministic_job_id();
            self.submitted.lock().unwrap().push(id.clone());
            Ok(id)
        }
    }

    fn account(policy: InboundCallPolicy) -> TelecomAccountRecord {
        TelecomAccountRecord {
            account_id: "tel-1".into(),
            kind: TelecomKind::Mock,
            label: "Support line".into(),
            enabled: true,
            carrier_account_id: "carrier-1".into(),
            from_number: "+15550000000".into(),
            credential_ref: Some("telecom:tel-1".into()),
            public_base_url: None,
            non_secret_config: serde_json::json!({}),
            inbound_policy: policy,
            outbound_approval: OutboundCallApproval::Approval,
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

    fn seeded(policy: InboundCallPolicy) -> (DaemonStore, TelecomAccountRecord) {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let account = account(policy);
        store.upsert_telecom_account(&account).expect("account");
        (store, account)
    }

    fn text(body: &str, id: &str) -> ChannelEnvelope {
        ChannelEnvelope {
            account_id: "tel-1".into(),
            kind: ChannelKind::Sms,
            provider_event_id: id.into(),
            conversation: ChannelConversation::direct("+15551234567"),
            sender: ChannelSender::new("+15551234567"),
            text: body.into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: NOW,
            metadata: Default::default(),
        }
    }

    #[test]
    fn a_text_runs_through_the_messaging_gate_not_beside_it() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        // An approved sender and a route: the same two things any other
        // messaging account needs, because SMS is one.
        store
            .insert_channel_route(&ChannelRoute {
                route_id: "route-1".into(),
                scope: RouteScope::global_default(),
                target: RouteTarget::new("chat"),
                enabled: true,
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("route");
        let queue = FakeQueue::default();

        let outcome = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundSms(Box::new(text("hello", "sms-1"))),
            NOW,
        )
        .expect("handled");

        // Pairing is the default for a stranger, so the first text is
        // challenged rather than run — exactly what a Telegram DM would do.
        assert_eq!(
            outcome,
            CarrierOutcome::Message {
                accepted: 0,
                ignored: 1
            }
        );
        assert!(queue.submitted.lock().unwrap().is_empty());

        let sms_account = store.channel_account("tel-1").expect("query").expect("row");
        assert_eq!(sms_account.kind, ChannelKind::Sms);
        assert_eq!(
            sms_account.access_policy.direct,
            AccessPolicy::Pairing,
            "a text from a stranger must not run on arrival"
        );
    }

    #[test]
    fn an_approved_sender_texting_becomes_a_run() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        store
            .insert_channel_route(&ChannelRoute {
                route_id: "route-1".into(),
                scope: RouteScope::global_default(),
                target: RouteTarget::new("chat"),
                enabled: true,
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("route");
        let queue = FakeQueue::default();
        // First text creates the paired channel account.
        let _ = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundSms(Box::new(text("hello", "sms-1"))),
            NOW,
        );
        store
            .upsert_channel_sender(
                "tel-1",
                "+15551234567",
                &super::super::channel_store::StoredSenderAuthorization {
                    sender_id: "+15551234567".into(),
                    state: little_monkey_lib::channels::policy::SenderState::Approved,
                    pairing_code_digest: None,
                    requested_at_ms: NOW,
                    expires_at_ms: None,
                    approved_at_ms: Some(NOW),
                    blocked_at_ms: None,
                    display_label: None,
                    metadata: Default::default(),
                },
            )
            .expect("approve");

        let outcome = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundSms(Box::new(text("status?", "sms-2"))),
            NOW,
        )
        .expect("handled");
        assert_eq!(
            outcome,
            CarrierOutcome::Message {
                accepted: 1,
                ignored: 0
            }
        );
        assert_eq!(queue.submitted.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_call_is_recorded_even_when_the_policy_refuses_it() {
        let (mut store, account) = seeded(InboundCallPolicy::Reject);
        let queue = FakeQueue::default();

        let outcome = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundCall {
                provider_call_id: "carrier-call-1".into(),
                from_number: "+15551234567".into(),
                to_number: "+15550000000".into(),
                received_at_ms: NOW,
            },
            NOW,
        )
        .expect("handled");

        let CarrierOutcome::Call { call_id, answered } = outcome else {
            panic!("expected a call");
        };
        assert!(!answered);
        let call = store.telecom_call(&call_id).expect("query").expect("row");
        assert_eq!(call.state, CallState::Completed);
        assert!(call.session_key.is_none(), "a refused call gets no session");
    }

    #[test]
    fn a_redelivered_ring_does_not_become_a_second_call() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();
        let ring = || TelecomEvent::InboundCall {
            provider_call_id: "carrier-call-1".into(),
            from_number: "+15551234567".into(),
            to_number: "+15550000000".into(),
            received_at_ms: NOW,
        };

        let first = handle_carrier_event(&mut store, &queue, &account, ring(), NOW).expect("first");
        let second =
            handle_carrier_event(&mut store, &queue, &account, ring(), NOW).expect("second");
        assert_eq!(first, second);
        assert_eq!(store.recent_calls("tel-1", 10).expect("calls").len(), 1);
    }

    #[test]
    fn progress_for_an_unknown_call_creates_nothing() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();

        let outcome = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::CallProgress {
                provider_call_id: "never-seen".into(),
                state: CallState::Completed,
                detail: None,
            },
            NOW,
        )
        .expect("handled");

        assert_eq!(outcome, CarrierOutcome::Nothing);
        assert!(store.recent_calls("tel-1", 10).expect("calls").is_empty());
    }

    #[test]
    fn progress_advances_the_call_it_names() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();
        let CarrierOutcome::Call { call_id, .. } = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundCall {
                provider_call_id: "carrier-call-1".into(),
                from_number: "+15551234567".into(),
                to_number: "+15550000000".into(),
                received_at_ms: NOW,
            },
            NOW,
        )
        .expect("ring") else {
            panic!("expected a call");
        };

        handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::CallProgress {
                provider_call_id: "carrier-call-1".into(),
                state: CallState::Completed,
                detail: None,
            },
            NOW + 5_000,
        )
        .expect("progress");

        let call = store.telecom_call(&call_id).expect("query").expect("row");
        assert_eq!(call.state, CallState::Completed);
        assert_eq!(call.ended_at_ms, Some(NOW + 5_000));
    }
}

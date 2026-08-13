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
    CallDirection, InboundCallPolicy, LimitBreach, TelecomAccountRecord, TelecomCallRecord,
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
            // A redelivered ring is answered from the row it already made.
            // Deciding again would compare the account's concurrency limit
            // against a live call that *is* this one, and refuse the call for
            // colliding with itself.
            if let Some(existing) =
                store.call_by_provider_id(&account.account_id, &provider_call_id)?
            {
                return Ok(CarrierOutcome::Call {
                    answered: existing.session_key.is_some(),
                    call_id: existing.call_id,
                });
            }
            // The row is written whatever the policy says: an operator who has
            // calls switched off still wants to see that the phone rang.
            let call_id = format!("call-{}", uuid::Uuid::new_v4().simple());
            // Being at the concurrency limit refuses the same way the policy
            // does: the ring is recorded, nothing is answered. A limit that only
            // applied to outbound calls would not be a limit on what the carrier
            // bills.
            let at_capacity =
                store.live_call_count(&account.account_id)? >= account.limits.max_concurrent_calls;
            let answered = !at_capacity
                && matches!(
                    account.inbound_policy,
                    InboundCallPolicy::Answer | InboundCallPolicy::Voicemail
                );
            let peer = from_number.clone();
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
                session_key: answered
                    .then(|| super::telecom_store::call_session_key(account, &peer, &call_id)),
                job_id: None,
                // The carrier's own call id is the natural idempotency key for
                // an inbound call: a redelivered callback finds this row rather
                // than creating a second one.
                idempotency_key: format!("inbound:{provider_call_id}"),
                // An inbound call says whatever greeting the operator wrote for
                // this number; the media session falls back to it too, so a
                // redelivered ring cannot lose it.
                opening_line: None,
                last_error: (!answered).then(|| {
                    if at_capacity {
                        "This number was already at its concurrent-call limit".to_string()
                    } else {
                        "Inbound calls are turned off for this number".to_string()
                    }
                }),
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

/// One sweep of the live calls: hang up anything that has outlived a limit.
///
/// Split from the loop below so a test can drive it with a mock carrier and no
/// timers. The carrier is asked to hang up *before* the row is closed, because a
/// row marked completed while the line is still open is a bill nobody is
/// watching. A carrier that will not confirm the hangup leaves the call in
/// `needs_reconciliation` rather than pretending it ended.
pub(crate) async fn sweep_call_limits(
    store: &mut DaemonStore,
    carriers: &std::collections::BTreeMap<
        String,
        std::sync::Arc<dyn super::telephony::TelecomProvider>,
    >,
    now_ms: i64,
) -> Result<u32, String> {
    let overdue = store.overdue_calls(now_ms)?;
    let mut ended = 0;
    for call in overdue {
        let closed = hang_up_and_close(
            store,
            carriers.get(&call.account_id),
            &call.call_id,
            call.provider_call_id.as_deref(),
            match call.breach {
                LimitBreach::RingTimeout => CallState::Failed,
                LimitBreach::MaxDuration => CallState::Completed,
            },
            call.breach.detail(),
            now_ms,
        )
        .await?;
        if closed {
            ended += 1;
        }
    }
    Ok(ended)
}

/// Ask the carrier to hang up one call, then close its row.
///
/// The order is the point: a row marked completed while the line is still open
/// is a bill nobody is watching, so the carrier is asked first and a refusal
/// leaves the call in `needs_reconciliation` rather than pretending it ended.
/// `Ok(false)` is that case — the call was settled, but not the way `ended`
/// asked for.
///
/// Every reason to end a live call routes through here: a limit the sweep
/// found, or a media stream that dropped and never came back.
pub(crate) async fn hang_up_and_close(
    store: &mut DaemonStore,
    carrier: Option<&std::sync::Arc<dyn super::telephony::TelecomProvider>>,
    call_id: &str,
    provider_call_id: Option<&str>,
    ended: CallState,
    detail: &str,
    now_ms: i64,
) -> Result<bool, String> {
    let hangup = match (carrier, provider_call_id) {
        (Some(carrier), Some(provider_call_id)) => carrier.hangup(provider_call_id).await,
        // Nothing the carrier ever acknowledged: a ring we recorded but
        // never got an id for is ours alone to close.
        (_, None) => Ok(()),
        (None, Some(_)) => Err("no carrier is loaded for this account".to_string()),
    };
    match hangup {
        Ok(()) => {
            store.advance_call(call_id, ended, Some(detail), now_ms)?;
            Ok(true)
        }
        Err(error) => {
            store.advance_call(
                call_id,
                CallState::NeedsReconciliation,
                Some(&format!(
                    "{detail} but the carrier did not confirm the hangup: {error}"
                )),
                now_ms,
            )?;
            Ok(false)
        }
    }
}

/// How often the sweep runs. A call cut at its limit is allowed to overshoot by
/// up to this much; the alternative is a timer per call, which buys seconds of
/// precision at the cost of a task per ring.
const SWEEP_INTERVAL_MS: u64 = 15_000;

/// Enforce call limits for as long as the daemon lives.
// ponytail: one sweep for every account. If an operator ever runs enough numbers
// for the join to matter, index it or shard the sweep per account.
pub(crate) fn spawn_telecom_runtime(paths: super::store::DaemonPaths) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(SWEEP_INTERVAL_MS)).await;
            let Ok(mut store) = DaemonStore::open(&paths) else {
                continue;
            };
            let Ok(now_ms) = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis())
                    .unwrap_or_default(),
            ) else {
                continue;
            };
            let carriers = load_carriers(&store);
            if let Err(error) = sweep_call_limits(&mut store, &carriers, now_ms).await {
                eprintln!("monkey daemon: call limit sweep failed: {error}");
            }
        }
    });
}

/// Build a carrier for every enabled telephony account, resolving each
/// credential from the keychain here so no provider reads it itself.
fn load_carriers(
    store: &DaemonStore,
) -> std::collections::BTreeMap<String, std::sync::Arc<dyn super::telephony::TelecomProvider>> {
    use super::channel_adapter::{ChannelSecrets, KeyringChannelSecrets};

    let mut carriers = std::collections::BTreeMap::new();
    let Ok(accounts) = store.telecom_accounts() else {
        return carriers;
    };
    for account in accounts.into_iter().filter(|account| account.enabled) {
        let secret = match &account.credential_ref {
            Some(reference) => KeyringChannelSecrets.get(reference).unwrap_or_default(),
            None => String::new(),
        };
        if let Ok(provider) = super::telephony::provider_for_account(&account, secret) {
            carriers.insert(account.account_id.clone(), provider);
        }
    }
    carriers
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::ingress::ConversationIngress;
    use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
    use little_monkey_lib::channels::types::{ChannelConversation, ChannelSender, HealthState};
    use std::sync::Mutex;

    use super::super::telecom_store::{CallLimits, OutboundCallApproval, TelecomAccountRecord};
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

    fn carriers() -> std::collections::BTreeMap<
        String,
        std::sync::Arc<dyn super::super::telephony::TelecomProvider>,
    > {
        let carrier: std::sync::Arc<dyn super::super::telephony::TelecomProvider> =
            std::sync::Arc::new(super::super::telephony::mock::MockProvider::new(
                super::super::telephony::TelecomConfig {
                    account_id: "tel-1".into(),
                    kind: TelecomKind::Mock,
                    carrier_account_id: "carrier-1".into(),
                    from_number: "+15550000000".into(),
                    secret: "shared".into(),
                    public_base_url: None,
                    webhook_public_key: None,
                },
            ));
        std::collections::BTreeMap::from([("tel-1".to_string(), carrier)])
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
    fn a_second_caller_is_refused_while_the_line_is_busy() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();
        let ring = |id: &str, from: &str| TelecomEvent::InboundCall {
            provider_call_id: id.into(),
            from_number: from.into(),
            to_number: "+15550000000".into(),
            received_at_ms: NOW,
        };

        let first = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            ring("c-1", "+15551110000"),
            NOW,
        )
        .expect("first");
        let second = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            ring("c-2", "+15552220000"),
            NOW,
        )
        .expect("second");

        assert_eq!(
            first,
            CarrierOutcome::Call {
                call_id: match &first {
                    CarrierOutcome::Call { call_id, .. } => call_id.clone(),
                    other => panic!("expected a call, got {other:?}"),
                },
                answered: true
            }
        );
        let CarrierOutcome::Call { call_id, answered } = second else {
            panic!("expected a call");
        };
        assert!(!answered, "the default limit is one call at a time");
        let call = store.telecom_call(&call_id).expect("query").expect("row");
        assert_eq!(call.state, CallState::Completed);
        assert!(call
            .last_error
            .expect("a reason")
            .contains("concurrent-call limit"));
    }

    #[tokio::test]
    async fn a_ring_nobody_answers_is_given_up_on() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();
        let CarrierOutcome::Call { call_id, .. } = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundCall {
                provider_call_id: "c-1".into(),
                from_number: "+15551110000".into(),
                to_number: "+15550000000".into(),
                received_at_ms: NOW,
            },
            NOW,
        )
        .expect("ring") else {
            panic!("expected a call");
        };

        // One second inside the 60s default changes nothing; one second past it
        // ends the call.
        let carriers = carriers();
        assert_eq!(
            sweep_call_limits(&mut store, &carriers, NOW + 59_000)
                .await
                .expect("sweep"),
            0
        );
        assert_eq!(
            sweep_call_limits(&mut store, &carriers, NOW + 61_000)
                .await
                .expect("sweep"),
            1
        );

        let call = store.telecom_call(&call_id).expect("query").expect("row");
        assert_eq!(call.state, CallState::Failed);
        assert!(call.last_error.expect("a reason").contains("ring timeout"));
    }

    #[tokio::test]
    async fn a_call_that_runs_long_is_hung_up_at_the_carrier() {
        let (mut store, account) = seeded(InboundCallPolicy::Answer);
        let queue = FakeQueue::default();
        let CarrierOutcome::Call { call_id, .. } = handle_carrier_event(
            &mut store,
            &queue,
            &account,
            TelecomEvent::InboundCall {
                provider_call_id: "c-1".into(),
                from_number: "+15551110000".into(),
                to_number: "+15550000000".into(),
                received_at_ms: NOW,
            },
            NOW,
        )
        .expect("ring") else {
            panic!("expected a call");
        };
        store
            .advance_call(&call_id, CallState::InProgress, None, NOW)
            .expect("answered");

        let carriers = carriers();

        // The default cap is 30 minutes, measured from when the call connected.
        let ended = sweep_call_limits(&mut store, &carriers, NOW + 1_801_000)
            .await
            .expect("sweep");

        assert_eq!(ended, 1);
        let call = store.telecom_call(&call_id).expect("query").expect("row");
        assert_eq!(call.state, CallState::Completed);
        assert!(call
            .last_error
            .expect("a reason")
            .contains("maximum call duration"));
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

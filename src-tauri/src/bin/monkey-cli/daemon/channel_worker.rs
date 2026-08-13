//! The two loops that make channels move: inbound polling and the outbox.
//!
//! Both are deliberately small and both are crash-safe by construction rather
//! than by care:
//!
//! - **Inbound.** An adapter's batch is handed one envelope at a time to
//!   `channel_ingress::plan_channel_ingress`, which records and deduplicates
//!   before it decides anything. The transport cursor is only advanced *after*
//!   the batch is durably recorded, so a crash mid-batch replays messages that
//!   the event log then collapses — the opposite order would lose them.
//! - **Outbound.** A claimed row is in `sending` before the request goes out.
//!   If the process dies there, `requeue_stuck_sending` moves it to
//!   `needs_reconciliation` rather than retrying it, because a send that may
//!   have reached the provider is not safe to repeat.
//!
//! Neither loop executes an agent. Inbound work becomes a normal durable run
//! through [`RunQueue`], which production implements with the daemon's one
//! `enqueue`.

use std::collections::BTreeMap;
use std::sync::Arc;

use little_monkey_lib::channels::ingress::{
    ConversationIngress, FrozenExecutionContext, FrozenExecutionContextV1,
};
use little_monkey_lib::channels::types::{ChannelEnvelope, SendOutcome};

use super::channel_adapter::ChannelAdapter;
use super::channel_ingress::{self, IngressPlan, OutboxPayload, PlannedDecision, SubmitOutcome};
use super::channel_store::{EventDirection, EventDisposition, NewChannelEvent};
use super::store::DaemonStore;

/// Cursor key the inbound loop stores an adapter's resume token under.
const POLL_CURSOR_KEY: &str = "inbound";

/// How many outbox rows one drain claims. Bounded so a backlog cannot hold the
/// database transaction open behind a slow provider.
const OUTBOX_BATCH: u32 = 16;

/// Seam through which accepted inbound work reaches the daemon's queue.
///
/// The same shape, and for the same reason, as the mobile chat seam: the
/// interesting behavior — dedupe, gating, cursor ordering, outbox state — is
/// testable without a configured daemon, while production does the real
/// enqueue.
pub(crate) trait RunQueue: Send + Sync {
    /// Resolves what this turn will execute with, so it can be frozen onto the
    /// durable row before anything runs.
    ///
    /// Separate from [`Self::submit`] because it happens at a different moment:
    /// resolution is part of *accepting* a turn, submission is what happens to
    /// an already-accepted one. Recovery calls only the second, which is what
    /// stops a configuration edit from retargeting a message already promised a
    /// run.
    fn freeze_execution(
        &self,
        ingress: &ConversationIngress,
    ) -> Result<FrozenExecutionContext, String>;

    /// Queues one accepted turn. Returns the daemon job id.
    fn submit(&self, ingress: &ConversationIngress, params: Vec<String>) -> Result<String, String>;

    /// Why the resources an *already frozen* context names can no longer be
    /// reached, if they cannot.
    ///
    /// Asked before continuing a turn that was accepted earlier, and deliberately
    /// not a re-resolution: the question is whether the model and credential this
    /// turn was frozen with are still there, never what the operator would pick
    /// today. A `Some` here is final — the continuation is refused and the reason
    /// shown — because the only alternatives are running under a credential
    /// nobody chose or failing later with a stranger error.
    ///
    /// Defaults to "nothing to check" so a test double that queues turns without
    /// an operator's keychain behind it does not have to lie about one.
    fn frozen_context_unusable(&self, _context: &FrozenExecutionContextV1) -> Option<String> {
        None
    }
}

/// A frozen execution context for the test doubles that stand in for the
/// daemon's queue.
///
/// Shared so every fake freezes the same shape, and so a test that asserts on
/// what recovery replays is asserting on something with a real recipe in it.
/// Never reachable from production: `DaemonChannelQueue` resolves the
/// operator's own recipe instead.
#[cfg(test)]
pub(crate) fn test_frozen_execution(ingress: &ConversationIngress) -> FrozenExecutionContext {
    use little_monkey_lib::channels::ingress::FrozenExecutionContextV1;

    let recipe = little_monkey_lib::recipes::Recipe {
        version: 1,
        name: ingress.target.recipe.clone(),
        description: None,
        target: little_monkey_lib::recipes::RecipeTarget {
            ollama: Some("qwen2.5:7b".to_string()),
            ..Default::default()
        },
        workspace: Some("/tmp".to_string()),
        permission_mode: "readonly".to_string(),
        system: None,
        prompt: "{{message}}".to_string(),
        params: Default::default(),
        max_iterations: None,
        timeout_seconds: None,
        output: Default::default(),
        channel_send: None,
        desktop_turn: None,
        placed_run: None,
    };
    FrozenExecutionContext::V1(
        FrozenExecutionContextV1 {
            recipe_ref: ingress.target.recipe.clone(),
            recipe_json: serde_json::to_string(&recipe).expect("serialize test recipe"),
            recipe_source_path: Some("/tmp/test-recipe.json".to_string()),
            workspace_path: Some("/tmp".to_string()),
            model_target: "ollama:qwen2.5:7b".to_string(),
            permission_mode: "readonly".to_string(),
            credential_ref: None,
            route_id: ingress.route_id.clone(),
            route_digest: ingress.route_digest.clone(),
            ..Default::default()
        }
        .seal(),
    )
}

/// What one inbound pass did, for the caller's logs and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct InboundReport {
    pub accepted: u32,
    pub challenged: u32,
    pub ignored: u32,
    pub duplicates: u32,
    /// Messages that could not be planned at all (no route, storage failure),
    /// or whose turn ran out of submission attempts. Counted rather than
    /// propagated: one bad message must not stop the batch.
    pub failed: u32,
    /// Turns that are durably accepted but did not reach the queue this pass.
    /// Not lost — the next recovery pass re-submits them.
    pub deferred: u32,
}

/// Feed one adapter batch through the ingress gate.
///
/// Returns the report and the cursor to persist. The cursor is returned rather
/// than written here so the caller can persist it after the whole batch is
/// durable — a cursor written before the events it covers would skip them
/// permanently after a crash.
pub(crate) fn ingest_batch(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    envelopes: &[ChannelEnvelope],
    now_ms: i64,
) -> InboundReport {
    let mut report = InboundReport::default();
    for envelope in envelopes {
        match channel_ingress::plan_channel_ingress(store, envelope, now_ms) {
            Ok(IngressPlan { event_id, decision }) => match decision {
                PlannedDecision::Run { ingress, params } => {
                    // Durable accept first, queue second. A crash in between
                    // leaves a row `recover_pending_ingress` finishes, rather
                    // than a message the provider considers delivered and the
                    // event log would refuse as a duplicate.
                    match channel_ingress::submit_conversation_turn(
                        store, queue, &ingress, &params, now_ms,
                    ) {
                        Ok(SubmitOutcome::Queued { job_id, .. })
                        | Ok(SubmitOutcome::AlreadyQueued { job_id, .. }) => {
                            report.accepted += 1;
                            // Best effort: the run is already queued, and
                            // failing to annotate the event must not undo it.
                            let _ = store.set_channel_event_disposition(
                                &event_id,
                                EventDisposition::Accepted,
                                None,
                                Some(&job_id),
                            );
                        }
                        Ok(SubmitOutcome::Deferred { error, .. }) => {
                            // Accepted durably, not queued yet. Counted as a
                            // failure of this pass, but the turn is not lost.
                            report.deferred += 1;
                            let _ = store.set_channel_event_disposition(
                                &event_id,
                                EventDisposition::Accepted,
                                Some(&error),
                                None,
                            );
                        }
                        Ok(SubmitOutcome::Parked { .. }) => {
                            report.failed += 1;
                            let _ = store.set_channel_event_disposition(
                                &event_id,
                                EventDisposition::Failed,
                                Some("The turn could not be queued and was parked"),
                                None,
                            );
                        }
                        Err(error) => {
                            report.failed += 1;
                            let _ = store.set_channel_event_disposition(
                                &event_id,
                                EventDisposition::Failed,
                                Some(&error),
                                None,
                            );
                        }
                    }
                }
                PlannedDecision::Challenge => report.challenged += 1,
                PlannedDecision::Ignore(_) => report.ignored += 1,
                PlannedDecision::Duplicate => report.duplicates += 1,
            },
            Err(_) => report.failed += 1,
        }
    }
    report
}

/// Poll one account once and ingest whatever it returned.
pub(crate) async fn poll_account_once(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    account_id: &str,
    adapter: &dyn ChannelAdapter,
    now_ms: i64,
) -> Result<InboundReport, String> {
    let cursor = store.channel_cursor(account_id, POLL_CURSOR_KEY)?;
    let mut batch = adapter.poll(cursor.as_deref()).await?;
    // Files are fetched before the turn becomes durable, so the stored event is
    // the turn as the agent will see it rather than a promise to look later.
    let limits = store
        .channel_account(account_id)
        .ok()
        .flatten()
        .map(|account| {
            super::channel_adapter::AttachmentLimits::for_account(&account.non_secret_config)
        })
        .unwrap_or_default();
    super::channel_adapter::hydrate_attachments(
        adapter,
        &super::channel_adapter::DaemonBlobs,
        limits,
        &mut batch.envelopes,
    )
    .await;
    let report = ingest_batch(store, queue, &batch.envelopes, now_ms);
    if let Some(next) = batch.cursor {
        store.set_channel_cursor(account_id, POLL_CURSOR_KEY, &next, now_ms)?;
    }
    Ok(report)
}

/// What one outbox drain did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct OutboxReport {
    pub sent: u32,
    pub retrying: u32,
    pub failed: u32,
    pub needs_reconciliation: u32,
    /// Rows for an account whose adapter is not loaded. Left claimed-and-
    /// returned rather than failed, since the account may simply be disabled
    /// right now.
    pub skipped: u32,
}

/// Send one batch of queued outbound messages.
pub(crate) async fn drain_outbox_once(
    store: &mut DaemonStore,
    adapters: &BTreeMap<String, Arc<dyn ChannelAdapter>>,
    now_ms: i64,
) -> Result<OutboxReport, String> {
    let claimed = store.claim_outbox_batch(now_ms, OUTBOX_BATCH)?;
    let mut report = OutboxReport::default();
    for row in claimed {
        let Some(adapter) = adapters.get(&row.account_id).cloned() else {
            report.skipped += 1;
            // Back to queued with a short delay: nothing was attempted.
            store.complete_outbox_send(
                &row.outbox_id,
                &SendOutcome::RetryableFailure {
                    error: "No adapter is loaded for this account".to_string(),
                    retry_after_ms: Some(60_000),
                },
                now_ms,
            )?;
            continue;
        };
        let payload: OutboxPayload = match serde_json::from_str(&row.payload_json) {
            Ok(payload) => payload,
            Err(error) => {
                // A payload this process cannot parse will never become
                // parseable by retrying it.
                report.failed += 1;
                store.complete_outbox_send(
                    &row.outbox_id,
                    &SendOutcome::PermanentFailure {
                        error: format!("Stored outbound message is unreadable: {error}"),
                    },
                    now_ms,
                )?;
                continue;
            }
        };
        let outcome = adapter.send(&payload.message).await;
        match &outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                report.sent += 1;
                // The outbound event log is what an operator reads to see what
                // Little Monkey said, and what the inbound side matches a
                // provider echo against.
                let _ = store.record_channel_event(&NewChannelEvent {
                    account_id: row.account_id.clone(),
                    source:
                        little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
                    direction: EventDirection::Outbound,
                    provider_event_id: provider_message_id
                        .clone()
                        .unwrap_or_else(|| format!("local:{}", row.outbox_id)),
                    conversation_id: row.conversation_id.clone(),
                    thread_id: row.thread_id.clone(),
                    sender_id: None,
                    envelope_json: row.payload_json.clone(),
                    disposition: EventDisposition::Accepted,
                    received_at_ms: now_ms,
                });
            }
            SendOutcome::RetryableFailure { .. } => report.retrying += 1,
            SendOutcome::PermanentFailure { .. } => report.failed += 1,
            SendOutcome::NeedsReconciliation { .. } => report.needs_reconciliation += 1,
        }
        store.complete_outbox_send(&row.outbox_id, &outcome, now_ms)?;
    }
    Ok(report)
}

/// Interval between passes when nothing is happening. Long-polling adapters
/// block inside `poll` for their own window, so this only paces the idle case.
const IDLE_TICK_MS: u64 = 2_000;

/// How often the account list and its adapters are rebuilt, so enabling an
/// account in the UI takes effect without restarting the daemon.
const RELOAD_INTERVAL_MS: u64 = 30_000;

/// How often accepted-but-unqueued turns are retried. Slow on purpose: what it
/// retries are local failures — a full queue, an engaged kill switch — and
/// hammering them buys nothing. The submission attempt budget bounds it.
const RECOVERY_INTERVAL_MS: u64 = 60_000;

/// One recovery pass, reporting only when it did something.
fn recover_accepted_turns(paths: &super::store::DaemonPaths, queue: &dyn RunQueue) {
    let (Ok(mut store), Ok(now)) = (DaemonStore::open(paths), current_ms()) else {
        return;
    };
    match channel_ingress::recover_pending_ingress(&mut store, queue, now) {
        Ok(recovery) if recovery.resubmitted + recovery.parked > 0 => eprintln!(
            "monkey daemon: resumed {} accepted turn(s), parked {}",
            recovery.resubmitted, recovery.parked
        ),
        Ok(_) => {}
        Err(error) => eprintln!("monkey daemon: could not resume accepted turns: {error}"),
    }
}

/// Run the channel subsystem for as long as the daemon lives.
///
/// One task for every account rather than one per account: polling is a
/// blocking wait, not a busy loop, and a handful of accounts cost a handful of
/// awaits.
// ponytail: sequential per-account polling. If one slow provider starves the
// others, give each account its own task and keep this as the supervisor.
pub(crate) fn spawn_channel_runtime(paths: super::store::DaemonPaths) {
    tokio::spawn(async move {
        // A send that was in flight when the process died cannot be retried
        // safely, so the first thing a fresh runtime does is park those rows.
        if let Ok(mut store) = DaemonStore::open(&paths) {
            if let Ok(now) = current_ms() {
                if let Ok(parked) = store.requeue_stuck_sending(now) {
                    if parked > 0 {
                        eprintln!(
                            "monkey daemon: {parked} outbound message(s) need reconciliation after a restart"
                        );
                    }
                }
            }
        }

        let queue: Arc<dyn RunQueue> = Arc::new(super::DaemonChannelQueue::new(paths.clone()));

        let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
        let mut next_reload_ms = 0_u64;
        // Zero, so the first pass through the loop is the startup recovery.
        let mut next_recovery_ms = 0_u64;
        // The health state each account last had written for it, so the loop
        // records transitions — a socket dying, a token being revoked, the
        // provider coming back — without rewriting an unchanged row every
        // tick.
        let mut posted_health: BTreeMap<String, little_monkey_lib::channels::types::HealthState> =
            BTreeMap::new();

        loop {
            let now = match current_ms() {
                Ok(now) => now,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
                    continue;
                }
            };
            let now_u64 = u64::try_from(now).unwrap_or(0);

            // Turns that were accepted but never queued are the other half of
            // a crash: whoever sent them considers them delivered, so nobody
            // else will ever send them again. Every origin's turns land in the
            // same table, so this covers all six — and it runs before the
            // adapters load, because a node with no messaging account at all
            // still has desktop, voice, mobile, peer and phone turns to finish.
            //
            // Repeated rather than done once at startup: a turn the queue
            // refused stays accepted, and leaving it until the next restart is
            // not what "recovery will try again" means to whoever is waiting
            // for an answer.
            if now_u64 >= next_recovery_ms {
                next_recovery_ms = now_u64.saturating_add(RECOVERY_INTERVAL_MS);
                recover_accepted_turns(&paths, queue.as_ref());
            }

            if now_u64 >= next_reload_ms {
                next_reload_ms = now_u64.saturating_add(RELOAD_INTERVAL_MS);
                match DaemonStore::open(&paths) {
                    Ok(mut store) => {
                        adapters = load_adapters(&paths, &mut store, now);
                        // Forget what was last written so the fresh adapter
                        // set re-asserts its state once per reload at most.
                        posted_health.clear();
                    }
                    Err(error) => eprintln!("monkey daemon: channels paused: {error}"),
                }
            }

            let mut store = match DaemonStore::open(&paths) {
                Ok(store) => store,
                Err(error) => {
                    eprintln!("monkey daemon: channels paused: {error}");
                    tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
                    continue;
                }
            };

            let mut worked = false;
            for (account_id, adapter) in &adapters {
                match poll_account_once(
                    &mut store,
                    queue.as_ref(),
                    account_id,
                    adapter.as_ref(),
                    now,
                )
                .await
                {
                    Ok(report) => {
                        worked |= report.accepted + report.challenged + report.duplicates > 0;
                        // A poll that came back is the transport working with
                        // this account's real credential — the "actual
                        // authenticated live transport" that may claim
                        // Connected, and the only thing here that may. An
                        // adapter holding a socket open answers for itself
                        // instead: its poll returns an empty batch whether
                        // the connection is live or dropped.
                        record_health_transition(
                            &mut store,
                            &mut posted_health,
                            account_id,
                            health_after_poll(adapter.as_ref()),
                            None,
                            now,
                        );
                    }
                    // One provider being unreachable must not stop the others,
                    // and must not stop the outbox either. It must show up,
                    // though: stored health saying Connected while every poll
                    // fails is the lie the health column exists to prevent.
                    Err(error) => {
                        record_health_transition(
                            &mut store,
                            &mut posted_health,
                            account_id,
                            little_monkey_lib::channels::types::HealthState::Degraded,
                            Some(&error),
                            now,
                        );
                        eprintln!("monkey daemon: channel {account_id} poll: {error}")
                    }
                }
            }

            match drain_outbox_once(&mut store, &adapters, now).await {
                Ok(report) => worked |= report.sent + report.retrying > 0,
                Err(error) => eprintln!("monkey daemon: channel outbox: {error}"),
            }

            if !worked {
                tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
            }
        }
    });
}

/// What a successful poll says about an account's health.
///
/// For a long-polling or webhook adapter the poll is the whole story: it
/// spoke to the provider with this account's real credential and came back.
/// An adapter holding a socket open answers for itself, because its poll
/// returns an empty batch whether the connection is live or dropped.
fn health_after_poll(
    adapter: &dyn ChannelAdapter,
) -> little_monkey_lib::channels::types::HealthState {
    adapter
        .live_transport()
        .unwrap_or(little_monkey_lib::channels::types::HealthState::Connected)
}

/// Persist one account's health when it differs from what was last written.
///
/// The map is the debounce: state transitions are recorded the moment they
/// happen, an unchanged state costs nothing per tick. `last_error` rides
/// along only on the transition, so a failure's first message is kept rather
/// than churned.
fn record_health_transition(
    store: &mut DaemonStore,
    posted: &mut BTreeMap<String, little_monkey_lib::channels::types::HealthState>,
    account_id: &str,
    state: little_monkey_lib::channels::types::HealthState,
    error: Option<&str>,
    now_ms: i64,
) {
    use little_monkey_lib::channels::types::{ChannelHealth, HealthState};
    if posted.get(account_id) == Some(&state) {
        return;
    }
    let health = ChannelHealth {
        state,
        detail: match state {
            HealthState::Connected => Some("Live transport active.".to_string()),
            HealthState::Degraded => {
                Some("The provider is unreachable; the daemon keeps retrying.".to_string())
            }
            HealthState::Error => Some("The account could not be started.".to_string()),
            _ => None,
        },
        last_error: error.map(str::to_string),
        probed_at_ms: now_ms,
    };
    match store.set_channel_account_health(account_id, &health, now_ms) {
        Ok(()) => {
            posted.insert(account_id.to_string(), state);
        }
        Err(error) => eprintln!("monkey daemon: channel {account_id} health: {error}"),
    }
}

/// Build an adapter for every enabled account, resolving each credential from
/// the keychain at load time so no adapter ever reads it itself.
///
/// An account that cannot be built — unreadable credential, config its
/// adapter refuses — has that failure written to its stored health rather
/// than only to the daemon's log: the operator looking at the Channels panel
/// is the one who can fix it.
fn load_adapters(
    paths: &super::store::DaemonPaths,
    store: &mut DaemonStore,
    now_ms: i64,
) -> BTreeMap<String, Arc<dyn ChannelAdapter>> {
    use super::channel_adapter::{AdapterConfig, ChannelSecrets, KeyringChannelSecrets};

    let secrets = KeyringChannelSecrets;
    let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
    let accounts = match store.channel_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            eprintln!("monkey daemon: could not read channel accounts: {error}");
            return adapters;
        }
    };
    // Written only on a state change, like the poll loop's transitions: a
    // permanently broken account is one row update, not one per reload.
    let mut mark_failed = |store: &mut DaemonStore,
                           account: &super::channel_store::ChannelAccountRecord,
                           error: &str| {
        use little_monkey_lib::channels::types::{ChannelHealth, HealthState};
        if account.health.state == HealthState::Error
            && account.health.last_error.as_deref() == Some(error)
        {
            return;
        }
        let health = ChannelHealth {
            state: HealthState::Error,
            detail: Some("The account could not be started.".to_string()),
            last_error: Some(error.to_string()),
            probed_at_ms: now_ms,
        };
        if let Err(write_error) =
            store.set_channel_account_health(&account.account_id, &health, now_ms)
        {
            eprintln!(
                "monkey daemon: channel {} health: {write_error}",
                account.account_id
            );
        }
    };
    for account in accounts.into_iter().filter(|account| account.enabled) {
        // An SMS account's carrier credential lives on the telephony account of
        // the same id, not on this row, so it is built from there.
        if account.kind == little_monkey_lib::channels::types::ChannelKind::Sms {
            match build_sms_adapter(paths, store, &secrets, &account.account_id) {
                Ok(adapter) => {
                    adapters.insert(account.account_id.clone(), adapter);
                }
                Err(error) => {
                    eprintln!(
                        "monkey daemon: SMS account {} cannot send: {error}",
                        account.account_id
                    );
                    mark_failed(store, &account, &error);
                }
            }
            continue;
        }
        let secret = match &account.credential_ref {
            Some(reference) => match secrets.get(reference) {
                Ok(secret) => secret,
                Err(error) => {
                    eprintln!(
                        "monkey daemon: channel account {} has no usable credential: {error}",
                        account.account_id
                    );
                    mark_failed(store, &account, &error);
                    continue;
                }
            },
            None => String::new(),
        };
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        match super::adapters::build_adapter(&config) {
            Ok(adapter) => {
                adapters.insert(account.account_id.clone(), adapter);
            }
            Err(error) => {
                eprintln!(
                    "monkey daemon: channel account {} is not runnable: {error}",
                    account.account_id
                );
                mark_failed(store, &account, &error);
            }
        }
    }
    adapters
}

/// Build the adapter that answers texts for one telephony account.
///
/// Separate from the loop above because it reads a different table: an SMS
/// channel account is a shadow of a telephony account (see
/// `telecom_worker::ensure_sms_channel_account`), and the carrier credential
/// only ever sits on the telephony row.
fn build_sms_adapter(
    paths: &super::store::DaemonPaths,
    store: &DaemonStore,
    secrets: &dyn super::channel_adapter::ChannelSecrets,
    account_id: &str,
) -> Result<Arc<dyn ChannelAdapter>, String> {
    let telecom = store
        .telecom_account(account_id)?
        .ok_or_else(|| "its telephony account no longer exists".to_string())?;
    let secret = match &telecom.credential_ref {
        Some(reference) => secrets.get(reference)?,
        None => String::new(),
    };
    // The app data directory is the daemon root's parent, the same derivation
    // the remote API uses to find the shared blob store.
    let app_data = paths
        .root
        .parent()
        .map(std::path::Path::to_path_buf)
        .ok_or_else(|| "the daemon root has no app-data parent".to_string())?;
    Ok(Arc::new(super::adapters::sms::SmsAdapter::new(
        &telecom, secret, app_data,
    )?))
}

fn current_ms() -> Result<i64, String> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "System clock is before the Unix epoch".to_string())?
            .as_millis(),
    )
    .map_err(|_| "System clock is beyond the supported range".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
    use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
    use little_monkey_lib::channels::types::{
        ChannelConversation, ChannelHealth, ChannelKind, ChannelSender, InboundTransport,
        OutboundMessage, ProviderCapabilities,
    };
    use std::sync::Mutex;

    use super::super::channel_adapter::InboundBatch;
    use super::super::channel_store::ChannelAccountRecord;

    const NOW: i64 = 1_700_000_000_000;

    #[derive(Default)]
    struct FakeQueue {
        submitted: Mutex<Vec<String>>,
        fail: bool,
    }

    impl RunQueue for FakeQueue {
        fn freeze_execution(
            &self,
            ingress: &ConversationIngress,
        ) -> Result<FrozenExecutionContext, String> {
            Ok(super::test_frozen_execution(ingress))
        }

        fn submit(
            &self,
            ingress: &ConversationIngress,
            _params: Vec<String>,
        ) -> Result<String, String> {
            if self.fail {
                return Err("queue is full".to_string());
            }
            let job_id = ingress.deterministic_job_id();
            self.submitted.lock().unwrap().push(job_id.clone());
            Ok(job_id)
        }
    }

    struct FakeAdapter {
        batches: Mutex<Vec<InboundBatch>>,
        outcomes: Mutex<Vec<SendOutcome>>,
        sent: Mutex<Vec<OutboundMessage>>,
        /// What a socket-holding adapter would report. `None` is every
        /// long-polling and webhook provider.
        live: Option<little_monkey_lib::channels::types::HealthState>,
    }

    impl FakeAdapter {
        fn new() -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                outcomes: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                live: None,
            }
        }

        fn with_live(state: little_monkey_lib::channels::types::HealthState) -> Self {
            Self {
                live: Some(state),
                ..Self::new()
            }
        }
    }

    #[async_trait]
    impl ChannelAdapter for FakeAdapter {
        fn kind(&self) -> ChannelKind {
            ChannelKind::Telegram
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities::minimal(ChannelKind::Telegram, InboundTransport::LongPoll)
        }

        fn live_transport(&self) -> Option<little_monkey_lib::channels::types::HealthState> {
            self.live
        }

        async fn probe(&self) -> ChannelHealth {
            ChannelHealth::connected(NOW, None)
        }

        async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
            let mut batches = self.batches.lock().unwrap();
            if batches.is_empty() {
                return Ok(InboundBatch::default());
            }
            Ok(batches.remove(0))
        }

        async fn send(&self, message: &OutboundMessage) -> SendOutcome {
            self.sent.lock().unwrap().push(message.clone());
            let mut outcomes = self.outcomes.lock().unwrap();
            if outcomes.is_empty() {
                return SendOutcome::Sent {
                    provider_message_id: Some("provider-1".to_string()),
                };
            }
            outcomes.remove(0)
        }
    }

    fn seeded_store() -> DaemonStore {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .upsert_channel_account(&ChannelAccountRecord {
                account_id: "acct-1".into(),
                kind: ChannelKind::Telegram,
                label: "Ops".into(),
                enabled: true,
                non_secret_config: serde_json::json!({}),
                credential_ref: Some("channel:acct-1".into()),
                access_policy: ChannelAccessPolicy {
                    direct: AccessPolicy::Open,
                    group: AccessPolicy::Open,
                    group_activation: GroupActivation::Always,
                },
                health: ChannelHealth::connected(NOW, None),
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("account");
        store
            .insert_channel_route(&ChannelRoute {
                route_id: "route-1".into(),
                scope: RouteScope::account("acct-1"),
                target: RouteTarget::new("chat"),
                enabled: true,
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("route");
        store
    }

    fn envelope(event_id: &str) -> ChannelEnvelope {
        ChannelEnvelope {
            account_id: "acct-1".into(),
            kind: ChannelKind::Telegram,
            provider_event_id: event_id.into(),
            conversation: ChannelConversation::direct("chat-7"),
            sender: ChannelSender::new("user-3"),
            text: "ship it".into(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            mentions_self: false,
            received_at_ms: NOW,
            metadata: Default::default(),
        }
    }

    #[test]
    fn a_batch_becomes_runs_and_a_replay_of_it_does_not() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let batch = vec![envelope("1"), envelope("2")];

        let first = ingest_batch(&mut store, &queue, &batch, NOW);
        assert_eq!(first.accepted, 2);
        assert_eq!(first.duplicates, 0);

        let replay = ingest_batch(&mut store, &queue, &batch, NOW);
        assert_eq!(replay.accepted, 0);
        assert_eq!(replay.duplicates, 2);
        assert_eq!(queue.submitted.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_queue_failure_is_recorded_and_does_not_stop_the_batch() {
        let mut store = seeded_store();
        let queue = FakeQueue {
            fail: true,
            ..Default::default()
        };

        let report = ingest_batch(&mut store, &queue, &[envelope("1"), envelope("2")], NOW);
        assert_eq!(report.deferred, 2);
        assert_eq!(report.accepted, 0);
        assert_eq!(report.failed, 0);

        // The queue refused, but the turns are not lost: both are durably
        // accepted, and the next recovery pass is what re-submits them.
        let events = store.recent_channel_events("acct-1", 10).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|event| event.disposition == EventDisposition::Accepted));
        assert!(events.iter().all(|event| event.job_id.is_none()));
        assert_eq!(store.pending_ingress_turns(10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn the_cursor_only_advances_after_the_batch_is_recorded() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let adapter = FakeAdapter::new();
        adapter.batches.lock().unwrap().push(InboundBatch {
            envelopes: vec![envelope("1")],
            cursor: Some("42".to_string()),
        });

        assert_eq!(
            store.channel_cursor("acct-1", POLL_CURSOR_KEY).unwrap(),
            None
        );
        let report = poll_account_once(&mut store, &queue, "acct-1", &adapter, NOW)
            .await
            .expect("poll");
        assert_eq!(report.accepted, 1);
        assert_eq!(
            store.channel_cursor("acct-1", POLL_CURSOR_KEY).unwrap(),
            Some("42".to_string())
        );

        // A batch with no cursor leaves the stored one alone.
        adapter.batches.lock().unwrap().push(InboundBatch {
            envelopes: vec![envelope("2")],
            cursor: None,
        });
        poll_account_once(&mut store, &queue, "acct-1", &adapter, NOW)
            .await
            .expect("poll");
        assert_eq!(
            store.channel_cursor("acct-1", POLL_CURSOR_KEY).unwrap(),
            Some("42".to_string())
        );
    }

    fn queue_reply(store: &mut DaemonStore, idempotency_key: &str) -> String {
        let payload = OutboxPayload {
            message: OutboundMessage {
                account_id: "acct-1".into(),
                kind: ChannelKind::Telegram,
                conversation_id: "chat-7".into(),
                thread_id: None,
                text: "on it".into(),
                attachments: Vec::new(),
                reply_to_provider_id: None,
                idempotency_key: idempotency_key.to_string(),
            },
            reply_depth: 0,
        };
        let payload_json = serde_json::to_string(&payload).unwrap();
        match store
            .enqueue_channel_message(&super::super::channel_store::NewOutboxMessage {
                account_id: "acct-1".into(),
                conversation_id: "chat-7".into(),
                thread_id: None,
                reply_to_provider_id: None,
                payload_digest: "digest".into(),
                payload_json,
                idempotency_key: idempotency_key.to_string(),
                max_attempts: 3,
                job_id: None,
                created_at_ms: NOW,
            })
            .unwrap()
        {
            super::super::channel_store::OutboxEnqueue::Queued { outbox_id }
            | super::super::channel_store::OutboxEnqueue::AlreadyQueued { outbox_id } => outbox_id,
        }
    }

    fn adapters_with(adapter: Arc<FakeAdapter>) -> BTreeMap<String, Arc<dyn ChannelAdapter>> {
        let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
        adapters.insert("acct-1".to_string(), adapter);
        adapters
    }

    #[tokio::test]
    async fn a_sent_message_is_logged_as_an_outbound_event() {
        let mut store = seeded_store();
        queue_reply(&mut store, "reply-1");
        let adapter = Arc::new(FakeAdapter::new());

        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);

        let events = store.recent_channel_events("acct-1", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].direction, EventDirection::Outbound);
        assert_eq!(events[0].provider_event_id, "provider-1");

        // Nothing is left to claim.
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unproven_send_is_never_retried() {
        let mut store = seeded_store();
        queue_reply(&mut store, "reply-1");
        let adapter = Arc::new(FakeAdapter::new());
        adapter
            .outcomes
            .lock()
            .unwrap()
            .push(SendOutcome::NeedsReconciliation {
                error: "connection dropped after the request was written".into(),
            });

        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW)
            .await
            .expect("drain");
        assert_eq!(report.needs_reconciliation, 1);

        // Not claimable now, and not claimable later either.
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
        assert!(store
            .claim_outbox_batch(NOW + 60 * 60 * 1000, 10)
            .unwrap()
            .is_empty());
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_retryable_failure_comes_back_later() {
        let mut store = seeded_store();
        queue_reply(&mut store, "reply-1");
        let adapter = Arc::new(FakeAdapter::new());
        adapter
            .outcomes
            .lock()
            .unwrap()
            .push(SendOutcome::RetryableFailure {
                error: "rate limited".into(),
                retry_after_ms: Some(5_000),
            });

        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW)
            .await
            .expect("drain");
        assert_eq!(report.retrying, 1);
        assert!(store
            .claim_outbox_batch(NOW + 1_000, 10)
            .unwrap()
            .is_empty());

        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 6_000)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        assert_eq!(adapter.sent.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_message_for_an_unloaded_account_waits_rather_than_failing() {
        let mut store = seeded_store();
        queue_reply(&mut store, "reply-1");

        let report = drain_outbox_once(&mut store, &BTreeMap::new(), NOW)
            .await
            .expect("drain");
        assert_eq!(report.skipped, 1);
        assert_eq!(report.failed, 0);
        assert!(!store
            .claim_outbox_batch(NOW + 120_000, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_socket_adapter_answers_for_its_own_connection() {
        use little_monkey_lib::channels::types::HealthState;
        // A long-polling provider: the poll came back, so it is connected.
        assert_eq!(
            health_after_poll(&FakeAdapter::new()),
            HealthState::Connected
        );
        // A socket provider whose connection has dropped polls exactly the
        // same way — empty batch, no error — so recording Connected off the
        // back of that would be the lie the health column exists to prevent.
        for state in [
            HealthState::Connecting,
            HealthState::Degraded,
            HealthState::Error,
        ] {
            assert_eq!(health_after_poll(&FakeAdapter::with_live(state)), state);
        }
    }

    #[test]
    fn health_is_written_on_a_change_and_not_on_every_tick() {
        use little_monkey_lib::channels::types::HealthState;
        let mut store = seeded_store();
        let mut posted = BTreeMap::new();

        record_health_transition(
            &mut store,
            &mut posted,
            "acct-1",
            HealthState::Degraded,
            Some("connection dropped"),
            NOW,
        );
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.state, HealthState::Degraded);
        assert_eq!(
            account.health.last_error.as_deref(),
            Some("connection dropped")
        );

        // The same state again writes nothing new — the error message from the
        // first transition is kept rather than churned.
        record_health_transition(
            &mut store,
            &mut posted,
            "acct-1",
            HealthState::Degraded,
            Some("a later, less useful message"),
            NOW + 1_000,
        );
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.probed_at_ms, NOW);

        record_health_transition(
            &mut store,
            &mut posted,
            "acct-1",
            HealthState::Connected,
            None,
            NOW + 2_000,
        );
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.state, HealthState::Connected);
        assert_eq!(account.health.last_error, None);
    }

    // -- The agent tool, end to end -----------------------------------------

    use super::super::channel_tool::{plan_send, queue_send, ChannelSendRequest, SendAuthority};
    use super::super::store::DaemonPaths;

    /// A throwaway app-data root, per test, so nothing here can reach the
    /// daemon state or the shared ledger of the machine running the suite.
    fn scratch_root() -> std::path::PathBuf {
        let root =
            std::env::temp_dir().join(format!("lm-channel-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&root).expect("scratch root");
        root
    }

    /// Paths whose artifact store is only ever touched by a send carrying
    /// files; a text-only send never opens anything under here.
    fn scratch_paths() -> DaemonPaths {
        DaemonPaths::under(&scratch_root())
    }

    fn send_to(conversation_id: &str, text: &str) -> ChannelSendRequest {
        ChannelSendRequest {
            account_id: Some("acct-1".to_string()),
            conversation_id: Some(conversation_id.to_string()),
            text: text.to_string(),
            ..ChannelSendRequest::default()
        }
    }

    /// The grant an operator gives a run that may reach one named account.
    fn account_authority() -> SendAuthority {
        SendAuthority {
            accounts: vec!["acct-1".to_string()],
            ..SendAuthority::default()
        }
    }

    #[tokio::test]
    async fn the_agent_tool_queues_a_message_the_worker_then_delivers() {
        // The whole path in one test: authority, a normalized outbound
        // message, a durable outbox row, and the provider adapter receiving
        // exactly what was queued. The tool itself never touches an adapter.
        let mut store = seeded_store();
        let paths = scratch_paths();
        let request = send_to("chat-9", "the build passed");
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");

        let queued = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            Some("job-1"),
            NOW,
        )
        .expect("queued");
        assert_eq!(queued["status"], "queued");
        assert!(queued["outbox_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        // Durable before anything is delivered: the row exists against the
        // job, which is also what the next call's idempotency key counts.
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);

        let adapter = Arc::new(FakeAdapter::new());
        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);

        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].account_id, "acct-1");
        assert_eq!(sent[0].conversation_id, "chat-9");
        assert_eq!(sent[0].text, "the build passed");
        // Keyed on the job, so a retried run cannot duplicate the send.
        assert_eq!(sent[0].idempotency_key, "send-job-1-0");

        let events = store.recent_channel_events("acct-1", 10).unwrap();
        assert_eq!(events[0].direction, EventDirection::Outbound);
    }

    #[tokio::test]
    async fn a_send_the_run_was_not_granted_never_reaches_the_outbox() {
        // The refusal is the point, but so is what did not happen: no row, so
        // nothing for the worker to find and nothing for an adapter to send.
        let mut store = seeded_store();
        let paths = scratch_paths();
        let request = send_to("chat-9", "exfiltrate this");

        let refused = plan_send(&request, &SendAuthority::default(), None)
            .expect_err("no grant for that account");
        assert!(refused.contains("does not grant"), "{refused}");

        // And the reply grant alone does not reach another account either.
        let reply_only = SendAuthority {
            reply: true,
            ..SendAuthority::default()
        };
        assert!(plan_send(&request, &reply_only, None).is_err());

        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW)
            .await
            .expect("drain");
        assert!(adapter.sent.lock().unwrap().is_empty());
        let _ = paths;
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_be_sent_through() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.enabled = false;
        store.upsert_channel_account(&account).unwrap();

        let request = send_to("chat-9", "still there?");
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            Some("job-1"),
            NOW,
        )
        .expect_err("disabled");
        assert!(error.contains("disabled"), "{error}");
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_queued_message_survives_a_restart_and_is_still_delivered() {
        // The daemon stopping between "queued" and "sent" is the ordinary
        // case, not the exotic one: the row is the only thing that carries
        // the send, so it has to outlive the process that wrote it.
        let paths = DaemonPaths::under(&scratch_root());

        let outbox_id = {
            let mut store = DaemonStore::open(&paths).expect("open");
            store
                .upsert_channel_account(&ChannelAccountRecord {
                    account_id: "acct-1".into(),
                    kind: ChannelKind::Telegram,
                    label: "Ops".into(),
                    enabled: true,
                    non_secret_config: serde_json::json!({}),
                    credential_ref: Some("channel:acct-1".into()),
                    access_policy: ChannelAccessPolicy::default(),
                    health: ChannelHealth::connected(NOW, None),
                    created_at_ms: NOW,
                    updated_at_ms: NOW,
                })
                .expect("account");
            let request = send_to("chat-9", "queued before the crash");
            let plan = plan_send(&request, &account_authority(), None).expect("authorized");
            let queued = queue_send(
                &mut store,
                &paths,
                &request,
                &plan,
                None,
                Some("job-restart"),
                NOW,
            )
            .expect("queued");
            queued["outbox_id"].as_str().expect("id").to_string()
        }; // the daemon stops here

        let mut store = DaemonStore::open(&paths).expect("reopen");
        let adapter = Arc::new(FakeAdapter::new());
        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent[0].text, "queued before the crash");
        // The same durable identity throughout — not a second message.
        assert!(!outbox_id.is_empty());
        assert!(store
            .claim_outbox_batch(NOW + 120_000, 10)
            .unwrap()
            .is_empty());
    }
}

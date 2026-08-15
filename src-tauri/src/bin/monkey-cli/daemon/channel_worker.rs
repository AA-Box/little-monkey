//! The loops that make channels move: inbound polling, the accepted-message
//! processor, and the outbox.
//!
//! All three are deliberately small and all three are crash-safe by
//! construction rather than by care:
//!
//! - **Polled inbound.** An adapter's batch is handed one envelope at a time to
//!   `channel_ingress::accept_channel_envelope`, which records and deduplicates
//!   before it decides anything. The transport cursor is only advanced *after*
//!   the batch is durably recorded, so a crash mid-batch replays messages that
//!   the event log then collapses — the opposite order would lose them.
//! - **Delivered-to inbound.** A webhook provider is answered as soon as its
//!   event is committed, which is before anything has been downloaded, routed
//!   or run. Everything after that acknowledgement is
//!   [`process_pending_channel_ingress`], which continues each accepted row —
//!   hydrate, decide, freeze, submit — and picks up wherever a restart left
//!   off. That split is the whole reason a provider is never made to wait on a
//!   media host, a route table or a queue.
//! - **Outbound.** A claimed row is in `sending` before the request goes out.
//!   If the process dies there, `requeue_stuck_sending` moves it to
//!   `needs_reconciliation` rather than retrying it, because a send that may
//!   have reached the provider is not safe to repeat.
//!
//! None of them executes an agent. Inbound work becomes a normal durable run
//! through [`RunQueue`], which production implements with the daemon's one
//! `enqueue`.

use std::collections::BTreeMap;
use std::sync::Arc;

use little_monkey_lib::channels::ingress::{
    ConversationIngress, FrozenExecutionContext, FrozenExecutionContextV1,
};
use little_monkey_lib::channels::types::{ChannelEnvelope, SendOutcome};

use super::channel_adapter::ChannelAdapter;
use super::channel_ingress::{self, ChannelAcceptance, OutboxPayload, SubmitOutcome};
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
    /// Envelopes that never crossed the durable acceptance boundary (a storage
    /// failure, or a decision that could not be committed). Tracked separately
    /// from `failed` because the cursor must not advance past them: everything
    /// in `failed` has a durable row a human can see, these have nothing, and
    /// only redelivery from the provider can bring them back.
    pub unrecorded: u32,
    /// The provider event ids that *did* cross it, in arrival order.
    ///
    /// This is the list a transport with its own delivery handshake may
    /// acknowledge, and it is deliberately not "everything in the batch": one
    /// envelope that failed to commit must not take the acknowledgement of its
    /// siblings with it, and must not be acknowledged itself.
    pub ack_safe: Vec<String>,
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
        let accepted =
            match channel_ingress::accept_channel_envelope(store, queue, envelope, now_ms) {
                Ok(accepted) => accepted,
                // Nothing was committed. Not `failed`: that bucket has a durable
                // row an operator can see, this one has nothing at all, so the
                // provider must be left to redeliver it.
                Err(_) => {
                    report.unrecorded += 1;
                    continue;
                }
            };
        // Everything below this line is already durable. Whatever happens to
        // the queue submission, the message is recoverable and the provider may
        // be told we have it.
        report.ack_safe.push(envelope.provider_event_id.clone());
        match accepted {
            ChannelAcceptance::Run {
                event_id,
                ingress_id,
                ingress,
                params,
                attempts,
            } => match channel_ingress::submit_accepted_turn(
                store,
                queue,
                &ingress,
                &params,
                &ingress_id,
                attempts,
                now_ms,
            ) {
                Ok(SubmitOutcome::Queued { job_id, .. })
                | Ok(SubmitOutcome::AlreadyQueued { job_id, .. }) => {
                    report.accepted += 1;
                    // Best effort: the run is already queued, and failing to
                    // annotate the event must not undo it.
                    let _ = store.set_channel_event_disposition(
                        &event_id,
                        EventDisposition::Accepted,
                        None,
                        Some(&job_id),
                    );
                }
                Ok(SubmitOutcome::Deferred { error, .. }) => {
                    // Durably accepted, not queued yet. Counted as a failure of
                    // this pass, but the turn is not lost.
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
                    // The submission never happened; the accepted turn is still
                    // durable, so recovery owns it and the event keeps saying so.
                    report.deferred += 1;
                    let _ = store.set_channel_event_disposition(
                        &event_id,
                        EventDisposition::Accepted,
                        Some(&error),
                        None,
                    );
                }
            },
            ChannelAcceptance::Challenge { .. } => report.challenged += 1,
            ChannelAcceptance::Ignore { .. } => report.ignored += 1,
            ChannelAcceptance::Refused { .. } => report.failed += 1,
            ChannelAcceptance::Duplicate { .. } => report.duplicates += 1,
        }
    }
    report
}

/// How many accepted-but-unprocessed inbound events one pass continues.
///
/// Bounded like every other sweep here: a backlog must not hold the supervisor
/// tick, and what is left is picked up two seconds later.
const PENDING_BATCH: u32 = 32;

/// How long one message's files may hold the channel worker.
///
/// No provider socket is waiting on this any more — the delivery was
/// acknowledged before any of it started — so the budget is generous. It
/// exists only so one media host that has gone quiet cannot stall every other
/// account's messages behind it for the download client's own ten minutes.
/// Blowing it costs the attachment, never the message.
const HYDRATION_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// What one pass over the accepted-but-unprocessed events did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PendingIngressReport {
    /// Messages that now own a queued run.
    pub queued: u32,
    /// Messages that reached a final decision that never runs — ignored,
    /// challenged, refused, or already handled by an earlier pass.
    pub settled: u32,
    /// Durably accepted turns that did not reach the queue this pass. Not
    /// lost: `recover_pending_ingress` owns them from here.
    pub deferred: u32,
    /// Rows recorded as failed, for an operator to look at.
    pub parked: u32,
}

/// Continue every inbound event that was accepted but never processed.
///
/// **This is everything the webhook route deliberately does not do.** A
/// delivered-to provider is acknowledged as soon as its event is committed, so
/// each row here is a message that is already ours and is owed the rest of the
/// path: download what it referenced, decide it against the operator's policy
/// and routes, freeze what it will execute with, and submit the run.
///
/// Every step is restart-safe because each one commits before the next begins.
/// A process that dies anywhere in here leaves a row that the next pass — in
/// this process or the one after the restart — selects again and continues
/// from, and the acceptance transaction underneath collapses a redelivery onto
/// the same turn. The result either way is exactly one run per provider event.
///
/// `fetchers` are the account adapters the supervisor already keeps loaded; an
/// account with no adapter loaded still has its message decided, with the files
/// it could not fetch carrying that as their reason rather than silently
/// looking like no attachment at all.
pub(crate) async fn process_pending_channel_ingress(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    fetchers: &BTreeMap<String, Arc<dyn ChannelAdapter>>,
    blobs: &dyn super::channel_adapter::BlobSource,
    now_ms: i64,
) -> Result<PendingIngressReport, String> {
    let mut report = PendingIngressReport::default();
    for pending in store.accepted_events_awaiting_processing(PENDING_BATCH)? {
        let envelope: ChannelEnvelope = match serde_json::from_str(&pending.envelope_json) {
            Ok(envelope) => envelope,
            Err(error) => {
                // Unreadable, so it can never be decided. Recorded as failed
                // where an operator can see it rather than left to be selected
                // again forever.
                let _ = store.set_channel_event_disposition(
                    &pending.event_id,
                    EventDisposition::Failed,
                    Some(&format!(
                        "This message was accepted but its stored envelope cannot be read \
                         back: {error}"
                    )),
                    None,
                );
                report.parked += 1;
                continue;
            }
        };
        let envelope = hydrate_pending_event(store, fetchers, blobs, &pending, envelope).await?;
        super::fail_points::fire(super::fail_points::FailPoint::AfterAttachmentHydration)?;

        match channel_ingress::accept_channel_envelope(store, queue, &envelope, now_ms) {
            Ok(ChannelAcceptance::Run {
                event_id,
                ingress_id,
                ingress,
                params,
                attempts,
            }) => match channel_ingress::submit_accepted_turn(
                store,
                queue,
                &ingress,
                &params,
                &ingress_id,
                attempts,
                now_ms,
            ) {
                Ok(SubmitOutcome::Queued { job_id, .. })
                | Ok(SubmitOutcome::AlreadyQueued { job_id, .. }) => {
                    report.queued += 1;
                    let _ = store.set_channel_event_disposition(
                        &event_id,
                        EventDisposition::Accepted,
                        None,
                        Some(&job_id),
                    );
                }
                Ok(SubmitOutcome::Parked { .. }) => {
                    report.parked += 1;
                    let _ = store.set_channel_event_disposition(
                        &event_id,
                        EventDisposition::Failed,
                        Some("The turn could not be queued and was parked"),
                        None,
                    );
                }
                Ok(SubmitOutcome::Deferred { error, .. }) | Err(error) => {
                    // The turn is durable and the event now points at it, so
                    // this row leaves this queue and `recover_pending_ingress`
                    // takes it from here.
                    report.deferred += 1;
                    let _ = store.set_channel_event_disposition(
                        &event_id,
                        EventDisposition::Accepted,
                        Some(&error),
                        None,
                    );
                }
            },
            Ok(_) => report.settled += 1,
            // Nothing was committed, so the row keeps its place in this queue
            // and the next pass tries again — a store that is briefly
            // unavailable must not turn an accepted message into a failed one.
            // The reason is written where an operator reads it, so a row that
            // keeps failing is visible rather than silently stuck.
            Err(error) => {
                report.deferred += 1;
                let _ = store.set_channel_event_disposition(
                    &pending.event_id,
                    EventDisposition::Accepted,
                    Some(&error),
                    None,
                );
            }
        }
    }
    Ok(report)
}

/// Download whatever one accepted event referenced, and store the result.
///
/// Persisted before the message is routed, which is what makes a crash here
/// cost nothing: a file already on disk is not fetched twice, and one that
/// failed carries its reason into the turn instead of reading as no attachment
/// at all.
async fn hydrate_pending_event(
    store: &mut DaemonStore,
    fetchers: &BTreeMap<String, Arc<dyn ChannelAdapter>>,
    blobs: &dyn super::channel_adapter::BlobSource,
    pending: &super::channel_store::PendingChannelEvent,
    envelope: ChannelEnvelope,
) -> Result<ChannelEnvelope, String> {
    let mut batch = [envelope];
    if !super::channel_adapter::needs_hydration(&batch) {
        let [envelope] = batch;
        return Ok(envelope);
    }
    let limits = store
        .channel_account(&pending.account_id)
        .ok()
        .flatten()
        .map(|account| {
            super::channel_adapter::AttachmentLimits::for_account(&account.non_secret_config)
        })
        .unwrap_or_default();
    match fetchers.get(&pending.account_id) {
        Some(adapter) => {
            let hydration = super::channel_adapter::hydrate_attachments(
                adapter.as_ref(),
                blobs,
                limits,
                &mut batch,
            );
            if tokio::time::timeout(HYDRATION_BUDGET, hydration)
                .await
                .is_err()
            {
                super::channel_adapter::note_unfetched_attachments(
                    &mut batch,
                    "The provider's media host did not answer in time",
                );
            }
        }
        None => super::channel_adapter::note_unfetched_attachments(
            &mut batch,
            "This account's provider connection was not running when the file was due to be \
             downloaded",
        ),
    }
    store.set_channel_event_envelope(
        &pending.event_id,
        &serde_json::to_string(&batch[0]).map_err(|error| error.to_string())?,
    )?;
    let [envelope] = batch;
    Ok(envelope)
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
    // The worker knows which account it polled; the adapter does not get a
    // vote. This both fills the field for adapters that leave it blank and
    // stops a confused adapter from writing into another account's event log.
    for envelope in &mut batch.envelopes {
        envelope.account_id = account_id.to_string();
    }
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
    // An envelope that never crossed the acceptance boundary has no durable
    // trace, so the cursor holds and the provider redelivers the whole batch;
    // the ones that did land collapse as duplicates. Advancing here would skip
    // the unaccepted message forever — a cursor is ordinal, and there is no way
    // to say "everything past this except that one".
    if report.unrecorded == 0 {
        super::fail_points::fire(super::fail_points::FailPoint::BeforeCursorCommit)
            .map_err(|error| error.to_string())?;
        if let Some(next) = batch.cursor {
            store.set_channel_cursor(account_id, POLL_CURSOR_KEY, &next, now_ms)?;
        }
    }
    // A transport with its own per-message handshake is not ordinal, so it
    // acknowledges exactly what became durable and leaves the rest to be
    // redelivered — even when a sibling in the same batch held the cursor back.
    if !report.ack_safe.is_empty() {
        let acknowledged: Vec<ChannelEnvelope> = batch
            .envelopes
            .iter()
            .filter(|envelope| report.ack_safe.contains(&envelope.provider_event_id))
            .cloned()
            .collect();
        adapter.commit_batch(&acknowledged).await;
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
            // Back to queued with a short delay, and with the claim's attempt
            // handed back: nothing was attempted, and an account that stays
            // disabled for an hour must not exhaust max_attempts without a
            // single real send.
            store.release_outbox_claim(&row.outbox_id, 60_000, now_ms)?;
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

/// One account's inbound worker: the live adapter plus the task polling it.
///
/// The fingerprint is what makes a reload cheap and safe: an unchanged account
/// keeps its adapter — and therefore its gateway/socket session — instead of
/// being torn down and rebuilt every [`RELOAD_INTERVAL_MS`], which for a
/// socket transport would mean a fresh provider session every thirty seconds
/// and a lingering old one beside it.
struct AccountWorker {
    fingerprint: String,
    adapter: Arc<dyn ChannelAdapter>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for AccountWorker {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Run the channel subsystem for as long as the daemon lives.
///
/// One inbound task per account: a long-polling adapter blocks inside `poll`
/// for its own window (Telegram waits 25 seconds on a quiet chat), and in a
/// shared loop that wait would also be the latency ceiling for every other
/// account's acknowledgements. The supervisor task keeps the account list
/// fresh, drains the outbox, and re-submits accepted-but-unqueued turns.
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

        let mut workers: BTreeMap<String, AccountWorker> = BTreeMap::new();
        let mut next_reload_ms = 0_u64;
        // Zero, so the first pass through the loop is the startup recovery.
        let mut next_recovery_ms = 0_u64;

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
                    Ok(mut store) => reconcile_workers(&paths, &mut store, &queue, &mut workers),
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

            let adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = workers
                .iter()
                .map(|(account_id, worker)| (account_id.clone(), worker.adapter.clone()))
                .collect();
            let mut worked = false;
            // Messages a delivered-to provider has already been told we have.
            // Everything past the acknowledgement happens here — the download,
            // the routing, the run — so this is also the pass that finishes
            // whatever a restart interrupted.
            match process_pending_channel_ingress(
                &mut store,
                queue.as_ref(),
                &adapters,
                &super::channel_adapter::DaemonBlobs,
                now,
            )
            .await
            {
                Ok(report) => {
                    worked |= report.queued + report.settled + report.parked > 0;
                    if report.parked > 0 {
                        eprintln!(
                            "monkey daemon: {} accepted message(s) could not be handled",
                            report.parked
                        );
                    }
                }
                Err(error) => eprintln!("monkey daemon: channel ingress: {error}"),
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

/// How often an unchanged health state is re-asserted anyway, so a row some
/// other writer (a manual `channels probe`, an operator edit) overwrote does
/// not stay stale behind the debounce forever.
const HEALTH_REASSERT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// What a successful poll says about an account's health, if anything.
///
/// For a long-polling or helper adapter the poll is the whole story: it spoke
/// to the provider with this account's real credential and came back. An
/// adapter holding a socket open answers for itself, because its poll returns
/// an empty batch whether the connection is live or dropped. And a webhook
/// adapter's poll is a local no-op that spoke to nobody — its success proves
/// nothing, so it moves health nowhere: for those accounts Connected can only
/// come from a real probe.
fn health_after_poll(
    adapter: &dyn ChannelAdapter,
) -> Option<little_monkey_lib::channels::types::HealthState> {
    if let Some(live) = adapter.live_transport() {
        return Some(live);
    }
    match adapter.capabilities().inbound_transport {
        little_monkey_lib::channels::types::InboundTransport::Webhook => None,
        // A helper's poll proves the helper answered, and nothing more. For
        // Signal that is a running process, not a registered number; for
        // iMessage it is a readable database, not permission to reply. Both
        // are capabilities only a real probe measures, so the poll moves
        // health nowhere and [`probe_health`] is what writes it.
        little_monkey_lib::channels::types::InboundTransport::Helper => None,
        _ => Some(little_monkey_lib::channels::types::HealthState::Connected),
    }
}

/// Whether this account's health can only come from asking the adapter.
///
/// True exactly for the helper providers: their poll succeeding is compatible
/// with an account that cannot send or receive a single message.
fn needs_probe_for_health(adapter: &dyn ChannelAdapter) -> bool {
    adapter.live_transport().is_none()
        && adapter.capabilities().inbound_transport
            == little_monkey_lib::channels::types::InboundTransport::Helper
}

/// Persist one probe's own answer, debounced on the state the way a
/// transition is.
///
/// Unlike [`record_health_transition`] this keeps the probe's *detail* — which
/// permission is missing, which number is not registered — because for these
/// providers that sentence is the only actionable part.
fn record_probe_health(
    store: &mut DaemonStore,
    posted: &mut BTreeMap<String, little_monkey_lib::channels::types::HealthState>,
    account_id: &str,
    health: &little_monkey_lib::channels::types::ChannelHealth,
) {
    if posted.get(account_id) == Some(&health.state) {
        return;
    }
    match store.set_channel_account_health(account_id, health, health.probed_at_ms) {
        Ok(()) => {
            posted.insert(account_id.to_string(), health.state);
        }
        Err(error) => eprintln!("monkey daemon: channel {account_id} health: {error}"),
    }
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

/// One account's inbound loop: poll, ingest, repeat until aborted.
///
/// Poll failures back off exponentially per account — a provider outage or a
/// rate-limited `getUpdates` must not turn into a hammer — and recover to full
/// speed on the first success. Health is derived from the transport after
/// every poll: a socket adapter answers for its own connection via
/// `live_transport`, a long-polling adapter's returned poll IS the proof, and
/// a failing poll is recorded as degraded with its reason.
async fn run_account_inbound(
    paths: super::store::DaemonPaths,
    queue: Arc<dyn RunQueue>,
    account_id: String,
    adapter: Arc<dyn ChannelAdapter>,
) {
    const MIN_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);
    const MAX_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);
    let mut error_backoff = MIN_ERROR_BACKOFF;
    // This task's one account, but the shared debounce helper wants a map.
    let mut posted_health: BTreeMap<String, little_monkey_lib::channels::types::HealthState> =
        BTreeMap::new();
    let mut last_asserted = std::time::Instant::now();
    // When this account was last asked what it can actually do — only used by
    // the providers whose poll cannot answer that. `None` means never, so the
    // first successful poll is followed by a real probe.
    let mut last_probed: Option<std::time::Instant> = None;
    loop {
        let Ok(now) = current_ms() else {
            tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
            continue;
        };
        let mut store = match DaemonStore::open(&paths) {
            Ok(store) => store,
            Err(error) => {
                eprintln!("monkey daemon: channel {account_id} paused: {error}");
                tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
                continue;
            }
        };
        // Periodically forget what was posted so the true state is re-asserted
        // even if something else rewrote the row in between.
        if last_asserted.elapsed() >= HEALTH_REASSERT_INTERVAL {
            last_asserted = std::time::Instant::now();
            posted_health.clear();
        }
        let started = std::time::Instant::now();
        match poll_account_once(
            &mut store,
            queue.as_ref(),
            &account_id,
            adapter.as_ref(),
            now,
        )
        .await
        {
            Ok(_) => {
                error_backoff = MIN_ERROR_BACKOFF;
                if let Some(state) = health_after_poll(adapter.as_ref()) {
                    record_health_transition(
                        &mut store,
                        &mut posted_health,
                        &account_id,
                        state,
                        None,
                        now,
                    );
                } else if needs_probe_for_health(adapter.as_ref())
                    && last_probed.is_none_or(|at: std::time::Instant| {
                        at.elapsed() >= HEALTH_REASSERT_INTERVAL
                    })
                {
                    // Paced rather than run every poll: for iMessage this
                    // sends Messages an Apple event, and a health check is not
                    // a reason to drive somebody's Messages.app in a loop.
                    last_probed = Some(std::time::Instant::now());
                    let health = adapter.probe().await;
                    record_probe_health(&mut store, &mut posted_health, &account_id, &health);
                }
                // A long-polling adapter paces this loop itself by blocking
                // inside `poll`. One that returns immediately forever — a
                // webhook adapter, or a broken one — must not busy-spin.
                if started.elapsed() < std::time::Duration::from_millis(500) {
                    tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
                }
            }
            Err(error) => {
                // Stored health saying Connected while every poll fails is
                // the lie the health column exists to prevent.
                record_health_transition(
                    &mut store,
                    &mut posted_health,
                    &account_id,
                    little_monkey_lib::channels::types::HealthState::Degraded,
                    Some(&error),
                    now,
                );
                eprintln!("monkey daemon: channel {account_id} poll: {error}");
                tokio::time::sleep(error_backoff).await;
                error_backoff = (error_backoff * 2).min(MAX_ERROR_BACKOFF);
            }
        }
    }
}

/// What actually feeds a live adapter: the transport-relevant parts of the
/// account row plus the resolved secret. Deliberately NOT the row's
/// updated-at stamp — an operator renaming an account or tightening its
/// access policy must not tear down a live gateway session, while rotating
/// the credential or editing the provider config must.
fn worker_fingerprint(
    account: &super::channel_store::ChannelAccountRecord,
    secret: &str,
) -> String {
    // A digest rather than the secret itself: the fingerprint is held for
    // the daemon's lifetime and compared every reload, and there is no
    // reason for a copy of the token to sit in it.
    super::trigger::sha256_hex(
        format!(
            "{}|{:?}|{}|{}",
            account.account_id,
            account.kind,
            super::trigger::sha256_hex(account.non_secret_config.to_string().as_bytes()),
            super::trigger::sha256_hex(secret.as_bytes()),
        )
        .as_bytes(),
    )
}

/// Bring the worker set in line with the enabled accounts, touching only what
/// changed.
///
/// The fingerprint covers what the adapter is built from — provider config
/// and the resolved secret — so rotating a token rebuilds that one adapter
/// while every other account keeps its live session, and editing a label
/// rebuilds nothing. Removing (or disabling) an account aborts its task and
/// drops its adapter, which is what tells a socket adapter's background task
/// to hang up. An account that cannot be built — unreadable credential,
/// config its adapter refuses — has that failure written to its stored
/// health rather than only to the daemon's log: the operator looking at the
/// Channels panel is the one who can fix it.
fn reconcile_workers(
    paths: &super::store::DaemonPaths,
    store: &mut DaemonStore,
    queue: &Arc<dyn RunQueue>,
    workers: &mut BTreeMap<String, AccountWorker>,
) {
    reconcile_workers_with(
        &super::channel_adapter::KeyringChannelSecrets,
        paths,
        store,
        queue,
        workers,
    );
}

/// [`reconcile_workers`] with the secret store injected, so the lifecycle —
/// spawn, keep, rebuild, stop — is provable against a store a test owns.
fn reconcile_workers_with(
    secrets: &dyn super::channel_adapter::ChannelSecrets,
    paths: &super::store::DaemonPaths,
    store: &mut DaemonStore,
    queue: &Arc<dyn RunQueue>,
    workers: &mut BTreeMap<String, AccountWorker>,
) {
    use super::channel_adapter::AdapterConfig;

    let Ok(now_ms) = current_ms() else {
        return;
    };
    let accounts = match store.channel_accounts() {
        Ok(accounts) => accounts,
        Err(error) => {
            eprintln!("monkey daemon: could not read channel accounts: {error}");
            return;
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
    let mut desired = std::collections::BTreeSet::new();
    for account in accounts.into_iter().filter(|account| account.enabled) {
        let is_sms = account.kind == little_monkey_lib::channels::types::ChannelKind::Sms;
        // An SMS account's carrier credential lives on the telephony account of
        // the same id, not on this row, so it is resolved from there.
        let secret = if is_sms {
            match store
                .telecom_account(&account.account_id)
                .ok()
                .flatten()
                .and_then(|telecom| telecom.credential_ref)
            {
                Some(reference) => match secrets.get(&reference) {
                    Ok(secret) => secret,
                    Err(error) => {
                        eprintln!(
                            "monkey daemon: SMS account {} cannot send: {error}",
                            account.account_id
                        );
                        mark_failed(store, &account, &error);
                        continue;
                    }
                },
                None => String::new(),
            }
        } else {
            match &account.credential_ref {
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
            }
        };
        let fingerprint = worker_fingerprint(&account, &secret);
        desired.insert(account.account_id.clone());
        if workers
            .get(&account.account_id)
            .map(|worker| worker.fingerprint.as_str())
            == Some(fingerprint.as_str())
        {
            continue;
        }
        let built = if is_sms {
            build_sms_adapter(paths, store, secrets, &account.account_id)
        } else {
            super::adapters::build_adapter(
                &AdapterConfig {
                    account: &account,
                    secret,
                },
                Some(paths),
            )
        };
        let adapter = match built {
            Ok(adapter) => adapter,
            Err(error) => {
                eprintln!(
                    "monkey daemon: channel account {} is not runnable: {error}",
                    account.account_id
                );
                mark_failed(store, &account, &error);
                // A broken replacement must not keep the stale adapter running
                // on the old credential.
                workers.remove(&account.account_id);
                continue;
            }
        };
        let handle = tokio::spawn(run_account_inbound(
            paths.clone(),
            queue.clone(),
            account.account_id.clone(),
            adapter.clone(),
        ));
        // Dropping the replaced worker aborts its task via `Drop`.
        workers.insert(
            account.account_id.clone(),
            AccountWorker {
                fingerprint,
                adapter,
                handle,
            },
        );
    }
    workers.retain(|account_id, _| desired.contains(account_id));
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
        transport: InboundTransport,
    }

    impl FakeAdapter {
        fn new() -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                outcomes: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
                live: None,
                transport: InboundTransport::LongPoll,
            }
        }

        fn with_live(state: little_monkey_lib::channels::types::HealthState) -> Self {
            Self {
                live: Some(state),
                ..Self::new()
            }
        }

        /// A delivered-to provider: `poll` is a local no-op that returns an
        /// empty batch without speaking to anyone.
        fn webhook() -> Self {
            Self {
                transport: InboundTransport::Webhook,
                ..Self::new()
            }
        }

        /// Signal and iMessage: `poll` reaches a helper process, which proves
        /// the process answered and nothing about whether the account behind
        /// it can send or receive.
        fn helper() -> Self {
            Self {
                transport: InboundTransport::Helper,
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
            ProviderCapabilities::minimal(ChannelKind::Telegram, self.transport)
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

    /// A message this daemon deliberately drops is finished the moment its
    /// decision is committed: nothing runs, and the provider may be told we
    /// have it. A challenge is the same, with its reply already queued.
    #[test]
    fn a_decision_never_to_run_is_still_durably_final_and_ack_safe() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.enabled = false;
        store.upsert_channel_account(&account).unwrap();

        let report = ingest_batch(&mut store, &queue, &[envelope("1")], NOW);

        assert_eq!(report.ignored, 1);
        assert_eq!(report.ack_safe, vec!["1".to_string()]);
        let events = store.recent_channel_events("acct-1", 10).unwrap();
        assert_eq!(events[0].disposition, EventDisposition::Ignored);
        assert!(events[0].ingress_id.is_none());
        assert!(queue.submitted.lock().unwrap().is_empty());
        // Nothing for a later pass to find: the decision is the whole story.
        assert!(store
            .accepted_events_awaiting_processing(10)
            .unwrap()
            .is_empty());
    }

    /// One envelope that could not be committed holds the ordinal cursor for
    /// the whole batch — a cursor cannot say "everything but that one" — and
    /// still must not take its siblings' per-message acknowledgements with it.
    #[tokio::test]
    async fn an_uncommitted_envelope_holds_the_cursor_but_not_its_siblings_ack() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let adapter = FakeAdapter::new();
        adapter.batches.lock().unwrap().push(InboundBatch {
            envelopes: vec![envelope("1"), envelope("2")],
            cursor: Some("42".to_string()),
        });

        // Armed once, so it fires on the first envelope and the second is
        // accepted normally.
        super::super::fail_points::arm(super::super::fail_points::FailPoint::AfterEventInsert);
        let report = poll_account_once(&mut store, &queue, "acct-1", &adapter, NOW)
            .await
            .expect("poll");

        assert_eq!(report.unrecorded, 1);
        assert_eq!(report.accepted, 1);
        assert_eq!(report.ack_safe, vec!["2".to_string()]);
        assert_eq!(
            store.channel_cursor("acct-1", POLL_CURSOR_KEY).unwrap(),
            None,
            "the cursor must not skip the envelope that was never accepted"
        );
        let events = store.recent_channel_events("acct-1", 10).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].provider_event_id, "2");
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
                invocation_id: None,
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
    async fn the_running_daemon_writes_health_from_the_transport_not_from_the_config() {
        use little_monkey_lib::channels::types::HealthState;
        let mut store = seeded_store();
        // A stored credential on its own proves nothing, so the account starts
        // out in the state a fresh configuration leaves it in.
        store
            .set_channel_account_health("acct-1", &ChannelHealth::error(NOW, "never probed"), NOW)
            .expect("seed health");

        // The same sequence the per-account loop performs after a poll.
        let adapter = FakeAdapter::new();
        let mut posted = BTreeMap::new();
        if let Some(state) = health_after_poll(&adapter) {
            record_health_transition(&mut store, &mut posted, "acct-1", state, None, NOW);
        }

        let account = store
            .channel_account("acct-1")
            .expect("read")
            .expect("account");
        assert_eq!(account.health.state, HealthState::Connected);
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

    // -- worker lifecycle ---------------------------------------------------

    /// A LINE account: webhook-delivered, so its spawned inbound task's poll
    /// is a local no-op — the one adapter whose worker can run in a test
    /// without a provider fixture behind it.
    fn line_account(updated_at_ms: i64, label: &str) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-line".into(),
            kind: ChannelKind::Line,
            label: label.into(),
            enabled: true,
            non_secret_config: serde_json::json!({}),
            credential_ref: Some("line-cred".into()),
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: NOW,
            updated_at_ms,
        }
    }

    fn line_secret() -> String {
        r#"{"channel_secret":"line-secret","channel_access_token":"line-token"}"#.to_string()
    }

    fn lifecycle_world() -> (
        DaemonStore,
        super::super::channel_adapter::MemoryChannelSecrets,
        super::super::store::DaemonPaths,
        Arc<dyn RunQueue>,
    ) {
        use super::super::channel_adapter::ChannelSecrets;
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .upsert_channel_account(&line_account(NOW, "Ops"))
            .expect("account");
        let secrets = super::super::channel_adapter::MemoryChannelSecrets::default();
        secrets.put("line-cred", &line_secret()).expect("secret");
        let paths = super::super::channel_restart_tests::temp_daemon_paths();
        let queue: Arc<dyn RunQueue> = Arc::new(FakeQueue::default());
        (store, secrets, paths, queue)
    }

    #[tokio::test]
    async fn a_label_edit_does_not_rebuild_a_live_worker() {
        let (mut store, secrets, paths, queue) = lifecycle_world();
        let mut workers = BTreeMap::new();
        reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
        assert_eq!(workers.len(), 1);
        let before = Arc::as_ptr(&workers.get("acct-line").unwrap().adapter);

        // A rename touches the row (and its updated-at stamp) but nothing the
        // transport is built from: the live session must survive it.
        store
            .upsert_channel_account(&line_account(NOW + 5_000, "Renamed"))
            .expect("rename");
        reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
        assert_eq!(workers.len(), 1);
        let after = Arc::as_ptr(&workers.get("acct-line").unwrap().adapter);
        assert!(
            std::ptr::eq(before, after),
            "a label edit must not tear down a live worker"
        );
    }

    #[tokio::test]
    async fn a_rotated_credential_replaces_the_worker_exactly_once() {
        use super::super::channel_adapter::ChannelSecrets;
        let (mut store, secrets, paths, queue) = lifecycle_world();
        let mut workers = BTreeMap::new();
        reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
        let before = Arc::as_ptr(&workers.get("acct-line").unwrap().adapter);

        secrets
            .put(
                "line-cred",
                r#"{"channel_secret":"rotated","channel_access_token":"rotated-token"}"#,
            )
            .expect("rotate");
        reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
        assert_eq!(workers.len(), 1, "one account, one worker");
        let after = Arc::as_ptr(&workers.get("acct-line").unwrap().adapter);
        assert!(
            !std::ptr::eq(before, after),
            "a rotated credential must rebuild the worker"
        );

        // Reconciling again with nothing changed keeps the rebuilt worker.
        reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
        let settled = Arc::as_ptr(&workers.get("acct-line").unwrap().adapter);
        assert!(std::ptr::eq(after, settled));
    }

    #[tokio::test]
    async fn rapid_enable_disable_cycles_leave_at_most_one_consumer() {
        let (mut store, secrets, paths, queue) = lifecycle_world();
        let mut workers = BTreeMap::new();
        for round in 0..5 {
            let mut account = line_account(NOW + round, "Ops");
            account.enabled = round % 2 == 0;
            store.upsert_channel_account(&account).expect("account");
            reconcile_workers_with(&secrets, &paths, &mut store, &queue, &mut workers);
            assert!(
                workers.len() <= 1,
                "round {round} left {} workers",
                workers.len()
            );
            assert_eq!(
                workers.contains_key("acct-line"),
                account.enabled,
                "round {round}: the worker set must mirror the enabled flag"
            );
        }
    }

    #[test]
    fn the_fingerprint_tracks_transport_inputs_and_nothing_else() {
        let account = line_account(NOW, "Ops");
        let base = worker_fingerprint(&account, "secret-1");

        let mut renamed = line_account(NOW + 99, "Renamed");
        renamed.access_policy = ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            group: AccessPolicy::Open,
            group_activation: GroupActivation::Always,
        };
        assert_eq!(
            worker_fingerprint(&renamed, "secret-1"),
            base,
            "labels, timestamps and access policy are not transport inputs"
        );

        assert_ne!(worker_fingerprint(&account, "secret-2"), base);
        let mut reconfigured = line_account(NOW, "Ops");
        reconfigured.non_secret_config = serde_json::json!({"base_url": "http://other"});
        assert_ne!(worker_fingerprint(&reconfigured, "secret-1"), base);
    }

    #[tokio::test]
    async fn skipped_claims_never_exhaust_the_attempt_budget() {
        // The account's adapter stays unloaded for far more drains than the
        // row has attempts. Each claim spends an attempt; each skip must hand
        // it back — otherwise a disabled account permanently fails replies
        // nothing ever tried to send.
        let mut store = seeded_store();
        queue_reply(&mut store, "reply-1");
        let mut now = NOW;
        for _ in 0..6 {
            let report = drain_outbox_once(&mut store, &BTreeMap::new(), now)
                .await
                .expect("drain");
            assert_eq!(report.skipped, 1, "row must stay claimable");
            assert_eq!(report.failed, 0);
            now += 61_000;
        }
        // The adapter comes back; the message still sends.
        let adapter = Arc::new(FakeAdapter::new());
        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), now)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_socket_adapter_answers_for_its_own_connection() {
        use little_monkey_lib::channels::types::HealthState;
        // A long-polling provider: the poll came back, so it is connected.
        assert_eq!(
            health_after_poll(&FakeAdapter::new()),
            Some(HealthState::Connected)
        );
        // A socket provider whose connection has dropped polls exactly the
        // same way — empty batch, no error — so recording Connected off the
        // back of that would be the lie the health column exists to prevent.
        for state in [
            HealthState::Connecting,
            HealthState::Degraded,
            HealthState::Error,
        ] {
            assert_eq!(
                health_after_poll(&FakeAdapter::with_live(state)),
                Some(state)
            );
        }
    }

    #[tokio::test]
    async fn a_webhook_adapters_no_op_poll_never_claims_connected() {
        use little_monkey_lib::channels::types::{ChannelHealth, HealthState};
        // A webhook adapter's poll succeeds without speaking to anyone, so a
        // successful tick must leave health exactly where it was.
        let adapter = FakeAdapter::webhook();
        assert_eq!(health_after_poll(&adapter), None);

        // End to end through the same sequence the runtime loop performs: the
        // poll succeeds, and a Disconnected account stays Disconnected.
        let mut store = seeded_store();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.health = ChannelHealth {
            state: HealthState::Disconnected,
            detail: None,
            last_error: None,
            probed_at_ms: NOW,
        };
        store.upsert_channel_account(&account).unwrap();

        let queue = FakeQueue::default();
        let mut posted = BTreeMap::new();
        poll_account_once(&mut store, &queue, "acct-1", &adapter, NOW)
            .await
            .expect("the no-op poll succeeds");
        if let Some(state) = health_after_poll(&adapter) {
            record_health_transition(&mut store, &mut posted, "acct-1", state, None, NOW);
        }
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.state, HealthState::Disconnected);

        // The same sequence with a long-polling adapter does move it: the
        // difference is the transport, not the emptiness of the batch.
        if let Some(state) = health_after_poll(&FakeAdapter::new()) {
            record_health_transition(&mut store, &mut posted, "acct-1", state, None, NOW);
        }
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.state, HealthState::Connected);
    }

    #[tokio::test]
    async fn a_helper_adapters_poll_never_claims_connected_on_its_own() {
        use little_monkey_lib::channels::types::{ChannelHealth, HealthState};
        // The false positive this exists to stop: signal-cli starts, its poll
        // comes back, and the account is reported Connected — while the number
        // it is configured for was never registered. The same shape for
        // iMessage, where a poll needs Full Disk Access and a *reply* needs a
        // permission a poll never touches.
        let adapter = FakeAdapter::helper();
        assert_eq!(health_after_poll(&adapter), None);
        assert!(needs_probe_for_health(&adapter));
        // A socket adapter answers for itself, so it is never probed for this.
        assert!(!needs_probe_for_health(&FakeAdapter::with_live(
            HealthState::Connected
        )));
        assert!(!needs_probe_for_health(&FakeAdapter::new()));

        let mut store = seeded_store();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.health = ChannelHealth {
            state: HealthState::Disconnected,
            detail: None,
            last_error: None,
            probed_at_ms: NOW,
        };
        store.upsert_channel_account(&account).unwrap();

        let queue = FakeQueue::default();
        let mut posted = BTreeMap::new();
        poll_account_once(&mut store, &queue, "acct-1", &adapter, NOW)
            .await
            .expect("the helper poll succeeds");
        if let Some(state) = health_after_poll(&adapter) {
            record_health_transition(&mut store, &mut posted, "acct-1", state, None, NOW);
        }
        assert_eq!(
            store
                .channel_account("acct-1")
                .unwrap()
                .unwrap()
                .health
                .state,
            HealthState::Disconnected,
            "a helper that answered is not an account that works"
        );

        // What the loop does instead: ask, and keep the answer's own reason.
        let probed = adapter.probe().await;
        record_probe_health(&mut store, &mut posted, "acct-1", &probed);
        let account = store.channel_account("acct-1").unwrap().unwrap();
        assert_eq!(account.health.state, probed.state);
        assert_eq!(account.health.detail, probed.detail);
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

    use super::super::channel_tool::{
        plan_send, queue_send, ChannelSendRequest, SendAuthority, SendInvocation,
    };
    use super::super::store::DaemonPaths;

    /// The durable identity of one tool invocation, as the agent loop passes
    /// it down: the daemon's job id plus the runtime-assigned tool-call id.
    fn invocation(job_id: &str, tool_call_id: &str) -> SendInvocation {
        SendInvocation {
            job_id: Some(job_id.to_string()),
            tool_call_id: Some(tool_call_id.to_string()),
        }
    }

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
            &invocation("job-1", "call-1"),
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
        // Keyed on the job and the tool-call id and nothing else, so a run
        // replaying this exact call recomputes the same key rather than a
        // second message.
        assert_eq!(sent[0].idempotency_key, "channel-send:job-1:call-1");

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
            &invocation("job-1", "call-1"),
            NOW,
        )
        .expect_err("disabled");
        assert!(error.contains("disabled"), "{error}");
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_artifact_over_the_account_configured_limit_never_reaches_the_outbox() {
        // The account, not a constant, is what bounds an outbound file: an
        // operator who set max_attachment_bytes low gets exactly that limit,
        // checked from durable metadata before any row is written.
        let mut store = seeded_store();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.non_secret_config = serde_json::json!({ "max_attachment_bytes": 4 });
        store.upsert_channel_account(&account).unwrap();

        let root = scratch_root();
        let paths = DaemonPaths::under(&root);
        let blobs = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
            root.join("content-v1"),
            16 * 1024 * 1024,
        )
        .expect("store");
        let blob = blobs.put(b"more than four bytes").expect("blob");

        let mut request = send_to("chat-9", "here is the file");
        request.artifact_ids = vec![blob.id.clone()];
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-1"),
            NOW,
        )
        .expect_err("over the account limit");
        assert!(error.contains("on this account"), "{error}");
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());

        // Raising the account's limit is all it takes for the same artifact.
        account.non_secret_config = serde_json::json!({ "max_attachment_bytes": 1024 });
        store.upsert_channel_account(&account).unwrap();
        queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-1"),
            NOW,
        )
        .expect("within the raised limit");
    }

    #[tokio::test]
    async fn more_artifacts_than_the_account_allows_are_refused_before_anything_is_resolved() {
        let mut store = seeded_store();
        let mut account = store.channel_account("acct-1").unwrap().unwrap();
        account.non_secret_config = serde_json::json!({ "max_listed_attachments": 1 });
        store.upsert_channel_account(&account).unwrap();

        let paths = scratch_paths();
        let mut request = send_to("chat-9", "two files");
        request.artifact_ids = vec!["a".repeat(64), "b".repeat(64)];
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-1"),
            NOW,
        )
        .expect_err("over the account's count limit");
        assert!(error.contains("at most 1"), "{error}");
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unknown_artifact_id_queues_nothing() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let mut request = send_to("chat-9", "the chart");
        request.artifact_ids = vec!["c".repeat(64)];
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-1"),
            NOW,
        )
        .expect_err("no such artifact");
        assert!(error.contains("no stored artifact"), "{error}");
        assert!(store.claim_outbox_batch(NOW, 10).unwrap().is_empty());
    }

    /// A file on its own is a message. The tool's schema stopped requiring
    /// text for exactly this: "here is the chart" is often the chart.
    #[tokio::test]
    async fn an_artifact_with_no_text_is_a_message_and_reaches_the_adapter() {
        let mut store = seeded_store();
        let root = scratch_root();
        let paths = DaemonPaths::under(&root);
        let blobs = little_monkey_lib::artifact_store::ArtifactStore::with_max_blob_size(
            root.join("content-v1"),
            16 * 1024 * 1024,
        )
        .expect("store");
        let blob = blobs.put(b"a rendered chart").expect("blob");

        let mut request = send_to("chat-9", "");
        request.artifact_ids = vec![blob.id.clone()];
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-a", "call-1"),
            NOW,
        )
        .expect("queued");

        let adapter = Arc::new(FakeAdapter::new());
        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent[0].text, "");
        assert_eq!(sent[0].attachments.len(), 1);
        assert_eq!(sent[0].attachments[0].artifact_id, blob.id);

        // Text alongside the same file is the other half of the contract.
        drop(sent);
        let mut with_text = request.clone();
        with_text.text = "the chart you asked for".to_string();
        let plan = plan_send(&with_text, &account_authority(), None).expect("authorized");
        queue_send(
            &mut store,
            &paths,
            &with_text,
            &plan,
            None,
            &invocation("job-b", "call-1"),
            NOW,
        )
        .expect("queued");
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 120_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[1].text, "the chart you asked for");
        assert_eq!(sent[1].attachments.len(), 1);
    }

    /// An origin reply inherits the thread and the message it answers, and an
    /// explicit destination inherits neither: a provider id from one
    /// conversation means nothing in another.
    #[tokio::test]
    async fn a_reply_carries_the_origin_thread_and_message_and_a_redirect_does_not() {
        use super::super::channel_store::ChannelOrigin;

        let mut store = seeded_store();
        let paths = scratch_paths();
        let origin = ChannelOrigin {
            account_id: "acct-1".to_string(),
            conversation_id: "chat-7".to_string(),
            thread_id: Some("thread-3".to_string()),
            provider_event_id: "msg-42".to_string(),
        };
        let reply_authority = SendAuthority {
            reply: true,
            cross_conversation: true,
            ..SendAuthority::default()
        };

        // Everything omitted: the destination is the origin, in full.
        let bare = ChannelSendRequest {
            text: "on it".to_string(),
            ..ChannelSendRequest::default()
        };
        let plan = plan_send(&bare, &reply_authority, Some(&origin)).expect("authorized");
        queue_send(
            &mut store,
            &paths,
            &bare,
            &plan,
            Some(&origin),
            &invocation("job-reply", "call-1"),
            NOW,
        )
        .expect("queued");

        // Another conversation on the same account: no borrowed thread, no
        // borrowed reply target.
        let elsewhere = send_to("chat-other", "heads up");
        let plan = plan_send(&elsewhere, &reply_authority, Some(&origin)).expect("authorized");
        queue_send(
            &mut store,
            &paths,
            &elsewhere,
            &plan,
            Some(&origin),
            &invocation("job-reply", "call-2"),
            NOW,
        )
        .expect("queued");

        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        let reply = sent
            .iter()
            .find(|message| message.conversation_id == "chat-7")
            .expect("the reply");
        assert_eq!(reply.thread_id.as_deref(), Some("thread-3"));
        assert_eq!(reply.reply_to_provider_id.as_deref(), Some("msg-42"));
        // Keyed on the invocation alone. Being an origin reply is what this
        // send *is*, not which invocation asked for it, so it leaves no mark
        // on the key; the redirect is a different tool call and gets its own.
        assert_eq!(reply.idempotency_key, "channel-send:job-reply:call-1");

        let redirected = sent
            .iter()
            .find(|message| message.conversation_id == "chat-other")
            .expect("the redirect");
        assert_eq!(redirected.thread_id, None);
        assert_eq!(redirected.reply_to_provider_id, None);
        // Two different sends from one job are two rows: the key separates
        // them by tool invocation, not by a counter.
        assert_ne!(redirected.idempotency_key, reply.idempotency_key);
    }

    /// The same run queueing the same message twice — a retried job replaying
    /// its tool calls — must not put a second message on the wire.
    #[tokio::test]
    async fn a_replayed_run_queues_one_message_not_two() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let request = send_to("chat-9", "the build passed");
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");

        let first = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-x", "call-4"),
            NOW,
        )
        .expect("first");
        assert_eq!(first["status"], "queued");

        // The replay carries the same invocation identity, so it recomputes
        // the same key and the first row is still the only one.
        let second = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-x", "call-4"),
            NOW + 5,
        )
        .expect("second");
        assert_eq!(second["status"], "already_queued");
        assert_eq!(second["outbox_id"], first["outbox_id"]);
        assert_eq!(store.outbox_count_for_job("job-x").unwrap(), 1);

        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);
    }

    /// "Reminder!" sent twice on purpose is two tool calls, and two tool
    /// calls are two messages — identical words are not a reason to swallow
    /// the second one. Only the invocation identity, never the content,
    /// decides whether a send is a duplicate.
    #[tokio::test]
    async fn two_intentional_identical_sends_from_one_run_are_two_deliveries() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let request = send_to("chat-9", "Reminder!");
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");

        let first = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-4"),
            NOW,
        )
        .expect("first call");
        let second = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &invocation("job-1", "call-7"),
            NOW + 5,
        )
        .expect("second call");
        assert_eq!(first["status"], "queued");
        assert_eq!(second["status"], "queued");
        assert_ne!(first["outbox_id"], second["outbox_id"]);
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 2);

        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].text, "Reminder!");
        assert_eq!(sent[1].text, "Reminder!");
        assert_ne!(sent[0].idempotency_key, sent[1].idempotency_key);
    }

    /// The same invocation identity arriving with different bytes is a
    /// consistency fault, not a retry: nothing overwrites the original row,
    /// nothing new is queued, and the error says so. The person still gets
    /// exactly the message the first enqueue made durable.
    #[tokio::test]
    async fn a_changed_payload_under_the_same_invocation_fails_closed() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let same_call = invocation("job-1", "call-4");

        let original = send_to("chat-9", "the build passed");
        let plan = plan_send(&original, &account_authority(), None).expect("authorized");
        let queued = queue_send(&mut store, &paths, &original, &plan, None, &same_call, NOW)
            .expect("queued");
        assert_eq!(queued["status"], "queued");

        let changed = send_to("chat-9", "the build FAILED");
        let plan = plan_send(&changed, &account_authority(), None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &changed,
            &plan,
            None,
            &same_call,
            NOW + 5,
        )
        .expect_err("a changed payload under the same invocation");
        assert!(error.contains("consistency"), "{error}");
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);

        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].text, "the build passed");
    }

    /// The invocation is the identity, so the account cannot be part of it.
    ///
    /// A replay that recomputes its destination onto another account is the
    /// same tool call asking for the same thing, and one tool call may become
    /// at most one outbound intent. Account-scoped uniqueness answered "has
    /// this account been told this?" and cheerfully queued a second message
    /// to a second person; the invocation index answers the question that
    /// matters, across every account.
    #[tokio::test]
    async fn the_same_invocation_replayed_onto_another_account_queues_nothing_new() {
        let mut store = seeded_store();
        store
            .upsert_channel_account(&ChannelAccountRecord {
                account_id: "acct-2".into(),
                kind: ChannelKind::Telegram,
                label: "Second".into(),
                enabled: true,
                non_secret_config: serde_json::json!({}),
                credential_ref: Some("channel:acct-2".into()),
                access_policy: ChannelAccessPolicy::default(),
                health: ChannelHealth::connected(NOW, None),
                created_at_ms: NOW,
                updated_at_ms: NOW,
            })
            .expect("second account");
        let paths = scratch_paths();
        let both = SendAuthority {
            accounts: vec!["acct-1".to_string(), "acct-2".to_string()],
            ..SendAuthority::default()
        };
        let same_call = invocation("job-1", "tool-1-2");

        let first = send_to("C1", "hello");
        let plan = plan_send(&first, &both, None).expect("authorized");
        let queued =
            queue_send(&mut store, &paths, &first, &plan, None, &same_call, NOW).expect("queued");
        assert_eq!(queued["status"], "queued");

        let elsewhere = ChannelSendRequest {
            account_id: Some("acct-2".to_string()),
            conversation_id: Some("C1".to_string()),
            text: "hello".to_string(),
            ..ChannelSendRequest::default()
        };
        let plan = plan_send(&elsewhere, &both, None).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &elsewhere,
            &plan,
            None,
            &same_call,
            NOW + 5,
        )
        .expect_err("the same invocation on another account");
        assert!(error.contains("consistency"), "{error}");
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);

        // And no second person hears from it.
        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].account_id, "acct-1");
    }

    /// Answering the origin and addressing a conversation explicitly are two
    /// things one invocation may be, not two invocations. The key carried a
    /// `reply-`/`send-` prefix, so a replay that reclassified itself became a
    /// second identity and a second message; the classification lives in the
    /// payload now, where a changed one is caught as changed intent.
    #[tokio::test]
    async fn the_same_invocation_replayed_as_an_explicit_send_queues_nothing_new() {
        use super::super::channel_store::ChannelOrigin;

        let mut store = seeded_store();
        let paths = scratch_paths();
        let origin = ChannelOrigin {
            account_id: "acct-1".to_string(),
            conversation_id: "chat-7".to_string(),
            thread_id: Some("thread-3".to_string()),
            provider_event_id: "msg-42".to_string(),
        };
        let authority = SendAuthority {
            reply: true,
            cross_conversation: true,
            ..SendAuthority::default()
        };
        let same_call = invocation("job-1", "tool-1-2");

        let bare = ChannelSendRequest {
            text: "on it".to_string(),
            ..ChannelSendRequest::default()
        };
        let plan = plan_send(&bare, &authority, Some(&origin)).expect("authorized");
        let queued = queue_send(
            &mut store,
            &paths,
            &bare,
            &plan,
            Some(&origin),
            &same_call,
            NOW,
        )
        .expect("queued");
        assert_eq!(queued["status"], "queued");

        let explicit = send_to("chat-other", "on it");
        let plan = plan_send(&explicit, &authority, Some(&origin)).expect("authorized");
        let error = queue_send(
            &mut store,
            &paths,
            &explicit,
            &plan,
            Some(&origin),
            &same_call,
            NOW + 5,
        )
        .expect_err("the same invocation reclassified as an explicit send");
        assert!(error.contains("consistency"), "{error}");
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);

        let adapter = Arc::new(FakeAdapter::new());
        drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 60_000)
            .await
            .expect("drain");
        let sent = adapter.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].conversation_id, "chat-7");
    }

    /// The destination is what the invocation asked to send, so changing it
    /// under the same identity is changed intent — caught by the digest, on
    /// the same account as anywhere else.
    #[test]
    fn the_same_invocation_replayed_to_another_conversation_queues_nothing_new() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let same_call = invocation("job-1", "tool-1-2");

        let first = send_to("C1", "hello");
        let plan = plan_send(&first, &account_authority(), None).expect("authorized");
        queue_send(&mut store, &paths, &first, &plan, None, &same_call, NOW).expect("queued");

        let moved = send_to("C2", "hello");
        let plan = plan_send(&moved, &account_authority(), None).expect("authorized");
        let error = queue_send(&mut store, &paths, &moved, &plan, None, &same_call, NOW + 5)
            .expect_err("the same invocation to another conversation");
        assert!(error.contains("consistency"), "{error}");
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);
    }

    /// The thread and the message being answered are part of the intent too:
    /// the same words in a different thread reach different people.
    #[test]
    fn the_same_invocation_replayed_into_another_thread_queues_nothing_new() {
        let mut store = seeded_store();
        let paths = scratch_paths();
        let same_call = invocation("job-1", "tool-1-2");

        let first = ChannelSendRequest {
            account_id: Some("acct-1".to_string()),
            conversation_id: Some("C1".to_string()),
            thread_id: Some("T1".to_string()),
            reply_to_provider_id: Some("msg-1".to_string()),
            text: "hello".to_string(),
            ..ChannelSendRequest::default()
        };
        let plan = plan_send(&first, &account_authority(), None).expect("authorized");
        queue_send(&mut store, &paths, &first, &plan, None, &same_call, NOW).expect("queued");

        for changed in [
            ChannelSendRequest {
                thread_id: Some("T2".to_string()),
                ..first.clone()
            },
            ChannelSendRequest {
                reply_to_provider_id: Some("msg-2".to_string()),
                ..first.clone()
            },
        ] {
            let plan = plan_send(&changed, &account_authority(), None).expect("authorized");
            let error = queue_send(
                &mut store,
                &paths,
                &changed,
                &plan,
                None,
                &same_call,
                NOW + 5,
            )
            .expect_err("the same invocation with a changed target");
            assert!(error.contains("consistency"), "{error}");
        }
        assert_eq!(store.outbox_count_for_job("job-1").unwrap(), 1);
    }

    /// A replay does not need the process that wrote the row: the invocation
    /// identity is recomputed against the reopened store and finds the
    /// existing row, exactly as it would in one process lifetime.
    #[tokio::test]
    async fn a_replayed_invocation_after_a_restart_finds_its_existing_row() {
        let paths = DaemonPaths::under(&scratch_root());
        let request = send_to("chat-9", "queued before the crash");
        let same_call = invocation("job-r", "call-2");

        let first_id = {
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
            let plan = plan_send(&request, &account_authority(), None).expect("authorized");
            let queued = queue_send(&mut store, &paths, &request, &plan, None, &same_call, NOW)
                .expect("queued");
            queued["outbox_id"].as_str().expect("id").to_string()
        }; // the daemon stops here

        let mut store = DaemonStore::open(&paths).expect("reopen");
        let plan = plan_send(&request, &account_authority(), None).expect("authorized");
        let replayed = queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &same_call,
            NOW + 60_000,
        )
        .expect("replay");
        assert_eq!(replayed["status"], "already_queued");
        assert_eq!(replayed["outbox_id"].as_str().unwrap(), first_id);
        assert_eq!(store.outbox_count_for_job("job-r").unwrap(), 1);

        let adapter = Arc::new(FakeAdapter::new());
        let report = drain_outbox_once(&mut store, &adapters_with(adapter.clone()), NOW + 120_000)
            .await
            .expect("drain");
        assert_eq!(report.sent, 1);
        assert_eq!(adapter.sent.lock().unwrap().len(), 1);
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
                &invocation("job-restart", "call-1"),
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

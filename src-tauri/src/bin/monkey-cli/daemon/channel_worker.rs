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

use little_monkey_lib::channels::ingress::ConversationIngress;
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
    /// Queues one accepted turn. Returns the daemon job id.
    ///
    fn submit(&self, ingress: &ConversationIngress, params: Vec<String>) -> Result<String, String>;
}

/// One fetched attachment, ready to be written where the run can read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InboundFile {
    /// Already sanitized: the sender chose the original name.
    pub name: String,
    pub bytes: Vec<u8>,
}

/// Directory, relative to the run's workspace, that a turn's attachments are
/// written into. Per job, so two conversations never see each other's files.
pub(crate) fn inbox_relative_dir(job_id: &str) -> String {
    format!(".little-monkey/inbox/{job_id}")
}

/// At most this many attachments are fetched per message.
const MAX_INBOUND_FILES: usize = 4;
/// Largest single attachment fetched, in bytes.
const MAX_INBOUND_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Largest total per message, so four maximum-size files cannot be used to
/// write 32 MiB into the workspace.
const MAX_INBOUND_TOTAL_BYTES: u64 = 16 * 1024 * 1024;

/// Fetch a turn's attachments, bounded in count and size.
///
/// Deliberately runs *after* the ingress gate, never before: fetching a
/// stranger's file before their message has been allowed would make an unpaired
/// sender able to spend this machine's bandwidth and disk.
///
/// A refusal is never fatal — the run still happens with the text, and the
/// skipped file is named in the note so the model does not answer as if it saw
/// something it did not.
async fn fetch_inbound_files(
    adapter: Option<&dyn ChannelAdapter>,
    ingress: &ConversationIngress,
) -> (Vec<InboundFile>, Vec<String>) {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    let Some(adapter) = adapter else {
        for attachment in &ingress.attachments {
            skipped.push(describe_skipped(
                attachment,
                "no adapter is available to fetch it",
            ));
        }
        return (files, skipped);
    };
    let mut total: u64 = 0;
    for (index, attachment) in ingress.attachments.iter().enumerate() {
        if files.len() >= MAX_INBOUND_FILES {
            skipped.push(describe_skipped(
                attachment,
                "only the first few attachments of a message are fetched",
            ));
            continue;
        }
        let remaining = MAX_INBOUND_TOTAL_BYTES.saturating_sub(total);
        let budget = MAX_INBOUND_FILE_BYTES.min(remaining);
        if budget == 0 {
            skipped.push(describe_skipped(
                attachment,
                "the message's size budget is used up",
            ));
            continue;
        }
        match adapter.fetch_attachment(attachment, budget).await {
            Ok(bytes) => {
                total += bytes.len() as u64;
                files.push(InboundFile {
                    name: safe_file_name(index, attachment.filename.as_deref()),
                    bytes,
                });
            }
            Err(error) => skipped.push(describe_skipped(attachment, &error)),
        }
    }
    (files, skipped)
}

fn describe_skipped(
    attachment: &little_monkey_lib::channels::types::ChannelAttachment,
    reason: &str,
) -> String {
    let name = attachment.filename.as_deref().unwrap_or("an attachment");
    format!("{name}: {reason}")
}

/// A filesystem-safe name for one attachment.
///
/// The sender picked the original, so it is not trusted with anything: the
/// index prefix makes collisions impossible, every character outside a small
/// allowlist becomes `_`, and the result is length-bounded. A name that
/// survives this cannot traverse, hide (`.`-prefixed), or collide.
fn safe_file_name(index: usize, filename: Option<&str>) -> String {
    let raw = filename.unwrap_or("attachment");
    let cleaned: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() {
        format!("{index}-attachment")
    } else {
        format!("{index}-{cleaned}")
    }
}

/// Append a note naming the files that were saved, and the ones that were not.
///
/// Written into the `message` parameter the planner already built, after the
/// untrusted-content wrapper rather than inside it: the paths are this
/// daemon's own words, and only the message text came from a stranger.
fn annotate_params_with_files(
    mut params: Vec<String>,
    relative_dir: &str,
    files: &[InboundFile],
    skipped: &[String],
) -> Vec<String> {
    if files.is_empty() && skipped.is_empty() {
        return params;
    }
    let mut note = String::new();
    if !files.is_empty() {
        let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
        note.push_str(&format!(
            "\n\n[Attachments saved in {relative_dir}/: {}]",
            names.join(", ")
        ));
    }
    if !skipped.is_empty() {
        note.push_str(&format!(
            "\n[Attachments that could not be fetched: {}]",
            skipped.join("; ")
        ));
    }
    for param in &mut params {
        if let Some(rest) = param.strip_prefix("message=") {
            *param = format!("message={rest}{note}");
            return params;
        }
    }
    params
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
/// `adapter` is what fetches an accepted message's attachments. `None` means
/// the caller has no adapter for this account — the turn still runs, and the
/// note tells the model the files were not retrieved.
pub(crate) async fn ingest_batch(
    store: &mut DaemonStore,
    queue: &dyn RunQueue,
    adapter: Option<&dyn ChannelAdapter>,
    envelopes: &[ChannelEnvelope],
    now_ms: i64,
) -> InboundReport {
    let mut report = InboundReport::default();
    for envelope in envelopes {
        match channel_ingress::plan_channel_ingress(store, envelope, now_ms) {
            Ok(IngressPlan { event_id, decision }) => match decision {
                PlannedDecision::Run { ingress, params } => {
                    // Files first: the parameters that name them are about to
                    // be persisted, and a recovery pass re-submits those
                    // parameters without re-fetching anything.
                    let (files, skipped) = if ingress.attachments.is_empty() {
                        (Vec::new(), Vec::new())
                    } else {
                        fetch_inbound_files(adapter, &ingress).await
                    };
                    let job_id_for_files = ingress.deterministic_job_id();
                    if !files.is_empty() {
                        // Best effort: a photo that could not be saved still
                        // gets its message answered, and the note says so.
                        let _ = super::write_inbound_files(
                            &ingress.target.recipe,
                            &job_id_for_files,
                            &files,
                        );
                    }
                    let params = annotate_params_with_files(
                        params,
                        &inbox_relative_dir(&job_id_for_files),
                        &files,
                        &skipped,
                    );
                    // Durable accept first, queue second. A crash in between
                    // leaves a row `recover_pending_ingress` finishes, rather
                    // than a message the provider considers delivered and the
                    // event log would refuse as a duplicate.
                    match channel_ingress::submit_ingress(store, queue, &ingress, &params, now_ms) {
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
    let batch = adapter.poll(cursor.as_deref()).await?;
    let report = ingest_batch(store, queue, Some(adapter), &batch.envelopes, now_ms).await;
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

        // Turns that were accepted but never queued are the other half of a
        // crash: the provider considers them delivered, so nobody else will
        // ever send them again.
        if let (Ok(mut store), Ok(now)) = (DaemonStore::open(&paths), current_ms()) {
            match channel_ingress::recover_pending_ingress(&mut store, queue.as_ref(), now) {
                Ok(recovery) if recovery.resubmitted + recovery.parked > 0 => eprintln!(
                    "monkey daemon: resumed {} accepted turn(s) after a restart, parked {}",
                    recovery.resubmitted, recovery.parked
                ),
                Ok(_) => {}
                Err(error) => eprintln!("monkey daemon: could not resume accepted turns: {error}"),
            }
        }

        let mut adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::new();
        let mut next_reload_ms = 0_u64;

        loop {
            let now = match current_ms() {
                Ok(now) => now,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(IDLE_TICK_MS)).await;
                    continue;
                }
            };
            let now_u64 = u64::try_from(now).unwrap_or(0);

            if now_u64 >= next_reload_ms {
                next_reload_ms = now_u64.saturating_add(RELOAD_INTERVAL_MS);
                match DaemonStore::open(&paths) {
                    Ok(store) => adapters = load_adapters(&store),
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
                        worked |= report.accepted + report.challenged + report.duplicates > 0
                    }
                    // One provider being unreachable must not stop the others,
                    // and must not stop the outbox either.
                    Err(error) => eprintln!("monkey daemon: channel {account_id} poll: {error}"),
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

/// Build an adapter for every enabled account, resolving each credential from
/// the keychain at load time so no adapter ever reads it itself.
fn load_adapters(store: &DaemonStore) -> BTreeMap<String, Arc<dyn ChannelAdapter>> {
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
    for account in accounts.into_iter().filter(|account| account.enabled) {
        // An SMS account's carrier credential lives on the telephony account of
        // the same id, not on this row, so it is built from there.
        if account.kind == little_monkey_lib::channels::types::ChannelKind::Sms {
            match build_sms_adapter(store, &secrets, &account.account_id) {
                Ok(adapter) => {
                    adapters.insert(account.account_id.clone(), adapter);
                }
                Err(error) => eprintln!(
                    "monkey daemon: SMS account {} cannot send: {error}",
                    account.account_id
                ),
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
            Err(error) => eprintln!(
                "monkey daemon: channel account {} is not runnable: {error}",
                account.account_id
            ),
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
    Ok(Arc::new(super::adapters::sms::SmsAdapter::new(
        &telecom, secret,
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
        last_params: Mutex<Vec<String>>,
        fail: bool,
    }

    impl RunQueue for FakeQueue {
        fn submit(
            &self,
            ingress: &ConversationIngress,
            params: Vec<String>,
        ) -> Result<String, String> {
            self.last_params.lock().unwrap().clone_from(&params);
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
    }

    impl FakeAdapter {
        fn new() -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                outcomes: Mutex::new(Vec::new()),
                sent: Mutex::new(Vec::new()),
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

        async fn fetch_attachment(
            &self,
            attachment: &little_monkey_lib::channels::types::ChannelAttachment,
            _max_bytes: u64,
        ) -> Result<Vec<u8>, String> {
            match attachment.filename.as_deref() {
                Some("gone.png") => Err("the provider deleted it".to_string()),
                _ => Ok(b"bytes".to_vec()),
            }
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

    fn ingress_with(
        attachments: Vec<little_monkey_lib::channels::types::ChannelAttachment>,
    ) -> ConversationIngress {
        let mut envelope = envelope("with-files");
        envelope.attachments = attachments;
        let route = ChannelRoute {
            route_id: "route-1".into(),
            scope: RouteScope::account("acct-1"),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        };
        ConversationIngress::from_channel(&envelope, &route)
    }

    fn attachment(filename: &str) -> little_monkey_lib::channels::types::ChannelAttachment {
        little_monkey_lib::channels::types::ChannelAttachment {
            provider_id: Some(filename.to_string()),
            kind: little_monkey_lib::channels::types::AttachmentKind::Image,
            filename: Some(filename.to_string()),
            mime_type: Some("image/png".into()),
            declared_size_bytes: Some(5),
            source: little_monkey_lib::channels::types::AttachmentSource::ProviderHandle {
                handle: filename.to_string(),
            },
        }
    }

    #[tokio::test]
    async fn attachments_are_fetched_bounded_and_never_keep_the_name_a_sender_chose() {
        let adapter = FakeAdapter::new();
        let ingress = ingress_with(vec![
            attachment("../../etc/passwd"),
            attachment("gone.png"),
            attachment("b.png"),
            attachment("c.png"),
            attachment("d.png"),
            attachment("e.png"),
        ]);

        let (files, skipped) = fetch_inbound_files(Some(&adapter), &ingress).await;

        assert_eq!(files.len(), MAX_INBOUND_FILES, "the per-message cap holds");
        assert_eq!(
            files[0].name, "0-_.._etc_passwd",
            "leading dots are stripped"
        );
        assert!(
            files.iter().all(|file| !file.name.contains('/')),
            "a sender-chosen name never survives as a path: {files:?}"
        );
        assert!(
            skipped.iter().any(|note| note.contains("gone.png")),
            "a file that could not be fetched is named, not silently dropped: {skipped:?}"
        );
    }

    #[test]
    fn the_run_is_told_where_its_files_are_and_what_is_missing() {
        let params = annotate_params_with_files(
            vec!["message=hello".to_string()],
            ".little-monkey/inbox/job-1",
            &[InboundFile {
                name: "0-photo.png".into(),
                bytes: vec![1, 2, 3],
            }],
            &["scan.pdf: too large".to_string()],
        );

        let message = &params[0];
        assert!(message.starts_with("message=hello"), "{message}");
        assert!(
            message.contains(".little-monkey/inbox/job-1/: 0-photo.png"),
            "{message}"
        );
        assert!(message.contains("scan.pdf: too large"), "{message}");
    }

    #[tokio::test]
    async fn a_turn_with_no_fetcher_still_runs_on_its_text() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let mut envelope = envelope("no-adapter");
        envelope.attachments = vec![attachment("gone.png")];

        let report = ingest_batch(&mut store, &queue, None, &[envelope], NOW).await;

        assert_eq!(report.accepted, 1, "the text still runs");
        let params = queue.last_params.lock().unwrap().clone();
        assert!(
            params
                .iter()
                .any(|param| param.contains("could not be fetched")),
            "the model is told the file is missing: {params:?}"
        );
    }

    #[tokio::test]
    async fn a_batch_becomes_runs_and_a_replay_of_it_does_not() {
        let mut store = seeded_store();
        let queue = FakeQueue::default();
        let batch = vec![envelope("1"), envelope("2")];

        let first = ingest_batch(&mut store, &queue, None, &batch, NOW).await;
        assert_eq!(first.accepted, 2);
        assert_eq!(first.duplicates, 0);

        let replay = ingest_batch(&mut store, &queue, None, &batch, NOW).await;
        assert_eq!(replay.accepted, 0);
        assert_eq!(replay.duplicates, 2);
        assert_eq!(queue.submitted.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_queue_failure_is_recorded_and_does_not_stop_the_batch() {
        let mut store = seeded_store();
        let queue = FakeQueue {
            fail: true,
            ..Default::default()
        };

        let report = ingest_batch(
            &mut store,
            &queue,
            None,
            &[envelope("1"), envelope("2")],
            NOW,
        )
        .await;
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
}

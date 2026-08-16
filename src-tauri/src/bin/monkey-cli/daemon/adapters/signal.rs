//! Signal adapter: speaks JSON-RPC to a user-installed `signal-cli` daemon
//! over its own stdio, using the `jsonRpc` sub-command's newline-delimited
//! JSON-RPC 2.0 convention (`signal-cli -a <account> --output=json jsonRpc`).
//!
//! Little Monkey never ships, bundles, or downloads signal-cli. The operator
//! installs and registers it themselves and points this adapter at the
//! resulting binary. There is no keychain credential for this provider —
//! the helper owns Signal's own encrypted local account store, and this
//! adapter never reads or copies it; `AdapterConfig::secret` is unused here.
//!
//! # Process shape
//!
//! [`SignalAdapter::ensure_started`] spawns the helper once, as an argument
//! vector via `tokio::process::Command` — never a shell string — and hands
//! its stdin/stdout to a background task ([`run_rpc_loop`]) that is the only
//! code in this module allowed to touch the child process directly:
//! - A line with an `id` is a JSON-RPC *response* to a request this adapter
//!   made (`send`, a probe's `listAccounts` call) — routed to the matching
//!   entry in `pending` and never seen by `poll`.
//! - A line with `"method": "receive"` and no `id` is an inbound
//!   *notification* — parsed by [`parse_event`] and offered to `poll`'s
//!   bounded channel without ever waiting on it. One stdout reader serves
//!   both directions, so blocking it on a downstream consumer would stop
//!   *RPC responses* from being read at all: a burst of inbound traffic
//!   would time out every `send` and probe behind it, and a helper that was
//!   answering perfectly would look dead. An overflow is therefore dropped,
//!   counted in [`Shared::dropped_inbound`], and surfaced as degraded
//!   health.
//! - EOF (the helper exited) resolves every still-pending request with a
//!   "helper exited" error, which is what turns an in-flight `send` into
//!   [`SendOutcome::NeedsReconciliation`] rather than leaving it hanging
//!   forever.
//!
//! [`parse_event`] is deliberately a pure `fn(&str) -> Option<ChannelEnvelope>`
//! so normalization is tested without spawning anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::Instant;

use crate::daemon::channel_adapter::{
    load_attachments, AdapterConfig, BlobSource, ChannelAdapter, DaemonBlobs, InboundBatch,
    LoadedAttachment, MAX_ATTACHMENT_BYTES,
};
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};

const INBOUND_CHANNEL_CAPACITY: usize = 256;
#[cfg(not(test))]
const RPC_TIMEOUT: Duration = Duration::from_secs(20);
/// The same budget, widened for tests. The fake helper is a `/bin/sh` script
/// competing with the rest of the suite for the machine, and no test asserts
/// that this deadline *fires* — a shorter one here only ever produces a
/// timeout that means "the box was busy".
#[cfg(test)]
const RPC_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait before spawning the helper again after the first failure.
///
/// A helper that dies on startup (unregistered account, broken install) would
/// otherwise be respawned once per `poll`, which is a process-spawn loop
/// against the operator's own machine.
const INITIAL_RESTART_COOLDOWN: Duration = Duration::from_secs(5);
/// The ceiling that cooldown backs off to. A helper that has been failing for
/// minutes is not going to be fixed by asking it again sooner, and an operator
/// who repairs their install waits at most this long for the next attempt.
const MAX_RESTART_COOLDOWN: Duration = Duration::from_secs(120);
/// A helper that ran at least this long counts as having worked, so the next
/// failure starts backing off from the beginning rather than inheriting the
/// previous outage's delay.
const HEALTHY_RUN: Duration = Duration::from_secs(60);
/// Signal has no server-enforced hard cap; this is signal-cli's own
/// practical ceiling before it starts truncating. Not a wire limit this
/// adapter has verified against every server version — ponytail: revisit if
/// signal-cli ever documents a different number.
const MAX_TEXT_CHARS: usize = 2000;

/// Outcome of one JSON-RPC round trip, distinguished by whether a request
/// provably reached the helper's stdin.
#[derive(Debug)]
enum CallError {
    /// Never written — the helper is not running or failed to start. Nothing
    /// happened, so this is the one arm a send may safely be retried from.
    NotSent(String),
    /// Written, but the outcome is unknown (write failed after some bytes
    /// may have gone out, the helper died before answering, or it never
    /// answered in time).
    Ambiguous(String),
    /// The helper answered with a JSON-RPC error.
    Remote(String),
}

impl CallError {
    fn into_message(self) -> String {
        match self {
            CallError::NotSent(message)
            | CallError::Ambiguous(message)
            | CallError::Remote(message) => message,
        }
    }
}

struct Shared {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
    stdin: Mutex<Option<ChildStdin>>,
    alive: AtomicBool,
    /// How long to wait before the next spawn attempt. Doubled by
    /// [`run_rpc_loop`] each time a helper dies quickly, reset once one has run
    /// for [`HEALTHY_RUN`] — so a broken install is retried at a decreasing
    /// rate while a one-off crash is recovered from immediately.
    restart_cooldown: Mutex<Duration>,
    /// Notifications normalized but never handed on, because `poll` fell far
    /// enough behind to fill the inbound queue.
    ///
    /// Counted rather than waited on: [`run_rpc_loop`] is the only reader of
    /// the helper's stdout, and stdout carries JSON-RPC *responses* down the
    /// same pipe. A reader parked on a full inbound queue stops resolving
    /// pending requests, which turns a working helper into timed-out sends
    /// and probes. Bounded and lossy beats unbounded and wedged — but a
    /// message that never arrived must still be something an operator can
    /// see, which is what [`SignalAdapter::probe`] reads this for.
    dropped_inbound: AtomicU64,
}

pub struct SignalAdapter {
    helper_path: String,
    account: String,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
    inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
    shared: Arc<Shared>,
    /// Serializes spawn attempts and remembers when the last one was made.
    /// `new` itself stays side-effect-free, same reasoning as the
    /// Discord/Mattermost adapters' own `started` field — but unlike a
    /// one-shot cell this allows a *re*start: signal-cli that crashes or is
    /// killed must not leave the account dead until the daemon restarts.
    last_start: Mutex<Option<Instant>>,
    blobs: Arc<dyn BlobSource>,
}

impl SignalAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let helper_path = config
            .account
            .non_secret_config
            .get("helper_path")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Signal account is missing helper_path".to_string())?
            .to_string();
        let account = config
            .account
            .non_secret_config
            .get("account")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "Signal account is missing the registered phone number".to_string())?
            .to_string();
        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
        Ok(Self {
            helper_path,
            account,
            inbound_tx,
            inbound_rx: Mutex::new(inbound_rx),
            shared: Arc::new(Shared {
                next_id: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                stdin: Mutex::new(None),
                alive: AtomicBool::new(false),
                restart_cooldown: Mutex::new(INITIAL_RESTART_COOLDOWN),
                dropped_inbound: AtomicU64::new(0),
            }),
            last_start: Mutex::new(None),
            blobs: Arc::new(DaemonBlobs),
        })
    }

    /// `Some(message)` when the configured path does not exist. Checked
    /// before every spawn attempt — including inside `ensure_started` — so
    /// a stale or never-installed path never reaches `Command::spawn`.
    fn helper_missing(&self) -> Option<String> {
        if std::path::Path::new(&self.helper_path).is_file() {
            None
        } else {
            Some("Install signal-cli and set its path in this account's settings".to_string())
        }
    }

    /// Start the helper, or restart it if a previous one exited.
    ///
    /// `alive` is the whole condition: `run_rpc_loop` clears it on EOF, so the
    /// next call here spawns a fresh helper rather than reporting forever that
    /// one "is not running". Attempts are rate-limited by [`RESTART_COOLDOWN`]
    /// and serialized by `last_start`, so concurrent callers produce one
    /// process, not one each.
    async fn ensure_started(&self) -> Result<(), String> {
        if self.shared.alive.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut last_start = self.last_start.lock().await;
        // Re-checked under the lock: whoever held it may have just started one.
        if self.shared.alive.load(Ordering::SeqCst) {
            return Ok(());
        }
        let cooldown = *self.shared.restart_cooldown.lock().await;
        if let Some(attempted_at) = *last_start {
            if attempted_at.elapsed() < cooldown {
                return Err(
                    "The signal-cli helper stopped; waiting before starting it again".to_string(),
                );
            }
        }
        *last_start = Some(Instant::now());
        if let Some(error) = self.helper_missing() {
            return Err(error);
        }
        let mut command = Command::new(&self.helper_path);
        command
            .args(["-a", &self.account, "--output=json", "jsonRpc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = command
            .spawn()
            .map_err(|error| format!("Failed to start the signal-cli helper: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "signal-cli helper has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "signal-cli helper has no stdout".to_string())?;
        *self.shared.stdin.lock().await = Some(stdin);
        self.shared.alive.store(true, Ordering::SeqCst);
        tokio::spawn(run_rpc_loop(
            child,
            stdout,
            self.shared.clone(),
            self.inbound_tx.clone(),
        ));
        Ok(())
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        self.ensure_started().await.map_err(CallError::NotSent)?;
        let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.shared.pending.lock().await.insert(id, tx);
        let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_vec(&request).map_err(|error| {
            self_cleanup_pending(&self.shared, id);
            CallError::NotSent(error.to_string())
        })?;
        line.push(b'\n');
        {
            let mut guard = self.shared.stdin.lock().await;
            let Some(stdin) = guard.as_mut() else {
                drop(guard);
                self.shared.pending.lock().await.remove(&id);
                return Err(CallError::NotSent(
                    "The signal-cli helper is not running".to_string(),
                ));
            };
            if let Err(error) = stdin.write_all(&line).await {
                drop(guard);
                self.shared.pending.lock().await.remove(&id);
                return Err(CallError::Ambiguous(format!(
                    "Write to the signal-cli helper failed: {error}"
                )));
            }
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(remote_error))) => Err(CallError::Remote(remote_error)),
            Ok(Err(_)) => Err(CallError::Ambiguous(
                "The signal-cli helper exited before answering".to_string(),
            )),
            Err(_) => Err(CallError::Ambiguous(
                "signal-cli did not answer in time".to_string(),
            )),
        }
    }
}

fn self_cleanup_pending(shared: &Arc<Shared>, id: u64) {
    // Best-effort: `try_lock` rather than `.await` because this only runs
    // from a `map_err` closure (sync context) on the rare path where
    // serializing the request itself fails.
    if let Ok(mut pending) = shared.pending.try_lock() {
        pending.remove(&id);
    }
}

async fn run_rpc_loop(
    mut child: Child,
    stdout: ChildStdout,
    shared: Arc<Shared>,
    inbound_tx: mpsc::Sender<ChannelEnvelope>,
) {
    let started_at = Instant::now();
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_u64) {
                    if let Some(sender) = shared.pending.lock().await.remove(&id) {
                        let result = match value.get("error") {
                            Some(error) => Err(error.to_string()),
                            None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = sender.send(result);
                    }
                    continue;
                }
                if let Some(envelope) = parse_event(&line) {
                    // `try_send`, never `send().await`: the next line down
                    // this pipe may be the JSON-RPC response an in-flight
                    // `send` is waiting on, and a reader parked on a full
                    // inbound queue would never reach it. Backpressure is
                    // kept — the queue stays bounded and the overflow is
                    // counted — it just stops being applied to the one task
                    // that must not absorb it.
                    match inbound_tx.try_send(envelope) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            shared.dropped_inbound.fetch_add(1, Ordering::Relaxed);
                        }
                        // The receiver lives in the adapter and is dropped
                        // only when the adapter itself is, so a closed
                        // channel means this helper has no owner left to
                        // deliver to. Stop reading and run the same shutdown
                        // the EOF arm does — the helper sees its stdin close
                        // and exits.
                        Err(mpsc::error::TrySendError::Closed(_)) => break,
                    }
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    shared.alive.store(false, Ordering::SeqCst);
    *shared.stdin.lock().await = None;
    // A helper that ran for a while and then died is a crash to recover from;
    // one that died immediately is an install that is not going to work yet,
    // and asking it again every five seconds forever helps nobody.
    {
        let mut cooldown = shared.restart_cooldown.lock().await;
        *cooldown = if started_at.elapsed() >= HEALTHY_RUN {
            INITIAL_RESTART_COOLDOWN
        } else {
            (*cooldown * 2).min(MAX_RESTART_COOLDOWN)
        };
    }
    // Dropped, not answered with an error: a request the helper never replied
    // to is *ambiguous*, not a provider rejection, and `call` distinguishes
    // the two by whether the sender was dropped. Sending `Err` here would
    // classify an in-flight send as a permanent failure and lose the message.
    shared.pending.lock().await.clear();
    let _ = child.wait().await;
}

/// Parses one newline-delimited JSON-RPC line from signal-cli's `receive`
/// notification stream into a normalized envelope. Returns `None` for
/// anything that is not an inbound-message notification (RPC responses,
/// other notification types, malformed JSON, an empty message with no
/// attachments) — `run_rpc_loop` handles the response case itself before
/// ever reaching here.
fn parse_event(line: &str) -> Option<ChannelEnvelope> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("method").and_then(Value::as_str) != Some("receive") {
        return None;
    }
    let envelope = value.get("params")?.get("envelope")?;
    let source = envelope.get("source").and_then(Value::as_str)?.to_string();
    let source_name = envelope
        .get("sourceName")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = envelope.get("timestamp").and_then(Value::as_i64)?;
    let data_message = envelope.get("dataMessage")?;
    let text = data_message
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let group_id = data_message
        .get("groupInfo")
        .and_then(|group| group.get("groupId"))
        .and_then(Value::as_str)
        .map(str::to_string);

    let attachments: Vec<ChannelAttachment> = data_message
        .get("attachments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("id").and_then(Value::as_str)?.to_string();
                    let content_type = item
                        .get("contentType")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let kind = content_type
                        .as_deref()
                        .map(AttachmentKind::from_mime)
                        .unwrap_or(AttachmentKind::Other);
                    Some(ChannelAttachment {
                        stored_artifact_id: None,
                        text_excerpt: None,
                        fetch_error: None,
                        provider_id: Some(id.clone()),
                        kind,
                        filename: item
                            .get("filename")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        mime_type: content_type,
                        declared_size_bytes: item.get("size").and_then(Value::as_u64),
                        stored_size_bytes: None,
                        source: AttachmentSource::ProviderHandle { handle: id },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let reply_to_provider_id = data_message
        .get("quote")
        .and_then(|quote| quote.get("id"))
        .and_then(Value::as_i64)
        .map(|id| id.to_string());

    let conversation = match &group_id {
        Some(group_id) => ChannelConversation::group(group_id.clone()),
        None => ChannelConversation::direct(source.clone()),
    };

    Some(ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::Signal,
        // Deterministic, never random: the durable-event dedupe key needs
        // the same line to always produce the same id.
        provider_event_id: format!("{source}:{timestamp}"),
        conversation,
        sender: ChannelSender {
            sender_id: source,
            display_label: source_name,
            // ponytail: always false — this parser only handles `receive`
            // notifications, and signal-cli reports the account's own
            // linked-device echoes as a separate `sentMessage` notification
            // this parser does not read yet. Add a syncMessage/sentMessage
            // branch here if multi-device echo suppression is needed.
            is_self: false,
            is_bot: false,
        },
        text,
        attachments,
        reply_to_provider_id,
        // ponytail: always false — `dataMessage.mentions[]` (UUID-keyed)
        // isn't checked because the account's own Signal UUID isn't
        // threaded through `non_secret_config` yet. Add when it is; group
        // gating falls back to allow-list authorization in the meantime.
        mentions_self: false,
        received_at_ms: timestamp,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

/// The registered numbers in a `listAccounts` result.
///
/// signal-cli has spelled this both as a bare array of objects and as one
/// wrapped in `accounts` across versions, and the number field as `number` or
/// `account`. All four are read rather than pinning one and reporting a working
/// install as broken; `None` means the shape was none of them.
fn registered_accounts(result: &Value) -> Option<Vec<String>> {
    let entries = result
        .as_array()
        .or_else(|| result.get("accounts").and_then(Value::as_array))?;
    Some(
        entries
            .iter()
            .filter_map(|entry| {
                entry
                    .as_str()
                    .or_else(|| entry.get("number").and_then(Value::as_str))
                    .or_else(|| entry.get("account").and_then(Value::as_str))
                    .map(str::to_string)
            })
            .collect(),
    )
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[async_trait]
impl ChannelAdapter for SignalAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Signal
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Signal,
            inbound_transport: InboundTransport::Helper,
            max_text_chars: MAX_TEXT_CHARS,
            supports_threads: false,
            supports_attachments: true,
            supports_mention_metadata: false,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        // Checked before any spawn attempt, per the module doc.
        if let Some(error) = self.helper_missing() {
            return ChannelHealth::unsupported(now, error);
        }
        // Registration, and nothing weaker. A helper process that starts fine
        // but has never registered this number cannot send or receive a single
        // message, so liveness — a `version` that answers, a process that is
        // running — is not evidence of anything this account needs. When
        // registration cannot be established, the honest answer is that setup
        // is unfinished, never Connected.
        match self.call("listAccounts", json!({})).await {
            Ok(result) => match registered_accounts(&result) {
                // Registration is established first and overflow is read only
                // inside this arm: an account that is not registered, or a
                // helper that cannot say, is a more fundamental failure than a
                // queue that overran, and must never be softened into
                // "connected but busy".
                Some(accounts) if accounts.iter().any(|number| number == &self.account) => {
                    match self.shared.dropped_inbound.load(Ordering::Relaxed) {
                        0 => ChannelHealth::connected(now, Some(self.account.clone())),
                        // No number, no message text, no helper path: an
                        // operator needs the count and the cause, and the
                        // count is not private.
                        dropped => ChannelHealth::degraded(
                            now,
                            format!(
                                "Signal is connected, but {dropped} inbound message(s) were \
                                 dropped because Little Monkey fell behind reading them.",
                            ),
                        ),
                    }
                }
                Some(_) => ChannelHealth::error(
                    now,
                    format!(
                        "signal-cli is running but {} is not registered with it. Register the \
                         number with signal-cli first.",
                        self.account
                    ),
                ),
                // The helper answered in a shape this adapter cannot read. It
                // may well be working — but "may well be" is not what Connected
                // means, and a silently unregistered account looks exactly like
                // this.
                None => ChannelHealth::unsupported(now, self.cannot_verify()),
            },
            // An older helper has no `listAccounts` at all. That is a helper
            // this adapter cannot verify an account with, which is a setup step
            // (update signal-cli), not a connection that is working.
            Err(CallError::Remote(_)) => ChannelHealth::unsupported(now, self.cannot_verify()),
            Err(error) => ChannelHealth::error(now, error.into_message()),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        self.ensure_started().await?;
        let mut rx = self.inbound_rx.lock().await;
        let mut envelopes = Vec::new();
        // Block for up to one bounded interval waiting for the first
        // notification, then drain whatever else has queued without
        // waiting further — the same "resume, don't spin" contract every
        // other adapter in this poll loop honors.
        match tokio::time::timeout(RPC_TIMEOUT, rx.recv()).await {
            Ok(Some(envelope)) => envelopes.push(envelope),
            Ok(None) => return Err("The signal-cli helper's inbound channel closed".to_string()),
            Err(_) => {}
        }
        while let Ok(envelope) = rx.try_recv() {
            envelopes.push(envelope);
        }
        Ok(InboundBatch {
            envelopes,
            cursor: None,
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        if !message.attachments.is_empty() {
            let files = match load_attachments(self.blobs.as_ref(), message) {
                Ok(files) => files,
                Err(outcome) => return outcome,
            };
            return self.send_with_attachments(message, &files).await;
        }
        // Signal's `OutboundMessage::conversation_id` carries either a
        // phone number (always `+`-prefixed, E.164) or a base64 group id —
        // never both, and the two alphabets never collide on the leading
        // byte — so that prefix is the entire "which JSON-RPC field" signal
        // this adapter needs.
        let mut params = json!({ "message": message.text });
        if message.conversation_id.starts_with('+') {
            params["recipient"] = json!([message.conversation_id]);
        } else {
            params["groupId"] = json!(message.conversation_id);
        }
        match self.call("send", params).await {
            Ok(result) => SendOutcome::Sent {
                provider_message_id: result
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .map(|timestamp| timestamp.to_string()),
            },
            // Never written to the helper's stdin — it is not running, or the
            // restart cooldown has not elapsed. Nothing happened, so this is
            // the one shape that is provably safe to send again.
            Err(CallError::NotSent(error)) => SendOutcome::RetryableFailure {
                error,
                retry_after_ms: Some(5_000),
            },
            Err(CallError::Ambiguous(error)) => SendOutcome::NeedsReconciliation { error },
            Err(CallError::Remote(error)) => SendOutcome::PermanentFailure { error },
        }
    }

    /// signal-cli keeps received attachments in its own store and hands them
    /// out by id through `getAttachment`, base64-encoded. Nothing here reads
    /// that store directly — the helper owns it, the same way it owns the
    /// account keys.
    ///
    /// A helper too old to know the method answers with a JSON-RPC error,
    /// which surfaces as a refusal rather than an empty file.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        // The account's own cap, never above this adapter's ceiling: a
        // configured limit that is ignored is a limit an operator set for
        // nothing.
        let max_bytes = limits.max_bytes.min(MAX_ATTACHMENT_BYTES);
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Signal attachment has no id.".to_string());
        };
        let result = self
            .call("getAttachment", json!({ "id": handle }))
            .await
            .map_err(|error| match error {
                CallError::NotSent(error)
                | CallError::Ambiguous(error)
                | CallError::Remote(error) => error,
            })?;
        // signal-cli has returned the payload both as a bare string and inside
        // a `data` field across versions; both are accepted rather than
        // pinning one and failing on the other.
        let encoded = result
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                result
                    .get("data")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .ok_or_else(|| "The helper returned no attachment data".to_string())?;
        // Base64 inflates by 4/3, so the encoded form is bounded first: this
        // refuses an oversized attachment before it is decoded into memory.
        if encoded.len() as u64 > max_bytes.saturating_mul(4).div_ceil(3) + 4 {
            return Err(format!("The attachment is larger than {max_bytes} bytes."));
        }
        let bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            encoded.as_bytes(),
        )
        .map_err(|_| "The helper returned unreadable attachment data".to_string())?;
        if bytes.len() as u64 > max_bytes {
            return Err(format!("The attachment is larger than {max_bytes} bytes."));
        }
        Ok(bytes)
    }
}

impl SignalAdapter {
    /// What health says when the helper is up but cannot be asked whether this
    /// account is registered.
    fn cannot_verify(&self) -> String {
        format!(
            "signal-cli is running, but this version cannot report which accounts are registered, \
             so Little Monkey cannot confirm that {} is able to send or receive. Update \
             signal-cli.",
            self.account
        )
    }

    /// signal-cli takes attachments on the same `send` call, as RFC 2397 data
    /// URIs (`data:<mime>;filename=<name>;base64,<data>`) rather than paths —
    /// which means nothing has to write the bytes to a temporary file that a
    /// crash could leave behind, and the helper never reads a path this daemon
    /// chose.
    async fn send_with_attachments(
        &self,
        message: &OutboundMessage,
        files: &[LoadedAttachment],
    ) -> SendOutcome {
        let encoded: Vec<String> = files
            .iter()
            .map(|file| {
                format!(
                    "data:{};filename={};base64,{}",
                    file.mime_type,
                    // A filename with a `;` or `,` would split the data URI
                    // itself, so the separators are the one thing removed.
                    file.filename.replace([';', ','], "_"),
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &file.bytes)
                )
            })
            .collect();
        let mut params = json!({ "message": message.text, "attachments": encoded });
        if message.conversation_id.starts_with('+') {
            params["recipient"] = json!([message.conversation_id]);
        } else {
            params["groupId"] = json!(message.conversation_id);
        }
        match self.call("send", params).await {
            Ok(result) => SendOutcome::Sent {
                provider_message_id: result
                    .get("timestamp")
                    .and_then(Value::as_i64)
                    .map(|timestamp| timestamp.to_string()),
            },
            // Never written to the helper's stdin — it is not running, or the
            // restart cooldown has not elapsed. Nothing happened, so this is
            // the one shape that is provably safe to send again.
            Err(CallError::NotSent(error)) => SendOutcome::RetryableFailure {
                error,
                retry_after_ms: Some(5_000),
            },
            Err(CallError::Ambiguous(error)) => SendOutcome::NeedsReconciliation { error },
            Err(CallError::Remote(error)) => SendOutcome::PermanentFailure { error },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DM_LINE: &str = r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{
        "source": "+15551230001",
        "sourceName": "Ada",
        "timestamp": 1700000000000,
        "dataMessage": {"message": "hello there"}
    }}}"#;

    const GROUP_LINE: &str = r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{
        "source": "+15551230002",
        "timestamp": 1700000001000,
        "dataMessage": {
            "message": "hi all",
            "groupInfo": {"groupId": "grp-abc="}
        }
    }}}"#;

    const ATTACHMENT_LINE: &str = r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{
        "source": "+15551230003",
        "timestamp": 1700000002000,
        "dataMessage": {
            "message": "look at this",
            "attachments": [{"id": "att-1", "contentType": "image/png", "filename": "cat.png", "size": 2048}]
        }
    }}}"#;

    const QUOTE_LINE: &str = r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{
        "source": "+15551230004",
        "timestamp": 1700000003000,
        "dataMessage": {"message": "agreed", "quote": {"id": 1699999999000}}
    }}}"#;

    const RESPONSE_LINE: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"timestamp":1700000004000}}"#;
    const EMPTY_LINE: &str = r#"{"jsonrpc":"2.0","method":"receive","params":{"envelope":{
        "source": "+15551230005",
        "timestamp": 1700000005000,
        "dataMessage": {"message": ""}
    }}}"#;

    #[test]
    fn every_shape_signal_cli_has_used_for_list_accounts_is_read() {
        assert_eq!(
            registered_accounts(&json!([{"number": "+15550000000"}])),
            Some(vec!["+15550000000".to_string()])
        );
        assert_eq!(
            registered_accounts(&json!({"accounts": [{"account": "+15550000000"}]})),
            Some(vec!["+15550000000".to_string()])
        );
        assert_eq!(
            registered_accounts(&json!(["+15550000000"])),
            Some(vec!["+15550000000".to_string()])
        );
        // A shape none of them use is unknown, not "no accounts" — reporting
        // a working install as unregistered is the worse mistake.
        assert_eq!(registered_accounts(&json!({"version": "0.13.0"})), None);
    }

    #[test]
    fn parses_a_direct_message() {
        let envelope = parse_event(DM_LINE).expect("envelope");
        assert_eq!(envelope.conversation.conversation_id, "+15551230001");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
        assert_eq!(envelope.text, "hello there");
        assert_eq!(envelope.sender.display_label.as_deref(), Some("Ada"));
    }

    #[test]
    fn parses_a_group_message_keyed_on_group_id() {
        let envelope = parse_event(GROUP_LINE).expect("envelope");
        assert_eq!(envelope.conversation.conversation_id, "grp-abc=");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
    }

    #[test]
    fn parses_an_attachment_as_a_provider_handle() {
        let envelope = parse_event(ATTACHMENT_LINE).expect("envelope");
        assert_eq!(envelope.attachments.len(), 1);
        let attachment = &envelope.attachments[0];
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.declared_size_bytes, Some(2048));
        match &attachment.source {
            AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "att-1"),
            other => panic!("expected a provider handle, got {other:?}"),
        }
    }

    #[test]
    fn carries_the_quoted_message_id_as_a_reply_target() {
        let envelope = parse_event(QUOTE_LINE).expect("envelope");
        assert_eq!(
            envelope.reply_to_provider_id.as_deref(),
            Some("1699999999000")
        );
    }

    #[test]
    fn ignores_rpc_responses_and_empty_messages() {
        assert!(parse_event(RESPONSE_LINE).is_none());
        assert!(parse_event(EMPTY_LINE).is_none());
        assert!(parse_event("not json").is_none());
    }

    #[test]
    fn the_same_line_always_produces_the_same_id() {
        let first = parse_event(DM_LINE).expect("envelope");
        let second = parse_event(DM_LINE).expect("envelope");
        assert_eq!(first.provider_event_id, second.provider_event_id);
        assert_eq!(first.provider_event_id, "+15551230001:1700000000000");
    }

    #[test]
    fn different_lines_produce_different_ids() {
        let dm = parse_event(DM_LINE).expect("envelope");
        let group = parse_event(GROUP_LINE).expect("envelope");
        assert_ne!(dm.provider_event_id, group.provider_event_id);
    }

    fn test_account(
        non_secret_config: Value,
    ) -> super::super::super::channel_store::ChannelAccountRecord {
        super::super::super::channel_store::ChannelAccountRecord {
            account_id: "acct-1".to_string(),
            kind: ChannelKind::Signal,
            label: "Signal".to_string(),
            enabled: true,
            non_secret_config,
            credential_ref: None,
            access_policy: Default::default(),
            health: ChannelHealth::error(0, "unused"),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[tokio::test]
    async fn a_missing_helper_path_yields_an_actionable_unsupported_health_never_a_panic() {
        let account = test_account(json!({
            "helper_path": "/definitely/not/a/real/path/signal-cli",
            "account": "+15550000000",
        }));
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        let adapter = SignalAdapter::new(&config).expect("adapter constructs");
        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Unsupported
        );
        let detail = health.detail.expect("detail");
        assert!(detail.contains("signal-cli"));
        // Nothing beyond the fixed instructional text and what the operator
        // typed for the helper path itself — no stdout, no stack trace.
        assert!(!detail.contains("/definitely/not/a/real/path/signal-cli"));
    }

    #[test]
    fn capabilities_report_the_declared_text_limit() {
        let account = test_account(json!({
            "helper_path": "/usr/local/bin/signal-cli",
            "account": "+15550000000",
        }));
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        let adapter = SignalAdapter::new(&config).expect("adapter");
        let capabilities = adapter.capabilities();
        assert_eq!(capabilities.max_text_chars, MAX_TEXT_CHARS);
        assert_eq!(capabilities.kind, ChannelKind::Signal);
    }

    #[test]
    fn rejects_missing_config() {
        let account = test_account(json!({}));
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(SignalAdapter::new(&config).is_err());
    }

    #[test]
    fn accepts_a_fully_configured_account_with_no_secret() {
        let account = test_account(json!({
            "helper_path": "/usr/local/bin/signal-cli",
            "account": "+15550000000",
        }));
        let config = AdapterConfig {
            account: &account,
            secret: String::new(),
        };
        assert!(SignalAdapter::new(&config).is_ok());
    }

    /// An opt-in round trip through the operator's own signal-cli.
    ///
    /// Never runs unless all three variables are set, so CI and every
    /// contributor's `cargo test` skip it. No number, no helper path and no
    /// recipient is bundled anywhere in this tree: the destination is one the
    /// person running it chose, and it is the only place a message is sent.
    ///
    /// ```text
    /// LM_SIGNAL_LIVE_HELPER=/usr/local/bin/signal-cli \
    /// LM_SIGNAL_LIVE_ACCOUNT=+15550000000 \
    /// LM_SIGNAL_LIVE_RECIPIENT=+15550000001 \
    ///   cargo test --bin monkey-cli a_live_signal_round_trip -- --nocapture
    /// ```
    #[tokio::test]
    async fn a_live_signal_round_trip() {
        let (Ok(helper), Ok(number), Ok(recipient)) = (
            std::env::var("LM_SIGNAL_LIVE_HELPER"),
            std::env::var("LM_SIGNAL_LIVE_ACCOUNT"),
            std::env::var("LM_SIGNAL_LIVE_RECIPIENT"),
        ) else {
            return;
        };
        let account = test_account(json!({
            "helper_path": helper,
            "account": number,
        }));
        let adapter = SignalAdapter::new(&AdapterConfig {
            account: &account,
            secret: String::new(),
        })
        .expect("adapter");

        let health = adapter.probe().await;
        assert_eq!(
            health.state,
            little_monkey_lib::channels::types::HealthState::Connected,
            "{health:?} — register this number with signal-cli first"
        );

        let marker = uuid::Uuid::new_v4().simple().to_string();
        let outcome = adapter
            .send(&OutboundMessage {
                account_id: "acct-1".to_string(),
                kind: ChannelKind::Signal,
                conversation_id: recipient,
                thread_id: None,
                text: format!("little-monkey live smoke test {marker}"),
                attachments: Vec::new(),
                reply_to_provider_id: None,
                idempotency_key: format!("live-{marker}"),
            })
            .await;
        assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");
    }

    /// Everything below drives a *fake* helper: a shell script that speaks the
    /// same newline-delimited JSON-RPC signal-cli does. No real signal-cli, no
    /// Signal account, no network — which is what makes the lifecycle
    /// (spawn, request/response, notification, malformed line, crash,
    /// restart) provable in CI at all.
    ///
    /// Unix only, because the fixture is a `#!/bin/sh` script. The adapter
    /// itself is not platform-specific; the fake is.
    #[cfg(unix)]
    mod fake_helper {
        use super::*;
        use little_monkey_lib::channels::types::HealthState;
        use std::os::unix::fs::PermissionsExt;

        /// Writes a fake signal-cli and returns its path.
        ///
        /// It emits one `receive` notification and one unparseable line
        /// immediately, then answers every request with a `version`-shaped
        /// result — except a `crash` request, which makes it exit like a
        /// helper that died mid-conversation.
        ///
        /// Each line goes out through `/bin/echo` rather than the shell's own
        /// builtin: a separate process writing and exiting cannot leave the
        /// line sitting in a shell's stdio buffer, which is the difference
        /// between this test being deterministic and being flaky.
        fn write_fake_helper(name: &str) -> std::path::PathBuf {
            write_fake_helper_registering(name, "+15550000000")
        }

        /// The same fixture, claiming a specific number is registered.
        ///
        /// Baked into the script rather than read from the environment: these
        /// tests share one process, and a process-wide variable would make the
        /// answer depend on which test ran last.
        fn write_fake_helper_registering(name: &str, registered: &str) -> std::path::PathBuf {
            write_fake_helper_registering_bursting(name, registered, 0)
        }

        /// The same, ahead of a burst of inbound notifications.
        fn write_fake_helper_registering_bursting(
            name: &str,
            registered: &str,
            burst: usize,
        ) -> std::path::PathBuf {
            write_fake_helper_bursting(
                name,
                &format!(r#"\"result\":[{{\"number\":\"{registered}\"}}]"#),
                burst,
            )
        }

        /// One valid `receive` notification per iteration, each from its own
        /// number so nothing downstream can collapse them into one event.
        const BURST_COMMAND: &str = r#"awk 'BEGIN{for(i=0;i<__COUNT__;i++)printf "{\"jsonrpc\":\"2.0\",\"method\":\"receive\",\"params\":{\"envelope\":{\"source\":\"+1555000%04d\",\"timestamp\":1700000000000,\"dataMessage\":{\"message\":\"burst\"}}}}\n", i}'"#;

        /// More notifications than the inbound queue holds, so the tail of the
        /// burst has nowhere to go while nothing is draining `poll`.
        const OVERFLOW_BURST: usize = INBOUND_CHANNEL_CAPACITY + 32;

        /// An adapter whose helper floods stdout before it will read a single
        /// request, and whose `poll` is never called.
        ///
        /// Every RPC response therefore arrives behind a full inbound queue,
        /// which is exactly the production shape: a Signal account under a
        /// burst while the consumer is busy.
        fn saturated_adapter(name: &str) -> (SignalAdapter, std::path::PathBuf) {
            let path = write_fake_helper_registering_bursting(name, "+15550000000", OVERFLOW_BURST);
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");
            (adapter, path)
        }

        /// The same fixture again, answering `listAccounts` with an arbitrary
        /// JSON-RPC tail — a result in a shape this adapter cannot read, or an
        /// outright error from a helper too old to have the method at all.
        fn write_fake_helper_answering(name: &str, list_accounts_tail: &str) -> std::path::PathBuf {
            write_fake_helper_bursting(name, list_accounts_tail, 0)
        }

        /// The same fixture once more, preceded by `burst` inbound
        /// notifications written before it reads a single request.
        ///
        /// One `awk` rather than `burst` separate `/bin/echo`s: the point is a
        /// crowd of lines arriving ahead of the RPC response, and spawning
        /// three hundred processes to produce them makes the test slow without
        /// making it stricter. It still exits before the read loop starts, so
        /// nothing is left sitting in a buffer.
        fn write_fake_helper_bursting(
            name: &str,
            list_accounts_tail: &str,
            burst: usize,
        ) -> std::path::PathBuf {
            let path = std::env::temp_dir().join(format!(
                "monkey-fake-signal-{name}-{}",
                uuid::Uuid::new_v4().simple()
            ));
            let burst_line = if burst == 0 {
                String::new()
            } else {
                BURST_COMMAND.replace("__COUNT__", &burst.to_string())
            };
            let script = r#"#!/bin/sh
/bin/echo '{"jsonrpc":"2.0","method":"receive","params":{"envelope":{"source":"+15551230001","sourceName":"Ada","timestamp":1700000000000,"dataMessage":{"message":"hello there"}}}}'
/bin/echo 'this line is not JSON'
__BURST__
while IFS= read -r line; do
  case "$line" in
    *crash*) exit 7 ;;
  esac
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *listAccounts*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,__LIST_ACCOUNTS__}" ;;
    *)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"version\":\"0.13.0\",\"timestamp\":1700000000001}}" ;;
  esac
done
"#;
            std::fs::write(
                &path,
                script
                    .replace("__LIST_ACCOUNTS__", list_accounts_tail)
                    .replace("__BURST__", &burst_line),
            )
            .expect("write fake helper");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod fake helper");
            path
        }

        fn helper_account(
            path: &std::path::Path,
        ) -> super::super::super::super::channel_store::ChannelAccountRecord {
            test_account(json!({
                "helper_path": path.to_string_lossy(),
                "account": "+15550000000",
            }))
        }

        #[tokio::test]
        async fn an_unregistered_number_is_a_real_error_not_a_connection() {
            let path = write_fake_helper_registering("unregistered", "+15559999999");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            let health = adapter.probe().await;
            // The helper is running perfectly. It just cannot reach Signal as
            // this number, which is not something to report as Connected.
            assert_eq!(health.state, HealthState::Error);
            let detail = health.last_error.expect("detail");
            assert!(detail.contains("not registered"), "{detail}");
            let _ = std::fs::remove_file(&path);
        }

        /// Registration cannot be established, so nothing here may be
        /// Connected — the failure class is a working *process* being mistaken
        /// for a working *account*.
        #[tokio::test]
        async fn a_helper_that_cannot_prove_registration_is_never_connected() {
            for (name, tail) in [
                // A `listAccounts` result in a shape this adapter cannot read.
                (
                    "unknown-shape",
                    r#"\"result\":{\"version\":\"0.13.0\"}"#.to_string(),
                ),
                // A helper old enough not to have the method at all.
                (
                    "no-method",
                    r#"\"error\":{\"code\":-32601,\"message\":\"Unknown method\"}"#.to_string(),
                ),
            ] {
                let path = write_fake_helper_answering(name, &tail);
                let account = helper_account(&path);
                let adapter = SignalAdapter::new(&AdapterConfig {
                    account: &account,
                    secret: String::new(),
                })
                .expect("adapter");

                let health = adapter.probe().await;
                assert_ne!(
                    health.state,
                    HealthState::Connected,
                    "{name}: a helper that answers is not an account that is registered"
                );
                assert_eq!(health.state, HealthState::Unsupported, "{name}");
                let detail = health.detail.unwrap_or_default();
                assert!(detail.contains("Update signal-cli"), "{name}: {detail}");
                let _ = std::fs::remove_file(&path);
            }
        }

        #[tokio::test]
        async fn a_send_the_helper_dies_during_is_reconciled_never_retried() {
            let path = write_fake_helper("ambiguous-send");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            // The fixture exits on any line containing `crash`, which is what
            // a helper killed mid-send looks like: written, never answered.
            let outcome = adapter
                .send(&OutboundMessage {
                    account_id: "acct-1".to_string(),
                    kind: ChannelKind::Signal,
                    conversation_id: "+15551230001".to_string(),
                    thread_id: None,
                    text: "crash".to_string(),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: "idem-crash".to_string(),
                })
                .await;
            assert!(
                matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
                "signal-cli may already have sent it: {outcome:?}"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn a_send_that_never_reached_the_helper_is_retryable() {
            let path = write_fake_helper("not-sent");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");
            // Kill the helper, then send inside the restart cooldown: the
            // request is never written, so nothing can have happened and the
            // message must stay queued rather than be dropped. The attempt
            // stamp is reset so the window starts now — otherwise a slow run
            // spends the whole cooldown before the send and gets a fresh
            // helper instead of the refusal being tested.
            let _ = adapter.call("crash", json!({})).await;
            *adapter.last_start.lock().await = Some(Instant::now());
            let outcome = adapter
                .send(&OutboundMessage {
                    account_id: "acct-1".to_string(),
                    kind: ChannelKind::Signal,
                    conversation_id: "+15551230001".to_string(),
                    thread_id: None,
                    text: "still queued".to_string(),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: "idem-2".to_string(),
                })
                .await;
            assert!(
                matches!(outcome, SendOutcome::RetryableFailure { .. }),
                "a message that never left must not be thrown away: {outcome:?}"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn a_helper_that_dies_at_once_is_retried_more_and_more_slowly() {
            let path = write_fake_helper("backoff");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            assert_eq!(
                *adapter.shared.restart_cooldown.lock().await,
                INITIAL_RESTART_COOLDOWN
            );
            for expected in [INITIAL_RESTART_COOLDOWN * 2, INITIAL_RESTART_COOLDOWN * 4] {
                // Clear the attempt stamp so the test does not sleep out the
                // cooldown it is asserting on.
                *adapter.last_start.lock().await = None;
                let _ = adapter.call("crash", json!({})).await;
                // The loop records the new cooldown as it exits.
                for _ in 0..100 {
                    if *adapter.shared.restart_cooldown.lock().await == expected {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(*adapter.shared.restart_cooldown.lock().await, expected);
            }
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn probes_over_the_helpers_own_json_rpc() {
            let path = write_fake_helper("probe");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            let health = adapter.probe().await;
            assert_eq!(health.state, HealthState::Connected);
            assert_eq!(health.detail.as_deref(), Some("+15550000000"));
            // The overflow counter must stay a report of something that
            // happened, not a state a healthy account drifts into: an account
            // nothing was dropped on is Connected, permanently.
            assert_eq!(adapter.shared.dropped_inbound.load(Ordering::Relaxed), 0);
            let _ = std::fs::remove_file(&path);
        }

        /// The one this whole mechanism exists for.
        ///
        /// One reader serves both directions of the helper's stdout, so a
        /// reader that waits for inbound-queue capacity stops reading JSON-RPC
        /// responses — and a helper answering perfectly starts timing out
        /// every send and probe behind the burst. Fails against a reader that
        /// awaits `inbound_tx.send`.
        #[tokio::test]
        async fn a_full_inbound_queue_never_blocks_an_rpc_response() {
            let (adapter, path) = saturated_adapter("saturated-rpc");
            // The burst is written before the fixture reads anything, so this
            // response sits behind all of it in the same pipe.
            let answered = tokio::time::timeout(
                Duration::from_secs(15),
                adapter.call("listAccounts", json!({})),
            )
            .await;
            assert!(
                matches!(answered, Ok(Ok(_))),
                "a full inbound queue starved the RPC reader: {answered:?}"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// The same starvation seen from the send path, where it costs more:
        /// an RPC that times out is not a failed send, it is a send nobody can
        /// classify, and it lands in reconciliation instead of being delivered.
        #[tokio::test]
        async fn a_full_inbound_queue_never_makes_a_send_ambiguous() {
            let (adapter, path) = saturated_adapter("saturated-send");
            let outcome = tokio::time::timeout(
                Duration::from_secs(15),
                adapter.send(&OutboundMessage {
                    account_id: "acct-1".to_string(),
                    kind: ChannelKind::Signal,
                    conversation_id: "+15551230001".to_string(),
                    thread_id: None,
                    text: "under load".to_string(),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: "idem-saturated".to_string(),
                }),
            )
            .await
            .expect("the helper answered, so the send must not have hung");
            match outcome {
                SendOutcome::Sent {
                    provider_message_id,
                } => assert_eq!(provider_message_id.as_deref(), Some("1700000000001")),
                other => panic!("inbound load must not make a send ambiguous: {other:?}"),
            }
            let _ = std::fs::remove_file(&path);
        }

        /// Dropping under load is the deliberate trade; dropping *silently* is
        /// not.
        #[tokio::test]
        async fn inbound_the_queue_could_not_take_is_counted() {
            let (adapter, path) = saturated_adapter("saturated-count");
            // The response arrives behind the whole burst, so by the time this
            // returns every line the queue refused has already been counted.
            adapter.probe().await;
            assert!(
                adapter.shared.dropped_inbound.load(Ordering::Relaxed) > 0,
                "the queue overran and nothing recorded it"
            );
            let _ = std::fs::remove_file(&path);
        }

        /// And the count has to reach the operator, because the messages did
        /// not reach the agent.
        #[tokio::test]
        async fn a_queue_that_overran_reports_degraded_with_the_count() {
            let (adapter, path) = saturated_adapter("saturated-health");
            let health = adapter.probe().await;
            assert_eq!(
                health.state,
                HealthState::Degraded,
                "messages were lost; this is not a clean connection"
            );
            let dropped = adapter.shared.dropped_inbound.load(Ordering::Relaxed);
            let detail = health.detail.expect("detail");
            assert!(detail.contains("dropped"), "{detail}");
            assert!(detail.contains(&dropped.to_string()), "{detail}");
            // A health string is read by whoever can see the account. Not the
            // number, not a word anybody sent, not where the helper lives.
            assert!(!detail.contains("+1555"), "{detail}");
            assert!(!detail.contains("burst"), "{detail}");
            assert!(
                !detail.contains(&path.to_string_lossy().to_string()),
                "{detail}"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn streams_inbound_notifications_and_ignores_unparseable_lines() {
            let path = write_fake_helper("inbound");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            let batch = adapter.poll(None).await.expect("poll");
            assert_eq!(batch.envelopes.len(), 1, "the junk line must not normalize");
            assert_eq!(batch.envelopes[0].text, "hello there");
            assert_eq!(
                batch.envelopes[0].provider_event_id,
                "+15551230001:1700000000000"
            );
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn a_send_the_helper_acknowledges_is_sent() {
            let path = write_fake_helper("send");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            let outcome = adapter
                .send(&OutboundMessage {
                    account_id: "acct-1".to_string(),
                    kind: ChannelKind::Signal,
                    conversation_id: "+15551230001".to_string(),
                    thread_id: None,
                    text: "ack".to_string(),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: "idem-1".to_string(),
                })
                .await;
            match outcome {
                SendOutcome::Sent {
                    provider_message_id,
                } => assert_eq!(provider_message_id.as_deref(), Some("1700000000001")),
                other => panic!("expected Sent, got {other:?}"),
            }
            let _ = std::fs::remove_file(&path);
        }

        #[tokio::test]
        async fn a_helper_that_exits_is_started_again_rather_than_left_dead() {
            let path = write_fake_helper("restart");
            let account = helper_account(&path);
            let adapter = SignalAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            assert_eq!(adapter.probe().await.state, HealthState::Connected);
            // The helper dies mid-request: the in-flight call must resolve as
            // ambiguous rather than hang, and the account must not be stuck.
            let died = adapter.call("crash", json!({})).await;
            assert!(matches!(died, Err(CallError::Ambiguous(_))));
            assert!(!adapter.shared.alive.load(Ordering::SeqCst));

            // Within the cooldown, the answer is a plain error — not a second
            // process spawned on every poll.
            assert_eq!(adapter.probe().await.state, HealthState::Error);

            // Once the cooldown has passed, the next call brings the helper
            // back. Simulated by clearing the attempt stamp so the test does
            // not sleep for it.
            *adapter.last_start.lock().await = None;
            assert_eq!(adapter.probe().await.state, HealthState::Connected);
            let _ = std::fs::remove_file(&path);
        }
    }
}

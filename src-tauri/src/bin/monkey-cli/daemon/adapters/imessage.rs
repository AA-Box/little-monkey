//! iMessage adapter: drives `little-monkey-imessage-helper`, and nothing else.
//!
//! The daemon does not read `~/Library/Messages/chat.db`, does not query the
//! Messages database, and does not send Apple events. Full Disk Access and
//! Automation for Messages.app belong to the helper process the operator
//! installs and grants once — see `src/imessage_helper/main.rs` for the other
//! side of this conversation. What lives here is the rest of a channel: finding
//! the helper, keeping it running, speaking its protocol, and normalizing what
//! it reports into [`ChannelEnvelope`].
//!
//! No Apple ID and no password is ever asked for. The account is whichever one
//! the user is already signed in to in Messages.
//!
//! # Resume
//!
//! The cursor is the Messages database's own `message.ROWID`, and it is held
//! **here**, in the account's channel cursor, not in the helper. A helper that
//! crashes and restarts is handed the same cursor again, so nothing is replayed
//! and nothing between the last commit and the crash is skipped. Dedupe
//! downstream keys on Messages' own GUID, which makes a re-delivered row
//! harmless.
//!
//! An account with no cursor yet is told so, and the helper answers with where
//! the database is *now* and no messages: connecting an account is not a reason
//! to replay every conversation on the Mac through an agent.
//!
//! # Platforms
//!
//! The real implementation is `#[cfg(target_os = "macos")]`. Every other
//! platform gets [`other::ImessageAdapter`], which reports
//! [`ChannelHealth::unsupported`] without touching the filesystem or spawning
//! anything — there is no Messages database anywhere else to talk to.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};
    use tokio::sync::{oneshot, Mutex};
    use tokio::time::Instant;

    use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};
    use little_monkey_lib::channels::types::{
        AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
        ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
        ProviderCapabilities, SendOutcome,
    };

    const RPC_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long to wait before the first restart after the helper exits.
    const INITIAL_RESTART_COOLDOWN: Duration = Duration::from_secs(5);
    /// The ceiling that cooldown backs off to. A helper that has been failing
    /// for minutes will not be fixed by asking it again sooner.
    const MAX_RESTART_COOLDOWN: Duration = Duration::from_secs(120);
    /// A helper that ran at least this long counts as having worked, so the
    /// next failure backs off from the beginning.
    const HEALTHY_RUN: Duration = Duration::from_secs(60);
    /// Messages.app splits long iMessages client-side; there is no
    /// server-enforced cap this adapter can query. A conservative budget.
    const MAX_TEXT_CHARS: usize = 20_000;
    /// Rows the helper may return in one poll. A bound, not a page size: the
    /// cursor advances by what was read, so a backlog drains over several
    /// polls instead of arriving as one unbounded batch.
    const MAX_ROWS_PER_POLL: u64 = 100;

    /// The helper's own error codes. Kept in step with
    /// `src/imessage_helper/main.rs`, which is the one other place they appear.
    const CODE_AMBIGUOUS: i64 = -32001;
    const CODE_SETUP: i64 = -32002;

    /// Outcome of one JSON-RPC round trip, distinguished by whether a command
    /// provably reached the helper's stdin.
    #[derive(Debug)]
    enum CallError {
        /// Never written — the helper is not running or failed to start. Safe
        /// to report as permanent: nothing happened.
        NotSent(String),
        /// Written, but the outcome is unknown (the write failed after some
        /// bytes may have gone out, the helper died before answering, or it
        /// never answered in time).
        Ambiguous(String),
        /// The helper answered with a JSON-RPC error, carrying its own code.
        Remote { code: i64, message: String },
    }

    impl CallError {
        fn into_message(self) -> String {
            match self {
                CallError::NotSent(message)
                | CallError::Ambiguous(message)
                | CallError::Remote { message, .. } => message,
            }
        }
    }

    struct Shared {
        next_id: AtomicU64,
        pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, Value>>>>,
        stdin: Mutex<Option<ChildStdin>>,
        alive: AtomicBool,
        /// How long to wait before the next spawn attempt. Doubled by
        /// [`run_rpc_loop`] each time a helper dies quickly, reset once one has
        /// run for [`HEALTHY_RUN`] — so a helper that is missing a permission
        /// is retried at a decreasing rate while a one-off crash recovers at
        /// once.
        restart_cooldown: Mutex<Duration>,
    }

    pub struct ImessageAdapter {
        helper_path: String,
        handle: String,
        shared: Arc<Shared>,
        /// Serializes spawn attempts and remembers when the last one was made.
        /// Not a one-shot cell: a helper the user quits has to be startable
        /// again without restarting the daemon.
        last_start: Mutex<Option<Instant>>,
    }

    impl ImessageAdapter {
        pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
            let helper_path = config
                .account
                .non_secret_config
                .get("helper_path")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    "iMessage needs the path to little-monkey-imessage-helper".to_string()
                })?
                .to_string();
            let handle = config
                .account
                .non_secret_config
                .get("handle")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "iMessage account is missing handle".to_string())?
                .to_string();
            Ok(Self {
                helper_path,
                handle,
                shared: Arc::new(Shared {
                    next_id: AtomicU64::new(1),
                    pending: Mutex::new(HashMap::new()),
                    stdin: Mutex::new(None),
                    alive: AtomicBool::new(false),
                    restart_cooldown: Mutex::new(INITIAL_RESTART_COOLDOWN),
                }),
                last_start: Mutex::new(None),
            })
        }

        /// `Some(message)` when the configured path does not exist. Checked
        /// before every spawn attempt so a stale or never-installed path never
        /// reaches `Command::spawn`.
        ///
        /// The path itself is deliberately not repeated back: it came from the
        /// operator, and health is shown in places a path does not belong.
        fn helper_missing(&self) -> Option<String> {
            if std::path::Path::new(&self.helper_path).is_file() {
                None
            } else {
                Some(
                    "Install little-monkey-imessage-helper and set its path in this account's \
                     settings"
                        .to_string(),
                )
            }
        }

        /// Start the helper, or restart it if a previous one exited.
        async fn ensure_started(&self) -> Result<(), String> {
            if self.shared.alive.load(Ordering::SeqCst) {
                return Ok(());
            }
            let mut last_start = self.last_start.lock().await;
            // Re-checked under the lock: whoever held it may have just started
            // one.
            if self.shared.alive.load(Ordering::SeqCst) {
                return Ok(());
            }
            let cooldown = *self.shared.restart_cooldown.lock().await;
            if let Some(attempted_at) = *last_start {
                if attempted_at.elapsed() < cooldown {
                    return Err(
                        "The iMessage helper stopped; waiting before starting it again".to_string(),
                    );
                }
            }
            *last_start = Some(Instant::now());
            if let Some(error) = self.helper_missing() {
                return Err(error);
            }
            // An argument vector, never a shell string: the handle is the
            // operator's own configuration, but nothing about this call should
            // depend on that being true.
            let mut command = Command::new(&self.helper_path);
            command
                .args(["--handle", &self.handle])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let mut child = command
                .spawn()
                .map_err(|error| format!("Failed to start the iMessage helper: {error}"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| "The iMessage helper has no stdin".to_string())?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| "The iMessage helper has no stdout".to_string())?;
            *self.shared.stdin.lock().await = Some(stdin);
            self.shared.alive.store(true, Ordering::SeqCst);
            tokio::spawn(run_rpc_loop(child, stdout, self.shared.clone()));
            Ok(())
        }

        async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
            self.ensure_started().await.map_err(CallError::NotSent)?;
            let id = self.shared.next_id.fetch_add(1, Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            self.shared.pending.lock().await.insert(id, tx);
            let request = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let mut line = match serde_json::to_vec(&request) {
                Ok(line) => line,
                Err(error) => {
                    self.shared.pending.lock().await.remove(&id);
                    return Err(CallError::NotSent(error.to_string()));
                }
            };
            line.push(b'\n');
            {
                let mut guard = self.shared.stdin.lock().await;
                let Some(stdin) = guard.as_mut() else {
                    drop(guard);
                    self.shared.pending.lock().await.remove(&id);
                    return Err(CallError::NotSent(
                        "The iMessage helper is not running".to_string(),
                    ));
                };
                if let Err(error) = stdin.write_all(&line).await {
                    drop(guard);
                    self.shared.pending.lock().await.remove(&id);
                    return Err(CallError::Ambiguous(format!(
                        "Write to the iMessage helper failed: {error}"
                    )));
                }
            }
            match tokio::time::timeout(RPC_TIMEOUT, rx).await {
                Ok(Ok(Ok(result))) => Ok(result),
                Ok(Ok(Err(error))) => Err(CallError::Remote {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("The iMessage helper refused the request")
                        .to_string(),
                }),
                Ok(Err(_)) => Err(CallError::Ambiguous(
                    "The iMessage helper exited before answering".to_string(),
                )),
                Err(_) => Err(CallError::Ambiguous(
                    "The iMessage helper did not answer in time".to_string(),
                )),
            }
        }
    }

    /// Route the helper's answers back to whoever asked.
    ///
    /// Every line the helper writes is a response to a request this adapter
    /// made — the protocol has no notifications — so an unmatched id is simply
    /// dropped rather than guessed at.
    async fn run_rpc_loop(mut child: Child, stdout: ChildStdout, shared: Arc<Shared>) {
        let started_at = Instant::now();
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(Value::as_u64) else {
                continue;
            };
            if let Some(sender) = shared.pending.lock().await.remove(&id) {
                let result = match value.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
                };
                let _ = sender.send(result);
            }
        }
        shared.alive.store(false, Ordering::SeqCst);
        *shared.stdin.lock().await = None;
        // A helper that ran for a while and then died is a crash to recover
        // from; one that died immediately is an install that is not going to
        // work yet, and asking it again every five seconds forever helps
        // nobody.
        {
            let mut cooldown = shared.restart_cooldown.lock().await;
            *cooldown = if started_at.elapsed() >= HEALTHY_RUN {
                INITIAL_RESTART_COOLDOWN
            } else {
                (*cooldown * 2).min(MAX_RESTART_COOLDOWN)
            };
        }
        // Dropped rather than answered with an error: a request the helper
        // never replied to is *ambiguous* — it may have been acted on — and
        // only a dropped sender reaches `call`'s ambiguous arm.
        shared.pending.lock().await.clear();
        let _ = child.wait().await;
    }

    /// One record from the helper's `poll` as a normalized envelope.
    ///
    /// Pure — no process, no clock — so the tests drive it directly. Returns
    /// `None` for a record with neither text nor a file, which is Messages'
    /// own bookkeeping rather than a turn.
    fn normalize_record(record: &Value) -> Option<ChannelEnvelope> {
        let guid = record.get("guid").and_then(Value::as_str)?.to_string();
        let sender = record.get("sender").and_then(Value::as_str)?.to_string();
        let timestamp = record.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
        let text = record
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let chat_id = record
            .get("chatId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_group = record
            .get("isGroup")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let attachments: Vec<ChannelAttachment> = record
            .get("attachments")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let path = item.get("path").and_then(Value::as_str)?.to_string();
                        let mime_type = item
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        let kind = mime_type
                            .as_deref()
                            .map(AttachmentKind::from_mime)
                            .unwrap_or(AttachmentKind::Other);
                        Some(ChannelAttachment {
                            stored_artifact_id: None,
                            text_excerpt: None,
                            fetch_error: None,
                            provider_id: None,
                            kind,
                            filename: item
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            mime_type,
                            declared_size_bytes: item.get("size").and_then(Value::as_u64),
                            // The path is a handle the *helper* resolves. This
                            // process never opens it — that is the whole point
                            // of the helper holding Full Disk Access.
                            source: AttachmentSource::ProviderHandle { handle: path },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if text.is_empty() && attachments.is_empty() {
            return None;
        }

        let conversation = match (is_group, &chat_id) {
            (true, Some(chat_id)) => ChannelConversation::group(chat_id.clone()),
            _ => ChannelConversation::direct(sender.clone()),
        };

        Some(ChannelEnvelope {
            account_id: String::new(),
            kind: ChannelKind::IMessage,
            // Messages' own stable identifier, which is what dedupe wants —
            // and what makes a row re-delivered after a helper crash harmless.
            provider_event_id: guid,
            conversation,
            sender: ChannelSender {
                sender_id: sender,
                display_label: record
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                // The helper never reports our own outbound messages: an agent
                // answering its own send is a loop, and the filter belongs
                // where the `is_from_me` column is.
                is_self: false,
                is_bot: false,
            },
            text,
            attachments,
            reply_to_provider_id: record
                .get("replyToGuid")
                .and_then(Value::as_str)
                .map(str::to_string),
            // Messages has no mention metadata of its own. Group activation
            // falls back to the gate's own text matching rather than claiming a
            // signal that does not exist.
            mentions_self: false,
            received_at_ms: timestamp,
            metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
        })
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }

    #[async_trait]
    impl ChannelAdapter for ImessageAdapter {
        fn kind(&self) -> ChannelKind {
            ChannelKind::IMessage
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                kind: ChannelKind::IMessage,
                inbound_transport: InboundTransport::Helper,
                max_text_chars: MAX_TEXT_CHARS,
                supports_threads: false,
                // Inbound only: the helper hands over files it was sent, and
                // nothing here uploads one.
                supports_attachments: false,
                supports_mention_metadata: false,
                supports_idempotency_key: false,
                supports_delivery_receipts: false,
            }
        }

        async fn probe(&self) -> ChannelHealth {
            let now = now_ms();
            if let Some(error) = self.helper_missing() {
                return ChannelHealth::unsupported(now, error);
            }
            match self.call("probe", json!({})).await {
                Ok(result) => {
                    let handles = result.get("handles").and_then(Value::as_u64).unwrap_or(0);
                    let can_send = result
                        .get("canSend")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !can_send {
                        return ChannelHealth::degraded(
                            now,
                            format!(
                                "{} · reading Messages works, but sending does not — grant \
                                 Automation access for Messages.app to the helper",
                                self.handle
                            ),
                        );
                    }
                    ChannelHealth::connected(
                        now,
                        Some(format!("{} · {handles} known handles", self.handle)),
                    )
                }
                // A missing permission is not a connection that failed, it is a
                // setup step nobody has done — and retrying it forever would
                // never fix it.
                Err(CallError::Remote { code, message }) if code == CODE_SETUP => {
                    ChannelHealth::unsupported(now, message)
                }
                Err(error) => ChannelHealth::error(now, error.into_message()),
            }
        }

        /// Ask the helper for everything after `cursor`.
        ///
        /// The cursor is a Messages `ROWID` and lives in this account's channel
        /// cursor, so a helper restart resumes rather than replaying. An
        /// account with none yet gets the current maximum and no messages.
        async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
            let since = cursor.and_then(|value| value.parse::<i64>().ok());
            let result = self
                .call(
                    "poll",
                    json!({ "since": since, "limit": MAX_ROWS_PER_POLL }),
                )
                .await
                .map_err(CallError::into_message)?;
            let envelopes = result
                .get("messages")
                .and_then(Value::as_array)
                .map(|records| records.iter().filter_map(normalize_record).collect())
                .unwrap_or_default();
            // Only advanced to what the helper actually reported. A cursor that
            // ran ahead of the rows handed on is how a message is lost between
            // a crash and the next poll.
            let cursor = result
                .get("cursor")
                .and_then(Value::as_i64)
                .map(|rowid| rowid.to_string());
            Ok(InboundBatch { envelopes, cursor })
        }

        async fn send(&self, message: &OutboundMessage) -> SendOutcome {
            // A group's conversation id is the chat identifier Messages
            // recorded and a direct one is the other party's handle. The
            // envelope the reply is to said which; this only has the id, so the
            // shape is what tells them apart.
            let is_group = message.conversation_id.contains(";+;")
                || message.conversation_id.starts_with("chat");
            let params = json!({
                "target": message.conversation_id,
                "text": message.text,
                "isGroup": is_group,
            });
            match self.call("send", params).await {
                Ok(_) => SendOutcome::Sent {
                    // Messages reports no identifier for what it just sent, and
                    // inventing one would poison dedupe.
                    provider_message_id: None,
                },
                Err(CallError::NotSent(error)) => SendOutcome::PermanentFailure { error },
                Err(CallError::Ambiguous(error)) => SendOutcome::NeedsReconciliation { error },
                // The helper's own "it may have happened" — Messages was asked
                // and did not answer in time. Retrying blind would double-send.
                Err(CallError::Remote { code, message }) if code == CODE_AMBIGUOUS => {
                    SendOutcome::NeedsReconciliation { error: message }
                }
                Err(CallError::Remote { message, .. }) => {
                    SendOutcome::PermanentFailure { error: message }
                }
            }
        }

        /// Ask the helper for one attachment's bytes.
        ///
        /// Nothing here opens the file. The Messages attachment store is behind
        /// Full Disk Access, which this process does not have and must not
        /// need.
        async fn fetch_attachment(
            &self,
            attachment: &ChannelAttachment,
            limits: crate::daemon::channel_adapter::AttachmentLimits,
        ) -> Result<Vec<u8>, String> {
            let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
                return Err("This iMessage attachment has no path.".to_string());
            };
            let result = self
                .call(
                    "fetchAttachment",
                    json!({ "path": handle, "maxBytes": limits.max_bytes }),
                )
                .await
                .map_err(CallError::into_message)?;
            let encoded = result
                .get("base64")
                .and_then(Value::as_str)
                .ok_or_else(|| "The helper returned no attachment data".to_string())?;
            // Base64 inflates by 4/3, so the encoded form is bounded first:
            // this refuses an oversized attachment before decoding it into
            // memory.
            if encoded.len() as u64 > limits.max_bytes.saturating_mul(4).div_ceil(3) + 4 {
                return Err(format!(
                    "The attachment is larger than the {}-byte limit",
                    limits.max_bytes
                ));
            }
            let bytes = base64::Engine::decode(
                &base64::engine::general_purpose::STANDARD,
                encoded.as_bytes(),
            )
            .map_err(|_| "The helper returned unreadable attachment data".to_string())?;
            if bytes.len() as u64 > limits.max_bytes {
                return Err(format!(
                    "The attachment is larger than the {}-byte limit",
                    limits.max_bytes
                ));
            }
            Ok(bytes)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use little_monkey_lib::channels::types::{ConversationKind, HealthState};

        fn record(extra: Value) -> Value {
            let mut base = json!({
                "guid": "GUID-1",
                "rowid": 1,
                "sender": "+15551230001",
                "isGroup": false,
                "text": "hello there",
                "timestamp": 1_700_000_000_000i64,
                "attachments": [],
            });
            for (key, value) in extra.as_object().expect("object") {
                base[key] = value.clone();
            }
            base
        }

        #[test]
        fn a_direct_message_is_keyed_on_the_sender_handle() {
            let envelope = normalize_record(&record(json!({}))).expect("envelope");
            assert_eq!(envelope.conversation.conversation_id, "+15551230001");
            assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
            assert_eq!(envelope.provider_event_id, "GUID-1");
            assert_eq!(envelope.received_at_ms, 1_700_000_000_000);
        }

        #[test]
        fn a_group_message_is_keyed_on_the_chat() {
            let envelope = normalize_record(&record(json!({
                "isGroup": true,
                "chatId": "chat9001",
                "displayName": "Weekend plans",
            })))
            .expect("envelope");
            assert_eq!(envelope.conversation.conversation_id, "chat9001");
            assert_eq!(envelope.conversation.kind, ConversationKind::Group);
            assert_eq!(
                envelope.sender.display_label.as_deref(),
                Some("Weekend plans")
            );
        }

        #[test]
        fn an_attachment_travels_as_a_handle_the_helper_will_resolve() {
            let envelope = normalize_record(&record(json!({
                "text": "",
                "attachments": [{
                    "path": "~/Library/Messages/Attachments/aa/photo.png",
                    "mimeType": "image/png",
                    "filename": "photo.png",
                    "size": 4096,
                }],
            })))
            .expect("an attachment alone is still a turn");
            let attachment = &envelope.attachments[0];
            assert_eq!(attachment.kind, AttachmentKind::Image);
            assert_eq!(attachment.declared_size_bytes, Some(4096));
            match &attachment.source {
                AttachmentSource::ProviderHandle { handle } => {
                    assert!(handle.ends_with("photo.png"));
                }
                other => panic!("expected a provider handle, got {other:?}"),
            }
        }

        #[test]
        fn a_reply_carries_the_originating_guid() {
            let envelope =
                normalize_record(&record(json!({"replyToGuid": "GUID-0"}))).expect("envelope");
            assert_eq!(envelope.reply_to_provider_id.as_deref(), Some("GUID-0"));
        }

        #[test]
        fn a_record_with_nothing_in_it_is_not_a_turn() {
            assert!(normalize_record(&record(json!({"text": ""}))).is_none());
            assert!(normalize_record(&json!({"text": "orphaned"})).is_none());
        }

        fn test_account(
            non_secret_config: Value,
        ) -> super::super::super::super::channel_store::ChannelAccountRecord {
            super::super::super::super::channel_store::ChannelAccountRecord {
                account_id: "acct-1".to_string(),
                kind: ChannelKind::IMessage,
                label: "iMessage".to_string(),
                enabled: true,
                non_secret_config,
                credential_ref: None,
                access_policy: Default::default(),
                health: ChannelHealth::error(0, "unused"),
                created_at_ms: 0,
                updated_at_ms: 0,
            }
        }

        #[test]
        fn an_account_without_a_helper_path_cannot_be_built() {
            // There is no native fallback any more: the permissions live in the
            // helper, so an account with no helper has nothing to talk to.
            let account = test_account(json!({ "handle": "user@example.com" }));
            assert!(ImessageAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .is_err());
        }

        #[test]
        fn an_account_needs_a_handle_too() {
            let account = test_account(json!({ "helper_path": "/usr/local/bin/helper" }));
            assert!(ImessageAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .is_err());
        }

        #[tokio::test]
        async fn a_missing_helper_is_setup_required_and_never_a_panic() {
            let account = test_account(json!({
                "helper_path": "/definitely/not/a/real/path/imessage-helper",
                "handle": "user@example.com",
            }));
            let adapter = ImessageAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter constructs");
            let health = adapter.probe().await;
            assert_eq!(health.state, HealthState::Unsupported);
            let detail = health.detail.expect("detail");
            assert!(detail.contains("little-monkey-imessage-helper"));
            // The operator's own path is not echoed back into a health string.
            assert!(!detail.contains("/definitely/not/a/real/path"));
        }

        #[test]
        fn no_apple_credential_is_ever_required() {
            // The account is whichever one Messages is already signed in to.
            let account = test_account(json!({
                "helper_path": "/usr/local/bin/helper",
                "handle": "user@example.com",
            }));
            assert!(ImessageAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .is_ok());
            assert!(!crate::daemon::channel_adapter::credential_required(
                &account
            ));
        }

        /// An opt-in smoke test against the Mac this runs on, using the
        /// operator's own Messages account.
        ///
        /// Never runs unless all three variables are set, so CI and every
        /// contributor's `cargo test` skip it. No Apple account is hard-coded
        /// anywhere — the destination is one the person running it chose, and
        /// it is the only place a message is ever sent.
        ///
        /// ```text
        /// LM_IMESSAGE_LIVE_HELPER=/usr/local/bin/little-monkey-imessage-helper \
        /// LM_IMESSAGE_LIVE_HANDLE=you@example.com \
        /// LM_IMESSAGE_LIVE_TARGET=+15550000000 \
        ///   cargo test --bin monkey-cli a_live_messages_round_trip -- --nocapture
        /// ```
        #[tokio::test]
        async fn a_live_messages_round_trip() {
            let (Ok(helper), Ok(handle), Ok(target)) = (
                std::env::var("LM_IMESSAGE_LIVE_HELPER"),
                std::env::var("LM_IMESSAGE_LIVE_HANDLE"),
                std::env::var("LM_IMESSAGE_LIVE_TARGET"),
            ) else {
                return;
            };
            let account = test_account(json!({
                "helper_path": helper,
                "handle": handle,
            }));
            let adapter = ImessageAdapter::new(&AdapterConfig {
                account: &account,
                secret: String::new(),
            })
            .expect("adapter");

            let health = adapter.probe().await;
            assert_eq!(
                health.state,
                HealthState::Connected,
                "{:?}{:?} — grant the helper Full Disk Access and Automation for Messages first",
                health.detail,
                health.last_error
            );

            // A first poll records where the database is rather than replaying.
            let batch = adapter.poll(None).await.expect("poll");
            assert!(batch.envelopes.is_empty());
            let cursor = batch.cursor.expect("a cursor");

            let marker = uuid::Uuid::new_v4().simple().to_string();
            let outcome = adapter
                .send(&OutboundMessage {
                    account_id: "acct-1".to_string(),
                    kind: ChannelKind::IMessage,
                    conversation_id: target,
                    thread_id: None,
                    text: format!("little-monkey live smoke test {marker}"),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: format!("live-{marker}"),
                })
                .await;
            assert!(matches!(outcome, SendOutcome::Sent { .. }), "{outcome:?}");

            // Our own outbound message must never come back as an inbound turn.
            let batch = adapter.poll(Some(&cursor)).await.expect("poll");
            assert!(
                !batch
                    .envelopes
                    .iter()
                    .any(|envelope| envelope.text.contains(&marker)),
                "an echo of our own send would make the agent answer itself"
            );
        }

        /// Everything below drives a *fake* helper — a shell script speaking
        /// the real protocol. No Messages.app, no Full Disk Access, no
        /// conversation: the lifecycle (spawn, request/response by id, poll
        /// cursor, error codes, crash, restart) is provable on a machine that
        /// has never been signed in to iMessage.
        mod fake_helper {
            use super::*;
            use std::os::unix::fs::PermissionsExt;

            /// Each line goes out through `/bin/echo` so no shell stdio buffer
            /// can hold it back.
            fn write_fake_helper(name: &str) -> std::path::PathBuf {
                let path = std::env::temp_dir().join(format!(
                    "monkey-fake-imessage-{name}-{}",
                    uuid::Uuid::new_v4().simple()
                ));
                let script = r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *\"crash\"*) exit 7 ;;
    *\"probe\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"handles\":12,\"canSend\":true}}" ;;
    *\"noAutomation\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"handles\":12,\"canSend\":false}}" ;;
    *\"needsSetup\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32002,\"message\":\"Grant Full Disk Access\"}}" ;;
    *\"ambiguous\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"error\":{\"code\":-32001,\"message\":\"Messages did not answer in time\"}}" ;;
    *\"since\":null*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"cursor\":42,\"messages\":[]}}" ;;
    *\"poll\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"cursor\":43,\"messages\":[{\"guid\":\"GUID-9\",\"rowid\":43,\"sender\":\"+15551230001\",\"isGroup\":false,\"text\":\"after the cursor\",\"timestamp\":1700000000000,\"attachments\":[]}]}}" ;;
    *\"fetchAttachment\"*)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"base64\":\"aGVsbG8=\"}}" ;;
    *)
      /bin/echo "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"sent\":true}}" ;;
  esac
done
"#;
                std::fs::write(&path, script).expect("write fake helper");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake helper");
                path
            }

            fn adapter(path: &std::path::Path) -> ImessageAdapter {
                let account = test_account(json!({
                    "helper_path": path.to_string_lossy(),
                    "handle": "user@example.com",
                }));
                ImessageAdapter::new(&AdapterConfig {
                    account: &account,
                    secret: String::new(),
                })
                .expect("adapter")
            }

            #[tokio::test]
            async fn a_probe_reports_what_the_helper_can_actually_do() {
                let path = write_fake_helper("probe");
                let health = adapter(&path).probe().await;
                assert_eq!(health.state, HealthState::Connected);
                assert!(health.detail.expect("detail").contains("12 known handles"));
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_first_poll_takes_the_cursor_and_replays_nothing() {
                let path = write_fake_helper("first-poll");
                let adapter = adapter(&path);

                let batch = adapter.poll(None).await.expect("poll");
                assert!(
                    batch.envelopes.is_empty(),
                    "connecting an account must not replay the Mac's history"
                );
                assert_eq!(batch.cursor.as_deref(), Some("42"));

                // And from a cursor, only what came after it.
                let batch = adapter.poll(Some("42")).await.expect("poll");
                assert_eq!(batch.envelopes.len(), 1);
                assert_eq!(batch.envelopes[0].provider_event_id, "GUID-9");
                assert_eq!(batch.cursor.as_deref(), Some("43"));
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn an_attachment_comes_back_through_the_helper_not_off_the_disk() {
                let path = write_fake_helper("attachment");
                let attachment = ChannelAttachment {
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                    provider_id: None,
                    kind: AttachmentKind::Image,
                    filename: Some("x.png".to_string()),
                    mime_type: Some("image/png".to_string()),
                    declared_size_bytes: None,
                    source: AttachmentSource::ProviderHandle {
                        // A path this process cannot read, and never tries to.
                        handle: "~/Library/Messages/Attachments/aa/x.png".to_string(),
                    },
                };
                let bytes = adapter(&path)
                    .fetch_attachment(&attachment, Default::default())
                    .await
                    .expect("bytes");
                assert_eq!(bytes, b"hello");
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_send_the_helper_acknowledges_is_sent() {
                let path = write_fake_helper("send");
                let outcome = adapter(&path)
                    .send(&OutboundMessage {
                        account_id: "acct-1".to_string(),
                        kind: ChannelKind::IMessage,
                        conversation_id: "+15551230001".to_string(),
                        thread_id: None,
                        text: "ack".to_string(),
                        attachments: Vec::new(),
                        reply_to_provider_id: None,
                        idempotency_key: "idem-1".to_string(),
                    })
                    .await;
                assert!(matches!(outcome, SendOutcome::Sent { .. }));
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn the_helpers_ambiguous_code_is_never_retried_blind() {
                let path = write_fake_helper("ambiguous");
                let adapter = adapter(&path);
                // The helper says Messages was asked and did not answer. A
                // retry here would send the same message twice.
                assert!(matches!(
                    adapter.call("ambiguous", json!({})).await,
                    Err(CallError::Remote {
                        code: CODE_AMBIGUOUS,
                        ..
                    })
                ));
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_missing_permission_is_setup_required_rather_than_an_error() {
                let path = write_fake_helper("setup");
                let adapter = adapter(&path);
                match adapter.call("needsSetup", json!({})).await {
                    Err(CallError::Remote { code, message }) => {
                        assert_eq!(code, CODE_SETUP);
                        assert!(message.contains("Full Disk Access"));
                    }
                    other => panic!(
                        "expected a setup error, got a different outcome: {}",
                        other
                            .map(|_| "success".to_string())
                            .unwrap_or_else(CallError::into_message)
                    ),
                }
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn automation_that_was_never_granted_reads_as_degraded_not_connected() {
                let path = write_fake_helper("automation");
                let adapter = adapter(&path);
                // Overriding the method the probe sends is not possible from
                // out here, so this drives the same branch through `call` and
                // asserts on the health `probe` would build from it.
                let result = adapter
                    .call("noAutomation", json!({}))
                    .await
                    .expect("the helper answered");
                assert_eq!(result["canSend"], false);
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_helper_that_exits_is_started_again_rather_than_left_dead() {
                let path = write_fake_helper("restart");
                let adapter = adapter(&path);

                assert_eq!(adapter.probe().await.state, HealthState::Connected);
                // A request in flight when the helper dies is ambiguous — it
                // may have been acted on — never a permanent failure.
                assert!(matches!(
                    adapter.call("crash", json!({})).await,
                    Err(CallError::Ambiguous(_))
                ));
                assert!(!adapter.shared.alive.load(Ordering::SeqCst));
                // Within the cooldown: an error, not a respawn per poll.
                assert_eq!(adapter.probe().await.state, HealthState::Error);
                // Past it: back up, without restarting the daemon.
                *adapter.last_start.lock().await = None;
                assert_eq!(adapter.probe().await.state, HealthState::Connected);
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_helper_that_dies_at_once_is_retried_more_and_more_slowly() {
                let path = write_fake_helper("backoff");
                let adapter = adapter(&path);
                assert_eq!(
                    *adapter.shared.restart_cooldown.lock().await,
                    INITIAL_RESTART_COOLDOWN
                );
                for expected in [INITIAL_RESTART_COOLDOWN * 2, INITIAL_RESTART_COOLDOWN * 4] {
                    *adapter.last_start.lock().await = None;
                    let _ = adapter.call("crash", json!({})).await;
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
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) use macos::ImessageAdapter;

/// Non-macOS stand-in: there is no Messages database and no Messages.app to
/// automate anywhere else, so every method here refuses rather than pretending.
/// `probe` never touches the filesystem or spawns anything.
#[cfg(not(target_os = "macos"))]
mod other {
    use async_trait::async_trait;
    use little_monkey_lib::channels::types::{
        ChannelHealth, ChannelKind, InboundTransport, OutboundMessage, ProviderCapabilities,
        SendOutcome,
    };

    use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};

    pub struct ImessageAdapter;

    impl ImessageAdapter {
        pub fn new(_config: &AdapterConfig<'_>) -> Result<Self, String> {
            Ok(Self)
        }
    }

    fn now_ms() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or(0)
    }

    #[async_trait]
    impl ChannelAdapter for ImessageAdapter {
        fn kind(&self) -> ChannelKind {
            ChannelKind::IMessage
        }

        fn capabilities(&self) -> ProviderCapabilities {
            ProviderCapabilities {
                kind: ChannelKind::IMessage,
                inbound_transport: InboundTransport::Helper,
                max_text_chars: 0,
                supports_threads: false,
                supports_attachments: false,
                supports_mention_metadata: false,
                supports_idempotency_key: false,
                supports_delivery_receipts: false,
            }
        }

        async fn probe(&self) -> ChannelHealth {
            ChannelHealth::unsupported(now_ms(), "iMessage is only available on macOS")
        }

        async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
            Err("iMessage is only available on macOS".to_string())
        }

        async fn send(&self, _message: &OutboundMessage) -> SendOutcome {
            SendOutcome::PermanentFailure {
                error: "iMessage is only available on macOS".to_string(),
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use little_monkey_lib::channels::types::HealthState;

        #[tokio::test]
        async fn refuses_without_touching_anything_on_non_macos() {
            let adapter = ImessageAdapter;
            let health = adapter.probe().await;
            assert_eq!(health.state, HealthState::Unsupported);
            assert!(adapter.poll(None).await.is_err());
            match adapter
                .send(&OutboundMessage {
                    account_id: "acct".to_string(),
                    kind: ChannelKind::IMessage,
                    conversation_id: "x".to_string(),
                    thread_id: None,
                    text: "hi".to_string(),
                    attachments: Vec::new(),
                    reply_to_provider_id: None,
                    idempotency_key: "k".to_string(),
                })
                .await
            {
                SendOutcome::PermanentFailure { .. } => {}
                other => panic!("expected PermanentFailure, got {other:?}"),
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) use other::ImessageAdapter;

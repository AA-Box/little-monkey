//! iMessage adapter: speaks newline-delimited JSON to a user-installed,
//! macOS-only helper process (JSON commands in on stdin, JSON events out on
//! stdout) — the same shape as `signal.rs`, and for the same reason. Little
//! Monkey does not ship a Messages integration, does not bypass SIP, does
//! not read `~/Library/Messages/chat.db` directly, does not link any
//! private Apple framework, and never builds an AppleScript command string
//! from message text (that would be command injection into `osascript`).
//! Everything provider-specific lives inside the operator's own helper; this
//! module only speaks its stdio protocol.
//!
//! The real implementation ([`macos::ImessageAdapter`]) is
//! `#[cfg(target_os = "macos")]`. Every other platform gets
//! [`other::ImessageAdapter`], a stub whose `probe` reports
//! [`ChannelHealth::unsupported`] without touching the filesystem or
//! spawning anything, and whose `poll`/`send` refuse outright.

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};
    use tokio::sync::{mpsc, oneshot, Mutex};
    use tokio::time::Instant;

    use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};
    use little_monkey_lib::channels::types::{
        AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
        ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
        ProviderCapabilities, SendOutcome,
    };

    const INBOUND_CHANNEL_CAPACITY: usize = 256;
    const RPC_TIMEOUT: Duration = Duration::from_secs(20);
    /// How long to wait before spawning the helper again after an attempt,
    /// so a helper that exits on startup is retried at a bounded rate
    /// rather than once per `poll`.
    const RESTART_COOLDOWN: Duration = Duration::from_secs(5);
    /// Messages.app splits long iMessages client-side; there is no
    /// server-enforced cap this adapter can query. A conservative budget —
    /// ponytail: revisit if a real helper reports the split boundary it
    /// actually uses.
    const MAX_TEXT_CHARS: usize = 20_000;

    /// Outcome of one JSON-RPC round trip, distinguished by whether a
    /// command provably reached the helper's stdin. Same shape as
    /// `signal.rs`'s `CallError` — duplicated rather than shared because
    /// this file owns exactly this module and `signal.rs`'s type is private
    /// to it.
    enum CallError {
        NotSent(String),
        Ambiguous(String),
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
    }

    pub struct ImessageAdapter {
        helper_path: String,
        handle: String,
        inbound_tx: mpsc::Sender<ChannelEnvelope>,
        inbound_rx: Mutex<mpsc::Receiver<ChannelEnvelope>>,
        shared: Arc<Shared>,
        /// Serializes spawn attempts and remembers when the last one was
        /// made — not a one-shot cell, for the same reason as `signal.rs`: a
        /// helper the user quits (or that Messages.app takes down with it)
        /// has to be startable again without restarting the daemon.
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
                .ok_or_else(|| "iMessage account is missing helper_path".to_string())?
                .to_string();
            let handle = config
                .account
                .non_secret_config
                .get("handle")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "iMessage account is missing handle".to_string())?
                .to_string();
            let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CHANNEL_CAPACITY);
            Ok(Self {
                helper_path,
                handle,
                inbound_tx,
                inbound_rx: Mutex::new(inbound_rx),
                shared: Arc::new(Shared {
                    next_id: AtomicU64::new(1),
                    pending: Mutex::new(HashMap::new()),
                    stdin: Mutex::new(None),
                    alive: AtomicBool::new(false),
                }),
                last_start: Mutex::new(None),
            })
        }

        /// `Some(message)` when the configured path does not exist. Checked
        /// before every spawn attempt — including inside `ensure_started` —
        /// so a stale or never-installed path never reaches
        /// `Command::spawn`.
        fn helper_missing(&self) -> Option<String> {
            if std::path::Path::new(&self.helper_path).is_file() {
                None
            } else {
                Some(
                    "Install the iMessage helper and set its path in this account's settings"
                        .to_string(),
                )
            }
        }

        /// Start the helper, or restart it if a previous one exited. See
        /// `signal.rs`'s `ensure_started` for the reasoning; `alive` is the
        /// whole condition and [`RESTART_COOLDOWN`] bounds how often a
        /// helper that dies immediately is retried.
        async fn ensure_started(&self) -> Result<(), String> {
            if self.shared.alive.load(Ordering::SeqCst) {
                return Ok(());
            }
            let mut last_start = self.last_start.lock().await;
            if self.shared.alive.load(Ordering::SeqCst) {
                return Ok(());
            }
            if let Some(attempted_at) = *last_start {
                if attempted_at.elapsed() < RESTART_COOLDOWN {
                    return Err(
                        "The iMessage helper stopped; waiting before starting it again".to_string(),
                    );
                }
            }
            *last_start = Some(Instant::now());
            if let Some(error) = self.helper_missing() {
                return Err(error);
            }
            let mut command = Command::new(&self.helper_path);
            command
                .args(["--handle", &self.handle, "stream"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            let mut child = command
                .spawn()
                .map_err(|error| format!("Failed to start the iMessage helper: {error}"))?;
            let stdin = child
                .stdin
                .take()
                .ok_or_else(|| "iMessage helper has no stdin".to_string())?;
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| "iMessage helper has no stdout".to_string())?;
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
            let mut line = match serde_json::to_vec(&request) {
                Ok(line) => line,
                Err(error) => {
                    if let Ok(mut pending) = self.shared.pending.try_lock() {
                        pending.remove(&id);
                    }
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
                Ok(Ok(Err(remote_error))) => Err(CallError::Remote(remote_error)),
                Ok(Err(_)) => Err(CallError::Ambiguous(
                    "The iMessage helper exited before answering".to_string(),
                )),
                Err(_) => Err(CallError::Ambiguous(
                    "The iMessage helper did not answer in time".to_string(),
                )),
            }
        }
    }

    async fn run_rpc_loop(
        mut child: Child,
        stdout: ChildStdout,
        shared: Arc<Shared>,
        inbound_tx: mpsc::Sender<ChannelEnvelope>,
    ) {
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
                        let _ = inbound_tx.send(envelope).await;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
        shared.alive.store(false, Ordering::SeqCst);
        *shared.stdin.lock().await = None;
        // Dropped rather than answered with an error, same as `signal.rs`: a
        // request the helper never replied to is ambiguous — it may have been
        // acted on — and only a dropped sender reaches `call`'s ambiguous arm.
        shared.pending.lock().await.clear();
        let _ = child.wait().await;
    }

    /// Deterministic fallback id for a helper event carrying no GUID: sha256
    /// over (handle, chat id, timestamp, text). The same inputs always hash
    /// to the same id — required for the durable-event dedupe key — and
    /// it is never random.
    fn deterministic_id(handle: &str, chat_id: &str, timestamp: i64, text: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(handle.as_bytes());
        hasher.update(b"\0");
        hasher.update(chat_id.as_bytes());
        hasher.update(b"\0");
        hasher.update(timestamp.to_le_bytes());
        hasher.update(b"\0");
        hasher.update(text.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Parses one newline-delimited JSON event from the helper's stream into
    /// a normalized envelope. Returns `None` for anything that is not an
    /// inbound message (malformed JSON, a non-`"message"` event, an empty
    /// message with no attachments).
    fn parse_event(line: &str) -> Option<ChannelEnvelope> {
        let value: Value = serde_json::from_str(line).ok()?;
        if value.get("type").and_then(Value::as_str) != Some("message") {
            return None;
        }
        let sender = value.get("sender").and_then(Value::as_str)?.to_string();
        let timestamp = value.get("timestamp").and_then(Value::as_i64)?;
        let text = value
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let chat_id = value
            .get("chatId")
            .and_then(Value::as_str)
            .map(str::to_string);
        let is_group = value
            .get("isGroup")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let attachments: Vec<ChannelAttachment> = value
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
                            provider_id: None,
                            kind,
                            filename: item
                                .get("filename")
                                .and_then(Value::as_str)
                                .map(str::to_string),
                            mime_type,
                            declared_size_bytes: item.get("size").and_then(Value::as_u64),
                            source: AttachmentSource::ProviderHandle { handle: path },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        if text.is_empty() && attachments.is_empty() {
            return None;
        }

        let conversation = if is_group {
            ChannelConversation::group(chat_id.clone().unwrap_or_else(|| sender.clone()))
        } else {
            ChannelConversation::direct(sender.clone())
        };

        let guid = value
            .get("guid")
            .and_then(Value::as_str)
            .map(str::to_string);
        let provider_event_id = guid.unwrap_or_else(|| {
            deterministic_id(&sender, chat_id.as_deref().unwrap_or(""), timestamp, &text)
        });

        Some(ChannelEnvelope {
            account_id: String::new(),
            kind: ChannelKind::IMessage,
            provider_event_id,
            conversation,
            sender: ChannelSender {
                sender_id: sender,
                display_label: None,
                is_self: false,
                is_bot: false,
            },
            text,
            attachments,
            // ponytail: no reply/thread field in this helper convention yet
            // — add when a real helper exposes one.
            reply_to_provider_id: None,
            // ponytail: no mention metadata in this payload; DMs need none,
            // groups fall back to allow-list authorization.
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
                supports_attachments: true,
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
            match self.call("handles", json!({})).await {
                Ok(_) => ChannelHealth::connected(now, Some(self.handle.clone())),
                Err(error) => ChannelHealth::error(now, error.into_message()),
            }
        }

        async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
            self.ensure_started().await?;
            let mut rx = self.inbound_rx.lock().await;
            let mut envelopes = Vec::new();
            match tokio::time::timeout(RPC_TIMEOUT, rx.recv()).await {
                Ok(Some(envelope)) => envelopes.push(envelope),
                Ok(None) => return Err("The iMessage helper's inbound channel closed".to_string()),
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
            let params = json!({ "target": message.conversation_id, "text": message.text });
            match self.call("send", params).await {
                Ok(result) => SendOutcome::Sent {
                    provider_message_id: result
                        .get("guid")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                Err(CallError::NotSent(error)) => SendOutcome::PermanentFailure { error },
                Err(CallError::Ambiguous(error)) => SendOutcome::NeedsReconciliation { error },
                Err(CallError::Remote(error)) => SendOutcome::PermanentFailure { error },
            }
        }

        /// The helper reports iMessage attachments as paths into the Messages
        /// attachment store on this machine, so the bytes are read directly —
        /// there is no network fetch and no second copy of the file.
        ///
        /// The size is checked from the directory entry before the read, so an
        /// oversized attachment costs a `stat` rather than the whole file.
        async fn fetch_attachment(
            &self,
            attachment: &ChannelAttachment,
            max_bytes: u64,
        ) -> Result<Vec<u8>, String> {
            let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
                return Err("This iMessage attachment has no path.".to_string());
            };
            let path = std::path::Path::new(handle);
            let metadata = tokio::fs::metadata(path)
                .await
                .map_err(|error| format!("That attachment is no longer readable: {error}"))?;
            if !metadata.is_file() {
                return Err("That iMessage attachment is not a file".to_string());
            }
            if metadata.len() > max_bytes {
                return Err(format!("The attachment is larger than {max_bytes} bytes."));
            }
            tokio::fs::read(path)
                .await
                .map_err(|error| format!("That attachment could not be read: {error}"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const DM_LINE: &str = r#"{
            "type": "message",
            "guid": "GUID-1",
            "sender": "+15551230001",
            "text": "hello there",
            "timestamp": 1700000000000
        }"#;

        const GROUP_LINE: &str = r#"{
            "type": "message",
            "guid": "GUID-2",
            "sender": "+15551230002",
            "chatId": "chat-abc",
            "isGroup": true,
            "text": "hi all",
            "timestamp": 1700000001000
        }"#;

        const ATTACHMENT_LINE: &str = r#"{
            "type": "message",
            "guid": "GUID-3",
            "sender": "+15551230003",
            "text": "look",
            "timestamp": 1700000002000,
            "attachments": [{"path": "/tmp/x.png", "mimeType": "image/png", "filename": "x.png", "size": 1024}]
        }"#;

        const NO_GUID_LINE: &str = r#"{
            "type": "message",
            "sender": "+15551230004",
            "text": "no guid here",
            "timestamp": 1700000003000
        }"#;

        const EMPTY_LINE: &str = r#"{
            "type": "message",
            "sender": "+15551230005",
            "text": "",
            "timestamp": 1700000004000
        }"#;

        #[test]
        fn parses_a_direct_message_keyed_on_sender_handle() {
            let envelope = parse_event(DM_LINE).expect("envelope");
            assert_eq!(envelope.conversation.conversation_id, "+15551230001");
            assert_eq!(
                envelope.conversation.kind,
                little_monkey_lib::channels::types::ConversationKind::Direct
            );
            assert_eq!(envelope.provider_event_id, "GUID-1");
        }

        #[test]
        fn parses_a_group_message_keyed_on_chat_id() {
            let envelope = parse_event(GROUP_LINE).expect("envelope");
            assert_eq!(envelope.conversation.conversation_id, "chat-abc");
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
            assert_eq!(attachment.declared_size_bytes, Some(1024));
            match &attachment.source {
                AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "/tmp/x.png"),
                other => panic!("expected a provider handle, got {other:?}"),
            }
        }

        #[test]
        fn falls_back_to_a_deterministic_sha256_id_when_no_guid_is_given() {
            let first = parse_event(NO_GUID_LINE).expect("envelope");
            let second = parse_event(NO_GUID_LINE).expect("envelope");
            assert_eq!(first.provider_event_id, second.provider_event_id);
            assert_ne!(first.provider_event_id, "");
            // Not the raw GUID shape (helper gave none) and not a UUID —
            // a hex sha256 digest is 64 hex chars.
            assert_eq!(first.provider_event_id.len(), 64);
            assert!(first
                .provider_event_id
                .chars()
                .all(|c| c.is_ascii_hexdigit()));
        }

        #[test]
        fn ignores_empty_messages_and_malformed_lines() {
            assert!(parse_event(EMPTY_LINE).is_none());
            assert!(parse_event("not json").is_none());
            assert!(parse_event(r#"{"type": "typing"}"#).is_none());
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

        #[tokio::test]
        async fn a_missing_helper_path_yields_an_actionable_unsupported_health_never_a_panic() {
            let account = test_account(json!({
                "helper_path": "/definitely/not/a/real/path/imessage-helper",
                "handle": "user@example.com",
            }));
            let config = AdapterConfig {
                account: &account,
                secret: String::new(),
            };
            let adapter = ImessageAdapter::new(&config).expect("adapter constructs");
            let health = adapter.probe().await;
            assert_eq!(
                health.state,
                little_monkey_lib::channels::types::HealthState::Unsupported
            );
            let detail = health.detail.expect("detail");
            assert!(!detail.contains("/definitely/not/a/real/path/imessage-helper"));
        }

        #[test]
        fn capabilities_report_the_declared_text_limit() {
            let account = test_account(json!({
                "helper_path": "/usr/local/bin/imessage-helper",
                "handle": "user@example.com",
            }));
            let config = AdapterConfig {
                account: &account,
                secret: String::new(),
            };
            let adapter = ImessageAdapter::new(&config).expect("adapter");
            let capabilities = adapter.capabilities();
            assert_eq!(capabilities.max_text_chars, MAX_TEXT_CHARS);
            assert_eq!(capabilities.kind, ChannelKind::IMessage);
        }

        #[test]
        fn rejects_missing_config() {
            let account = test_account(json!({}));
            let config = AdapterConfig {
                account: &account,
                secret: String::new(),
            };
            assert!(ImessageAdapter::new(&config).is_err());
        }

        #[test]
        fn accepts_a_fully_configured_account() {
            let account = test_account(json!({
                "helper_path": "/usr/local/bin/imessage-helper",
                "handle": "user@example.com",
            }));
            let config = AdapterConfig {
                account: &account,
                secret: String::new(),
            };
            assert!(ImessageAdapter::new(&config).is_ok());
        }

        /// Drives a *fake* helper — a shell script speaking this module's own
        /// stdio convention. No Messages.app, no Full Disk Access, no real
        /// conversation: the lifecycle (spawn, request/response, inbound
        /// event, malformed line, crash, restart) is provable on a machine
        /// that has never been signed in to iMessage.
        mod fake_helper {
            use super::*;
            use little_monkey_lib::channels::types::HealthState;
            use std::os::unix::fs::PermissionsExt;

            /// Each line goes out through `/bin/echo` so no shell stdio
            /// buffer can hold it back — see `signal.rs`'s equivalent.
            fn write_fake_helper(name: &str) -> std::path::PathBuf {
                let path = std::env::temp_dir().join(format!(
                    "monkey-fake-imessage-{name}-{}",
                    uuid::Uuid::new_v4().simple()
                ));
                let script = r#"#!/bin/sh
/bin/echo '{"type":"message","guid":"GUID-1","sender":"+15551230001","text":"hello there","timestamp":1700000000000}'
/bin/echo 'this line is not JSON'
while IFS= read -r line; do
  case "$line" in
    *crash*) exit 7 ;;
  esac
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  /bin/echo "{\"id\":$id,\"result\":{\"guid\":\"GUID-SENT\"}}"
done
"#;
                std::fs::write(&path, script).expect("write fake helper");
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod fake helper");
                path
            }

            fn helper_account(
                path: &std::path::Path,
            ) -> super::super::super::super::super::channel_store::ChannelAccountRecord
            {
                test_account(json!({
                    "helper_path": path.to_string_lossy(),
                    "handle": "user@example.com",
                }))
            }

            #[tokio::test]
            async fn probes_and_streams_inbound_ignoring_unparseable_lines() {
                let path = write_fake_helper("probe");
                let account = helper_account(&path);
                let adapter = ImessageAdapter::new(&AdapterConfig {
                    account: &account,
                    secret: String::new(),
                })
                .expect("adapter");

                let health = adapter.probe().await;
                assert_eq!(health.state, HealthState::Connected);

                let batch = adapter.poll(None).await.expect("poll");
                assert_eq!(batch.envelopes.len(), 1, "the junk line must not normalize");
                assert_eq!(batch.envelopes[0].provider_event_id, "GUID-1");
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_send_the_helper_acknowledges_carries_its_guid() {
                let path = write_fake_helper("send");
                let account = helper_account(&path);
                let adapter = ImessageAdapter::new(&AdapterConfig {
                    account: &account,
                    secret: String::new(),
                })
                .expect("adapter");

                match adapter
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
                    .await
                {
                    SendOutcome::Sent {
                        provider_message_id,
                    } => assert_eq!(provider_message_id.as_deref(), Some("GUID-SENT")),
                    other => panic!("expected Sent, got {other:?}"),
                }
                let _ = std::fs::remove_file(&path);
            }

            #[tokio::test]
            async fn a_helper_that_exits_is_started_again_rather_than_left_dead() {
                let path = write_fake_helper("restart");
                let account = helper_account(&path);
                let adapter = ImessageAdapter::new(&AdapterConfig {
                    account: &account,
                    secret: String::new(),
                })
                .expect("adapter");

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
        }
    }
}

// `daemon/adapters/mod.rs` (owned by a sibling change, not this file) does
// not yet route `ChannelKind::IMessage` to either re-export below, so this
// symbol has no in-crate caller until that wiring lands.
#[cfg(target_os = "macos")]
#[allow(unused_imports)]
pub(crate) use macos::ImessageAdapter;

/// Non-macOS stand-in: iMessage automation only exists on macOS — there is
/// no cross-platform Messages helper to speak to — so every method here
/// refuses rather than pretending to work. `probe` never touches the
/// filesystem or spawns anything; there is nothing this build could
/// possibly connect to.
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
#[allow(unused_imports)]
pub(crate) use other::ImessageAdapter;

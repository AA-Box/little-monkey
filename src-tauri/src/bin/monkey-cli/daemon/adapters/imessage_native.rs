//! The real macOS iMessage backend: read the Messages database, send
//! through Messages.app.
//!
//! `imessage.rs` can drive either a user-installed helper process or this
//! module. This one is what runs when no helper is configured, and it is the
//! only path that needs nothing installed beyond macOS itself.
//!
//! # Inbound: `~/Library/Messages/chat.db`
//!
//! Messages.app keeps its own SQLite database. It is opened **read-only**,
//! with SQLite's immutable-free `mode=ro` so a live Messages.app writing to
//! it is never blocked, and nothing here ever writes, migrates or vacuums it:
//! a corrupted Messages database is not a failure mode this app is allowed to
//! introduce. Reading it requires Full Disk Access for the process, which is
//! a normal macOS permission the user grants in System Settings — no SIP
//! change, no injected dylib, no private framework.
//!
//! Resume is the message table's own `ROWID`, which is monotonic per insert;
//! it is stored as the account's channel cursor. The **first** poll of an
//! account deliberately returns nothing and simply records the current
//! maximum: without that, connecting an account would replay years of
//! history into the agent as if it had all just arrived.
//!
//! # Text is not always in `text`
//!
//! Since macOS 11 the plain-text column is frequently NULL, with the body
//! living in `attributedBody` — an `NSAttributedString` serialized as a
//! typedstream. [`typedstream_text`] pulls the string payload out of it
//! without linking any Apple framework. It is a parser over a documented
//! archive layout, not an injection point: it only ever reads.
//!
//! # Outbound: `osascript`
//!
//! Sending uses AppleScript's Messages dictionary, run as an argument
//! vector: the script is a fixed constant with `on run argv`, and the
//! recipient and the message text arrive as *arguments*. Message text is
//! never interpolated into script source — that would be command injection
//! into `osascript` from whatever a stranger typed. Sending needs the
//! Automation permission for Messages.app, again a normal macOS prompt.

use std::path::{Path, PathBuf};
use std::time::Duration;

use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelKind, ChannelSender,
};
use rusqlite::{Connection, OpenFlags};

/// Seconds between the Unix epoch and Apple's (2001-01-01T00:00:00Z).
const APPLE_EPOCH_OFFSET_SECONDS: i64 = 978_307_200;
/// Rows per poll. A bound, not a page size: the cursor advances by what was
/// read, so a backlog drains over several polls instead of arriving as one
/// unbounded batch.
const MAX_ROWS_PER_POLL: usize = 100;
/// How long `osascript` gets to hand a message to Messages.app.
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// `chat.style` for a group conversation. `45` is a one-to-one chat.
const CHAT_STYLE_GROUP: i64 = 43;

/// AppleScript run with `osascript <script> <target> <text> <is_group>`.
///
/// Fixed source. Everything variable is an element of `argv`, which is what
/// keeps a message body from being read as script.
const SEND_SCRIPT: &str = r#"on run argv
    set targetId to item 1 of argv
    set messageText to item 2 of argv
    set isGroup to item 3 of argv
    tell application "Messages"
        if isGroup is "1" then
            send messageText to chat id targetId
        else
            set targetService to 1st account whose service type = iMessage
            send messageText to participant targetId of targetService
        end if
    end tell
end run
"#;

/// Where the Messages database and the script runner live.
///
/// Both are overridable so the tests can drive a database they built and an
/// `osascript` that records what it was handed. Neither override is exposed
/// as a way to run an arbitrary command with arbitrary text: the script is
/// still this module's own constant, and the runner is still invoked as an
/// argument vector.
#[derive(Debug, Clone)]
pub(crate) struct NativeConfig {
    pub db_path: PathBuf,
    pub osascript_path: PathBuf,
}

impl NativeConfig {
    /// The stock locations, plus whatever the account overrode.
    pub fn resolve(non_secret_config: &serde_json::Value) -> Self {
        let db_path = non_secret_config
            .get("db_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(default_db_path);
        let osascript_path = non_secret_config
            .get("osascript_path")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/bin/osascript"));
        Self {
            db_path,
            osascript_path,
        }
    }
}

fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Messages/chat.db")
}

/// One inbound batch plus the ROWID to resume from.
pub(crate) struct NativeBatch {
    pub envelopes: Vec<ChannelEnvelope>,
    pub cursor: i64,
}

/// Open the database read-only.
///
/// `SQLITE_OPEN_READ_ONLY` alone still takes a shared lock and still wants a
/// writable directory for the WAL; `mode=ro` on a URI does the same without
/// either, which is what lets this coexist with a running Messages.app.
fn open_read_only(db_path: &Path) -> Result<Connection, String> {
    if !db_path.exists() {
        return Err(format!(
            "No Messages database at {}. Sign in to Messages on this Mac first.",
            db_path.display()
        ));
    }
    let uri = format!("file:{}?mode=ro", db_path.to_string_lossy());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| describe_open_failure(db_path, &error))
}

/// Turn a SQLite failure into the sentence that names the fix.
///
/// The overwhelmingly common one is Full Disk Access: without it the open
/// fails with a plain "unable to open database file", which tells a user
/// nothing about the switch they have to flip.
fn describe_open_failure(db_path: &Path, error: &rusqlite::Error) -> String {
    let message = error.to_string();
    if message.contains("unable to open") || message.contains("authorization denied") {
        return format!(
            "Cannot read the Messages database at {}. Grant Full Disk Access to this app in \
             System Settings → Privacy & Security → Full Disk Access, then try again.",
            db_path.display()
        );
    }
    format!("Cannot read the Messages database: {message}")
}

/// The largest `message.ROWID` that exists right now.
///
/// This is what the first poll records instead of returning history.
pub(crate) fn latest_rowid(config: &NativeConfig) -> Result<i64, String> {
    let connection = open_read_only(&config.db_path)?;
    connection
        .query_row("SELECT IFNULL(MAX(ROWID), 0) FROM message", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("Cannot read the Messages database: {error}"))
}

/// Confirm the database is readable and report the account it belongs to.
///
/// Returns the number of known handles, which is a cheap, non-sensitive way
/// of saying "this is a real, populated Messages database" without reading a
/// single message.
pub(crate) fn probe(config: &NativeConfig) -> Result<u64, String> {
    let connection = open_read_only(&config.db_path)?;
    connection
        .query_row("SELECT COUNT(*) FROM handle", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count.max(0) as u64)
        .map_err(|error| format!("Cannot read the Messages database: {error}"))
}

/// Read every inbound message newer than `cursor`.
///
/// Messages this account sent (`is_from_me = 1`) are skipped: an agent
/// answering its own outbound message is a loop, and the gate downstream
/// should never have to be the thing that catches it.
pub(crate) fn poll_since(config: &NativeConfig, cursor: i64) -> Result<NativeBatch, String> {
    let connection = open_read_only(&config.db_path)?;
    let mut statement = connection
        .prepare(
            "SELECT m.ROWID, m.guid, m.text, m.attributedBody, m.date, \
                    h.id, c.chat_identifier, c.style, c.display_name, m.thread_originator_guid \
             FROM message m \
             LEFT JOIN handle h ON h.ROWID = m.handle_id \
             LEFT JOIN chat_message_join cmj ON cmj.message_id = m.ROWID \
             LEFT JOIN chat c ON c.ROWID = cmj.chat_id \
             WHERE m.ROWID > ?1 AND m.is_from_me = 0 \
             ORDER BY m.ROWID ASC LIMIT ?2",
        )
        .map_err(|error| format!("Cannot read the Messages database: {error}"))?;

    let mut highest = cursor;
    let mut envelopes = Vec::new();
    let rows = statement
        .query_map(rusqlite::params![cursor, MAX_ROWS_PER_POLL as i64], |row| {
            Ok(MessageRow {
                rowid: row.get(0)?,
                guid: row.get::<_, Option<String>>(1)?,
                text: row.get::<_, Option<String>>(2)?,
                attributed_body: row.get::<_, Option<Vec<u8>>>(3)?,
                date: row.get::<_, Option<i64>>(4)?.unwrap_or_default(),
                handle: row.get::<_, Option<String>>(5)?,
                chat_identifier: row.get::<_, Option<String>>(6)?,
                chat_style: row.get::<_, Option<i64>>(7)?,
                display_name: row.get::<_, Option<String>>(8)?,
                thread_originator_guid: row.get::<_, Option<String>>(9)?,
            })
        })
        .map_err(|error| format!("Cannot read the Messages database: {error}"))?;

    for row in rows {
        let row = row.map_err(|error| format!("Cannot read a Messages row: {error}"))?;
        highest = highest.max(row.rowid);
        let attachments = read_attachments(&connection, row.rowid)?;
        if let Some(envelope) = normalize(row, attachments) {
            envelopes.push(envelope);
        }
    }

    Ok(NativeBatch {
        envelopes,
        cursor: highest,
    })
}

struct MessageRow {
    rowid: i64,
    guid: Option<String>,
    text: Option<String>,
    attributed_body: Option<Vec<u8>>,
    date: i64,
    handle: Option<String>,
    chat_identifier: Option<String>,
    chat_style: Option<i64>,
    display_name: Option<String>,
    thread_originator_guid: Option<String>,
}

/// Attachments recorded against one message.
///
/// The file stays where Messages put it — nothing is copied out here. The
/// path travels as a provider handle, which is what the attachment pipeline
/// resolves under its own size and type limits.
fn read_attachments(
    connection: &Connection,
    message_rowid: i64,
) -> Result<Vec<ChannelAttachment>, String> {
    let mut statement = connection
        .prepare(
            "SELECT a.filename, a.mime_type, a.transfer_name, a.total_bytes \
             FROM attachment a \
             JOIN message_attachment_join maj ON maj.attachment_id = a.ROWID \
             WHERE maj.message_id = ?1",
        )
        .map_err(|error| format!("Cannot read Messages attachments: {error}"))?;
    let rows = statement
        .query_map([message_rowid], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(|error| format!("Cannot read Messages attachments: {error}"))?;

    let mut attachments = Vec::new();
    for row in rows {
        let (filename, mime_type, transfer_name, total_bytes) =
            row.map_err(|error| format!("Cannot read a Messages attachment: {error}"))?;
        let Some(path) = filename else { continue };
        let kind = mime_type
            .as_deref()
            .map(AttachmentKind::from_mime)
            .unwrap_or(AttachmentKind::Other);
        attachments.push(ChannelAttachment {
            provider_id: None,
            kind,
            filename: transfer_name,
            mime_type,
            declared_size_bytes: total_bytes.and_then(|bytes| u64::try_from(bytes).ok()),
            source: AttachmentSource::ProviderHandle { handle: path },
            stored_artifact_id: None,
            fetch_error: None,
            text_excerpt: None,
        });
    }
    Ok(attachments)
}

/// One database row as a normalized envelope, or `None` when there is
/// nothing to deliver (no text, no attachments, or no sender to attribute it
/// to — a row with a NULL handle is Messages' own bookkeeping, not a turn).
fn normalize(row: MessageRow, attachments: Vec<ChannelAttachment>) -> Option<ChannelEnvelope> {
    let sender_id = row.handle?;
    let text = row
        .text
        .filter(|value| !value.is_empty())
        .or_else(|| row.attributed_body.as_deref().and_then(typedstream_text))
        .unwrap_or_default();
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    let is_group = row.chat_style == Some(CHAT_STYLE_GROUP);
    let conversation = match (&row.chat_identifier, is_group) {
        (Some(chat_id), true) => ChannelConversation::group(chat_id.clone()),
        _ => ChannelConversation::direct(sender_id.clone()),
    };

    // `guid` is Messages' own stable identifier and is what dedupe wants.
    // The ROWID fallback is deterministic too — a row keeps its ROWID — and
    // is never random.
    let provider_event_id = row
        .guid
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("rowid:{}", row.rowid));

    Some(ChannelEnvelope {
        account_id: String::new(),
        kind: ChannelKind::IMessage,
        provider_event_id,
        conversation,
        sender: ChannelSender {
            sender_id,
            display_label: row.display_name.filter(|value| !value.is_empty()),
            is_self: false,
            is_bot: false,
        },
        text,
        attachments,
        // Messages records a real reply as the originating message's GUID,
        // which is the same identifier space `provider_event_id` uses.
        reply_to_provider_id: row.thread_originator_guid.filter(|value| !value.is_empty()),
        // Messages has no mention metadata of its own. Group activation
        // falls back to the gate's own text matching rather than claiming a
        // signal that does not exist.
        mentions_self: false,
        received_at_ms: apple_date_to_unix_ms(row.date),
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

/// Apple's `message.date` as Unix milliseconds.
///
/// The column changed units in macOS 10.13: it was seconds since the Apple
/// epoch and is now nanoseconds. Both still turn up — an upgraded Mac keeps
/// its old rows — so the unit is decided per value rather than per machine.
/// A nanosecond count is at least 11 digits for any date after 2001; a
/// second count will not reach that until the year 5138.
fn apple_date_to_unix_ms(date: i64) -> i64 {
    if date == 0 {
        return 0;
    }
    if date.abs() > 100_000_000_000 {
        // Nanoseconds since the Apple epoch.
        (date / 1_000_000) + APPLE_EPOCH_OFFSET_SECONDS * 1_000
    } else {
        (date + APPLE_EPOCH_OFFSET_SECONDS) * 1_000
    }
}

/// Pull the message body out of an archived `NSAttributedString`.
///
/// The blob is a typedstream: a class name (`NSString`), then the string's
/// own bytes behind a length. The length is one byte under 128, and
/// otherwise a marker (`0x81`/`0x82`/`0x83`) naming how many little-endian
/// bytes carry it. Anything that does not parse yields `None`, which reads
/// as "no text" — never a panic, and never a guess at a partial body.
fn typedstream_text(blob: &[u8]) -> Option<String> {
    let marker = b"NSString";
    let start = blob
        .windows(marker.len())
        .position(|window| window == marker)?
        + marker.len();
    // The archive writes a class version and a `+` (0x2B) tag between the
    // class name and the payload. Scan the short run after the name for it
    // rather than assuming a fixed offset, which differs across macOS
    // versions.
    let plus = blob
        .iter()
        .skip(start)
        .take(16)
        .position(|byte| *byte == 0x2B)?
        + start
        + 1;
    let (length, value_start) = match *blob.get(plus)? {
        0x81 => (
            u16::from_le_bytes([*blob.get(plus + 1)?, *blob.get(plus + 2)?]) as usize,
            plus + 3,
        ),
        0x82 => (
            u32::from_le_bytes([
                *blob.get(plus + 1)?,
                *blob.get(plus + 2)?,
                *blob.get(plus + 3)?,
                0,
            ]) as usize,
            plus + 4,
        ),
        0x83 => (
            u32::from_le_bytes([
                *blob.get(plus + 1)?,
                *blob.get(plus + 2)?,
                *blob.get(plus + 3)?,
                *blob.get(plus + 4)?,
            ]) as usize,
            plus + 5,
        ),
        short if short < 0x80 => (short as usize, plus + 1),
        _ => return None,
    };
    let value = blob.get(value_start..value_start.checked_add(length)?)?;
    String::from_utf8(value.to_vec())
        .ok()
        .filter(|text| !text.is_empty())
}

/// What happened to one send attempt.
pub(crate) enum NativeSend {
    Sent,
    /// Messages refused it outright — a handle it cannot reach, no signed-in
    /// account, Automation permission not granted.
    Refused(String),
    /// The script never ran, or never finished. It may or may not have
    /// delivered, so the caller must reconcile rather than retry.
    Ambiguous(String),
}

/// Send one message through Messages.app.
///
/// `target` is a handle (phone number or Apple ID) for a direct message, or
/// a chat GUID for a group. The text is passed as an argument and is never
/// part of the script.
pub(crate) async fn send(
    config: &NativeConfig,
    target: &str,
    text: &str,
    is_group: bool,
) -> NativeSend {
    if !config.osascript_path.exists() {
        return NativeSend::Refused(format!(
            "{} does not exist; iMessage sending needs macOS's own osascript",
            config.osascript_path.display()
        ));
    }
    let mut command = tokio::process::Command::new(&config.osascript_path);
    command
        .arg("-")
        .arg(target)
        .arg(text)
        .arg(if is_group { "1" } else { "0" })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return NativeSend::Refused(format!("Could not run osascript: {error}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(error) = stdin.write_all(SEND_SCRIPT.as_bytes()).await {
            return NativeSend::Ambiguous(format!(
                "Could not hand the script to osascript: {error}"
            ));
        }
        drop(stdin);
    }

    match tokio::time::timeout(SEND_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => NativeSend::Sent,
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            NativeSend::Refused(format!(
                "Messages refused the send: {}",
                first_line(detail.trim())
            ))
        }
        Ok(Err(error)) => {
            NativeSend::Ambiguous(format!("osascript could not be waited on: {error}"))
        }
        Err(_) => NativeSend::Ambiguous(
            "Messages did not answer in time; the message may or may not have been sent"
                .to_string(),
        ),
    }
}

/// One line of a subprocess's stderr, bounded. AppleScript errors can carry
/// a whole script listing, and none of that belongs in an account's health.
fn first_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or("no detail").trim();
    if line.len() > 200 {
        format!("{}…", &line[..200])
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the subset of the Messages schema this module reads. The real
    /// database has ~70 columns on `message` alone; these are the ones any
    /// of this code touches, with the same names and types.
    fn build_db(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("create db");
        connection
            .execute_batch(
                "CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT, service TEXT);
                 CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, guid TEXT, chat_identifier TEXT,
                                    display_name TEXT, style INTEGER);
                 CREATE TABLE message (ROWID INTEGER PRIMARY KEY, guid TEXT, text TEXT,
                                       attributedBody BLOB, date INTEGER, handle_id INTEGER,
                                       is_from_me INTEGER DEFAULT 0,
                                       thread_originator_guid TEXT);
                 CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
                 CREATE TABLE attachment (ROWID INTEGER PRIMARY KEY, filename TEXT, mime_type TEXT,
                                          transfer_name TEXT, total_bytes INTEGER);
                 CREATE TABLE message_attachment_join (message_id INTEGER, attachment_id INTEGER);
                 INSERT INTO handle (ROWID, id, service) VALUES (1, '+15551230001', 'iMessage');
                 INSERT INTO handle (ROWID, id, service) VALUES (2, 'ada@example.com', 'iMessage');
                 INSERT INTO chat (ROWID, guid, chat_identifier, display_name, style)
                     VALUES (1, 'iMessage;-;+15551230001', '+15551230001', NULL, 45);
                 INSERT INTO chat (ROWID, guid, chat_identifier, display_name, style)
                     VALUES (2, 'iMessage;+;chat9001', 'chat9001', 'Weekend plans', 43);",
            )
            .expect("schema");
        connection
    }

    fn temp_db() -> (PathBuf, Connection) {
        let path = std::env::temp_dir().join(format!(
            "monkey-chatdb-{}.sqlite",
            uuid::Uuid::new_v4().simple()
        ));
        let connection = build_db(&path);
        (path, connection)
    }

    fn config_for(path: &Path) -> NativeConfig {
        NativeConfig {
            db_path: path.to_path_buf(),
            osascript_path: PathBuf::from("/usr/bin/osascript"),
        }
    }

    /// One Apple-epoch nanosecond timestamp for 2024-01-01T00:00:00Z.
    const APPLE_NS_2024: i64 = 725_760_000_000_000_000;

    #[test]
    fn reads_a_direct_message_and_converts_apple_time() {
        let (path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                 VALUES (1, 'GUID-1', 'hello there', ?1, 1, 0)",
                [APPLE_NS_2024],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chat_message_join (chat_id, message_id) VALUES (1, 1)",
                [],
            )
            .unwrap();

        let batch = poll_since(&config_for(&path), 0).expect("poll");
        assert_eq!(batch.envelopes.len(), 1);
        let envelope = &batch.envelopes[0];
        assert_eq!(envelope.provider_event_id, "GUID-1");
        assert_eq!(envelope.text, "hello there");
        assert_eq!(envelope.sender.sender_id, "+15551230001");
        assert_eq!(envelope.conversation.conversation_id, "+15551230001");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        );
        // 2024-01-01T00:00:00Z
        assert_eq!(envelope.received_at_ms, 1_704_067_200_000);
        assert_eq!(batch.cursor, 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_group_chat_is_keyed_on_the_chat_not_the_sender() {
        let (path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                 VALUES (5, 'GUID-5', 'hi all', ?1, 2, 0)",
                [APPLE_NS_2024],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chat_message_join (chat_id, message_id) VALUES (2, 5)",
                [],
            )
            .unwrap();

        let batch = poll_since(&config_for(&path), 0).expect("poll");
        let envelope = &batch.envelopes[0];
        assert_eq!(envelope.conversation.conversation_id, "chat9001");
        assert_eq!(
            envelope.conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Group
        );
        assert_eq!(envelope.sender.sender_id, "ada@example.com");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn our_own_outbound_messages_are_never_read_back_in() {
        let (path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                 VALUES (7, 'GUID-7', 'this is mine', ?1, 1, 1)",
                [APPLE_NS_2024],
            )
            .unwrap();

        let batch = poll_since(&config_for(&path), 0).expect("poll");
        assert!(
            batch.envelopes.is_empty(),
            "an echo of our own send is a loop"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_cursor_advances_so_the_same_message_is_read_once() {
        let (path, connection) = temp_db();
        for rowid in 1..=3 {
            connection
                .execute(
                    "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                     VALUES (?1, ?2, 'ping', ?3, 1, 0)",
                    rusqlite::params![rowid, format!("GUID-{rowid}"), APPLE_NS_2024],
                )
                .unwrap();
        }

        let config = config_for(&path);
        let first = poll_since(&config, 0).expect("poll");
        assert_eq!(first.envelopes.len(), 3);
        assert_eq!(first.cursor, 3);
        let second = poll_since(&config, first.cursor).expect("poll");
        assert!(second.envelopes.is_empty());
        assert_eq!(second.cursor, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_attachment_travels_as_the_path_messages_already_stored() {
        let (path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                 VALUES (9, 'GUID-9', '', ?1, 1, 0)",
                [APPLE_NS_2024],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO attachment (ROWID, filename, mime_type, transfer_name, total_bytes)
                 VALUES (1, '~/Library/Messages/Attachments/aa/photo.png', 'image/png', 'photo.png', 4096)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (9, 1)",
                [],
            )
            .unwrap();

        let batch = poll_since(&config_for(&path), 0).expect("poll");
        assert_eq!(
            batch.envelopes.len(),
            1,
            "an attachment alone is still a turn"
        );
        let attachment = &batch.envelopes[0].attachments[0];
        assert_eq!(attachment.kind, AttachmentKind::Image);
        assert_eq!(attachment.declared_size_bytes, Some(4096));
        match &attachment.source {
            AttachmentSource::ProviderHandle { handle } => {
                assert!(handle.ends_with("photo.png"));
            }
            other => panic!("expected a provider handle, got {other:?}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reads_the_body_out_of_an_attributed_string_when_text_is_null() {
        // The shape macOS writes: the class name, the archive's `+` tag, a
        // one-byte length, then the UTF-8 body.
        let mut blob: Vec<u8> = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01\x40\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01\x2b".to_vec();
        let body = "sent from a newer macOS";
        blob.push(body.len() as u8);
        blob.extend_from_slice(body.as_bytes());

        let (path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, attributedBody, date, handle_id, is_from_me)
                 VALUES (11, 'GUID-11', NULL, ?1, ?2, 1, 0)",
                rusqlite::params![blob, APPLE_NS_2024],
            )
            .unwrap();

        let batch = poll_since(&config_for(&path), 0).expect("poll");
        assert_eq!(batch.envelopes[0].text, body);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_body_that_does_not_parse_is_no_text_rather_than_a_panic() {
        assert_eq!(typedstream_text(b""), None);
        assert_eq!(typedstream_text(b"NSString"), None);
        // A length that runs past the end of the blob.
        assert_eq!(
            typedstream_text(b"NSString\x01\x94\x84\x01\x2b\x40short"),
            None
        );
    }

    #[test]
    fn seconds_and_nanoseconds_both_convert() {
        // Same instant, both units.
        assert_eq!(apple_date_to_unix_ms(725_760_000), 1_704_067_200_000);
        assert_eq!(apple_date_to_unix_ms(APPLE_NS_2024), 1_704_067_200_000);
        assert_eq!(apple_date_to_unix_ms(0), 0);
    }

    #[test]
    fn a_missing_database_names_the_permission_rather_than_the_error() {
        let missing = std::env::temp_dir().join("monkey-no-such-chat.db");
        let error = probe(&config_for(&missing)).expect_err("must fail");
        assert!(error.contains("Sign in to Messages"), "{error}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_message_text_is_an_argument_never_part_of_the_script() {
        use std::os::unix::fs::PermissionsExt;
        // A fake osascript that records its argv. If the text were ever
        // interpolated into the script, it would arrive on stdin instead of
        // as an argument — and a body containing a quote would break it.
        let recorded = std::env::temp_dir().join(format!(
            "monkey-osascript-argv-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let fake = std::env::temp_dir().join(format!(
            "monkey-fake-osascript-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ncat > /dev/null\nfor arg in \"$@\"; do /bin/echo \"$arg\" >> {}; done\n",
                recorded.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = NativeConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            osascript_path: fake.clone(),
        };
        let hostile = "\"; tell application \\\"Finder\\\" to empty trash --";
        assert!(matches!(
            send(&config, "+15551230001", hostile, false).await,
            NativeSend::Sent
        ));

        let argv = std::fs::read_to_string(&recorded).unwrap();
        let lines: Vec<&str> = argv.lines().collect();
        assert_eq!(lines, vec!["-", "+15551230001", hostile, "0"]);
        let _ = std::fs::remove_file(&fake);
        let _ = std::fs::remove_file(&recorded);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_refusal_from_messages_is_reported_not_retried_blindly() {
        use std::os::unix::fs::PermissionsExt;
        let fake = std::env::temp_dir().join(format!(
            "monkey-fake-osascript-fail-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(
            &fake,
            "#!/bin/sh\ncat > /dev/null\n/bin/echo 'execution error: Not authorized to send Apple events to Messages. (-1743)' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = NativeConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            osascript_path: fake.clone(),
        };
        match send(&config, "+15551230001", "hi", false).await {
            NativeSend::Refused(detail) => assert!(detail.contains("-1743"), "{detail}"),
            _ => panic!("a refusal must not read as sent or ambiguous"),
        }
        let _ = std::fs::remove_file(&fake);
    }

    #[tokio::test]
    async fn a_missing_osascript_refuses_without_spawning_anything() {
        let config = NativeConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            osascript_path: std::env::temp_dir().join("definitely-not-osascript"),
        };
        match send(&config, "+1555", "hi", false).await {
            NativeSend::Refused(detail) => assert!(detail.contains("osascript"), "{detail}"),
            _ => panic!("expected a refusal"),
        }
    }
}

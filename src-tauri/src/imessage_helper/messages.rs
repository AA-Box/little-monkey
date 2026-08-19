//! The helper's macOS integration: read the Messages database, send through
//! Messages.app, hand back attachment bytes.
//!
//! This is the *only* code in the tree that touches Messages, and it runs in
//! the helper process the operator installs and grants permissions to — never
//! in the daemon. The daemon speaks JSON-RPC to `main.rs` and receives plain
//! records; it holds no Full Disk Access, opens no `chat.db`, and sends no
//! Apple events.
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
//! # Attachments leave by handle, never by path
//!
//! This process holds Full Disk Access, so a `read_attachment(path)` would be
//! an arbitrary-file reader for whatever can reach the helper's stdin. Instead
//! [`read_attachments`] issues an opaque handle — Messages' own attachment row
//! id — and [`read_attachment`] resolves it here, against the database, and
//! refuses anything whose canonical path is not inside Messages' own attachment
//! store.
//!
//! # Outbound: `osascript`
//!
//! Sending uses AppleScript's Messages dictionary, run as an argument
//! vector: the script is a fixed constant with `on run argv`, and the
//! recipient and the message text arrive as *arguments*. Message text is
//! never interpolated into script source — that would be command injection
//! into `osascript` from whatever a stranger typed. Sending needs the
//! Automation permission for Messages.app, again a normal macOS prompt.
//!
//! # Permissions are measured, never assumed
//!
//! [`probe`] reports three separate capabilities. `/usr/bin/osascript`
//! existing says nothing about whether this process may drive Messages, so the
//! Automation grant is checked by actually sending Messages a read-only Apple
//! event — which makes macOS run the authorization check without putting a test
//! message in anybody's conversation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

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

/// AppleScript run with `osascript <script>`, taking no arguments.
///
/// Reads two counts out of Messages and sends nothing. Its purpose is as much
/// the *attempt* as the answer: any Apple event to Messages.app makes macOS
/// perform the Automation authorization check, so a script that only reads is
/// the honest way to find out whether sending would be permitted — without
/// putting a test message in somebody's conversation.
///
/// `accounts` distinguishes "Messages has no account at all" from "Messages is
/// signed in"; the iMessage-typed count is the one that matters for sending,
/// and falls back to the total on a macOS whose dictionary spells the filter
/// differently rather than failing the whole probe.
const PROBE_SCRIPT: &str = r#"on run
    tell application "Messages"
        set total to count of accounts
        set usable to total
        try
            set usable to count of (every account whose service type is iMessage)
        end try
        return "accounts:" & (total as text) & " imessage:" & (usable as text)
    end tell
end run
"#;

/// How long the read-only capability probe gets. Bounded because the first
/// Apple event to Messages can raise the Automation prompt, and a health check
/// must not sit behind a dialog forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Where the Messages database, its attachment store and the script runner
/// live.
///
/// All three are overridable so the tests can drive a database they built, an
/// attachment directory they own, and an `osascript` that records what it was
/// handed. None of them is a way to run an arbitrary command with arbitrary
/// text: the scripts are still this module's own constants, and the runner is
/// still invoked as an argument vector. `attachments_root` in particular is a
/// *narrowing* control, not a widening one — see [`read_attachment`].
#[derive(Debug, Clone)]
pub struct MessagesConfig {
    pub db_path: PathBuf,
    pub attachments_root: PathBuf,
    pub osascript_path: PathBuf,
}

impl Default for MessagesConfig {
    fn default() -> Self {
        let db_path = default_db_path();
        Self {
            attachments_root: default_attachments_root(&db_path),
            db_path,
            osascript_path: PathBuf::from("/usr/bin/osascript"),
        }
    }
}

/// The only directory attachment bytes are ever read from: Messages' own
/// attachment store, beside the database it is recorded in.
fn default_attachments_root(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .join("Attachments")
}

impl MessagesConfig {
    /// The stock locations, plus whatever the command line overrode.
    ///
    /// Both overrides exist so this helper can be exercised against a database
    /// a test built and a script runner that records its argv. Neither is a way
    /// to run an arbitrary command with arbitrary text: the AppleScript is
    /// still this module's own constant and is still invoked as an argument
    /// vector.
    pub fn from_args(args: &[String]) -> Self {
        let mut config = Self::default();
        let mut explicit_attachments_root = false;
        let mut pairs = args.windows(2);
        while let Some([flag, value]) = pairs.next() {
            match flag.as_str() {
                "--db-path" => config.db_path = PathBuf::from(value),
                "--attachments-root" => {
                    config.attachments_root = PathBuf::from(value);
                    explicit_attachments_root = true;
                }
                "--osascript-path" => config.osascript_path = PathBuf::from(value),
                _ => {}
            }
        }
        // A moved database takes its attachment store with it, so the default
        // follows `--db-path` rather than staying pinned to the real Mac's.
        if !explicit_attachments_root {
            config.attachments_root = default_attachments_root(&config.db_path);
        }
        config
    }
}

fn default_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join("Library/Messages/chat.db")
}

/// One inbound batch plus the ROWID to resume from.
///
/// The records are deliberately plain data, not `ChannelEnvelope`s: normalizing
/// into the common envelope is the daemon's job, and the helper's contract is a
/// stable JSON shape rather than an internal Rust type.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Batch {
    pub messages: Vec<MessageRecord>,
    pub cursor: i64,
}

/// One message as the helper reports it.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageRecord {
    /// Messages' own stable identifier, and the daemon's dedupe key.
    pub guid: String,
    /// The row this came from, which is what the cursor advances over.
    pub rowid: i64,
    pub sender: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chat_id: Option<String>,
    pub is_group: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub text: String,
    /// Unix milliseconds.
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to_guid: Option<String>,
    pub attachments: Vec<AttachmentRecord>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentRecord {
    /// An opaque handle this helper issued, and the only thing the daemon ever
    /// learns about where the file is.
    ///
    /// It is Messages' own `attachment.ROWID` rendered as text, which has three
    /// properties that matter: the daemon cannot turn it into a path, a handle
    /// that names no row cannot be fetched at all, and it keeps working across
    /// a helper restart because the database — not this process's memory — is
    /// what resolves it.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
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
pub fn latest_rowid(config: &MessagesConfig) -> Result<i64, String> {
    let connection = open_read_only(&config.db_path)?;
    connection
        .query_row("SELECT IFNULL(MAX(ROWID), 0) FROM message", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| format!("Cannot read the Messages database: {error}"))
}

/// What this Mac can actually do, capability by capability.
///
/// Three separate facts, because they fail separately and are fixed
/// separately: Full Disk Access makes the database readable, Automation for
/// Messages.app makes sending possible, and a signed-in account is what either
/// of them is for. A single boolean would collapse three different System
/// Settings panes into one unactionable "not working".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    pub database_readable: bool,
    pub automation_authorized: bool,
    pub messages_available: bool,
    /// Known handles, once the database is readable: a cheap, non-sensitive way
    /// of saying "this is a real, populated Messages database" without reading
    /// a single message.
    pub handles: u64,
    /// The sentence naming the first thing that is not working, if any.
    pub detail: Option<String>,
}

/// Measure every capability the iMessage channel needs.
///
/// Never returns an error: a capability that is missing *is* the answer, and
/// the caller has to be able to tell which one. Nothing here sends a message —
/// see [`PROBE_SCRIPT`] for how Automation is checked without one.
pub async fn probe(config: &MessagesConfig) -> Capabilities {
    let (database_readable, handles, database_detail) = match read_handle_count(config) {
        Ok(handles) => (true, handles, None),
        Err(error) => (false, 0, Some(error)),
    };
    let (automation_authorized, messages_available, messages_detail) =
        match probe_messages_app(config).await {
            AutomationProbe::Ready { imessage_accounts } => (
                true,
                imessage_accounts > 0,
                (imessage_accounts == 0).then(|| {
                    "Messages is running but has no iMessage account signed in. Sign in to \
                     Messages on this Mac."
                        .to_string()
                }),
            ),
            AutomationProbe::NotAuthorized(detail) => (false, false, Some(detail)),
            AutomationProbe::Unavailable(detail) => (false, false, Some(detail)),
        };
    Capabilities {
        database_readable,
        automation_authorized,
        messages_available,
        handles,
        // Reading is what the channel does most of, so its failure is named
        // first when both are broken.
        detail: database_detail.or(messages_detail),
    }
}

fn read_handle_count(config: &MessagesConfig) -> Result<u64, String> {
    let connection = open_read_only(&config.db_path)?;
    connection
        .query_row("SELECT COUNT(*) FROM handle", [], |row| {
            row.get::<_, i64>(0)
        })
        .map(|count| count.max(0) as u64)
        .map_err(|error| format!("Cannot read the Messages database: {error}"))
}

/// What asking Messages.app a harmless question said about this Mac.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AutomationProbe {
    /// The Apple event went through, so Automation is granted.
    Ready { imessage_accounts: u64 },
    /// macOS refused the Apple event: the Automation grant is missing.
    NotAuthorized(String),
    /// The event could not be attempted, or Messages could not answer it.
    Unavailable(String),
}

/// Ask Messages.app for two counts, and read the answer *and* the refusal.
///
/// This is the whole difference between reporting a real capability and
/// reporting that a file exists: `/usr/bin/osascript` is present on every Mac
/// ever made, and proves nothing about whether this process may drive Messages.
async fn probe_messages_app(config: &MessagesConfig) -> AutomationProbe {
    if !config.osascript_path.exists() {
        return AutomationProbe::Unavailable(format!(
            "{} does not exist; iMessage sending needs macOS's own osascript",
            config.osascript_path.display()
        ));
    }
    let mut command = tokio::process::Command::new(&config.osascript_path);
    command
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return AutomationProbe::Unavailable(format!("Could not run osascript: {error}"))
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(error) = stdin.write_all(PROBE_SCRIPT.as_bytes()).await {
            return AutomationProbe::Unavailable(format!(
                "Could not hand the script to osascript: {error}"
            ));
        }
        drop(stdin);
    }
    match tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => {
            let answer = String::from_utf8_lossy(&output.stdout);
            AutomationProbe::Ready {
                imessage_accounts: parse_probe_count(&answer, "imessage:").unwrap_or(0),
            }
        }
        Ok(Ok(output)) => classify_script_failure(&String::from_utf8_lossy(&output.stderr)),
        Ok(Err(error)) => {
            AutomationProbe::Unavailable(format!("osascript could not be waited on: {error}"))
        }
        Err(_) => AutomationProbe::Unavailable(
            "Messages did not answer the permission check in time. If macOS is showing an \
             automation prompt, allow it and try again."
                .to_string(),
        ),
    }
}

/// `accounts:2 imessage:1` → the number after the named key.
fn parse_probe_count(answer: &str, key: &str) -> Option<u64> {
    answer
        .split_whitespace()
        .find_map(|token| token.strip_prefix(key)?.parse::<u64>().ok())
}

/// Tell "macOS refused this" apart from "Messages could not answer".
///
/// `-1743` is `errAEEventNotPermitted`, which is exactly the Automation grant
/// being absent or denied — the one failure that is fixed in System Settings
/// rather than in Messages.
fn classify_script_failure(stderr: &str) -> AutomationProbe {
    let detail = first_line(stderr.trim());
    if stderr.contains("-1743") || stderr.to_lowercase().contains("not authorized") {
        return AutomationProbe::NotAuthorized(
            "Little Monkey's iMessage helper is not allowed to control Messages. Allow it under \
             System Settings → Privacy & Security → Automation → Messages, then try again."
                .to_string(),
        );
    }
    AutomationProbe::Unavailable(format!("Messages could not answer: {detail}"))
}

/// Read every inbound message newer than `cursor`.
///
/// Messages this account sent (`is_from_me = 1`) are skipped: an agent
/// answering its own outbound message is a loop, and the gate downstream
/// should never have to be the thing that catches it.
pub fn poll_since(config: &MessagesConfig, cursor: i64) -> Result<Batch, String> {
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
    let mut messages = Vec::new();
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
        if let Some(record) = to_record(row, attachments) {
            messages.push(record);
        }
    }

    Ok(Batch {
        messages,
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
/// The file stays where Messages put it — nothing is copied out here, and the
/// path never leaves this process. What travels is the row's own id, which
/// [`read_attachment`] resolves back to a path under its own containment
/// check.
fn read_attachments(
    connection: &Connection,
    message_rowid: i64,
) -> Result<Vec<AttachmentRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT a.ROWID, a.filename, a.mime_type, a.transfer_name, a.total_bytes \
             FROM attachment a \
             JOIN message_attachment_join maj ON maj.attachment_id = a.ROWID \
             WHERE maj.message_id = ?1",
        )
        .map_err(|error| format!("Cannot read Messages attachments: {error}"))?;
    let rows = statement
        .query_map([message_rowid], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<i64>>(4)?,
            ))
        })
        .map_err(|error| format!("Cannot read Messages attachments: {error}"))?;

    let mut attachments = Vec::new();
    for row in rows {
        let (rowid, filename, mime_type, transfer_name, total_bytes) =
            row.map_err(|error| format!("Cannot read a Messages attachment: {error}"))?;
        // A row with no filename has no bytes to hand over, so it is not an
        // attachment anybody can fetch.
        if filename.is_none() {
            continue;
        }
        attachments.push(AttachmentRecord {
            id: rowid.to_string(),
            mime_type,
            filename: transfer_name,
            size: total_bytes.and_then(|bytes| u64::try_from(bytes).ok()),
        });
    }
    Ok(attachments)
}

/// One database row as a reportable record, or `None` when there is nothing to
/// deliver (no text, no attachments, or no sender to attribute it to — a row
/// with a NULL handle is Messages' own bookkeeping, not a turn).
fn to_record(row: MessageRow, attachments: Vec<AttachmentRecord>) -> Option<MessageRecord> {
    let sender_id = row.handle?;
    let text = row
        .text
        .filter(|value| !value.is_empty())
        .or_else(|| row.attributed_body.as_deref().and_then(typedstream_text))
        .unwrap_or_default();
    if text.is_empty() && attachments.is_empty() {
        return None;
    }

    // `guid` is Messages' own stable identifier and is what dedupe wants. The
    // ROWID fallback is deterministic too — a row keeps its ROWID — and is
    // never random.
    let guid = row
        .guid
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("rowid:{}", row.rowid));

    Some(MessageRecord {
        guid,
        rowid: row.rowid,
        sender: sender_id,
        chat_id: row.chat_identifier,
        is_group: row.chat_style == Some(CHAT_STYLE_GROUP),
        display_name: row.display_name.filter(|value| !value.is_empty()),
        text,
        timestamp: apple_date_to_unix_ms(row.date),
        // Messages records a real reply as the originating message's GUID,
        // which is the same identifier space `guid` uses.
        reply_to_guid: row.thread_originator_guid.filter(|value| !value.is_empty()),
        attachments,
    })
}

/// Read one attachment's bytes off this machine, by the handle this helper
/// issued.
///
/// # Why this is not `read_attachment(path)`
///
/// This process holds Full Disk Access. A method that took a path would make it
/// an arbitrary-file reader for whatever can talk to its stdin — every file on
/// the Mac, including the ones the daemon is deliberately not allowed to open.
/// So the daemon never names a path. It names a handle, and the resolution runs
/// entirely here, behind three separate gates:
///
/// 1. the handle must be a Messages `attachment.ROWID` that exists — a handle
///    this helper never issued resolves to nothing;
/// 2. the path comes out of the database, never off the wire;
/// 3. the resolved path is canonicalized (which collapses `..` and follows any
///    symlink) and must still be inside Messages' own attachment store, so a
///    row doctored to point at `~/.ssh/id_ed25519` is refused rather than read.
///
/// The size cap is applied to the directory entry first, so an oversized file
/// costs a `stat` rather than its own size.
pub fn read_attachment(
    config: &MessagesConfig,
    handle: &str,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    let rowid: i64 = handle
        .parse()
        .map_err(|_| "That is not an iMessage attachment handle".to_string())?;
    let connection = open_read_only(&config.db_path)?;
    let stored: Option<String> = connection
        .query_row(
            "SELECT filename FROM attachment WHERE ROWID = ?1",
            [rowid],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|_| "This Mac's Messages has no such attachment".to_string())?;
    let stored = stored.ok_or_else(|| "That iMessage attachment has no file".to_string())?;

    let expanded = expand_tilde(&stored);
    let resolved = expanded
        .canonicalize()
        .map_err(|error| format!("That attachment is no longer readable: {error}"))?;
    // Compared against the canonical root, so a symlinked store still matches
    // and a symlink *out* of it still does not.
    let root = config
        .attachments_root
        .canonicalize()
        .map_err(|error| format!("Messages' attachment store is not readable: {error}"))?;
    if !resolved.starts_with(&root) {
        return Err("That file is not inside Messages' attachment store".to_string());
    }

    let metadata = std::fs::metadata(&resolved)
        .map_err(|error| format!("That attachment is no longer readable: {error}"))?;
    if !metadata.is_file() {
        return Err("That iMessage attachment is not a file".to_string());
    }
    if metadata.len() > max_bytes {
        return Err(format!(
            "The attachment is larger than the {max_bytes}-byte limit"
        ));
    }
    std::fs::read(&resolved).map_err(|error| format!("That attachment could not be read: {error}"))
}

/// `~/Library/...` as Messages writes it, resolved against this user's home.
fn expand_tilde(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
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
pub enum SendResult {
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
pub async fn send(config: &MessagesConfig, target: &str, text: &str, is_group: bool) -> SendResult {
    if !config.osascript_path.exists() {
        return SendResult::Refused(format!(
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
        Err(error) => return SendResult::Refused(format!("Could not run osascript: {error}")),
    };
    if let Some(mut stdin) = child.stdin.take() {
        use tokio::io::AsyncWriteExt;
        if let Err(error) = stdin.write_all(SEND_SCRIPT.as_bytes()).await {
            return SendResult::Ambiguous(format!(
                "Could not hand the script to osascript: {error}"
            ));
        }
        drop(stdin);
    }

    match tokio::time::timeout(SEND_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(output)) if output.status.success() => SendResult::Sent,
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            SendResult::Refused(format!(
                "Messages refused the send: {}",
                first_line(detail.trim())
            ))
        }
        Ok(Err(error)) => {
            SendResult::Ambiguous(format!("osascript could not be waited on: {error}"))
        }
        Err(_) => SendResult::Ambiguous(
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

    fn config_for(path: &Path) -> MessagesConfig {
        MessagesConfig {
            attachments_root: default_attachments_root(path),
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
        assert_eq!(batch.messages.len(), 1);
        let record = &batch.messages[0];
        assert_eq!(record.guid, "GUID-1");
        assert_eq!(record.text, "hello there");
        assert_eq!(record.sender, "+15551230001");
        assert_eq!(
            record.chat_id.as_deref().unwrap_or_default(),
            "+15551230001"
        );
        assert!(!record.is_group);
        // 2024-01-01T00:00:00Z
        assert_eq!(record.timestamp, 1_704_067_200_000);
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
        let record = &batch.messages[0];
        assert_eq!(record.chat_id.as_deref().unwrap_or_default(), "chat9001");
        assert!(record.is_group);
        assert_eq!(record.sender, "ada@example.com");
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
            batch.messages.is_empty(),
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
        assert_eq!(first.messages.len(), 3);
        assert_eq!(first.cursor, 3);
        let second = poll_since(&config, first.cursor).expect("poll");
        assert!(second.messages.is_empty());
        assert_eq!(second.cursor, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn an_attachment_travels_as_an_opaque_handle_never_a_path() {
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
            batch.messages.len(),
            1,
            "an attachment alone is still a turn"
        );
        let attachment = &batch.messages[0].attachments[0];
        assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
        assert_eq!(attachment.size, Some(4096));
        // The daemon learns a handle, never a path.
        assert_eq!(attachment.id, "1");
        assert!(!attachment.id.contains('/'), "a path leaked as a handle");
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
        assert_eq!(batch.messages[0].text, body);
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

    #[tokio::test]
    async fn a_missing_database_names_the_permission_rather_than_the_error() {
        let missing = std::env::temp_dir().join("monkey-no-such-chat.db");
        let capabilities = probe(&config_for(&missing)).await;
        assert!(!capabilities.database_readable);
        let detail = capabilities.detail.expect("a reason");
        assert!(detail.contains("Sign in to Messages"), "{detail}");
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

        let config = MessagesConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            attachments_root: std::env::temp_dir(),
            osascript_path: fake.clone(),
        };
        let hostile = "\"; tell application \\\"Finder\\\" to empty trash --";
        assert!(matches!(
            send(&config, "+15551230001", hostile, false).await,
            SendResult::Sent
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
            "#!/bin/sh\nwhile IFS= read -r _; do :; done\nprintf '%s\\n' 'execution error: Not authorized to send Apple events to Messages. (-1743)' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let config = MessagesConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            attachments_root: std::env::temp_dir(),
            osascript_path: fake.clone(),
        };
        match send(&config, "+15551230001", "hi", false).await {
            SendResult::Refused(detail) => assert!(detail.contains("-1743"), "{detail}"),
            _ => panic!("a refusal must not read as sent or ambiguous"),
        }
        let _ = std::fs::remove_file(&fake);
    }

    // -- the helper is not a file reader --------------------------------------
    //
    // This process holds Full Disk Access. Everything below is about the one
    // consequence: nothing the daemon can say may turn into "open this path".

    /// A database with one attachment row pointing at `stored`, plus a real
    /// file at `on_disk` inside an attachment store this test owns.
    fn db_with_attachment(stored: &str) -> (PathBuf, PathBuf, MessagesConfig) {
        let (db_path, connection) = temp_db();
        let root = std::env::temp_dir().join(format!(
            "monkey-attachments-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("attachment store");
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
                 VALUES (1, ?1, 'image/png', 'photo.png', 5)",
                [stored],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO message_attachment_join (message_id, attachment_id) VALUES (9, 1)",
                [],
            )
            .unwrap();
        let config = MessagesConfig {
            db_path: db_path.clone(),
            attachments_root: root.clone(),
            osascript_path: PathBuf::from("/usr/bin/osascript"),
        };
        (db_path, root, config)
    }

    #[test]
    fn a_handle_the_database_knows_resolves_to_its_bytes() {
        let root = std::env::temp_dir().join(format!(
            "monkey-attachments-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("store");
        let file = root.join("photo.png");
        std::fs::write(&file, b"bytes").expect("write");
        let (db_path, _, mut config) = db_with_attachment(&file.to_string_lossy());
        config.attachments_root = root.clone();

        assert_eq!(
            read_attachment(&config, "1", 1024).expect("readable"),
            b"bytes"
        );
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_handle_this_helper_never_issued_resolves_to_nothing() {
        let root = std::env::temp_dir().join(format!(
            "monkey-attachments-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("store");
        let file = root.join("photo.png");
        std::fs::write(&file, b"bytes").expect("write");
        let (db_path, _, mut config) = db_with_attachment(&file.to_string_lossy());
        config.attachments_root = root.clone();

        // A row that does not exist, and a handle that is not a row id at all.
        assert!(read_attachment(&config, "4242", 1024).is_err());
        assert!(read_attachment(&config, "/etc/passwd", 1024).is_err());
        assert!(read_attachment(&config, "../../etc/passwd", 1024).is_err());
        assert!(read_attachment(&config, "", 1024).is_err());
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_row_pointing_outside_the_attachment_store_is_refused() {
        // The path never came off the wire — it came out of the database — and
        // it is *still* refused. A doctored row is the last way an FDA process
        // could be talked into reading somebody's private file.
        let outside = std::env::temp_dir().join(format!(
            "monkey-outside-{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&outside, b"secret").expect("write");
        let (db_path, root, config) = db_with_attachment(&outside.to_string_lossy());

        let error = read_attachment(&config, "1", 1024).expect_err("must refuse");
        assert!(error.contains("not inside Messages"), "{error}");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_attachment_store_is_refused() {
        let outside = std::env::temp_dir().join(format!(
            "monkey-outside-{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&outside, b"secret").expect("write");
        let root = std::env::temp_dir().join(format!(
            "monkey-attachments-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("store");
        let link = root.join("photo.png");
        std::os::unix::fs::symlink(&outside, &link).expect("symlink");

        let (db_path, _, mut config) = db_with_attachment(&link.to_string_lossy());
        config.attachments_root = root.clone();

        // The stored path *is* inside the store. Canonicalizing is what catches
        // it: a prefix check on the un-resolved path would have passed.
        let error = read_attachment(&config, "1", 1024).expect_err("must refuse");
        assert!(error.contains("not inside Messages"), "{error}");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_traversal_inside_the_stored_path_is_collapsed_before_the_check() {
        let root = std::env::temp_dir().join(format!(
            "monkey-attachments-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(root.join("aa")).expect("store");
        let outside = std::env::temp_dir().join(format!(
            "monkey-outside-{}.txt",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(&outside, b"secret").expect("write");
        let climbing = format!(
            "{}/aa/../../{}",
            root.display(),
            outside.file_name().expect("name").to_string_lossy()
        );

        let (db_path, _, mut config) = db_with_attachment(&climbing);
        config.attachments_root = root.clone();
        let error = read_attachment(&config, "1", 1024).expect_err("must refuse");
        assert!(error.contains("not inside Messages"), "{error}");
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&root);
    }

    // -- capabilities are measured, never assumed -----------------------------

    /// A fake `osascript` that answers the probe script with `stdout`, or
    /// fails with `stderr` and a non-zero status.
    #[cfg(unix)]
    fn fake_osascript(name: &str, stdout: &str, stderr: &str, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!(
            "monkey-fake-osascript-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat > /dev/null\n/bin/echo '{stdout}'\n/bin/echo '{stderr}' >&2\nexit \
                 {code}\n"
            ),
        )
        .expect("write fake osascript");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn every_capability_is_reported_separately() {
        let (db_path, connection) = temp_db();
        connection
            .execute(
                "INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                 VALUES (1, 'GUID-1', 'hi', ?1, 1, 0)",
                [APPLE_NS_2024],
            )
            .unwrap();

        // Everything working.
        let working = fake_osascript("ok", "accounts:2 imessage:1", "", 0);
        let mut config = config_for(&db_path);
        config.osascript_path = working.clone();
        let capabilities = probe(&config).await;
        assert_eq!(
            capabilities,
            Capabilities {
                database_readable: true,
                automation_authorized: true,
                messages_available: true,
                handles: 2,
                detail: None,
            }
        );

        // Automation refused: `-1743` is macOS saying this process may not
        // drive Messages, which is a different pane from Full Disk Access.
        let refused = fake_osascript(
            "denied",
            "",
            "execution error: Not authorized to send Apple events to Messages. (-1743)",
            1,
        );
        config.osascript_path = refused.clone();
        let capabilities = probe(&config).await;
        assert!(capabilities.database_readable);
        assert!(!capabilities.automation_authorized);
        let detail = capabilities.detail.expect("a reason");
        assert!(detail.contains("Automation"), "{detail}");

        // Automation granted, nobody signed in.
        let empty = fake_osascript("no-account", "accounts:0 imessage:0", "", 0);
        config.osascript_path = empty.clone();
        let capabilities = probe(&config).await;
        assert!(capabilities.automation_authorized);
        assert!(!capabilities.messages_available);
        assert!(capabilities
            .detail
            .expect("a reason")
            .contains("Sign in to Messages"));

        // Full Disk Access missing: the database is what fails, and it is named
        // first even though Automation works.
        let mut missing_db = config.clone();
        missing_db.db_path = std::env::temp_dir().join("monkey-no-such-chat.db");
        missing_db.osascript_path = working.clone();
        let capabilities = probe(&missing_db).await;
        assert!(!capabilities.database_readable);
        assert!(capabilities.automation_authorized);

        for path in [working, refused, empty] {
            let _ = std::fs::remove_file(&path);
        }
        let _ = std::fs::remove_file(&db_path);
    }

    #[tokio::test]
    async fn a_missing_osascript_is_not_a_working_automation_grant() {
        // The exact false positive: `/usr/bin/osascript` existing was once
        // treated as proof that sending works.
        let (db_path, _) = temp_db();
        let mut config = config_for(&db_path);
        config.osascript_path = std::env::temp_dir().join("definitely-not-osascript");
        let capabilities = probe(&config).await;
        assert!(!capabilities.automation_authorized);
        let _ = std::fs::remove_file(&db_path);
    }

    #[test]
    fn the_probe_answer_is_parsed_by_key_not_by_position() {
        assert_eq!(
            parse_probe_count("accounts:3 imessage:2\n", "imessage:"),
            Some(2)
        );
        assert_eq!(
            parse_probe_count("accounts:3 imessage:2", "accounts:"),
            Some(3)
        );
        assert_eq!(parse_probe_count("nonsense", "imessage:"), None);
    }

    #[tokio::test]
    async fn a_missing_osascript_refuses_without_spawning_anything() {
        let config = MessagesConfig {
            db_path: std::env::temp_dir().join("unused.db"),
            attachments_root: std::env::temp_dir(),
            osascript_path: std::env::temp_dir().join("definitely-not-osascript"),
        };
        match send(&config, "+1555", "hi", false).await {
            SendResult::Refused(detail) => assert!(detail.contains("osascript"), "{detail}"),
            _ => panic!("expected a refusal"),
        }
    }
}

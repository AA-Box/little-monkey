//! `little-monkey-imessage-helper` — the macOS side of the iMessage channel.
//!
//! Little Monkey's daemon does not read the Messages database and does not send
//! Apple events. It cannot: the two permissions that make iMessage work — Full
//! Disk Access to read `~/Library/Messages/chat.db`, and Automation to drive
//! Messages.app — belong to *this* process, which the operator installs and
//! grants once. The daemon knows only where this binary is, how to keep it
//! running, and how to talk to it.
//!
//! Nothing here asks for an Apple ID or a password, and nothing here could use
//! one. The account is whichever one the user is already signed in to in
//! Messages, and that stays true whether this helper is running or not.
//!
//! # The protocol
//!
//! Newline-delimited JSON-RPC 2.0 on stdin/stdout. Every request carries an
//! `id` and every response quotes it back, so a slow call can never be mistaken
//! for the answer to a later one. One line in, one line out, in that order.
//!
//! ```text
//! version           → { "version": …, "platform": "macos" }
//! probe             → { "protocol": …, "databaseReadable": bool,
//!                       "automationAuthorized": bool,
//!                       "messagesAvailable": bool, "handles": n,
//!                       "detail": …|null }
//! poll   { since }  → { "cursor": rowid, "messages": [ … ] }
//! send   { target, text, isGroup } → { "sent": true }
//! fetchAttachment { handle, maxBytes } → { "base64": … }
//! shutdown          → { "ok": true }, then exit
//! ```
//!
//! `probe` measures three separate capabilities rather than reporting one
//! boolean, because they fail separately and are fixed in three different
//! places. It never sends a message: Automation is checked by asking Messages a
//! read-only question, which is enough to make macOS run the authorization
//! check.
//!
//! # The daemon never learns a path
//!
//! `poll` reports attachments by an opaque handle this helper issued, and
//! `fetchAttachment` takes that handle back. A method that accepted a *path*
//! would turn a process holding Full Disk Access into an arbitrary-file reader
//! for anything that can write to its stdin.
//!
//! `poll` with a null `since` reports the current maximum row and no messages:
//! connecting an account is not a reason to replay every conversation on the
//! Mac. The cursor lives in the daemon, not here, so a helper restart resumes
//! rather than re-reading.
//!
//! # Error codes
//!
//! The daemon has to tell three failures apart, because they mean three
//! different things to a queued reply:
//!
//! - [`CODE_REFUSED`] — it definitely did not happen. Permanent.
//! - [`CODE_AMBIGUOUS`] — it may have happened. Reconcile, never blind-retry.
//! - [`CODE_SETUP`] — a permission or an install is missing. Not an error to
//!   retry, a thing for a person to fix.
//!
//! # Nothing is ever interpolated into a script
//!
//! Message text reaches AppleScript as an element of `argv`, never as script
//! source — see [`messages`]. A body containing quotes is a body, not a
//! command.

#[cfg(target_os = "macos")]
mod messages;

/// It definitely did not happen.
#[cfg(target_os = "macos")]
const CODE_REFUSED: i64 = -32000;
/// It may have happened; the caller must reconcile rather than retry.
#[cfg(target_os = "macos")]
const CODE_AMBIGUOUS: i64 = -32001;
/// A permission or an install is missing.
#[cfg(target_os = "macos")]
const CODE_SETUP: i64 = -32002;
/// The request did not name a method this helper has.
#[cfg(target_os = "macos")]
const CODE_UNKNOWN_METHOD: i64 = -32601;

/// The helper's own protocol version, bumped when the shape above changes.
///
/// `2` replaced `probe`'s single `canSend` boolean with measured capabilities
/// and `fetchAttachment`'s `path` with an opaque handle. The daemon refuses to
/// speak to a helper that answers anything else rather than guessing at an
/// older shape — see `adapters/imessage.rs`.
#[cfg(target_os = "macos")]
const PROTOCOL_VERSION: &str = "2";

/// What one attachment may cost, unless the caller asks for less.
///
/// The daemon passes its own account-configured cap; this is the ceiling when
/// it does not, so a helper driven by hand cannot be talked into reading a disk
/// image into memory.
#[cfg(target_os = "macos")]
const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;

#[cfg(not(target_os = "macos"))]
fn main() {
    // Not a stub that pretends: there is no Messages database and no
    // Messages.app to automate anywhere else, and saying so on stderr with a
    // failing status is the whole honest behaviour.
    eprintln!("little-monkey-imessage-helper only runs on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = messages::MessagesConfig::from_args(&args);
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("little-monkey-imessage-helper could not start: {error}");
            std::process::exit(1);
        }
    };
    runtime.block_on(serve(config, tokio::io::stdin(), tokio::io::stdout()));
}

/// Read requests until stdin ends, answering each one in order.
///
/// Generic over its streams so the tests drive the real dispatch over a pipe
/// rather than over the process's own stdio.
#[cfg(target_os = "macos")]
async fn serve<R, W>(config: messages::MessagesConfig, input: R, mut output: W)
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut lines = BufReader::new(input).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let (response, stop) = handle_line(&config, &line).await;
        let Some(response) = response else {
            // A line with no id is a notification: there is nobody to answer,
            // and inventing an id would be answering a question nobody asked.
            if stop {
                return;
            }
            continue;
        };
        if output
            .write_all(format!("{response}\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        let _ = output.flush().await;
        if stop {
            return;
        }
    }
}

/// Handle one line: the JSON to write back (if any), and whether to stop.
#[cfg(target_os = "macos")]
async fn handle_line(
    config: &messages::MessagesConfig,
    line: &str,
) -> (Option<serde_json::Value>, bool) {
    let request: serde_json::Value = match serde_json::from_str(line) {
        Ok(request) => request,
        // Unparseable and therefore unattributable: there is no id to answer
        // with, so the only honest thing is to ignore it.
        Err(_) => return (None, false),
    };
    let Some(id) = request.get("id").cloned().filter(|id| !id.is_null()) else {
        return (None, false);
    };
    let method = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);

    if method == "shutdown" {
        return (Some(ok(&id, serde_json::json!({ "ok": true }))), true);
    }
    let outcome = dispatch(config, &method, &params).await;
    let response = match outcome {
        Ok(result) => ok(&id, result),
        Err((code, message)) => fail(&id, code, &message),
    };
    (Some(response), false)
}

#[cfg(target_os = "macos")]
async fn dispatch(
    config: &messages::MessagesConfig,
    method: &str,
    params: &serde_json::Value,
) -> Result<serde_json::Value, (i64, String)> {
    match method {
        "version" => Ok(serde_json::json!({
            "version": PROTOCOL_VERSION,
            "platform": "macos",
        })),
        // Never an error: a capability that is missing *is* the answer, and the
        // daemon has to be able to tell the three apart — they are three
        // different things for a person to fix.
        "probe" => {
            let capabilities = messages::probe(config).await;
            Ok(serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "databaseReadable": capabilities.database_readable,
                "automationAuthorized": capabilities.automation_authorized,
                "messagesAvailable": capabilities.messages_available,
                "handles": capabilities.handles,
                "detail": capabilities.detail,
            }))
        }
        "poll" => {
            let since = params.get("since").and_then(serde_json::Value::as_i64);
            let Some(since) = since else {
                // No cursor yet: record where the database is now and deliver
                // nothing. Connecting an account must not replay years of
                // conversations through an agent.
                let cursor = messages::latest_rowid(config).map_err(|error| (CODE_SETUP, error))?;
                return Ok(serde_json::json!({ "cursor": cursor, "messages": [] }));
            };
            let batch = messages::poll_since(config, since).map_err(|error| (CODE_SETUP, error))?;
            serde_json::to_value(batch).map_err(|error| (CODE_REFUSED, error.to_string()))
        }
        "send" => {
            let target = string_param(params, "target")?;
            let text = string_param(params, "text")?;
            let is_group = params
                .get("isGroup")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            match messages::send(config, &target, &text, is_group).await {
                messages::SendResult::Sent => Ok(serde_json::json!({ "sent": true })),
                messages::SendResult::Refused(error) => Err((CODE_REFUSED, error)),
                messages::SendResult::Ambiguous(error) => Err((CODE_AMBIGUOUS, error)),
            }
        }
        // The daemon names a handle this helper issued, never a path. See
        // `messages::read_attachment` for why a process holding Full Disk
        // Access must not accept one.
        "fetchAttachment" => {
            let handle = string_param(params, "handle")?;
            let max_bytes = params
                .get("maxBytes")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(MAX_ATTACHMENT_BYTES)
                .min(MAX_ATTACHMENT_BYTES);
            let bytes = messages::read_attachment(config, &handle, max_bytes)
                .map_err(|error| (CODE_REFUSED, error))?;
            Ok(serde_json::json!({
                "base64": base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    &bytes,
                ),
            }))
        }
        other => Err((
            CODE_UNKNOWN_METHOD,
            format!("this helper has no '{other}' method"),
        )),
    }
}

#[cfg(target_os = "macos")]
fn string_param(params: &serde_json::Value, name: &str) -> Result<String, (i64, String)> {
    params
        .get(name)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| (CODE_REFUSED, format!("'{name}' is required")))
}

#[cfg(target_os = "macos")]
fn ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

#[cfg(target_os = "macos")]
fn fail(id: &serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    fn config() -> messages::MessagesConfig {
        messages::MessagesConfig {
            db_path: std::env::temp_dir().join("no-such-chat.db"),
            attachments_root: std::env::temp_dir().join("no-such-attachments"),
            osascript_path: std::path::PathBuf::from("/usr/bin/osascript"),
        }
    }

    async fn answer(request: serde_json::Value) -> serde_json::Value {
        let (response, _) = handle_line(&config(), &request.to_string()).await;
        response.expect("a request with an id is always answered")
    }

    #[tokio::test]
    async fn every_answer_quotes_the_id_it_was_asked_with() {
        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "version"
        }))
        .await;
        assert_eq!(response["id"], 7);
        assert_eq!(response["result"]["platform"], "macos");
        assert_eq!(response["result"]["version"], PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn a_line_with_no_id_is_never_answered() {
        // Answering would mean inventing an id, and the daemon routes replies
        // by exactly that field.
        let (response, stop) = handle_line(
            &config(),
            &serde_json::json!({"method": "version"}).to_string(),
        )
        .await;
        assert!(response.is_none());
        assert!(!stop);
        let (response, _) = handle_line(&config(), "this is not json").await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn an_unknown_method_is_refused_by_name() {
        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "deleteEverything"
        }))
        .await;
        assert_eq!(response["error"]["code"], CODE_UNKNOWN_METHOD);
        assert!(response["error"]["message"]
            .as_str()
            .expect("message")
            .contains("deleteEverything"));
    }

    #[tokio::test]
    async fn a_probe_reports_capabilities_rather_than_failing() {
        // Which capability is missing is the answer, not an error: Full Disk
        // Access and Automation are two different things for a person to fix,
        // and one error code cannot say which.
        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "probe"
        }))
        .await;
        assert!(response.get("error").is_none(), "{response}");
        assert_eq!(response["result"]["protocol"], PROTOCOL_VERSION);
        assert_eq!(response["result"]["databaseReadable"], false);
        assert!(response["result"]["detail"]
            .as_str()
            .expect("a reason")
            .contains("Messages"));
    }

    #[tokio::test]
    async fn fetch_attachment_takes_a_handle_and_refuses_a_path() {
        // The whole point of the handle: this process holds Full Disk Access,
        // so a `path` parameter would make it a general file reader.
        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "fetchAttachment",
            "params": {"path": "/etc/passwd"}
        }))
        .await;
        assert_eq!(response["error"]["code"], CODE_REFUSED);
        assert!(response["error"]["message"]
            .as_str()
            .expect("message")
            .contains("handle"));

        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "fetchAttachment",
            "params": {"handle": "/etc/passwd"}
        }))
        .await;
        assert_eq!(response["error"]["code"], CODE_REFUSED);
    }

    #[tokio::test]
    async fn a_send_with_no_target_is_refused_rather_than_attempted() {
        let response = answer(serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "send", "params": {"text": "hi"}
        }))
        .await;
        assert_eq!(response["error"]["code"], CODE_REFUSED);
    }

    #[tokio::test]
    async fn shutdown_answers_before_it_stops() {
        let (response, stop) = handle_line(
            &config(),
            &serde_json::json!({"jsonrpc": "2.0", "id": 9, "method": "shutdown"}).to_string(),
        )
        .await;
        assert!(stop);
        assert_eq!(response.expect("answered")["result"]["ok"], true);
    }

    /// A database with two messages in it, so `poll` can be driven end to end.
    fn seeded_db() -> (std::path::PathBuf, messages::MessagesConfig) {
        let path = std::env::temp_dir().join(format!(
            "monkey-helper-chatdb-{}.sqlite",
            uuid::Uuid::new_v4().simple()
        ));
        let connection = rusqlite::Connection::open(&path).expect("create db");
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
                 INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                     VALUES (1, 'GUID-1', 'first', 725760000000000000, 1, 0);
                 INSERT INTO message (ROWID, guid, text, date, handle_id, is_from_me)
                     VALUES (2, 'GUID-2', 'second', 725760000000000000, 1, 0);",
            )
            .expect("schema");
        let config = messages::MessagesConfig {
            db_path: path.clone(),
            attachments_root: std::env::temp_dir().join("no-such-attachments"),
            osascript_path: std::path::PathBuf::from("/usr/bin/osascript"),
        };
        (path, config)
    }

    #[tokio::test]
    async fn a_first_poll_records_where_the_database_is_and_replays_nothing() {
        let (path, config) = seeded_db();
        let (response, _) = handle_line(
            &config,
            &serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "poll", "params": {}})
                .to_string(),
        )
        .await;
        let response = response.expect("answered");
        assert_eq!(response["result"]["cursor"], 2);
        assert_eq!(
            response["result"]["messages"]
                .as_array()
                .expect("messages")
                .len(),
            0,
            "connecting an account must not replay the Mac's history"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_poll_from_a_cursor_returns_only_what_came_after_it() {
        let (path, config) = seeded_db();
        let (response, _) = handle_line(
            &config,
            &serde_json::json!({
                "jsonrpc": "2.0", "id": 2, "method": "poll", "params": {"since": 1}
            })
            .to_string(),
        )
        .await;
        let response = response.expect("answered");
        let messages = response["result"]["messages"]
            .as_array()
            .expect("messages")
            .clone();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["guid"], "GUID-2");
        assert_eq!(messages[0]["rowid"], 2);
        assert_eq!(response["result"]["cursor"], 2);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn requests_are_answered_in_order_over_the_stream() {
        let (path, config) = seeded_db();
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"version"}"#,
            "\n",
            "not json at all\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"poll","params":{"since":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":3,"method":"shutdown"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":4,"method":"version"}"#,
            "\n",
        );
        let mut output = Vec::new();
        serve(config, input.as_bytes(), &mut output).await;

        let answers: Vec<serde_json::Value> = String::from_utf8(output)
            .expect("utf8")
            .lines()
            .map(|line| serde_json::from_str(line).expect("json"))
            .collect();
        let ids: Vec<i64> = answers
            .iter()
            .map(|answer| answer["id"].as_i64().expect("id"))
            .collect();
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "the junk line is skipped and nothing is answered after shutdown"
        );
        let _ = std::fs::remove_file(&path);
    }
}

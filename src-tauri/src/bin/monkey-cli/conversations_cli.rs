//! `monkey conversations` — conversations this installation holds that do not
//! live in the desktop app itself.
//!
//! The desktop's own chat sessions are the app's business and are not listed
//! here: they live in the app's session file, not in the daemon. What this
//! adds is everything the daemon owns that no surface could previously see
//! next to them: a paired phone's chat (`remote_control`) and a messaging
//! conversation the operator's agent is answering (`channel:<provider>`, e.g.
//! `channel:slack`).
//!
//! Unlike `monkey ingress`, this IS a transcript surface: `show` returns the
//! message text this machine durably recorded, because the point is to read a
//! conversation that happened somewhere else.

use crate::daemon::remote::store::RemoteStore;
use crate::daemon::store::{DaemonPaths, DaemonStore};

/// `monkey conversations <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum ConversationsCmd {
    /// Conversations outside the desktop app, newest first.
    List {
        /// Only this environment: `remote_control`, or `channel:<provider>`
        /// (`channel` on its own means every provider).
        #[arg(long)]
        environment: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// One conversation's messages, oldest first.
    Show {
        #[arg(long)]
        environment: String,
        /// The id `list` gave this conversation.
        #[arg(long)]
        id: String,
        #[arg(long, default_value_t = 500)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
}

/// A paired phone's chat with this installation. `local` — the desktop's own
/// sessions — is the app's own list and never comes from here.
const REMOTE_CONTROL: &str = "remote_control";
/// Prefix for a messaging conversation's environment; the provider token
/// follows (`channel:slack`), because two providers are two environments even
/// though one subsystem serves both.
const CHANNEL_PREFIX: &str = "channel:";

pub fn dispatch(command: &ConversationsCmd) -> Result<(), String> {
    match command {
        ConversationsCmd::List {
            environment,
            limit,
            json,
        } => list(environment.as_deref(), *limit, *json),
        ConversationsCmd::Show {
            environment,
            id,
            limit,
            json,
        } => show(environment, id, *limit, *json),
    }
}

/// Which halves of the listing an `--environment` value asks for.
struct Wanted {
    remote_control: bool,
    /// `None` means every provider, `Some(kind)` exactly one.
    channels: Option<Option<String>>,
}

fn wanted(environment: Option<&str>) -> Result<Wanted, String> {
    match environment {
        None => Ok(Wanted {
            remote_control: true,
            channels: Some(None),
        }),
        Some(REMOTE_CONTROL) => Ok(Wanted {
            remote_control: true,
            channels: None,
        }),
        Some("channel") => Ok(Wanted {
            remote_control: false,
            channels: Some(None),
        }),
        Some(value) if value.starts_with(CHANNEL_PREFIX) => {
            let provider = value[CHANNEL_PREFIX.len()..].to_string();
            if provider.is_empty() {
                return Err("A channel environment needs a provider, e.g. channel:slack".into());
            }
            Ok(Wanted {
                remote_control: false,
                channels: Some(Some(provider)),
            })
        }
        Some(other) => Err(format!(
            "Unknown environment '{other}' (expected {REMOTE_CONTROL} or channel:<provider>)"
        )),
    }
}

fn list(environment: Option<&str>, limit: u32, json: bool) -> Result<(), String> {
    let wanted = wanted(environment)?;
    let paths = DaemonPaths::resolve()?;
    let mut rows: Vec<serde_json::Value> = Vec::new();

    // A machine that has never paired a phone has no remote store at all.
    // That is an empty listing, not a failure: the desktop asks for this on
    // every sidebar refresh and must not see an error for "nothing yet".
    if wanted.remote_control {
        if let Ok(store) = RemoteStore::open(&paths.root) {
            for session in store.mobile_session_summaries()? {
                rows.push(serde_json::json!({
                    "environment": REMOTE_CONTROL,
                    "provider": serde_json::Value::Null,
                    "id": session.session_id,
                    "title": session.title,
                    "account_label": serde_json::Value::Null,
                    "updated_at_ms": session.updated_at_ms,
                    "message_count": session.message_count,
                }));
            }
        }
    }

    if let Some(provider) = wanted.channels {
        let store = DaemonStore::open(&paths)?;
        for conversation in store.channel_conversations(limit)? {
            if provider
                .as_deref()
                .is_some_and(|wanted| wanted != conversation.account_kind)
            {
                continue;
            }
            rows.push(serde_json::json!({
                "environment": format!("{CHANNEL_PREFIX}{}", conversation.account_kind),
                "provider": conversation.account_kind,
                "id": conversation.session_key,
                "title": conversation.title.unwrap_or(conversation.conversation_id),
                "account_label": conversation.account_label,
                "updated_at_ms": conversation.last_activity_ms,
                "message_count": conversation.message_count,
            }));
        }
    }

    rows.sort_by_key(|row| {
        std::cmp::Reverse(
            row.get("updated_at_ms")
                .and_then(|at| at.as_i64())
                .unwrap_or(0),
        )
    });
    rows.truncate(limit as usize);

    if json {
        println!("{}", serde_json::json!({ "conversations": rows }));
        return Ok(());
    }
    if rows.is_empty() {
        println!("No conversations outside this desktop yet.");
        return Ok(());
    }
    for row in &rows {
        println!(
            "{:<15} {:>4} msg  {}",
            row["environment"].as_str().unwrap_or(""),
            row["message_count"].as_u64().unwrap_or(0),
            row["title"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

fn show(environment: &str, id: &str, limit: u32, json: bool) -> Result<(), String> {
    let paths = DaemonPaths::resolve()?;
    let messages: Vec<serde_json::Value> = if environment == REMOTE_CONTROL {
        RemoteStore::open(&paths.root)?
            .mobile_messages(id, limit)?
            .into_iter()
            .map(|message| {
                serde_json::json!({
                    "role": message.role,
                    "text": message.text,
                    "at_ms": message.created_at_ms,
                    "author": serde_json::Value::Null,
                })
            })
            .collect()
    } else if environment == "channel" || environment.starts_with(CHANNEL_PREFIX) {
        // The provider is part of the environment's name but not of the
        // conversation's identity: the session key already names exactly one
        // conversation on exactly one account.
        DaemonStore::open(&paths)?
            .channel_conversation_messages(id, limit)?
            .into_iter()
            .map(|message| {
                serde_json::json!({
                    // An inbound channel message is what a person said to
                    // Little Monkey, and an outbound one is its answer — the
                    // two roles a transcript reader already knows.
                    "role": if message.outbound { "assistant" } else { "user" },
                    "text": message.text,
                    "at_ms": message.at_ms,
                    "author": message.author,
                })
            })
            .collect()
    } else {
        return Err(format!(
            "Unknown environment '{environment}' (expected {REMOTE_CONTROL} or channel:<provider>)"
        ));
    };

    if json {
        println!("{}", serde_json::json!({ "messages": messages }));
        return Ok(());
    }
    for message in &messages {
        println!(
            "{:<10} {}",
            message["role"].as_str().unwrap_or(""),
            message["text"].as_str().unwrap_or(""),
        );
    }
    Ok(())
}

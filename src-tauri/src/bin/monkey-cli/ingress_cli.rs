//! `monkey ingress` — what arrived from outside, and what it became.
//!
//! One listing across every origin: a Telegram DM, an inbound call, a paired
//! phone, a peer handover, a voice turn. Each row is a durable turn and the run
//! it produced, which is what lets an operator answer "did my message actually
//! do anything?" without reading a database.
//!
//! Message text is deliberately absent. This is a status surface, not a
//! transcript export: identifiers, state, and the reason a turn failed.

use little_monkey_lib::channels::ingress::ConversationSource;

use crate::daemon::ingress_store::StoredIngressTurn;
use crate::daemon::store::{DaemonPaths, DaemonStore};

/// `monkey ingress <action>`.
#[derive(clap::Subcommand, Debug)]
pub enum IngressCmd {
    /// Recent turns, newest first.
    List {
        /// Only this origin: desktop, mobile, messaging_channel, peer, voice,
        /// telephone.
        #[arg(long)]
        source: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    /// One turn, by the identity its origin submitted it under, with every
    /// continuation it produced.
    Show {
        #[arg(long)]
        source: String,
        /// Account, device, session or line the turn arrived on.
        #[arg(long)]
        account: String,
        /// The origin's own event id for the turn.
        #[arg(long)]
        event: String,
        #[arg(long)]
        json: bool,
    },
    /// Continue an already accepted turn that was frozen at a tool boundary.
    ///
    /// The continuation inherits the accepted turn's frozen execution context
    /// verbatim, so a recipe, model or permission mode changed since then does
    /// not affect it. Nothing here re-resolves configuration, and nothing here
    /// can invent a turn: a request to continue something that was never
    /// accepted is refused.
    Resume {
        #[arg(long)]
        source: String,
        #[arg(long)]
        account: String,
        /// The accepted turn's own event id — the parent, not a new one.
        #[arg(long)]
        event: String,
        /// The caller's own id for this Resume, minted once before the first
        /// attempt and repeated verbatim by every retry of it.
        ///
        /// Required, and deliberately not defaulted to something fresh: a
        /// generated id would make a retried request a second resume, which is
        /// the duplicate run this identity exists to prevent. Two intentional
        /// resumes of one turn are two ids.
        #[arg(long)]
        request_id: String,
        #[arg(long)]
        json: bool,
    },
}

pub fn dispatch(command: &IngressCmd) -> Result<(), String> {
    match command {
        IngressCmd::List {
            source,
            limit,
            json,
        } => list(source.as_deref(), *limit, *json),
        IngressCmd::Show {
            source,
            account,
            event,
            json,
        } => show(source, account, event, *json),
        IngressCmd::Resume {
            source,
            account,
            event,
            request_id,
            json,
        } => resume(source, account, event, request_id, *json),
    }
}

/// Parse an origin token, or refuse before anything is opened.
fn parse_source(value: &str) -> Result<ConversationSource, String> {
    ConversationSource::parse(value).ok_or_else(|| format!("Unknown conversation source '{value}'"))
}

/// One turn and its continuations, as the desktop reads them while a turn runs.
///
/// The continuations are what make an unmet workspace-mutation contract visible
/// to whoever is watching: the run that answers the operator may be the
/// continuation's, not the one they submitted, and the only way to find it
/// without the UI owning execution is to ask.
pub fn show(source: &str, account: &str, event: &str, json: bool) -> Result<(), String> {
    let source = parse_source(source)?;
    let store = DaemonStore::open(&DaemonPaths::resolve()?)?;
    let key = little_monkey_lib::channels::ingress::dedupe_key_for(source, account, event);
    let Some(turn) = store.ingress_turn_by_dedupe_key(&key)? else {
        if json {
            println!(
                "{}",
                serde_json::json!({ "turn": null, "continuations": [] })
            );
        } else {
            println!("No turn recorded for {key}.");
        }
        return Ok(());
    };
    let continuations: Vec<serde_json::Value> = store
        .ingress_continuations(&turn.ingress_id)?
        .iter()
        .map(|child| turn_json(&store, child))
        .collect::<Result<_, String>>()?;
    let row = turn_json(&store, &turn)?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "turn": row, "continuations": continuations })
        );
    } else {
        println!("{row}");
        for child in &continuations {
            println!("  continuation: {child}");
        }
    }
    Ok(())
}

/// What resuming an accepted turn produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResumedTurn {
    pub ingress_id: String,
    pub parent_ingress_id: String,
    pub job_id: String,
}

/// Submit a durable resume of an accepted turn.
///
/// Separate from the CLI wrapper so the two properties that matter — the
/// continuation runs what the *parent* was accepted with whatever the machine
/// says now, and one Resume is one continuation however many times the request
/// arrives — are testable against an in-memory store rather than only through a
/// process.
pub(crate) fn resume_accepted_turn(
    store: &mut DaemonStore,
    queue: &dyn crate::daemon::channel_worker::RunQueue,
    source: ConversationSource,
    account: &str,
    event: &str,
    request_id: &str,
    now_ms: i64,
) -> Result<ResumedTurn, String> {
    use little_monkey_lib::channels::ingress::ConversationIngress;

    if request_id.trim().is_empty() {
        return Err(
            "A resume must carry the caller's own request id, or a retry cannot be told from a second resume"
                .to_string(),
        );
    }
    if store.kill_switch()? {
        return Err("Global kill switch is engaged; nothing can be resumed".to_string());
    }
    let key = little_monkey_lib::channels::ingress::dedupe_key_for(source, account, event);
    let parent = store
        .ingress_turn_by_dedupe_key(&key)?
        .ok_or_else(|| format!("No accepted turn '{key}' to continue"))?;
    let accepted = store
        .accepted_ingress_turn(&parent.ingress_id)?
        .ok_or_else(|| format!("Accepted turn '{}' is unreadable", parent.ingress_id))?;
    // Refused rather than resolved. A turn accepted before execution contexts
    // were frozen has no snapshot to replay, and continuing it would mean
    // silently running whatever the machine is configured with now — which is
    // exactly the thing freezing exists to prevent.
    if accepted.ingress.execution.is_none() {
        return Err(format!(
            "Turn '{key}' was accepted without a frozen execution context and cannot be continued; start a new turn instead"
        ));
    }
    // The caller's request id, not a count of what is already here: resuming a
    // turn twice is two continuations because it is two ids, and a retry of one
    // request is one continuation because it is one id. Counting could not tell
    // those apart — the second press and the retry look identical from here.
    let continuation =
        ConversationIngress::resume_of(&accepted.ingress, &parent.ingress_id, request_id);

    let outcome = crate::daemon::channel_ingress::submit_conversation_turn(
        store,
        queue,
        &continuation,
        &accepted.params,
        now_ms,
    )?;
    let (ingress_id, job_id) = match outcome {
        crate::daemon::channel_ingress::SubmitOutcome::Queued { ingress_id, job_id }
        | crate::daemon::channel_ingress::SubmitOutcome::AlreadyQueued { ingress_id, job_id } => {
            (ingress_id, job_id)
        }
        crate::daemon::channel_ingress::SubmitOutcome::Deferred { error, .. } => return Err(error),
        crate::daemon::channel_ingress::SubmitOutcome::Parked { .. } => {
            return Err("This resumed turn could not be queued and was parked".to_string())
        }
    };
    Ok(ResumedTurn {
        ingress_id,
        parent_ingress_id: parent.ingress_id,
        job_id,
    })
}

fn resume(
    source: &str,
    account: &str,
    event: &str,
    request_id: &str,
    json: bool,
) -> Result<(), String> {
    let source = parse_source(source)?;
    let paths = DaemonPaths::resolve()?;
    let mut store = DaemonStore::open(&paths)?;
    let queue = crate::daemon::DaemonChannelQueue::new(paths.clone());
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis(),
    )
    .unwrap_or(i64::MAX);
    let resumed =
        resume_accepted_turn(&mut store, &queue, source, account, event, request_id, now)?;
    let run_id = store
        .get_job(&resumed.job_id)?
        .and_then(|job| job.run_id)
        .ok_or_else(|| format!("Resumed turn '{}' is still preparing", resumed.job_id))?;
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ingress_id": resumed.ingress_id,
                "parent_ingress_id": resumed.parent_ingress_id,
                "job_id": resumed.job_id,
                "run_id": run_id,
            })
        );
    } else {
        println!("Resumed as {run_id}");
    }
    Ok(())
}

pub fn list(source: Option<&str>, limit: u32, json: bool) -> Result<(), String> {
    let filter = match source {
        Some(value) => Some(
            ConversationSource::parse(value)
                .ok_or_else(|| format!("Unknown conversation source '{value}'"))?,
        ),
        None => None,
    };
    let store = DaemonStore::open(&DaemonPaths::resolve()?)?;
    let turns: Vec<StoredIngressTurn> = store
        .recent_ingress_turns(limit.clamp(1, 200))?
        .into_iter()
        .filter(|turn| filter.is_none_or(|source| turn.source == source))
        .collect();

    if json {
        let rows: Vec<serde_json::Value> = turns
            .iter()
            .map(|turn| turn_json(&store, turn))
            .collect::<Result<_, String>>()?;
        println!("{}", serde_json::json!({ "turns": rows }));
        return Ok(());
    }
    if turns.is_empty() {
        println!("No external turns recorded yet.");
        return Ok(());
    }
    for turn in &turns {
        // Enough to answer "did this arrive, what did it become, and under
        // which configuration" without reaching for --json. The frozen-context
        // digest is truncated the way a commit hash is: long enough to compare
        // two turns by eye, short enough to fit on the line.
        println!(
            "{}  {}  {}  {}  {}  {}  attempts={}  {}{}",
            turn.created_at_ms,
            turn.source.as_str(),
            turn.source_account_id,
            turn.source_event_id,
            turn.state.as_str(),
            turn.job_id.as_deref().unwrap_or("-"),
            turn.attempts,
            match (&turn.execution_version, &turn.execution_digest) {
                (Some(version), Some(digest)) => format!("cfg=v{version}:{}", &digest[..12]),
                _ => "cfg=-".to_string(),
            },
            match &turn.last_error {
                Some(error) => format!("  {error}"),
                None => String::new(),
            }
        );
    }
    Ok(())
}

/// One turn as the desktop reads it: the turn's own state, plus the run's when
/// there is a run. Both are shown because they answer different questions —
/// "did Little Monkey take the message?" and "how did the work go?".
fn turn_json(store: &DaemonStore, turn: &StoredIngressTurn) -> Result<serde_json::Value, String> {
    let job = match &turn.job_id {
        Some(job_id) => store.get_job(job_id)?,
        None => None,
    };
    // Only a messaging account has an operator-chosen label; every other origin
    // is identified by the account id its own subsystem assigned.
    let account_label = match turn.source {
        ConversationSource::MessagingChannel => store
            .channel_account(&turn.source_account_id)?
            .map(|account| account.label),
        _ => None,
    };
    Ok(serde_json::json!({
        "ingress_id": turn.ingress_id,
        "source": turn.source.as_str(),
        "source_account_id": turn.source_account_id,
        "account_label": account_label,
        "source_event_id": turn.source_event_id,
        "session_key": turn.session_key,
        "state": turn.state.as_str(),
        "attempts": turn.attempts,
        "last_error": turn.last_error,
        // Which configuration this turn was accepted under. The digest is what
        // an operator compares when a recovered run behaves like a different
        // one; the definition behind it is deliberately not exposed here.
        "execution_version": turn.execution_version,
        "execution_digest": turn.execution_digest,
        // The workspace-mutation contract: what the turn promised, where that
        // promise ended up, and what the run reported about it. Never message
        // text — the detail is a file count and, at most, a tool's own error.
        "mutation_required": turn.mutation_required,
        "mutation_state": turn.mutation_state.map(|state| state.as_str()),
        "mutation_detail": turn.mutation_detail,
        // Lineage, so a correction or a resume is never mistaken for a second
        // thing the operator asked for.
        "parent_ingress_id": turn.parent_ingress_id,
        "continuation_kind": turn.continuation_kind,
        "continuation_attempt": turn.continuation_attempt,
        "job_id": turn.job_id,
        "run_id": job.as_ref().and_then(|job| job.run_id.clone()),
        "run_state": job.as_ref().map(|job| job.state.token()),
        "run_error": job.as_ref().and_then(|job| job.last_error.clone()),
        "created_at_ms": turn.created_at_ms,
        "updated_at_ms": turn.updated_at_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::channels::ingress::ConversationIngress;
    use little_monkey_lib::channels::routing::RouteTarget;

    const NOW: i64 = 1_700_000_000_000;

    #[test]
    fn a_turn_is_reported_with_its_status_and_no_message_text() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        let ingress = ConversationIngress::direct(
            ConversationSource::Telephone,
            "tel-1",
            "call-1",
            "telephone:+15550100",
            "please call me back",
            RouteTarget::new("chat"),
            NOW,
        );
        store
            .accept_ingress_turn(&ingress, &["message=please call me back".into()], NOW)
            .expect("accept");

        let turn = &store.recent_ingress_turns(10).unwrap()[0];
        let row = turn_json(&store, turn).expect("row");

        assert_eq!(row["source"], "telephone");
        assert_eq!(row["state"], "accepted");
        assert_eq!(row["session_key"], "telephone:+15550100");
        assert!(row["job_id"].is_null());
        assert!(row["run_state"].is_null());
        assert!(!row.to_string().contains("please call me back"));
    }

    /// The typed bridge's two halves are written in different languages, so
    /// the only thing that keeps them in step is a test that reads both. This
    /// one fails when a field is added, renamed or dropped on either side.
    #[test]
    fn the_bridge_row_matches_the_frontend_type() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .accept_ingress_turn(
                &ConversationIngress::direct(
                    ConversationSource::Peer,
                    "node-1",
                    "handover-1",
                    "peer:node-1",
                    "take this",
                    RouteTarget::new("chat"),
                    NOW,
                ),
                &[],
                NOW,
            )
            .expect("accept");
        let row = turn_json(&store, &store.recent_ingress_turns(1).unwrap()[0]).expect("row");
        let mut emitted: Vec<String> = row
            .as_object()
            .expect("object")
            .keys()
            .map(String::from)
            .collect();
        emitted.sort();

        let client = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/lib/ingressClient.ts"),
        )
        .expect("src/lib/ingressClient.ts");
        let declared_block = client
            .split_once("export interface IngressTurn {")
            .expect("the IngressTurn interface")
            .1
            .split_once("\n}")
            .expect("the end of the interface")
            .0;
        let mut declared: Vec<String> = declared_block
            .lines()
            .filter_map(|line| line.trim().split_once(':'))
            .map(|(field, _)| field.trim().to_string())
            .filter(|field| !field.starts_with('*') && !field.starts_with("//"))
            .collect();
        declared.sort();

        assert_eq!(emitted, declared);

        // Every origin the durable contract defines has to be spellable on the
        // frontend too, or a turn arrives that the UI cannot label.
        for source in [
            ConversationSource::Desktop,
            ConversationSource::Mobile,
            ConversationSource::MessagingChannel,
            ConversationSource::Peer,
            ConversationSource::Voice,
            ConversationSource::Telephone,
        ] {
            assert!(
                client.contains(&format!("\"{}\"", source.as_str())),
                "src/lib/ingressClient.ts does not know the '{}' origin",
                source.as_str()
            );
        }
    }

    #[test]
    fn an_unknown_source_filter_is_refused_before_anything_is_read() {
        assert!(list(Some("carrier pigeon"), 10, true)
            .expect_err("unknown source")
            .contains("carrier pigeon"));
    }
}

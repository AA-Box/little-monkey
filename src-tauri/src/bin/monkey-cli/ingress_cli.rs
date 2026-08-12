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
}

pub fn dispatch(command: &IngressCmd) -> Result<(), String> {
    match command {
        IngressCmd::List {
            source,
            limit,
            json,
        } => list(source.as_deref(), *limit, *json),
    }
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
        println!(
            "{}  {}  {}  {}  {}",
            turn.created_at_ms,
            turn.source.as_str(),
            turn.state.as_str(),
            turn.job_id.as_deref().unwrap_or("-"),
            turn.session_key
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

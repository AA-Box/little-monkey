//! The handful of messages the daemon answers itself instead of running a turn.
//!
//! A provider's command menu — Telegram's BotFather list, say — is autocomplete
//! and nothing else: `/status` arrives as ordinary message text, and without
//! this module it would be handed to the model as a question about the word
//! "status". Every command here is one the model *cannot* answer, because the
//! answer is daemon state the run has no access to: which model the route
//! points at, whether a turn is in flight, whether the kill switch is engaged.
//!
//! Placed after the access gate on purpose. An unpaired stranger gets the
//! pairing challenge, never a menu — commands are for senders the account has
//! already decided to talk to.
//!
//! A model pick is the *sender's own*. `/model 2` records a target for that
//! person on that account and nothing else changes: the routed recipe stays the
//! machine's default, everybody else is still answered on it, and two people in
//! the same group can be answered on two models at once. Nobody is asked to
//! pick before being answered — the first message runs on the default and the
//! sender is told, once, what that is and how to change it.
//!
//! Nothing here queues a run, and nothing here writes the outbox: this module
//! decides *what to say*, and `channel_ingress` commits the answer with the
//! event, the same way it commits a pairing challenge. Keeping the durable
//! write there is what leaves the outbox with the few producers
//! `channel_restart_tests` guards.

use little_monkey_lib::channels::ingress::ConversationSource;
use little_monkey_lib::channels::routing::{resolve_route, ChannelRoute};
use little_monkey_lib::channels::types::ChannelEnvelope;
use little_monkey_lib::recipes::{self, RecipeTarget};

use super::channel_store::ChannelAccountRecord;
use super::ingress_store::{IngressState, StoredIngressTurn};
use super::store::DaemonStore;

/// How far back a conversation's own turn is looked for. A person asking to
/// stop or resume means the thing that just happened, not something from last
/// week, and this is a scan of the newest rows rather than a query per session.
const RECENT_TURN_SCAN: u32 = 100;

/// One inbound message the daemon answers on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Command {
    /// `/start`, optionally carrying a model choice from the menu it prints.
    Start(Option<usize>),
    Help,
    /// `/new` and `/clear`, which mean the same thing here — see [`answer`].
    Clear,
    Status,
    Stop,
    Resume,
    /// `/settings`, optionally carrying a model choice.
    Settings(Option<usize>),
}

/// The meta row the first-run *gate* this module used to have wrote. Any value
/// means the machine already went through the model conversation once — the
/// menu was shown, or a pick was made — so nobody here is introduced again.
const MODEL_GATE_KEY: &str = "channel_model_chosen";

/// One meta row per sender who has been told, once, which model answers them.
const INTRODUCED_KEY_PREFIX: &str = "channel_introduced:";

/// One meta row per sender who picked a model for themselves.
const SENDER_MODEL_KEY_PREFIX: &str = "channel_sender_model:";

fn sender_model_key(account_id: &str, sender_id: &str) -> String {
    format!("{SENDER_MODEL_KEY_PREFIX}{account_id}:{sender_id}")
}

/// The model this sender picked for themselves, if they have.
///
/// A row this build cannot read counts as no pick: the default answers, and the
/// person can pick again. Refusing their messages over a stale row would be
/// worse than either.
pub(super) fn sender_model(
    store: &DaemonStore,
    account_id: &str,
    sender_id: &str,
) -> Result<Option<RecipeTarget>, String> {
    let Some(raw) = store.get_meta(&sender_model_key(account_id, sender_id))? else {
        return Ok(None);
    };
    Ok(serde_json::from_str(&raw).ok())
}

pub(super) fn set_sender_model(
    store: &mut DaemonStore,
    account_id: &str,
    sender_id: &str,
    target: &RecipeTarget,
) -> Result<(), String> {
    let raw = serde_json::to_string(target).map_err(|error| error.to_string())?;
    store.set_meta(&sender_model_key(account_id, sender_id), &raw)
}

/// Whether this is the first message this account has answered from this
/// sender — and, if it is, records that it no longer is.
///
/// Written *before* the caller queues the notice on purpose: a crash between
/// the two loses one line of courtesy, where the other order would send it
/// twice. A machine that went through the old first-run gate has already had
/// the model conversation with its people and introduces nobody.
pub(super) fn first_contact(
    store: &mut DaemonStore,
    account_id: &str,
    sender_id: &str,
) -> Result<bool, String> {
    if store.get_meta(MODEL_GATE_KEY)?.is_some() {
        return Ok(false);
    }
    if store
        .get_meta(&introduced_key(account_id, sender_id))?
        .is_some()
    {
        return Ok(false);
    }
    mark_introduced(store, account_id, sender_id)?;
    Ok(true)
}

fn introduced_key(account_id: &str, sender_id: &str) -> String {
    format!("{INTRODUCED_KEY_PREFIX}{account_id}:{sender_id}")
}

/// This sender has been told which model answers them — by the notice, or by a
/// command whose answer already says so, which is why `/start` and then a
/// message does not name the model twice.
fn mark_introduced(
    store: &mut DaemonStore,
    account_id: &str,
    sender_id: &str,
) -> Result<(), String> {
    store.set_meta(&introduced_key(account_id, sender_id), "1")
}

/// Everything this module remembers about one sender — their model pick and
/// that they were introduced — gone, for an operator forgetting them. Their
/// next message is a stranger's, and is greeted as one.
pub(crate) fn forget_sender_state(
    store: &mut DaemonStore,
    account_id: &str,
    sender_id: &str,
) -> Result<(), String> {
    store.delete_meta(&sender_model_key(account_id, sender_id))?;
    store.delete_meta(&introduced_key(account_id, sender_id))?;
    Ok(())
}

/// The sender's own pick, in the one line a listing shows — `None` when they
/// are answered on the machine's default.
pub(crate) fn sender_model_label(
    store: &DaemonStore,
    account_id: &str,
    sender_id: &str,
) -> Result<Option<String>, String> {
    Ok(sender_model(store, account_id, sender_id)?
        .map(|target| super::describe_recipe_target(&target)))
}

/// A machine whose people were all told already, for the fixtures that are
/// about something other than the first-contact notice — without it every
/// fixture's first message would grow an extra outbox row.
#[cfg(test)]
pub(crate) fn suppress_first_run_notice(store: &mut DaemonStore) -> Result<(), String> {
    store.set_meta(MODEL_GATE_KEY, "1")
}

/// The command in `text`, if it is one.
///
/// Telegram addresses a command in a group as `/status@botname`, so the suffix
/// is stripped; a provider that does not do that is unaffected. A numeric
/// argument is only ever read from `/start` and `/settings`, never from a bare
/// message, so a conversation that happens to contain "2" cannot change which
/// model this machine answers on.
pub(super) fn parse(text: &str) -> Option<Command> {
    let mut words = text.split_whitespace();
    let head = words.next()?;
    let name = head.split('@').next()?.to_ascii_lowercase();
    let choice = words.next().and_then(|value| value.parse::<usize>().ok());
    match name.as_str() {
        "/start" => Some(Command::Start(choice)),
        "/help" => Some(Command::Help),
        "/new" | "/clear" => Some(Command::Clear),
        "/status" => Some(Command::Status),
        "/stop" => Some(Command::Stop),
        "/resume" => Some(Command::Resume),
        // `/model` and `/settings` are the same command: one names the thing
        // being changed, the other is where a person looks for it.
        "/settings" | "/model" => Some(Command::Settings(choice)),
        _ => None,
    }
}

/// The command this envelope carries, if it is one addressed to us.
///
/// A group message may name the bot it is for (`/status@somebot`), and only the
/// adapter knows which name is ours — it reports that as `mentions_self`. So an
/// addressed command is ours only when the envelope says the mention was; an
/// unaddressed `/status` is taken as ours, which is what lets a person use the
/// command menu in a group without knowing the bot's username. A command we do
/// not implement still parses to nothing and is left alone, so another bot's
/// `/weather` is not answered by us.
pub(super) fn command_for(envelope: &ChannelEnvelope) -> Option<Command> {
    let head = envelope.text.split_whitespace().next()?;
    if head.contains('@') && !envelope.mentions_self {
        return None;
    }
    parse(&envelope.text)
}

/// What to send back for one command, performing whatever it asks for on the
/// way: `/stop` requests the cancel, `/settings n` writes the new model.
///
/// Returns text rather than sending it. The caller commits it with the event,
/// so a command is answered exactly once however many times the provider
/// redelivers it.
pub(super) fn reply(
    store: &mut DaemonStore,
    queue: &dyn super::channel_worker::RunQueue,
    account: &ChannelAccountRecord,
    envelope: &ChannelEnvelope,
    command: Command,
    now_ms: i64,
) -> Result<String, String> {
    let route = store
        .channel_routes()
        .ok()
        .and_then(|routes| resolve_route(&routes, envelope).ok().cloned());
    let who = Who {
        account_id: &account.account_id,
        sender_id: &envelope.sender.sender_id,
    };
    // These answers name the model; a person who has read one needs no notice.
    if matches!(
        command,
        Command::Start(None) | Command::Settings(None) | Command::Status
    ) {
        mark_introduced(store, who.account_id, who.sender_id)?;
    }
    Ok(match command {
        Command::Start(None) => start_text(store, who, route.as_ref()),
        Command::Start(Some(choice)) | Command::Settings(Some(choice)) => {
            apply_choice(store, who, route.as_ref(), choice, &catalogue())
        }
        Command::Settings(None) => settings_text(store, who, route.as_ref()),
        Command::Help => HELP.to_string(),
        Command::Clear => CLEAR.to_string(),
        Command::Status => status_text(store, who, account, envelope, route.as_ref())?,
        Command::Stop => stop_text(store, envelope, route.as_ref(), now_ms)?,
        Command::Resume => resume_text(store, queue, account, envelope, route.as_ref(), now_ms)?,
    })
}

/// The person a model line or a pick is about: one sender on one account.
#[derive(Clone, Copy)]
pub(super) struct Who<'a> {
    pub(super) account_id: &'a str,
    pub(super) sender_id: &'a str,
}

const HELP: &str = "\
/start — what this is, and the model it answers on
/help — this list
/new, /clear — about conversation memory
/status — connection, model, and whether a turn is running
/stop — cancel the turn running now
/resume — retry a turn that failed to start
/model, /settings — show the model answering you, or `/model 2` to pick another for yourself";

/// `/new` and `/clear` both mean "forget what we said", and there is nothing to
/// forget: a channel turn is queued with the message as its only parameter, so
/// every message is already answered on its own. Saying so is the honest
/// answer; pretending to clear something would be a lie the next reply exposes.
const CLEAR: &str = "\
Nothing to clear — each message is answered on its own, with no memory of the \
ones before it.";

fn start_text(store: &DaemonStore, who: Who<'_>, route: Option<&ChannelRoute>) -> String {
    format!(
        "Send a message and it is answered by this machine.\n\n{}\n\n{}",
        current_model_line(store, who, route),
        menu_text()
    )
}

/// What a person is told, once, with the answer to their first message: which
/// model is answering and that `/model` changes it — for them alone.
///
/// No menu here. This also goes out over SMS, where a machine with a dozen
/// models installed would bill its inventory to somebody's first text.
pub(super) fn first_run_notice(
    store: &DaemonStore,
    who: Who<'_>,
    route: Option<&ChannelRoute>,
) -> String {
    format!(
        "{}\nSend /model to see the models on this machine, or `/model 2` to be \
         answered on another one. Your pick changes nothing for anybody else.",
        current_model_line(store, who, route)
    )
}

fn settings_text(store: &DaemonStore, who: Who<'_>, route: Option<&ChannelRoute>) -> String {
    format!(
        "{}\n\n{}",
        current_model_line(store, who, route),
        menu_text()
    )
}

/// The model answering this person: their own pick when they made one, else
/// the routed recipe's target, read the same way the runner reads it.
fn current_model_line(store: &DaemonStore, who: Who<'_>, route: Option<&ChannelRoute>) -> String {
    let Some(route) = route else {
        return "No task is routed to this conversation yet.".to_string();
    };
    if let Ok(Some(chosen)) = sender_model(store, who.account_id, who.sender_id) {
        return format!(
            "Answering you on {} — your own pick (task '{}').",
            super::describe_recipe_target(&chosen),
            route.target.recipe
        );
    }
    let Ok(roots) = recipes::global_config_roots() else {
        return format!("Task: {}", route.target.recipe);
    };
    match recipes::resolve_recipe(&route.target.recipe, None, &roots) {
        Ok(recipe) => format!(
            "Answering on {} — this machine's default (task '{}').",
            super::describe_recipe_target(&recipe.target),
            route.target.recipe
        ),
        Err(error) => format!("Task '{}' cannot be read: {error}", route.target.recipe),
    }
}

/// Record the model the sender picked, for that sender alone.
///
/// The pick lives in this database, keyed by account and sender, and is read
/// back at accept time into the turn's frozen recipe. The recipe file is never
/// written: it is the default every *other* conversation runs on, and one
/// person changing it for everybody is the bug this replaced.
fn apply_choice(
    store: &mut DaemonStore,
    who: Who<'_>,
    route: Option<&ChannelRoute>,
    choice: usize,
    models: &[ModelChoice],
) -> String {
    if route.is_none() {
        return "No task is routed to this conversation yet, so there is no model to change."
            .to_string();
    }
    let Some(model) = choice.checked_sub(1).and_then(|index| models.get(index)) else {
        return format!("No model {choice} in the list.\n\n{}", render(models));
    };
    match set_sender_model(store, who.account_id, who.sender_id, &model.target) {
        Ok(()) => format!(
            "Now answering you on {}. Nobody else's model changes, and a turn already \
             running keeps the model it started with.",
            model.label
        ),
        Err(error) => format!("Could not change the model: {error}"),
    }
}

fn status_text(
    store: &mut DaemonStore,
    who: Who<'_>,
    account: &ChannelAccountRecord,
    envelope: &ChannelEnvelope,
    route: Option<&ChannelRoute>,
) -> Result<String, String> {
    let running = match active_turn(store, envelope, route)? {
        Some(turn) => match turn.job_id {
            Some(job_id) => format!("A turn is running (job {job_id})."),
            None => "A turn is accepted and waiting to start.".to_string(),
        },
        None => "No turn is running.".to_string(),
    };
    let switch = if store.kill_switch()? {
        "\nThe kill switch is engaged: nothing runs until an operator releases it."
    } else {
        ""
    };
    Ok(format!(
        "Connection: {}{}.\n{}\n{running}{switch}",
        account.health.state.as_str(),
        if account.enabled {
            ""
        } else {
            ", account disabled"
        },
        current_model_line(store, who, route),
    ))
}

/// Cancel the turn this conversation has in flight.
///
/// A cancel is *requested*, not performed: the run's own loop is what stops,
/// and a job already terminal is left alone. So this reports what it asked for
/// rather than claiming the run has stopped.
fn stop_text(
    store: &mut DaemonStore,
    envelope: &ChannelEnvelope,
    route: Option<&ChannelRoute>,
    now_ms: i64,
) -> Result<String, String> {
    let Some(turn) = active_turn(store, envelope, route)? else {
        return Ok("Nothing is running for this conversation.".to_string());
    };
    let Some(job_id) = turn.job_id else {
        return Ok("The turn has not started yet; it will run and then finish.".to_string());
    };
    let now = u64::try_from(now_ms.max(0)).unwrap_or(0);
    match store.request_cancel(&job_id, now) {
        Ok(job) if job.state.is_terminal() => Ok("That turn had already finished.".to_string()),
        Ok(_) => Ok("Asked the running turn to stop.".to_string()),
        Err(error) => Ok(format!("Could not stop it: {error}")),
    }
}

/// Re-drive a turn that was accepted and never made it to the queue.
///
/// Not "resume the conversation": there is no conversation state to resume. The
/// case this exists for is a turn parked by a credential that disappeared or a
/// queue that refused it — durably accepted, never run, and recoverable under
/// the configuration it was accepted with.
fn resume_text(
    store: &mut DaemonStore,
    queue: &dyn super::channel_worker::RunQueue,
    account: &ChannelAccountRecord,
    envelope: &ChannelEnvelope,
    route: Option<&ChannelRoute>,
    now_ms: i64,
) -> Result<String, String> {
    let key = session_key(envelope, route);
    let stalled = store
        .recent_ingress_turns(RECENT_TURN_SCAN)?
        .into_iter()
        .find(|turn| turn.session_key == key && turn.state == IngressState::Failed);
    let Some(turn) = stalled else {
        return Ok("Nothing to resume — no turn here failed to start.".to_string());
    };
    // The request id is the provider's own event id for *this* `/resume`, so a
    // redelivery of the command is the same resume rather than a second one.
    let request_id = format!("command-resume-{}", envelope.provider_event_id);
    match crate::ingress_cli::resume_accepted_turn(
        store,
        queue,
        ConversationSource::MessagingChannel,
        &account.account_id,
        &turn.source_event_id,
        &request_id,
        now_ms,
    ) {
        Ok(crate::ingress_cli::ResumeOutcome::Accepted(_)) => {
            Ok("Picked that turn back up.".to_string())
        }
        Ok(crate::ingress_cli::ResumeOutcome::Refused(reason)) => Ok(reason),
        Err(error) => Ok(format!("Could not resume it: {error}")),
    }
}

/// The newest turn of this conversation that has not finished reaching the
/// queue. `Queued` carries a job id; `Accepted` is on its way to one.
fn active_turn(
    store: &mut DaemonStore,
    envelope: &ChannelEnvelope,
    route: Option<&ChannelRoute>,
) -> Result<Option<StoredIngressTurn>, String> {
    let key = session_key(envelope, route);
    Ok(store
        .recent_ingress_turns(RECENT_TURN_SCAN)?
        .into_iter()
        .find(|turn| {
            turn.session_key == key
                && matches!(turn.state, IngressState::Queued | IngressState::Accepted)
        }))
}

/// The session a turn of this conversation is recorded under. Read from the
/// route's own scope, because that is what wrote it.
fn session_key(envelope: &ChannelEnvelope, route: Option<&ChannelRoute>) -> String {
    match route {
        Some(route) => route.target.session_scope.session_key(envelope),
        None => envelope.default_session_key(),
    }
}

/// One model this machine can answer on.
struct ModelChoice {
    label: String,
    target: RecipeTarget,
}

fn menu_text() -> String {
    render(&catalogue())
}

fn render(models: &[ModelChoice]) -> String {
    if models.is_empty() {
        return "No model is installed on this machine to switch to.".to_string();
    }
    let mut out = String::from("Models on this machine:\n");
    for (index, model) in models.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", index + 1, model.label));
    }
    out.push_str(
        "\nSend `/model 1` to be answered on that one instead. Your pick changes nothing for \
         anybody else.",
    );
    out
}

/// Every local model this machine could answer on, in the order
/// `channels_cli::starter_recipe_target` prefers them.
///
/// Local only. A cloud provider is deliberately absent for the same reason the
/// starter target never guesses one: picking it picks a bill, and the person
/// typing `/settings` in a chat is not necessarily the person paying it. Cloud
/// targets are set by the operator, from the desktop app or the recipe file.
fn catalogue() -> Vec<ModelChoice> {
    let mut models = ollama_models();
    let Some(app_data) = crate::app_data_dir() else {
        return models;
    };
    // Both stores a `managed_model` id can resolve from, in the order the
    // runner tries them: the M3 hub, then the app's own managed models
    // directory. Listing only the hub hid the model the desktop app downloaded
    // — the one most machines actually answer on.
    let mut seen = std::collections::BTreeSet::new();
    let hub = little_monkey_lib::m3_runtime_hub::installed_model_inventory(&app_data)
        .into_iter()
        .map(|installed| installed.model_id);
    let app_models = little_monkey_lib::models::app_managed_chat_models(&app_data)
        .into_iter()
        .map(|(model_id, _)| model_id);
    for model_id in hub.chain(app_models) {
        if !seen.insert(model_id.clone()) {
            continue;
        }
        models.push(ModelChoice {
            label: format!("Local · {model_id}"),
            target: RecipeTarget {
                managed_model: Some(model_id),
                ..Default::default()
            },
        });
    }
    models
}

/// Ollama's installed tags, asked for on a thread of this function's own.
///
/// The acceptance path is synchronous and is called from inside the inbound
/// loop's runtime as well as from tests that have no runtime at all, so neither
/// `block_on` nor `block_in_place` is safe here. A short-lived thread with its
/// own current-thread runtime is correct in both. The request is bounded by
/// `egress::hardened`'s connect and read budget, so an Ollama daemon that
/// accepts the connection and never answers costs one timeout, not a hang.
fn ollama_models() -> Vec<ModelChoice> {
    let fetched = std::thread::spawn(|| {
        let client = little_monkey_lib::egress::hardened().build().ok()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(crate::ollama_api::tags(&client)).ok()
    })
    .join();
    let Ok(Some(tags)) = fetched else {
        return Vec::new();
    };
    tags.models
        .into_iter()
        .map(|tag| ModelChoice {
            label: format!("Ollama · {}", tag.name),
            target: RecipeTarget {
                ollama: Some(tag.name),
                ..Default::default()
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_command_is_recognised_with_and_without_the_bots_name() {
        assert_eq!(parse("/status"), Some(Command::Status));
        assert_eq!(parse("  /Status@little_monkey_bot "), Some(Command::Status));
        assert_eq!(parse("/new"), Some(Command::Clear));
        assert_eq!(parse("/clear"), Some(Command::Clear));
    }

    #[test]
    fn a_model_choice_is_only_read_from_the_commands_that_offer_one() {
        assert_eq!(parse("/settings 2"), Some(Command::Settings(Some(2))));
        assert_eq!(parse("/start 3"), Some(Command::Start(Some(3))));
        assert_eq!(parse("/settings"), Some(Command::Settings(None)));
        // A number the model was asked about is a message, not a choice.
        assert_eq!(parse("2"), None);
        assert_eq!(parse("/stop 2"), Some(Command::Stop));
    }

    #[test]
    fn ordinary_text_is_never_a_command() {
        assert_eq!(parse("what is your status?"), None);
        assert_eq!(parse("/unknown"), None);
        assert_eq!(parse(""), None);
        // A path is not a command, even though it starts with a slash.
        assert_eq!(parse("/etc/hosts is missing"), None);
    }

    #[test]
    fn the_menu_numbers_from_one_and_says_how_to_pick() {
        let models = vec![
            ModelChoice {
                label: "Ollama · qwen".to_string(),
                target: RecipeTarget {
                    ollama: Some("qwen".to_string()),
                    ..Default::default()
                },
            },
            ModelChoice {
                label: "Local · llama".to_string(),
                target: RecipeTarget {
                    managed_model: Some("llama".to_string()),
                    ..Default::default()
                },
            },
        ];
        let rendered = render(&models);
        assert!(rendered.contains("1. Ollama · qwen"), "{rendered}");
        assert!(rendered.contains("2. Local · llama"), "{rendered}");
        assert!(rendered.contains("/model 1"), "{rendered}");
    }

    #[test]
    fn an_empty_inventory_says_so_rather_than_offering_nothing() {
        assert!(render(&[]).contains("No model is installed"));
    }

    fn route_to(recipe: &str) -> ChannelRoute {
        ChannelRoute {
            route_id: "route-1".into(),
            scope: little_monkey_lib::channels::routing::RouteScope::account("acct-1"),
            target: little_monkey_lib::channels::routing::RouteTarget::new(recipe),
            enabled: true,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn models() -> Vec<ModelChoice> {
        vec![
            ModelChoice {
                label: "Ollama · qwen".to_string(),
                target: RecipeTarget {
                    ollama: Some("qwen".to_string()),
                    ..Default::default()
                },
            },
            ModelChoice {
                label: "Local · llama".to_string(),
                target: RecipeTarget {
                    managed_model: Some("llama".to_string()),
                    ..Default::default()
                },
            },
        ]
    }

    const ADA: Who<'static> = Who {
        account_id: "acct-1",
        sender_id: "ada",
    };
    const BO: Who<'static> = Who {
        account_id: "acct-1",
        sender_id: "bo",
    };

    /// The pick is the sender's own: it is read back for them and for nobody
    /// else, and the same person on another account is another person.
    #[test]
    fn a_pick_is_the_senders_own() {
        let mut store = DaemonStore::open_in_memory().expect("open");

        let said = apply_choice(&mut store, ADA, Some(&route_to("chat")), 2, &models());

        assert!(said.contains("Local · llama"), "{said}");
        assert!(said.contains("Nobody else"), "{said}");
        assert_eq!(
            sender_model(&store, "acct-1", "ada").expect("read"),
            Some(models()[1].target.clone())
        );
        assert_eq!(sender_model(&store, "acct-1", "bo").expect("read"), None);
        assert_eq!(sender_model(&store, "acct-2", "ada").expect("read"), None);
    }

    /// Two people, two models, at the same time — the complaint this came from
    /// was one person's `/model` changing what everybody else was answered on.
    #[test]
    fn two_senders_keep_two_picks() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        apply_choice(&mut store, ADA, Some(&route_to("chat")), 1, &models());
        apply_choice(&mut store, BO, Some(&route_to("chat")), 2, &models());

        assert_eq!(
            sender_model(&store, "acct-1", "ada").expect("ada"),
            Some(models()[0].target.clone())
        );
        assert_eq!(
            sender_model(&store, "acct-1", "bo").expect("bo"),
            Some(models()[1].target.clone())
        );
    }

    /// A number outside the menu changes nothing and shows the menu again.
    #[test]
    fn a_choice_off_the_end_of_the_menu_changes_nothing() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        for choice in [0, 3] {
            let said = apply_choice(&mut store, ADA, Some(&route_to("chat")), choice, &models());
            assert!(said.contains("No model"), "{said}");
        }
        assert_eq!(sender_model(&store, "acct-1", "ada").expect("read"), None);
    }

    /// A pick that no longer parses is no pick: the default answers rather
    /// than the person's messages being refused over a stale row.
    #[test]
    fn an_unreadable_pick_reads_as_none() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .set_meta(&sender_model_key("acct-1", "ada"), "not json")
            .expect("meta");
        assert_eq!(sender_model(&store, "acct-1", "ada").expect("read"), None);
    }

    /// A person is introduced once, per account; a machine that already had
    /// the model conversation under the old gate introduces nobody.
    #[test]
    fn a_sender_is_a_first_contact_exactly_once() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        assert!(first_contact(&mut store, "acct-1", "ada").expect("first"));
        assert!(!first_contact(&mut store, "acct-1", "ada").expect("again"));
        assert!(first_contact(&mut store, "acct-1", "bo").expect("someone else"));
        assert!(first_contact(&mut store, "acct-2", "ada").expect("another account"));

        let mut told = DaemonStore::open_in_memory().expect("open");
        suppress_first_run_notice(&mut told).expect("mark");
        assert!(!first_contact(&mut told, "acct-1", "ada").expect("introduced machine"));
    }

    /// Reading `/start`'s answer is being introduced: the notice would say the
    /// same thing again.
    #[test]
    fn a_command_that_names_the_model_counts_as_the_introduction() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        mark_introduced(&mut store, "acct-1", "ada").expect("mark");
        assert!(!first_contact(&mut store, "acct-1", "ada").expect("told already"));
    }

    /// Forgetting a sender forgets their pick and their introduction, and
    /// nobody else's.
    #[test]
    fn forgetting_a_sender_clears_only_their_state() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        set_sender_model(&mut store, "acct-1", "ada", &models()[0].target).expect("ada");
        set_sender_model(&mut store, "acct-1", "bo", &models()[1].target).expect("bo");
        assert!(first_contact(&mut store, "acct-1", "ada").expect("ada first"));

        forget_sender_state(&mut store, "acct-1", "ada").expect("forget");

        assert_eq!(sender_model(&store, "acct-1", "ada").expect("ada"), None);
        assert_eq!(
            sender_model_label(&store, "acct-1", "ada").expect("ada label"),
            None
        );
        assert_eq!(
            sender_model_label(&store, "acct-1", "bo").expect("bo label"),
            Some("managed:llama".to_string())
        );
        assert!(first_contact(&mut store, "acct-1", "ada").expect("ada again"));
    }

    /// The notice says which model and how to change it, and never carries the
    /// menu — it also goes out over SMS.
    #[test]
    fn the_first_contact_notice_names_the_model_and_the_command_without_the_menu() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        set_sender_model(&mut store, "acct-1", "ada", &models()[1].target).expect("pick");

        let notice = first_run_notice(&store, ADA, Some(&route_to("chat")));

        assert!(notice.contains("managed:llama"), "{notice}");
        assert!(notice.contains("/model"), "{notice}");
        assert!(!notice.contains("Models on this machine"), "{notice}");
    }

    /// `/model` is `/settings` under the name a person looks for mid-chat.
    #[test]
    fn model_is_the_same_command_as_settings() {
        assert_eq!(parse("/model"), Some(Command::Settings(None)));
        assert_eq!(parse("/model 2"), Some(Command::Settings(Some(2))));
        assert_eq!(
            parse("/model@little_monkey_bot 2"),
            Some(Command::Settings(Some(2)))
        );
    }
}

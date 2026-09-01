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
//! Nothing here queues a run, and nothing here writes the outbox: this module
//! decides *what to say*, and `channel_ingress` commits the answer with the
//! event, the same way it commits a pairing challenge. Keeping the durable
//! write there is what leaves the outbox with the few producers
//! `channel_restart_tests` guards.

use std::path::PathBuf;

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
    /// Not typed by anyone: the answer to the first message on a machine where
    /// nobody has picked a model yet. See [`model_chosen`].
    ChooseFirst,
}

/// Where the first-run model gate stands on this machine. Machine-wide, because
/// the pick is: a pick writes the routed recipe, which is the one file every
/// conversation on that route runs.
const MODEL_GATE_KEY: &str = "channel_model_chosen";

/// A model has been chosen; nothing is gated again.
const GATE_OPEN: &str = "1";

/// Armed, and the model the route named when it was armed. A target that no
/// longer matches means somebody chose since — see [`model_chosen`].
const GATE_ARMED_PREFIX: &str = "pending:";

/// The target string recorded when the routed recipe could not be read. Any
/// real target differs from it, which is the answer that fails safe: an
/// unreadable recipe leaves the gate armed rather than opening it by accident.
const UNKNOWN_TARGET: &str = "unknown";

/// Whether the machine may answer, or owes the sender a model menu first.
///
/// Three states, in one meta row:
///
/// - **Unset** — nobody has been asked yet. Arms the gate against the model the
///   route names *now*, and answers with the menu. A machine with nothing
///   installed opens instead: a menu with no options and no way past it is
///   worse than answering on whatever the recipe already says.
/// - **Armed** — gated, until the routed recipe names a different model than it
///   did when armed. That is what lets a choice made anywhere else count: the
///   desktop app's model picker, `monkey channels`, or an operator editing the
///   recipe by hand all change the same file, and none of them can reach into
///   this database to say so.
/// - **Open** — answered normally, forever.
pub(super) fn model_chosen(
    store: &mut DaemonStore,
    current_target: Option<&str>,
) -> Result<bool, String> {
    let target = current_target.unwrap_or(UNKNOWN_TARGET);
    match store.get_meta(MODEL_GATE_KEY)? {
        Some(value) if value == GATE_OPEN => Ok(true),
        Some(value) => match value.strip_prefix(GATE_ARMED_PREFIX) {
            // Same model as when we armed: nobody has chosen yet.
            Some(armed) if armed == target => Ok(false),
            // Changed since, so somebody chose — wherever they did it.
            Some(_) => {
                open_gate(store)?;
                Ok(true)
            }
            // A value this build does not recognise. Answering is the safe
            // reading of a row we cannot interpret.
            None => Ok(true),
        },
        None => {
            let models = catalogue();
            // Nothing to offer, or the route already names a model this machine
            // can actually run: either way there is nothing to ask. Asking a
            // person to choose while their bot is working is not a first-run
            // question, it is an obstacle — and the recipe naming a model that
            // *is* installed is as good an answer as one typed into a chat.
            if models.is_empty() || runnable_target(&models, current_target) {
                open_gate(store)?;
                return Ok(true);
            }
            store.set_meta(MODEL_GATE_KEY, &format!("{GATE_ARMED_PREFIX}{target}"))?;
            Ok(false)
        }
    }
}

/// The model the route's recipe names right now, in the same words
/// [`current_model_line`] shows. `None` when there is no route or the recipe
/// cannot be read.
pub(super) fn routed_target(route: Option<&ChannelRoute>) -> Option<String> {
    let route = route?;
    let roots = recipes::global_config_roots().ok()?;
    let recipe = recipes::resolve_recipe(&route.target.recipe, None, &roots).ok()?;
    Some(super::describe_recipe_target(&recipe.target))
}

/// Whether `target` is one of the models on the menu — that is, one this
/// machine can start. A target naming something uninstalled is what makes the
/// question worth asking.
fn runnable_target(models: &[ModelChoice], target: Option<&str>) -> bool {
    let Some(target) = target else {
        return false;
    };
    models
        .iter()
        .any(|model| super::describe_recipe_target(&model.target) == target)
}

fn open_gate(store: &mut DaemonStore) -> Result<(), String> {
    store.set_meta(MODEL_GATE_KEY, GATE_OPEN)
}

/// A machine whose model was already chosen, for the fixtures that are about
/// something other than the first-run gate.
#[cfg(test)]
pub(crate) fn mark_model_chosen(store: &mut DaemonStore) -> Result<(), String> {
    open_gate(store)
}

/// A machine that has been asked and has not answered — the gated state, armed
/// against `target` so a test pins the gate without depending on what this
/// machine happens to have installed.
#[cfg(test)]
pub(crate) fn arm_model_gate(store: &mut DaemonStore, target: Option<&str>) -> Result<(), String> {
    let target = target.unwrap_or(UNKNOWN_TARGET);
    store.set_meta(MODEL_GATE_KEY, &format!("{GATE_ARMED_PREFIX}{target}"))
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
    Ok(match command {
        Command::Start(None) => start_text(route.as_ref()),
        Command::Start(Some(choice)) | Command::Settings(Some(choice)) => {
            let (text, picked) = choose_model(route.as_ref(), choice);
            // Written only on a pick that reached the file: a menu shown, or a
            // number off the end of it, must not open the gate.
            if picked {
                open_gate(store)?;
            }
            text
        }
        Command::ChooseFirst => first_run_text(route.as_ref()),
        Command::Settings(None) => settings_text(route.as_ref()),
        Command::Help => HELP.to_string(),
        Command::Clear => CLEAR.to_string(),
        Command::Status => status_text(store, account, envelope, route.as_ref())?,
        Command::Stop => stop_text(store, envelope, route.as_ref(), now_ms)?,
        Command::Resume => resume_text(store, queue, account, envelope, route.as_ref(), now_ms)?,
    })
}

const HELP: &str = "\
/start — what this is, and the model it answers on
/help — this list
/new, /clear — about conversation memory
/status — connection, model, and whether a turn is running
/stop — cancel the turn running now
/resume — retry a turn that failed to start
/model, /settings — show the model, or `/model 2` to change it";

/// `/new` and `/clear` both mean "forget what we said", and there is nothing to
/// forget: a channel turn is queued with the message as its only parameter, so
/// every message is already answered on its own. Saying so is the honest
/// answer; pretending to clear something would be a lie the next reply exposes.
const CLEAR: &str = "\
Nothing to clear — each message is answered on its own, with no memory of the \
ones before it.";

fn start_text(route: Option<&ChannelRoute>) -> String {
    format!(
        "Send a message and it is answered by this machine.\n\n{}\n\n{}",
        current_model_line(route),
        menu_text()
    )
}

/// The answer to a message that arrived before anyone picked a model.
///
/// Says to send the message again, because this one is not queued: it settled
/// as answered-by-the-daemon, and silently running it after the pick would
/// answer something the person may have moved on from.
fn first_run_text(route: Option<&ChannelRoute>) -> String {
    format!(
        "Pick a model first, then send your message again.\n\n{}\n\n{}",
        current_model_line(route),
        menu_text()
    )
}

fn settings_text(route: Option<&ChannelRoute>) -> String {
    format!("{}\n\n{}", current_model_line(route), menu_text())
}

/// The model the route's recipe names, read the same way the runner reads it.
fn current_model_line(route: Option<&ChannelRoute>) -> String {
    let Some(route) = route else {
        return "No task is routed to this conversation yet.".to_string();
    };
    let Ok(roots) = recipes::global_config_roots() else {
        return format!("Task: {}", route.target.recipe);
    };
    match recipes::resolve_recipe(&route.target.recipe, None, &roots) {
        Ok(recipe) => format!(
            "Answering on {} (task '{}').",
            super::describe_recipe_target(&recipe.target),
            route.target.recipe
        ),
        Err(error) => format!("Task '{}' cannot be read: {error}", route.target.recipe),
    }
}

/// Point the route's recipe at the model the sender picked.
///
/// The recipe file is what the runner resolves for the next message, so writing
/// it is the whole change — but it is also the recipe *every* conversation on
/// this route runs, and this is deliberately machine-wide rather than per-chat.
/// A route scoped to one conversation is how an operator narrows that, and it
/// is set from the desktop app or `monkey channels add-route`, not from here.
fn choose_model(route: Option<&ChannelRoute>, choice: usize) -> (String, bool) {
    let roots = match recipes::global_config_roots() {
        Ok(roots) => roots,
        Err(error) => {
            return (
                format!("Could not find this machine's tasks: {error}"),
                false,
            )
        }
    };
    apply_choice(route, choice, &roots, &catalogue())
}

/// The choice itself, against a given inventory and set of config roots — the
/// two things a test can supply and a chat cannot.
fn apply_choice(
    route: Option<&ChannelRoute>,
    choice: usize,
    roots: &[PathBuf],
    models: &[ModelChoice],
) -> (String, bool) {
    let Some(route) = route else {
        return (
            "No task is routed to this conversation yet, so there is no model to change."
                .to_string(),
            false,
        );
    };
    let Some(model) = choice.checked_sub(1).and_then(|index| models.get(index)) else {
        return (
            format!("No model {choice} in the list.\n\n{}", render(models)),
            false,
        );
    };
    match recipes::set_recipe_target(&route.target.recipe, None, roots, &model.target) {
        Ok(_) => (
            format!(
                "Now answering on {}. A turn already running keeps the model it started with.",
                model.label
            ),
            true,
        ),
        Err(error) => (format!("Could not change the model: {error}"), false),
    }
}

fn status_text(
    store: &mut DaemonStore,
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
        current_model_line(route),
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
    out.push_str("\nSend `/model 1` to answer on that one instead.");
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

    fn scratch_root() -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("lm-command-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(root.join("recipes")).expect("scratch root");
        root
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

    /// The pick has to reach the file the runner reads, or the next message is
    /// still answered by the old model and the confirmation was a lie.
    #[test]
    fn a_pick_rewrites_the_routed_recipes_model() {
        let root = scratch_root();
        let path = root.join("recipes").join("chat.yml");
        std::fs::write(
            &path,
            "version: 1\nname: \"chat\"\ntarget:\n  ollama: \"old-model\"\npermission_mode: plan\nprompt: |\n  {{message}}\n",
        )
        .expect("recipe");

        let (said, picked) = apply_choice(Some(&route_to("chat")), 2, &[root.clone()], &models());

        assert!(
            picked,
            "a pick that wrote the file must report itself: {said}"
        );
        assert!(said.contains("Local · llama"), "{said}");
        let raw = std::fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("managed_model: \"llama\""), "{raw}");
        assert!(!raw.contains("old-model"), "{raw}");
    }

    /// Armed against the model the route named: still nobody's choice, so the
    /// next message meets the menu rather than the model.
    #[test]
    fn an_armed_gate_stays_shut_while_the_model_is_the_one_it_armed_against() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .set_meta(MODEL_GATE_KEY, "pending:ollama:qwen")
            .expect("arm");

        assert!(!model_chosen(&mut store, Some("ollama:qwen")).expect("gate"));
    }

    #[test]
    fn a_pick_opens_the_gate_for_good() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        mark_model_chosen(&mut store).expect("mark");
        assert!(model_chosen(&mut store, Some("ollama:qwen")).expect("gate"));
    }

    /// A model chosen anywhere but a chat — the desktop app's picker, `monkey
    /// channels`, an operator editing the recipe — is still a choice, and none
    /// of those can write this database. The routed recipe naming something
    /// else is the evidence, and it is durable.
    #[test]
    fn a_model_changed_outside_the_chat_opens_the_gate() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        store
            .set_meta(MODEL_GATE_KEY, "pending:ollama:qwen")
            .expect("arm");

        assert!(model_chosen(&mut store, Some("managed:llama")).expect("gate"));
        // Recorded as open, so it stays open once the new model is the one a
        // fresh arm would have chosen.
        assert_eq!(
            store.get_meta(MODEL_GATE_KEY).expect("meta").as_deref(),
            Some(GATE_OPEN)
        );
        assert!(model_chosen(&mut store, Some("managed:llama")).expect("gate"));
    }

    /// An unreadable recipe must not be mistaken for a change: the gate stays
    /// shut rather than opening on a missing file.
    #[test]
    fn an_unreadable_recipe_does_not_open_the_gate() {
        let mut store = DaemonStore::open_in_memory().expect("open");
        arm_model_gate(&mut store, None).expect("arm");

        assert!(!model_chosen(&mut store, None).expect("gate"));
    }

    /// Arming reads this machine's own inventory, so the assertion is about the
    /// two states that follow from it: nothing installed means never gated,
    /// and anything installed means armed against the model in hand. Either
    /// way the answer is recorded, so the inventory is asked once.
    #[test]
    fn the_first_message_arms_or_opens_and_records_which() {
        let mut store = DaemonStore::open_in_memory().expect("open");

        let open = model_chosen(&mut store, Some("ollama:qwen")).expect("gate");
        let recorded = store.get_meta(MODEL_GATE_KEY).expect("meta");
        if open {
            assert_eq!(recorded.as_deref(), Some(GATE_OPEN), "{recorded:?}");
        } else {
            assert_eq!(recorded.as_deref(), Some("pending:ollama:qwen"));
        }
    }

    /// The complaint this came from: a machine already answering on an
    /// installed model was told to pick one before it would answer at all.
    /// A route naming a model the machine can run is not a machine with an
    /// unanswered question.
    #[test]
    fn a_route_already_naming_a_runnable_model_is_never_gated() {
        assert!(runnable_target(&models(), Some("managed:llama")));
        assert!(runnable_target(&models(), Some("ollama:qwen")));
    }

    /// And the case that is worth asking about: the recipe names something
    /// this machine cannot start, so every message would fail.
    #[test]
    fn a_route_naming_a_model_that_is_not_installed_is_asked_about() {
        assert!(!runnable_target(&models(), Some("managed:not-installed")));
        assert!(!runnable_target(&models(), None));
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

    /// A number outside the menu changes nothing and shows the menu again.
    #[test]
    fn a_choice_off_the_end_of_the_menu_changes_no_file() {
        let root = scratch_root();
        let path = root.join("recipes").join("chat.yml");
        let raw = "version: 1\nname: \"chat\"\ntarget:\n  ollama: \"old-model\"\npermission_mode: plan\nprompt: |\n  {{message}}\n";
        std::fs::write(&path, raw).expect("recipe");

        for choice in [0, 3] {
            let (said, picked) =
                apply_choice(Some(&route_to("chat")), choice, &[root.clone()], &models());
            assert!(said.contains("No model"), "{said}");
            // Nothing was chosen, so the first-message gate must stay shut.
            assert!(!picked, "{said}");
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read back"), raw);
    }
}

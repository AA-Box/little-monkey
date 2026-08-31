//! The model a channel route answers on, end to end.
//!
//! # What this proves
//!
//! That changing a route's model changes what the runner actually resolves —
//! not that a JSON object changed shape. Every step below is the production
//! one:
//!
//! ```text
//! ChannelEnvelope
//!   -> routing::resolve_route          (the daemon's own ladder)
//!   -> RouteTarget.recipe              (a *name*, resolved late)
//!   -> freeze_execution_for            (what the daemon calls on accept)
//!   -> recipes::resolve_recipe_with_path  (reads the real file off disk)
//!   -> FrozenExecutionContextV1.recipe_json
//!   -> task::resolve_recipe_chat_target   (what run_recipe calls, line for line)
//!   -> ResolvedTarget / chat::Target      (the backend the run will speak to)
//! ```
//!
//! The only thing not exercised is the HTTP call to that backend, which is
//! where the boundary belongs: `Target::Provider { provider_id, model }` and
//! `ResolvedTarget::ManagedModel { model_id }` *are* the backend selection, and
//! everything after them is somebody else's socket.
//!
//! The mutation in the middle is `recipes::set_recipe_target` — the exact
//! function the `recipes_set_target` Tauri command wraps, resolving the recipe
//! by the same `resolve_recipe_with_path` the freeze does, so there is no
//! second notion of "which file is this recipe" for the two sides to disagree
//! about.

use std::path::{Path, PathBuf};

use little_monkey_lib::channels::routing::{
    resolve_route, ChannelRoute, RouteScope, RouteTarget,
};
use little_monkey_lib::channels::types::{
    BoundedMetadata, ChannelConversation, ChannelEnvelope, ChannelKind, ChannelSender,
    ConversationKind,
};
use little_monkey_lib::recipes::{self, Recipe, RecipeTarget};

use crate::task::{resolve_recipe_chat_target, ResolvedTarget};

fn temp_root(label: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "little_monkey_route_model_{label}_{}_{n}_{nanos}",
        std::process::id()
    ));
    std::fs::create_dir_all(dir.join("recipes")).unwrap();
    dir
}

/// The starter recipe as `channels_cli::starter_recipe_yaml` writes it —
/// comments, block scalars and all, so a swap that reflows the file shows up
/// here rather than in somebody's editor.
fn write_starter_recipe(root: &Path, target_block: &str) {
    let content = format!(
        "# The task a channel message runs as. Yours to edit.\n\
         version: 1\n\
         name: \"channel-chat\"\n\
         description: \"Answer a message that arrived on a messaging channel.\"\n\
         target:\n\
         {target_block}\
         permission_mode: plan\n\
         system: |\n\
         \x20 You are answering a person who messaged this machine.\n\
         \x20 Send your answer by calling the send_message tool.\n\
         prompt: |\n\
         \x20 {{{{message}}}}\n\
         params:\n\
         \x20 \"message\": \"\"\n"
    );
    std::fs::write(root.join("recipes").join("channel-chat.yml"), content).unwrap();
}

fn envelope() -> ChannelEnvelope {
    ChannelEnvelope {
        account_id: "acct-1".into(),
        kind: ChannelKind::Slack,
        provider_event_id: "evt-1".into(),
        provider_message_id: None,
        conversation: ChannelConversation {
            conversation_id: "C1".into(),
            kind: ConversationKind::Channel,
            thread_id: Some("T1".into()),
            title: None,
        },
        sender: ChannelSender::new("U1"),
        text: "what model are you?".into(),
        attachments: Vec::new(),
        reply_to_provider_id: None,
        mentions_self: true,
        received_at_ms: 5,
        metadata: BoundedMetadata::new(),
    }
}

fn global_route(recipe: &str) -> ChannelRoute {
    ChannelRoute {
        route_id: "route-global".into(),
        scope: RouteScope::default(),
        target: RouteTarget::new(recipe),
        enabled: true,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

/// Drive the whole chain once and report what the run would actually talk to.
///
/// Returns `(frozen model_target line, the resolved backend, the frozen
/// recipe)` — the middle one being what `run_recipe` branches on.
fn resolve_through_the_daemon(
    routes: &[ChannelRoute],
    roots: &[PathBuf],
) -> (String, ResolvedTarget, Recipe) {
    let route = resolve_route(routes, &envelope()).expect("the global route matches everything");
    let frozen = super::freeze_execution_for(&route.target, Some(&route.route_id), roots, None)
        .expect("the daemon freezes this turn");
    let v1 = frozen.as_v1();
    // Exactly what `enqueue` does with a frozen context before it runs.
    assert!(
        v1.recipe_matches_digest(),
        "the frozen recipe must match its own digest or the run is refused"
    );
    let recipe: Recipe =
        serde_json::from_str(&v1.recipe_json).expect("the frozen recipe is readable");
    let resolved = resolve_recipe_chat_target(&recipe).expect("the runner resolves the target");
    (v1.model_target.clone(), resolved, recipe)
}

fn backend_description(resolved: &ResolvedTarget) -> String {
    match resolved {
        ResolvedTarget::ManagedModel { model_id } => format!("managed:{model_id}"),
        ResolvedTarget::Ready(crate::chat::Target::Provider { provider_id, model }) => {
            format!("provider:{provider_id}/{model}")
        }
        ResolvedTarget::Ready(crate::chat::Target::Local {
            model,
            native_ollama,
            ..
        }) => format!(
            "local:{}:{}",
            if *native_ollama { "ollama" } else { "openai" },
            model.clone().unwrap_or_default()
        ),
    }
}

/// The headline property: a route resolves model A, the model is changed to B
/// through the same call the UI makes, and the *next* message resolves B.
///
/// Both directions of the swap are checked — managed to Ollama and back — so a
/// test that passed because the second resolution simply re-read a stale file
/// could not stay green.
#[test]
fn changing_a_routes_model_changes_what_the_runner_resolves() {
    let root = temp_root("swap");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");
    let routes = vec![global_route("channel-chat")];

    let (frozen, resolved, _) = resolve_through_the_daemon(&routes, &roots);
    assert_eq!(frozen, "managed:Qwen2.5-7B-Instruct");
    assert_eq!(backend_description(&resolved), "managed:Qwen2.5-7B-Instruct");

    // The operator picks a different model. This is the function the
    // `recipes_set_target` command wraps, nothing test-only.
    recipes::set_recipe_target(
        "channel-chat",
        None,
        &roots,
        &RecipeTarget {
            ollama: Some("qwen3:8b".into()),
            ..Default::default()
        },
    )
    .expect("the new target is written");

    let (frozen, resolved, _) = resolve_through_the_daemon(&routes, &roots);
    assert_eq!(frozen, "ollama:qwen3:8b");
    assert_eq!(backend_description(&resolved), "local:ollama:qwen3:8b");

    // And back, so neither direction is the one that happens to work.
    recipes::set_recipe_target(
        "channel-chat",
        None,
        &roots,
        &RecipeTarget {
            managed_model: Some("Llama-3.1-8B-Instruct".into()),
            ..Default::default()
        },
    )
    .expect("the target is written back");
    let (frozen, resolved, _) = resolve_through_the_daemon(&routes, &roots);
    assert_eq!(frozen, "managed:Llama-3.1-8B-Instruct");
    assert_eq!(
        backend_description(&resolved),
        "managed:Llama-3.1-8B-Instruct"
    );
}

/// Each of the three target kinds the picker can produce, all the way to the
/// backend the run would speak to.
///
/// A provider target additionally has to freeze a credential reference: the
/// run needs a key, and a target swap that forgot to name one would fail at
/// execution with the provider's own 401 instead of anything actionable.
#[test]
fn every_target_kind_the_picker_offers_resolves_to_its_own_backend() {
    let root = temp_root("kinds");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");
    let routes = vec![global_route("channel-chat")];

    for (target, expected_frozen, expected_backend, expects_credential) in [
        (
            RecipeTarget {
                managed_model: Some("Qwen2.5-7B-Instruct".into()),
                ..Default::default()
            },
            "managed:Qwen2.5-7B-Instruct",
            "managed:Qwen2.5-7B-Instruct",
            false,
        ),
        (
            RecipeTarget {
                ollama: Some("qwen3:8b".into()),
                ..Default::default()
            },
            "ollama:qwen3:8b",
            "local:ollama:qwen3:8b",
            false,
        ),
        (
            RecipeTarget {
                provider: Some("openrouter".into()),
                model: Some("anthropic/claude-sonnet-4".into()),
                ..Default::default()
            },
            "provider:openrouter/anthropic/claude-sonnet-4",
            "provider:openrouter/anthropic/claude-sonnet-4",
            true,
        ),
    ] {
        recipes::set_recipe_target("channel-chat", None, &roots, &target)
            .unwrap_or_else(|error| panic!("{expected_frozen} must be writable: {error}"));

        let route = resolve_route(&routes, &envelope()).unwrap();
        let frozen =
            super::freeze_execution_for(&route.target, Some(&route.route_id), &roots, None).unwrap();
        let v1 = frozen.as_v1();
        assert_eq!(v1.model_target, expected_frozen);
        assert_eq!(
            v1.credential_ref.is_some(),
            expects_credential,
            "credential_ref for {expected_frozen}"
        );

        let recipe: Recipe = serde_json::from_str(&v1.recipe_json).unwrap();
        assert_eq!(recipe.target, target);
        assert_eq!(
            backend_description(&resolve_recipe_chat_target(&recipe).unwrap()),
            expected_backend
        );
    }
}

/// Changing the model changes the model and nothing else.
///
/// The prompt, the system prompt, the permission mode and the declared params
/// are what make a channel task safe to run on a stranger's text — a swap that
/// quietly reset `permission_mode: plan` to the default would hand an
/// unattended run more tools, and nothing in the UI would say so.
#[test]
fn changing_the_model_leaves_every_other_recipe_field_alone() {
    let root = temp_root("fields");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");
    let routes = vec![global_route("channel-chat")];

    let (_, _, before) = resolve_through_the_daemon(&routes, &roots);
    let raw_before =
        std::fs::read_to_string(root.join("recipes").join("channel-chat.yml")).unwrap();

    recipes::set_recipe_target(
        "channel-chat",
        None,
        &roots,
        &RecipeTarget {
            provider: Some("openrouter".into()),
            model: Some("anthropic/claude-sonnet-4".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let (_, _, after) = resolve_through_the_daemon(&routes, &roots);
    assert_ne!(before.target, after.target, "the model did change");
    assert_eq!(before.name, after.name);
    assert_eq!(before.description, after.description);
    assert_eq!(before.permission_mode, after.permission_mode);
    assert_eq!(before.system, after.system);
    assert_eq!(before.prompt, after.prompt);
    assert_eq!(before.params, after.params);
    assert_eq!(before.workspace, after.workspace);
    assert_eq!(before.output.json, after.output.json);
    assert_eq!(before.max_iterations, after.max_iterations);
    assert_eq!(before.timeout_seconds, after.timeout_seconds);

    // And at the byte level. Everything before the `target:` line and
    // everything from the next top-level key onwards is the operator's file,
    // byte for byte — comment, block scalars, quoting and all. Re-serializing
    // the recipe through its struct would have quietly rewritten all of it.
    let raw_after = std::fs::read_to_string(root.join("recipes").join("channel-chat.yml")).unwrap();
    let split = |raw: &str| {
        let (head, rest) = raw.split_once("target:\n").expect("the file has a target block");
        let tail = rest
            .split_once("permission_mode:")
            .expect("the file has a key after its target block")
            .1
            .to_string();
        (head.to_string(), tail)
    };
    let (head_before, tail_before) = split(&raw_before);
    let (head_after, tail_after) = split(&raw_after);
    assert_eq!(head_before, head_after, "{raw_after}");
    assert_eq!(tail_before, tail_after, "{raw_after}");
    assert!(
        raw_after.contains("target:\n  provider: \"openrouter\"\n  model: \"anthropic/claude-sonnet-4\"\n"),
        "{raw_after}"
    );
    assert!(!raw_after.contains("managed_model"), "{raw_after}");
}

/// Two routes naming one recipe share one model — deliberately, because the
/// model lives in the recipe and a route names a recipe by name.
///
/// This is asserted rather than assumed: it is the product behaviour the UI
/// has to warn about, and if it ever stops being true the warning becomes a
/// lie.
#[test]
fn routes_sharing_a_recipe_share_its_model() {
    let root = temp_root("shared");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");

    let mut second = global_route("channel-chat");
    second.route_id = "route-account".into();
    second.scope = RouteScope {
        account_id: Some("acct-1".into()),
        ..Default::default()
    };
    let routes = vec![global_route("channel-chat"), second.clone()];

    recipes::set_recipe_target(
        "channel-chat",
        None,
        &roots,
        &RecipeTarget {
            ollama: Some("qwen3:8b".into()),
            ..Default::default()
        },
    )
    .unwrap();

    // The account route wins the ladder for this envelope; the global one is
    // frozen directly to show it moved too.
    let (frozen, _, _) = resolve_through_the_daemon(&routes, &roots);
    assert_eq!(frozen, "ollama:qwen3:8b");
    let global_only = vec![global_route("channel-chat")];
    let (frozen, _, _) = resolve_through_the_daemon(&global_only, &roots);
    assert_eq!(frozen, "ollama:qwen3:8b");
}

/// A recipe of its own keeps a model of its own: the swap is scoped to the
/// file the route actually names.
#[test]
fn a_route_with_its_own_recipe_keeps_its_own_model() {
    let root = temp_root("split");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");
    std::fs::write(
        root.join("recipes").join("channel-triage.yml"),
        "version: 1\nname: \"channel-triage\"\ntarget:\n  ollama: \"qwen3:8b\"\npermission_mode: plan\nprompt: \"{{message}}\"\nparams:\n  \"message\": \"\"\n",
    )
    .unwrap();

    recipes::set_recipe_target(
        "channel-chat",
        None,
        &roots,
        &RecipeTarget {
            provider: Some("openrouter".into()),
            model: Some("anthropic/claude-sonnet-4".into()),
            ..Default::default()
        },
    )
    .unwrap();

    let (chat, _, _) = resolve_through_the_daemon(&[global_route("channel-chat")], &roots);
    let (triage, _, _) = resolve_through_the_daemon(&[global_route("channel-triage")], &roots);
    assert_eq!(chat, "provider:openrouter/anthropic/claude-sonnet-4");
    assert_eq!(triage, "ollama:qwen3:8b", "the other recipe did not move");
}

/// A target the XOR refuses never reaches the file.
///
/// The check is in `RecipeTarget::validate`, before the read, so a rejected
/// swap cannot leave a half-written recipe behind — and the route goes on
/// resolving what it resolved before.
#[test]
fn an_invalid_target_is_refused_before_anything_is_written() {
    let root = temp_root("invalid");
    let roots = vec![root.clone()];
    write_starter_recipe(&root, "  managed_model: \"Qwen2.5-7B-Instruct\"\n");
    let routes = vec![global_route("channel-chat")];
    let before = std::fs::read_to_string(root.join("recipes").join("channel-chat.yml")).unwrap();

    for invalid in [
        RecipeTarget::default(),
        RecipeTarget {
            ollama: Some("qwen3:8b".into()),
            managed_model: Some("Qwen2.5-7B-Instruct".into()),
            ..Default::default()
        },
        RecipeTarget {
            provider: Some("openrouter".into()),
            ..Default::default()
        },
    ] {
        assert!(recipes::set_recipe_target("channel-chat", None, &roots, &invalid).is_err());
    }

    assert_eq!(
        std::fs::read_to_string(root.join("recipes").join("channel-chat.yml")).unwrap(),
        before
    );
    let (frozen, _, _) = resolve_through_the_daemon(&routes, &roots);
    assert_eq!(frozen, "managed:Qwen2.5-7B-Instruct");
}

/// A route naming a recipe that is not on disk fails at the freeze, which is
/// where the operator can be told about it — not silently on some other model.
#[test]
fn a_route_naming_a_missing_recipe_freezes_nothing() {
    let root = temp_root("missing");
    let roots = vec![root.clone()];
    let route = global_route("not-saved");
    let error =
        super::freeze_execution_for(&route.target, Some(&route.route_id), &roots, None).unwrap_err();
    assert!(error.contains("not-saved"), "{error}");
}

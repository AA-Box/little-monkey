//! LM's terminal agent: same sandboxed file/shell tools and permission
//! model as the desktop app (reused directly from `little_monkey_lib`), driven from
//! a shell instead of a WebView. Supports both a one-shot invocation
//! (`monkey MODEL "prompt"`) and an interactive REPL (`monkey MODEL`, no prompt).
//! `monkey pull/run` use Little Monkey's app-owned model store and bundled
//! llama-server by default; explicit `--provider ollama` and the remaining
//! Ollama-compatible management commands retain their daemon behavior. A bare
//! `monkey` prints the subcommand overview (see [`is_bare_invocation`]).

mod acp;
mod agent;
mod chat;
mod checkpoints_cli;
mod cmds;
mod daemon;
mod durable_run;
mod embed_cli;
mod managed_model_cli;
mod mcp_cli;
mod modelfile;
mod ollama_api;
mod permission;
mod plugins_cli;
mod providers_cli;
mod repl;
mod security_cli;
mod skills_cli;
mod sse;
mod stacks_cli;
mod task;
mod tools_cli;
mod tools_def;
mod verify_cli;
mod web_cli;
mod workflow_cli;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::prompts::{self, PromptEntry};
use little_monkey_lib::workspace::{self, WorkspaceRoot};
use little_monkey_lib::{memory, rules, AppState};

use permission::{PermissionMode, TerminalPermissions};

#[derive(Parser, Debug)]
#[command(name = "monkey", bin_name = "monkey", version, about)]
struct Cli {
    /// Ollama-style model management subcommand. Note: a bare first argument
    /// matching a subcommand name (e.g. `monkey list`) parses as that
    /// subcommand, not as a prompt — quote-and-rephrase such prompts.
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Model name in the short form (`monkey llama3.2 "prompt"`), or the
    /// prompt when a legacy explicit target flag already supplies a model.
    #[arg(value_name = "MODEL")]
    model_or_prompt: Option<String>,

    /// One-shot prompt for the short model-first form. Omit it to start an
    /// interactive REPL with the selected model.
    #[arg(value_name = "PROMPT")]
    prompt: Option<String>,

    /// Workspace root the agent's tools are sandboxed to. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH", global = true)]
    workspace: Option<PathBuf>,

    /// Provider id (for example ollama, managed-llama, openai, anthropic,
    /// gemini, openrouter, or a custom provider id). Use only to override or
    /// disambiguate automatic local-first model resolution.
    #[arg(long)]
    provider: Option<String>,

    /// Compatibility form for a model id. Prefer the first positional model:
    /// `monkey [--provider ID] MODEL [PROMPT]`.
    #[arg(long)]
    model: Option<String>,

    /// Compatibility alias for `--provider ollama MODEL`.
    #[arg(long)]
    ollama: Option<String>,

    /// Talk to a local OpenAI-compatible server (llama-server, etc.) at
    /// this base URL, e.g. http://127.0.0.1:8090.
    #[arg(long)]
    local_url: Option<String>,

    /// Permission mode: manual (default, prompts every mutation),
    /// acceptEdits, smart (auto-approves write_file/edit_file unless the
    /// path is sensitive — see permissions::path_risk_floor — run_shell
    /// always prompts), plan (read-only; call present_plan and approve at
    /// its prompt to switch to acceptEdits), auto, or bypass. Matches the
    /// desktop app's modes.
    #[arg(long, default_value = "manual", global = true)]
    permission_mode: String,

    /// Skip auto-loading MONKEY.md rules and remembered facts into the
    /// system prompt. Without this flag, global + workspace MONKEY.md files
    /// and remembered facts (see `rules.rs`/`memory.rs`) are composed into a
    /// default system prompt every invocation; any `--system` given is
    /// appended after that section rather than replacing it.
    #[arg(long, global = true)]
    no_rules: bool,

    /// Skip loading MCP servers from `mcp_servers.json` (the same config
    /// file the desktop app's Settings > MCP tab writes). Without this
    /// flag, every server with `enabled: true` there is connected at
    /// startup and its tools merged into the model's tool set, namespaced
    /// `mcp__<serverId>__<toolName>` — same default-on-if-configured
    /// behavior as rules/facts (see `--no-rules`), just for MCP instead.
    #[arg(long, global = true)]
    no_mcp: bool,

    /// Slash-command of a saved persona (see the desktop app's Settings >
    /// Prompts tab, or `/prompts` in the REPL) to append to the system
    /// prompt, e.g. `--persona code-reviewer`. Resolved against the same
    /// `prompts.json` the GUI reads/writes (`little_monkey_lib::prompts`);
    /// an unrecognized command is a startup error. Composed the same
    /// "append, never replace" way as the desktop app's per-session
    /// persona (see `compose_persona_and_system`): if `--system` is also
    /// given, the persona's content is appended first and `--system` keeps
    /// the final say, mirroring how `run`'s own `--system` already
    /// overrides one given before the subcommand.
    #[arg(long, global = true)]
    persona: Option<String>,

    /// Attach a knowledge stack (by name, as created in the desktop app's
    /// Settings > Knowledge tab) so the agent gains the `search_docs` tool
    /// this turn — repeatable for more than one. Mirrors the desktop app's
    /// `StackPicker` attachment: the tool is only offered at all when at
    /// least one `--stack` is given (see `tools_def::search_docs_tool_def`),
    /// and its description lists exactly the names given here.
    #[arg(long = "stack", global = true, value_name = "NAME")]
    stack: Vec<String>,

    #[command(flatten)]
    chat: ChatFlags,
}

/// Generation options shared by the flat invocation and `run` (declared
/// global so they parse after subcommands too, like --workspace). The
/// Ollama-only ones (--keepalive, --think) warn and are ignored on
/// OpenAI-compatible targets; managed `run` consumes --num-ctx while starting
/// its app-owned server.
#[derive(Args, Debug)]
struct ChatFlags {
    /// Sampling temperature (0 = deterministic-ish)
    #[arg(long, global = true)]
    temperature: Option<f64>,

    /// Nucleus sampling cutoff
    #[arg(long, global = true)]
    top_p: Option<f64>,

    /// Random seed for reproducible generation
    #[arg(long, global = true)]
    seed: Option<i64>,

    /// Stop sequence (repeatable)
    #[arg(long, global = true, value_name = "SEQUENCE")]
    stop: Vec<String>,

    /// Context window size (Ollama requests or managed run server startup)
    #[arg(long, global = true)]
    num_ctx: Option<i64>,

    /// Max tokens to generate (OpenAI targets: max_tokens)
    #[arg(long, global = true)]
    num_predict: Option<i64>,

    /// System prompt overriding the model's default. (Not `global` like its
    /// siblings: `show` has its own `--system` section flag, and clap can't
    /// share a name between a subcommand arg and a propagated global — `run`
    /// declares its own copy instead.)
    #[arg(long)]
    system: Option<String>,

    /// Constrain output: "json", an inline JSON schema, or @schema-file
    #[arg(long, global = true, value_name = "FORMAT")]
    format: Option<String>,

    /// Enable thinking; optionally =true/false/low/medium/high
    /// (Ollama targets only)
    #[arg(long, global = true, value_name = "LEVEL", num_args = 0..=1,
          default_missing_value = "true", require_equals = true)]
    think: Option<String>,

    /// Receive thinking output but don't print it
    #[arg(long = "hidethinking", global = true)]
    hidethinking: bool,

    /// How long the model stays loaded after the request, e.g. 5m, 0, -1
    /// (Ollama targets only)
    #[arg(long = "keepalive", global = true, value_name = "DURATION")]
    keepalive: Option<String>,

    /// Print timing/token metrics after each response
    #[arg(long, global = true)]
    verbose: bool,

    /// Attach image files (png/jpg/jpeg/webp paths in the prompt) on
    /// OpenAI-compatible targets. Ollama targets attach automatically when
    /// the model reports vision support; without this flag, other targets
    /// receive the prompt verbatim.
    #[arg(long = "attach-images", global = true)]
    attach_images: bool,

    /// Run the current workspace's configured verification commands (see
    /// the desktop app's Settings > Verification tab, or `/verify` in the
    /// REPL) automatically after any turn that writes files, feeding the
    /// first failure back to the model as a fix instruction for up to
    /// `agent::DEFAULT_VERIFY_MAX_ROUNDS` rounds — a Rust port of the
    /// desktop app's `verifyEnabled` setting/`runVerificationPhase`
    /// (`src/lib/agentLoop.ts`). Off by default: running arbitrary
    /// configured shell automatically should be opt-in, same posture as the
    /// GUI's `verifyEnabled` default.
    #[arg(long, global = true)]
    verify: bool,

    /// Explicitly disable verification for this invocation — mostly useful
    /// to override a `--verify` given earlier on the command line (e.g. a
    /// shell alias); redundant with the default otherwise.
    #[arg(long = "no-verify", global = true)]
    no_verify: bool,

    /// Offer the `task` tool this turn, letting the model delegate a scoped
    /// subtask to a subagent with an explicit explore or code profile — CLI parity for the
    /// desktop app's Subagents feature (docs/roadmap/p3-subagents.md slice
    /// 5). Off by default, same posture as `--verify`/the GUI's
    /// `subagentsEnabled` toggle: running an extra model-initiated loop
    /// should be opt-in. Code-profile writes reuse the parent turn checkpoint
    /// and normal permission gate; neither profile can delegate recursively.
    #[arg(long, global = true)]
    subagents: bool,
}

impl ChatFlags {
    /// Validates the raw flag strings and converts them into `ChatOptions`.
    fn to_options(&self) -> Result<chat::ChatOptions, String> {
        Ok(chat::ChatOptions {
            temperature: self.temperature,
            top_p: self.top_p,
            seed: self.seed,
            stop: self.stop.clone(),
            num_ctx: self.num_ctx,
            num_predict: self.num_predict,
            system: self.system.clone(),
            format: self
                .format
                .as_deref()
                .map(chat::parse_format_flag)
                .transpose()?,
            think: self
                .think
                .as_deref()
                .map(chat::parse_think_flag)
                .transpose()?,
            hide_thinking: self.hidethinking,
            keep_alive: self.keepalive.clone(),
            effort: None,
            verbose: self.verbose,
            attach_images: self.attach_images,
            verify: self.verify && !self.no_verify,
            verify_max_rounds: None,
            subagents: self.subagents,
            memory_enabled: None,
            quiet: false,
        })
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Agent Client Protocol v1 server over newline-delimited JSON-RPC stdio.
    /// The IDE is a client only; Little Monkey retains approval authority.
    Acp,
    /// List models available locally
    List,
    /// List running models
    Ps,
    /// Install a verified public GGUF into Little Monkey's app-owned model store
    Pull {
        /// Ollama tag, or pinned Hugging Face GGUF reference
        model: String,
        /// Legacy Ollama daemon only; managed installs reject insecure transport
        #[arg(long)]
        insecure: bool,
    },
    /// Remove one or more models
    Rm {
        #[arg(required = true)]
        models: Vec<String>,
    },
    /// Copy a model
    Cp { source: String, destination: String },
    /// Show information for a model
    Show {
        model: String,
        /// Show the Modelfile of a model
        #[arg(long)]
        modelfile: bool,
        /// Show parameters of a model
        #[arg(long)]
        parameters: bool,
        /// Show template of a model
        #[arg(long)]
        template: bool,
        /// Show system message of a model
        #[arg(long)]
        system: bool,
        /// Show license of a model
        #[arg(long)]
        license: bool,
    },
    /// Stop a running model
    Stop { model: String },
    /// Push a model to a registry
    Push {
        model: String,
        /// Use an insecure registry
        #[arg(long)]
        insecure: bool,
    },
    /// Create a model from a Modelfile
    Create {
        model: String,
        /// Path of the Modelfile to use
        #[arg(short = 'f', long = "file", default_value = "Modelfile")]
        file: PathBuf,
        /// Quantize the created model (e.g. q4_K_M)
        #[arg(short = 'q', long)]
        quantize: Option<String>,
    },
    /// Sign in to ollama.com (runs the ollama binary)
    Signin,
    /// Sign out of ollama.com (runs the ollama binary)
    Signout,
    /// Start the ollama daemon (runs the ollama binary)
    Serve,
    /// Install and chat with a public GGUF using Little Monkey's bundled runtime
    Run {
        /// Ollama tag, or pinned Hugging Face GGUF reference
        model: String,
        /// One-shot prompt. Omit to start an interactive REPL instead.
        prompt: Option<String>,
        /// System prompt overriding the model's default
        #[arg(long)]
        system: Option<String>,
    },
    /// Revert a checkpoint's file changes (defaults to the most recent one
    /// from this CLI). Prints the restored-file count.
    Revert {
        /// Checkpoint id to revert; omit for the most recent CLI checkpoint.
        id: Option<String>,
    },
    /// Run the local OpenAI-compatible API server headlessly (no GUI),
    /// reusing the exact same routing/proxy core the desktop app's Settings
    /// > API Server toggle drives (`little_monkey_lib::server`). Reads and
    /// writes the SAME `api_server.json`/`providers.json` the GUI does — a
    /// token minted in Settings works here, and vice versa (see the design
    /// doc's "config drift" risk note). Runs until interrupted (Ctrl+C).
    ApiServe {
        /// Port to bind on 127.0.0.1 — defaults to whatever's saved in
        /// `api_server.json` (1234 if nothing's been configured yet).
        /// Overriding it here does NOT persist back to the config file.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Knowledge Stacks parity (RAG design doc, slice 4): list stacks
    /// created in the desktop app's Settings > Knowledge tab, or reindex
    /// one by name. Read-only management stays in the GUI — no
    /// create/delete/add-source here.
    #[command(subcommand)]
    Stacks(StacksCmd),
    /// Saved recipe (YAML/JSON) headless runner — CI-suitable, machine
    /// readable output, deterministic exit codes. Design doc:
    /// docs/roadmap/p3-scheduled-automation.md.
    #[command(subcommand)]
    Task(TaskCmd),
    /// Validate, run, inspect, and replay the same typed workflows as the
    /// desktop visual editor.
    #[command(subcommand)]
    Workflow(workflow_cli::WorkflowCmd),
    /// Explicitly installed persistent local background-agent service.
    #[command(subcommand)]
    Daemon(daemon::DaemonCmd),
    /// Discover, preview, install, update, disable, and roll back data-only SKILL.md skills.
    #[command(subcommand)]
    Skills(skills_cli::SkillsCmd),
    /// Inspect the same declarative plugin runtime and health aggregate as the desktop app.
    #[command(subcommand)]
    Plugins(plugins_cli::PluginsCmd),
    /// Inspect local security posture and apply narrowly-scoped safe fixes.
    #[command(subcommand)]
    Security(security_cli::SecurityCmd),
}

#[derive(Subcommand, Debug)]
enum TaskCmd {
    /// Runs a saved recipe (by name, resolved via `.littlemonkey/recipes/`
    /// in the current directory or the global recipes directory, or a
    /// direct file path) headlessly. Exit codes: 0 success, 1
    /// config/transport error, 2 permission-denied or Plan-Mode-blocked, 3
    /// timeout or iteration-cap reached.
    Run {
        /// Recipe name or file path.
        name_or_path: String,
        /// Parameter override in `key=value` form — repeatable. Must match
        /// a param the recipe actually declares.
        #[arg(long = "param", value_name = "KEY=VALUE")]
        param: Vec<String>,
        /// Stable caller-owned idempotency key for crash/retry safety. The
        /// raw value is never stored; the ledger receives its SHA-256 digest.
        /// Falls back to LITTLE_MONKEY_RUN_KEY, then a fresh per-invocation key.
        #[arg(long, value_name = "KEY")]
        run_key: Option<String>,
        /// Emit a single JSON result object on stdout instead of plain text.
        #[arg(long)]
        json: bool,
    },
    /// Parses and validates a recipe file without running it.
    Validate {
        /// Path to the recipe file.
        path: String,
    },
    /// Normalizes and compares desktop/CLI durable event streams from one
    /// JSON fixture (`{"desktop":[...],"cli":[...]}`). Volatile authority
    /// metadata and model-delta chunk boundaries do not count as differences.
    Conformance {
        /// Path to the conformance fixture JSON.
        fixture: String,
    },
    /// Lists every recipe visible from the current directory (its own
    /// `.littlemonkey/recipes/`) plus the global recipes directory.
    List,
    /// Emits a ready-to-install launchd plist (macOS) or crontab line for
    /// running this recipe on a schedule outside the app — always prints,
    /// never installs anything itself (design doc slice 4, optional;
    /// ROADMAP.md §4 explicitly rules out the app self-daemonizing).
    Schedule {
        /// Recipe name or file path.
        name_or_path: String,
        /// Cron expression (croner syntax — see `automations::cron_validate`).
        #[arg(long)]
        cron: String,
    },
}

#[derive(Subcommand, Debug)]
enum StacksCmd {
    /// List every knowledge stack: name, source count, chunk/indexed state,
    /// and embedding model.
    List,
    /// Reindex a stack by name, incrementally skipping any file whose
    /// content hasn't changed since the last index.
    Reindex {
        /// Stack name, matched case-insensitively.
        name: String,
    },
    /// Manage the embeddings-only `llama-server` instance a `llama`-backend
    /// stack's `reindex`/`search_docs` needs reachable on port 8091 — see
    /// `embed_cli.rs`'s module doc for why this exists as a pid-file-based
    /// subcommand rather than reusing the desktop app's in-memory lifecycle.
    #[command(subcommand)]
    EmbedServer(EmbedServerCmd),
}

#[derive(Subcommand, Debug)]
enum EmbedServerCmd {
    /// Start the embeddings server against a downloaded GGUF model file,
    /// waiting until it's healthy and verified before returning. Leaves the
    /// process running in the background for subsequent `monkey-cli` invocations.
    Start {
        /// Path to a downloaded embedding model's GGUF file (e.g. one
        /// downloaded via the desktop app's Settings > Knowledge tab).
        #[arg(long = "model-path")]
        model_path: String,
    },
    /// Stop the embeddings server, if running.
    Stop,
    /// Report whether the embeddings server is currently running.
    Status,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FlatInvocation {
    model: Option<String>,
    prompt: Option<String>,
}

fn flat_invocation(cli: &Cli) -> Result<FlatInvocation, String> {
    let explicit_targets = usize::from(cli.provider.is_some())
        + usize::from(cli.ollama.is_some())
        + usize::from(cli.local_url.is_some());
    if explicit_targets > 1 {
        return Err("Choose only one of --provider, --ollama, or --local-url".to_string());
    }
    let reject_extra = || {
        if cli.prompt.is_some() {
            Err("Too many positional arguments for the legacy target form".to_string())
        } else {
            Ok(())
        }
    };

    if let Some(model) = &cli.ollama {
        reject_extra()?;
        return Ok(FlatInvocation {
            model: Some(model.clone()),
            prompt: cli.model_or_prompt.clone(),
        });
    }
    if cli.provider.is_some() {
        if let Some(model) = &cli.model {
            reject_extra()?;
            return Ok(FlatInvocation {
                model: Some(model.clone()),
                prompt: cli.model_or_prompt.clone(),
            });
        }
        return Ok(FlatInvocation {
            model: cli.model_or_prompt.clone(),
            prompt: cli.prompt.clone(),
        });
    }
    if cli.local_url.is_some() {
        if let Some(model) = &cli.model {
            reject_extra()?;
            return Ok(FlatInvocation {
                model: Some(model.clone()),
                prompt: cli.model_or_prompt.clone(),
            });
        }
        // One positional keeps the historical `--local-url URL "prompt"`
        // form (the server may expose one implicit model); two positionals
        // use the new MODEL PROMPT form.
        return if cli.prompt.is_some() {
            Ok(FlatInvocation {
                model: cli.model_or_prompt.clone(),
                prompt: cli.prompt.clone(),
            })
        } else {
            Ok(FlatInvocation {
                model: None,
                prompt: cli.model_or_prompt.clone(),
            })
        };
    }
    if let Some(model) = &cli.model {
        reject_extra()?;
        return Ok(FlatInvocation {
            model: Some(model.clone()),
            prompt: cli.model_or_prompt.clone(),
        });
    }
    Ok(FlatInvocation {
        model: cli.model_or_prompt.clone(),
        prompt: cli.prompt.clone(),
    })
}

fn native_ollama_target(model: String) -> chat::Target {
    chat::Target::Local {
        base_url: ollama_api::host(),
        model: Some(model),
        native_ollama: true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelCommandBackend {
    Managed,
    Ollama,
}

/// `pull` and `run` are app-owned by default. The explicit provider override
/// is the compatibility boundary for callers that intentionally want the
/// legacy Ollama daemon behavior.
fn model_command_backend(cli: &Cli) -> Result<ModelCommandBackend, String> {
    if cli.ollama.is_some() {
        return Ok(ModelCommandBackend::Ollama);
    }
    match cli.provider.as_deref() {
        None | Some("managed-llama" | "llama" | "llama.cpp") => {
            Ok(ModelCommandBackend::Managed)
        }
        Some("ollama") => Ok(ModelCommandBackend::Ollama),
        Some(provider) => Err(format!(
            "`monkey pull/run` supports the app-owned managed runtime or explicit `--provider ollama`, not provider '{provider}'"
        )),
    }
}

fn ollama_has_model(tags: &ollama_api::TagsResp, model: &str) -> bool {
    let wanted = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    tags.models
        .iter()
        .any(|entry| entry.name == model || entry.name == wanted)
}

async fn providers_with_model(model: &str) -> Vec<String> {
    let custom = providers_cli::load_custom_providers();
    let mut ids = little_monkey_lib::providers::providers_list_presets()
        .into_iter()
        .map(|preset| preset.id)
        .collect::<BTreeSet<_>>();
    ids.extend(custom.iter().map(|provider| provider.id.clone()));
    let checks = ids.into_iter().map(|provider_id| {
        let custom = custom.clone();
        let model = model.to_string();
        async move {
            let base_url =
                little_monkey_lib::providers::resolve_base_url(&provider_id, &custom).ok()?;
            let key = little_monkey_lib::providers::read_key_with_env(&provider_id).ok()?;
            let models = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                little_monkey_lib::providers::fetch_models(&base_url, &provider_id, &key),
            )
            .await
            .ok()?
            .ok()?;
            models
                .iter()
                .any(|entry| entry.id == model)
                .then_some(provider_id)
        }
    });
    let mut matches = futures_util::future::join_all(checks)
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    matches.sort();
    matches
}

async fn resolve_target(
    cli: &Cli,
    invocation: &FlatInvocation,
    client: &reqwest::Client,
) -> Result<chat::Target, String> {
    if let Some(provider) = &cli.provider {
        let model = invocation
            .model
            .clone()
            .ok_or("--provider requires a positional MODEL (or legacy --model)")?;
        return match provider.as_str() {
            "ollama" => Ok(native_ollama_target(model)),
            "managed-llama" | "llama" | "llama.cpp" => Ok(chat::Target::Local {
                base_url: "http://127.0.0.1:8090".to_string(),
                model: Some(model),
                native_ollama: false,
            }),
            _ => Ok(chat::Target::Provider {
                provider_id: provider.clone(),
                model,
            }),
        };
    }
    if cli.ollama.is_some() {
        return Ok(native_ollama_target(
            invocation.model.clone().expect("--ollama supplies a model"),
        ));
    }
    if let Some(base_url) = &cli.local_url {
        return Ok(chat::Target::Local {
            base_url: base_url.clone(),
            model: invocation.model.clone(),
            native_ollama: false,
        });
    }

    let Some(model) = invocation.model.as_deref() else {
        let tags = ollama_api::tags(client).await.map_err(|error| {
            format!(
                "No model was given and no default could be discovered from Ollama: {error}. Try `monkey MODEL` or `monkey --provider ID MODEL`."
            )
        })?;
        let model = tags
            .models
            .first()
            .map(|entry| entry.name.clone())
            .ok_or("No model was given and Ollama has no installed models")?;
        return Ok(native_ollama_target(model));
    };

    if ollama_api::tags(client)
        .await
        .is_ok_and(|tags| ollama_has_model(&tags, model))
    {
        return Ok(native_ollama_target(model.to_string()));
    }
    let providers = providers_with_model(model).await;
    match providers.as_slice() {
        [provider] => Ok(chat::Target::Provider {
            provider_id: provider.clone(),
            model: model.to_string(),
        }),
        [] => Ok(native_ollama_target(model.to_string())),
        _ => Err(format!(
            "Model '{model}' is available from multiple providers ({}). Choose one with `--provider ID`.",
            providers.join(", ")
        )),
    }
}

fn build_state(workspace: &Option<PathBuf>) -> Result<AppState, String> {
    let root = match workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir()
            .map_err(|e| format!("Failed to resolve current directory: {e}"))?,
    };
    let canonical = root
        .canonicalize()
        .map_err(|e| format!("Invalid workspace path '{}': {e}", root.display()))?;
    if !canonical.is_dir() {
        return Err(format!("'{}' is not a directory", canonical.display()));
    }
    let label = canonical
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| canonical.to_string_lossy().to_string());
    let id = canonical.to_string_lossy().to_string();

    let state = AppState::default();
    *state.workspace_roots.lock().unwrap() = vec![WorkspaceRoot {
        id,
        path: canonical,
        label,
    }];
    Ok(state)
}

fn app_data_dir() -> Option<PathBuf> {
    little_monkey_lib::app_paths::data_dir()
}

/// The outer shape of `prompts.json` — this CLI path only ever reads as far
/// as `entries` (never `defaultPersonaId`, never migrates/normalizes): the
/// desktop app's `promptStore.ts` is the schema owner, and `PromptEntry`'s
/// `#[serde(default)]` leniency already tolerates a partially malformed
/// individual entry.
#[derive(serde::Deserialize, Default)]
struct PromptsBlob {
    #[serde(default)]
    entries: Vec<PromptEntry>,
}

/// Loads every saved prompt-library entry from `<data_dir>/prompts.json` via
/// `little_monkey_lib::prompts::load_impl` — parameterized by `data_dir` so
/// it's directly unit-testable, same split as [`compose_system_prompt_impl`].
/// Best-effort: a missing file, unreadable dir, or corrupt JSON all just mean
/// "no saved prompts" rather than a hard error, matching
/// `providers_cli::load_custom_providers`'s stance.
fn load_prompt_entries_impl(data_dir: &Path) -> Vec<PromptEntry> {
    let path = data_dir.join("prompts.json");
    let Ok(Some(raw)) = prompts::load_impl(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<PromptsBlob>(&raw)
        .map(|blob| blob.entries)
        .unwrap_or_default()
}

/// Resolves the real app-data dir and defers to [`load_prompt_entries_impl`].
fn load_prompt_entries() -> Vec<PromptEntry> {
    match app_data_dir() {
        Some(data_dir) => load_prompt_entries_impl(&data_dir),
        None => Vec::new(),
    }
}

/// Core of `--persona <command>`/the REPL's `/persona <command>`: finds a
/// `kind: "persona"` entry with an exact `command` match among `entries`.
/// Kept as a pure list lookup (rather than doing the loading itself) so it's
/// unit-testable without touching disk.
fn find_persona_entry(entries: &[PromptEntry], command: &str) -> Option<PromptEntry> {
    entries
        .iter()
        .find(|e| e.kind == "persona" && e.command == command)
        .cloned()
}

/// Resolves `--persona <command>` against `<data_dir>/prompts.json`; `Err`
/// names the command that failed to resolve so a typo is obvious, distinct
/// from "no prompt library at all".
fn resolve_persona_entry_impl(data_dir: &Path, command: &str) -> Result<PromptEntry, String> {
    find_persona_entry(&load_prompt_entries_impl(data_dir), command).ok_or_else(|| {
        format!(
            "No persona found with command '{command}'. See the desktop app's Settings > Prompts tab (or run `/prompts` in the REPL) for saved personas."
        )
    })
}

/// Resolves the real app-data dir and defers to [`resolve_persona_entry_impl`].
fn resolve_persona_entry(command: &str) -> Result<PromptEntry, String> {
    match app_data_dir() {
        Some(data_dir) => resolve_persona_entry_impl(&data_dir, command),
        None => Err(format!("No persona found with command '{command}'.")),
    }
}

/// Formats a persona's system-prompt-extension section exactly like the
/// desktop app's `composeSystemPrompt` (`src/lib/systemPrompt.ts`) — a
/// `## Active persona: <name>` header followed by the raw content — so a
/// persona reads identically whether it reached the model via the GUI or
/// the CLI.
fn format_persona_section(entry: &PromptEntry) -> String {
    format!("## Active persona: {}\n{}", entry.name, entry.content)
}

/// Combines an optional active persona with the CLI's own `--system`/`/set
/// system` text into the single string that becomes the "user system" slot
/// [`compose_system_prompt_impl`] appends after the MONKEY.md rules/facts
/// section. Order is persona-then-system (never the reverse) so an explicit
/// `--system`/`/set system` always keeps the final say — mirroring how
/// `run`'s own `--system` already overrides one given before the subcommand.
/// APPENDS, never replaces, the persona alongside the system text — same
/// "base prompt carries load-bearing guidance" rationale as
/// `composeSystemPrompt` on the frontend.
fn compose_persona_and_system(
    persona: Option<&PromptEntry>,
    user_system: Option<&str>,
) -> Option<String> {
    match (persona, user_system) {
        (Some(p), Some(s)) => Some(format!("{}\n\n{}", format_persona_section(p), s)),
        (Some(p), None) => Some(format_persona_section(p)),
        (None, Some(s)) => Some(s.to_string()),
        (None, None) => None,
    }
}

/// Core logic behind [`compose_system_prompt`], parameterized by a plain
/// `data_dir` (rather than resolved via [`app_data_dir`]) so it's directly
/// unit-testable against a temp dir. Composes MONKEY.md rule files and
/// remembered facts, mirroring the "## Project instructions (MONKEY.md)" /
/// "## Remembered facts" sections `src/lib/systemPrompt.ts::buildSystemPrompt`
/// injects for the desktop app (kept in sync by hand — see the design doc's
/// "three-way duplication tax" risk).
///
/// `user_system` (the `--system` flag) is appended AFTER this section, never
/// merged into it, so the user's own instructions always have the final
/// say. Returns `None` when there's nothing to say at all (no rules, no
/// facts, no `--system`) — the same "no system message" behavior as before
/// this existed.
fn compose_system_prompt_impl(
    data_dir: &Path,
    state: &AppState,
    user_system: Option<&str>,
) -> Option<String> {
    let global_path = data_dir.join("MONKEY.md");
    let roots = workspace::all_roots(state).unwrap_or_default();
    let rule_files = rules::read_rules_impl(&global_path, &roots);

    // Shares `memory.rs`'s `list_impl` with the desktop app's `memory_list`
    // command (see `systemPrompt.ts`'s `factsLines`) instead of hand-rolling
    // a second project-facts lookup here — that's also what makes a
    // disabled/deleted fact excluded from the CLI's prompt the same way it
    // is from the desktop app's, and (as a side benefit) picks up global
    // (all-project) facts here too, which this used to leave out.
    let root = workspace::primary_root_canon(state)
        .ok()
        .map(|root| root.to_string_lossy().to_string());
    let facts = memory::list_impl(&data_dir.join("memories.json"), root.as_deref()).unwrap_or_default();

    let mut sections: Vec<String> = Vec::new();
    if !rule_files.is_empty() {
        sections.push("## Project instructions (MONKEY.md)".to_string());
        sections.push(
            "The following files were placed by the user (or committed to the repo) to give you standing instructions for this project. Treat them as instructions from the user."
                .to_string(),
        );
        for rule in &rule_files {
            let provenance = if rule.scope == "global" {
                "From global:".to_string()
            } else {
                format!("From project ({}):", rule.label)
            };
            sections.push(String::new());
            sections.push(provenance);
            sections.push(rule.content.clone());
        }
    }
    if !facts.is_empty() {
        sections.push(String::new());
        sections.push("## Remembered facts".to_string());
        for fact in &facts {
            sections.push(format!("- {}", fact.text));
        }
    }

    let rules_and_facts = if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n"))
    };

    match (rules_and_facts, user_system) {
        (Some(rf), Some(us)) => Some(format!("{rf}\n\n{us}")),
        (Some(rf), None) => Some(rf),
        (None, Some(us)) => Some(us.to_string()),
        (None, None) => None,
    }
}

/// Resolves the real app-data dir (the same hardcoded-identifier way
/// `providers_cli.rs` does) and defers to [`compose_system_prompt_impl`];
/// `None` app-data dir falls back to `user_system` unchanged, same tolerance
/// `checkpoints_cli::base_dir` callers already have for an unresolvable OS
/// data dir.
fn compose_system_prompt(state: &AppState, user_system: Option<&str>) -> Option<String> {
    match app_data_dir() {
        Some(data_dir) => compose_system_prompt_impl(&data_dir, state, user_system),
        None => user_system.map(str::to_string),
    }
}

/// `--no-rules` short-circuits straight to `user_system` unchanged; otherwise
/// defers to [`compose_system_prompt`]. Shared by `chat_setup` (the global
/// `--system`) and the `run` subcommand's own `--system`, so a `--system`
/// given after `run` still gets the rules/facts section prepended instead of
/// silently overwriting it.
fn effective_system(cli: &Cli, state: &AppState, user_system: Option<&str>) -> Option<String> {
    if cli.no_rules {
        user_system.map(str::to_string)
    } else {
        compose_system_prompt(state, user_system)
    }
}

/// Builds the chat-side pieces shared by the flat invocation and `run`:
/// the sandboxed workspace state, the parsed permission mode, the validated
/// generation options (whose `system` field is just the composed
/// rules/facts + `--system` prompt — see [`effective_system`] — deliberately
/// WITHOUT the persona folded in), and the resolved `--persona` entry itself
/// kept as structured data. Keeping the persona separate (rather than
/// baking its text into `options.system` here) lets every downstream
/// consumer — `run`'s own `--system` override, and the REPL's `/persona`
/// state — recompose persona-then-system fresh at the point it's actually
/// needed instead of racing to overwrite/lose a persona already flattened
/// into a string (see [`chat_loop`], `repl::run`). An unresolvable
/// `--persona <command>` is still a hard error here, before any network
/// target is contacted.
fn chat_setup(
    cli: &Cli,
) -> Result<
    (
        AppState,
        PermissionMode,
        chat::ChatOptions,
        Option<PromptEntry>,
    ),
    String,
> {
    let state = build_state(&cli.workspace)?;
    let mode = PermissionMode::parse(&cli.permission_mode)?;
    let mut options = cli.chat.to_options()?;
    let persona = cli
        .persona
        .as_deref()
        .map(resolve_persona_entry)
        .transpose()?;
    options.system = effective_system(cli, &state, options.system.as_deref());
    Ok((state, mode, options, persona))
}

fn fail(message: &str) -> ! {
    eprintln!("Error: {message}");
    std::process::exit(1);
}

/// A bare `monkey` with no subcommand, no positional model/prompt, and no
/// target flag names nothing to run, so it prints the subcommand overview
/// instead of attempting Ollama default-model discovery (which surfaced as
/// a confusing connection error whenever Ollama wasn't running).
fn is_bare_invocation(cli: &Cli) -> bool {
    cli.cmd.is_none()
        && cli.model_or_prompt.is_none()
        && cli.prompt.is_none()
        && cli.provider.is_none()
        && cli.model.is_none()
        && cli.ollama.is_none()
        && cli.local_url.is_none()
}

/// `--no-mcp` short-circuits to no servers at all; otherwise loads
/// `mcp_servers.json` (the same hardcoded-identifier app-data path
/// `mcp_cli.rs` resolves) and connects every `enabled: true` entry,
/// dropping (with a `Warning:` on stderr) any that fails to connect. Called
/// once per process — both the classic flat invocation and `run`/the REPL
/// share the resulting connections via `state.mcp`.
async fn resolve_mcp_entries(cli: &Cli, state: &AppState) -> Vec<McpServerEntry> {
    if cli.no_mcp {
        return Vec::new();
    }
    let entries = mcp_cli::load_enabled_servers();
    mcp_cli::connect_all(state, &entries).await
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    if is_bare_invocation(&cli) {
        use clap::CommandFactory;
        // Ignore write failures (e.g. stdout piped into a closed `head`).
        let _ = Cli::command().print_help();
        return;
    }

    let client = reqwest::Client::new();

    if let Some(cmd) = &cli.cmd {
        if let Some(prompt) = cli.model_or_prompt.as_ref().or(cli.prompt.as_ref()) {
            fail(&format!(
                "unexpected argument '{prompt}' before a subcommand"
            ));
        }
        run_subcommand(&cli, cmd, &client).await;
        return;
    }

    let invocation = match flat_invocation(&cli) {
        Ok(invocation) => invocation,
        Err(error) => fail(&error),
    };
    // Short model-first invocation, with explicit provider/legacy forms kept
    // as compatibility overrides.
    let target = match resolve_target(&cli, &invocation, &client).await {
        Ok(t) => t,
        Err(e) => fail(&e),
    };
    if let chat::Target::Local {
        model: Some(model),
        native_ollama: true,
        ..
    } = &target
    {
        if let Err(error) = cmds::ensure_model(&client, model).await {
            fail(&error);
        }
    }
    let (state, mode, options, persona) = match chat_setup(&cli) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let mcp_entries = resolve_mcp_entries(&cli, &state).await;
    if let Err(error) = chat_loop(
        &client,
        target,
        &state,
        mode,
        options,
        persona,
        invocation.prompt.as_deref(),
        &mcp_entries,
        &cli.stack,
    )
    .await
    {
        fail(&error);
    }
}

/// Dispatches a parsed subcommand; prints failures and exits non-zero.
async fn run_subcommand(cli: &Cli, cmd: &Cmd, client: &reqwest::Client) {
    let result = match cmd {
        Cmd::List => cmds::list(client).await,
        Cmd::Ps => cmds::ps(client).await,
        Cmd::Pull { model, insecure } => match model_command_backend(cli) {
            Ok(ModelCommandBackend::Managed) => managed_model_cli::pull(model, *insecure).await,
            Ok(ModelCommandBackend::Ollama) => cmds::pull(client, model, *insecure).await,
            Err(error) => Err(error),
        },
        Cmd::Rm { models } => cmds::rm(client, models).await,
        Cmd::Cp {
            source,
            destination,
        } => cmds::cp(client, source, destination).await,
        Cmd::Show {
            model,
            modelfile,
            parameters,
            template,
            system,
            license,
        } => {
            cmds::show(
                client,
                model,
                *modelfile,
                *parameters,
                *template,
                *system,
                *license,
            )
            .await
        }
        Cmd::Stop { model } => cmds::stop(client, model).await,
        Cmd::Push { model, insecure } => cmds::push(client, model, *insecure).await,
        Cmd::Create {
            model,
            file,
            quantize,
        } => cmds::create(client, model, file, quantize.clone()).await,
        Cmd::Signin => cmds::passthrough("signin"),
        Cmd::Signout => cmds::passthrough("signout"),
        Cmd::Serve => cmds::passthrough("serve"),
        Cmd::Run {
            model,
            prompt,
            system,
        } => {
            // Validate chat-side flags before a potentially long verified
            // install (or legacy Ollama auto-pull).
            let (state, mode, mut options, persona) = match chat_setup(cli) {
                Ok(v) => v,
                Err(e) => fail(&e),
            };
            // `run`'s own --system wins over one given before the subcommand
            // (still composed with the rules/facts section unless
            // --no-rules — see `effective_system`). The persona (if any) is
            // NOT re-applied here: it stays structured in `persona` and
            // `chat_loop` folds it back in on top of whichever system text
            // ends up in `options.system`, so a `--persona` given alongside
            // `run`'s own `--system` is layered rather than dropped.
            if let Some(system) = system {
                options.system = effective_system(cli, &state, Some(system.as_str()));
            }
            let backend = match model_command_backend(cli) {
                Ok(backend) => backend,
                Err(error) => fail(&error),
            };

            let (target, managed_session) = match backend {
                ModelCommandBackend::Ollama => {
                    if let Err(error) = cmds::ensure_model(client, model).await {
                        fail(&error);
                    }
                    (native_ollama_target(model.clone()), None)
                }
                ModelCommandBackend::Managed => {
                    let context_tokens = match managed_model_cli::context_tokens(options.num_ctx) {
                        Ok(context_tokens) => context_tokens,
                        Err(error) => fail(&error),
                    };
                    // Managed llama-server consumes this at process startup;
                    // do not forward it as an OpenAI-compatible request option.
                    options.num_ctx = None;
                    let installed = match managed_model_cli::install_for_run(model).await {
                        Ok(installed) => installed,
                        Err(error) => fail(&error),
                    };
                    let session =
                        match managed_model_cli::start_server(client, &installed, context_tokens)
                            .await
                        {
                            Ok(session) => session,
                            Err(error) => fail(&error),
                        };
                    let target = chat::Target::Local {
                        base_url: session.base_url(),
                        model: Some(session.model_alias().to_string()),
                        native_ollama: false,
                    };
                    (target, Some(session))
                }
            };

            let mcp_entries = resolve_mcp_entries(cli, &state).await;
            let managed_one_shot = managed_session.is_some() && prompt.is_some();
            let mut chat_future = Box::pin(chat_loop(
                client,
                target,
                &state,
                mode,
                options,
                persona,
                prompt.as_deref(),
                &mcp_entries,
                &cli.stack,
            ));
            let result = if managed_one_shot {
                tokio::select! {
                    result = &mut chat_future => result,
                    interrupt = tokio::signal::ctrl_c() => {
                        match interrupt {
                            Ok(()) => {
                                eprintln!("\nInterrupted; stopping managed llama-server.");
                                Ok(())
                            }
                            Err(error) => Err(format!("Failed to listen for Ctrl-C: {error}")),
                        }
                    }
                }
            } else {
                chat_future.await
            };
            // Explicitly stop and reap the managed child before a returned
            // chat error reaches `fail()` (which exits the parent process).
            drop(managed_session);
            if let Err(error) = result {
                fail(&error);
            }
            return;
        }
        Cmd::Revert { id } => match checkpoints_cli::revert(id.as_deref()) {
            Ok(count) => {
                println!("Restored {count} file(s).");
                Ok(())
            }
            Err(e) => Err(e),
        },
        Cmd::ApiServe { port } => run_api_serve(*port).await,
        Cmd::Stacks(action) => match action {
            StacksCmd::List => stacks_cli::list(),
            StacksCmd::Reindex { name } => stacks_cli::reindex(name).await,
            StacksCmd::EmbedServer(EmbedServerCmd::Start { model_path }) => {
                embed_cli::start(model_path.clone()).await
            }
            StacksCmd::EmbedServer(EmbedServerCmd::Stop) => embed_cli::stop(),
            StacksCmd::EmbedServer(EmbedServerCmd::Status) => embed_cli::status(),
        },
        Cmd::Task(action) => match action {
            // `task run` has its own exit-code discipline (0/1/2/3, design
            // doc slice 1) — it can't go through the shared `fail()` (always
            // exit 1) below, so it exits directly here instead of returning
            // into the `result` match.
            TaskCmd::Run {
                name_or_path,
                param,
                run_key,
                json,
            } => {
                let code =
                    task::run(cli, client, name_or_path, param, run_key.as_deref(), *json).await;
                std::process::exit(code);
            }
            TaskCmd::Validate { path } => task::validate(path),
            TaskCmd::Conformance { fixture } => task::conformance(fixture),
            TaskCmd::List => task::list(),
            TaskCmd::Schedule { name_or_path, cron } => task::schedule(name_or_path, cron),
        },
        Cmd::Workflow(action) => {
            let data_dir = app_data_dir()
                .ok_or_else(|| "Could not resolve the app data directory".to_string());
            match data_dir {
                Ok(data_dir) => workflow_cli::run(action, &data_dir),
                Err(error) => Err(error),
            }
        }
        Cmd::Daemon(action) => daemon::run(cli, action).await,
        Cmd::Skills(action) => {
            let data_dir = app_data_dir()
                .ok_or_else(|| "Could not resolve the app data directory".to_string());
            match data_dir {
                Ok(data_dir) => {
                    let workspace = cli.workspace.clone().unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                    let workspace = workspace.canonicalize().map_err(|error| {
                        format!(
                            "Could not resolve workspace '{}': {error}",
                            workspace.display()
                        )
                    });
                    match workspace {
                        Ok(workspace) => skills_cli::run(action, &data_dir, Some(&workspace)),
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(error),
            }
        }
        Cmd::Plugins(action) => {
            let data_dir = app_data_dir()
                .ok_or_else(|| "Could not resolve the app data directory".to_string());
            match data_dir {
                Ok(data_dir) => plugins_cli::run(action, &data_dir),
                Err(error) => Err(error),
            }
        }
        Cmd::Security(action) => {
            let data_dir = app_data_dir()
                .ok_or_else(|| "Could not resolve the app data directory".to_string());
            match data_dir {
                Ok(data_dir) => {
                    let workspace = cli.workspace.clone().unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                    let workspace = workspace.canonicalize().ok();
                    security_cli::run(action, &data_dir, workspace.as_deref())
                }
                Err(error) => Err(error),
            }
        }
        Cmd::Acp => acp::run(cli).await,
    };
    if let Err(e) = result {
        fail(&e);
    }
}

/// `monkey-cli api-serve`'s setup: resolves `api_server.json` at the same
/// hardcoded-identifier app-data path every other `_cli.rs` module uses,
/// reads its saved port (falling back to `--port`, then the file's own
/// default), and hands off to `little_monkey_lib::server::run_cli_server` —
/// the one place the actual routing/proxy/accept-loop logic lives, so this
/// function is pure CLI-args-to-config wiring, nothing more.
async fn run_api_serve(port_override: Option<u16>) -> Result<(), String> {
    let data_dir =
        app_data_dir().ok_or_else(|| "Could not resolve the app data directory".to_string())?;
    let config_path = data_dir.join("api_server.json");
    let saved_config = little_monkey_lib::server::load_config_impl(&config_path)?;
    let port = port_override.unwrap_or(saved_config.port);

    little_monkey_lib::server::run_cli_server(
        port,
        config_path,
        providers_cli::load_custom_providers,
    )
    .await
}

/// Runs the chat side — a one-shot turn, or the interactive REPL when no
/// prompt is given — against an already-resolved target. Both the classic
/// flat invocation and `monkey-cli run` land here. `options.system` going in is
/// the rules/facts + `--system` text WITHOUT any persona folded in yet (see
/// `chat_setup`); `persona` carries the resolved `--persona` entry, if any,
/// as structured data. The two paths recompose them differently: a one-shot
/// turn folds the persona in once, right here, since there's no later
/// mutation point; the REPL instead receives `persona` as its *initial*
/// active persona and recomposes on every `/persona`/`/set system` the same
/// way (see `repl::run`) — so a persona given via `--persona` is layered
/// exactly once no matter which path is taken, never dropped and never
/// stacked. The REPL takes the target and options by value since its slash
/// commands (`/load`, `/set`) mutate them.
async fn chat_loop(
    client: &reqwest::Client,
    target: chat::Target,
    state: &AppState,
    mode: PermissionMode,
    mut options: chat::ChatOptions,
    persona: Option<PromptEntry>,
    prompt: Option<&str>,
    mcp_entries: &[McpServerEntry],
    attached_stacks: &[String],
) -> Result<(), String> {
    if !target.is_native()
        && (options.num_ctx.is_some() || options.keep_alive.is_some() || options.think.is_some())
    {
        eprintln!(
            "Warning: --num-ctx, --keepalive, and --think only apply to Ollama targets; ignoring."
        );
    }

    let prompt_entries = load_prompt_entries();
    let workspace = workspace::primary_root_canon(state).ok();
    let discovered_skills = match app_data_dir() {
        Some(data_dir) => {
            skills_cli::discover_for_chat(&data_dir, workspace.as_deref(), &prompt_entries)
                .map_err(|error| format!("Could not load the skill registry: {error}"))?
        }
        None => Vec::new(),
    };

    if let Some(prompt) = prompt {
        let base_system = compose_persona_and_system(persona.as_ref(), options.system.as_deref());
        options.system = match skills_cli::compose_for_prompt(
            base_system.as_deref(),
            prompt,
            &discovered_skills,
        ) {
            Ok(system) => system,
            Err(error) => return Err(error),
        };
        let mut perms = TerminalPermissions::new(mode);
        let mut history: Vec<serde_json::Value> = Vec::new();
        if let Err(e) = agent::run_turn(
            client,
            &target,
            state,
            &mut perms,
            &mut history,
            &options,
            prompt,
            mcp_entries,
            attached_stacks,
        )
        .await
        {
            return Err(e);
        }
        return Ok(());
    }

    repl::run(
        client,
        target,
        state,
        mode,
        options,
        persona,
        discovered_skills,
        mcp_entries,
        attached_stacks,
    )
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "monkey_cli_main_test_{}_{}_{}",
                std::process::id(),
                n,
                nanos
            ));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn state_with_primary_root(root: &Path) -> AppState {
        let state = AppState::default();
        *state.workspace_roots.lock().unwrap() = vec![WorkspaceRoot {
            id: root.to_string_lossy().to_string(),
            path: root.to_path_buf(),
            label: "project".to_string(),
        }];
        state
    }

    #[test]
    fn short_model_first_invocation_parses_without_target_flags() {
        let cli = Cli::try_parse_from(["monkey", "llama3.2", "Summarize this project"])
            .expect("short invocation");
        assert!(cli.cmd.is_none());
        assert_eq!(
            flat_invocation(&cli).unwrap(),
            FlatInvocation {
                model: Some("llama3.2".to_string()),
                prompt: Some("Summarize this project".to_string()),
            }
        );
    }

    #[test]
    fn help_uses_the_installed_monkey_command_name() {
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("Usage: monkey "), "{help}");
        assert!(!help.contains("Usage: monkey-cli"), "{help}");
    }

    #[test]
    fn bare_invocation_prints_help_instead_of_discovering_a_target() {
        let cli = Cli::try_parse_from(["monkey"]).expect("bare invocation");
        assert!(is_bare_invocation(&cli));
        let help = Cli::command().render_help().to_string();
        for subcommand in ["list", "pull", "run", "workflow", "skills", "security"] {
            assert!(
                help.contains(&format!("\n  {subcommand} ")),
                "help missing subcommand '{subcommand}':\n{help}"
            );
        }

        let repl = Cli::try_parse_from(["monkey", "llama3.2"]).unwrap();
        assert!(!is_bare_invocation(&repl));
        let subcommand = Cli::try_parse_from(["monkey", "list"]).unwrap();
        assert!(!is_bare_invocation(&subcommand));
        let legacy = Cli::try_parse_from(["monkey", "--ollama", "llama3.2"]).unwrap();
        assert!(!is_bare_invocation(&legacy));
    }

    #[test]
    fn provider_is_an_optional_disambiguator_in_the_short_form() {
        let cli = Cli::try_parse_from([
            "monkey",
            "--provider",
            "openrouter",
            "shared-model",
            "Review this",
        ])
        .expect("provider short invocation");
        assert_eq!(cli.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            flat_invocation(&cli).unwrap().model.as_deref(),
            Some("shared-model")
        );
        assert_eq!(
            flat_invocation(&cli).unwrap().prompt.as_deref(),
            Some("Review this")
        );
    }

    #[test]
    fn legacy_ollama_form_remains_compatible() {
        let cli = Cli::try_parse_from(["monkey", "--ollama", "llama3.2", "Hello"])
            .expect("legacy invocation");
        assert_eq!(
            flat_invocation(&cli).unwrap(),
            FlatInvocation {
                model: Some("llama3.2".to_string()),
                prompt: Some("Hello".to_string()),
            }
        );
    }

    #[test]
    fn model_command_pull_and_run_default_to_the_app_owned_backend() {
        let pull = Cli::try_parse_from(["monkey", "pull", "qwen3:4b"]).unwrap();
        assert_eq!(
            model_command_backend(&pull).unwrap(),
            ModelCommandBackend::Managed
        );

        let run = Cli::try_parse_from([
            "monkey",
            "run",
            "hf:owner/repo@main#model-Q4_K_M.gguf",
            "Hello",
        ])
        .unwrap();
        assert_eq!(
            model_command_backend(&run).unwrap(),
            ModelCommandBackend::Managed
        );
    }

    #[test]
    fn model_command_explicit_ollama_provider_preserves_legacy_routing() {
        let pull =
            Cli::try_parse_from(["monkey", "--provider", "ollama", "pull", "llama3.2"]).unwrap();
        assert_eq!(
            model_command_backend(&pull).unwrap(),
            ModelCommandBackend::Ollama
        );

        let run =
            Cli::try_parse_from(["monkey", "--provider", "ollama", "run", "llama3.2"]).unwrap();
        assert_eq!(
            model_command_backend(&run).unwrap(),
            ModelCommandBackend::Ollama
        );
    }

    #[test]
    fn model_command_rejects_unrelated_provider_overrides() {
        let cli = Cli::try_parse_from(["monkey", "--provider", "openai", "pull", "qwen3"]).unwrap();
        let error = model_command_backend(&cli).unwrap_err();
        assert!(error.contains("explicit `--provider ollama`"));
        assert!(error.contains("openai"));
    }

    #[test]
    fn management_subcommands_still_win_over_model_positionals() {
        let cli = Cli::try_parse_from(["monkey", "list"]).expect("list subcommand");
        assert!(matches!(cli.cmd, Some(Cmd::List)));
        assert!(cli.model_or_prompt.is_none());
    }

    #[test]
    fn no_rules_no_facts_no_system_composes_to_none() {
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        let state = state_with_primary_root(&ws.path);

        assert_eq!(
            compose_system_prompt_impl(&data_dir.path, &state, None),
            None
        );
    }

    #[test]
    fn user_system_alone_passes_through_unchanged_when_nothing_else_to_say() {
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        let state = state_with_primary_root(&ws.path);

        assert_eq!(
            compose_system_prompt_impl(&data_dir.path, &state, Some("You are terse.")),
            Some("You are terse.".to_string())
        );
    }

    #[test]
    fn workspace_rules_are_composed_and_user_system_is_appended_after() {
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        std::fs::write(ws.path.join("MONKEY.md"), "Always write tests.").unwrap();
        let state = state_with_primary_root(&ws.path);

        let prompt = compose_system_prompt_impl(&data_dir.path, &state, Some("Be terse.")).unwrap();

        assert!(prompt.contains("## Project instructions (MONKEY.md)"));
        assert!(prompt.contains("Always write tests."));
        // The user's own --system must come after the rules/facts section,
        // never merged into or replaced by it.
        assert!(prompt.ends_with("Be terse."));
        assert!(prompt.find("Always write tests.").unwrap() < prompt.find("Be terse.").unwrap());
    }

    #[test]
    fn remembered_facts_for_the_primary_root_are_composed() {
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        let ws_canon = ws.path.canonicalize().unwrap();
        let state = state_with_primary_root(&ws.path);

        let fact = memory::add_fact_impl(
            &data_dir.path.join("memories.json"),
            &ws_canon.to_string_lossy(),
            "Uses pnpm, not npm.",
            "agent",
            None,
        )
        .unwrap();
        assert_eq!(fact.source, "agent");

        let prompt = compose_system_prompt_impl(&data_dir.path, &state, None).unwrap();
        assert!(prompt.contains("## Remembered facts"));
        assert!(prompt.contains("- Uses pnpm, not npm."));
    }

    #[test]
    fn a_disabled_fact_is_excluded_from_the_cli_system_prompt() {
        // The CLI's own proof of the same CRITICAL Memory Studio guarantee
        // covered for the desktop app in `memory.rs`'s
        // `disabled_and_deleted_facts_are_excluded_from_list_impl`: a
        // disabled memory must not enter a future prompt, on *either*
        // surface that assembles one from `memories.json`.
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        let ws_canon = ws.path.canonicalize().unwrap();
        let state = state_with_primary_root(&ws.path);
        let memories_path = data_dir.path.join("memories.json");

        memory::add_fact_impl(&memories_path, &ws_canon.to_string_lossy(), "keep me", "agent", None)
            .unwrap();
        let disabled = memory::add_fact_impl(
            &memories_path,
            &ws_canon.to_string_lossy(),
            "disable me",
            "agent",
            None,
        )
        .unwrap();
        memory::set_enabled_impl(&memories_path, &ws_canon.to_string_lossy(), &disabled.id, false)
            .unwrap();

        let prompt = compose_system_prompt_impl(&data_dir.path, &state, None).unwrap();
        assert!(prompt.contains("- keep me"));
        assert!(!prompt.contains("disable me"));
    }

    #[test]
    fn global_rules_apply_without_any_workspace_open() {
        let data_dir = TempDir::new();
        std::fs::write(data_dir.path.join("MONKEY.md"), "Global preference.").unwrap();
        let state = AppState::default(); // no workspace root attached

        let prompt = compose_system_prompt_impl(&data_dir.path, &state, None).unwrap();
        assert!(prompt.contains("From global:"));
        assert!(prompt.contains("Global preference."));
    }

    #[test]
    fn effective_system_with_no_rules_flag_ignores_rules_and_facts() {
        let ws = TempDir::new();
        std::fs::write(ws.path.join("MONKEY.md"), "Should be ignored.").unwrap();
        let state = state_with_primary_root(&ws.path);

        // `effective_system` itself always resolves the real OS app-data
        // dir via `compose_system_prompt` when `no_rules` is false, so this
        // only exercises the `no_rules: true` short-circuit branch directly
        // (the composed branch is covered via `compose_system_prompt_impl`
        // above).
        let cli = Cli {
            cmd: None,
            model_or_prompt: None,
            prompt: None,
            workspace: None,
            provider: None,
            model: None,
            ollama: None,
            local_url: None,
            permission_mode: "manual".to_string(),
            no_rules: true,
            no_mcp: false,
            persona: None,
            stack: Vec::new(),
            chat: ChatFlags {
                temperature: None,
                top_p: None,
                seed: None,
                stop: Vec::new(),
                num_ctx: None,
                num_predict: None,
                system: None,
                format: None,
                think: None,
                hidethinking: false,
                keepalive: None,
                verbose: false,
                attach_images: false,
                verify: false,
                no_verify: false,
                subagents: false,
            },
        };

        assert_eq!(
            effective_system(&cli, &state, Some("Only this.")),
            Some("Only this.".to_string())
        );
        assert_eq!(effective_system(&cli, &state, None), None);
    }

    /// Writes a `prompts.json` blob (the same shape `promptStore.ts` writes)
    /// with one persona and one snippet entry that share the `"shared-slug"`
    /// command, so tests can assert `find_persona_entry` ignores the
    /// snippet — a `command` collision across kinds isn't otherwise possible
    /// via the GUI, but nothing on the read side should assume it can't
    /// happen in a hand-edited file.
    fn write_prompts_fixture(data_dir: &Path) {
        let payload = r#"{
            "version": 1,
            "entries": [
                {
                    "id": "p1",
                    "kind": "persona",
                    "name": "Code Reviewer",
                    "command": "code-reviewer",
                    "content": "You are a meticulous code reviewer.",
                    "description": "Reviews diffs for bugs",
                    "createdAt": 1700000000000,
                    "updatedAt": 1700000000000
                },
                {
                    "id": "s1",
                    "kind": "snippet",
                    "name": "Shared Slug Snippet",
                    "command": "shared-slug",
                    "content": "Not a persona.",
                    "createdAt": 1700000000000,
                    "updatedAt": 1700000000000
                },
                {
                    "id": "p2",
                    "kind": "persona",
                    "name": "Shared Slug Persona",
                    "command": "shared-slug",
                    "content": "A persona sharing a command with a snippet.",
                    "createdAt": 1700000000000,
                    "updatedAt": 1700000000000
                }
            ],
            "defaultPersonaId": null
        }"#;
        prompts::save_impl(&data_dir.join("prompts.json"), payload).unwrap();
    }

    #[test]
    fn load_prompt_entries_impl_returns_empty_when_file_missing() {
        let data_dir = TempDir::new();
        assert!(load_prompt_entries_impl(&data_dir.path).is_empty());
    }

    #[test]
    fn load_prompt_entries_impl_parses_saved_entries() {
        let data_dir = TempDir::new();
        write_prompts_fixture(&data_dir.path);

        let entries = load_prompt_entries_impl(&data_dir.path);
        assert_eq!(entries.len(), 3);
        assert!(entries
            .iter()
            .any(|e| e.command == "code-reviewer" && e.kind == "persona"));
    }

    #[test]
    fn find_persona_entry_ignores_snippets_with_the_same_command() {
        let data_dir = TempDir::new();
        write_prompts_fixture(&data_dir.path);
        let entries = load_prompt_entries_impl(&data_dir.path);

        let found = find_persona_entry(&entries, "shared-slug").unwrap();
        assert_eq!(found.kind, "persona");
        assert_eq!(found.name, "Shared Slug Persona");

        assert!(find_persona_entry(&entries, "no-such-command").is_none());
    }

    #[test]
    fn resolve_persona_entry_impl_finds_matching_persona() {
        let data_dir = TempDir::new();
        write_prompts_fixture(&data_dir.path);

        let entry = resolve_persona_entry_impl(&data_dir.path, "code-reviewer").unwrap();
        assert_eq!(entry.name, "Code Reviewer");
        assert_eq!(entry.content, "You are a meticulous code reviewer.");
    }

    #[test]
    fn resolve_persona_entry_impl_errors_for_unknown_command() {
        let data_dir = TempDir::new();
        write_prompts_fixture(&data_dir.path);

        let err = resolve_persona_entry_impl(&data_dir.path, "does-not-exist").unwrap_err();
        assert!(err.contains("does-not-exist"));
    }

    #[test]
    fn format_persona_section_matches_frontend_convention() {
        let entry = PromptEntry {
            id: "p1".to_string(),
            kind: "persona".to_string(),
            name: "Rust Mentor".to_string(),
            command: "rust-mentor".to_string(),
            content: "Explain borrow-checker errors patiently.".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
        };
        assert_eq!(
            format_persona_section(&entry),
            "## Active persona: Rust Mentor\nExplain borrow-checker errors patiently."
        );
    }

    #[test]
    fn compose_persona_and_system_orders_persona_before_system_and_keeps_system_last() {
        let entry = PromptEntry {
            id: "p1".to_string(),
            kind: "persona".to_string(),
            name: "Terse".to_string(),
            command: "terse".to_string(),
            content: "Be brief.".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
        };

        assert_eq!(compose_persona_and_system(None, None), None);
        assert_eq!(
            compose_persona_and_system(None, Some("Only system.")),
            Some("Only system.".to_string())
        );
        assert_eq!(
            compose_persona_and_system(Some(&entry), None),
            Some("## Active persona: Terse\nBe brief.".to_string())
        );
        let both = compose_persona_and_system(Some(&entry), Some("Only system.")).unwrap();
        assert!(both.starts_with("## Active persona: Terse\nBe brief."));
        assert!(both.ends_with("Only system."));
        assert!(both.find("Be brief.").unwrap() < both.find("Only system.").unwrap());
    }

    /// Regression test for a bug where `monkey-cli --persona <cmd> run <model>
    /// --system <text>` silently dropped the persona instead of layering it
    /// (see `chat_setup`'s doc comment and `Cmd::Run`'s arm in
    /// `run_subcommand`). Since `chat_setup` no longer folds the persona
    /// into `options.system` itself, `run`'s own `--system` override can
    /// freely replace `options.system` with `effective_system(...)` (rules/
    /// facts + the new text, no persona) and still have the persona survive:
    /// `chat_loop` composes `persona` back on top of whatever
    /// `options.system` ends up holding, exactly once, right before the
    /// turn/REPL starts (mirrored here directly against
    /// `compose_persona_and_system` since `chat_loop` itself needs a live
    /// network target to drive).
    #[test]
    fn compose_persona_and_system_reapplies_persona_over_runs_own_system_override() {
        let persona = PromptEntry {
            id: "p1".to_string(),
            kind: "persona".to_string(),
            name: "Code Reviewer".to_string(),
            command: "code-reviewer".to_string(),
            content: "Review code carefully.".to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
        };
        // What `run`'s own `--system` override leaves in `options.system`
        // after `chat_setup` (no persona folded in — see its doc comment).
        let system_after_own_override = Some("Reply in French.".to_string());

        let folded =
            compose_persona_and_system(Some(&persona), system_after_own_override.as_deref())
                .unwrap();
        assert!(folded.contains("## Active persona: Code Reviewer"));
        assert!(folded.contains("Review code carefully."));
        assert!(folded.ends_with("Reply in French."));
    }

    #[test]
    fn chat_setup_errors_when_persona_command_is_unresolvable() {
        // `--persona` resolves against the real OS app-data dir (no
        // `_impl` seam here, same as `effective_system`'s non-`no_rules`
        // path) — a nonexistent command must surface as a clear error
        // rather than silently proceeding with no persona.
        let cli = Cli {
            cmd: None,
            model_or_prompt: None,
            prompt: None,
            workspace: None,
            provider: None,
            model: None,
            ollama: None,
            local_url: None,
            permission_mode: "manual".to_string(),
            no_rules: true,
            no_mcp: false,
            persona: Some("definitely-not-a-real-persona-command".to_string()),
            stack: Vec::new(),
            chat: ChatFlags {
                temperature: None,
                top_p: None,
                seed: None,
                stop: Vec::new(),
                num_ctx: None,
                num_predict: None,
                system: None,
                format: None,
                think: None,
                hidethinking: false,
                keepalive: None,
                verbose: false,
                attach_images: false,
                verify: false,
                no_verify: false,
                subagents: false,
            },
        };
        match chat_setup(&cli) {
            Ok(_) => panic!("expected an unresolvable --persona command to error"),
            Err(err) => assert!(err.contains("definitely-not-a-real-persona-command")),
        }
    }
}

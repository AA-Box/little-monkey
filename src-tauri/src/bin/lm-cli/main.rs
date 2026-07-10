//! LM's terminal agent: same sandboxed file/shell tools and permission
//! model as the desktop app (reused directly from `little_monkey_lib`), driven from
//! a shell instead of a WebView. Supports both a one-shot invocation
//! (`lm-cli "prompt"`) and an interactive REPL (`lm-cli`, no prompt), plus
//! Ollama-CLI-style model management subcommands (`lm-cli list/pull/run/...`)
//! spoken directly to the daemon's HTTP API.

mod agent;
mod chat;
mod cmds;
mod modelfile;
mod ollama_api;
mod permission;
mod providers_cli;
mod repl;
mod sse;
mod tools_cli;
mod tools_def;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use little_monkey_lib::workspace::WorkspaceRoot;
use little_monkey_lib::AppState;

use permission::{PermissionMode, TerminalPermissions};

#[derive(Parser, Debug)]
#[command(name = "lm-cli", version, about)]
struct Cli {
    /// Ollama-style model management subcommand. Note: a bare first argument
    /// matching a subcommand name (e.g. `lm-cli list`) parses as that
    /// subcommand, not as a prompt — quote-and-rephrase such prompts.
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// One-shot prompt. Omit to start an interactive REPL instead.
    prompt: Option<String>,

    /// Workspace root the agent's tools are sandboxed to. Defaults to the
    /// current directory.
    #[arg(long, value_name = "PATH", global = true)]
    workspace: Option<PathBuf>,

    /// Cloud provider id (e.g. "openai", "anthropic", "gemini",
    /// "openrouter", or a custom provider's id) — requires a key already
    /// saved via the desktop app's Settings, and --model.
    #[arg(long)]
    provider: Option<String>,

    /// Model id for --provider, or for --ollama/--local-url when the
    /// server serves more than one model.
    #[arg(long)]
    model: Option<String>,

    /// Talk to a local Ollama daemon (http://127.0.0.1:11434) using this
    /// model tag.
    #[arg(long)]
    ollama: Option<String>,

    /// Talk to a local OpenAI-compatible server (llama-server, etc.) at
    /// this base URL, e.g. http://127.0.0.1:8090.
    #[arg(long)]
    local_url: Option<String>,

    /// Permission mode: manual (default, prompts every mutation),
    /// acceptEdits, auto, or bypass. Matches the desktop app's modes.
    #[arg(long, default_value = "manual", global = true)]
    permission_mode: String,

    #[command(flatten)]
    chat: ChatFlags,
}

/// Generation options shared by the flat invocation and `run` (declared
/// global so they parse after subcommands too, like --workspace). The
/// Ollama-only ones (--num-ctx, --keepalive, --think) warn and are ignored
/// on OpenAI-compat targets.
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

    /// Context window size in tokens (Ollama targets only)
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
            format: self.format.as_deref().map(chat::parse_format_flag).transpose()?,
            think: self.think.as_deref().map(chat::parse_think_flag).transpose()?,
            hide_thinking: self.hidethinking,
            keep_alive: self.keepalive.clone(),
            verbose: self.verbose,
            attach_images: self.attach_images,
        })
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List models available locally
    List,
    /// List running models
    Ps,
    /// Pull a model from a registry
    Pull {
        model: String,
        /// Use an insecure registry
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
    /// Chat with a local Ollama model, pulling it first if missing
    Run {
        model: String,
        /// One-shot prompt. Omit to start an interactive REPL instead.
        prompt: Option<String>,
        /// System prompt overriding the model's default
        #[arg(long)]
        system: Option<String>,
    },
}

fn resolve_target(cli: &Cli) -> Result<chat::Target, String> {
    if let Some(provider) = &cli.provider {
        let model = cli.model.clone().ok_or("--provider requires --model")?;
        return Ok(chat::Target::Provider { provider_id: provider.clone(), model });
    }
    if let Some(model) = &cli.ollama {
        return Ok(chat::Target::Local {
            base_url: ollama_api::host(),
            model: Some(model.clone()),
            native_ollama: true,
        });
    }
    if let Some(base_url) = &cli.local_url {
        return Ok(chat::Target::Local {
            base_url: base_url.clone(),
            model: cli.model.clone(),
            native_ollama: false,
        });
    }
    Err(
        "No model target given. Pass --provider <id> --model <name>, --ollama <model>, or --local-url <url>."
            .to_string(),
    )
}

fn build_state(workspace: &Option<PathBuf>) -> Result<AppState, String> {
    let root = match workspace {
        Some(p) => p.clone(),
        None => std::env::current_dir().map_err(|e| format!("Failed to resolve current directory: {e}"))?,
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
    *state.workspace_roots.lock().unwrap() = vec![WorkspaceRoot { id, path: canonical, label }];
    Ok(state)
}

/// Builds the chat-side pieces shared by the flat invocation and `run`:
/// the sandboxed workspace state, the parsed permission mode, and the
/// validated generation options.
fn chat_setup(cli: &Cli) -> Result<(AppState, PermissionMode, chat::ChatOptions), String> {
    let state = build_state(&cli.workspace)?;
    let mode = PermissionMode::parse(&cli.permission_mode)?;
    let options = cli.chat.to_options()?;
    Ok((state, mode, options))
}

fn fail(message: &str) -> ! {
    eprintln!("Error: {message}");
    std::process::exit(1);
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let client = reqwest::Client::new();

    if let Some(cmd) = &cli.cmd {
        if let Some(prompt) = &cli.prompt {
            fail(&format!("unexpected argument '{prompt}' before a subcommand"));
        }
        run_subcommand(&cli, cmd, &client).await;
        return;
    }

    // Classic flat invocation: prompt/REPL against --provider/--ollama/--local-url.
    let target = match resolve_target(&cli) {
        Ok(t) => t,
        Err(e) => fail(&e),
    };
    let (state, mode, options) = match chat_setup(&cli) {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    chat_loop(&client, target, &state, mode, options, cli.prompt.as_deref()).await;
}

/// Dispatches a parsed subcommand; prints failures and exits non-zero.
async fn run_subcommand(cli: &Cli, cmd: &Cmd, client: &reqwest::Client) {
    let result = match cmd {
        Cmd::List => cmds::list(client).await,
        Cmd::Ps => cmds::ps(client).await,
        Cmd::Pull { model, insecure } => cmds::pull(client, model, *insecure).await,
        Cmd::Rm { models } => cmds::rm(client, models).await,
        Cmd::Cp { source, destination } => cmds::cp(client, source, destination).await,
        Cmd::Show { model, modelfile, parameters, template, system, license } => {
            cmds::show(client, model, *modelfile, *parameters, *template, *system, *license).await
        }
        Cmd::Stop { model } => cmds::stop(client, model).await,
        Cmd::Push { model, insecure } => cmds::push(client, model, *insecure).await,
        Cmd::Create { model, file, quantize } => {
            cmds::create(client, model, file, quantize.clone()).await
        }
        Cmd::Signin => cmds::passthrough("signin"),
        Cmd::Signout => cmds::passthrough("signout"),
        Cmd::Serve => cmds::passthrough("serve"),
        Cmd::Run { model, prompt, system } => {
            // Validate chat-side flags before a potentially long auto-pull.
            let (state, mode, mut options) = match chat_setup(cli) {
                Ok(v) => v,
                Err(e) => fail(&e),
            };
            // `run`'s own --system wins over one given before the subcommand.
            if system.is_some() {
                options.system = system.clone();
            }
            if let Err(e) = cmds::ensure_model(client, model).await {
                fail(&e);
            }
            let target = chat::Target::Local {
                base_url: ollama_api::host(),
                model: Some(model.clone()),
                native_ollama: true,
            };
            chat_loop(client, target, &state, mode, options, prompt.as_deref()).await;
            return;
        }
    };
    if let Err(e) = result {
        fail(&e);
    }
}

/// Runs the chat side — a one-shot turn, or the interactive REPL when no
/// prompt is given — against an already-resolved target. Both the classic
/// flat invocation and `lm-cli run` land here. The REPL takes the target and
/// options by value since its slash commands (`/load`, `/set`) mutate them.
async fn chat_loop(
    client: &reqwest::Client,
    target: chat::Target,
    state: &AppState,
    mode: PermissionMode,
    options: chat::ChatOptions,
    prompt: Option<&str>,
) {
    if !target.is_native()
        && (options.num_ctx.is_some() || options.keep_alive.is_some() || options.think.is_some())
    {
        eprintln!("Warning: --num-ctx, --keepalive, and --think only apply to Ollama targets; ignoring.");
    }

    if let Some(prompt) = prompt {
        let mut perms = TerminalPermissions::new(mode);
        let mut history: Vec<serde_json::Value> = Vec::new();
        if let Err(e) =
            agent::run_turn(client, &target, state, &mut perms, &mut history, &options, prompt).await
        {
            eprintln!("\nError: {e}");
            std::process::exit(1);
        }
        return;
    }

    repl::run(client, target, state, mode, options).await;
}

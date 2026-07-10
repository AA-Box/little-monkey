//! LM's terminal agent: same sandboxed file/shell tools and permission
//! model as the desktop app (reused directly from `little_monkey_lib`), driven from
//! a shell instead of a WebView. Supports both a one-shot invocation
//! (`lm-cli "prompt"`) and an interactive REPL (`lm-cli`, no prompt), plus
//! Ollama-CLI-style model management subcommands (`lm-cli list/pull/run/...`)
//! spoken directly to the daemon's HTTP API.

mod agent;
mod chat;
mod checkpoints_cli;
mod cmds;
mod mcp_cli;
mod modelfile;
mod ollama_api;
mod permission;
mod providers_cli;
mod repl;
mod sse;
mod tools_cli;
mod tools_def;

use std::path::{Path, PathBuf};

use clap::{Args, Parser, Subcommand};
use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::workspace::{self, WorkspaceRoot};
use little_monkey_lib::{memory, rules, AppState};

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
    /// Revert a checkpoint's file changes (defaults to the most recent one
    /// from this CLI). Prints the restored-file count.
    Revert {
        /// Checkpoint id to revert; omit for the most recent CLI checkpoint.
        id: Option<String>,
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

/// Must match `identifier` in `src-tauri/tauri.conf.json` — same
/// hardcoded-identifier app-data resolution as `providers_cli.rs`/
/// `checkpoints_cli.rs` (duplicated per module rather than shared, following
/// their precedent).
const APP_IDENTIFIER: &str = "com.littlemonkey.app";

fn app_data_dir() -> Option<PathBuf> {
    dirs::data_dir().map(|d| d.join(APP_IDENTIFIER))
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
fn compose_system_prompt_impl(data_dir: &Path, state: &AppState, user_system: Option<&str>) -> Option<String> {
    let global_path = data_dir.join("MONKEY.md");
    let roots = workspace::all_roots(state).unwrap_or_default();
    let rule_files = rules::read_rules_impl(&global_path, &roots);

    let facts = workspace::primary_root_canon(state)
        .ok()
        .and_then(|root| {
            memory::load_impl(&data_dir.join("memories.json"))
                .ok()
                .and_then(|memories| memories.projects.get(&root.to_string_lossy().to_string()).cloned())
        })
        .map(|project| project.facts)
        .unwrap_or_default();

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

    let rules_and_facts = if sections.is_empty() { None } else { Some(sections.join("\n")) };

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
/// the sandboxed workspace state, the parsed permission mode, and the
/// validated generation options (whose `system` field is the composed
/// rules/facts + `--system` prompt — see [`effective_system`]).
fn chat_setup(cli: &Cli) -> Result<(AppState, PermissionMode, chat::ChatOptions), String> {
    let state = build_state(&cli.workspace)?;
    let mode = PermissionMode::parse(&cli.permission_mode)?;
    let mut options = cli.chat.to_options()?;
    options.system = effective_system(cli, &state, options.system.as_deref());
    Ok((state, mode, options))
}

fn fail(message: &str) -> ! {
    eprintln!("Error: {message}");
    std::process::exit(1);
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
    let mcp_entries = resolve_mcp_entries(&cli, &state).await;
    chat_loop(&client, target, &state, mode, options, cli.prompt.as_deref(), &mcp_entries).await;
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
            // `run`'s own --system wins over one given before the subcommand
            // (still composed with the rules/facts section unless
            // --no-rules — see `effective_system`).
            if let Some(system) = system {
                options.system = effective_system(cli, &state, Some(system.as_str()));
            }
            if let Err(e) = cmds::ensure_model(client, model).await {
                fail(&e);
            }
            let target = chat::Target::Local {
                base_url: ollama_api::host(),
                model: Some(model.clone()),
                native_ollama: true,
            };
            let mcp_entries = resolve_mcp_entries(cli, &state).await;
            chat_loop(client, target, &state, mode, options, prompt.as_deref(), &mcp_entries).await;
            return;
        }
        Cmd::Revert { id } => match checkpoints_cli::revert(id.as_deref()) {
            Ok(count) => {
                println!("Restored {count} file(s).");
                Ok(())
            }
            Err(e) => Err(e),
        },
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
    mcp_entries: &[McpServerEntry],
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
            agent::run_turn(client, &target, state, &mut perms, &mut history, &options, prompt, mcp_entries).await
        {
            eprintln!("\nError: {e}");
            std::process::exit(1);
        }
        return;
    }

    repl::run(client, target, state, mode, options, mcp_entries).await;
}

#[cfg(test)]
mod tests {
    use super::*;
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
            let path = std::env::temp_dir().join(format!("lm_cli_main_test_{}_{}_{}", std::process::id(), n, nanos));
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
    fn no_rules_no_facts_no_system_composes_to_none() {
        let data_dir = TempDir::new();
        let ws = TempDir::new();
        let state = state_with_primary_root(&ws.path);

        assert_eq!(compose_system_prompt_impl(&data_dir.path, &state, None), None);
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
        )
        .unwrap();
        assert_eq!(fact.source, "agent");

        let prompt = compose_system_prompt_impl(&data_dir.path, &state, None).unwrap();
        assert!(prompt.contains("## Remembered facts"));
        assert!(prompt.contains("- Uses pnpm, not npm."));
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
            prompt: None,
            workspace: None,
            provider: None,
            model: None,
            ollama: None,
            local_url: None,
            permission_mode: "manual".to_string(),
            no_rules: true,
            no_mcp: false,
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
            },
        };

        assert_eq!(effective_system(&cli, &state, Some("Only this.")), Some("Only this.".to_string()));
        assert_eq!(effective_system(&cli, &state, None), None);
    }
}

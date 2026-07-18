//! The interactive REPL — an `ollama run`-style chat prompt: rustyline line
//! editing with persistent history in `~/.monkey_cli_history`, a `>>> ` prompt,
//! `"""` multi-line messages, and slash commands (`/set`, `/show`, `/save`,
//! `/load`, `/revert`, `/clear`, `/bye`, `/?`) that mutate the live session. When stdin
//! is not a TTY the whole thing falls back to a plain `read_line` loop with
//! no prompts or banner, so piped input (`echo hi | monkey-cli --ollama X`)
//! keeps working.

use std::io::IsTerminal;
use std::path::PathBuf;

use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::prompts::PromptEntry;
use little_monkey_lib::AppState;
use rustyline::error::ReadlineError;

use crate::agent;
use crate::chat::{self, Target};
use crate::cmds;
use crate::ollama_api;
use crate::permission::{PermissionMode, TerminalPermissions};
use crate::skills_cli::{self, CliSkill};

/// One read from the line source. `Interrupted` (Ctrl-C) only occurs on the
/// rustyline path; the plain reader maps everything unusual to `Eof`.
enum ReadOutcome {
    Line(String),
    Interrupted,
    Eof,
}

/// Line source: rustyline (editing, shortcuts, history file) on a TTY, a
/// plain unprompted `read_line` otherwise. Reads block the calling thread —
/// fine here, the REPL is strictly sequential (nothing else needs the
/// runtime while we sit at the prompt).
struct Reader {
    editor: Option<(rustyline::DefaultEditor, Option<PathBuf>)>,
}

impl Reader {
    fn new() -> Self {
        if std::io::stdin().is_terminal() {
            if let Ok(mut editor) = rustyline::DefaultEditor::new() {
                let path = dirs::home_dir().map(|h| h.join(".monkey_cli_history"));
                if let Some(path) = &path {
                    let _ = editor.load_history(path);
                }
                return Self {
                    editor: Some((editor, path)),
                };
            }
        }
        Self { editor: None }
    }

    fn interactive(&self) -> bool {
        self.editor.is_some()
    }

    /// Reads one line (without its trailing newline). Interactive reads are
    /// appended to the history file as they happen.
    fn read(&mut self, prompt: &str) -> ReadOutcome {
        match &mut self.editor {
            Some((editor, path)) => match editor.readline(prompt) {
                Ok(line) => {
                    if !line.trim().is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                        if let Some(path) = path {
                            let _ = editor.save_history(path);
                        }
                    }
                    ReadOutcome::Line(line)
                }
                Err(ReadlineError::Interrupted) => ReadOutcome::Interrupted,
                Err(_) => ReadOutcome::Eof, // Eof, or a terminal error we can't recover from
            },
            None => {
                let mut line = String::new();
                match std::io::stdin().read_line(&mut line) {
                    Ok(0) | Err(_) => ReadOutcome::Eof,
                    Ok(_) => {
                        while line.ends_with('\n') || line.ends_with('\r') {
                            line.pop();
                        }
                        ReadOutcome::Line(line)
                    }
                }
            }
        }
    }
}

/// Runs the interactive session against an already-resolved target until
/// `/bye`, `exit`, `quit`, or EOF. `options.system` coming in is just
/// rules/facts + `--system` text — WITHOUT any `--persona` folded in (see
/// `main.rs::chat_setup`/`chat_loop`); `initial_persona` carries that
/// resolved persona, if any, as structured data so it becomes the REPL's
/// own active persona from the start instead of being baked unremovably
/// into the base text.
pub async fn run(
    client: &reqwest::Client,
    mut target: Target,
    state: &AppState,
    mode: PermissionMode,
    mut options: chat::ChatOptions,
    initial_persona: Option<PromptEntry>,
    skills: Vec<CliSkill>,
    mcp_entries: &[McpServerEntry],
    attached_stacks: &[String],
) {
    let mut reader = Reader::new();
    if reader.interactive() {
        println!("Type /? for help. \"\"\" begins multiline.\n");
    }

    let mut perms = TerminalPermissions::new(mode);
    let mut history: Vec<serde_json::Value> = Vec::new();
    let mut keep_history = true;
    // The system text `/persona` layers its section on top of, restored
    // verbatim by `/persona clear` — the rules/facts + `--system` text ONLY,
    // never a persona (see the doc comment on `run` above). Kept in sync by
    // `/set system` too (see `handle_set`). Layering rather than clobbering
    // mirrors the desktop app's "append, never replace" `composeSystemPrompt`
    // convention.
    let mut system_base = options.system.clone();
    // The REPL's own active persona, set by `/persona <command>` and cleared
    // by `/persona clear`/`/persona none` — seeded from whatever `--persona`
    // resolved to at startup (if anything), so `/persona clear` can actually
    // remove it and `/persona <other>` replaces it instead of stacking.
    let mut persona: Option<PromptEntry> = initial_persona;
    options.system = crate::compose_persona_and_system(persona.as_ref(), system_base.as_deref());

    loop {
        let line = match reader.read(">>> ") {
            ReadOutcome::Line(l) => l,
            ReadOutcome::Interrupted => {
                println!("Use Ctrl + d or /bye to exit.");
                continue;
            }
            ReadOutcome::Eof => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "exit" || trimmed == "quit" || trimmed == "/bye" {
            break;
        }

        if trimmed.starts_with('/') {
            let slash_command = trimmed
                .strip_prefix('/')
                .and_then(|value| value.split_whitespace().next())
                .unwrap_or_default();
            if skills.iter().any(|skill| skill.command == slash_command) {
                if !keep_history {
                    truncate_to_system(&mut history);
                }
                let mut skill_options = options.clone();
                skill_options.system = match skills_cli::compose_for_prompt(
                    options.system.as_deref(),
                    trimmed,
                    &skills,
                ) {
                    Ok(system) => system,
                    Err(error) => {
                        eprintln!("Error: {error}");
                        continue;
                    }
                };
                if let Err(error) = agent::run_turn(
                    client,
                    &target,
                    state,
                    &mut perms,
                    &mut history,
                    &skill_options,
                    trimmed,
                    mcp_entries,
                    attached_stacks,
                )
                .await
                {
                    eprintln!("\nError: {error}");
                }
                println!();
                continue;
            }
            let result = handle_command(
                client,
                state,
                &mut target,
                &mut options,
                &mut history,
                &mut keep_history,
                &mut system_base,
                &mut persona,
                &skills,
                trimmed,
            )
            .await;
            if let Err(e) = result {
                eprintln!("Error: {e}");
            }
            continue;
        }

        let text = match read_prompt_text(trimmed, |p| reader.read(p)) {
            Some(t) => t,
            None => continue, // multiline input cancelled with Ctrl-C
        };
        if text.is_empty() {
            continue;
        }

        if !keep_history {
            truncate_to_system(&mut history);
        }
        if let Err(e) = agent::run_turn(
            client,
            &target,
            state,
            &mut perms,
            &mut history,
            &options,
            &text,
            mcp_entries,
            attached_stacks,
        )
        .await
        {
            eprintln!("\nError: {e}");
        }
        println!();
    }
}

/// Expands a `"""` opener into the full multi-line message, pulling
/// continuation lines from `next_line` (prompted `... `) until the closing
/// `"""`. Non-openers pass through unchanged. Returns `None` when the input
/// was cancelled with Ctrl-C; EOF closes the message as-is.
fn read_prompt_text(first: &str, mut next_line: impl FnMut(&str) -> ReadOutcome) -> Option<String> {
    let Some(rest) = first.strip_prefix("\"\"\"") else {
        return Some(first.to_string());
    };
    if let Some(end) = rest.find("\"\"\"") {
        return Some(rest[..end].trim().to_string());
    }
    let mut parts = vec![rest.to_string()];
    loop {
        match next_line("... ") {
            ReadOutcome::Line(line) => match line.find("\"\"\"") {
                Some(end) => {
                    parts.push(line[..end].to_string());
                    return Some(parts.join("\n").trim().to_string());
                }
                None => parts.push(line),
            },
            ReadOutcome::Interrupted => return None,
            ReadOutcome::Eof => return Some(parts.join("\n").trim().to_string()),
        }
    }
}

/// Drops everything from the conversation except a leading system message.
fn truncate_to_system(history: &mut Vec<serde_json::Value>) {
    let keep = usize::from(
        history
            .first()
            .map(|m| m["role"] == "system")
            .unwrap_or(false),
    );
    history.truncate(keep);
}

/// Dispatches one slash command. Errors are printed by the caller without
/// ending the session.
async fn handle_command(
    client: &reqwest::Client,
    state: &AppState,
    target: &mut Target,
    options: &mut chat::ChatOptions,
    history: &mut Vec<serde_json::Value>,
    keep_history: &mut bool,
    system_base: &mut Option<String>,
    persona: &mut Option<PromptEntry>,
    skills: &[CliSkill],
    line: &str,
) -> Result<(), String> {
    let (cmd, rest) = split_first_word(line);
    match cmd {
        "/?" | "/help" => {
            match split_first_word(rest).0 {
                "shortcuts" => print_shortcuts(),
                _ => print_help(),
            }
            Ok(())
        }
        "/clear" => {
            truncate_to_system(history);
            println!("Cleared session context");
            Ok(())
        }
        "/set" => handle_set(
            options,
            keep_history,
            target.is_native(),
            system_base,
            persona,
            rest,
        ),
        "/show" => handle_show(client, target, options, rest).await,
        "/save" => handle_save(client, target, options, rest).await,
        "/load" => handle_load(client, target, rest).await,
        "/revert" => handle_revert(rest),
        "/persona" => handle_persona(options, system_base, persona, rest),
        "/prompts" => {
            print_prompts();
            Ok(())
        }
        "/skills" => {
            if skills.is_empty() {
                println!("No eligible skills are enabled.");
            } else {
                println!("Enabled skills:");
                for skill in skills {
                    println!(
                        "  /{:<24} {} [{} {}]",
                        skill.command, skill.name, skill.source, skill.version
                    );
                }
            }
            Ok(())
        }
        "/verify" => handle_verify(state, rest).await,
        other => Err(format!("Unknown command '{other}'. Type /? for help")),
    }
}

/// `/verify` (no args): lists the current workspace's configured
/// verification commands (see `little_monkey_lib::verify`), enabled or not,
/// so a user can see what's set up before turning on `--verify`. `/verify
/// run`: runs every ENABLED command right now, on demand, via the same
/// `verify::run_command_impl` `agent.rs`'s automatic post-turn phase uses —
/// just without feeding any failure back into the conversation, since this
/// is a manual spot-check, not a turn's verification phase.
async fn handle_verify(state: &AppState, rest: &str) -> Result<(), String> {
    let root = little_monkey_lib::workspace::primary_root_canon(state)?;
    let (sub, _) = split_first_word(rest);

    if sub == "run" {
        let commands = crate::verify_cli::enabled_commands(&root);
        if commands.is_empty() {
            println!("No enabled verification commands configured for this workspace.");
            return Ok(());
        }
        for cmd in &commands {
            println!("Running \"{}\"...", cmd.label);
            let result = little_monkey_lib::verify::run_command_impl(state, &root, cmd, None).await;
            let ok = !result.timed_out && result.code == Some(0);
            println!(
                "{} — {} ({} ms)",
                result.label,
                if ok { "PASS" } else { "FAIL" },
                result.duration_ms
            );
            if !ok {
                let stdout = result.stdout.trim();
                let stderr = result.stderr.trim();
                if !stdout.is_empty() {
                    println!("{stdout}");
                }
                if !stderr.is_empty() {
                    println!("{stderr}");
                }
            }
        }
        return Ok(());
    }

    let commands = crate::verify_cli::all_commands(&root);
    if commands.is_empty() {
        println!(
            "No verification commands configured for this workspace. Configure them in the desktop app's Settings > Verification tab."
        );
        return Ok(());
    }
    println!("Configured verification commands:");
    for cmd in &commands {
        let mark = if cmd.enabled { "x" } else { " " };
        println!(
            "  [{mark}] {:<8} {:<20} {}",
            cmd.kind, cmd.label, cmd.command
        );
    }
    println!("\nUse /verify run to run the enabled commands now.");
    Ok(())
}

/// `/persona <command>`: resolves a saved persona by its slash-command
/// (`crate::resolve_persona_entry`, the same `prompts.json` the desktop app
/// reads/writes) and layers its content on top of `system_base` via
/// `crate::compose_persona_and_system` — the REPL analogue of the toolbar
/// `PersonaSelector` in the desktop app. `/persona clear` (or `/persona
/// none`) removes the layered section, restoring `system_base` unchanged.
fn handle_persona(
    options: &mut chat::ChatOptions,
    system_base: &Option<String>,
    persona: &mut Option<PromptEntry>,
    args: &str,
) -> Result<(), String> {
    let (sub, _) = split_first_word(args);
    if sub.is_empty() {
        return Err("Usage: /persona <command>  (/persona clear to remove)".to_string());
    }
    if sub == "clear" || sub == "none" {
        *persona = None;
        options.system = system_base.clone();
        println!("Cleared active persona.");
        return Ok(());
    }
    let entry = crate::resolve_persona_entry(sub)?;
    println!("Set active persona: {}", entry.name);
    *persona = Some(entry);
    options.system = crate::compose_persona_and_system(persona.as_ref(), system_base.as_deref());
    Ok(())
}

/// `/prompts`: lists every saved prompt-library entry (personas and
/// snippets alike) — the REPL's read-only analogue of the desktop app's
/// Settings > Prompts tab. Snippet *insertion* stays GUI-only per the design
/// doc (meaningless in a line-editor REPL), but listing them here still
/// lets a user see what commands are already taken before picking a new
/// one, or check a persona's exact command before `/persona <command>`.
fn print_prompts() {
    let entries = crate::load_prompt_entries();
    if entries.is_empty() {
        println!("No saved prompts. Use the desktop app's Settings > Prompts tab to create one.");
        return;
    }
    for entry in &entries {
        let desc = entry.description.as_deref().unwrap_or("");
        println!(
            "  {:<8} /{:<20} {}  {}",
            entry.kind, entry.command, entry.name, desc
        );
    }
}

/// `/revert [id]`: restores a checkpoint's file changes — defaults to the
/// most recent one opened by this CLI session (see `checkpoints_cli.rs`).
/// Prints the restored-file count; never touches `history` or the
/// conversation (no rewind for the CLI, unlike the desktop app).
fn handle_revert(rest: &str) -> Result<(), String> {
    let id = rest.trim();
    let count = crate::checkpoints_cli::revert(if id.is_empty() { None } else { Some(id) })?;
    println!("Restored {count} file(s).");
    Ok(())
}

/// Warns (stderr) that an Ollama-only option was set on a target whose wire
/// protocol has no equivalent — it is stored but never sent.
fn warn_native_only(name: &str) {
    eprintln!("Warning: '{name}' only applies to Ollama targets; ignoring.");
}

/// `/set ...`: mutates the live `ChatOptions` (and the history toggle).
/// `is_native` gates the warning for Ollama-only options that OpenAI-compat
/// requests silently drop. `system_base`/`persona` are threaded through so
/// `/set system` updates the text `/persona` layers on top of, rather than
/// silently discarding whichever persona is currently active (see
/// `handle_persona`).
fn handle_set(
    options: &mut chat::ChatOptions,
    keep_history: &mut bool,
    is_native: bool,
    system_base: &mut Option<String>,
    persona: &Option<PromptEntry>,
    args: &str,
) -> Result<(), String> {
    let (sub, rest) = split_first_word(args);
    match sub {
        "" => {
            print_set_help();
            Ok(())
        }
        "parameter" => {
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.is_empty() {
                print_parameter_help();
                return Ok(());
            }
            let message = set_parameter(options, is_native, tokens[0], &tokens[1..])?;
            println!("{message}");
            Ok(())
        }
        "system" => {
            if rest.is_empty() {
                return Err("Usage: /set system <message>".to_string());
            }
            *system_base = Some(strip_wrapping_quotes(rest).to_string());
            options.system =
                crate::compose_persona_and_system(persona.as_ref(), system_base.as_deref());
            println!("Set system message.");
            Ok(())
        }
        "format" => {
            if rest.is_empty() {
                return Err("Usage: /set format <json|inline-schema|@file>".to_string());
            }
            let value = chat::parse_format_flag(rest.trim())?;
            let label = if value == serde_json::Value::String("json".to_string()) {
                "'json'"
            } else {
                "the given schema"
            };
            options.format = Some(value);
            println!("Set format to {label}.");
            Ok(())
        }
        "noformat" => {
            options.format = None;
            println!("Disabled format.");
            Ok(())
        }
        "think" => {
            let value = if rest.is_empty() { "true" } else { rest.trim() };
            options.think = Some(chat::parse_think_flag(value)?);
            if !is_native {
                warn_native_only("think");
            }
            println!("Set 'think' mode.");
            Ok(())
        }
        "nothink" => {
            options.think = Some(serde_json::Value::Bool(false));
            if !is_native {
                warn_native_only("think");
            }
            println!("Set 'nothink' mode.");
            Ok(())
        }
        "verbose" => {
            options.verbose = true;
            println!("Set 'verbose' mode.");
            Ok(())
        }
        "quiet" => {
            options.verbose = false;
            println!("Set 'quiet' mode.");
            Ok(())
        }
        "history" => {
            *keep_history = true;
            println!("Enabled history.");
            Ok(())
        }
        "nohistory" => {
            *keep_history = false;
            println!("Disabled history.");
            Ok(())
        }
        other => Err(format!(
            "Unknown /set option '{other}'. Type /set for options"
        )),
    }
}

/// `/set parameter <name> <value...>` — returns the confirmation line.
fn set_parameter(
    options: &mut chat::ChatOptions,
    is_native: bool,
    name: &str,
    values: &[&str],
) -> Result<String, String> {
    if values.is_empty() {
        return Err(format!("Usage: /set parameter {name} <value>"));
    }
    let first = values[0];
    let as_f64 = |v: &str| {
        v.parse::<f64>()
            .map_err(|_| format!("Invalid value '{v}' for '{name}' (expected a number)"))
    };
    let as_i64 = |v: &str| {
        v.parse::<i64>()
            .map_err(|_| format!("Invalid value '{v}' for '{name}' (expected an integer)"))
    };
    match name {
        "temperature" => options.temperature = Some(as_f64(first)?),
        "top_p" => options.top_p = Some(as_f64(first)?),
        "seed" => options.seed = Some(as_i64(first)?),
        "num_ctx" => {
            options.num_ctx = Some(as_i64(first)?);
            if !is_native {
                warn_native_only("num_ctx");
            }
        }
        "num_predict" => options.num_predict = Some(as_i64(first)?),
        "stop" => {
            options.stop = values
                .iter()
                .map(|v| strip_wrapping_quotes(v).to_string())
                .collect()
        }
        other => {
            return Err(format!(
                "Unknown parameter '{other}' (temperature, top_p, seed, num_ctx, num_predict, stop)"
            ))
        }
    }
    Ok(format!("Set parameter '{name}' to '{}'", values.join(" ")))
}

/// `/show <section>`: model info via the daemon for Ollama targets, a short
/// target description for everything else.
async fn handle_show(
    client: &reqwest::Client,
    target: &Target,
    options: &chat::ChatOptions,
    args: &str,
) -> Result<(), String> {
    let (sub, _) = split_first_word(args);
    if sub.is_empty() {
        print_show_help();
        return Ok(());
    }
    let model = match target {
        Target::Local {
            model,
            native_ollama: true,
            ..
        } => model.clone().unwrap_or_default(),
        Target::Provider { provider_id, model } => {
            if sub == "info" {
                println!("  Target\n    provider    {provider_id}\n    model       {model}");
                return Ok(());
            }
            return Err(format!("'/show {sub}' requires an Ollama target"));
        }
        Target::Local {
            base_url, model, ..
        } => {
            if sub == "info" {
                println!(
                    "  Target\n    endpoint    {base_url} (OpenAI-compatible)\n    model       {}",
                    model.clone().unwrap_or_else(|| "local".to_string())
                );
                return Ok(());
            }
            return Err(format!("'/show {sub}' requires an Ollama target"));
        }
    };
    match sub {
        "info" => cmds::show(client, &model, false, false, false, false, false).await,
        "parameters" => cmds::show(client, &model, false, true, false, false, false).await,
        "template" => cmds::show(client, &model, false, false, true, false, false).await,
        "system" => match &options.system {
            // The session's system message shadows the model's, same as it
            // does on the wire.
            Some(system) => {
                println!("{system}");
                Ok(())
            }
            None => cmds::show(client, &model, false, false, false, true, false).await,
        },
        "license" => cmds::show(client, &model, false, false, false, false, true).await,
        "modelfile" => cmds::show(client, &model, true, false, false, false, false).await,
        other => Err(format!(
            "Unknown /show option '{other}'. Type /show for options"
        )),
    }
}

/// `/save <name>`: persists the session's system message and parameters as a
/// new model layered on the current one (Ollama targets only).
async fn handle_save(
    client: &reqwest::Client,
    target: &Target,
    options: &chat::ChatOptions,
    args: &str,
) -> Result<(), String> {
    let (name, _) = split_first_word(args);
    if name.is_empty() {
        return Err("Usage: /save <modelname>".to_string());
    }
    let Target::Local {
        model,
        native_ollama: true,
        ..
    } = target
    else {
        return Err("/save requires an Ollama target".to_string());
    };
    let mut parameters = serde_json::Map::new();
    if let Some(temperature) = options.temperature {
        parameters.insert("temperature".to_string(), serde_json::json!(temperature));
    }
    if let Some(top_p) = options.top_p {
        parameters.insert("top_p".to_string(), serde_json::json!(top_p));
    }
    if let Some(seed) = options.seed {
        parameters.insert("seed".to_string(), serde_json::json!(seed));
    }
    if let Some(num_ctx) = options.num_ctx {
        parameters.insert("num_ctx".to_string(), serde_json::json!(num_ctx));
    }
    if let Some(num_predict) = options.num_predict {
        parameters.insert("num_predict".to_string(), serde_json::json!(num_predict));
    }
    if !options.stop.is_empty() {
        parameters.insert("stop".to_string(), serde_json::json!(options.stop));
    }
    let req = ollama_api::CreateRequest {
        model: name.to_string(),
        from: Some(model.clone().unwrap_or_default()),
        system: options.system.clone(),
        parameters: if parameters.is_empty() {
            None
        } else {
            Some(parameters)
        },
        stream: true,
        ..Default::default()
    };
    ollama_api::create(client, &req, |_| {}).await?;
    println!("Created new model '{name}'");
    Ok(())
}

/// `/load <name>`: switches the session to another local model (keeping the
/// conversation, like ollama). Missing models are not auto-pulled.
async fn handle_load(
    client: &reqwest::Client,
    target: &mut Target,
    args: &str,
) -> Result<(), String> {
    let (name, _) = split_first_word(args);
    if name.is_empty() {
        return Err("Usage: /load <modelname>".to_string());
    }
    let Target::Local {
        model,
        native_ollama: true,
        ..
    } = target
    else {
        return Err("/load requires an Ollama target".to_string());
    };
    let tags = ollama_api::tags(client).await?;
    let want = if name.contains(':') {
        name.to_string()
    } else {
        format!("{name}:latest")
    };
    if !tags.models.iter().any(|m| m.name == want) {
        return Err(format!(
            "model '{name}' not found — run: monkey pull {name}"
        ));
    }
    *model = Some(want.clone());
    println!("Loading model '{want}'");
    Ok(())
}

/// Splits off the first whitespace-delimited word, returning it and the
/// trimmed remainder (both empty for blank input).
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim();
    match s.find(char::is_whitespace) {
        Some(i) => (&s[..i], s[i..].trim_start()),
        None => (s, ""),
    }
}

/// Strips one matched pair of wrapping quotes (`"""`, `"`, or `'`).
fn strip_wrapping_quotes(s: &str) -> &str {
    let s = s.trim();
    for quote in ["\"\"\"", "\"", "'"] {
        if s.len() >= 2 * quote.len() && s.starts_with(quote) && s.ends_with(quote) {
            return &s[quote.len()..s.len() - quote.len()];
        }
    }
    s
}

fn print_help() {
    println!(
        "Available Commands:
  /set             Set session options
  /show            Show model information
  /save <model>    Save the session's system/parameters as a new model
  /load <model>    Switch to a different local model (keeps the conversation)
  /revert [id]     Restore a checkpoint's files (defaults to the most recent)
  /persona <cmd>   Set the active persona (/persona clear to remove)
  /prompts         List saved personas, snippets, and local prompt skills
  /skills          List eligible native, local, and signed-package skills
  /<skill> [args]  Invoke an enabled skill for exactly one turn
  /verify          List this workspace's configured verification commands
  /verify run      Run the enabled verification commands now
  /clear           Clear session context
  /bye             Exit (also exit, quit, or Ctrl+D)
  /?, /help        Help for a command
  /? shortcuts     Help for keyboard shortcuts

Use \"\"\" to begin a multi-line message."
    );
}

fn print_shortcuts() {
    println!(
        "Available keyboard shortcuts:
  Ctrl + a         Move to the beginning of the line (Home)
  Ctrl + e         Move to the end of the line (End)
  Ctrl + k         Delete to the end of the line
  Ctrl + u         Delete to the beginning of the line
  Ctrl + w         Delete the word before the cursor
  Ctrl + l         Clear the screen
  Ctrl + c         Cancel the current input
  Ctrl + d         Exit (on an empty line)
  Up / Down        Walk the input history (~/.monkey_cli_history)"
    );
}

fn print_set_help() {
    println!(
        "Available Commands:
  /set parameter ...      Set a generation parameter
  /set system <message>   Set the session system message
  /set format json        Constrain output to JSON (/set noformat to disable)
  /set think [level]      Enable thinking: true/low/medium/high (/set nothink to disable)
  /set verbose            Show timing/token metrics (/set quiet to disable)
  /set history            Keep conversation context (/set nohistory to disable)"
    );
}

fn print_parameter_help() {
    println!(
        "Available Parameters:
  /set parameter temperature <float>
  /set parameter top_p <float>
  /set parameter seed <int>
  /set parameter num_ctx <int>
  /set parameter num_predict <int>
  /set parameter stop <string> [<string> ...]"
    );
}

fn print_show_help() {
    println!(
        "Available Commands:
  /show info         Show details for this model
  /show license      Show model license
  /show modelfile    Show Modelfile for this model
  /show parameters   Show parameters for this model
  /show system       Show system message
  /show template     Show prompt template"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_first_word_cases() {
        assert_eq!(split_first_word(""), ("", ""));
        assert_eq!(split_first_word("  "), ("", ""));
        assert_eq!(split_first_word("word"), ("word", ""));
        assert_eq!(
            split_first_word("set parameter temperature"),
            ("set", "parameter temperature")
        );
        assert_eq!(split_first_word("  a   b c  "), ("a", "b c"));
    }

    #[test]
    fn strip_wrapping_quotes_cases() {
        assert_eq!(strip_wrapping_quotes("plain"), "plain");
        assert_eq!(strip_wrapping_quotes("\"quoted\""), "quoted");
        assert_eq!(strip_wrapping_quotes("'quoted'"), "quoted");
        assert_eq!(strip_wrapping_quotes("\"\"\"triple\"\"\""), "triple");
        assert_eq!(strip_wrapping_quotes("\"unbalanced"), "\"unbalanced");
        assert_eq!(strip_wrapping_quotes("mid\"dle"), "mid\"dle");
    }

    #[test]
    fn set_parameter_updates_options() {
        let mut options = chat::ChatOptions::default();
        assert_eq!(
            set_parameter(&mut options, true, "temperature", &["0.5"]).unwrap(),
            "Set parameter 'temperature' to '0.5'"
        );
        set_parameter(&mut options, true, "top_p", &["0.9"]).unwrap();
        set_parameter(&mut options, true, "seed", &["7"]).unwrap();
        set_parameter(&mut options, true, "num_ctx", &["8192"]).unwrap();
        set_parameter(&mut options, true, "num_predict", &["128"]).unwrap();
        set_parameter(&mut options, true, "stop", &["\"END\"", "STOP"]).unwrap();
        assert_eq!(options.temperature, Some(0.5));
        assert_eq!(options.top_p, Some(0.9));
        assert_eq!(options.seed, Some(7));
        assert_eq!(options.num_ctx, Some(8192));
        assert_eq!(options.num_predict, Some(128));
        assert_eq!(options.stop, vec!["END".to_string(), "STOP".to_string()]);
    }

    #[test]
    fn set_parameter_rejects_bad_input() {
        let mut options = chat::ChatOptions::default();
        assert!(set_parameter(&mut options, true, "temperature", &[]).is_err());
        assert!(set_parameter(&mut options, true, "temperature", &["hot"]).is_err());
        assert!(set_parameter(&mut options, true, "seed", &["1.5"]).is_err());
        assert!(set_parameter(&mut options, true, "nope", &["1"]).is_err());
        assert_eq!(options.temperature, None);
    }

    #[test]
    fn set_parameter_still_sets_ollama_only_values_on_non_native_targets() {
        // The value is stored (and a stderr warning printed, not asserted
        // here) — matching the startup flags' warn-and-ignore behavior.
        let mut options = chat::ChatOptions::default();
        set_parameter(&mut options, false, "num_ctx", &["8192"]).unwrap();
        assert_eq!(options.num_ctx, Some(8192));
    }

    #[test]
    fn handle_set_toggles_and_modes() {
        let mut options = chat::ChatOptions::default();
        let mut keep_history = true;
        let mut system_base: Option<String> = None;
        let persona: Option<PromptEntry> = None;

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "verbose",
        )
        .unwrap();
        assert!(options.verbose);
        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "quiet",
        )
        .unwrap();
        assert!(!options.verbose);

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "nohistory",
        )
        .unwrap();
        assert!(!keep_history);
        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "history",
        )
        .unwrap();
        assert!(keep_history);

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "think low",
        )
        .unwrap();
        assert_eq!(options.think, Some(serde_json::json!("low")));
        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "think",
        )
        .unwrap();
        assert_eq!(options.think, Some(serde_json::json!(true)));
        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "nothink",
        )
        .unwrap();
        assert_eq!(options.think, Some(serde_json::json!(false)));

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "format json",
        )
        .unwrap();
        assert_eq!(options.format, Some(serde_json::json!("json")));
        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "noformat",
        )
        .unwrap();
        assert_eq!(options.format, None);

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "system \"You are terse.\"",
        )
        .unwrap();
        assert_eq!(options.system.as_deref(), Some("You are terse."));

        assert!(handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "bogus on"
        )
        .is_err());
        assert!(handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "system"
        )
        .is_err());
    }

    fn stub_persona(name: &str, content: &str) -> PromptEntry {
        PromptEntry {
            id: "p1".to_string(),
            kind: "persona".to_string(),
            name: name.to_string(),
            command: "stub".to_string(),
            content: content.to_string(),
            description: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn handle_set_system_recomposes_on_top_of_the_active_persona() {
        // `/set system` after a persona is already active must not silently
        // drop the persona section — it re-layers it via
        // `compose_persona_and_system` (see `handle_set`'s "system" arm).
        let mut options = chat::ChatOptions::default();
        let mut keep_history = true;
        let mut system_base: Option<String> = None;
        let persona = Some(stub_persona("Terse", "Be brief."));

        handle_set(
            &mut options,
            &mut keep_history,
            true,
            &mut system_base,
            &persona,
            "system \"Reply in French.\"",
        )
        .unwrap();

        assert_eq!(system_base.as_deref(), Some("Reply in French."));
        let system = options.system.as_deref().unwrap();
        assert!(system.contains("## Active persona: Terse\nBe brief."));
        assert!(system.ends_with("Reply in French."));
    }

    #[test]
    fn handle_persona_requires_an_argument() {
        let mut options = chat::ChatOptions::default();
        let system_base: Option<String> = None;
        let mut persona: Option<PromptEntry> = None;
        assert!(handle_persona(&mut options, &system_base, &mut persona, "").is_err());
        assert!(persona.is_none());
    }

    #[test]
    fn handle_persona_clear_restores_system_base_and_drops_the_persona() {
        let mut options = chat::ChatOptions {
            system: Some("## Active persona: Terse\nBe brief.\n\nUser system.".to_string()),
            ..chat::ChatOptions::default()
        };
        let system_base = Some("User system.".to_string());
        let mut persona = Some(stub_persona("Terse", "Be brief."));

        handle_persona(&mut options, &system_base, &mut persona, "clear").unwrap();

        assert!(persona.is_none());
        assert_eq!(options.system.as_deref(), Some("User system."));

        // "none" is accepted as a synonym for "clear".
        let mut persona2 = Some(stub_persona("Terse", "Be brief."));
        handle_persona(&mut options, &system_base, &mut persona2, "none").unwrap();
        assert!(persona2.is_none());
    }

    #[test]
    fn truncate_to_system_keeps_leading_system_only() {
        let mut history = vec![
            serde_json::json!({ "role": "system", "content": "sys" }),
            serde_json::json!({ "role": "user", "content": "hi" }),
            serde_json::json!({ "role": "assistant", "content": "hello" }),
        ];
        truncate_to_system(&mut history);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["role"], "system");

        let mut history = vec![
            serde_json::json!({ "role": "user", "content": "hi" }),
            serde_json::json!({ "role": "assistant", "content": "hello" }),
        ];
        truncate_to_system(&mut history);
        assert!(history.is_empty());
    }

    /// Feeds scripted lines to `read_prompt_text` in place of the terminal.
    fn scripted(lines: Vec<ReadOutcome>) -> impl FnMut(&str) -> ReadOutcome {
        let mut lines = lines.into_iter();
        move |_prompt| lines.next().unwrap_or(ReadOutcome::Eof)
    }

    #[test]
    fn read_prompt_text_passthrough_and_single_line() {
        assert_eq!(
            read_prompt_text("plain prompt", scripted(vec![])),
            Some("plain prompt".to_string())
        );
        assert_eq!(
            read_prompt_text("\"\"\"one line\"\"\"", scripted(vec![])),
            Some("one line".to_string())
        );
    }

    #[test]
    fn read_prompt_text_multiline_joins_until_close() {
        let lines = vec![
            ReadOutcome::Line("world".to_string()),
            ReadOutcome::Line("end\"\"\" trailing ignored".to_string()),
        ];
        assert_eq!(
            read_prompt_text("\"\"\"hello", scripted(lines)),
            Some("hello\nworld\nend".to_string())
        );
    }

    #[test]
    fn read_prompt_text_interrupt_cancels_eof_closes() {
        let lines = vec![
            ReadOutcome::Line("kept".to_string()),
            ReadOutcome::Interrupted,
        ];
        assert_eq!(read_prompt_text("\"\"\"", scripted(lines)), None);

        let lines = vec![ReadOutcome::Line("kept".to_string()), ReadOutcome::Eof];
        assert_eq!(
            read_prompt_text("\"\"\"", scripted(lines)),
            Some("kept".to_string())
        );
    }
}

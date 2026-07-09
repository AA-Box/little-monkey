//! The interactive REPL — an `ollama run`-style chat prompt: rustyline line
//! editing with persistent history in `~/.lm_cli_history`, a `>>> ` prompt,
//! `"""` multi-line messages, and slash commands (`/set`, `/show`, `/save`,
//! `/load`, `/clear`, `/bye`, `/?`) that mutate the live session. When stdin
//! is not a TTY the whole thing falls back to a plain `read_line` loop with
//! no prompts or banner, so piped input (`echo hi | lm-cli --ollama X`)
//! keeps working.

use std::io::IsTerminal;
use std::path::PathBuf;

use little_monkey_lib::AppState;
use rustyline::error::ReadlineError;

use crate::agent;
use crate::chat::{self, Target};
use crate::cmds;
use crate::ollama_api;
use crate::permission::{PermissionMode, TerminalPermissions};

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
                let path = dirs::home_dir().map(|h| h.join(".lm_cli_history"));
                if let Some(path) = &path {
                    let _ = editor.load_history(path);
                }
                return Self { editor: Some((editor, path)) };
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
/// `/bye`, `exit`, `quit`, or EOF.
pub async fn run(
    client: &reqwest::Client,
    mut target: Target,
    state: &AppState,
    mode: PermissionMode,
    mut options: chat::ChatOptions,
) {
    let mut reader = Reader::new();
    if reader.interactive() {
        println!("Type /? for help. \"\"\" begins multiline.\n");
    }

    let mut perms = TerminalPermissions::new(mode);
    let mut history: Vec<serde_json::Value> = Vec::new();
    let mut keep_history = true;

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
            let result = handle_command(
                client,
                &mut target,
                &mut options,
                &mut history,
                &mut keep_history,
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
        if let Err(e) =
            agent::run_turn(client, &target, state, &mut perms, &mut history, &options, &text).await
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
    let keep = usize::from(history.first().map(|m| m["role"] == "system").unwrap_or(false));
    history.truncate(keep);
}

/// Dispatches one slash command. Errors are printed by the caller without
/// ending the session.
async fn handle_command(
    client: &reqwest::Client,
    target: &mut Target,
    options: &mut chat::ChatOptions,
    history: &mut Vec<serde_json::Value>,
    keep_history: &mut bool,
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
        "/set" => handle_set(options, keep_history, rest),
        "/show" => handle_show(client, target, options, rest).await,
        "/save" => handle_save(client, target, options, rest).await,
        "/load" => handle_load(client, target, rest).await,
        other => Err(format!("Unknown command '{other}'. Type /? for help")),
    }
}

/// `/set ...`: mutates the live `ChatOptions` (and the history toggle).
fn handle_set(
    options: &mut chat::ChatOptions,
    keep_history: &mut bool,
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
            let message = set_parameter(options, tokens[0], &tokens[1..])?;
            println!("{message}");
            Ok(())
        }
        "system" => {
            if rest.is_empty() {
                return Err("Usage: /set system <message>".to_string());
            }
            options.system = Some(strip_wrapping_quotes(rest).to_string());
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
            println!("Set 'think' mode.");
            Ok(())
        }
        "nothink" => {
            options.think = Some(serde_json::Value::Bool(false));
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
        other => Err(format!("Unknown /set option '{other}'. Type /set for options")),
    }
}

/// `/set parameter <name> <value...>` — returns the confirmation line.
fn set_parameter(
    options: &mut chat::ChatOptions,
    name: &str,
    values: &[&str],
) -> Result<String, String> {
    if values.is_empty() {
        return Err(format!("Usage: /set parameter {name} <value>"));
    }
    let first = values[0];
    let as_f64 = |v: &str| {
        v.parse::<f64>().map_err(|_| format!("Invalid value '{v}' for '{name}' (expected a number)"))
    };
    let as_i64 = |v: &str| {
        v.parse::<i64>()
            .map_err(|_| format!("Invalid value '{v}' for '{name}' (expected an integer)"))
    };
    match name {
        "temperature" => options.temperature = Some(as_f64(first)?),
        "top_p" => options.top_p = Some(as_f64(first)?),
        "seed" => options.seed = Some(as_i64(first)?),
        "num_ctx" => options.num_ctx = Some(as_i64(first)?),
        "num_predict" => options.num_predict = Some(as_i64(first)?),
        "stop" => {
            options.stop = values.iter().map(|v| strip_wrapping_quotes(v).to_string()).collect()
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
        Target::Local { model, native_ollama: true, .. } => model.clone().unwrap_or_default(),
        Target::Provider { provider_id, model } => {
            if sub == "info" {
                println!("  Target\n    provider    {provider_id}\n    model       {model}");
                return Ok(());
            }
            return Err(format!("'/show {sub}' requires an Ollama target"));
        }
        Target::Local { base_url, model, .. } => {
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
        other => Err(format!("Unknown /show option '{other}'. Type /show for options")),
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
    let Target::Local { model, native_ollama: true, .. } = target else {
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
        parameters: if parameters.is_empty() { None } else { Some(parameters) },
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
    let Target::Local { model, native_ollama: true, .. } = target else {
        return Err("/load requires an Ollama target".to_string());
    };
    let tags = ollama_api::tags(client).await?;
    let want = if name.contains(':') { name.to_string() } else { format!("{name}:latest") };
    if !tags.models.iter().any(|m| m.name == want) {
        return Err(format!("model '{name}' not found — run: lm-cli pull {name}"));
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
  Up / Down        Walk the input history (~/.lm_cli_history)"
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
        assert_eq!(split_first_word("set parameter temperature"), ("set", "parameter temperature"));
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
            set_parameter(&mut options, "temperature", &["0.5"]).unwrap(),
            "Set parameter 'temperature' to '0.5'"
        );
        set_parameter(&mut options, "top_p", &["0.9"]).unwrap();
        set_parameter(&mut options, "seed", &["7"]).unwrap();
        set_parameter(&mut options, "num_ctx", &["8192"]).unwrap();
        set_parameter(&mut options, "num_predict", &["128"]).unwrap();
        set_parameter(&mut options, "stop", &["\"END\"", "STOP"]).unwrap();
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
        assert!(set_parameter(&mut options, "temperature", &[]).is_err());
        assert!(set_parameter(&mut options, "temperature", &["hot"]).is_err());
        assert!(set_parameter(&mut options, "seed", &["1.5"]).is_err());
        assert!(set_parameter(&mut options, "nope", &["1"]).is_err());
        assert_eq!(options.temperature, None);
    }

    #[test]
    fn handle_set_toggles_and_modes() {
        let mut options = chat::ChatOptions::default();
        let mut keep_history = true;

        handle_set(&mut options, &mut keep_history, "verbose").unwrap();
        assert!(options.verbose);
        handle_set(&mut options, &mut keep_history, "quiet").unwrap();
        assert!(!options.verbose);

        handle_set(&mut options, &mut keep_history, "nohistory").unwrap();
        assert!(!keep_history);
        handle_set(&mut options, &mut keep_history, "history").unwrap();
        assert!(keep_history);

        handle_set(&mut options, &mut keep_history, "think low").unwrap();
        assert_eq!(options.think, Some(serde_json::json!("low")));
        handle_set(&mut options, &mut keep_history, "think").unwrap();
        assert_eq!(options.think, Some(serde_json::json!(true)));
        handle_set(&mut options, &mut keep_history, "nothink").unwrap();
        assert_eq!(options.think, Some(serde_json::json!(false)));

        handle_set(&mut options, &mut keep_history, "format json").unwrap();
        assert_eq!(options.format, Some(serde_json::json!("json")));
        handle_set(&mut options, &mut keep_history, "noformat").unwrap();
        assert_eq!(options.format, None);

        handle_set(&mut options, &mut keep_history, "system \"You are terse.\"").unwrap();
        assert_eq!(options.system.as_deref(), Some("You are terse."));

        assert!(handle_set(&mut options, &mut keep_history, "bogus on").is_err());
        assert!(handle_set(&mut options, &mut keep_history, "system").is_err());
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
        assert_eq!(read_prompt_text("\"\"\"", scripted(lines)), Some("kept".to_string()));
    }
}

//! The interactive launcher a bare `monkey` opens: an arrow-key menu, a
//! per-session settings screen (`→` on the first row), a type-to-filter model
//! picker covering both backends, and rows that hand off to whichever other
//! agent CLIs are installed. Whatever it produces is handed straight to
//! `main::run_model`, so a launched session is identical to a typed one.
//!
//! It is a front door for discovery only — every subcommand and flag still
//! works exactly as before (any argument at all skips this entirely), so
//! nothing here is on the path of a scripted invocation.
//!
//! Rendering is deliberately plain crossterm: draw N lines, on the next
//! frame walk the cursor back up N lines and clear downward. No alternate
//! screen, so whatever the user had on their terminal before is still there
//! after, and the block the launcher drew is erased on the way out.
//!
//! Not a TTY (piped, CI) → the caller falls back to printing help; nothing
//! in here ever blocks on a terminal it doesn't have.

use std::io::{stdout, Write};
use std::path::{Path, PathBuf};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Print};
use crossterm::terminal::{self, Clear, ClearType};
use crossterm::{cursor, queue};

use little_monkey_lib::model_sources;
use little_monkey_lib::models::{self, ModelKind};

use crate::ollama_api;

/// Raw mode for the duration of one screen, restored on every exit path
/// (including `?` and panics) — a launcher that leaves the terminal in raw
/// mode makes the user's shell unusable.
struct RawMode;

impl RawMode {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode()
            .map_err(|error| format!("Could not enter raw mode: {error}"))?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// Everything the launcher decided, applied to the parsed `Cli` before the
/// session starts so a launched run goes through the exact same
/// `chat_setup`/`run_model` path a typed one does.
pub struct Launch {
    pub model: String,
    /// True for an app-owned GGUF (installed or about to be installed);
    /// false for a local Ollama tag.
    pub managed: bool,
    settings: Settings,
}

impl Launch {
    /// Writes the settings screen's choices into the flags they mirror.
    pub fn apply(&self, cli: &mut crate::Cli) {
        cli.permission_mode = self.settings.permission_mode.to_string();
        cli.no_rules = !self.settings.rules;
        cli.no_mcp = !self.settings.mcp;
        cli.chat.verify = self.settings.verify;
        cli.chat.subagents = self.settings.subagents;
    }
}

/// The session knobs the `→ configure` screen exposes — the subset of the
/// CLI's flags that change how a chat session behaves rather than which
/// model it talks to. Defaults match the flags' own defaults exactly.
#[derive(Clone)]
struct Settings {
    permission_mode: &'static str,
    rules: bool,
    mcp: bool,
    verify: bool,
    subagents: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            permission_mode: "manual",
            rules: true,
            mcp: true,
            verify: false,
            subagents: false,
        }
    }
}

const PERMISSION_MODES: [&str; 6] = ["manual", "smart", "acceptEdits", "plan", "auto", "bypass"];

/// What one keypress did to the screen we're on.
enum Nav {
    Chosen(usize),
    Configure,
    Back,
    Quit,
}

/// Draws `lines`, having erased the previous frame's `drawn` lines. Returns
/// the new line count to pass back in next frame.
fn draw(lines: &[String], drawn: u16) -> Result<u16, String> {
    let mut out = stdout();
    if drawn > 0 {
        queue!(out, cursor::MoveToPreviousLine(drawn))
            .map_err(|error| format!("Terminal write failed: {error}"))?;
    }
    queue!(out, Clear(ClearType::FromCursorDown))
        .map_err(|error| format!("Terminal write failed: {error}"))?;
    for line in lines {
        queue!(out, Print(line), Print("\r\n"))
            .map_err(|error| format!("Terminal write failed: {error}"))?;
    }
    out.flush()
        .map_err(|error| format!("Terminal write failed: {error}"))?;
    Ok(lines.len() as u16)
}

/// Erases the frame still on screen — called once as each screen exits, so
/// the launcher leaves no residue above the session banner.
fn erase(drawn: u16) {
    let mut out = stdout();
    if drawn > 0 {
        let _ = queue!(out, cursor::MoveToPreviousLine(drawn));
    }
    let _ = queue!(out, Clear(ClearType::FromCursorDown));
    let _ = out.flush();
}

fn bold(text: &str) -> String {
    format!("{}{text}{}", Attribute::Bold, Attribute::Reset)
}

fn dim(text: &str) -> String {
    format!("{}{text}{}", Attribute::Dim, Attribute::Reset)
}

/// One key read, mapped to a navigation intent. `Char` keys that aren't
/// controls are handed back to the caller as `Some(char)` for the picker's
/// filter; other screens ignore them.
enum Key {
    Up,
    Down,
    Enter,
    Right,
    Back,
    Quit,
    Backspace,
    Char(char),
    Ignored,
}

fn read_key() -> Result<Key, String> {
    loop {
        let Event::Key(key) = event::read().map_err(|error| format!("Key read failed: {error}"))?
        else {
            continue;
        };
        // Windows terminals deliver Release/Repeat too; acting on all of
        // them double-steps every arrow press.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(match key.code {
                KeyCode::Char('c') | KeyCode::Char('d') => Key::Quit,
                // Raw mode reports the control bytes Return can arrive as
                // (CR = Ctrl-M, LF = Ctrl-J, which is what a terminal that
                // had ICRNL set before we grabbed it sends) as control
                // chars, not KeyCode::Enter.
                KeyCode::Char('j') | KeyCode::Char('m') => Key::Enter,
                _ => Key::Ignored,
            });
        }
        return Ok(match key.code {
            KeyCode::Up => Key::Up,
            KeyCode::Down => Key::Down,
            KeyCode::Enter | KeyCode::Char('\n') | KeyCode::Char('\r') => Key::Enter,
            KeyCode::Right => Key::Right,
            KeyCode::Left => Key::Back,
            KeyCode::Esc => Key::Quit,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Char(c) => Key::Char(c),
            _ => Key::Ignored,
        });
    }
}

/// How many list rows fit under a fixed header/footer, so a long model list
/// scrolls inside the window instead of scrolling the frame off the top.
fn window_rows(reserved: u16) -> usize {
    let height = terminal::size().map(|(_, h)| h).unwrap_or(24);
    height.saturating_sub(reserved).max(3) as usize
}

/// Shifts the visible window so `selected` stays inside it.
fn window_start(selected: usize, len: usize, rows: usize) -> usize {
    if len <= rows {
        return 0;
    }
    selected
        .saturating_sub(rows / 2)
        .min(len.saturating_sub(rows))
}

/// Another agent CLI the user may already have installed. Selecting an
/// installed one hands the terminal over to it (same folder, same terminal);
/// selecting a missing one prints where to get it and exits — the launcher
/// never runs an installer on the user's behalf.
struct Agent {
    label: &'static str,
    binary: &'static str,
    description: &'static str,
    install: &'static str,
}

const AGENTS: [Agent; 4] = [
    Agent {
        label: "Claude Code",
        binary: "claude",
        description: "Anthropic's coding tool with subagents",
        install: "npm install -g @anthropic-ai/claude-code",
    },
    Agent {
        label: "Codex",
        binary: "codex",
        description: "OpenAI's coding agent",
        install: "npm install -g @openai/codex",
    },
    Agent {
        label: "Gemini CLI",
        binary: "gemini",
        description: "Google's coding agent",
        install: "npm install -g @google/gemini-cli",
    },
    Agent {
        label: "OpenCode",
        binary: "opencode",
        description: "Open-source coding agent",
        install: "see https://opencode.ai for install instructions",
    },
];

/// PATH lookup without a `which` crate: the extension candidates make it
/// work for Windows shims (`claude.cmd`) as well as unix binaries.
fn on_path(binary: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in [
            binary.to_string(),
            format!("{binary}.exe"),
            format!("{binary}.cmd"),
        ] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

const OWN_ROWS: usize = 3;

/// The first screen: our own three entries, then one row per other agent CLI.
fn menu(settings: &Settings, agents: &[Option<PathBuf>]) -> Result<Nav, String> {
    let _raw = RawMode::enter()?;
    let own: [(String, String); OWN_ROWS] = [
        (
            "Chat, Code, & Work".to_string(),
            "Chat with a model, run tools, and edit code in this folder".to_string(),
        ),
        (
            "Models".to_string(),
            "What's installed locally, and what's currently loaded".to_string(),
        ),
        (
            "Commands & options".to_string(),
            "Every subcommand and flag (same as monkey --help)".to_string(),
        ),
    ];
    let rows: Vec<(String, String)> = own
        .into_iter()
        .chain(AGENTS.iter().zip(agents).map(|(agent, found)| {
            let suffix = if found.is_some() { "" } else { " (install)" };
            (
                format!("Launch {}{suffix}", agent.label),
                agent.description.to_string(),
            )
        }))
        .collect();

    let mut selected = 0usize;
    let mut drawn = 0u16;
    loop {
        let mut lines = vec![
            bold(&format!("Little Monkey {}", env!("CARGO_PKG_VERSION"))),
            String::new(),
        ];
        for (index, (title, description)) in rows.iter().enumerate() {
            lines.push(if index == selected {
                format!("▸ {}", bold(title))
            } else {
                format!("  {title}")
            });
            lines.push(dim(&format!("    {description}")));
        }
        lines.push(String::new());
        lines.push(dim(&format!(
            "{} · {} rules · {} MCP",
            settings.permission_mode,
            if settings.rules { "with" } else { "no" },
            if settings.mcp { "with" } else { "no" },
        )));
        lines.push(dim("↑/↓ navigate • enter launch • → configure • esc quit"));
        drawn = draw(&lines, drawn)?;

        match read_key()? {
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => selected = (selected + 1).min(rows.len() - 1),
            Key::Enter => {
                erase(drawn);
                return Ok(Nav::Chosen(selected));
            }
            Key::Right => {
                erase(drawn);
                return Ok(Nav::Configure);
            }
            Key::Quit => {
                erase(drawn);
                return Ok(Nav::Quit);
            }
            _ => {}
        }
    }
}

/// The `→ configure` screen: session flags, edited in place. Enter/space/→
/// cycles the highlighted row; ←/esc returns to the menu with whatever is
/// set (there is nothing to cancel — the menu shows the live values).
fn configure(settings: &mut Settings) -> Result<(), String> {
    let _raw = RawMode::enter()?;
    let mut selected = 0usize;
    let mut drawn = 0u16;
    loop {
        let rows = [
            ("Permission mode", settings.permission_mode.to_string()),
            ("Project rules (MONKEY.md) & facts", on_off(settings.rules)),
            ("MCP servers", on_off(settings.mcp)),
            ("Verify after edits", on_off(settings.verify)),
            ("Subagents (task tool)", on_off(settings.subagents)),
        ];
        let mut lines = vec![bold("Session settings"), String::new()];
        for (index, (label, value)) in rows.iter().enumerate() {
            let row = format!("{label:<34}{value}");
            lines.push(if index == selected {
                format!("▸ {}", bold(&row))
            } else {
                format!("  {row}")
            });
        }
        lines.push(String::new());
        lines.push(dim("↑/↓ navigate • enter/→ change • ← back • esc back"));
        drawn = draw(&lines, drawn)?;

        match read_key()? {
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => selected = (selected + 1).min(rows.len() - 1),
            Key::Enter | Key::Right | Key::Char(' ') => match selected {
                0 => {
                    let next = PERMISSION_MODES
                        .iter()
                        .position(|mode| *mode == settings.permission_mode)
                        .map(|index| (index + 1) % PERMISSION_MODES.len())
                        .unwrap_or(0);
                    settings.permission_mode = PERMISSION_MODES[next];
                }
                1 => settings.rules = !settings.rules,
                2 => settings.mcp = !settings.mcp,
                3 => settings.verify = !settings.verify,
                _ => settings.subagents = !settings.subagents,
            },
            Key::Back | Key::Quit => {
                erase(drawn);
                return Ok(());
            }
            _ => {}
        }
    }
}

fn on_off(value: bool) -> String {
    if value { "on" } else { "off" }.to_string()
}

/// One runnable model as the picker sees it, whichever backend it comes from.
struct ModelChoice {
    section: &'static str,
    label: String,
    detail: String,
    /// What `run_model` is handed: an Ollama tag, or a GGUF reference.
    reference: String,
    managed: bool,
}

/// The model picker. Filtering is a plain case-insensitive substring match
/// on the model name — no fuzzy ranking, which for a list this size is
/// indistinguishable from one and a lot less code.
fn pick_model(choices: &[ModelChoice]) -> Result<Nav, String> {
    let _raw = RawMode::enter()?;
    let mut filter = String::new();
    let mut selected = 0usize;
    let mut drawn = 0u16;
    loop {
        let matches: Vec<usize> = choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.label.to_lowercase().contains(&filter.to_lowercase()))
            .map(|(index, _)| index)
            .collect();
        selected = selected.min(matches.len().saturating_sub(1));

        let mut lines = vec![
            format!(
                "{} {}",
                bold("Select model to run:"),
                if filter.is_empty() {
                    dim("Type to filter...")
                } else {
                    filter.clone()
                }
            ),
            String::new(),
        ];
        if matches.is_empty() {
            lines.push(dim("  No model matches that filter."));
        } else {
            let rows = window_rows(7);
            let start = window_start(selected, matches.len(), rows);
            let mut section = "";
            for (offset, choice_index) in matches.iter().skip(start).take(rows).enumerate() {
                let choice = &choices[*choice_index];
                if choice.section != section {
                    section = choice.section;
                    lines.push(dim(&format!("  {section}")));
                }
                let index = start + offset;
                let detail = if choice.detail.is_empty() {
                    String::new()
                } else {
                    format!(" {}", dim(&choice.detail))
                };
                lines.push(if index == selected {
                    format!("▸ {}{detail}", bold(&choice.label))
                } else {
                    format!("  {}{detail}", choice.label)
                });
            }
            if matches.len() > rows {
                lines.push(dim(&format!(
                    "  ({rows} of {} shown — type to filter)",
                    matches.len()
                )));
            }
        }
        lines.push(String::new());
        lines.push(dim(
            "↑/↓ navigate • type to filter • enter select • ← back • esc quit",
        ));
        drawn = draw(&lines, drawn)?;

        match read_key()? {
            Key::Up => selected = selected.saturating_sub(1),
            Key::Down => selected = (selected + 1).min(matches.len().saturating_sub(1)),
            Key::Enter => {
                if let Some(index) = matches.get(selected) {
                    erase(drawn);
                    return Ok(Nav::Chosen(*index));
                }
            }
            Key::Back => {
                erase(drawn);
                return Ok(Nav::Back);
            }
            Key::Quit => {
                erase(drawn);
                return Ok(Nav::Quit);
            }
            Key::Backspace => {
                filter.pop();
            }
            Key::Char(c) => filter.push(c),
            _ => {}
        }
    }
}

/// Hands the terminal to another agent CLI in the current folder and exits
/// with its status — the launcher is done once it has chosen who runs.
fn launch_agent(agent: &Agent, found: Option<&PathBuf>) -> ! {
    let Some(path) = found else {
        println!(
            "{} is not installed.\n  Install it with: {}",
            agent.label, agent.install
        );
        std::process::exit(0);
    };
    match std::process::Command::new(path).status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(0)),
        Err(error) => {
            eprintln!("Error: could not start {}: {error}", agent.label);
            std::process::exit(1);
        }
    }
}

/// Drives menu → (settings) → picker and returns what to run, or `None` when
/// the user quit or picked a screen that just prints. Screens that hand the
/// terminal to something else (another agent CLI) exit the process directly.
pub async fn run(client: &reqwest::Client) -> Result<Option<Launch>, String> {
    let agents: Vec<Option<PathBuf>> = AGENTS.iter().map(|agent| on_path(agent.binary)).collect();
    let mut settings = Settings::default();
    loop {
        match menu(&settings, &agents)? {
            Nav::Quit | Nav::Back => return Ok(None),
            Nav::Configure => configure(&mut settings)?,
            Nav::Chosen(1) => {
                crate::cmds::list(client).await?;
                return Ok(None);
            }
            Nav::Chosen(2) => {
                use clap::CommandFactory;
                let _ = crate::Cli::command().print_help();
                println!();
                return Ok(None);
            }
            Nav::Chosen(index) if index >= OWN_ROWS => {
                let agent = &AGENTS[index - OWN_ROWS];
                launch_agent(agent, agents[index - OWN_ROWS].as_ref());
            }
            Nav::Chosen(_) => {
                let choices = model_choices(client).await?;
                match pick_model(&choices)? {
                    Nav::Chosen(index) => {
                        return Ok(Some(Launch {
                            model: choices[index].reference.clone(),
                            managed: choices[index].managed,
                            settings: settings.clone(),
                        }))
                    }
                    Nav::Back | Nav::Configure => continue,
                    Nav::Quit => return Ok(None),
                }
            }
        }
    }
}

/// Every model the picker can start: the curated recommendations (installed
/// or downloadable on first run), whatever GGUFs are already in the
/// app-owned model store, and whatever the local Ollama daemon has pulled.
/// A daemon that isn't running is not an error — it just contributes no
/// rows, same as an empty model store.
async fn model_choices(client: &reqwest::Client) -> Result<Vec<ModelChoice>, String> {
    let installed = installed_managed_models();
    let mut choices = Vec::new();

    for model in models::curated_models() {
        if model.kind != ModelKind::Chat {
            continue;
        }
        let reference = format!("hf:{}#{}", model.repo, model.file);
        let have = installed
            .iter()
            .any(|provenance| provenance.source_file_name == model.file);
        choices.push(ModelChoice {
            section: "Recommended",
            label: model.name.clone(),
            detail: if have {
                format!("~{:.1} GB, installed", model.size_gb)
            } else {
                format!("~{:.1} GB, downloads on first run", model.size_gb)
            },
            reference,
            managed: true,
        });
    }

    for provenance in &installed {
        // Curated entries already cover their own installed copies.
        if models::curated_models()
            .iter()
            .any(|model| model.file == provenance.source_file_name)
        {
            continue;
        }
        choices.push(ModelChoice {
            section: "Installed (app-owned)",
            label: provenance.display_name.clone(),
            detail: "GGUF".to_string(),
            reference: if provenance.requested_reference.is_empty() {
                provenance.canonical_reference.clone()
            } else {
                provenance.requested_reference.clone()
            },
            managed: true,
        });
    }

    if let Ok(tags) = ollama_api::tags(client).await {
        for entry in tags.models {
            choices.push(ModelChoice {
                section: "Installed (Ollama)",
                label: entry.name.clone(),
                detail: String::new(),
                reference: entry.name,
                managed: false,
            });
        }
    }

    if choices.is_empty() {
        return Err("No models are available. Pull one with `monkey pull qwen3:4b`.".to_string());
    }
    Ok(choices)
}

/// Reads the provenance sidecar of every GGUF in the app-owned model store.
/// A model without a valid sidecar is deliberately skipped rather than
/// guessed at — `run_model` would refuse to start it anyway.
fn installed_managed_models() -> Vec<model_sources::ManagedModelProvenance> {
    let Some(dir) = little_monkey_lib::app_paths::data_dir().map(|data| data.join("models")) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_gguf(path))
        .filter_map(|path| model_sources::load_provenance(&path).ok().flatten())
        .collect()
}

fn is_gguf(path: &Path) -> bool {
    path.is_file()
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_keeps_the_selection_visible_at_both_ends() {
        // Short list: never scrolls.
        assert_eq!(window_start(0, 5, 10), 0);
        assert_eq!(window_start(4, 5, 10), 0);
        // Long list: first item pins to the top, last item pins the window
        // to the end (never past it, which would blank rows).
        assert_eq!(window_start(0, 100, 10), 0);
        assert_eq!(window_start(50, 100, 10), 45);
        assert_eq!(window_start(99, 100, 10), 90);
    }

    #[test]
    fn a_short_terminal_still_leaves_rows_for_the_list() {
        assert!(window_rows(6) >= 3);
    }

    #[test]
    fn cycling_permission_modes_wraps_and_covers_every_flag_value() {
        let mut settings = Settings::default();
        let mut seen = Vec::new();
        for _ in 0..PERMISSION_MODES.len() {
            seen.push(settings.permission_mode);
            let next = PERMISSION_MODES
                .iter()
                .position(|mode| *mode == settings.permission_mode)
                .map(|index| (index + 1) % PERMISSION_MODES.len())
                .unwrap();
            settings.permission_mode = PERMISSION_MODES[next];
        }
        assert_eq!(settings.permission_mode, "manual", "cycle must wrap");
        // Every mode offered here must be one `--permission-mode` accepts.
        for mode in seen {
            assert!(
                crate::permission::PermissionMode::parse(mode).is_ok(),
                "{mode}"
            );
        }
    }

    #[test]
    fn settings_land_on_the_flags_they_mirror() {
        let launch = Launch {
            model: "qwen3:4b".to_string(),
            managed: false,
            settings: Settings {
                permission_mode: "bypass",
                rules: false,
                mcp: false,
                verify: true,
                subagents: true,
            },
        };
        use clap::Parser;
        let mut cli = crate::Cli::try_parse_from(["monkey"]).unwrap();
        launch.apply(&mut cli);
        assert_eq!(cli.permission_mode, "bypass");
        assert!(cli.no_rules);
        assert!(cli.no_mcp);
        assert!(cli.chat.verify);
        assert!(cli.chat.subagents);
    }
}

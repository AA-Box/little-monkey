//! In-app cron scheduler backend (design doc:
//! docs/roadmap/p3-scheduled-automation.md, slice 3) — two independent
//! pieces:
//!
//! 1. Opaque `automations.json` blob persistence, copying `sessions.rs`'s
//!    exact pattern (atomic temp+rename write, a `*_changed` event for other
//!    windows to rehydrate on): the frontend's `automationsStore.ts` owns the
//!    `AutomationEntry[]` schema entirely, this side never parses it.
//! 2. `cron_validate`/`cron_next` — thin wrappers around the `croner` crate
//!    so cron parsing/next-occurrence math stays Rust-side (no separate
//!    frontend cron dependency, per the design doc).

use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::Utc;
use croner::Cron;
use tauri::{Emitter};
use crate::profiles::ProfileScopedPaths;

const AUTOMATIONS_FILE: &str = "automations.json";

/// Emitted after a successful [`automations_save`], with the saving window's
/// label as payload — same cross-window sync mechanism as
/// `sessions.rs::SESSIONS_CHANGED_EVENT`.
pub const AUTOMATIONS_CHANGED_EVENT: &str = "automations://changed";

fn automations_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {e}"))?;
    Ok(dir.join(AUTOMATIONS_FILE))
}

/// Core load logic, parameterized by path for testability — mirrors
/// `sessions.rs::load_from` exactly.
fn load_from(path: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Failed to read automations file: {e}")),
    }
}

/// Core save logic: temp file + rename, same atomicity guarantee as
/// `sessions.rs::save_to`.
fn save_to(path: &Path, payload: &str) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write automations file: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize automations file: {e}"))?;
    Ok(())
}

/// The persisted automations blob as a raw JSON string, or `None` if nothing
/// has been saved yet.
#[tauri::command]
pub fn automations_load(app: tauri::AppHandle) -> Result<Option<String>, String> {
    load_from(&automations_file_path(&app)?)
}

/// Persists the automations blob (opaque JSON string owned by the frontend's
/// `automationsStore.ts`).
#[tauri::command]
pub fn automations_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    payload: String,
) -> Result<(), String> {
    save_to(&automations_file_path(&app)?, &payload)?;
    let _ = app.emit(AUTOMATIONS_CHANGED_EVENT, window.label());
    Ok(())
}

/// Parses `expr` and returns its human-readable description (e.g. "At 03:00
/// AM, only on Monday") — the Tasks panel's live cron-field feedback, so a
/// typo'd expression is caught before it's ever saved, and a valid one shows
/// what it actually means (croner's POSIX weekday numbering can otherwise
/// silently mean a different day than a user expects coming from Quartz-style
/// cron).
pub fn validate_cron_impl(expr: &str) -> Result<String, String> {
    Cron::from_str(expr)
        .map(|cron| cron.describe())
        .map_err(|e| format!("Invalid cron expression: {e}"))
}

#[tauri::command]
pub fn cron_validate(expr: String) -> Result<String, String> {
    validate_cron_impl(&expr)
}

/// Computes the next `n` occurrences of `expr`, starting strictly after now,
/// as epoch-millisecond timestamps — what `scheduler.ts`'s due-check and the
/// Tasks panel's "next run" preview both need.
pub fn next_occurrences_impl(expr: &str, n: u32) -> Result<Vec<i64>, String> {
    let cron = Cron::from_str(expr).map_err(|e| format!("Invalid cron expression: {e}"))?;
    let mut cursor = Utc::now();
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let next = cron
            .find_next_occurrence(&cursor, false)
            .map_err(|e| format!("Failed to compute next occurrence: {e}"))?;
        out.push(next.timestamp_millis());
        cursor = next;
    }
    Ok(out)
}

#[tauri::command]
pub fn cron_next(expr: String, n: u32) -> Result<Vec<i64>, String> {
    next_occurrences_impl(&expr, n)
}

/// The most recent occurrence of `expr` at-or-before now, as an epoch-ms
/// timestamp — what `scheduler.ts`'s 30s tick uses to detect "a scheduled
/// time fell inside the last tick interval": comparing this against the
/// entry's own last-checked timestamp (kept client-side) tells the tick loop
/// whether an occurrence was just crossed, without needing the scheduler to
/// track cron math itself.
pub fn previous_occurrence_impl(expr: &str) -> Result<i64, String> {
    let cron = Cron::from_str(expr).map_err(|e| format!("Invalid cron expression: {e}"))?;
    let now = Utc::now();
    cron.find_previous_occurrence(&now, true)
        .map(|dt| dt.timestamp_millis())
        .map_err(|e| format!("Failed to compute previous occurrence: {e}"))
}

#[tauri::command]
pub fn cron_previous(expr: String) -> Result<i64, String> {
    previous_occurrence_impl(&expr)
}

// ---------------------------------------------------------------------------
// OS-level scheduling export (design doc slice 4, optional) — `monkey-cli
// task schedule <recipe> --cron '...' --print` emits text to install rather
// than the app daemonizing itself (a documented non-goal — see ROADMAP.md
// §4: "App self-daemonizing for schedules").
// ---------------------------------------------------------------------------

/// A single fixed numeric value per cron field (covers the overwhelming
/// majority of real schedules — "M H * * *" daily, "M H * * D" weekly, "M H
/// D * *" monthly) mapped onto launchd's `StartCalendarInterval` keys.
/// launchd has no direct equivalent for ranges/lists/steps (`1-5`, `*/15`,
/// `1,15`) — those return `None` here so the caller falls back to a plain
/// crontab line instead, which IS cron syntax and needs no conversion.
fn simple_calendar_fields(expr: &str) -> Option<Vec<(&'static str, u32)>> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return None;
    }
    let keys = ["Minute", "Hour", "Day", "Month", "Weekday"];
    let mut fields = Vec::new();
    for (key, raw) in keys.iter().zip(parts.iter()) {
        if *raw == "*" {
            continue;
        }
        fields.push((*key, raw.parse::<u32>().ok()?));
    }
    Some(fields)
}

/// Formats a ready-to-install launchd plist for `expr`, or `None` when
/// `expr` uses cron syntax launchd can't express directly (see
/// [`simple_calendar_fields`]).
pub fn format_launchd_plist(
    label: &str,
    program: &str,
    args: &[String],
    expr: &str,
) -> Option<String> {
    let fields = simple_calendar_fields(expr)?;
    let calendar_entries: String = fields
        .iter()
        .map(|(key, value)| {
            format!("        <key>{key}</key>\n        <integer>{value}</integer>\n")
        })
        .collect();
    let arg_entries: String = args
        .iter()
        .map(|a| format!("        <string>{a}</string>\n"))
        .collect();
    Some(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{label}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>\n\
        <string>{program}</string>\n\
{arg_entries}\
    </array>\n\
    <key>StartCalendarInterval</key>\n\
    <dict>\n\
{calendar_entries}\
    </dict>\n\
    <key>StandardOutPath</key>\n\
    <string>/tmp/{label}.log</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>/tmp/{label}.log</string>\n\
</dict>\n\
</plist>\n"
    ))
}

/// Formats a crontab line for `expr` — works for any valid cron expression,
/// no conversion needed, unlike launchd's calendar-interval format.
pub fn format_crontab_line(expr: &str, program: &str, args: &[String]) -> String {
    let quoted_args: Vec<String> = args.iter().map(|a| format!("'{a}'")).collect();
    format!("{expr} '{program}' {}", quoted_args.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_automations_test_{}_{n}_{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let path = temp_file();
        assert_eq!(load_from(&path).unwrap(), None);
    }

    #[test]
    fn save_then_load_roundtrips() {
        let path = temp_file();
        let payload = r#"{"entries":[]}"#;
        save_to(&path, payload).unwrap();
        assert_eq!(load_from(&path).unwrap().as_deref(), Some(payload));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_overwrites_previous_content_atomically_and_leaves_no_temp_file() {
        let path = temp_file();
        save_to(&path, "first").unwrap();
        save_to(&path, "second").unwrap();
        assert_eq!(load_from(&path).unwrap().as_deref(), Some("second"));
        assert!(!path.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn validate_cron_impl_accepts_a_standard_five_field_expression() {
        let description = validate_cron_impl("0 3 * * *").unwrap();
        assert!(!description.is_empty());
    }

    #[test]
    fn validate_cron_impl_rejects_garbage() {
        assert!(validate_cron_impl("not a cron expression").is_err());
    }

    #[test]
    fn validate_cron_impl_rejects_an_out_of_range_field() {
        assert!(validate_cron_impl("0 99 * * *").is_err());
    }

    #[test]
    fn next_occurrences_impl_returns_the_requested_count_in_strictly_increasing_order() {
        let occurrences = next_occurrences_impl("0 3 * * *", 3).unwrap();
        assert_eq!(occurrences.len(), 3);
        assert!(occurrences[0] < occurrences[1]);
        assert!(occurrences[1] < occurrences[2]);
    }

    #[test]
    fn next_occurrences_impl_returns_only_future_timestamps() {
        let now_ms = Utc::now().timestamp_millis();
        let occurrences = next_occurrences_impl("* * * * *", 1).unwrap();
        assert!(occurrences[0] > now_ms);
    }

    #[test]
    fn next_occurrences_impl_rejects_garbage() {
        assert!(next_occurrences_impl("garbage", 1).is_err());
    }

    #[test]
    fn next_occurrences_impl_with_zero_count_returns_an_empty_vec() {
        assert_eq!(
            next_occurrences_impl("0 3 * * *", 0).unwrap(),
            Vec::<i64>::new()
        );
    }

    #[test]
    fn previous_occurrence_impl_returns_a_timestamp_at_or_before_now() {
        let previous = previous_occurrence_impl("* * * * *").unwrap();
        // Captured AFTER the call (not before): the function's internal
        // `Utc::now()` is guaranteed to have run before this bound, so a
        // minute boundary crossing between the two calls can never make
        // this assertion flaky — capturing the bound first (before calling)
        // can, if the boundary falls between the two `Utc::now()` reads.
        let after_ms = Utc::now().timestamp_millis();
        assert!(previous <= after_ms);
    }

    #[test]
    fn previous_occurrence_impl_rejects_garbage() {
        assert!(previous_occurrence_impl("garbage").is_err());
    }

    #[test]
    fn format_launchd_plist_converts_a_daily_schedule() {
        let plist = format_launchd_plist(
            "com.littlemonkey.task.nightly-audit",
            "/usr/local/bin/monkey-cli",
            &[
                "task".to_string(),
                "run".to_string(),
                "/ws/recipe.yml".to_string(),
            ],
            "0 3 * * *",
        )
        .expect("a fixed-value daily schedule must convert");

        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>com.littlemonkey.task.nightly-audit</string>"));
        assert!(plist.contains("<key>Minute</key>\n        <integer>0</integer>"));
        assert!(plist.contains("<key>Hour</key>\n        <integer>3</integer>"));
        // `*` fields (day/month/weekday here) must be omitted, not emitted
        // as some sentinel value — an absent key is launchd's own "every".
        assert!(!plist.contains("<key>Day</key>"));
        assert!(!plist.contains("<key>Month</key>"));
        assert!(!plist.contains("<key>Weekday</key>"));
        assert!(plist.contains("<string>/ws/recipe.yml</string>"));
    }

    #[test]
    fn format_launchd_plist_converts_a_weekly_schedule() {
        let plist =
            format_launchd_plist("label", "monkey-cli", &["task".to_string()], "30 9 * * 1")
                .unwrap();
        assert!(plist.contains("<key>Weekday</key>\n        <integer>1</integer>"));
        assert!(!plist.contains("<key>Day</key>"));
    }

    #[test]
    fn format_launchd_plist_returns_none_for_a_range_or_step_expression() {
        assert!(format_launchd_plist("l", "p", &[], "*/15 * * * *").is_none());
        assert!(format_launchd_plist("l", "p", &[], "0 9-17 * * *").is_none());
        assert!(format_launchd_plist("l", "p", &[], "0 3 * * 1,3,5").is_none());
    }

    #[test]
    fn format_crontab_line_includes_the_expression_and_quoted_args() {
        let line = format_crontab_line(
            "0 3 * * *",
            "/usr/local/bin/monkey-cli",
            &[
                "task".to_string(),
                "run".to_string(),
                "/ws/r.yml".to_string(),
            ],
        );
        assert_eq!(
            line,
            "0 3 * * * '/usr/local/bin/monkey-cli' 'task' 'run' '/ws/r.yml'"
        );
    }

    #[test]
    fn format_crontab_line_works_for_expressions_launchd_cannot_express() {
        // No conversion needed — crontab syntax IS cron syntax.
        let line = format_crontab_line("*/15 9-17 * * 1-5", "monkey-cli", &["task".to_string()]);
        assert!(line.starts_with("*/15 9-17 * * 1-5 "));
    }
}

//! Implementations of monkey-cli's Ollama-parity subcommands (list/ps/pull/rm/
//! cp/show/stop/push/create): terminal rendering — aligned tables, rewriting
//! progress lines — on top of the `ollama_api` client, plus passthroughs to
//! the `ollama` binary for the account/daemon commands the HTTP API doesn't
//! cover (signin/signout/serve). Everything honors `OLLAMA_HOST` via
//! `ollama_api::host()`.

use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::modelfile;
use crate::ollama_api::{self, ProgressLine, ShowResp};

pub async fn list(client: &reqwest::Client) -> Result<(), String> {
    let tags = ollama_api::tags(client).await?;
    let rows: Vec<Vec<String>> = tags
        .models
        .iter()
        .map(|m| {
            vec![
                m.name.clone(),
                short_digest(&m.digest),
                human_bytes(m.size),
                time_ago(&m.modified_at),
            ]
        })
        .collect();
    print_table(&["NAME", "ID", "SIZE", "MODIFIED"], &rows);
    Ok(())
}

pub async fn ps(client: &reqwest::Client) -> Result<(), String> {
    let ps = ollama_api::ps(client).await?;
    let rows: Vec<Vec<String>> = ps
        .models
        .iter()
        .map(|m| {
            vec![
                m.name.clone(),
                short_digest(&m.digest),
                human_bytes(m.size),
                processor(m.size, m.size_vram),
                time_until(&m.expires_at),
            ]
        })
        .collect();
    print_table(&["NAME", "ID", "SIZE", "PROCESSOR", "UNTIL"], &rows);
    Ok(())
}

pub async fn pull(client: &reqwest::Client, model: &str, insecure: bool) -> Result<(), String> {
    let mut progress = ProgressRenderer::default();
    let result = ollama_api::pull(client, model, insecure, |line| progress.update(line)).await;
    progress.finish();
    result
}

pub async fn push(client: &reqwest::Client, model: &str, insecure: bool) -> Result<(), String> {
    let mut progress = ProgressRenderer::default();
    let result = ollama_api::push(client, model, insecure, |line| progress.update(line)).await;
    progress.finish();
    result
}

pub async fn rm(client: &reqwest::Client, models: &[String]) -> Result<(), String> {
    for model in models {
        ollama_api::delete(client, model).await?;
        println!("deleted '{model}'");
    }
    Ok(())
}

pub async fn cp(client: &reqwest::Client, source: &str, destination: &str) -> Result<(), String> {
    ollama_api::copy(client, source, destination).await?;
    println!("copied '{source}' to '{destination}'");
    Ok(())
}

pub async fn stop(client: &reqwest::Client, model: &str) -> Result<(), String> {
    ollama_api::unload(client, model).await?;
    println!("stopped '{model}'");
    Ok(())
}

/// `show`: with one section flag set, prints that section raw; with none,
/// prints an `ollama show`-style summary block.
pub async fn show(
    client: &reqwest::Client,
    model: &str,
    modelfile: bool,
    parameters: bool,
    template: bool,
    system: bool,
    license: bool,
) -> Result<(), String> {
    if [modelfile, parameters, template, system, license]
        .iter()
        .filter(|f| **f)
        .count()
        > 1
    {
        return Err(
            "only one of --modelfile, --parameters, --template, --system, or --license can be specified"
                .to_string(),
        );
    }
    let resp = ollama_api::show(client, model).await?;
    let section = if modelfile {
        Some(&resp.modelfile)
    } else if parameters {
        Some(&resp.parameters)
    } else if template {
        Some(&resp.template)
    } else if system {
        Some(&resp.system)
    } else if license {
        Some(&resp.license)
    } else {
        None
    };
    match section {
        Some(text) => println!("{}", text.trim_end()),
        None => print_show_summary(&resp),
    }
    Ok(())
}

pub async fn create(
    client: &reqwest::Client,
    model: &str,
    file: &Path,
    quantize: Option<String>,
) -> Result<(), String> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| format!("Failed to read Modelfile '{}': {e}", file.display()))?;
    let parsed = modelfile::parse(&text)?;
    let req = modelfile::to_create_request(parsed, model, quantize)?;
    let mut progress = ProgressRenderer::default();
    let result = ollama_api::create(client, &req, |line| progress.update(line)).await;
    progress.finish();
    result
}

/// Runs `ollama <subcommand>` with inherited stdio, for the account/daemon
/// commands (signin/signout/serve) that have no daemon HTTP equivalent. A
/// non-zero child exit code becomes our own exit code.
pub fn passthrough(subcommand: &str) -> Result<(), String> {
    let status = std::process::Command::new("ollama")
        .arg(subcommand)
        .status()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "`monkey {subcommand}` requires the ollama binary, which was not found on PATH"
                )
            } else {
                format!("Failed to run `ollama {subcommand}`: {e}")
            }
        })?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Makes sure `model` exists locally before `run` chats with it, pulling it
/// (with progress) when missing — mirrors `ollama run`'s auto-pull. A bare
/// name matches its `:latest` tag, like Ollama.
pub async fn ensure_model(client: &reqwest::Client, model: &str) -> Result<(), String> {
    let tags = ollama_api::tags(client).await?;
    let want = if model.contains(':') {
        model.to_string()
    } else {
        format!("{model}:latest")
    };
    if tags.models.iter().any(|m| m.name == want) {
        return Ok(());
    }
    pull(client, model, false).await
}

/// Renders pull/push/create NDJSON progress on stderr: status-only lines
/// print once, layer lines with byte counts rewrite a single line in place,
/// with a newline whenever the status changes.
#[derive(Default)]
struct ProgressRenderer {
    status: Option<String>,
    line_dirty: bool,
}

impl ProgressRenderer {
    fn update(&mut self, line: ProgressLine) {
        let status = line.status.unwrap_or_default();
        let changed = self.status.as_deref() != Some(status.as_str());
        if changed {
            self.finish();
            self.status = Some(status.clone());
        }
        if let Some(total) = line.total.filter(|t| *t > 0) {
            let completed = line.completed.unwrap_or(0).min(total);
            let percent = completed * 100 / total;
            eprint!(
                "\r{status} {percent:>3}% {}/{}\x1b[K",
                human_bytes(completed),
                human_bytes(total)
            );
            std::io::stderr().flush().ok();
            self.line_dirty = true;
        } else if changed && !status.is_empty() {
            eprintln!("{status}");
        }
    }

    /// Terminates a still-rewriting line so later output starts on its own.
    fn finish(&mut self) {
        if self.line_dirty {
            eprintln!();
            self.line_dirty = false;
        }
    }
}

/// Prints an `ollama show`-style summary: Model details, Capabilities,
/// Parameters, System, and the first lines of the License. Sections a model
/// doesn't have are skipped.
fn print_show_summary(resp: &ShowResp) {
    let mut model_rows: Vec<(&str, String)> = Vec::new();
    if !resp.details.family.is_empty() {
        model_rows.push(("architecture", resp.details.family.clone()));
    }
    if !resp.details.parameter_size.is_empty() {
        model_rows.push(("parameters", resp.details.parameter_size.clone()));
    }
    if let Some(context_length) = resp
        .model_info
        .as_object()
        .and_then(|info| info.iter().find(|(k, _)| k.ends_with(".context_length")))
        .map(|(_, v)| v.to_string())
    {
        model_rows.push(("context length", context_length));
    }
    if !resp.details.quantization_level.is_empty() {
        model_rows.push(("quantization", resp.details.quantization_level.clone()));
    }
    if !model_rows.is_empty() {
        println!("  Model");
        let width = model_rows.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
        for (key, value) in &model_rows {
            println!("    {key:<width$}    {value}");
        }
        println!();
    }
    if !resp.capabilities.is_empty() {
        println!("  Capabilities");
        for capability in &resp.capabilities {
            println!("    {capability}");
        }
        println!();
    }
    if !resp.parameters.trim().is_empty() {
        println!("  Parameters");
        for line in resp.parameters.trim_end().lines() {
            println!("    {}", line.trim_end());
        }
        println!();
    }
    if !resp.system.trim().is_empty() {
        println!("  System");
        for line in resp.system.trim_end().lines() {
            println!("    {line}");
        }
        println!();
    }
    if !resp.license.trim().is_empty() {
        println!("  License");
        for line in resp
            .license
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(2)
        {
            println!("    {}", line.trim());
        }
        println!();
    }
}

/// Prints an aligned column table (four-space gutters, no separator row),
/// like ollama's list/ps output.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let render = |cells: Vec<&str>| {
        let mut line = String::new();
        for (i, cell) in cells.iter().enumerate() {
            line.push_str(cell);
            if i + 1 < cells.len() {
                for _ in 0..widths[i] - cell.chars().count() + 4 {
                    line.push(' ');
                }
            }
        }
        line
    };
    println!("{}", render(headers.to_vec()));
    for row in rows {
        println!("{}", render(row.iter().map(String::as_str).collect()));
    }
}

/// Ollama-style short model ID: the digest hex (scheme prefix stripped) cut
/// to 12 characters.
fn short_digest(digest: &str) -> String {
    let hex = digest.split_once(':').map(|(_, h)| h).unwrap_or(digest);
    hex.chars().take(12).collect()
}

/// Decimal-unit byte humanization matching ollama's `format.HumanBytes`:
/// one decimal place only below 10 of a unit and only when non-integral.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [(f64, &str); 4] = [(1e12, "TB"), (1e9, "GB"), (1e6, "MB"), (1e3, "KB")];
    let b = bytes as f64;
    for (scale, unit) in UNITS {
        if b >= scale {
            let value = b / scale;
            return if value >= 10.0 || value == value.trunc() {
                format!("{} {unit}", value.trunc() as u64)
            } else {
                format!("{value:.1} {unit}")
            };
        }
    }
    format!("{bytes} B")
}

/// PROCESSOR column for `ps`, derived from how much of the model fits in
/// VRAM — same buckets as ollama's own CLI.
fn processor(size: u64, size_vram: u64) -> String {
    if size_vram == 0 {
        "100% CPU".to_string()
    } else if size_vram >= size {
        "100% GPU".to_string()
    } else {
        let cpu = ((size - size_vram) as f64 / size as f64 * 100.0).round() as u64;
        format!("{}%/{}% CPU/GPU", cpu, 100 - cpu)
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// "3 weeks ago"-style humanization of an RFC3339 timestamp; falls back to
/// the raw date part when the timestamp doesn't parse.
fn time_ago(rfc3339: &str) -> String {
    match parse_rfc3339_epoch(rfc3339) {
        Some(then) => {
            let delta = now_epoch() - then;
            if delta < 0 {
                "just now".to_string()
            } else {
                format!("{} ago", human_delta(delta))
            }
        }
        None => rfc3339.split('T').next().unwrap_or(rfc3339).to_string(),
    }
}

/// UNTIL column for `ps`: expiry humanized as "... from now", "Never" for
/// zero/absent expiries, "Forever" for far-future ones (keep_alive -1).
fn time_until(rfc3339: &str) -> String {
    if rfc3339.is_empty() {
        return "Never".to_string();
    }
    let Some(then) = parse_rfc3339_epoch(rfc3339) else {
        return rfc3339.to_string();
    };
    if then <= 0 {
        return "Never".to_string(); // Go zero time (year 1)
    }
    let delta = then - now_epoch();
    if delta > 10 * 365 * 86400 {
        return "Forever".to_string();
    }
    if delta <= 0 {
        return "Stopping...".to_string();
    }
    format!("{} from now", human_delta(delta))
}

/// Humanizes a positive duration in seconds with ollama's bucket phrasing
/// ("about a minute", "3 weeks", ...).
fn human_delta(secs: i64) -> String {
    let minutes = secs / 60;
    let hours = secs / 3600;
    if secs < 1 {
        "less than a second".to_string()
    } else if secs == 1 {
        "1 second".to_string()
    } else if secs < 60 {
        format!("{secs} seconds")
    } else if minutes == 1 {
        "about a minute".to_string()
    } else if minutes < 60 {
        format!("{minutes} minutes")
    } else if hours == 1 {
        "about an hour".to_string()
    } else if hours < 48 {
        format!("{hours} hours")
    } else if hours < 24 * 14 {
        format!("{} days", hours / 24)
    } else if hours < 24 * 60 {
        format!("{} weeks", hours / 24 / 7)
    } else if hours < 24 * 365 * 2 {
        format!("{} months", hours / 24 / 30)
    } else {
        format!("{} years", hours / 24 / 365)
    }
}

/// Parses an RFC3339 timestamp (`YYYY-MM-DDTHH:MM:SS[.frac][Z|±HH:MM]`) to
/// Unix seconds without a date-time dependency. Returns None on any
/// unexpected shape.
fn parse_rfc3339_epoch(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.len() < 19 || !text.is_char_boundary(19) {
        return None;
    }
    let bytes = text.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = text[0..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    let hour: i64 = text[11..13].parse().ok()?;
    let minute: i64 = text[14..16].parse().ok()?;
    let second: i64 = text[17..19].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }

    let mut rest = &text[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.len() - frac.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        rest = &frac[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let sign = match rest.as_bytes()[0] {
                b'+' => 1i64,
                b'-' => -1,
                _ => return None,
            };
            let oh: i64 = rest[1..3].parse().ok()?;
            let om: i64 = rest[4..6].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    Some(days_from_civil(year, month, day) * 86400 + hour * 3600 + minute * 60 + second - offset)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// days-from-civil algorithm).
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1000), "1 KB");
        assert_eq!(human_bytes(1500), "1.5 KB");
        assert_eq!(human_bytes(986_000_000), "986 MB");
        assert_eq!(human_bytes(9_500_000_000), "9.5 GB");
        assert_eq!(human_bytes(18_200_000_000), "18 GB");
    }

    #[test]
    fn short_digest_strips_scheme_and_truncates() {
        assert_eq!(short_digest("sha256:abcdef0123456789"), "abcdef012345");
        assert_eq!(short_digest("abcdef0123456789"), "abcdef012345");
        assert_eq!(short_digest("short"), "short");
    }

    #[test]
    fn processor_splits_cpu_gpu() {
        assert_eq!(processor(100, 0), "100% CPU");
        assert_eq!(processor(100, 100), "100% GPU");
        assert_eq!(processor(100, 52), "48%/52% CPU/GPU");
    }

    #[test]
    fn rfc3339_parses_offsets_fractions_and_rejects_junk() {
        assert_eq!(parse_rfc3339_epoch("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_epoch("2024-01-15T10:30:00Z"),
            Some(1705314600)
        );
        assert_eq!(
            parse_rfc3339_epoch("2024-01-15T10:30:00.123456789Z"),
            Some(1705314600)
        );
        assert_eq!(
            parse_rfc3339_epoch("2024-01-15T02:30:00-08:00"),
            Some(1705314600)
        );
        assert_eq!(
            parse_rfc3339_epoch("2024-01-15T18:30:00+08:00"),
            Some(1705314600)
        );
        assert_eq!(parse_rfc3339_epoch("not a date"), None);
        assert_eq!(parse_rfc3339_epoch("2024-13-01T00:00:00Z"), None);
    }

    #[test]
    fn human_delta_buckets() {
        assert_eq!(human_delta(0), "less than a second");
        assert_eq!(human_delta(1), "1 second");
        assert_eq!(human_delta(30), "30 seconds");
        assert_eq!(human_delta(90), "about a minute");
        assert_eq!(human_delta(45 * 60), "45 minutes");
        assert_eq!(human_delta(90 * 60), "about an hour");
        assert_eq!(human_delta(30 * 3600), "30 hours");
        assert_eq!(human_delta(3 * 86400), "3 days");
        assert_eq!(human_delta(21 * 86400), "3 weeks");
        assert_eq!(human_delta(70 * 86400), "2 months");
        assert_eq!(human_delta(800 * 86400), "2 years");
    }

    #[test]
    fn time_until_special_cases() {
        assert_eq!(time_until(""), "Never");
        assert_eq!(time_until("0001-01-01T00:00:00Z"), "Never");
        assert_eq!(time_until("9999-01-01T00:00:00Z"), "Forever");
        assert_eq!(time_until("1970-01-02T00:00:00Z"), "Stopping...");
    }
}

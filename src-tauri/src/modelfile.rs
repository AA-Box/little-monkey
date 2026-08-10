//! Modelfile Studio: a real Ollama Modelfile parser, validator, short-name
//! guard, and GGUF/safetensors format sniffer — the "Modelfile Studio and
//! Import Hardening" Phase 8 item.
//!
//! This module is deliberately Tauri-free (bar the thin `#[tauri::command]`
//! wrappers at the bottom, which never touch an `AppHandle`) so the grammar,
//! validation, and file-format sniffing are independently unit-testable and
//! reusable outside the desktop app.
//!
//! Scope note: this closes the real gap in `ollama.rs::ollama_import_model`,
//! which writes only a throwaway one-line `FROM <path>` Modelfile. That
//! command, and its `ollama create -f` invocation, are left completely
//! unchanged — this module adds a *new*, additive, hardened path
//! (`ollama::ollama_create_from_modelfile`) that parses/validates a real,
//! full Modelfile (instructions, parameters, templates, licenses, adapters,
//! `REQUIRES`) and lets the frontend preview/validate it — the acceptance
//! criterion — before anything is installed into the model library.
//!
//! Grammar reference: Ollama's own Modelfile instructions are `FROM`,
//! `PARAMETER`, `TEMPLATE`, `SYSTEM`, `ADAPTER`, `LICENSE`, `MESSAGE`, and
//! `REQUIRES` (minimum Ollama version, semver). `TEMPLATE`/`SYSTEM`/`LICENSE`
//! accept either a single-line value or a `"""triple-quoted"""` block that
//! may span multiple lines; every other instruction is single-line. `#`
//! starts a full-line comment. The format is not case-sensitive for
//! instruction keywords and instructions may appear in any order (bar the
//! singular ones — `FROM`/`TEMPLATE`/`SYSTEM`/`REQUIRES` — which this parser
//! rejects if repeated, since Ollama's own behavior for duplicates is
//! undefined and silently picking one would hide a real authoring mistake).
//!
//! Two related fields on `m3_runtime_hub::M3CatalogModel` — `template` and
//! `projector` — exist for the *M3 download/manifest* pipeline, a completely
//! different install path from Ollama's own `ollama create`. This module
//! deliberately does not conflate `ADAPTER` (a LoRA/fine-tune weights file)
//! with a projector (a vision/audio encoder) — they are different concepts
//! in Ollama's own model, and no Modelfile instruction here maps to an M3
//! projector reference. The dry-run report below simply names its
//! `template`/`templatePresent` fields consistently with that sibling
//! module's naming for a reader moving between the two.

use std::io::Read;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Hard cap on how large a `TEMPLATE`/`SYSTEM`/`LICENSE` text file loaded
/// via [`modelfile_read_text_file`] may be — this backs a text editor field,
/// not a bulk file transfer.
const MAX_REFERENCE_TEXT_BYTES: u64 = 2 * 1024 * 1024;

/// Hard cap on a safetensors JSON header's declared length. Real headers are
/// a few KB to a few MB (large models have many tensors); this is generous
/// headroom while still rejecting a corrupt/adversarial length field before
/// it drives a multi-gigabyte allocation.
const MAX_SAFETENSORS_HEADER_BYTES: u64 = 64 * 1024 * 1024;

/// Maximum accepted length for a Modelfile "short name" (the `ollama create
/// <name>` argument). Ollama tag/name components are short in practice;
/// this is a defensive ceiling, not a real observed limit.
const MAX_SHORT_NAME_LEN: usize = 128;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A single parse/validation problem, optionally anchored to a 1-indexed
/// source line. Rendered via `Display` as the user-facing message returned
/// by every `#[tauri::command]` in this module (`Result<_, String>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelfileIssue {
    pub line: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for ModelfileIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

fn issue(line: Option<usize>, message: impl Into<String>) -> ModelfileIssue {
    ModelfileIssue {
        line,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// Parsed representation
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelfileParameter {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelfileMessage {
    pub role: String,
    pub content: String,
}

/// The structured result of parsing a Modelfile's text — grammar only, no
/// filesystem or semantic checks. See [`validate_modelfile`] for those.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedModelfile {
    pub from: Option<String>,
    pub requires: Option<String>,
    pub template: Option<String>,
    pub system: Option<String>,
    pub parameters: Vec<ModelfileParameter>,
    pub adapters: Vec<String>,
    pub licenses: Vec<String>,
    pub messages: Vec<ModelfileMessage>,
}

// ---------------------------------------------------------------------------
// Grammar parser
// ---------------------------------------------------------------------------

/// Splits `s` on its first run of whitespace, returning `(first_token, rest)`
/// with `rest` left-trimmed. `rest` is `""` if there was no whitespace.
fn split_first_token(s: &str) -> (&str, &str) {
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

/// Strips one layer of matching double quotes from `value`, if present —
/// covers `PARAMETER stop "AI assistant:"`-style quoted values.
fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

/// Consumes a `"""triple-quoted"""` block that begins with `rest` (the
/// remainder of the instruction's first line, already known to start with
/// `"""`). Returns the block's inner text (a single leading/trailing
/// newline immediately inside the markers is trimmed, matching the common
/// `INSTR """\n...\n"""` authoring style) and the index of the next
/// unconsumed line. Errors if end-of-file is reached with no closing `"""`.
fn collect_triple_quoted(
    rest: &str,
    lines: &[&str],
    start_index: usize,
    start_line_no: usize,
) -> Result<(String, usize), ModelfileIssue> {
    let after_open = &rest[3..];
    if let Some(close_idx) = after_open.find(r#"""""#) {
        return Ok((after_open[..close_idx].to_string(), start_index + 1));
    }

    let mut buf = String::new();
    buf.push_str(after_open);
    let mut idx = start_index + 1;
    loop {
        if idx >= lines.len() {
            return Err(issue(
                Some(start_line_no),
                "unterminated triple-quoted block (missing closing \"\"\")",
            ));
        }
        let line = lines[idx];
        if let Some(close_idx) = line.find(r#"""""#) {
            buf.push('\n');
            buf.push_str(&line[..close_idx]);
            idx += 1;
            break;
        }
        buf.push('\n');
        buf.push_str(line);
        idx += 1;
    }

    let mut content = buf;
    if let Some(stripped) = content.strip_prefix('\n') {
        content = stripped.to_string();
    }
    if let Some(stripped) = content.strip_suffix('\n') {
        content = stripped.to_string();
    }
    Ok((content, idx))
}

/// Parses real Ollama Modelfile grammar into a [`ParsedModelfile`]. Fails
/// fast on the first structural problem: an unknown instruction keyword, a
/// missing required value, an unrecognized `MESSAGE` role, a duplicate
/// singular instruction, or an unterminated triple-quoted block. Does *not*
/// check the filesystem or that `FROM` is present at all — see
/// [`validate_modelfile`].
pub fn parse_modelfile(text: &str) -> Result<ParsedModelfile, ModelfileIssue> {
    let lines: Vec<&str> = text.lines().collect();
    let mut result = ParsedModelfile::default();
    let mut from_seen = false;
    let mut template_seen = false;
    let mut system_seen = false;
    let mut requires_seen = false;

    let mut i = 0usize;
    while i < lines.len() {
        let line_no = i + 1;
        let trimmed = lines[i].trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        let (keyword, rest) = split_first_token(trimmed);
        let keyword_upper = keyword.to_ascii_uppercase();
        let supports_block = matches!(keyword_upper.as_str(), "TEMPLATE" | "SYSTEM" | "LICENSE");

        let (value, next_i) = if supports_block && rest.trim_start().starts_with(r#"""""#) {
            collect_triple_quoted(rest.trim_start(), &lines, i, line_no)?
        } else {
            (rest.trim().to_string(), i + 1)
        };

        match keyword_upper.as_str() {
            "FROM" => {
                if from_seen {
                    return Err(issue(Some(line_no), "duplicate FROM instruction"));
                }
                if value.is_empty() {
                    return Err(issue(Some(line_no), "FROM requires a value"));
                }
                result.from = Some(value);
                from_seen = true;
            }
            "TEMPLATE" => {
                if template_seen {
                    return Err(issue(Some(line_no), "duplicate TEMPLATE instruction"));
                }
                result.template = Some(value);
                template_seen = true;
            }
            "SYSTEM" => {
                if system_seen {
                    return Err(issue(Some(line_no), "duplicate SYSTEM instruction"));
                }
                result.system = Some(value);
                system_seen = true;
            }
            "REQUIRES" => {
                if requires_seen {
                    return Err(issue(Some(line_no), "duplicate REQUIRES instruction"));
                }
                if value.is_empty() {
                    return Err(issue(Some(line_no), "REQUIRES requires a version value"));
                }
                result.requires = Some(value);
                requires_seen = true;
            }
            "PARAMETER" => {
                let (key, raw_value) = split_first_token(&value);
                if key.is_empty() || raw_value.trim().is_empty() {
                    return Err(issue(
                        Some(line_no),
                        "PARAMETER requires a name and a value",
                    ));
                }
                result.parameters.push(ModelfileParameter {
                    key: key.to_string(),
                    value: unquote(raw_value.trim()),
                });
            }
            "ADAPTER" => {
                if value.is_empty() {
                    return Err(issue(Some(line_no), "ADAPTER requires a path"));
                }
                result.adapters.push(value);
            }
            "LICENSE" => {
                if value.is_empty() {
                    return Err(issue(Some(line_no), "LICENSE requires a value"));
                }
                result.licenses.push(value);
            }
            "MESSAGE" => {
                let (role, content) = split_first_token(&value);
                if role.is_empty() || content.trim().is_empty() {
                    return Err(issue(Some(line_no), "MESSAGE requires a role and content"));
                }
                let role_lower = role.to_ascii_lowercase();
                if !matches!(role_lower.as_str(), "system" | "user" | "assistant") {
                    return Err(issue(
                        Some(line_no),
                        format!(
                            "unknown MESSAGE role '{role}' (expected system, user, or assistant)"
                        ),
                    ));
                }
                result.messages.push(ModelfileMessage {
                    role: role_lower,
                    content: content.trim().to_string(),
                });
            }
            other => {
                return Err(issue(
                    Some(line_no),
                    format!("unknown instruction '{other}'"),
                ));
            }
        }

        i = next_i;
    }

    Ok(result)
}

impl ParsedModelfile {
    /// Serializes this parsed Modelfile back into Ollama's real Modelfile
    /// text syntax. Used only by the hardened create path
    /// (`ollama::ollama_create_from_modelfile`) after validation succeeds —
    /// this is not a place to silently drop, reorder, or reinterpret any
    /// user-authored instruction.
    pub fn render(&self) -> String {
        let mut out = String::new();
        if let Some(from) = &self.from {
            out.push_str(&format!("FROM {from}\n"));
        }
        if let Some(requires) = &self.requires {
            out.push_str(&format!("REQUIRES {requires}\n"));
        }
        if let Some(template) = &self.template {
            out.push_str(&format!("TEMPLATE \"\"\"{template}\"\"\"\n"));
        }
        if let Some(system) = &self.system {
            out.push_str(&format!("SYSTEM \"\"\"{system}\"\"\"\n"));
        }
        for parameter in &self.parameters {
            let value = if parameter.value.chars().any(char::is_whitespace) {
                format!("\"{}\"", parameter.value)
            } else {
                parameter.value.clone()
            };
            out.push_str(&format!("PARAMETER {} {value}\n", parameter.key));
        }
        for adapter in &self.adapters {
            out.push_str(&format!("ADAPTER {adapter}\n"));
        }
        for license in &self.licenses {
            out.push_str(&format!("LICENSE \"\"\"{license}\"\"\"\n"));
        }
        for message in &self.messages {
            out.push_str(&format!("MESSAGE {} {}\n", message.role, message.content));
        }
        out
    }

    /// Returns a copy with `FROM` and every `ADAPTER` value canonicalized to
    /// an absolute filesystem path, when that value resolves to a real
    /// path — existing-model references (e.g. `FROM llama3.2:latest`) are
    /// left untouched. Necessary because the final Modelfile is written into
    /// a throwaway temp directory, so a relative path the user typed must be
    /// resolved against the original working directory *before* that move,
    /// or it would silently point somewhere else (or nowhere).
    pub fn with_canonicalized_paths(&self) -> Result<ParsedModelfile, ModelfileIssue> {
        let mut cloned = self.clone();
        if let Some(from) = &cloned.from {
            if looks_like_filesystem_path(from) {
                let canonical = Path::new(from)
                    .canonicalize()
                    .map_err(|e| issue(None, format!("failed to resolve FROM path {from}: {e}")))?;
                cloned.from = Some(canonical.display().to_string());
            }
        }
        for adapter in cloned.adapters.iter_mut() {
            let canonical = Path::new(adapter).canonicalize().map_err(|e| {
                issue(
                    None,
                    format!("failed to resolve ADAPTER path {adapter}: {e}"),
                )
            })?;
            *adapter = canonical.display().to_string();
        }
        Ok(cloned)
    }
}

// ---------------------------------------------------------------------------
// Semantic validation
// ---------------------------------------------------------------------------

enum ParamType {
    Int,
    Float,
    Text,
}

/// Parameter names and value types documented on Ollama's own Modelfile
/// reference. Unknown parameter names degrade to a warning rather than a
/// hard error — Ollama's accepted parameter set has grown over time, and
/// wrongly rejecting a newer/undocumented one would be a worse failure mode
/// than letting it through with a heads-up.
const KNOWN_PARAMETERS: &[(&str, ParamType)] = &[
    ("num_ctx", ParamType::Int),
    ("repeat_last_n", ParamType::Int),
    ("repeat_penalty", ParamType::Float),
    ("temperature", ParamType::Float),
    ("seed", ParamType::Int),
    ("stop", ParamType::Text),
    ("num_predict", ParamType::Int),
    ("draft_num_predict", ParamType::Int),
    ("top_k", ParamType::Int),
    ("top_p", ParamType::Float),
    ("min_p", ParamType::Float),
];

/// Validates a `REQUIRES` value looks like the semver Ollama expects
/// (e.g. `0.5.0`, `0.14`) — digits-only components separated by `.`.
fn is_plausible_semver(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    (2..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// Runs every non-filesystem semantic check over an already-parsed
/// Modelfile — required `FROM`, known-parameter value types, `REQUIRES`
/// semver shape, and `ADAPTER` file existence — returning collected
/// warnings on success or the first hard error encountered.
pub fn validate_modelfile(parsed: &ParsedModelfile) -> Result<Vec<String>, ModelfileIssue> {
    if parsed.from.is_none() {
        return Err(issue(
            None,
            "Modelfile is missing a required FROM instruction",
        ));
    }

    let mut warnings = Vec::new();

    for parameter in &parsed.parameters {
        match KNOWN_PARAMETERS
            .iter()
            .find(|(name, _)| *name == parameter.key.as_str())
        {
            Some((_, ParamType::Int)) => {
                if parameter.value.trim().parse::<i64>().is_err() {
                    return Err(issue(
                        None,
                        format!(
                            "malformed parameter value for '{}': expected an integer, got '{}'",
                            parameter.key, parameter.value
                        ),
                    ));
                }
            }
            Some((_, ParamType::Float)) => {
                if parameter.value.trim().parse::<f64>().is_err() {
                    return Err(issue(
                        None,
                        format!(
                            "malformed parameter value for '{}': expected a number, got '{}'",
                            parameter.key, parameter.value
                        ),
                    ));
                }
            }
            Some((_, ParamType::Text)) => {}
            None => {
                warnings.push(format!(
                    "unknown parameter '{}' — Ollama may not recognize this",
                    parameter.key
                ));
            }
        }
    }

    if let Some(requires) = &parsed.requires {
        if !is_plausible_semver(requires) {
            return Err(issue(
                None,
                format!("REQUIRES must be a semantic version like 0.5.0 (got '{requires}')"),
            ));
        }
    }

    for adapter in &parsed.adapters {
        if !Path::new(adapter).exists() {
            return Err(issue(None, format!("adapter file not found at {adapter}")));
        }
    }

    Ok(warnings)
}

// ---------------------------------------------------------------------------
// Short name validation
// ---------------------------------------------------------------------------

/// Validates and normalizes the `ollama create <name>` short name — this
/// value becomes both a filesystem path component (Ollama's own model
/// storage) and a literal CLI argument, so it is checked defensively even
/// though `Command::arg` never invokes a shell. Rejects: empty/whitespace,
/// excessive length, path-traversal-like segments (`..`, leading `/`,
/// leading `.`, empty path segments from `//`), more than one `:` tag
/// separator, and any character outside Ollama's own tag charset.
pub fn validate_short_name(raw: &str) -> Result<String, ModelfileIssue> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(issue(None, "Short name must not be empty"));
    }
    if trimmed.len() > MAX_SHORT_NAME_LEN {
        return Err(issue(
            None,
            format!("Short name is too long ({MAX_SHORT_NAME_LEN} character max)"),
        ));
    }
    if trimmed.starts_with('-') || trimmed.starts_with('.') || trimmed.starts_with('/') {
        return Err(issue(
            None,
            "Short name must not start with '-', '.', or '/'",
        ));
    }
    if trimmed.contains("..") {
        return Err(issue(None, "Short name must not contain '..'"));
    }
    if trimmed.split('/').any(|segment| segment.is_empty()) {
        return Err(issue(
            None,
            "Short name must not contain empty path segments ('//' or a leading/trailing '/')",
        ));
    }
    let valid_chars = trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '/' | '-'));
    if !valid_chars {
        return Err(issue(
            None,
            "Short name may only contain letters, numbers, '.', '_', ':', '/', and '-'",
        ));
    }
    if trimmed.matches(':').count() > 1 {
        return Err(issue(
            None,
            "Short name must contain at most one ':' tag separator",
        ));
    }
    Ok(trimmed.to_string())
}

// ---------------------------------------------------------------------------
// GGUF / safetensors format sniffing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum DetectedFormat {
    Gguf,
    SafetensorsFile,
    SafetensorsDirectory,
    /// `FROM` did not resolve to any path on disk — treated as a reference
    /// to a model Ollama already knows about (e.g. `FROM llama3.2:latest`).
    ExistingModelReference,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInspection {
    pub original_path: String,
    pub size_bytes: u64,
    pub format: DetectedFormat,
    pub warnings: Vec<String>,
}

/// Heuristic: does `raw` look like it's meant to be a filesystem path,
/// rather than an Ollama model tag like `llama3.2:latest`? Used only to
/// decide whether `FROM`'s value should be treated as a path (and thus
/// validated/sniffed) versus an existing pulled model reference — real tag
/// names never contain a path separator or a leading `.`, so this stays
/// conservative and falls back to an outright existence check.
fn looks_like_filesystem_path(raw: &str) -> bool {
    raw.contains('/') || raw.contains('\\') || raw.starts_with('.') || Path::new(raw).exists()
}

/// Inspects a `FROM` value: either a GGUF file, a safetensors file/directory,
/// or (if it doesn't resolve to a path at all) an existing model reference.
/// Returns specific, actionable errors — never a generic "import failed" —
/// for anything that looks like a path but isn't a valid model source.
pub fn inspect_source(raw: &str) -> Result<SourceInspection, ModelfileIssue> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(issue(None, "FROM value must not be empty"));
    }
    if !looks_like_filesystem_path(trimmed) {
        return Ok(SourceInspection {
            original_path: trimmed.to_string(),
            size_bytes: 0,
            format: DetectedFormat::ExistingModelReference,
            warnings: Vec::new(),
        });
    }

    let path = Path::new(trimmed);
    if !path.exists() {
        return Err(issue(None, format!("file not found at {trimmed}")));
    }
    let metadata = std::fs::metadata(path)
        .map_err(|e| issue(None, format!("failed to read metadata for {trimmed}: {e}")))?;

    if metadata.is_dir() {
        inspect_safetensors_directory(path, trimmed)
    } else {
        inspect_model_file(path, trimmed, metadata.len())
    }
}

fn inspect_model_file(
    path: &Path,
    original: &str,
    size_bytes: u64,
) -> Result<SourceInspection, ModelfileIssue> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    let mut file = std::fs::File::open(path)
        .map_err(|e| issue(None, format!("failed to open {original}: {e}")))?;
    let mut magic = [0u8; 4];
    let read = file.read(&mut magic).unwrap_or(0);

    if read == 4 && &magic == b"GGUF" {
        return validate_gguf_header(&mut file, original, size_bytes);
    }

    match extension.as_deref() {
        Some("gguf") => Err(issue(
            None,
            format!("not a valid GGUF file: bad magic bytes in {original}"),
        )),
        Some("safetensors") => validate_safetensors_header(path, original, size_bytes),
        _ => Err(issue(
            None,
            format!(
                "unrecognized model file format for {original} (expected .gguf or .safetensors)"
            ),
        )),
    }
}

/// Reads the GGUF header immediately following the already-consumed 4-byte
/// magic (`file`'s cursor is at offset 4): a `uint32` little-endian version
/// field. This is a sanity check, not a full GGUF parse — it exists to
/// reject truncated/corrupt files with a specific error instead of an
/// opaque failure surfacing only once `ollama create` itself chokes on them.
fn validate_gguf_header(
    file: &mut std::fs::File,
    original: &str,
    size_bytes: u64,
) -> Result<SourceInspection, ModelfileIssue> {
    let mut version_buf = [0u8; 4];
    if file.read(&mut version_buf).unwrap_or(0) != 4 {
        return Err(issue(
            None,
            format!("not a valid GGUF file: file too small to contain a header ({original})"),
        ));
    }
    let version = u32::from_le_bytes(version_buf);

    let mut warnings = Vec::new();
    if !(1..=10).contains(&version) {
        warnings.push(format!(
            "unusual GGUF version {version} in {original} — this may not be a supported build"
        ));
    }

    Ok(SourceInspection {
        original_path: original.to_string(),
        size_bytes,
        format: DetectedFormat::Gguf,
        warnings,
    })
}

/// Reads a safetensors file's header: an 8-byte little-endian `u64` length
/// prefix followed by that many bytes of JSON. Validates the length is
/// plausible (non-zero, fits within the file, under a generous cap) and
/// that the header bytes are actually valid JSON — the two failure modes
/// that would otherwise surface only as an opaque Ollama/llama.cpp error.
fn validate_safetensors_header(
    path: &Path,
    original: &str,
    size_bytes: u64,
) -> Result<SourceInspection, ModelfileIssue> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| issue(None, format!("failed to open {original}: {e}")))?;
    let mut len_buf = [0u8; 8];
    if file.read(&mut len_buf).unwrap_or(0) != 8 {
        return Err(issue(
            None,
            format!(
                "not a valid safetensors file: file too small to contain a header ({original})"
            ),
        ));
    }
    let header_len = u64::from_le_bytes(len_buf);
    if header_len == 0
        || header_len > size_bytes.saturating_sub(8)
        || header_len > MAX_SAFETENSORS_HEADER_BYTES
    {
        return Err(issue(
            None,
            format!("not a valid safetensors file: implausible header length in {original}"),
        ));
    }

    let mut header_buf = vec![0u8; header_len as usize];
    file.read_exact(&mut header_buf).map_err(|e| {
        issue(
            None,
            format!("failed to read safetensors header from {original}: {e}"),
        )
    })?;
    serde_json::from_slice::<serde_json::Value>(&header_buf).map_err(|_| {
        issue(
            None,
            format!("safetensors header is not valid JSON ({original})"),
        )
    })?;

    Ok(SourceInspection {
        original_path: original.to_string(),
        size_bytes,
        format: DetectedFormat::SafetensorsFile,
        warnings: Vec::new(),
    })
}

/// Inspects a Hugging Face-style safetensors checkout directory: warns if
/// `config.json` is missing (Ollama may still cope, but conversion is more
/// likely to fail without it), and validates the first `*.safetensors` file
/// found — erroring if none exist at all.
fn inspect_safetensors_directory(
    path: &Path,
    original: &str,
) -> Result<SourceInspection, ModelfileIssue> {
    let mut warnings = Vec::new();
    if !path.join("config.json").exists() {
        warnings.push(format!(
            "no config.json found in {original} — Ollama may not be able to infer the model architecture"
        ));
    }

    let entries = std::fs::read_dir(path)
        .map_err(|e| issue(None, format!("failed to read directory {original}: {e}")))?;

    let mut first_safetensors: Option<std::path::PathBuf> = None;
    let mut total_size = 0u64;
    for entry in entries.flatten() {
        let entry_path = entry.path();
        if let Ok(meta) = entry.metadata() {
            if meta.is_file() {
                total_size += meta.len();
                let is_safetensors = entry_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("safetensors"))
                    .unwrap_or(false);
                if is_safetensors && first_safetensors.is_none() {
                    first_safetensors = Some(entry_path);
                }
            }
        }
    }

    let Some(safetensors_path) = first_safetensors else {
        return Err(issue(
            None,
            format!("no .safetensors files found in directory {original}"),
        ));
    };
    let file_size = std::fs::metadata(&safetensors_path)
        .map(|m| m.len())
        .unwrap_or(0);
    let inspected = validate_safetensors_header(
        &safetensors_path,
        &safetensors_path.display().to_string(),
        file_size,
    )?;

    Ok(SourceInspection {
        original_path: original.to_string(),
        size_bytes: total_size,
        format: DetectedFormat::SafetensorsDirectory,
        warnings: [warnings, inspected.warnings].concat(),
    })
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelfileDryRunRequest {
    pub short_name: String,
    pub modelfile_text: String,
}

/// Structured preview of a Modelfile plus its `FROM` source, shown to the
/// user *before* anything is installed into the model library — the
/// Phase 8 acceptance requirement this module exists to satisfy.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelfileDryRunReport {
    pub short_name: String,
    pub from: Option<String>,
    pub source: Option<SourceInspection>,
    pub requires: Option<String>,
    pub template_present: bool,
    pub system_present: bool,
    pub parameters: Vec<ModelfileParameter>,
    pub license_present: bool,
    pub licenses: Vec<String>,
    pub adapters: Vec<String>,
    pub messages_count: usize,
    pub warnings: Vec<String>,
}

/// Runs the full preview pipeline: short-name validation, grammar parse,
/// semantic validation (including `ADAPTER` existence), and `FROM` source
/// inspection (GGUF/safetensors header sanity or existing-model reference).
/// Never touches the model library — this only ever reads.
pub fn build_dry_run_report(
    request: &ModelfileDryRunRequest,
) -> Result<ModelfileDryRunReport, ModelfileIssue> {
    let short_name = validate_short_name(&request.short_name)?;
    let parsed = parse_modelfile(&request.modelfile_text)?;
    let mut warnings = validate_modelfile(&parsed)?;

    let source = match &parsed.from {
        Some(from) => {
            let inspection = inspect_source(from)?;
            warnings.extend(inspection.warnings.clone());
            Some(inspection)
        }
        None => None,
    };

    Ok(ModelfileDryRunReport {
        short_name,
        from: parsed.from.clone(),
        source,
        requires: parsed.requires.clone(),
        template_present: parsed.template.is_some(),
        system_present: parsed.system.is_some(),
        parameters: parsed.parameters.clone(),
        license_present: !parsed.licenses.is_empty(),
        licenses: parsed.licenses.clone(),
        adapters: parsed.adapters.clone(),
        messages_count: parsed.messages.len(),
        warnings,
    })
}

/// Reads a small text file from disk for use as a `TEMPLATE`/`SYSTEM`/
/// `LICENSE` value in the Modelfile Studio editor — e.g. loading a
/// `LICENSE.md` or a saved system prompt. Bounded to
/// [`MAX_REFERENCE_TEXT_BYTES`] and rejects non-UTF-8 content, since this
/// backs a text editor field, not a general file transfer.
fn read_reference_text_file(raw_path: &str) -> Result<String, ModelfileIssue> {
    let trimmed = raw_path.trim();
    if trimmed.is_empty() {
        return Err(issue(None, "Path must not be empty"));
    }
    let path = Path::new(trimmed);
    if !path.exists() {
        return Err(issue(None, format!("file not found at {trimmed}")));
    }
    let metadata = std::fs::metadata(path)
        .map_err(|e| issue(None, format!("failed to read metadata for {trimmed}: {e}")))?;
    if metadata.is_dir() {
        return Err(issue(None, format!("{trimmed} is a directory, not a file")));
    }
    if metadata.len() > MAX_REFERENCE_TEXT_BYTES {
        return Err(issue(
            None,
            format!(
                "{trimmed} is too large to load as text (max {MAX_REFERENCE_TEXT_BYTES} bytes)"
            ),
        ));
    }
    let bytes =
        std::fs::read(path).map_err(|e| issue(None, format!("failed to read {trimmed}: {e}")))?;
    String::from_utf8(bytes).map_err(|_| issue(None, format!("{trimmed} is not valid UTF-8 text")))
}

// ---------------------------------------------------------------------------
// Tauri command glue
// ---------------------------------------------------------------------------

/// Parses Modelfile text for live editor feedback — grammar only, no
/// filesystem access. Cheap enough to call on every keystroke debounce.
#[tauri::command]
pub fn modelfile_parse(text: String) -> Result<ParsedModelfile, String> {
    parse_modelfile(&text).map_err(|e| e.to_string())
}

/// Full preview/validate pipeline backing the "Preview & Validate" step in
/// Modelfile Studio. See [`build_dry_run_report`].
#[tauri::command]
pub fn modelfile_dry_run(request: ModelfileDryRunRequest) -> Result<ModelfileDryRunReport, String> {
    build_dry_run_report(&request).map_err(|e| e.to_string())
}

/// Loads a small local text file's contents into the Modelfile Studio editor
/// (e.g. a `LICENSE` or saved `SYSTEM` prompt file). See
/// [`read_reference_text_file`].
#[tauri::command]
pub fn modelfile_read_text_file(path: String) -> Result<String, String> {
    read_reference_text_file(&path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempPath(std::path::PathBuf);

    impl TempPath {
        fn dir(label: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("modelfile-test-{label}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempPath(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(dir: &Path, name: &str, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("create fixture file");
        file.write_all(bytes).expect("write fixture bytes");
        path
    }

    fn valid_gguf_bytes() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"GGUF");
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]); // padding so the file isn't suspiciously tiny
        bytes
    }

    fn valid_safetensors_bytes() -> Vec<u8> {
        let header = br#"{"__metadata__":{"format":"pt"}}"#;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(header.len() as u64).to_le_bytes());
        bytes.extend_from_slice(header);
        bytes
    }

    // -- grammar: happy path -------------------------------------------------

    #[test]
    fn parses_every_instruction_kind() {
        let text = r#"
# a full-line comment is ignored
FROM llama3.2:latest
REQUIRES 0.5.0
TEMPLATE """
{{ .System }}
{{ .Prompt }}
"""
SYSTEM You are a terse assistant.
PARAMETER temperature 0.7
PARAMETER stop "AI assistant:"
LICENSE """
Apache-2.0
"""
MESSAGE system Be concise.
MESSAGE user Hello!
"#;
        let parsed = parse_modelfile(text).expect("valid modelfile parses");
        assert_eq!(parsed.from.as_deref(), Some("llama3.2:latest"));
        assert_eq!(parsed.requires.as_deref(), Some("0.5.0"));
        assert_eq!(
            parsed.template.as_deref(),
            Some("{{ .System }}\n{{ .Prompt }}")
        );
        assert_eq!(parsed.system.as_deref(), Some("You are a terse assistant."));
        assert_eq!(
            parsed.parameters,
            vec![
                ModelfileParameter {
                    key: "temperature".into(),
                    value: "0.7".into()
                },
                ModelfileParameter {
                    key: "stop".into(),
                    value: "AI assistant:".into()
                },
            ]
        );
        assert_eq!(parsed.licenses, vec!["Apache-2.0".to_string()]);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].role, "system");
        assert_eq!(parsed.messages[1].content, "Hello!");
    }

    #[test]
    fn instruction_keywords_are_case_insensitive() {
        let parsed = parse_modelfile("from llama3.2:latest\nparameter temperature 0.5\n")
            .expect("lowercase instructions parse");
        assert_eq!(parsed.from.as_deref(), Some("llama3.2:latest"));
        assert_eq!(parsed.parameters[0].key, "temperature");
    }

    #[test]
    fn single_line_triple_quote_block_is_supported() {
        let parsed = parse_modelfile(
            r#"FROM x
SYSTEM """You are helpful."""
"#,
        )
        .expect("single-line triple-quoted block parses");
        assert_eq!(parsed.system.as_deref(), Some("You are helpful."));
    }

    // -- grammar: hardening / errors -----------------------------------------

    #[test]
    fn rejects_unknown_instruction() {
        let err = parse_modelfile("FROM x\nBOGUS something\n").unwrap_err();
        assert_eq!(err.line, Some(2));
        assert!(err.message.contains("unknown instruction 'BOGUS'"));
    }

    #[test]
    fn rejects_unterminated_triple_quote_block() {
        let err = parse_modelfile("FROM x\nSYSTEM \"\"\"\nunterminated\n").unwrap_err();
        assert_eq!(err.line, Some(2));
        assert!(err.message.contains("unterminated triple-quoted block"));
    }

    #[test]
    fn rejects_duplicate_from() {
        let err = parse_modelfile("FROM a\nFROM b\n").unwrap_err();
        assert!(err.message.contains("duplicate FROM"));
    }

    #[test]
    fn rejects_malformed_parameter_missing_value() {
        let err = parse_modelfile("FROM x\nPARAMETER temperature\n").unwrap_err();
        assert!(err
            .message
            .contains("PARAMETER requires a name and a value"));
    }

    #[test]
    fn rejects_unknown_message_role() {
        let err = parse_modelfile("FROM x\nMESSAGE narrator Once upon a time\n").unwrap_err();
        assert!(err.message.contains("unknown MESSAGE role 'narrator'"));
    }

    #[test]
    fn missing_from_is_a_validation_error_not_a_parse_error() {
        let parsed = parse_modelfile("PARAMETER temperature 0.5\n").expect("parses structurally");
        assert!(parsed.from.is_none());
        let err = validate_modelfile(&parsed).unwrap_err();
        assert!(err.message.contains("missing a required FROM"));
    }

    #[test]
    fn rejects_malformed_numeric_parameter_value() {
        let parsed = parse_modelfile("FROM x\nPARAMETER num_ctx not-a-number\n").expect("parses");
        let err = validate_modelfile(&parsed).unwrap_err();
        assert!(err
            .message
            .contains("malformed parameter value for 'num_ctx'"));
    }

    #[test]
    fn unknown_parameter_name_is_a_warning_not_an_error() {
        let parsed = parse_modelfile("FROM x\nPARAMETER some_future_param 123\n").expect("parses");
        let warnings = validate_modelfile(&parsed).expect("unknown parameter is non-fatal");
        assert!(warnings.iter().any(|w| w.contains("some_future_param")));
    }

    #[test]
    fn rejects_malformed_requires_version() {
        let parsed = parse_modelfile("FROM x\nREQUIRES not-a-version\n").expect("parses");
        let err = validate_modelfile(&parsed).unwrap_err();
        assert!(err.message.contains("REQUIRES must be a semantic version"));
    }

    #[test]
    fn rejects_missing_adapter_file() {
        let missing = format!("/nonexistent/{}/adapter.gguf", uuid::Uuid::new_v4());
        let parsed = parse_modelfile(&format!("FROM x\nADAPTER {missing}\n")).expect("parses");
        let err = validate_modelfile(&parsed).unwrap_err();
        assert!(err.message.contains("adapter file not found"));
    }

    #[test]
    fn accepts_existing_adapter_file() {
        let dir = TempPath::dir("adapter");
        let adapter_path = write_file(&dir.0, "adapter.safetensors", b"whatever");
        let parsed = parse_modelfile(&format!("FROM x\nADAPTER {}\n", adapter_path.display()))
            .expect("parses");
        let warnings = validate_modelfile(&parsed).expect("existing adapter file passes");
        assert!(warnings.is_empty());
    }

    // -- short name -----------------------------------------------------------

    #[test]
    fn accepts_reasonable_short_names() {
        for name in ["my-model", "namespace/model:tag", "model.v2", "model_v2"] {
            assert!(
                validate_short_name(name).is_ok(),
                "expected {name} to be valid"
            );
        }
    }

    #[test]
    fn rejects_path_traversal_short_names() {
        for name in ["../etc/passwd", "a/../b", "..", "/etc/passwd", "a//b", "a/"] {
            assert!(
                validate_short_name(name).is_err(),
                "expected {name} to be rejected"
            );
        }
    }

    #[test]
    fn rejects_excessively_long_short_names() {
        let long = "a".repeat(MAX_SHORT_NAME_LEN + 1);
        assert!(validate_short_name(&long).is_err());
    }

    #[test]
    fn rejects_invalid_characters_and_extra_tag_separators() {
        assert!(validate_short_name("model name").is_err());
        assert!(validate_short_name("model$").is_err());
        assert!(validate_short_name("model:tag:extra").is_err());
    }

    #[test]
    fn rejects_empty_short_name() {
        assert!(validate_short_name("   ").is_err());
    }

    // -- GGUF / safetensors sniffing ------------------------------------------

    #[test]
    fn detects_valid_gguf_file() {
        let dir = TempPath::dir("gguf-ok");
        let path = write_file(&dir.0, "model.gguf", &valid_gguf_bytes());
        let inspection =
            inspect_source(&path.display().to_string()).expect("valid GGUF sniffs clean");
        assert_eq!(inspection.format, DetectedFormat::Gguf);
        assert!(inspection.warnings.is_empty());
    }

    #[test]
    fn rejects_gguf_file_with_bad_magic_bytes() {
        let dir = TempPath::dir("gguf-bad-magic");
        let path = write_file(&dir.0, "model.gguf", b"NOTGGUF-and-some-more-bytes-padding");
        let err = inspect_source(&path.display().to_string()).unwrap_err();
        assert!(err
            .message
            .contains("not a valid GGUF file: bad magic bytes"));
    }

    #[test]
    fn rejects_truncated_gguf_file() {
        let dir = TempPath::dir("gguf-truncated");
        let path = write_file(&dir.0, "model.gguf", b"GGUF");
        let err = inspect_source(&path.display().to_string()).unwrap_err();
        assert!(err.message.contains("file too small to contain a header"));
    }

    #[test]
    fn detects_valid_safetensors_file() {
        let dir = TempPath::dir("safetensors-ok");
        let path = write_file(&dir.0, "model.safetensors", &valid_safetensors_bytes());
        let inspection =
            inspect_source(&path.display().to_string()).expect("valid safetensors sniffs clean");
        assert_eq!(inspection.format, DetectedFormat::SafetensorsFile);
    }

    #[test]
    fn rejects_safetensors_file_with_non_json_header() {
        let dir = TempPath::dir("safetensors-bad-json");
        let mut bytes = Vec::new();
        let bogus_header = b"not json at all";
        bytes.extend_from_slice(&(bogus_header.len() as u64).to_le_bytes());
        bytes.extend_from_slice(bogus_header);
        let path = write_file(&dir.0, "model.safetensors", &bytes);
        let err = inspect_source(&path.display().to_string()).unwrap_err();
        assert!(err.message.contains("safetensors header is not valid JSON"));
    }

    #[test]
    fn rejects_safetensors_file_with_implausible_header_length() {
        let dir = TempPath::dir("safetensors-bad-length");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        bytes.extend_from_slice(b"short");
        let path = write_file(&dir.0, "model.safetensors", &bytes);
        let err = inspect_source(&path.display().to_string()).unwrap_err();
        assert!(err.message.contains("implausible header length"));
    }

    #[test]
    fn rejects_missing_file() {
        let missing = format!("/nonexistent/{}/model.gguf", uuid::Uuid::new_v4());
        let err = inspect_source(&missing).unwrap_err();
        assert!(err.message.contains("file not found at"));
    }

    #[test]
    fn rejects_unrecognized_extension() {
        let dir = TempPath::dir("unrecognized");
        let path = write_file(&dir.0, "model.bin", b"whatever bytes");
        let err = inspect_source(&path.display().to_string()).unwrap_err();
        assert!(err.message.contains("unrecognized model file format"));
    }

    #[test]
    fn bare_model_tag_is_an_existing_model_reference() {
        let inspection =
            inspect_source("llama3.2:latest").expect("tag-shaped FROM sniffs as a reference");
        assert_eq!(inspection.format, DetectedFormat::ExistingModelReference);
    }

    #[test]
    fn detects_safetensors_directory_and_warns_on_missing_config() {
        let dir = TempPath::dir("safetensors-dir");
        write_file(
            &dir.0,
            "model-00001-of-00001.safetensors",
            &valid_safetensors_bytes(),
        );
        let inspection =
            inspect_source(&dir.0.display().to_string()).expect("directory sniffs clean");
        assert_eq!(inspection.format, DetectedFormat::SafetensorsDirectory);
        assert!(inspection
            .warnings
            .iter()
            .any(|w| w.contains("config.json")));
    }

    #[test]
    fn rejects_directory_with_no_safetensors_files() {
        let dir = TempPath::dir("safetensors-dir-empty");
        write_file(&dir.0, "config.json", b"{}");
        let err = inspect_source(&dir.0.display().to_string()).unwrap_err();
        assert!(err.message.contains("no .safetensors files found"));
    }

    // -- dry run ---------------------------------------------------------------

    #[test]
    fn dry_run_reports_a_full_summary_without_touching_the_model_library() {
        let dir = TempPath::dir("dry-run");
        let gguf_path = write_file(&dir.0, "model.gguf", &valid_gguf_bytes());
        let request = ModelfileDryRunRequest {
            short_name: "my-custom-model".to_string(),
            modelfile_text: format!(
                "FROM {}\nTEMPLATE \"\"\"hi\"\"\"\nPARAMETER temperature 0.6\nLICENSE \"\"\"MIT\"\"\"\n",
                gguf_path.display()
            ),
        };
        let report = build_dry_run_report(&request).expect("valid dry run succeeds");
        assert_eq!(report.short_name, "my-custom-model");
        assert!(report.template_present);
        assert!(report.license_present);
        assert_eq!(report.parameters.len(), 1);
        assert_eq!(report.source.as_ref().unwrap().format, DetectedFormat::Gguf);
    }

    #[test]
    fn dry_run_surfaces_invalid_short_name_before_parsing() {
        // Deliberately doesn't *start* with '.'/'-'/'/' — this specifically
        // exercises the `..`-substring check rather than the leading-char
        // check, which would otherwise fire first and mask it (see
        // `rejects_path_traversal_short_names` above for the leading-char
        // cases).
        let request = ModelfileDryRunRequest {
            short_name: "escape/../etc".to_string(),
            modelfile_text: "FROM llama3.2:latest\n".to_string(),
        };
        let err = build_dry_run_report(&request).unwrap_err();
        assert!(err.message.contains("Short name must not contain '..'"));
    }

    #[test]
    fn dry_run_surfaces_missing_source_file_with_a_specific_error() {
        let missing = format!("/nonexistent/{}/model.gguf", uuid::Uuid::new_v4());
        let request = ModelfileDryRunRequest {
            short_name: "my-model".to_string(),
            modelfile_text: format!("FROM {missing}\n"),
        };
        let err = build_dry_run_report(&request).unwrap_err();
        assert!(err.message.contains("file not found at"));
    }

    // -- render / canonicalization -----------------------------------------

    #[test]
    fn render_roundtrips_every_instruction_kind() {
        let parsed = ParsedModelfile {
            from: Some("llama3.2:latest".to_string()),
            requires: Some("0.5.0".to_string()),
            template: Some("{{ .Prompt }}".to_string()),
            system: Some("Be terse.".to_string()),
            parameters: vec![ModelfileParameter {
                key: "temperature".into(),
                value: "0.7".into(),
            }],
            adapters: vec!["/tmp/adapter.gguf".to_string()],
            licenses: vec!["MIT".to_string()],
            messages: vec![ModelfileMessage {
                role: "user".into(),
                content: "hi".into(),
            }],
        };
        let rendered = parsed.render();
        let reparsed = parse_modelfile(&rendered).expect("rendered modelfile reparses cleanly");
        assert_eq!(reparsed.from, parsed.from);
        assert_eq!(reparsed.requires, parsed.requires);
        assert_eq!(reparsed.template, parsed.template);
        assert_eq!(reparsed.system, parsed.system);
        assert_eq!(reparsed.parameters, parsed.parameters);
        assert_eq!(reparsed.licenses, parsed.licenses);
        assert_eq!(reparsed.messages, parsed.messages);
    }

    #[test]
    fn canonicalizes_from_and_adapter_paths_but_leaves_model_references_alone() {
        // Deliberately avoids `std::env::set_current_dir` here — mutating the
        // process-wide working directory would race with every other test in
        // this binary running concurrently. An absolute path with a
        // redundant `.` segment exercises the same canonicalization logic
        // without needing a relative path at all.
        let dir = TempPath::dir("canonicalize");
        let gguf_path = write_file(&dir.0, "model.gguf", &valid_gguf_bytes());
        let adapter_path = write_file(&dir.0, "adapter.safetensors", b"lora");
        let noisy_from = dir.0.join(".").join(gguf_path.file_name().unwrap());

        let parsed = ParsedModelfile {
            from: Some(noisy_from.display().to_string()),
            adapters: vec![adapter_path.display().to_string()],
            ..Default::default()
        };
        let resolved = parsed
            .with_canonicalized_paths()
            .expect("canonicalization succeeds for real files");

        assert!(Path::new(resolved.from.as_ref().unwrap()).is_absolute());
        assert_eq!(
            Path::new(resolved.from.as_ref().unwrap()),
            gguf_path.canonicalize().unwrap()
        );
        assert!(Path::new(&resolved.adapters[0]).is_absolute());
    }

    #[test]
    fn leaves_existing_model_reference_from_untouched() {
        let parsed = ParsedModelfile {
            from: Some("llama3.2:latest".to_string()),
            ..Default::default()
        };
        let resolved = parsed
            .with_canonicalized_paths()
            .expect("no path to resolve");
        assert_eq!(resolved.from.as_deref(), Some("llama3.2:latest"));
    }

    // -- reference text file loading ------------------------------------------

    #[test]
    fn reads_a_small_reference_text_file() {
        let dir = TempPath::dir("reference-text");
        let path = write_file(&dir.0, "LICENSE.txt", b"MIT License text");
        let content = read_reference_text_file(&path.display().to_string()).expect("reads file");
        assert_eq!(content, "MIT License text");
    }

    #[test]
    fn rejects_missing_reference_text_file() {
        let missing = format!("/nonexistent/{}/LICENSE.txt", uuid::Uuid::new_v4());
        let err = read_reference_text_file(&missing).unwrap_err();
        assert!(err.message.contains("file not found at"));
    }

    #[test]
    fn rejects_directory_as_reference_text_file() {
        let dir = TempPath::dir("reference-text-dir");
        let err = read_reference_text_file(&dir.0.display().to_string()).unwrap_err();
        assert!(err.message.contains("is a directory"));
    }
}

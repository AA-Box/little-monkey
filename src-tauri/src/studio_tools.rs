//! Studio's sidecar tool tier: operations that are not diffusion at all.
//!
//! Face swap, detectors, segmenters, background removal — none of them are
//! `sd-server` features and none of them will ever arrive by pinning a newer
//! engine, because they are different programs. This module is how they reach
//! the app without becoming code inside it.
//!
//! # Why a process and not a plugin
//!
//! The obvious shape — let a tool ship code the app loads — is the one shape
//! this app cannot have. It is signed and notarized, it holds keychain items,
//! and it enforces an egress policy; arbitrary third-party code inside that
//! process defeats all three at once, and on macOS loading unsigned native code
//! into a hardened runtime does not even link. So a tool is a **separate
//! executable speaking a small HTTP contract**, exactly as VS Code puts
//! extensions in an extension host and browsers put them in their own world.
//! The app supervises it, talks to it over loopback, and can kill it.
//!
//! # The contract
//!
//! A tool binary is launched as `<binary> --host 127.0.0.1 --port <port>` and
//! must serve two routes:
//!
//! - `GET /tool/v1/manifest` → [`ToolManifest`], which *declares its own
//!   inputs*. Studio renders its form from that declaration, so a tool adds UI
//!   without shipping any: the manifest is the only thing standing between "a
//!   new binary" and "a working panel". This is the A1111 feel — browse,
//!   install, it appears — reached without an interpreter.
//! - `POST /tool/v1/run` with `{"inputs": {…}}` → [`ToolRunResponse`].
//!
//! Synchronous, unlike the diffusion engine's submit-and-poll: these
//! operations run in seconds, not minutes, so a job id and a queue would be
//! protocol nobody needs. A tool that genuinely needs minutes is the reason to
//! add one, and not before.
//!
//! # Everything here is untrusted input
//!
//! A manifest and a run response are bytes from a downloaded binary, so both
//! are validated as hard as anything arriving over the network: bounded
//! counts, bounded lengths, a media-type allowlist, and a byte ceiling checked
//! before the body is buffered. [`validate_manifest`] and
//! [`validate_inputs`] are pure so the rules are tested without a subprocess.

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The only manifest shape this build understands. A tool declaring anything
/// else is refused rather than guessed at — a misread input list would render
/// a form that silently drops what the user typed.
pub const TOOL_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// How long a tool gets to answer `/tool/v1/manifest` after launch. Generous
/// because a tool loads its own model — a face-swap ONNX is a few hundred
/// megabytes — but far short of the engine's 300s, since nothing here loads
/// diffusion weights.
const READY_TIMEOUT: Duration = Duration::from_secs(120);
/// Manifests are a page of JSON. Anything larger is a tool trying to make the
/// app buffer for it.
const MAX_MANIFEST_BYTES: u64 = 256 * 1024;
/// One run's worth of returned media, base64 inside JSON.
const MAX_RUN_RESPONSE_BYTES: u64 = 64 * 1024 * 1024;
/// The tail of a tool's own output kept for diagnosis, as the engine keeps.
const MAX_STDERR_TAIL: usize = 8 * 1024;

const MAX_INPUTS: usize = 32;
const MAX_IMAGE_INPUTS: usize = 4;
const MAX_CHOICES: usize = 64;
const MAX_MEDIA_ITEMS: usize = 8;
/// One supplied image, base64. Matches what the generation form accepts.
const MAX_INPUT_IMAGE_CHARS: usize = 32 * 1024 * 1024;
const MAX_INPUT_TEXT_CHARS: usize = 4 * 1024;

/// What a tool may hand back. An allowlist rather than a passthrough: the
/// media type is what the gallery and the artifact store believe about these
/// bytes, so a tool must not be able to name `text/html` and have the app file
/// it as media.
const ALLOWED_MEDIA_TYPES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/webp",
    "video/mp4",
    "audio/wav",
];

/// One input a tool asks for, and everything Studio needs to draw a control
/// for it without knowing what the tool does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolInputKind {
    /// Base64 image. Studio offers the gallery and a file picker.
    Image,
    Text,
    Number,
    Toggle,
    /// One of [`ToolInput::options`].
    Choice,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolChoice {
    pub value: String,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolInput {
    pub key: String,
    pub label: String,
    pub kind: ToolInputKind,
    #[serde(default)]
    pub required: bool,
    /// Pre-filled value. Type-checked against `kind` at validation, so a
    /// manifest cannot hand the form a number where it will draw a checkbox.
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub options: Vec<ToolChoice>,
    /// Shown behind the ⓘ on the card, like every other Studio setting group.
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolManifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub inputs: Vec<ToolInput>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolMedia {
    pub media_type: String,
    pub data_base64: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolRunResponse {
    pub media: Vec<ToolMedia>,
}

/// One tool in the user's library.
///
/// `managed` is the whole trust story in one flag: true means the bytes came
/// through [`crate::m3_runtime_hub::M3ComponentHub`], which downloaded them
/// from a registry entry and checked them against a declared SHA-256 before
/// activating. False means the user pointed at a binary they already had —
/// allowed for the same reason a user's own weight file is allowed, and
/// labelled in the UI so the two are never confused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioTool {
    pub id: String,
    pub name: String,
    /// Absolute path to the executable.
    pub path: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub managed: bool,
}

fn is_identifier(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn bounded(value: &str, limit: usize) -> bool {
    !value.trim().is_empty() && value.chars().count() <= limit
}

/// Validates a library entry before it is stored.
///
/// Existence is checked by the caller that has a filesystem, as
/// [`crate::generation::validate_lora_asset`] does, so this stays pure.
pub fn validate_tool(tool: &StudioTool) -> Result<(), String> {
    if !is_identifier(&tool.id, 128) {
        return Err(
            "A tool needs an id of letters, digits, dots, dashes or underscores".to_string(),
        );
    }
    if !bounded(&tool.name, 100) {
        return Err("A tool needs a name".to_string());
    }
    if !Path::new(&tool.path).is_absolute() {
        return Err("A tool needs an absolute path to its executable".to_string());
    }
    if let Some(version) = &tool.version {
        if !is_identifier(version, 64) {
            return Err(
                "A tool version must be letters, digits, dots, dashes or underscores".to_string(),
            );
        }
    }
    Ok(())
}

/// Checks a manifest a tool just served.
///
/// Every rule here exists because breaking it produces a form that lies: a
/// duplicate key silently drops one control's value, a `Choice` with no
/// options renders an empty dropdown the user cannot satisfy, and a default of
/// the wrong type pre-fills a control with something it cannot display.
pub fn validate_manifest(manifest: &ToolManifest) -> Result<(), String> {
    if manifest.schema_version != TOOL_MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "This tool declares manifest schema {} and this app speaks {TOOL_MANIFEST_SCHEMA_VERSION}. Update the app.",
            manifest.schema_version
        ));
    }
    if !is_identifier(&manifest.id, 128) {
        return Err("The tool's manifest id is not a valid identifier".to_string());
    }
    if !bounded(&manifest.name, 100) {
        return Err("The tool's manifest has no name".to_string());
    }
    if let Some(description) = &manifest.description {
        if description.chars().count() > 1_000 {
            return Err("The tool's description is too long".to_string());
        }
    }
    if manifest.inputs.len() > MAX_INPUTS {
        return Err(format!("A tool may declare at most {MAX_INPUTS} inputs"));
    }

    let mut seen = BTreeSet::new();
    let mut images = 0;
    for input in &manifest.inputs {
        if !is_identifier(&input.key, 64) {
            return Err(format!(
                "Input key '{}' is not a valid identifier",
                input.key
            ));
        }
        if !seen.insert(input.key.as_str()) {
            return Err(format!("Input key '{}' is declared twice", input.key));
        }
        if !bounded(&input.label, 100) {
            return Err(format!("Input '{}' has no label", input.key));
        }
        if let Some(hint) = &input.hint {
            if hint.chars().count() > 500 {
                return Err(format!("The hint on input '{}' is too long", input.key));
            }
        }
        if input.kind == ToolInputKind::Image {
            images += 1;
            if images > MAX_IMAGE_INPUTS {
                return Err(format!(
                    "A tool may declare at most {MAX_IMAGE_INPUTS} image inputs"
                ));
            }
        }
        validate_input_shape(input)?;
        if let Some(default) = &input.default {
            check_value(input, default)
                .map_err(|error| format!("The default for input '{}' {error}", input.key))?;
        }
    }
    Ok(())
}

/// The per-kind rules: which of the optional fields are meaningful, and which
/// being present means the manifest contradicts itself.
fn validate_input_shape(input: &ToolInput) -> Result<(), String> {
    match input.kind {
        ToolInputKind::Choice => {
            if input.options.is_empty() {
                return Err(format!("Input '{}' is a choice with no options", input.key));
            }
            if input.options.len() > MAX_CHOICES {
                return Err(format!(
                    "Input '{}' declares more than {MAX_CHOICES} options",
                    input.key
                ));
            }
            let mut values = BTreeSet::new();
            for option in &input.options {
                if !bounded(&option.value, 128) || !bounded(&option.label, 100) {
                    return Err(format!("Input '{}' has a blank option", input.key));
                }
                if !values.insert(option.value.as_str()) {
                    return Err(format!(
                        "Input '{}' lists the option '{}' twice",
                        input.key, option.value
                    ));
                }
            }
        }
        // Options on anything else means the tool author expected a dropdown
        // and will not get one. Refusing beats rendering the wrong control.
        _ if !input.options.is_empty() => {
            return Err(format!(
                "Input '{}' is not a choice but lists options",
                input.key
            ));
        }
        ToolInputKind::Number => {
            if let (Some(min), Some(max)) = (input.min, input.max) {
                if min > max {
                    return Err(format!("Input '{}' has a min above its max", input.key));
                }
            }
            for (field, value) in [("min", input.min), ("max", input.max), ("step", input.step)] {
                if value.is_some_and(|value| !value.is_finite()) {
                    return Err(format!("Input '{}' has a non-finite {field}", input.key));
                }
            }
            if input.step.is_some_and(|step| step <= 0.0) {
                return Err(format!("Input '{}' has a step of zero or less", input.key));
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether one value is acceptable for one declared input. The error is a
/// sentence fragment so both callers can prefix their own subject.
fn check_value(input: &ToolInput, value: &Value) -> Result<(), String> {
    match input.kind {
        ToolInputKind::Image => {
            let text = value.as_str().ok_or("must be a base64 image")?;
            if text.len() > MAX_INPUT_IMAGE_CHARS {
                return Err("exceeds the image size limit".to_string());
            }
            base64::engine::general_purpose::STANDARD
                .decode(text)
                .map_err(|_| "is not valid base64".to_string())?;
        }
        ToolInputKind::Text => {
            let text = value.as_str().ok_or("must be text")?;
            if text.chars().count() > MAX_INPUT_TEXT_CHARS {
                return Err("is longer than the text limit".to_string());
            }
        }
        ToolInputKind::Number => {
            let number = value.as_f64().filter(|value| value.is_finite());
            let number = number.ok_or("must be a number")?;
            if input.min.is_some_and(|min| number < min) {
                return Err("is below the declared minimum".to_string());
            }
            if input.max.is_some_and(|max| number > max) {
                return Err("is above the declared maximum".to_string());
            }
        }
        ToolInputKind::Toggle => {
            value.as_bool().ok_or("must be true or false")?;
        }
        ToolInputKind::Choice => {
            let text = value
                .as_str()
                .ok_or("must be one of the declared options")?;
            if !input.options.iter().any(|option| option.value == text) {
                return Err("is not one of the declared options".to_string());
            }
        }
    }
    Ok(())
}

/// Checks what the user filled in against what the tool declared, and returns
/// the body to send.
///
/// Unknown keys are refused rather than forwarded. A tool cannot be trusted to
/// ignore what it did not ask for, and a key that reaches it without appearing
/// in the manifest is by definition one the user never saw a control for.
/// Absent optional inputs are filled from their declared defaults so the tool
/// receives one complete set either way.
pub fn validate_inputs(
    manifest: &ToolManifest,
    supplied: &BTreeMap<String, Value>,
) -> Result<BTreeMap<String, Value>, String> {
    let declared: BTreeSet<&str> = manifest.inputs.iter().map(|i| i.key.as_str()).collect();
    if let Some(unknown) = supplied.keys().find(|key| !declared.contains(key.as_str())) {
        return Err(format!("'{unknown}' is not an input this tool accepts"));
    }

    let mut body = BTreeMap::new();
    for input in &manifest.inputs {
        let value = supplied
            .get(&input.key)
            .filter(|value| !is_blank(value))
            .cloned()
            .or_else(|| input.default.clone());
        match value {
            Some(value) => {
                check_value(input, &value).map_err(|error| format!("{} {error}", input.label))?;
                body.insert(input.key.clone(), value);
            }
            None if input.required => {
                return Err(format!("{} is required", input.label));
            }
            None => {}
        }
    }
    Ok(body)
}

/// An empty string is an untouched text box, not an answer — otherwise a
/// required field is satisfied by the user clearing it.
fn is_blank(value: &Value) -> bool {
    value.as_str().is_some_and(|text| text.trim().is_empty()) || value.is_null()
}

/// Checks what a tool returned before any of it is stored.
pub fn validate_run_response(response: &ToolRunResponse) -> Result<(), String> {
    if response.media.is_empty() {
        return Err("The tool returned no media".to_string());
    }
    if response.media.len() > MAX_MEDIA_ITEMS {
        return Err(format!(
            "The tool returned more than {MAX_MEDIA_ITEMS} media items"
        ));
    }
    for media in &response.media {
        if !ALLOWED_MEDIA_TYPES.contains(&media.media_type.as_str()) {
            return Err(format!(
                "The tool returned an unsupported media type: {}",
                media.media_type
            ));
        }
        if media.data_base64.is_empty() {
            return Err("The tool returned an empty media item".to_string());
        }
    }
    Ok(())
}

/// Decodes what [`validate_run_response`] has already accepted.
pub fn decode_media(media: &ToolMedia) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD
        .decode(&media.data_base64)
        .map_err(|_| "The tool returned media that is not valid base64".to_string())
}

/// A one-line summary of a run, for the gallery caption. The gallery shows a
/// prompt under every entry and a tool run has none, so this is what goes
/// there instead of an empty line.
pub fn run_summary(manifest: &ToolManifest, inputs: &BTreeMap<String, Value>) -> String {
    let mut parts = vec![manifest.name.clone()];
    for input in &manifest.inputs {
        // Images are megabytes of base64 and toggles read as noise; the
        // caption is for the settings a person would recognise.
        if input.kind == ToolInputKind::Image {
            continue;
        }
        if let Some(value) = inputs.get(&input.key) {
            let rendered = match value {
                Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            if !rendered.trim().is_empty() {
                parts.push(format!("{}: {rendered}", input.label));
            }
        }
    }
    let summary = parts.join(" · ");
    summary.chars().take(500).collect()
}

// -------------------------------------------------------------------------
// Process supervision
// -------------------------------------------------------------------------

fn free_port() -> Result<u16, String> {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|error| format!("Failed to reserve a port for the tool: {error}"))
}

/// Keeps the last [`MAX_STDERR_TAIL`] bytes of a tool's output so a launch
/// failure reports the tool's own words rather than a bare exit code.
fn drain_output(stream: impl Read + Send + 'static, tail: Arc<Mutex<String>>) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            if let Ok(mut buffer) = tail.lock() {
                buffer.push_str(&line);
                buffer.push('\n');
                if buffer.len() > MAX_STDERR_TAIL {
                    let cut = buffer.len() - MAX_STDERR_TAIL;
                    // Split on a char boundary: a tool may log UTF-8 and
                    // slicing mid-sequence panics this thread.
                    let cut = (cut..buffer.len())
                        .find(|index| buffer.is_char_boundary(*index))
                        .unwrap_or(buffer.len());
                    *buffer = buffer[cut..].to_string();
                }
            }
        }
    });
}

/// The one running tool sidecar.
///
/// ponytail: one at a time. Switching tools stops the previous process, which
/// costs a reload of its model. Tools are small and used one at a time in the
/// UI, so a pool would be idle memory; give this a map keyed by tool id if
/// chaining two tools in one run ever becomes a feature.
#[derive(Default)]
pub struct StudioToolState {
    inner: Mutex<ToolProcess>,
}

#[derive(Default)]
struct ToolProcess {
    child: Option<Child>,
    tool_id: Option<String>,
    port: Option<u16>,
    manifest: Option<ToolManifest>,
    stderr_tail: Option<Arc<Mutex<String>>>,
}

impl StudioToolState {
    pub fn running_tool(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.tool_id.clone())
    }

    pub fn base_url(&self) -> Option<String> {
        self.inner
            .lock()
            .ok()
            .and_then(|state| state.port)
            .map(|port| format!("http://127.0.0.1:{port}"))
    }

    pub fn stop(&self) -> Result<(), String> {
        let mut state = self.inner.lock().map_err(|error| error.to_string())?;
        if let Some(mut child) = state.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        state.tool_id = None;
        state.port = None;
        state.manifest = None;
        Ok(())
    }

    /// `Some(message)` when the child is already gone, carrying its own output
    /// so the failure says why.
    fn child_exited(&self) -> Result<Option<String>, String> {
        let mut state = self.inner.lock().map_err(|error| error.to_string())?;
        let Some(child) = state.child.as_mut() else {
            return Ok(Some("The tool is not running".to_string()));
        };
        let outcome = match child.try_wait() {
            Ok(Some(status)) => format!("The tool exited early ({status})"),
            Ok(None) => return Ok(None),
            Err(error) => format!("The tool is unreachable: {error}"),
        };
        let detail = state
            .stderr_tail
            .as_ref()
            .and_then(|tail| tail.lock().ok().map(|value| value.trim().to_string()))
            .filter(|value| !value.is_empty());
        Ok(Some(match detail {
            Some(detail) => format!("{outcome}:\n{detail}"),
            None => outcome,
        }))
    }

    /// Ensures `tool` is running and has served a valid manifest, then returns
    /// its base URL and that manifest.
    pub async fn ensure_ready(
        &self,
        tool: &StudioTool,
        client: &reqwest::Client,
    ) -> Result<(String, ToolManifest), String> {
        let warm = {
            let state = self.inner.lock().map_err(|error| error.to_string())?;
            state.tool_id.as_deref() == Some(tool.id.as_str())
        };
        if warm && self.child_exited()?.is_none() {
            if let (Some(base_url), Some(manifest)) = (
                self.base_url(),
                self.inner
                    .lock()
                    .map_err(|error| error.to_string())?
                    .manifest
                    .clone(),
            ) {
                return Ok((base_url, manifest));
            }
        }
        self.stop()?;

        let binary = PathBuf::from(&tool.path);
        if !binary.is_file() {
            return Err(format!(
                "{} is not installed at {}",
                tool.name,
                binary.display()
            ));
        }
        ensure_executable(&binary)?;

        let port = free_port()?;
        let base_url = format!("http://127.0.0.1:{port}");
        let mut child = Command::new(&binary)
            .args(["--host", "127.0.0.1", "--port", &port.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("Failed to start {}: {error}", tool.name))?;
        let tail = Arc::new(Mutex::new(String::new()));
        // Both streams, for the engine's reason: a piped stream nobody reads
        // fills its buffer and blocks the child.
        if let Some(stream) = child.stdout.take() {
            drain_output(stream, Arc::clone(&tail));
        }
        if let Some(stream) = child.stderr.take() {
            drain_output(stream, Arc::clone(&tail));
        }
        {
            let mut state = self.inner.lock().map_err(|error| error.to_string())?;
            state.child = Some(child);
            state.tool_id = Some(tool.id.clone());
            state.port = Some(port);
            state.stderr_tail = Some(tail);
        }

        let endpoint = format!("{base_url}/tool/v1/manifest");
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last_error = String::new();
        while Instant::now() < deadline {
            if let Some(failure) = self.child_exited()? {
                self.stop()?;
                return Err(failure);
            }
            if let Ok(response) = crate::egress::send(client.get(&endpoint)).await {
                if response.status().is_success() {
                    // A rejected manifest stops the tool rather than leaving a
                    // process running that the app will never talk to again.
                    let manifest = match read_manifest(response).await {
                        Ok(manifest) => manifest,
                        Err(error) => {
                            self.stop()?;
                            return Err(error);
                        }
                    };
                    if let Ok(mut state) = self.inner.lock() {
                        state.manifest = Some(manifest.clone());
                    }
                    return Ok((base_url, manifest));
                }
                last_error = format!("The tool answered {} on its manifest", response.status());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        self.stop()?;
        Err(if last_error.is_empty() {
            format!("{} did not start in time", tool.name)
        } else {
            last_error
        })
    }

    /// Runs `inputs` against an already-ready tool.
    pub async fn run(
        &self,
        base_url: &str,
        client: &reqwest::Client,
        inputs: &BTreeMap<String, Value>,
    ) -> Result<ToolRunResponse, String> {
        let response = crate::egress::send(
            client
                .post(format!("{base_url}/tool/v1/run"))
                .json(&serde_json::json!({ "inputs": inputs })),
        )
        .await
        .map_err(|error| format!("The tool did not answer: {error}"))?;
        let status = response.status();
        let body = read_capped(response, MAX_RUN_RESPONSE_BYTES).await?;
        if !status.is_success() {
            // A tool's own explanation is the only actionable part of a
            // failure — "no face found in the source image" and "model file
            // missing" are both a bare 400 otherwise.
            let detail = serde_json::from_slice::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("error")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            return Err(if detail.is_empty() {
                format!("The tool refused the run ({status})")
            } else {
                format!("The tool refused the run ({status}): {detail}")
            });
        }
        let parsed: ToolRunResponse = serde_json::from_slice(&body)
            .map_err(|error| format!("The tool returned a result this app cannot read: {error}"))?;
        validate_run_response(&parsed)?;
        Ok(parsed)
    }
}

impl Drop for StudioToolState {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

async fn read_manifest(response: reqwest::Response) -> Result<ToolManifest, String> {
    let body = read_capped(response, MAX_MANIFEST_BYTES).await?;
    let manifest: ToolManifest = serde_json::from_slice(&body)
        .map_err(|error| format!("The tool served a manifest this app cannot read: {error}"))?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

/// Buffers a response body, refusing one that declares more than `limit` and
/// stopping if the stream exceeds it anyway.
///
/// The declared length is checked first so an oversized body costs nothing,
/// but it is only a hint — a chunked response has none, and a tool is free to
/// lie — so the running total is what actually enforces the ceiling.
async fn read_capped(response: reqwest::Response, limit: u64) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > limit)
    {
        return Err("The tool's response exceeds its size limit".to_string());
    }
    let mut response = response;
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("The tool's response failed mid-read: {error}"))?
    {
        if body.len() as u64 + chunk.len() as u64 > limit {
            return Err("The tool's response exceeds its size limit".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Gives a downloaded artifact the owner-execute bit.
///
/// The component hub stores what it fetched as a plain blob, because for every
/// other component kind the bytes are weights or an archive. A tool artifact is
/// the program itself, so without this every managed install fails at `spawn`
/// with a bare permission error.
#[cfg(unix)]
fn ensure_executable(binary: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let metadata = std::fs::metadata(binary).map_err(|error| error.to_string())?;
    let mode = metadata.permissions().mode();
    if mode & 0o100 != 0 {
        return Ok(());
    }
    let mut permissions = metadata.permissions();
    permissions.set_mode(mode | 0o700);
    std::fs::set_permissions(binary, permissions)
        .map_err(|error| format!("Failed to make {} executable: {error}", binary.display()))
}

#[cfg(not(unix))]
fn ensure_executable(_binary: &Path) -> Result<(), String> {
    // Windows decides by extension, not by a permission bit.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Serves one canned HTTP response on a loopback port and returns its base
    /// URL.
    ///
    /// The transport half of this module — the byte ceiling, the JSON parse,
    /// the error extraction — is only exercised against a real socket, and a
    /// real socket is cheaper here than a fixture binary: it needs no build
    /// step, no platform-specific script and no `chmod`.
    fn serve_once(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                // The request has to be drained before the reply, or the
                // client sees a reset instead of the response.
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn test_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn a_run_returns_the_media_the_tool_produced() {
        let base = serve_once(
            "200 OK",
            r#"{"media":[{"mediaType":"image/png","dataBase64":"QUJD"}]}"#,
        );
        let state = StudioToolState::default();
        let response = state
            .run(&base, &test_client(), &BTreeMap::new())
            .await
            .unwrap();
        assert_eq!(response.media.len(), 1);
        assert_eq!(decode_media(&response.media[0]).unwrap(), b"ABC");
    }

    /// The one failure mode that is otherwise invisible: without this, every
    /// refusal reads as a bare status and the sentence explaining it — "no face
    /// found in the source image" — is thrown away.
    #[tokio::test]
    async fn a_refused_run_carries_the_tool_s_own_explanation() {
        let base = serve_once("400 Bad Request", r#"{"error":"no face found"}"#);
        let state = StudioToolState::default();
        let error = state
            .run(&base, &test_client(), &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(error.contains("no face found"), "{error}");
    }

    /// A tool must not be able to make the app store whatever it likes: the
    /// media type is what the gallery and artifact store believe these bytes
    /// are, so the allowlist is enforced on the wire and not only in the pure
    /// validator.
    #[tokio::test]
    async fn a_run_returning_an_unlisted_media_type_is_refused_over_the_wire() {
        let base = serve_once(
            "200 OK",
            r#"{"media":[{"mediaType":"text/html","dataBase64":"PGI+"}]}"#,
        );
        let state = StudioToolState::default();
        let error = state
            .run(&base, &test_client(), &BTreeMap::new())
            .await
            .unwrap_err();
        assert!(error.contains("unsupported media type"), "{error}");
    }

    #[tokio::test]
    async fn a_manifest_larger_than_the_ceiling_is_refused_before_it_is_parsed() {
        let base = serve_once("200 OK", &"x".repeat(MAX_MANIFEST_BYTES as usize + 1));
        let response = crate::egress::send(test_client().get(format!("{base}/tool/v1/manifest")))
            .await
            .unwrap();
        let error = read_manifest(response).await.unwrap_err();
        assert!(error.contains("size limit"), "{error}");
    }

    /// The manifest is the tool's whole UI surface, so an invalid one must
    /// stop at the boundary rather than reaching the form.
    #[tokio::test]
    async fn a_manifest_that_fails_validation_is_refused_on_arrival() {
        let base = serve_once(
            "200 OK",
            r#"{"schemaVersion":1,"id":"t","name":"T","inputs":[{"key":"mode","label":"Mode","kind":"choice","options":[]}]}"#,
        );
        let response = crate::egress::send(test_client().get(format!("{base}/tool/v1/manifest")))
            .await
            .unwrap();
        let error = read_manifest(response).await.unwrap_err();
        assert!(error.contains("no options"), "{error}");
    }

    #[tokio::test]
    async fn a_well_formed_manifest_survives_the_round_trip() {
        let base = serve_once(
            "200 OK",
            r#"{"schemaVersion":1,"id":"face-swap","name":"Face Swap","inputs":[{"key":"source","label":"Source","kind":"image","required":true}]}"#,
        );
        let response = crate::egress::send(test_client().get(format!("{base}/tool/v1/manifest")))
            .await
            .unwrap();
        let manifest = read_manifest(response).await.unwrap();
        assert_eq!(manifest.id, "face-swap");
        assert!(manifest.inputs[0].required);
        assert_eq!(manifest.inputs[0].kind, ToolInputKind::Image);
    }

    /// The whole tier against a real process: spawn, serve a manifest, accept a
    /// run, hand back media.
    ///
    /// `examples/studio-tool-echo.mjs` is the reference tool the contract doc
    /// points people at, so this is simultaneously the end-to-end check and the
    /// guarantee that the example still speaks the contract — a doc example that
    /// silently stopped working would be worse than none.
    ///
    /// Unix only: the script is launched through its shebang, which is also what
    /// exercises [`ensure_executable`]. Windows dispatches by extension and
    /// would need a wrapper, which would be testing the wrapper.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_reference_tool_runs_end_to_end() {
        let example = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("examples/studio-tool-echo.mjs");
        assert!(example.is_file(), "the documented example is missing");
        if Command::new("node")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_err()
        {
            eprintln!("skipping: no node on this host");
            return;
        }

        let state = StudioToolState::default();
        let tool = StudioTool {
            id: "echo".to_string(),
            name: "Echo".to_string(),
            path: example.to_string_lossy().to_string(),
            version: None,
            managed: false,
        };
        let client = test_client();
        let (base_url, manifest) = state.ensure_ready(&tool, &client).await.unwrap();
        assert_eq!(manifest.id, "echo");
        assert_eq!(state.running_tool().as_deref(), Some("echo"));

        // Through the same validator the command uses, so the example's own
        // declarations are checked against the rules the UI enforces.
        let image = base64::engine::general_purpose::STANDARD.encode(b"pretend png");
        let body = validate_inputs(
            &manifest,
            &BTreeMap::from([("image".to_string(), Value::String(image.clone()))]),
        )
        .unwrap();
        // The optional inputs came back filled from the manifest's own defaults.
        assert_eq!(body.get("mode"), Some(&Value::String("passthrough".into())));

        let response = state.run(&base_url, &client, &body).await.unwrap();
        assert_eq!(decode_media(&response.media[0]).unwrap(), b"pretend png");

        // A tool's own refusal reaches the user as its own sentence.
        let mut failing = body.clone();
        failing.insert("mode".to_string(), Value::String("fail".into()));
        let error = state.run(&base_url, &client, &failing).await.unwrap_err();
        assert!(error.contains("on purpose"), "{error}");

        state.stop().unwrap();
        assert_eq!(state.running_tool(), None);
    }

    /// A tool that is not there must fail with the path, and must not leave a
    /// half-started process behind.
    #[tokio::test]
    async fn a_missing_binary_fails_with_its_path_and_starts_nothing() {
        let state = StudioToolState::default();
        let tool = StudioTool {
            id: "ghost".to_string(),
            name: "Ghost".to_string(),
            path: "/nonexistent/studio-tool-ghost".to_string(),
            version: None,
            managed: false,
        };
        let error = state.ensure_ready(&tool, &test_client()).await.unwrap_err();
        assert!(error.contains("/nonexistent/studio-tool-ghost"), "{error}");
        assert_eq!(state.running_tool(), None);
    }

    fn input(key: &str, kind: ToolInputKind) -> ToolInput {
        ToolInput {
            key: key.to_string(),
            label: key.to_string(),
            kind,
            required: false,
            default: None,
            min: None,
            max: None,
            step: None,
            options: Vec::new(),
            hint: None,
        }
    }

    fn manifest(inputs: Vec<ToolInput>) -> ToolManifest {
        ToolManifest {
            schema_version: TOOL_MANIFEST_SCHEMA_VERSION,
            id: "face-swap".to_string(),
            name: "Face Swap".to_string(),
            description: None,
            inputs,
        }
    }

    #[test]
    fn a_well_formed_manifest_is_accepted() {
        let mut strength = input("strength", ToolInputKind::Number);
        strength.min = Some(0.0);
        strength.max = Some(1.0);
        strength.default = Some(serde_json::json!(0.8));
        let manifest = manifest(vec![
            input("source", ToolInputKind::Image),
            strength,
            ToolInput {
                options: vec![ToolChoice {
                    value: "gfpgan".to_string(),
                    label: "GFPGAN".to_string(),
                }],
                ..input("restorer", ToolInputKind::Choice)
            },
        ]);
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn a_future_schema_is_refused_rather_than_guessed_at() {
        let mut future = manifest(Vec::new());
        future.schema_version = TOOL_MANIFEST_SCHEMA_VERSION + 1;
        assert!(validate_manifest(&future)
            .unwrap_err()
            .contains("Update the app"));
    }

    #[test]
    fn duplicate_input_keys_are_refused() {
        let duplicate = manifest(vec![
            input("scale", ToolInputKind::Number),
            input("scale", ToolInputKind::Text),
        ]);
        assert!(validate_manifest(&duplicate).unwrap_err().contains("twice"));
    }

    #[test]
    fn a_choice_needs_options_and_a_non_choice_must_not_have_them() {
        assert!(
            validate_manifest(&manifest(vec![input("mode", ToolInputKind::Choice)]))
                .unwrap_err()
                .contains("no options")
        );
        let stray = ToolInput {
            options: vec![ToolChoice {
                value: "a".to_string(),
                label: "A".to_string(),
            }],
            ..input("caption", ToolInputKind::Text)
        };
        assert!(validate_manifest(&manifest(vec![stray]))
            .unwrap_err()
            .contains("not a choice"));
    }

    #[test]
    fn a_default_of_the_wrong_type_is_refused() {
        let mut toggle = input("upscale", ToolInputKind::Toggle);
        toggle.default = Some(serde_json::json!("yes"));
        assert!(validate_manifest(&manifest(vec![toggle]))
            .unwrap_err()
            .contains("true or false"));
    }

    #[test]
    fn a_number_default_outside_its_own_range_is_refused() {
        let mut number = input("scale", ToolInputKind::Number);
        number.min = Some(1.0);
        number.max = Some(4.0);
        number.default = Some(serde_json::json!(8.0));
        assert!(validate_manifest(&manifest(vec![number]))
            .unwrap_err()
            .contains("above the declared maximum"));
    }

    #[test]
    fn too_many_image_inputs_are_refused() {
        let inputs = (0..=MAX_IMAGE_INPUTS)
            .map(|index| input(&format!("image{index}"), ToolInputKind::Image))
            .collect();
        assert!(validate_manifest(&manifest(inputs))
            .unwrap_err()
            .contains("image inputs"));
    }

    #[test]
    fn an_undeclared_input_never_reaches_the_tool() {
        let manifest = manifest(vec![input("scale", ToolInputKind::Number)]);
        let supplied = BTreeMap::from([("sneaky".to_string(), serde_json::json!("rm -rf"))]);
        assert!(validate_inputs(&manifest, &supplied)
            .unwrap_err()
            .contains("not an input this tool accepts"));
    }

    #[test]
    fn a_missing_required_input_is_refused_and_a_default_fills_an_optional_one() {
        let mut required = input("source", ToolInputKind::Image);
        required.required = true;
        let mut optional = input("scale", ToolInputKind::Number);
        optional.default = Some(serde_json::json!(2));
        let manifest = manifest(vec![required, optional]);

        assert!(validate_inputs(&manifest, &BTreeMap::new())
            .unwrap_err()
            .contains("is required"));

        let image = base64::engine::general_purpose::STANDARD.encode([1, 2, 3]);
        let body = validate_inputs(
            &manifest,
            &BTreeMap::from([("source".to_string(), serde_json::json!(image))]),
        )
        .unwrap();
        assert_eq!(body.get("scale"), Some(&serde_json::json!(2)));
    }

    #[test]
    fn a_cleared_text_box_does_not_satisfy_a_required_input() {
        let mut required = input("prompt", ToolInputKind::Text);
        required.required = true;
        let manifest = manifest(vec![required]);
        let supplied = BTreeMap::from([("prompt".to_string(), serde_json::json!("   "))]);
        assert!(validate_inputs(&manifest, &supplied)
            .unwrap_err()
            .contains("is required"));
    }

    #[test]
    fn a_number_outside_the_declared_range_is_refused() {
        let mut number = input("scale", ToolInputKind::Number);
        number.min = Some(1.0);
        number.max = Some(4.0);
        let manifest = manifest(vec![number]);
        let supplied = BTreeMap::from([("scale".to_string(), serde_json::json!(9))]);
        assert!(validate_inputs(&manifest, &supplied)
            .unwrap_err()
            .contains("above the declared maximum"));
    }

    #[test]
    fn a_choice_outside_the_declared_options_is_refused() {
        let choice = ToolInput {
            options: vec![ToolChoice {
                value: "gfpgan".to_string(),
                label: "GFPGAN".to_string(),
            }],
            ..input("restorer", ToolInputKind::Choice)
        };
        let manifest = manifest(vec![choice]);
        let supplied = BTreeMap::from([("restorer".to_string(), serde_json::json!("anything"))]);
        assert!(validate_inputs(&manifest, &supplied)
            .unwrap_err()
            .contains("not one of the declared options"));
    }

    #[test]
    fn media_of_an_unlisted_type_is_refused() {
        let response = ToolRunResponse {
            media: vec![ToolMedia {
                media_type: "text/html".to_string(),
                data_base64: "PGI+".to_string(),
            }],
        };
        assert!(validate_run_response(&response)
            .unwrap_err()
            .contains("unsupported media type"));
    }

    #[test]
    fn an_empty_result_is_refused() {
        assert!(
            validate_run_response(&ToolRunResponse { media: Vec::new() })
                .unwrap_err()
                .contains("no media")
        );
    }

    #[test]
    fn the_run_summary_names_the_tool_and_skips_image_payloads() {
        let manifest = manifest(vec![
            input("source", ToolInputKind::Image),
            input("scale", ToolInputKind::Number),
        ]);
        let inputs = BTreeMap::from([
            ("source".to_string(), serde_json::json!("QUJD")),
            ("scale".to_string(), serde_json::json!(2)),
        ]);
        let summary = run_summary(&manifest, &inputs);
        assert!(summary.starts_with("Face Swap"));
        assert!(summary.contains("scale: 2"));
        assert!(!summary.contains("QUJD"));
    }

    #[test]
    fn a_tool_needs_an_absolute_path_and_a_sane_id() {
        let good = StudioTool {
            id: "face-swap".to_string(),
            name: "Face Swap".to_string(),
            path: "/opt/tools/face-swap".to_string(),
            version: Some("1.2.0".to_string()),
            managed: true,
        };
        assert!(validate_tool(&good).is_ok());
        assert!(validate_tool(&StudioTool {
            path: "tools/face-swap".to_string(),
            ..good.clone()
        })
        .is_err());
        assert!(validate_tool(&StudioTool {
            id: "../escape".to_string(),
            ..good
        })
        .is_err());
    }
}

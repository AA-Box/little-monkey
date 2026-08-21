//! YAML/JSON "recipe" files — a saved agent task (prompt template + model
//! target + permission policy + declared parameters) runnable headlessly via
//! `monkey-cli task run` (CI-suitable, machine-readable output, deterministic
//! exit codes) or from the desktop app's Settings > Tasks recipe library.
//! Design doc: `docs/roadmap/p3-scheduled-automation.md`.
//!
//! `pub` (not `mod`, like `checkpoints`/`rules`/`memory` above) so
//! `monkey-cli`'s `task.rs` can call every function here directly — parsing,
//! validation, param substitution, and discovery are all `AppHandle`-free by
//! construction, following the same `*_impl` convention `checkpoints.rs`
//! establishes: only the thin `#[tauri::command]` wrappers at the bottom of
//! this file ever touch an `AppHandle`.
//!
//! Recipe discovery deliberately checks TWO locations, local shadowing
//! global by `name` (not filename): workspace-local
//! `.littlemonkey/recipes/*.{yml,yaml,json}` (checked into the repo,
//! shareable with a team) and the global `<agent-home>/recipes/` directory
//! (plus automatic discovery of an existing legacy app-data directory).
//! New recipes use the home; edits to legacy recipes stay at their original
//! path so relative workspaces keep the same meaning. `permission_mode`
//! is a required field with NO default — a lesson from Goose Recipes and
//! Cline's headless mode (see the design doc's "Competitor reference"):
//! nothing should run unattended without an explicit policy choice.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use regex::Regex;
use sha2::{Digest, Sha256};
use tauri::Emitter;

use crate::app_paths;
use crate::run_protocol::{
    ModelTargetSnapshot, PermissionMode as RunPermissionMode, PermissionPolicySnapshot,
    WorkspaceContext,
};

/// Current (and, so far, only) recipe schema version.
pub const RECIPE_SCHEMA_VERSION: u32 = 1;

/// One saved recipe's model target — mirrors `monkey-cli`'s own `Target`
/// resolution (`chat.rs`), but kept independent of it: this is a shared-lib
/// type parsed from user-authored YAML, while `chat::Target` is a
/// `monkey-cli`-only binary type resolved against live provider/keychain
/// state. `monkey-cli`'s `task.rs` is what bridges the two at run time.
/// Exactly one of `provider` (+ `model`), `ollama`, or `local_url` must be
/// set — see [`RecipeTarget::validate`].
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecipeTarget {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub ollama: Option<String>,
    #[serde(default)]
    pub local_url: Option<String>,
    /// A model id installed in **this machine's** managed runtime hub, served by
    /// the app's own verified `llama-server` for the life of the run.
    ///
    /// # Why this is a fourth option rather than a `local_url`
    ///
    /// `local_url` names an origin that is *already listening*. The managed
    /// runtime is not: it is started on a fresh loopback port when the run
    /// starts and killed when it ends, so its origin does not exist at the time
    /// the recipe is written. A recipe that tried to express it as a `local_url`
    /// would have to invent a port and would be wrong by the time it ran.
    ///
    /// This is also what a run placed by another owned machine needs (roadmap
    /// K17). A placed `ModelTargetSnapshot::ManagedLlama` names a model id and a
    /// `model_path` **on the submitter's disk**; the path is meaningless here, so
    /// the receiving node resolves the id against its own hub inventory and lets
    /// the executing process start the runtime. Before this field existed there
    /// was no recipe target that could say that, so placements naming the
    /// managed runtime had to be refused outright.
    #[serde(default)]
    pub managed_model: Option<String>,
}

impl RecipeTarget {
    /// Enforces the design doc's XOR constraint: `provider: openrouter #
    /// XOR ollama: "qwen2.5:14b" XOR local_url: "http://127.0.0.1:8090"` —
    /// plus `managed_model: "<hub model id>"` as a fourth, mutually exclusive
    /// option.
    pub fn validate(&self) -> Result<(), String> {
        let set_count = [
            self.provider.is_some(),
            self.ollama.is_some(),
            self.local_url.is_some(),
            self.managed_model.is_some(),
        ]
        .iter()
        .filter(|set| **set)
        .count();
        if set_count == 0 {
            return Err(
                "recipe target must set exactly one of provider, ollama, local_url, or managed_model"
                    .to_string(),
            );
        }
        if set_count > 1 {
            return Err("recipe target must set exactly one of provider, ollama, local_url, or managed_model — not more than one".to_string());
        }
        if self.provider.is_some() && self.model.is_none() {
            return Err("recipe target with 'provider' must also set 'model'".to_string());
        }
        if self
            .managed_model
            .as_deref()
            .is_some_and(|value| value.trim().is_empty() || value.len() > 512)
        {
            return Err("recipe target 'managed_model' must be a non-empty model id".to_string());
        }
        Ok(())
    }
}

pub const DESKTOP_TURN_SCHEMA_VERSION: u32 = 3;
const MAX_DESKTOP_HISTORY_MESSAGES: usize = 2_000;
const MAX_DESKTOP_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;
const MAX_DESKTOP_MCP_SERVERS: usize = 64;
const MAX_DESKTOP_STACKS: usize = 128;

/// One exact workspace root used to reconstruct the desktop's multi-root
/// sandbox inside the daemon-owned CLI task process. `WorkspaceContext`
/// intentionally stores grants rather than display labels, so this small
/// execution-only companion retains the label required by path routing.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopWorkspaceRootSnapshot {
    pub root_id: String,
    pub canonical_path: String,
    pub label: String,
    pub is_primary: bool,
}

/// Attachment bytes/text are embedded in the immutable queue snapshot. The
/// daemon never re-reads the original picker path after submission, so an
/// edited or removed source file cannot change the model input later.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopAttachmentSnapshot {
    pub path: String,
    pub kind: String,
    pub media_type: String,
    pub content: String,
    pub content_sha256: String,
    pub size_bytes: u64,
}

/// Secret-free selection of one MCP server that was connected and offered to
/// the desktop model at submission time. The digest covers the normalized
/// local config entry (including stdio environment values) without copying
/// those values into the daemon ledger. HTTP bearer tokens remain exclusively
/// in the OS keychain and are resolved only after the digest check succeeds.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopMcpServerSnapshot {
    pub id: String,
    pub config_sha256: String,
    pub tool_allowlist: Option<Vec<String>>,
}

/// Generation controls frozen with an interactive desktop turn. Most desktop
/// transports currently leave the native Ollama/OpenAI knobs unset, but
/// carrying the complete CLI-supported shape prevents a queued request from
/// inheriting later CLI defaults and preserves Anthropic effort exactly.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DesktopGenerationSettingsSnapshot {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub seed: Option<i64>,
    pub stop: Vec<String>,
    pub num_ctx: Option<i64>,
    pub num_predict: Option<i64>,
    pub format: Option<serde_json::Value>,
    pub think: Option<serde_json::Value>,
    pub hide_thinking: bool,
    pub keep_alive: Option<String>,
    pub effort: Option<String>,
}

/// Desktop tool-availability settings that change authorization or whether a
/// model can initiate more work. These are read once when the turn is queued,
/// then drive the daemon CLI loop without consulting mutable UI settings.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DesktopToolProfileSnapshot {
    pub memory_enabled: bool,
    pub web_tools_enabled: bool,
    pub verify_enabled: bool,
    pub verify_max_rounds: u32,
    pub subagents_enabled: bool,
}

/// Frozen M6A desktop request carried inside the ordinary daemon recipe
/// snapshot. This is audit/input data only; execution still goes through the
/// daemon's one queue and `monkey-cli task run` agent loop.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct DesktopTurnSnapshot {
    pub schema_version: u32,
    pub session_id: String,
    pub turn_id: String,
    pub submitted_at_ms: u64,
    /// Exact transport origin for managed-llama and Ollama execution. A
    /// provider target is resolved by its credential-bound provider id and
    /// must leave this unset.
    pub execution_base_url: Option<String>,
    pub history: Vec<serde_json::Value>,
    pub target: ModelTargetSnapshot,
    #[serde(default)]
    pub workspace: Option<WorkspaceContext>,
    pub execution_roots: Vec<DesktopWorkspaceRootSnapshot>,
    pub permission_policy: PermissionPolicySnapshot,
    pub generation: DesktopGenerationSettingsSnapshot,
    pub tool_profile: DesktopToolProfileSnapshot,
    pub mcp_servers: Vec<DesktopMcpServerSnapshot>,
    pub attached_stack_ids: Vec<String>,
    pub attached_stack_names: Vec<String>,
    #[serde(default)]
    pub attachments: Vec<DesktopAttachmentSnapshot>,
    /// Whether this turn promised the workspace would be different afterwards.
    ///
    /// Decided by the surface that took the send, frozen here, and checked by
    /// the runtime against what the turn actually changed — see
    /// [`crate::channels::mutation`]. Defaults to `false` so a turn submitted by
    /// an older webview is an ordinary answer rather than acquiring a promise it
    /// never made.
    #[serde(default)]
    pub workspace_mutation_required: bool,
}

fn sha256_hex(value: &[u8]) -> String {
    format!("{:x}", Sha256::digest(value))
}

/// MCP tool allowlists are semantic sets. Sorting and de-duplicating before
/// hashing means harmless hand-edited ordering does not look like config
/// drift, while any actual selection change still does.
pub fn normalized_mcp_tool_allowlist(value: Option<&[String]>) -> Option<Vec<String>> {
    value.map(|items| {
        let mut normalized = items.to_vec();
        normalized.sort();
        normalized.dedup();
        normalized
    })
}

fn canonical_json(value: &serde_json::Value, output: &mut String) {
    match value {
        serde_json::Value::Null => output.push_str("null"),
        serde_json::Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        serde_json::Value::Number(value) => output.push_str(&value.to_string()),
        serde_json::Value::String(value) => {
            output.push_str(&serde_json::to_string(value).expect("JSON strings are serializable"));
        }
        serde_json::Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                canonical_json(value, output);
            }
            output.push(']');
        }
        serde_json::Value::Object(values) => {
            output.push('{');
            let mut keys: Vec<&String> = values.keys().collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(',');
                }
                output.push_str(
                    &serde_json::to_string(key).expect("JSON object keys are serializable"),
                );
                output.push(':');
                canonical_json(&values[key], output);
            }
            output.push('}');
        }
    }
}

/// Cross-language digest for one normalized `mcp_servers.json` entry. The
/// TypeScript bridge uses the same recursively key-sorted JSON algorithm.
pub fn mcp_server_config_digest(entry: &crate::mcp::McpServerEntry) -> Result<String, String> {
    let mut normalized = entry.clone();
    normalized.tool_allowlist = normalized_mcp_tool_allowlist(entry.tool_allowlist.as_deref());
    let value = serde_json::to_value(normalized)
        .map_err(|error| format!("Failed to normalize MCP server '{}': {error}", entry.id))?;
    let mut canonical = String::new();
    canonical_json(&value, &mut canonical);
    Ok(sha256_hex(canonical.as_bytes()))
}

fn valid_snapshot_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

impl DesktopGenerationSettingsSnapshot {
    fn validate(&self) -> Result<(), String> {
        if self.temperature.is_some_and(|value| !value.is_finite())
            || self.top_p.is_some_and(|value| !value.is_finite())
        {
            return Err("desktop generation floats must be finite".to_string());
        }
        if self.stop.len() > 64
            || self
                .stop
                .iter()
                .any(|value| value.is_empty() || value.len() > 4_096)
        {
            return Err("desktop stop sequences are invalid".to_string());
        }
        if self.num_ctx.is_some_and(|value| value <= 0)
            || self.num_predict.is_some_and(|value| value <= 0)
        {
            return Err("desktop token limits must be positive".to_string());
        }
        if self
            .keep_alive
            .as_ref()
            .is_some_and(|value| value.is_empty() || value.len() > 128)
        {
            return Err("desktop keep_alive is invalid".to_string());
        }
        if self
            .effort
            .as_deref()
            .is_some_and(|value| !matches!(value, "low" | "medium" | "high" | "xhigh" | "max"))
        {
            return Err("desktop effort is invalid".to_string());
        }
        if self.think.as_ref().is_some_and(|value| {
            !matches!(value, serde_json::Value::Bool(_))
                && !matches!(value.as_str(), Some("low" | "medium" | "high"))
        }) {
            return Err("desktop think setting is invalid".to_string());
        }
        if self.format.as_ref().is_some_and(|value| {
            !matches!(value, serde_json::Value::Object(_)) && value.as_str() != Some("json")
        }) {
            return Err("desktop response format is invalid".to_string());
        }
        let structured_bytes =
            serde_json::to_vec(&(&self.format, &self.think)).map_err(|error| error.to_string())?;
        if structured_bytes.len() > 64 * 1024 {
            return Err("desktop generation schema exceeds 64 KiB".to_string());
        }
        Ok(())
    }
}

fn permission_mode_token(mode: &RunPermissionMode) -> &'static str {
    match mode {
        RunPermissionMode::Manual => "manual",
        RunPermissionMode::AcceptEdits => "acceptEdits",
        RunPermissionMode::Smart => "smart",
        RunPermissionMode::Plan => "plan",
        RunPermissionMode::Auto => "auto",
        RunPermissionMode::Bypass => "bypass",
    }
}

impl DesktopTurnSnapshot {
    pub fn validate_for_recipe(&self, recipe: &Recipe) -> Result<(), String> {
        if self.schema_version != DESKTOP_TURN_SCHEMA_VERSION {
            return Err(format!(
                "unsupported desktop turn version {} (expected {})",
                self.schema_version, DESKTOP_TURN_SCHEMA_VERSION
            ));
        }
        if self.session_id.trim().is_empty()
            || self.session_id.len() > 256
            || self.turn_id.trim().is_empty()
            || self.turn_id.len() > 256
            || self.submitted_at_ms == 0
        {
            return Err("desktop turn identity/timestamp is invalid".to_string());
        }
        self.target.validate().map_err(|error| error.to_string())?;
        match (&self.target, &self.execution_base_url) {
            (ModelTargetSnapshot::Ollama { base_url, .. }, Some(execution))
                if base_url == execution => {}
            (ModelTargetSnapshot::ManagedLlama { .. }, Some(execution)) => {
                let parsed = url::Url::parse(execution)
                    .map_err(|error| format!("desktop managed runtime URL is invalid: {error}"))?;
                if parsed.scheme() != "http"
                    || !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(
                        "desktop managed runtime must use a credential-free loopback HTTP origin"
                            .to_string(),
                    );
                }
            }
            (ModelTargetSnapshot::Provider { .. }, None) => {}
            _ => {
                return Err("desktop execution origin does not match the frozen target".to_string())
            }
        }
        match &self.target {
            ModelTargetSnapshot::Provider {
                provider_id, model, ..
            } if recipe.target.provider.as_deref() == Some(provider_id.as_str())
                && recipe.target.model.as_deref() == Some(model.as_str()) => {}
            ModelTargetSnapshot::Ollama { model, .. }
                if recipe.target.ollama.as_deref() == Some(model.as_str()) => {}
            ModelTargetSnapshot::ManagedLlama { model_id, .. }
                if recipe.target.local_url.as_deref() == self.execution_base_url.as_deref()
                    && recipe.target.model.as_deref() == Some(model_id.as_str()) => {}
            _ => {
                return Err(
                    "desktop recipe target fields differ from the frozen model target".to_string(),
                )
            }
        }
        if let Some(workspace) = &self.workspace {
            workspace.validate().map_err(|error| error.to_string())?;
        } else if recipe.workspace.is_some() || !self.execution_roots.is_empty() {
            return Err(
                "desktop chat-only turns must not carry a workspace or execution roots".to_string(),
            );
        }
        if self.workspace.is_none() && self.workspace_mutation_required {
            return Err("desktop workspace mutation requires an open workspace".to_string());
        }
        self.permission_policy
            .validate()
            .map_err(|error| error.to_string())?;
        if !self.permission_policy.unattended {
            return Err("daemon-backed desktop turns must snapshot unattended=true".to_string());
        }
        if permission_mode_token(&self.permission_policy.mode) != recipe.permission_mode {
            return Err(
                "desktop permission snapshot does not match recipe permission_mode".to_string(),
            );
        }
        if self.permission_policy.allow_network != self.tool_profile.web_tools_enabled {
            return Err(
                "desktop web-tool profile differs from the frozen network permission".to_string(),
            );
        }
        self.generation.validate()?;
        if self.tool_profile.verify_max_rounds > 3 {
            return Err("desktop verify_max_rounds must be between 0 and 3".to_string());
        }
        let system = recipe
            .system
            .as_deref()
            .ok_or_else(|| "desktop turns require a frozen system prompt".to_string())?;
        if system.len() > 1024 * 1024 {
            return Err("desktop system prompt exceeds 1 MiB".to_string());
        }

        if self.mcp_servers.len() > MAX_DESKTOP_MCP_SERVERS {
            return Err(format!(
                "desktop MCP selection exceeds {MAX_DESKTOP_MCP_SERVERS} servers"
            ));
        }
        let mut mcp_ids = HashSet::new();
        for server in &self.mcp_servers {
            if !valid_snapshot_id(&server.id)
                || !valid_digest(&server.config_sha256)
                || !mcp_ids.insert(server.id.as_str())
            {
                return Err("desktop MCP selection metadata is invalid".to_string());
            }
            if let Some(allowlist) = &server.tool_allowlist {
                if allowlist.len() > 1_000
                    || allowlist
                        .iter()
                        .any(|name| name.is_empty() || name.len() > 512)
                    || normalized_mcp_tool_allowlist(Some(allowlist)) != Some(allowlist.clone())
                {
                    return Err(format!(
                        "desktop MCP tool allowlist for '{}' is not normalized",
                        server.id
                    ));
                }
            }
        }

        if self.attached_stack_ids.len() > MAX_DESKTOP_STACKS {
            return Err(format!(
                "desktop stack selection exceeds {MAX_DESKTOP_STACKS} stacks"
            ));
        }
        let mut stack_ids = HashSet::new();
        if self
            .attached_stack_ids
            .iter()
            .any(|id| !valid_snapshot_id(id) || !stack_ids.insert(id.as_str()))
        {
            return Err("desktop attached stack ids are invalid or duplicated".to_string());
        }
        let mut stack_names = HashSet::new();
        if self.attached_stack_names.len() != self.attached_stack_ids.len()
            || self.attached_stack_names.iter().any(|name| {
                let normalized = name.trim().to_ascii_lowercase();
                normalized.is_empty() || name.len() > 512 || !stack_names.insert(normalized)
            })
        {
            return Err(
                "desktop attached stack names must uniquely match the frozen ids".to_string(),
            );
        }
        if self.history.is_empty() || self.history.len() > MAX_DESKTOP_HISTORY_MESSAGES {
            return Err(format!(
                "desktop history must contain 1..={MAX_DESKTOP_HISTORY_MESSAGES} messages"
            ));
        }
        for (index, message) in self.history.iter().enumerate() {
            let role = message.get("role").and_then(serde_json::Value::as_str);
            if !matches!(role, Some("system" | "user" | "assistant" | "tool")) {
                return Err(format!(
                    "desktop history message {index} has an invalid role"
                ));
            }
            if message.get("content").is_none() {
                return Err(format!("desktop history message {index} omits content"));
            }
        }
        if self
            .history
            .last()
            .and_then(|message| message.get("role"))
            .and_then(serde_json::Value::as_str)
            != Some("user")
        {
            return Err("desktop history must end with the submitted user message".to_string());
        }
        let history_bytes = serde_json::to_vec(&self.history).map_err(|error| error.to_string())?;
        if history_bytes.len() > MAX_DESKTOP_SNAPSHOT_BYTES {
            return Err("desktop history exceeds the 32 MiB snapshot limit".to_string());
        }

        if let Some(workspace) = &self.workspace {
            if self.execution_roots.is_empty()
                || self.execution_roots.len() != workspace.roots.len()
                || self
                    .execution_roots
                    .iter()
                    .filter(|root| root.is_primary)
                    .count()
                    != 1
            {
                return Err(
                    "desktop execution roots must exactly cover the workspace grants".to_string(),
                );
            }
            for root in &self.execution_roots {
                if root.label.trim().is_empty() || root.label.len() > 512 {
                    return Err("desktop workspace root label is invalid".to_string());
                }
                let grant = workspace
                    .roots
                    .iter()
                    .find(|grant| grant.root_id == root.root_id)
                    .ok_or_else(|| {
                        "desktop execution root is absent from workspace grants".to_string()
                    })?;
                if grant.canonical_path != root.canonical_path {
                    return Err(
                        "desktop execution root path differs from its workspace grant".to_string(),
                    );
                }
                if root.is_primary != (root.root_id == workspace.primary_root_id) {
                    return Err("desktop execution root primary marker is inconsistent".to_string());
                }
            }
            let primary = self
                .execution_roots
                .iter()
                .find(|root| root.is_primary)
                .ok_or_else(|| "desktop primary execution root is missing".to_string())?;
            if recipe.workspace.as_deref() != Some(primary.canonical_path.as_str()) {
                return Err("desktop primary workspace does not match recipe workspace".to_string());
            }
        }

        let mut attachment_bytes = 0usize;
        for attachment in &self.attachments {
            if attachment.path.trim().is_empty()
                || attachment.kind.trim().is_empty()
                || attachment.media_type.trim().is_empty()
                || attachment.content_sha256.len() != 64
                || !attachment
                    .content_sha256
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
            {
                return Err("desktop attachment metadata is invalid".to_string());
            }
            let bytes = attachment.content.as_bytes();
            if attachment.size_bytes != bytes.len() as u64
                || sha256_hex(bytes) != attachment.content_sha256.to_ascii_lowercase()
            {
                return Err(format!(
                    "desktop attachment '{}' failed its content digest",
                    attachment.path
                ));
            }
            attachment_bytes = attachment_bytes
                .checked_add(bytes.len())
                .ok_or_else(|| "desktop attachment size overflow".to_string())?;
        }
        if attachment_bytes > MAX_DESKTOP_SNAPSHOT_BYTES {
            return Err("desktop attachments exceed the 32 MiB snapshot limit".to_string());
        }
        Ok(())
    }
}

/// CLI-only output shaping — see `monkey-cli`'s `task run --json` (design
/// doc slice 1). Desktop-app runs (slice 2's "Run now") ignore this entirely
/// since they always render as an ordinary chat turn.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RecipeOutput {
    #[serde(default)]
    pub json: bool,
}

pub const AUTONOMOUS_TASK_RECIPE_SCHEMA_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct AutonomousTaskGuidanceSnapshot {
    pub guidance_id: String,
    pub text: String,
    pub applies_to: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct AutonomousTaskOwnerSnapshot {
    pub kind: String,
    pub instance_id: String,
    pub lease_epoch: u64,
    pub lease_expires_at_ms: u64,
}

fn autonomous_owner_path(task_id: &str) -> Result<PathBuf, String> {
    if task_id.is_empty()
        || task_id.len() > 256
        || !task_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("Autonomous task id contains unsupported path characters".to_string());
    }
    let data_dir = app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the Little Monkey data directory".to_string())?;
    Ok(data_dir
        .join("daemon")
        .join("autonomous-owners")
        .join(format!("{task_id}.json")))
}

/// Claims the immutable execution owner for a task. The first writer wins;
/// retries with the exact same lease are idempotent, while every competing
/// epoch or instance is rejected instead of overwriting the checkpoint.
pub fn claim_autonomous_task_owner(
    task_id: &str,
    owner: &AutonomousTaskOwnerSnapshot,
) -> Result<(), String> {
    let path = autonomous_owner_path(task_id)?;
    let parent = path
        .parent()
        .ok_or_else(|| "Autonomous owner path has no parent".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Could not create autonomous owner directory: {error}"))?;
    let bytes = serde_json::to_vec_pretty(owner)
        .map_err(|error| format!("Could not serialize autonomous owner: {error}"))?;
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            file.write_all(&bytes)
                .map_err(|error| format!("Could not persist autonomous owner: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("Could not sync autonomous owner: {error}"))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing: AutonomousTaskOwnerSnapshot =
                serde_json::from_slice(&std::fs::read(&path).map_err(|read_error| {
                    format!("Could not read autonomous owner: {read_error}")
                })?)
                .map_err(|read_error| {
                    format!("Autonomous owner checkpoint is invalid: {read_error}")
                })?;
            if existing == *owner {
                Ok(())
            } else {
                Err(format!(
                    "Autonomous task {task_id} is already owned by {} at lease epoch {}",
                    existing.instance_id, existing.lease_epoch
                ))
            }
        }
        Err(error) => Err(format!("Could not claim autonomous task owner: {error}")),
    }
}

pub fn autonomous_task_owner_matches(
    task_id: &str,
    owner: &AutonomousTaskOwnerSnapshot,
) -> Result<bool, String> {
    let path = autonomous_owner_path(task_id)?;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Could not read autonomous owner: {error}")),
    };
    let existing: AutonomousTaskOwnerSnapshot = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Autonomous owner checkpoint is invalid: {error}"))?;
    Ok(existing == *owner)
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default)]
pub struct AutonomousTaskSnapshot {
    pub schema_version: u32,
    pub task_id: String,
    pub objective: String,
    pub source: String,
    #[serde(default)]
    pub relevant_files: Vec<String>,
    pub current_workspace_revision: String,
    pub max_repair_rounds: u32,
    pub max_workers: u32,
    #[serde(default)]
    pub guidance: Vec<AutonomousTaskGuidanceSnapshot>,
    #[serde(default)]
    pub delivery_intent: Option<String>,
    #[serde(default)]
    pub execution_owner: Option<AutonomousTaskOwnerSnapshot>,
    /// The exact durable task state captured at handoff, when submitted by
    /// the desktop coordinator. CLI-created recipes may omit it.
    #[serde(default)]
    pub task_snapshot: Option<serde_json::Value>,
    /// Succeeded node IDs from the desktop coordinator at handoff.
    #[serde(default)]
    pub completed_nodes: Vec<String>,
    /// The node at the exact durable execution boundary.
    #[serde(default)]
    pub next_node_id: Option<String>,
}

/// A saved recipe, parsed from YAML or JSON (extension-sniffed — see
/// [`parse_recipe`]). `permission_mode` deliberately has NO `#[serde(default)]`:
/// omitting it from the recipe file is a hard parse error, not a silent
/// fallback to some default mode — see the module doc for why.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Recipe {
    pub version: u32,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub target: RecipeTarget,
    #[serde(default)]
    pub workspace: Option<String>,
    pub permission_mode: String,
    #[serde(default)]
    pub system: Option<String>,
    pub prompt: String,
    /// Declared params: name -> optional default. A param with `None` has
    /// no default and MUST be supplied via `--param name=value` at run time
    /// (see [`resolve_param_values`]).
    #[serde(default)]
    pub params: HashMap<String, Option<String>>,
    #[serde(default)]
    pub max_iterations: Option<usize>,
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
    #[serde(default)]
    pub output: RecipeOutput,
    /// Messaging destinations a run of this recipe may reach beyond answering
    /// its own conversation — see [`crate::run_protocol::ChannelSendPolicy`].
    /// Absent means reply-only, which is what every recipe written before this
    /// field existed keeps meaning. Rejected alongside `desktop_turn`, whose
    /// own permission policy carries the same grant.
    #[serde(default)]
    pub channel_send: Option<crate::run_protocol::ChannelSendPolicy>,
    /// Present only for immutable desktop turns submitted to the resident
    /// daemon. Hand-authored/scheduled recipes remain unchanged.
    #[serde(default)]
    pub desktop_turn: Option<DesktopTurnSnapshot>,
    /// Present only for a run **placed on this node by another machine we own**
    /// (roadmap K17 S2/S3). Written by the node's own placement route; never
    /// hand-authored, and rejected on any recipe that also carries a
    /// `desktop_turn`.
    ///
    /// # Why the frozen spec has to ride here rather than be re-derived
    ///
    /// The node's queue takes recipes, and the executing process builds the
    /// `RunSpec` from the recipe it was handed. A recipe carries a permission
    /// *mode* and a timeout and nothing else — no egress allowlist, no token or
    /// cost budget — so a placed spec converted to a recipe and back would come
    /// out wearing the **node's** defaults. The submitter's policy would have
    /// travelled and then been silently discarded at the last step, which is
    /// worse than never travelling: it reads as a guarantee. These four fields
    /// ride verbatim so the executing process builds its spec from the
    /// submitter's policy and budgets.
    #[serde(default)]
    pub placed_run: Option<crate::node_placement::PlacedRunSnapshot>,
    /// Frozen Universal AutonomousTask coordinator input. The daemon owns the
    /// execution after this snapshot is accepted; it never reconstructs the
    /// task from a recipe name or mutable desktop state.
    #[serde(default)]
    pub autonomous_task: Option<AutonomousTaskSnapshot>,
}

fn is_valid_recipe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Full validation beyond what `serde` already enforces (required fields
/// present, right types): schema version supported, name is a valid slug,
/// target's XOR constraint, and `permission_mode` is a real mode per
/// `permissions::VALID_MODES` — reusing that list directly so this can never
/// drift from what the permission gate itself accepts — MINUS `bypass`,
/// rejected separately below. A recipe can run fully unattended (croner-
/// scheduled by `scheduler.ts`, or via `monkey-cli task run` in CI), and
/// `bypass` short-circuits every tool prompt, `run_shell` included (see
/// `permissions::mode_short_circuit`'s `bypass` arm) — allowing it here would
/// let a scheduled/imported recipe execute arbitrary shell commands with no
/// human ever present to catch it, silently contradicting the "run_shell is
/// never auto-approved regardless of mode" invariant the rest of the app
/// holds to. Every other real mode still degrades safely unattended: it
/// prompts, gets no answer, and the run times out/fails instead of acting.
pub fn validate_recipe(recipe: &Recipe) -> Result<(), String> {
    if recipe.version != RECIPE_SCHEMA_VERSION {
        return Err(format!(
            "unsupported recipe version {} (expected {})",
            recipe.version, RECIPE_SCHEMA_VERSION
        ));
    }
    if !is_valid_recipe_name(&recipe.name) {
        return Err(format!(
            "recipe name '{}' must match [a-z0-9-]+",
            recipe.name
        ));
    }
    recipe.target.validate()?;
    if recipe.permission_mode == "bypass" {
        return Err(
            "recipe permission_mode 'bypass' is not allowed — recipes can run unattended, \
             and bypass auto-approves every tool (including run_shell) with nobody present \
             to catch it; pick a mode that still prompts or only auto-approves edits"
                .to_string(),
        );
    }
    if !crate::permissions::VALID_MODES.contains(&recipe.permission_mode.as_str()) {
        return Err(format!(
            "recipe permission_mode '{}' is invalid (expected one of {:?})",
            recipe.permission_mode,
            crate::permissions::VALID_MODES
        ));
    }
    if recipe.prompt.trim().is_empty() {
        return Err("recipe prompt must not be empty".to_string());
    }
    if let Some(channel_send) = &recipe.channel_send {
        if recipe.desktop_turn.is_some() {
            return Err(
                "recipe channel_send is not allowed alongside desktop_turn — the desktop \
                 turn's own permission policy carries that grant"
                    .to_string(),
            );
        }
        channel_send.validate().map_err(|error| error.to_string())?;
    }
    if let Some(snapshot) = &recipe.desktop_turn {
        snapshot.validate_for_recipe(recipe)?;
    }
    if let Some(placed) = &recipe.placed_run {
        if recipe.desktop_turn.is_some() {
            // Both freeze the same four fields, and a recipe carrying both
            // would leave "which one wins" to the order of two `unwrap_or_else`
            // chains in `task.rs`. Refused here so that question never arises.
            return Err("a recipe cannot be both a desktop turn and a placed run".to_string());
        }
        placed.validate()?;
    }
    if let Some(task) = &recipe.autonomous_task {
        if task.schema_version != AUTONOMOUS_TASK_RECIPE_SCHEMA_VERSION {
            return Err(format!(
                "unsupported autonomous task snapshot version {} (expected {})",
                task.schema_version, AUTONOMOUS_TASK_RECIPE_SCHEMA_VERSION
            ));
        }
        if task.task_id.trim().is_empty() || task.objective.trim().is_empty() {
            return Err("autonomous task snapshot requires task_id and objective".to_string());
        }
        if task.current_workspace_revision.trim().is_empty() {
            return Err("autonomous task snapshot requires a workspace revision".to_string());
        }
        if !(1..=16).contains(&task.max_workers) {
            return Err("autonomous task max_workers must be between 1 and 16".to_string());
        }
        if task.max_repair_rounds > 8 {
            return Err("autonomous task max_repair_rounds must be at most 8".to_string());
        }
        for file in &task.relevant_files {
            let path = std::path::Path::new(file);
            if path.is_absolute() || file.split('/').any(|part| part == "..") {
                return Err(format!(
                    "autonomous task file scope escapes the workspace: {file}"
                ));
            }
        }
        if task.guidance.len() > 32 {
            return Err("autonomous task guidance exceeds 32 items".to_string());
        }
        if task.completed_nodes.len() > 64 {
            return Err("autonomous task completed node list exceeds 64 items".to_string());
        }
        if let Some(task_snapshot) = &task.task_snapshot {
            let snapshot_bytes = serde_json::to_vec(task_snapshot).map_err(|error| {
                format!("autonomous task snapshot is not serializable: {error}")
            })?;
            if snapshot_bytes.len() > 512 * 1024 {
                return Err("autonomous task snapshot exceeds 512 KiB".to_string());
            }
        }
        if let Some(owner) = &task.execution_owner {
            if owner.instance_id.trim().is_empty()
                || owner.lease_epoch == 0
                || owner.lease_expires_at_ms == 0
            {
                return Err("autonomous task execution owner has an invalid lease".to_string());
            }
            if !matches!(owner.kind.as_str(), "desktop" | "daemon" | "remote") {
                return Err(format!(
                    "unsupported autonomous task execution owner '{}'",
                    owner.kind
                ));
            }
        }
        if recipe.desktop_turn.is_some() {
            return Err("an autonomous task recipe cannot also be a desktop turn".to_string());
        }
    }
    Ok(())
}

/// Parses `content` as YAML (via serde-saphyr) or JSON (via serde_json,
/// when `extension` is `"json"`, case-insensitive), then validates it —
/// callers never see an unvalidated `Recipe`.
pub fn parse_recipe(content: &str, extension: &str) -> Result<Recipe, String> {
    let recipe: Recipe = if extension.eq_ignore_ascii_case("json") {
        serde_json::from_str(content).map_err(|e| format!("Failed to parse recipe JSON: {e}"))?
    } else {
        serde_saphyr::from_str(content).map_err(|e| format!("Failed to parse recipe YAML: {e}"))?
    };
    validate_recipe(&recipe)?;
    Ok(recipe)
}

fn placeholder_regex() -> Regex {
    Regex::new(r"\{\{(\w+)\}\}").expect("static regex must compile")
}

/// Substitutes every `{{name}}` placeholder in `template` from `values`.
/// Every placeholder must resolve — an unsubstituted `{{name}}` left in the
/// output (no matching key in `values`) is a hard error, never sent to the
/// model as literal `{{...}}` text.
pub fn substitute_params(
    template: &str,
    values: &HashMap<String, String>,
) -> Result<String, String> {
    let re = placeholder_regex();
    let mut missing: Vec<String> = Vec::new();
    let substituted = re.replace_all(template, |caps: &regex::Captures| {
        let name = &caps[1];
        match values.get(name) {
            Some(v) => v.clone(),
            None => {
                missing.push(name.to_string());
                caps[0].to_string()
            }
        }
    });
    if !missing.is_empty() {
        missing.sort();
        missing.dedup();
        return Err(format!(
            "unsubstituted parameter placeholder(s): {}",
            missing.join(", ")
        ));
    }
    Ok(substituted.into_owned())
}

/// Resolves the final `name -> value` map for a recipe run: every key in
/// `overrides` (a `--param name=value` flag) must already be declared in
/// `recipe.params` — an unknown key is a hard error (typo protection, not a
/// silent no-op) — and every declared param either has an override, has its
/// own default, or is reported as missing (also a hard error, since a param
/// with no default and no override can't be substituted).
pub fn resolve_param_values(
    recipe: &Recipe,
    overrides: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let mut unknown: Vec<&str> = overrides
        .keys()
        .filter(|k| !recipe.params.contains_key(k.as_str()))
        .map(|k| k.as_str())
        .collect();
    if !unknown.is_empty() {
        unknown.sort();
        return Err(format!(
            "unknown --param key(s) not declared in this recipe: {}",
            unknown.join(", ")
        ));
    }

    let mut values = HashMap::new();
    let mut missing: Vec<&str> = Vec::new();
    for (name, default) in &recipe.params {
        if let Some(v) = overrides.get(name) {
            values.insert(name.clone(), v.clone());
        } else if let Some(d) = default {
            values.insert(name.clone(), d.clone());
        } else {
            missing.push(name.as_str());
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(format!(
            "missing required --param value(s) (no default): {}",
            missing.join(", ")
        ));
    }
    Ok(values)
}

/// A recipe's prompt/system, fully rendered (every `{{name}}` substituted) —
/// what `monkey-cli task run` and the GUI's `recipeRunner.ts` equivalent
/// actually feed into a turn.
#[derive(serde::Serialize)]
pub struct RenderedRecipe {
    pub prompt: String,
    pub system: Option<String>,
}

/// Resolves param values then substitutes them into `prompt`/`system` — the
/// one function both `task run` and the GUI's "Run now" call.
pub fn render_recipe(
    recipe: &Recipe,
    overrides: &HashMap<String, String>,
) -> Result<RenderedRecipe, String> {
    let values = resolve_param_values(recipe, overrides)?;
    let prompt = substitute_params(&recipe.prompt, &values)?;
    let system = recipe
        .system
        .as_deref()
        .map(|s| substitute_params(s, &values))
        .transpose()?;
    Ok(RenderedRecipe { prompt, system })
}

const RECIPE_EXTENSIONS: &[&str] = &["yml", "yaml", "json"];

/// Where a discovered recipe file came from — `Workspace` shadows `Global`
/// when both declare the same `name` (see [`discover_recipes`]).
#[derive(serde::Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipeSource {
    Workspace,
    Global,
}

/// One recipe file found on disk — `recipe`/`error` are mutually exclusive
/// (a parse/validation failure still surfaces the file with `error` set
/// rather than silently dropping it, so `recipes_list`/`task list` can show
/// "this recipe is broken" instead of just omitting it).
#[derive(serde::Serialize, Clone, Debug)]
pub struct DiscoveredRecipe {
    pub path: PathBuf,
    pub source: RecipeSource,
    pub recipe: Option<Recipe>,
    pub error: Option<String>,
}

fn scan_recipe_dir(dir: &Path, source: RecipeSource) -> Vec<DiscoveredRecipe> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !RECIPE_EXTENSIONS
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) => {
                out.push(DiscoveredRecipe {
                    path,
                    source: source.clone(),
                    recipe: None,
                    error: Some(format!("Failed to read recipe: {error}")),
                });
                continue;
            }
        };
        match parse_recipe(&content, ext) {
            Ok(recipe) => out.push(DiscoveredRecipe {
                path,
                source: source.clone(),
                recipe: Some(recipe),
                error: None,
            }),
            Err(e) => out.push(DiscoveredRecipe {
                path,
                source: source.clone(),
                recipe: None,
                error: Some(e),
            }),
        }
    }
    out.sort_by(|left, right| left.path.cmp(&right.path));
    out
}

fn recipe_shadow_keys(discovered: &DiscoveredRecipe) -> Vec<String> {
    if let Some(recipe) = &discovered.recipe {
        return vec![recipe.name.clone()];
    }
    discovered
        .path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(|stem| vec![stem.to_string()])
        .unwrap_or_default()
}

fn extend_unshadowed(
    visible: &mut Vec<DiscoveredRecipe>,
    seen_keys: &mut std::collections::HashSet<String>,
    discovered: Vec<DiscoveredRecipe>,
) {
    for item in discovered {
        let keys = recipe_shadow_keys(&item);
        let shadowed = keys.iter().any(|key| seen_keys.contains(key));
        seen_keys.extend(keys);
        if !shadowed {
            visible.push(item);
        }
    }
}

/// Discovers every recipe visible right now: workspace-local
/// `.littlemonkey/recipes/` (skipped entirely when `workspace_root` is
/// `None` — no workspace open) plus the global `<agent-home>/recipes/`
/// directory, with a workspace recipe shadowing a global one of the same
/// `name` (never both — the workspace copy wins, matching "local shadows
/// global" from the design doc).
pub fn discover_recipes(
    workspace_root: Option<&Path>,
    global_config_roots: &[PathBuf],
) -> Vec<DiscoveredRecipe> {
    let mut local = workspace_root
        .map(|root| {
            scan_recipe_dir(
                &root.join(".littlemonkey").join("recipes"),
                RecipeSource::Workspace,
            )
        })
        .unwrap_or_default();
    let mut global = Vec::new();
    let mut global_keys = std::collections::HashSet::new();
    for root in global_config_roots {
        extend_unshadowed(
            &mut global,
            &mut global_keys,
            scan_recipe_dir(&root.join("recipes"), RecipeSource::Global),
        );
    }

    let local_keys: std::collections::HashSet<String> =
        local.iter().flat_map(recipe_shadow_keys).collect();

    local.extend(global.into_iter().filter(|discovered| {
        recipe_shadow_keys(discovered)
            .iter()
            .all(|key| !local_keys.contains(key))
    }));
    local
}

/// Resolves `name_or_path`: a direct filesystem path to a recipe file if one
/// exists at that exact path, otherwise a bare recipe `name` looked up via
/// [`discover_recipes`]. Used by `recipes_read`; see [`resolve_recipe_with_path`]
/// for the variant `monkey-cli task run` needs (which also needs the file's
/// own directory, to resolve a recipe's `workspace: .` field against it).
pub fn resolve_recipe(
    name_or_path: &str,
    workspace_root: Option<&Path>,
    global_config_roots: &[PathBuf],
) -> Result<Recipe, String> {
    resolve_recipe_with_path(name_or_path, workspace_root, global_config_roots)
        .map(|(recipe, _path)| recipe)
}

/// Same resolution as [`resolve_recipe`], but also returns the file path the
/// recipe was loaded from.
pub fn resolve_recipe_with_path(
    name_or_path: &str,
    workspace_root: Option<&Path>,
    global_config_roots: &[PathBuf],
) -> Result<(Recipe, PathBuf), String> {
    let direct_path = Path::new(name_or_path);
    if direct_path.is_file() {
        let ext = direct_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("yml");
        let content = std::fs::read_to_string(direct_path)
            .map_err(|e| format!("Failed to read '{name_or_path}': {e}"))?;
        return Ok((parse_recipe(&content, ext)?, direct_path.to_path_buf()));
    }
    let discovered = discover_recipes(workspace_root, global_config_roots);
    if let Some((recipe, path)) = discovered.iter().find_map(|item| {
        item.recipe
            .as_ref()
            .filter(|recipe| recipe.name == name_or_path)
            .cloned()
            .map(|recipe| (recipe, item.path.clone()))
    }) {
        return Ok((recipe, path));
    }
    if let Some(broken) = discovered.iter().find(|item| {
        item.recipe.is_none()
            && item.path.file_stem().and_then(|stem| stem.to_str()) == Some(name_or_path)
    }) {
        return Err(format!(
            "Recipe '{}' failed to parse: {}",
            broken.path.display(),
            broken.error.as_deref().unwrap_or("unknown error")
        ));
    }
    Err(format!(
        "No recipe named '{name_or_path}' found (checked workspace .littlemonkey/recipes/ and the global recipes directory)"
    ))
}

fn validate_recipe_id(name: &str) -> Result<(), String> {
    if !is_valid_recipe_name(name) {
        return Err(format!("recipe name '{name}' must match [a-z0-9-]+"));
    }
    Ok(())
}

/// Saves `yaml_content` as `<global-config-root>/recipes/<name>.yml`, atomically
/// (temp file + rename, same pattern as `sessions.rs::save_to`) — always
/// into the GLOBAL directory: the desktop app's recipe library (design doc
/// slice 2) has no concept of "which workspace" a saved recipe belongs to,
/// unlike a hand-authored `.littlemonkey/recipes/` file committed to a repo.
pub fn save_recipe_impl(
    global_config_root: &Path,
    name: &str,
    yaml_content: &str,
) -> Result<Recipe, String> {
    let path = global_config_root
        .join("recipes")
        .join(format!("{name}.yml"));
    save_recipe_at_path(&path, name, yaml_content)
}

fn save_recipe_at_path(path: &Path, name: &str, content: &str) -> Result<Recipe, String> {
    validate_recipe_id(name)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            format!(
                "Recipe path '{}' has no supported extension",
                path.display()
            )
        })?;
    let recipe = parse_recipe(content, extension)?;
    if recipe.name != name {
        return Err(format!(
            "recipe content's name '{}' does not match the target '{name}'",
            recipe.name
        ));
    }
    let dir = path
        .parent()
        .ok_or_else(|| format!("Recipe path '{}' has no parent", path.display()))?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create recipes directory: {e}"))?;
    let tmp = dir.join(format!(".{name}-{}.tmp", uuid::Uuid::new_v4().simple()));
    if let Err(error) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Failed to write recipe: {error}"));
    }
    replace_recipe_file(&tmp, path)?;
    Ok(recipe)
}

fn replace_recipe_file(temporary: &Path, destination: &Path) -> Result<(), String> {
    match std::fs::rename(temporary, destination) {
        Ok(()) => Ok(()),
        Err(first_error) => {
            let metadata = match std::fs::symlink_metadata(destination) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let _ = std::fs::remove_file(temporary);
                    return Err(format!("Failed to finalize recipe: {first_error}"));
                }
                Err(error) => {
                    let _ = std::fs::remove_file(temporary);
                    return Err(format!(
                        "Failed to inspect recipe destination after {first_error}: {error}"
                    ));
                }
            };
            if metadata.is_dir() {
                let _ = std::fs::remove_file(temporary);
                return Err(format!(
                    "Recipe destination '{}' is a directory",
                    destination.display()
                ));
            }
            let parent = destination
                .parent()
                .ok_or_else(|| format!("Recipe path '{}' has no parent", destination.display()))?;
            let backup = parent.join(format!(".recipe-{}.bak", uuid::Uuid::new_v4().simple()));
            std::fs::rename(destination, &backup).map_err(|error| {
                let _ = std::fs::remove_file(temporary);
                format!("Failed to prepare recipe replacement after {first_error}: {error}")
            })?;
            if let Err(error) = std::fs::rename(temporary, destination) {
                let restore_error = std::fs::rename(&backup, destination).err();
                let _ = std::fs::remove_file(temporary);
                return Err(match restore_error {
                    Some(restore) => format!(
                        "Failed to finalize recipe: {error}; restoring the previous file also failed: {restore}"
                    ),
                    None => format!("Failed to finalize recipe: {error}"),
                });
            }
            cleanup_committed_recipe_backup(&backup, |path| std::fs::remove_file(path));
            Ok(())
        }
    }
}

fn cleanup_committed_recipe_backup(
    backup: &Path,
    remove: impl FnOnce(&Path) -> std::io::Result<()>,
) {
    // The destination already contains the new recipe. A leftover backup is
    // preferable to reporting a failure for a save that visibly succeeded.
    let _ = remove(backup);
}

/// Deletes `<global-config-root>/recipes/<name>.yml` — a no-op success (not an error)
/// when it's already gone, same "delete is idempotent" convention as every
/// other per-item store in this codebase.
pub fn delete_recipe_impl(global_config_root: &Path, name: &str) -> Result<(), String> {
    validate_recipe_id(name)?;
    let path = global_config_root
        .join("recipes")
        .join(format!("{name}.yml"));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Failed to delete recipe: {e}")),
    }
}

/// Deletes every global file whose parsed recipe declares `name`, across all
/// compatibility roots and supported extensions. Workspace recipes are never
/// touched by a global Settings deletion.
pub fn delete_global_recipe_impl(
    global_config_roots: &[PathBuf],
    name: &str,
) -> Result<(), String> {
    delete_global_recipe_with(global_config_roots, name, |path| std::fs::remove_file(path))
}

fn delete_global_recipe_with(
    global_config_roots: &[PathBuf],
    name: &str,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    validate_recipe_id(name)?;
    for root in global_config_roots.iter().rev() {
        for discovered in scan_recipe_dir(&root.join("recipes"), RecipeSource::Global)
            .into_iter()
            .rev()
        {
            if discovered
                .recipe
                .as_ref()
                .map(|recipe| recipe.name.as_str())
                == Some(name)
            {
                match remove(&discovered.path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(format!(
                            "Failed to delete recipe '{}': {error}",
                            discovered.path.display()
                        ))
                    }
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tauri commands — thin wrappers resolving `AppHandle`/`AppState` down to the
// plain paths every `*_impl`/free function above actually needs.
// ---------------------------------------------------------------------------

pub fn global_config_roots() -> Result<Vec<PathBuf>, String> {
    Ok(app_paths::agent_config_roots()?.ordered())
}

fn select_global_config_write_path(roots: &[PathBuf], name: &str) -> Result<PathBuf, String> {
    let preferred = roots
        .first()
        .cloned()
        .ok_or_else(|| "Could not resolve the global recipes directory".to_string())?;
    if let Some(discovered) = discover_recipes(None, roots)
        .into_iter()
        .find(|discovered| {
            discovered
                .recipe
                .as_ref()
                .map(|recipe| recipe.name.as_str())
                == Some(name)
                || (discovered.recipe.is_none()
                    && discovered.path.file_stem().and_then(|stem| stem.to_str()) == Some(name))
        })
    {
        return Ok(discovered.path);
    }
    Ok(preferred.join("recipes").join(format!("{name}.yml")))
}

fn save_global_recipe_impl(roots: &[PathBuf], name: &str, content: &str) -> Result<Recipe, String> {
    let path = select_global_config_write_path(roots, name)?;
    save_recipe_at_path(&path, name, content)
}

#[tauri::command]
pub fn recipes_list(
    _app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
) -> Result<Vec<DiscoveredRecipe>, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    Ok(discover_recipes(
        workspace_root.as_deref(),
        &global_config_roots()?,
    ))
}

#[tauri::command]
pub fn recipes_read(
    _app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
) -> Result<Recipe, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    resolve_recipe(
        &name_or_path,
        workspace_root.as_deref(),
        &global_config_roots()?,
    )
}

/// Reads a recipe's raw (unparsed) file content — the Settings > Tasks
/// editor's "Edit" action needs the original YAML text to edit, not the
/// parsed `Recipe` `recipes_read` returns. Resolution is otherwise
/// identical to `recipes_read`; `tool_read_file` can't be reused here since
/// it's sandboxed to workspace roots and a global recipe lives in an
/// agent-home or compatible legacy directory, outside all of them.
#[tauri::command]
pub fn recipes_read_raw(
    _app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
) -> Result<String, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    let (_recipe, path) = resolve_recipe_with_path(
        &name_or_path,
        workspace_root.as_deref(),
        &global_config_roots()?,
    )?;
    std::fs::read_to_string(&path).map_err(|e| format!("Failed to read '{}': {e}", path.display()))
}

/// Resolves `name_or_path` and renders its prompt/system with `overrides` —
/// the one place `{{param}}` substitution happens for the desktop app's "Run
/// now" (`recipeRunner.ts`), so there is exactly one implementation shared
/// with `monkey-cli task run`, not two independently maintained ones.
#[tauri::command]
pub fn recipes_render(
    _app: tauri::AppHandle,
    state: tauri::State<'_, crate::AppState>,
    name_or_path: String,
    overrides: HashMap<String, String>,
) -> Result<RenderedRecipe, String> {
    let workspace_root = crate::workspace::primary_root_canon(state.inner()).ok();
    let recipe = resolve_recipe(
        &name_or_path,
        workspace_root.as_deref(),
        &global_config_roots()?,
    )?;
    render_recipe(&recipe, &overrides)
}

/// Emitted after a successful `recipes_save`/`recipes_delete`, with the
/// acting window's label as payload — same cross-window sync mechanism as
/// `sessions.rs`/`prompts.rs`: another open window re-lists on this instead
/// of polling, and ignores its own echo by comparing the payload to its own
/// label.
pub const RECIPES_CHANGED_EVENT: &str = "recipes://changed";

#[tauri::command]
pub fn recipes_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    name: String,
    content: String,
) -> Result<Recipe, String> {
    let roots = app_paths::ensure_agent_config_roots()?.ordered();
    let recipe = save_global_recipe_impl(&roots, &name, &content)?;
    let _ = app.emit(RECIPES_CHANGED_EVENT, window.label());
    Ok(recipe)
}

#[tauri::command]
pub fn recipes_delete(
    app: tauri::AppHandle,
    window: tauri::Window,
    name: String,
) -> Result<(), String> {
    let roots = app_paths::ensure_agent_config_roots()?.ordered();
    delete_global_recipe_impl(&roots, &name)?;
    let _ = app.emit(RECIPES_CHANGED_EVENT, window.label());
    Ok(())
}

/// Validates recipe content without saving it — the editor's live-validate
/// affordance (design doc slice 2). Extension-sniffs the same way
/// [`parse_recipe`] does, defaulting to YAML since the editor is a plain
/// YAML textarea.
#[tauri::command]
pub fn recipes_validate(content: String, extension: Option<String>) -> Result<Recipe, String> {
    parse_recipe(&content, extension.as_deref().unwrap_or("yml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_target() -> RecipeTarget {
        RecipeTarget {
            provider: Some("openrouter".to_string()),
            model: Some("anthropic/claude-sonnet".to_string()),
            ollama: None,
            local_url: None,
            managed_model: None,
        }
    }

    fn valid_recipe() -> Recipe {
        Recipe {
            version: 1,
            name: "nightly-deps-audit".to_string(),
            description: Some("Audit dependencies".to_string()),
            target: valid_target(),
            workspace: None,
            permission_mode: "acceptEdits".to_string(),
            system: None,
            prompt: "Check {{manifest}} for outdated deps.".to_string(),
            params: HashMap::from([("manifest".to_string(), Some("package.json".to_string()))]),
            max_iterations: None,
            timeout_seconds: None,
            output: RecipeOutput::default(),
            channel_send: None,
            desktop_turn: None,
            placed_run: None,
            autonomous_task: None,
        }
    }

    #[test]
    fn autonomous_snapshot_requires_bounded_scopes_and_a_valid_owner_lease() {
        let mut recipe = valid_recipe();
        recipe.autonomous_task = Some(AutonomousTaskSnapshot {
            schema_version: AUTONOMOUS_TASK_RECIPE_SCHEMA_VERSION,
            task_id: "task-1".to_string(),
            objective: "update the parser".to_string(),
            source: "cli".to_string(),
            relevant_files: vec!["src/parser.rs".to_string()],
            current_workspace_revision: "revision-1".to_string(),
            max_repair_rounds: 2,
            max_workers: 4,
            guidance: Vec::new(),
            delivery_intent: Some("leave_worktree".to_string()),
            execution_owner: Some(AutonomousTaskOwnerSnapshot {
                kind: "daemon".to_string(),
                instance_id: "daemon-1".to_string(),
                lease_epoch: 1,
                lease_expires_at_ms: 10,
            }),
            task_snapshot: None,
            completed_nodes: Vec::new(),
            next_node_id: Some("plan".to_string()),
        });
        validate_recipe(&recipe).expect("valid autonomous snapshot");
        recipe.autonomous_task.as_mut().unwrap().relevant_files = vec!["../secret".to_string()];
        let error = validate_recipe(&recipe).unwrap_err();
        assert!(error.contains("escapes the workspace"));
    }

    /// **The policy really does survive the trip** (roadmap K17 S3).
    ///
    /// The node writes this recipe to disk and the executing child parses it
    /// back, so the round trip is the actual mechanism, not a stand-in for one.
    /// The egress allowlist and the budgets are the two fields a recipe has
    /// nowhere else to put — re-deriving either on the node would silently swap
    /// the submitter's policy for this machine's defaults, which is the failure
    /// the whole slice exists to prevent.
    #[test]
    fn a_placed_runs_policy_and_budgets_survive_the_recipe_round_trip() {
        let spec = crate::node_placement::tests_support::placement_spec("run:placed");
        let mut spec = spec;
        spec.permission_policy.egress_allowlist = Some(crate::run_protocol::EgressAllowlist {
            hosts: vec!["api.example.com".to_string()],
            ports: vec![443],
            protocols: vec!["https".to_string()],
        });
        spec.budgets.max_output_tokens = 4_321;

        let mut recipe = valid_recipe();
        recipe.placed_run = Some(crate::node_placement::PlacedRunSnapshot::from_spec(&spec));
        validate_recipe(&recipe).expect("a placed recipe must validate");

        let written = serde_json::to_string(&recipe).expect("the node writes JSON");
        let parsed = parse_recipe(&written, "json").expect("the child parses it back");
        let placed = parsed.placed_run.expect("the placement snapshot survives");
        assert_eq!(
            placed
                .permission_policy
                .egress_allowlist
                .as_ref()
                .map(|list| list.hosts.clone()),
            Some(vec!["api.example.com".to_string()]),
            "the travelled allowlist must reach the executing process"
        );
        assert_eq!(placed.budgets.max_output_tokens, 4_321);
        assert_eq!(placed.submitted_run_id, "run:placed");
    }

    /// Both snapshots freeze the same four fields, so a recipe carrying both
    /// would leave "which wins" to the order of two fallback chains in
    /// `task.rs`. Refused at parse time so that question never arises.
    #[test]
    fn a_recipe_cannot_be_both_a_desktop_turn_and_a_placed_run() {
        let spec = crate::node_placement::tests_support::placement_spec("run:placed");
        let mut recipe = desktop_recipe();
        validate_recipe(&recipe).expect("the desktop half alone is valid");
        recipe.placed_run = Some(crate::node_placement::PlacedRunSnapshot::from_spec(&spec));
        let error = validate_recipe(&recipe).unwrap_err();
        assert!(
            error.contains("desktop turn") && error.contains("placed run"),
            "the refusal must name both: {error}"
        );
    }

    fn unknown_capability() -> crate::run_protocol::CapabilityAssessment {
        crate::run_protocol::CapabilityAssessment {
            state: crate::run_protocol::CapabilityState::Unknown,
            evidence: "test snapshot".to_string(),
        }
    }

    fn test_capabilities() -> crate::run_protocol::ModelCapabilitiesSnapshot {
        crate::run_protocol::ModelCapabilitiesSnapshot {
            tool_calling: unknown_capability(),
            vision: unknown_capability(),
            embeddings: unknown_capability(),
            structured_output: unknown_capability(),
            image_generation: unknown_capability(),
            audio: unknown_capability(),
            runtime_lifecycle: unknown_capability(),
            fim: unknown_capability(),
            code_completion: unknown_capability(),
            inline_edit: unknown_capability(),
            fim_metadata: None,
        }
    }

    fn desktop_recipe() -> Recipe {
        let mut recipe = valid_recipe();
        recipe.workspace = Some("/workspace/project".to_string());
        recipe.system = Some("frozen desktop system".to_string());
        let content = "exact attachment bytes".to_string();
        recipe.desktop_turn = Some(DesktopTurnSnapshot {
            schema_version: DESKTOP_TURN_SCHEMA_VERSION,
            session_id: "session-one".to_string(),
            turn_id: "turn-one".to_string(),
            submitted_at_ms: 1,
            execution_base_url: None,
            history: vec![serde_json::json!({"role":"user","content":"inspect"})],
            target: crate::run_protocol::ModelTargetSnapshot::Provider {
                target_id: "target-openrouter".to_string(),
                label: "OpenRouter test".to_string(),
                provider_id: "openrouter".to_string(),
                endpoint: "https://openrouter.ai/api/v1".to_string(),
                model: "anthropic/claude-sonnet".to_string(),
                credential_ref_id: "credential-openrouter".to_string(),
                capabilities: test_capabilities(),
            },
            workspace: Some(crate::run_protocol::WorkspaceContext {
                workspace_id: "workspace-test".to_string(),
                primary_root_id: "root-primary".to_string(),
                roots: vec![crate::run_protocol::RootGrant {
                    root_id: "root-primary".to_string(),
                    canonical_path: "/workspace/project".to_string(),
                    access: crate::run_protocol::RootAccess::ReadWrite,
                    allow_symlinks_within_root: false,
                }],
                repository_policy: None,
            }),
            execution_roots: vec![DesktopWorkspaceRootSnapshot {
                root_id: "root-primary".to_string(),
                canonical_path: "/workspace/project".to_string(),
                label: "project".to_string(),
                is_primary: true,
            }],
            permission_policy: crate::run_protocol::PermissionPolicySnapshot {
                mode: crate::run_protocol::PermissionMode::AcceptEdits,
                unattended: true,
                approval_timeout_ms: 60_000,
                default_tool_decision: crate::run_protocol::ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: false,
                allow_external_mutations: false,
                egress_allowlist: None,
                channel_send: None,
            },
            generation: DesktopGenerationSettingsSnapshot {
                temperature: None,
                top_p: None,
                seed: None,
                stop: Vec::new(),
                num_ctx: None,
                num_predict: None,
                format: None,
                think: None,
                hide_thinking: false,
                keep_alive: None,
                effort: Some("high".to_string()),
            },
            tool_profile: DesktopToolProfileSnapshot {
                memory_enabled: true,
                web_tools_enabled: false,
                verify_enabled: true,
                verify_max_rounds: 2,
                subagents_enabled: false,
            },
            mcp_servers: Vec::new(),
            attached_stack_ids: vec!["stack-one".to_string()],
            attached_stack_names: vec!["Docs".to_string()],
            attachments: vec![DesktopAttachmentSnapshot {
                path: "/workspace/project/input.txt".to_string(),
                kind: "file".to_string(),
                media_type: "text/plain".to_string(),
                content_sha256: sha256_hex(content.as_bytes()),
                size_bytes: content.len() as u64,
                content,
            }],
            workspace_mutation_required: true,
        });
        recipe
    }

    #[test]
    fn desktop_snapshot_rejects_attachment_tampering_and_scope_drift() {
        let recipe = desktop_recipe();
        validate_recipe(&recipe).unwrap();

        let mut tampered = recipe.clone();
        tampered.desktop_turn.as_mut().unwrap().attachments[0]
            .content
            .push_str(" changed");
        assert!(validate_recipe(&tampered)
            .unwrap_err()
            .contains("content digest"));

        let mut mismatched_root = recipe.clone();
        mismatched_root
            .desktop_turn
            .as_mut()
            .unwrap()
            .execution_roots[0]
            .canonical_path = "/workspace/other".to_string();
        assert!(validate_recipe(&mismatched_root)
            .unwrap_err()
            .contains("differs from its workspace grant"));

        let mut hostile_origin = recipe;
        hostile_origin
            .desktop_turn
            .as_mut()
            .unwrap()
            .execution_base_url = Some("https://attacker.invalid".to_string());
        assert!(validate_recipe(&hostile_origin)
            .unwrap_err()
            .contains("execution origin"));
    }

    #[test]
    fn desktop_snapshot_accepts_chat_only_without_workspace() {
        let mut recipe = desktop_recipe();
        recipe.workspace = None;
        let snapshot = recipe.desktop_turn.as_mut().unwrap();
        snapshot.workspace = None;
        snapshot.execution_roots.clear();
        snapshot.workspace_mutation_required = false;
        validate_recipe(&recipe).expect("a chat-only desktop turn must validate");
    }

    #[test]
    fn desktop_snapshot_rejects_tool_profile_and_selection_tampering() {
        let mut recipe = desktop_recipe();
        recipe
            .desktop_turn
            .as_mut()
            .unwrap()
            .tool_profile
            .web_tools_enabled = true;
        assert!(validate_recipe(&recipe)
            .unwrap_err()
            .contains("network permission"));

        let mut duplicate_stack = desktop_recipe();
        duplicate_stack
            .desktop_turn
            .as_mut()
            .unwrap()
            .attached_stack_ids
            .push("stack-one".to_string());
        assert!(validate_recipe(&duplicate_stack)
            .unwrap_err()
            .contains("duplicated"));

        let mut bad_rounds = desktop_recipe();
        bad_rounds
            .desktop_turn
            .as_mut()
            .unwrap()
            .tool_profile
            .verify_max_rounds = 4;
        assert!(validate_recipe(&bad_rounds)
            .unwrap_err()
            .contains("verify_max_rounds"));

        let mut changed_model = desktop_recipe();
        changed_model.target.model = Some("different-model".to_string());
        assert!(validate_recipe(&changed_model)
            .unwrap_err()
            .contains("frozen model target"));
    }

    #[test]
    fn normalized_mcp_digest_is_order_stable_and_secret_free() {
        use crate::mcp::{McpServerEntry, McpTransport};
        let mut entry = McpServerEntry {
            id: "docs".to_string(),
            label: "Docs".to_string(),
            transport: McpTransport::Stdio {
                command: "docs-server".to_string(),
                args: vec!["--safe".to_string()],
                env: std::collections::BTreeMap::from([(
                    "TOKEN".to_string(),
                    "keychain-local".to_string(),
                )]),
            },
            enabled: true,
            tool_allowlist: Some(vec!["search".to_string(), "read".to_string()]),
            timeout_secs: Some(30),
        };
        let first = mcp_server_config_digest(&entry).unwrap();
        assert_eq!(
            first,
            "df5d5a0b8e06cffe7f147abe9e439633fb80c71bd4a831386fd6406dc1b2bf20"
        );
        entry.tool_allowlist = Some(vec!["read".to_string(), "search".to_string()]);
        assert_eq!(mcp_server_config_digest(&entry).unwrap(), first);
        assert!(!first.contains("keychain-local"));
        entry.timeout_secs = Some(31);
        assert_ne!(mcp_server_config_digest(&entry).unwrap(), first);
    }

    // --- RecipeTarget::validate ---

    #[test]
    fn target_rejects_nothing_set() {
        let t = RecipeTarget::default();
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_rejects_more_than_one_set() {
        let t = RecipeTarget {
            provider: Some("openrouter".to_string()),
            model: Some("x".to_string()),
            ollama: Some("qwen2.5:14b".to_string()),
            local_url: None,
            managed_model: None,
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_rejects_provider_without_model() {
        let t = RecipeTarget {
            provider: Some("openrouter".to_string()),
            model: None,
            ollama: None,
            local_url: None,
            managed_model: None,
        };
        let err = t.validate().unwrap_err();
        assert!(err.contains("model"));
    }

    #[test]
    fn target_accepts_ollama_alone() {
        let t = RecipeTarget {
            provider: None,
            model: None,
            ollama: Some("qwen2.5:14b".to_string()),
            local_url: None,
            managed_model: None,
        };
        assert!(t.validate().is_ok());
    }

    /// The fourth option, and why it is one: `managed_model` names a model this
    /// machine has installed rather than an origin that is already listening,
    /// because the managed runtime is started on a fresh loopback port for the
    /// life of the run. It is still mutually exclusive with the other three.
    #[test]
    fn target_accepts_a_managed_model_alone_and_never_beside_another() {
        let managed = RecipeTarget {
            provider: None,
            model: None,
            ollama: None,
            local_url: None,
            managed_model: Some("qwen3-8b".to_string()),
        };
        assert!(managed.validate().is_ok());

        let both = RecipeTarget {
            local_url: Some("http://127.0.0.1:8090".to_string()),
            ..managed.clone()
        };
        assert!(both.validate().is_err());

        let empty = RecipeTarget {
            managed_model: Some("   ".to_string()),
            ..managed
        };
        assert!(empty.validate().unwrap_err().contains("managed_model"));
    }

    #[test]
    fn target_accepts_local_url_alone() {
        let t = RecipeTarget {
            provider: None,
            model: None,
            ollama: None,
            local_url: Some("http://127.0.0.1:8090".to_string()),
            managed_model: None,
        };
        assert!(t.validate().is_ok());
    }

    /// Shared with `recipeStore.test.ts`'s canonical-fixture test — a single
    /// fixture read by both a Rust unit test and a vitest test, not two
    /// independently hand-typed literals, is what actually pins the
    /// TS<->Rust schema against drift (ROADMAP.md §3 item 6). Recipes are
    /// the schema most likely to be hand-edited by users (YAML files
    /// authored outside either language), which makes this the fixture
    /// pair with the most to protect. Exercises both `Option<T>` branches
    /// (`workspace`/`system`/`max_iterations` absent, `description`/
    /// `timeout_seconds` present) alongside `#[serde(default)]` leniency.
    const CANONICAL_RECIPE_JSON: &str = include_str!("../fixtures/recipe.canonical.json");

    #[test]
    fn recipe_deserializes_canonical_fixture() {
        let recipe: Recipe = serde_json::from_str(CANONICAL_RECIPE_JSON).unwrap();
        assert_eq!(recipe.version, 1);
        assert_eq!(recipe.name, "nightly-deps-audit");
        assert_eq!(
            recipe.description.as_deref(),
            Some("Audit dependencies for known vulnerabilities and file a report")
        );
        assert_eq!(recipe.target.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            recipe.target.model.as_deref(),
            Some("anthropic/claude-sonnet")
        );
        assert_eq!(recipe.target.ollama, None);
        assert_eq!(recipe.target.local_url, None);
        assert_eq!(recipe.workspace, None);
        assert_eq!(recipe.permission_mode, "acceptEdits");
        assert_eq!(recipe.system, None);
        assert_eq!(
            recipe.prompt,
            "Check {{manifest}} for outdated or vulnerable dependencies and summarize findings."
        );
        assert_eq!(
            recipe.params.get("manifest"),
            Some(&Some("package.json".to_string()))
        );
        assert_eq!(recipe.max_iterations, None);
        assert_eq!(recipe.timeout_seconds, Some(900));
        assert!(!recipe.output.json);
        // The fixture is a well-formed recipe, not just a well-formed shape.
        assert!(validate_recipe(&recipe).is_ok());
    }

    // --- validate_recipe ---

    #[test]
    fn validate_recipe_accepts_a_well_formed_recipe() {
        assert!(validate_recipe(&valid_recipe()).is_ok());
    }

    #[test]
    fn validate_recipe_rejects_unsupported_version() {
        let mut r = valid_recipe();
        r.version = 2;
        assert!(validate_recipe(&r).unwrap_err().contains("version"));
    }

    #[test]
    fn validate_recipe_rejects_a_bad_name() {
        for bad in ["Has Spaces", "UPPER", "trailing_underscore_", "has/slash"] {
            let mut r = valid_recipe();
            r.name = bad.to_string();
            assert!(
                validate_recipe(&r).is_err(),
                "expected '{bad}' to be rejected"
            );
        }
    }

    #[test]
    fn validate_recipe_rejects_an_invalid_permission_mode() {
        let mut r = valid_recipe();
        r.permission_mode = "yolo".to_string();
        assert!(validate_recipe(&r).unwrap_err().contains("permission_mode"));
    }

    #[test]
    fn validate_recipe_accepts_every_real_permission_mode_except_bypass() {
        for mode in crate::permissions::VALID_MODES {
            let mut r = valid_recipe();
            r.permission_mode = mode.to_string();
            if *mode == "bypass" {
                assert!(
                    validate_recipe(&r).is_err(),
                    "expected 'bypass' to be rejected"
                );
            } else {
                assert!(
                    validate_recipe(&r).is_ok(),
                    "expected '{mode}' to be accepted"
                );
            }
        }
    }

    #[test]
    fn validate_recipe_rejects_bypass_permission_mode() {
        let mut r = valid_recipe();
        r.permission_mode = "bypass".to_string();
        let err = validate_recipe(&r).unwrap_err();
        assert!(err.contains("bypass"), "error should mention bypass: {err}");
        assert!(
            err.contains("unattended"),
            "error should explain why: {err}"
        );
    }

    #[test]
    fn validate_recipe_rejects_an_empty_prompt() {
        let mut r = valid_recipe();
        r.prompt = "   ".to_string();
        assert!(validate_recipe(&r).is_err());
    }

    #[test]
    fn validate_recipe_rejects_a_bad_target() {
        let mut r = valid_recipe();
        r.target = RecipeTarget::default();
        assert!(validate_recipe(&r).is_err());
    }

    // --- parse_recipe: YAML + JSON, matching the design doc's exact shape ---

    const YAML_RECIPE: &str = r#"
version: 1
name: nightly-deps-audit
description: Audit dependencies and write a report
target:
  provider: openrouter
  model: anthropic/claude-sonnet
permission_mode: acceptEdits
prompt: |
  Check {{manifest}} for outdated deps and summarize risks.
params:
  manifest: package.json
"#;

    #[test]
    fn parse_recipe_reads_the_design_docs_yaml_shape() {
        let recipe = parse_recipe(YAML_RECIPE, "yml").expect("should parse");
        assert_eq!(recipe.name, "nightly-deps-audit");
        assert_eq!(recipe.target.provider.as_deref(), Some("openrouter"));
        assert_eq!(
            recipe.target.model.as_deref(),
            Some("anthropic/claude-sonnet")
        );
        assert_eq!(recipe.permission_mode, "acceptEdits");
        assert!(recipe.prompt.contains("{{manifest}}"));
        assert_eq!(
            recipe.params.get("manifest"),
            Some(&Some("package.json".to_string()))
        );
    }

    #[test]
    fn parse_recipe_rejects_malformed_yaml() {
        assert!(parse_recipe("not: [valid: yaml", "yml").is_err());
    }

    #[test]
    fn parse_recipe_rejects_a_recipe_missing_permission_mode() {
        let no_mode = YAML_RECIPE.replace("permission_mode: acceptEdits\n", "");
        let err = parse_recipe(&no_mode, "yml").unwrap_err();
        // serde's own "missing field" error — the whole point of NOT giving
        // `permission_mode` a `#[serde(default)]`.
        assert!(
            err.to_lowercase().contains("permission_mode")
                || err.to_lowercase().contains("missing")
        );
    }

    #[test]
    fn parse_recipe_reads_json_when_extension_is_json() {
        let json = r#"{
            "version": 1,
            "name": "json-recipe",
            "target": {"ollama": "qwen2.5:14b"},
            "permission_mode": "manual",
            "prompt": "Do the thing."
        }"#;
        let recipe = parse_recipe(json, "json").expect("should parse");
        assert_eq!(recipe.name, "json-recipe");
        assert_eq!(recipe.target.ollama.as_deref(), Some("qwen2.5:14b"));
    }

    // --- substitute_params / resolve_param_values / render_recipe ---

    #[test]
    fn substitute_params_replaces_every_placeholder() {
        let values = HashMap::from([("name".to_string(), "world".to_string())]);
        assert_eq!(
            substitute_params("Hello {{name}}!", &values).unwrap(),
            "Hello world!"
        );
    }

    #[test]
    fn substitute_params_errors_on_unresolved_placeholders_listing_each_one() {
        let err = substitute_params("{{a}} and {{b}}", &HashMap::new()).unwrap_err();
        assert!(err.contains('a'));
        assert!(err.contains('b'));
    }

    #[test]
    fn resolve_param_values_rejects_an_unknown_override_key() {
        let recipe = valid_recipe();
        let overrides = HashMap::from([("typo_key".to_string(), "x".to_string())]);
        let err = resolve_param_values(&recipe, &overrides).unwrap_err();
        assert!(err.contains("typo_key"));
    }

    #[test]
    fn resolve_param_values_uses_the_override_over_the_default() {
        let recipe = valid_recipe();
        let overrides = HashMap::from([("manifest".to_string(), "pyproject.toml".to_string())]);
        let values = resolve_param_values(&recipe, &overrides).unwrap();
        assert_eq!(values.get("manifest"), Some(&"pyproject.toml".to_string()));
    }

    #[test]
    fn resolve_param_values_uses_the_default_when_no_override_given() {
        let recipe = valid_recipe();
        let values = resolve_param_values(&recipe, &HashMap::new()).unwrap();
        assert_eq!(values.get("manifest"), Some(&"package.json".to_string()));
    }

    #[test]
    fn resolve_param_values_errors_when_a_no_default_param_has_no_override() {
        let mut recipe = valid_recipe();
        recipe.params.insert("required_param".to_string(), None);
        let err = resolve_param_values(&recipe, &HashMap::new()).unwrap_err();
        assert!(err.contains("required_param"));
    }

    #[test]
    fn render_recipe_substitutes_both_prompt_and_system() {
        let mut recipe = valid_recipe();
        recipe.system = Some("You are auditing {{manifest}}.".to_string());
        let rendered = render_recipe(&recipe, &HashMap::new()).unwrap();
        assert!(rendered.prompt.contains("package.json"));
        assert_eq!(
            rendered.system.as_deref(),
            Some("You are auditing package.json.")
        );
    }

    // --- discovery / resolution / save / delete ---

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "little_monkey_recipes_test_{label}_{}_{n}_{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_recipe_file(dir: &Path, filename: &str, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let content = format!(
            "version: 1\nname: {name}\ntarget:\n  ollama: qwen2.5:14b\npermission_mode: manual\nprompt: do the thing\n"
        );
        std::fs::write(dir.join(filename), content).unwrap();
    }

    #[test]
    fn discover_recipes_finds_both_workspace_and_global_recipes() {
        let workspace = temp_dir("ws");
        let app_data = temp_dir("app");
        write_recipe_file(
            &workspace.join(".littlemonkey").join("recipes"),
            "local.yml",
            "local-recipe",
        );
        write_recipe_file(&app_data.join("recipes"), "global.yml", "global-recipe");

        let found = discover_recipes(Some(&workspace), std::slice::from_ref(&app_data));
        let names: Vec<&str> = found
            .iter()
            .filter_map(|d| d.recipe.as_ref().map(|r| r.name.as_str()))
            .collect();
        assert!(names.contains(&"local-recipe"));
        assert!(names.contains(&"global-recipe"));
    }

    #[test]
    fn discover_recipes_lets_a_workspace_recipe_shadow_a_global_one_with_the_same_name() {
        let workspace = temp_dir("ws-shadow");
        let app_data = temp_dir("app-shadow");
        write_recipe_file(
            &workspace.join(".littlemonkey").join("recipes"),
            "r.yml",
            "shared-name",
        );
        write_recipe_file(&app_data.join("recipes"), "r.yml", "shared-name");

        let found = discover_recipes(Some(&workspace), std::slice::from_ref(&app_data));
        let matches: Vec<&DiscoveredRecipe> = found
            .iter()
            .filter(|d| {
                d.recipe
                    .as_ref()
                    .map(|r| r.name == "shared-name")
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            matches.len(),
            1,
            "the global copy must be shadowed, not listed twice"
        );
        assert_eq!(matches[0].source, RecipeSource::Workspace);
    }

    #[test]
    fn authored_global_recipes_shadow_legacy_without_hiding_other_legacy_recipes() {
        let authored = temp_dir("authored-global");
        let legacy = temp_dir("legacy-global");
        write_recipe_file(&authored.join("recipes"), "shared.yml", "shared-name");
        write_recipe_file(&legacy.join("recipes"), "shared.yml", "shared-name");
        write_recipe_file(&legacy.join("recipes"), "legacy.yml", "legacy-only");

        let found = discover_recipes(None, &[authored.clone(), legacy]);
        let shared = found
            .iter()
            .filter(|discovered| {
                discovered
                    .recipe
                    .as_ref()
                    .map(|recipe| recipe.name.as_str())
                    == Some("shared-name")
            })
            .collect::<Vec<_>>();
        assert_eq!(shared.len(), 1);
        assert!(shared[0].path.starts_with(&authored));
        assert!(found.iter().any(|discovered| {
            discovered
                .recipe
                .as_ref()
                .map(|recipe| recipe.name.as_str())
                == Some("legacy-only")
        }));
    }

    #[test]
    fn a_valid_recipe_shadows_by_declared_name_not_filename() {
        let authored = temp_dir("mismatched-name-authored");
        let legacy = temp_dir("mismatched-name-legacy");
        write_recipe_file(&authored.join("recipes"), "old-name.yml", "new-name");
        write_recipe_file(&legacy.join("recipes"), "legacy.yml", "old-name");

        let found = discover_recipes(None, &[authored.clone(), legacy.clone()]);
        let names = found
            .iter()
            .filter_map(|item| item.recipe.as_ref().map(|recipe| recipe.name.as_str()))
            .collect::<Vec<_>>();

        assert!(names.contains(&"new-name"));
        assert!(names.contains(&"old-name"));
        let (recipe, path) =
            resolve_recipe_with_path("old-name", None, &[authored, legacy.clone()]).unwrap();
        assert_eq!(recipe.name, "old-name");
        assert!(path.starts_with(legacy));
    }

    #[test]
    fn malformed_authored_recipe_blocks_and_reports_a_stale_legacy_fallback() {
        let authored = temp_dir("malformed-authored");
        let legacy = temp_dir("malformed-legacy");
        std::fs::create_dir_all(authored.join("recipes")).unwrap();
        std::fs::write(authored.join("recipes/nightly.yml"), "not: [valid").unwrap();
        write_recipe_file(&legacy.join("recipes"), "old-name.yml", "nightly");

        let found = discover_recipes(None, &[authored.clone(), legacy.clone()]);

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, authored.join("recipes/nightly.yml"));
        assert!(found[0].recipe.is_none());
        let error = resolve_recipe("nightly", None, &[authored, legacy]).unwrap_err();
        assert!(error.contains("failed to parse"));
        assert!(error.contains("nightly.yml"));
    }

    #[test]
    fn saving_a_malformed_authored_recipe_repairs_it_instead_of_editing_legacy() {
        let authored = temp_dir("repair-malformed-authored");
        let legacy = temp_dir("repair-malformed-legacy");
        let authored_path = authored.join("recipes/nightly.yml");
        let legacy_path = legacy.join("recipes/old-name.yml");
        std::fs::create_dir_all(authored.join("recipes")).unwrap();
        std::fs::write(&authored_path, "not: [valid").unwrap();
        write_recipe_file(&legacy.join("recipes"), "old-name.yml", "nightly");
        let replacement = "version: 1\nname: nightly\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: repaired\n";

        save_global_recipe_impl(&[authored.clone(), legacy.clone()], "nightly", replacement)
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(&authored_path).unwrap(),
            replacement
        );
        assert!(std::fs::read_to_string(&legacy_path)
            .unwrap()
            .contains("prompt: do the thing"));
        let (_, path) = resolve_recipe_with_path("nightly", None, &[authored, legacy]).unwrap();
        assert_eq!(path, authored_path);
    }

    #[test]
    fn existing_recipe_edits_stay_at_their_origin_and_new_recipes_use_home() {
        let authored = temp_dir("write-authored");
        let legacy = temp_dir("write-legacy");
        write_recipe_file(&legacy.join("recipes"), "old.yml", "existing");
        let roots = [authored.clone(), legacy.clone()];

        assert_eq!(
            select_global_config_write_path(&roots, "existing").unwrap(),
            legacy.join("recipes/old.yml")
        );
        assert_eq!(
            select_global_config_write_path(&roots, "new-recipe").unwrap(),
            authored.join("recipes/new-recipe.yml")
        );
    }

    #[test]
    fn existing_recipe_save_updates_its_exact_file_without_a_duplicate() {
        let authored = temp_dir("save-existing-authored");
        let legacy = temp_dir("save-existing-legacy");
        let existing = legacy.join("recipes/custom-name.yaml");
        write_recipe_file(&legacy.join("recipes"), "custom-name.yaml", "existing");
        let updated = "version: 1\nname: existing\ntarget:\n  ollama: q2\npermission_mode: manual\nprompt: updated\n";

        save_global_recipe_impl(&[authored.clone(), legacy.clone()], "existing", updated).unwrap();

        assert_eq!(std::fs::read_to_string(existing).unwrap(), updated);
        assert!(!authored.join("recipes/existing.yml").exists());
        assert!(!legacy.join("recipes/existing.yml").exists());
    }

    #[test]
    fn discover_recipes_tolerates_no_workspace_open() {
        let app_data = temp_dir("app-no-ws");
        write_recipe_file(&app_data.join("recipes"), "g.yml", "global-only");
        let found = discover_recipes(None, std::slice::from_ref(&app_data));
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn discover_recipes_surfaces_a_malformed_file_with_an_error_instead_of_dropping_it() {
        let app_data = temp_dir("app-malformed");
        let dir = app_data.join("recipes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("broken.yml"), "not: [valid").unwrap();

        let found = discover_recipes(None, std::slice::from_ref(&app_data));
        assert_eq!(found.len(), 1);
        assert!(found[0].recipe.is_none());
        assert!(found[0].error.is_some());
    }

    #[test]
    fn resolve_recipe_finds_a_recipe_by_bare_name() {
        let app_data = temp_dir("app-resolve-name");
        write_recipe_file(&app_data.join("recipes"), "g.yml", "findable");
        let recipe = resolve_recipe("findable", None, std::slice::from_ref(&app_data)).unwrap();
        assert_eq!(recipe.name, "findable");
    }

    #[test]
    fn resolve_recipe_finds_a_recipe_by_direct_path() {
        let app_data = temp_dir("app-resolve-path");
        let dir = app_data.join("somewhere-else");
        write_recipe_file(&dir, "custom.yml", "path-recipe");
        let recipe = resolve_recipe(
            dir.join("custom.yml").to_str().unwrap(),
            None,
            std::slice::from_ref(&app_data),
        )
        .unwrap();
        assert_eq!(recipe.name, "path-recipe");
    }

    #[test]
    fn resolve_recipe_errors_with_a_clear_message_when_nothing_matches() {
        let app_data = temp_dir("app-resolve-missing");
        let err =
            resolve_recipe("does-not-exist", None, std::slice::from_ref(&app_data)).unwrap_err();
        assert!(err.contains("does-not-exist"));
    }

    #[test]
    fn save_then_read_back_roundtrips_and_writes_atomically() {
        let app_data = temp_dir("app-save");
        let yaml = "version: 1\nname: saved-recipe\ntarget:\n  ollama: qwen2.5:14b\npermission_mode: manual\nprompt: do it\n";
        let saved = save_recipe_impl(&app_data, "saved-recipe", yaml).unwrap();
        assert_eq!(saved.name, "saved-recipe");
        assert!(!app_data
            .join("recipes")
            .join("saved-recipe.yml.tmp")
            .exists());

        let reread = resolve_recipe("saved-recipe", None, std::slice::from_ref(&app_data)).unwrap();
        assert_eq!(reread.name, "saved-recipe");
    }

    #[test]
    fn post_commit_backup_cleanup_failure_does_not_turn_save_into_failure() {
        let backup = Path::new("old-recipe.bak");
        let mut attempted = false;

        cleanup_committed_recipe_backup(backup, |path| {
            attempted = true;
            assert_eq!(path, backup);
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "backup is locked",
            ))
        });

        assert!(attempted);
    }

    #[test]
    fn recipe_save_never_relocates_a_directory_at_the_destination() {
        let root = temp_dir("save-directory-destination");
        let destination = root.join("recipes/directory-recipe.yml");
        std::fs::create_dir_all(&destination).unwrap();
        let sentinel = destination.join("keep.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        let yaml = "version: 1\nname: directory-recipe\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n";

        let error = save_recipe_impl(&root, "directory-recipe", yaml).unwrap_err();

        assert!(error.contains("is a directory"));
        assert_eq!(std::fs::read_to_string(sentinel).unwrap(), "keep");
    }

    #[test]
    fn save_recipe_rejects_a_name_content_mismatch() {
        let app_data = temp_dir("app-save-mismatch");
        let yaml = "version: 1\nname: actual-name\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n";
        let err = save_recipe_impl(&app_data, "different-name", yaml).unwrap_err();
        assert!(err.contains("does not match"));
    }

    #[test]
    fn delete_recipe_removes_the_file_and_is_idempotent() {
        let app_data = temp_dir("app-delete");
        let yaml = "version: 1\nname: to-delete\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n";
        save_recipe_impl(&app_data, "to-delete", yaml).unwrap();
        assert!(app_data.join("recipes").join("to-delete.yml").exists());

        delete_recipe_impl(&app_data, "to-delete").unwrap();
        assert!(!app_data.join("recipes").join("to-delete.yml").exists());

        // Deleting again must not error.
        delete_recipe_impl(&app_data, "to-delete").unwrap();
    }

    #[test]
    fn global_delete_removes_declared_name_across_roots_and_extensions() {
        let authored = temp_dir("delete-authored");
        let legacy = temp_dir("delete-legacy");
        write_recipe_file(
            &authored.join("recipes"),
            "different-file.yaml",
            "to-delete",
        );
        std::fs::create_dir_all(legacy.join("recipes")).unwrap();
        std::fs::write(
            legacy.join("recipes/another-name.json"),
            r#"{"version":1,"name":"to-delete","target":{"ollama":"q"},"permission_mode":"manual","prompt":"x"}"#,
        )
        .unwrap();
        write_recipe_file(&legacy.join("recipes"), "keep.yml", "keep-me");
        write_recipe_file(&legacy.join("recipes"), "to-delete.yml", "different-recipe");

        delete_global_recipe_impl(&[authored.clone(), legacy.clone()], "to-delete").unwrap();

        assert!(!authored.join("recipes/different-file.yaml").exists());
        assert!(!legacy.join("recipes/another-name.json").exists());
        assert!(legacy.join("recipes/keep.yml").exists());
        assert!(legacy.join("recipes/to-delete.yml").exists());
    }

    #[test]
    fn global_delete_keeps_preferred_recipe_when_fallback_deletion_fails() {
        let authored = temp_dir("delete-failure-authored");
        let legacy = temp_dir("delete-failure-legacy");
        let authored_path = authored.join("recipes/preferred.yml");
        let legacy_path = legacy.join("recipes/fallback.yml");
        write_recipe_file(&authored.join("recipes"), "preferred.yml", "to-delete");
        write_recipe_file(&legacy.join("recipes"), "fallback.yml", "to-delete");
        let mut attempted = Vec::new();

        let error =
            delete_global_recipe_with(&[authored.clone(), legacy.clone()], "to-delete", |path| {
                attempted.push(path.to_path_buf());
                if path == legacy_path {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "locked legacy recipe",
                    ))
                } else {
                    std::fs::remove_file(path)
                }
            })
            .unwrap_err();

        assert!(error.contains("locked legacy recipe"));
        assert_eq!(attempted, vec![legacy_path.clone()]);
        assert!(legacy_path.exists());
        assert!(authored_path.exists());
    }

    #[test]
    fn global_delete_keeps_the_visible_same_root_recipe_when_hidden_deletion_fails() {
        let authored = temp_dir("delete-duplicate-failure");
        let visible_path = authored.join("recipes/a.yml");
        let hidden_path = authored.join("recipes/b.yml");
        write_recipe_file(&authored.join("recipes"), "a.yml", "to-delete");
        write_recipe_file(&authored.join("recipes"), "b.yml", "to-delete");
        let mut attempted = Vec::new();

        let error =
            delete_global_recipe_with(std::slice::from_ref(&authored), "to-delete", |path| {
                attempted.push(path.to_path_buf());
                if path == hidden_path {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "locked hidden recipe",
                    ))
                } else {
                    std::fs::remove_file(path)
                }
            })
            .unwrap_err();

        assert!(error.contains("locked hidden recipe"));
        assert_eq!(attempted, vec![hidden_path.clone()]);
        assert!(hidden_path.exists());
        assert!(visible_path.exists());
    }

    #[test]
    fn recipe_id_validation_rejects_path_traversal_style_names() {
        let app_data = temp_dir("app-traversal");
        assert!(save_recipe_impl(
            &app_data,
            "../evil",
            "version: 1\nname: x\ntarget:\n  ollama: q\npermission_mode: manual\nprompt: x\n"
        )
        .is_err());
        assert!(delete_recipe_impl(&app_data, "../evil").is_err());
    }
}

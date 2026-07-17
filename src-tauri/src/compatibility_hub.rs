//! Versioned OpenAI/Anthropic compatibility and secure-LAN policy core.
//!
//! This module intentionally exposes no workspace, file, shell, Git, MCP, or
//! agent-tool route. It translates only inference schemas and model lifecycle
//! authorization. LAN credentials are paired, scope-limited, rate-limited,
//! revocable, digest-only at rest, and stored in authenticated append-only
//! snapshots through injected entropy and state-protection boundaries.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use base64::Engine as _;
use rand::RngExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use url::{Host, Url};

pub const COMPATIBILITY_SCHEMA_VERSION: u32 = 1;
pub const CONFORMANCE_MANIFEST_VERSION: u32 = 1;
pub const LAN_SECURITY_STATE_VERSION: u32 = 1;

const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_MESSAGES: usize = 100_000;
const MAX_TOOLS: usize = 128;
const MAX_ID_BYTES: usize = 512;
const MAX_AUDIT_EVENTS: usize = 10_000;
const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PAIRING_ATTEMPTS: u8 = 5;
const TOKEN_PREFIX: &str = "lmk-lan-";
const STATE_FILE_PREFIX: &str = "security-state-";
const STATE_FILE_SUFFIX: &str = ".json";

pub type CompatibilityResult<T> = Result<T, CompatibilityError>;

#[derive(Debug)]
pub enum CompatibilityError {
    InvalidRequest {
        path: String,
        message: String,
    },
    Unsupported {
        feature: String,
        message: String,
    },
    Limit {
        name: &'static str,
        observed: u64,
        max: u64,
    },
    Unauthorized(String),
    Forbidden(String),
    RateLimited {
        retry_after_ms: u64,
    },
    Conflict(String),
    StateProtection(String),
    CorruptState(String),
    Entropy(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    LockPoisoned,
}

impl fmt::Display for CompatibilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest { path, message } => {
                write!(f, "invalid compatibility request at {path}: {message}")
            }
            Self::Unsupported { feature, message } => {
                write!(f, "unsupported compatibility feature {feature}: {message}")
            }
            Self::Limit {
                name,
                observed,
                max,
            } => write!(f, "{name} is {observed}, exceeding {max}"),
            Self::Unauthorized(message) => write!(f, "unauthorized: {message}"),
            Self::Forbidden(message) => write!(f, "forbidden: {message}"),
            Self::RateLimited { retry_after_ms } => {
                write!(f, "rate limit exceeded; retry after {retry_after_ms} ms")
            }
            Self::Conflict(message) => write!(f, "compatibility conflict: {message}"),
            Self::StateProtection(message) => {
                write!(f, "LAN state authentication failed: {message}")
            }
            Self::CorruptState(message) => write!(f, "corrupt LAN security state: {message}"),
            Self::Entropy(message) => write!(f, "secure entropy failed: {message}"),
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Json(error) => write!(f, "compatibility JSON error: {error}"),
            Self::LockPoisoned => write!(f, "compatibility state lock is poisoned"),
        }
    }
}

impl Error for CompatibilityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for CompatibilityError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompatibilityProtocol {
    OpenAiChatCompletions,
    OpenAiResponses,
    AnthropicMessages,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanonicalRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalContent {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalMessage {
    pub role: CanonicalRole,
    pub content: Vec<CanonicalContent>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    /// Whether the originating protocol requires exact schema adherence.
    /// Anthropic tools do not expose this switch and therefore use `false`.
    #[serde(default)]
    pub strict: bool,
}

/// Returns whether `name` matches a tool the request actually offered for
/// this turn. Local-runtime and remote-model output parsers must call this
/// before treating a materialized tool call as valid: a parsing bug, a
/// confused model, or an adversarial response can otherwise name a tool that
/// was never advertised, and a caller that trusts the name blindly would
/// execute something it never agreed to expose.
pub fn request_offers_tool(request: &CanonicalInferenceRequest, name: &str) -> bool {
    request.tools.iter().any(|tool| tool.name == name)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalInferenceRequest {
    pub schema_version: u32,
    pub protocol: CompatibilityProtocol,
    pub request_id: String,
    pub model: String,
    pub messages: Vec<CanonicalMessage>,
    pub tools: Vec<CanonicalToolDefinition>,
    pub max_output_tokens: u32,
    pub temperature: Option<f64>,
    pub stream: bool,
    pub response_schema: Option<Value>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CanonicalInferenceResponse {
    pub response_id: String,
    pub model: String,
    pub content: Vec<CanonicalContent>,
    pub finish_reason: String,
    pub usage: CanonicalUsage,
    pub created_at_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CanonicalStreamEvent {
    ResponseStart {
        response_id: String,
        model: String,
        created_at_seconds: u64,
    },
    TextStart {
        index: usize,
    },
    TextDelta {
        index: usize,
        text: String,
    },
    TextEnd {
        index: usize,
    },
    ToolCallStart {
        index: usize,
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        index: usize,
        call_id: String,
        json_delta: String,
    },
    ToolCallEnd {
        index: usize,
        call_id: String,
    },
    ResponseCompleted {
        response_id: String,
        finish_reason: String,
        usage: CanonicalUsage,
    },
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProtocolStreamFrame {
    pub event: Option<String>,
    pub data: String,
}

impl ProtocolStreamFrame {
    pub fn to_sse_bytes(&self) -> Vec<u8> {
        let mut output = String::new();
        if let Some(event) = &self.event {
            output.push_str("event: ");
            output.push_str(event);
            output.push('\n');
        }
        for line in self.data.lines() {
            output.push_str("data: ");
            output.push_str(line);
            output.push('\n');
        }
        if self.data.is_empty() {
            output.push_str("data: \n");
        }
        output.push('\n');
        output.into_bytes()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointConformance {
    pub protocol: CompatibilityProtocol,
    pub method: String,
    pub path: String,
    pub streaming: bool,
    pub tools: bool,
    pub structured_output: bool,
    pub images: bool,
    pub audio: bool,
    pub unsupported_fields_rejected: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompatibilityConformanceManifest {
    pub manifest_version: u32,
    pub compatibility_schema_version: u32,
    pub endpoints: Vec<EndpointConformance>,
    pub lifecycle_paths: BTreeMap<String, String>,
    pub workspace_tool_routes_exposed: bool,
}

pub fn compatibility_conformance_manifest() -> CompatibilityConformanceManifest {
    CompatibilityConformanceManifest {
        manifest_version: CONFORMANCE_MANIFEST_VERSION,
        compatibility_schema_version: COMPATIBILITY_SCHEMA_VERSION,
        endpoints: vec![
            EndpointConformance {
                protocol: CompatibilityProtocol::OpenAiChatCompletions,
                method: "POST".to_string(),
                path: "/v1/chat/completions".to_string(),
                streaming: true,
                tools: true,
                structured_output: true,
                images: false,
                audio: false,
                unsupported_fields_rejected: true,
            },
            EndpointConformance {
                protocol: CompatibilityProtocol::OpenAiResponses,
                method: "POST".to_string(),
                path: "/v1/responses".to_string(),
                streaming: true,
                tools: true,
                structured_output: true,
                images: false,
                audio: false,
                unsupported_fields_rejected: true,
            },
            EndpointConformance {
                protocol: CompatibilityProtocol::AnthropicMessages,
                method: "POST".to_string(),
                path: "/v1/messages".to_string(),
                streaming: true,
                tools: true,
                structured_output: false,
                images: false,
                audio: false,
                unsupported_fields_rejected: true,
            },
        ],
        lifecycle_paths: BTreeMap::from([
            ("discover".to_string(), "/v1/models".to_string()),
            ("download".to_string(), "/v1/models/download".to_string()),
            ("load".to_string(), "/v1/models/load".to_string()),
            ("unload".to_string(), "/v1/models/unload".to_string()),
            ("status".to_string(), "/v1/models/status".to_string()),
            ("delete".to_string(), "/v1/models/delete".to_string()),
        ]),
        workspace_tool_routes_exposed: false,
    }
}

pub fn translate_request(
    protocol: CompatibilityProtocol,
    request_id: &str,
    body: &[u8],
) -> CompatibilityResult<CanonicalInferenceRequest> {
    validate_id(request_id, "requestId")?;
    if body.len() > MAX_BODY_BYTES {
        return Err(limit(
            "compatibility request bytes",
            body.len() as u64,
            MAX_BODY_BYTES as u64,
        ));
    }
    let value: Value = serde_json::from_slice(body)?;
    let request = match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => translate_openai_chat(request_id, value)?,
        CompatibilityProtocol::OpenAiResponses => translate_openai_responses(request_id, value)?,
        CompatibilityProtocol::AnthropicMessages => {
            translate_anthropic_messages(request_id, value)?
        }
    };
    validate_canonical_request(&request)?;
    Ok(request)
}

fn translate_openai_chat(
    request_id: &str,
    value: Value,
) -> CompatibilityResult<CanonicalInferenceRequest> {
    let object = require_object(&value, "$", "request must be an object")?;
    reject_unknown(
        object,
        &[
            "model",
            "messages",
            "tools",
            "stream",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "response_format",
            "metadata",
        ],
        "$",
    )?;
    let model = required_string(object, "model", "$.model")?;
    let message_values = required_array(object, "messages", "$.messages")?;
    let mut messages = Vec::with_capacity(message_values.len());
    for (index, value) in message_values.iter().enumerate() {
        messages.push(parse_openai_message(
            value,
            &format!("$.messages[{index}]"),
        )?);
    }
    let tools = object
        .get("tools")
        .map(|value| parse_openai_tools(value, "$.tools"))
        .transpose()?
        .unwrap_or_default();
    let max_output_tokens = mutually_exclusive_token_limit(
        object.get("max_tokens"),
        object.get("max_completion_tokens"),
        4_096,
        "$",
    )?;
    let response_schema = object
        .get("response_format")
        .map(|value| parse_openai_response_format(value, "$.response_format"))
        .transpose()?
        .flatten();
    Ok(CanonicalInferenceRequest {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        protocol: CompatibilityProtocol::OpenAiChatCompletions,
        request_id: request_id.to_string(),
        model,
        messages,
        tools,
        max_output_tokens,
        temperature: optional_f64(object, "temperature", "$.temperature")?,
        stream: optional_bool(object, "stream", false, "$.stream")?,
        response_schema,
        metadata: object.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

fn parse_openai_message(value: &Value, path: &str) -> CompatibilityResult<CanonicalMessage> {
    let object = require_object(value, path, "message must be an object")?;
    reject_unknown(
        object,
        &["role", "content", "tool_calls", "tool_call_id"],
        path,
    )?;
    let role = required_string(object, "role", &format!("{path}.role"))?;
    let canonical_role = match role.as_str() {
        "system" | "developer" => CanonicalRole::System,
        "user" => CanonicalRole::User,
        "assistant" => CanonicalRole::Assistant,
        "tool" => CanonicalRole::Tool,
        _ => return Err(invalid(format!("{path}.role"), "unsupported role")),
    };
    let mut content = Vec::new();
    if let Some(value) = object.get("content") {
        match value {
            Value::Null if canonical_role == CanonicalRole::Assistant => {}
            Value::String(text) => content.push(CanonicalContent::Text { text: text.clone() }),
            Value::Array(blocks) => {
                for (index, block) in blocks.iter().enumerate() {
                    let block_path = format!("{path}.content[{index}]");
                    let block = require_object(block, &block_path, "content block must be object")?;
                    reject_unknown(block, &["type", "text"], &block_path)?;
                    if required_string(block, "type", &format!("{block_path}.type"))? != "text" {
                        return Err(unsupported(
                            "multimodal_content",
                            "only text content blocks are advertised",
                        ));
                    }
                    content.push(CanonicalContent::Text {
                        text: required_string(block, "text", &format!("{block_path}.text"))?,
                    });
                }
            }
            _ => {
                return Err(invalid(
                    format!("{path}.content"),
                    "must be string or text blocks",
                ))
            }
        }
    }
    if canonical_role == CanonicalRole::Tool {
        let tool_use_id = required_string(object, "tool_call_id", &format!("{path}.tool_call_id"))?;
        let text = content
            .iter()
            .filter_map(|block| match block {
                CanonicalContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        content = vec![CanonicalContent::ToolResult {
            tool_use_id,
            content: text,
            is_error: false,
        }];
    }
    if let Some(tool_calls) = object.get("tool_calls") {
        let calls = require_array(tool_calls, &format!("{path}.tool_calls"), "must be array")?;
        for (index, call) in calls.iter().enumerate() {
            let call_path = format!("{path}.tool_calls[{index}]");
            let call = require_object(call, &call_path, "tool call must be object")?;
            reject_unknown(call, &["id", "type", "function"], &call_path)?;
            if required_string(call, "type", &format!("{call_path}.type"))? != "function" {
                return Err(unsupported(
                    "tool_type",
                    "only function tools are supported",
                ));
            }
            let function = require_object(
                call.get("function")
                    .ok_or_else(|| invalid(&call_path, "missing function"))?,
                &format!("{call_path}.function"),
                "function must be object",
            )?;
            reject_unknown(
                function,
                &["name", "arguments"],
                &format!("{call_path}.function"),
            )?;
            let arguments = required_string(
                function,
                "arguments",
                &format!("{call_path}.function.arguments"),
            )?;
            let input = serde_json::from_str(&arguments).map_err(|error| {
                invalid(
                    format!("{call_path}.function.arguments"),
                    format!("must be JSON: {error}"),
                )
            })?;
            content.push(CanonicalContent::ToolUse {
                id: required_string(call, "id", &format!("{call_path}.id"))?,
                name: required_string(function, "name", &format!("{call_path}.function.name"))?,
                input,
            });
        }
    }
    if content.is_empty() {
        return Err(invalid(path, "message contains no supported content"));
    }
    Ok(CanonicalMessage {
        role: canonical_role,
        content,
    })
}

fn parse_openai_tools(
    value: &Value,
    path: &str,
) -> CompatibilityResult<Vec<CanonicalToolDefinition>> {
    let tools = require_array(value, path, "tools must be an array")?;
    let mut output = Vec::with_capacity(tools.len());
    for (index, tool) in tools.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let object = require_object(tool, &item_path, "tool must be object")?;
        reject_unknown(object, &["type", "function"], &item_path)?;
        if required_string(object, "type", &format!("{item_path}.type"))? != "function" {
            return Err(unsupported(
                "tool_type",
                "only function tools are supported",
            ));
        }
        let function = require_object(
            object
                .get("function")
                .ok_or_else(|| invalid(&item_path, "missing function"))?,
            &format!("{item_path}.function"),
            "function must be object",
        )?;
        reject_unknown(
            function,
            &["name", "description", "parameters", "strict"],
            &format!("{item_path}.function"),
        )?;
        output.push(CanonicalToolDefinition {
            name: required_string(function, "name", &format!("{item_path}.function.name"))?,
            description: optional_string(function, "description", "")?,
            input_schema: function
                .get("parameters")
                .cloned()
                .unwrap_or_else(|| json!({"type":"object"})),
            strict: optional_bool(
                function,
                "strict",
                false,
                &format!("{item_path}.function.strict"),
            )?,
        });
    }
    Ok(output)
}

fn translate_openai_responses(
    request_id: &str,
    value: Value,
) -> CompatibilityResult<CanonicalInferenceRequest> {
    let object = require_object(&value, "$", "request must be object")?;
    reject_unknown(
        object,
        &[
            "model",
            "input",
            "instructions",
            "tools",
            "stream",
            "max_output_tokens",
            "temperature",
            "text",
            "metadata",
        ],
        "$",
    )?;
    let mut messages = Vec::new();
    if let Some(instructions) = object.get("instructions") {
        messages.push(CanonicalMessage {
            role: CanonicalRole::System,
            content: vec![CanonicalContent::Text {
                text: require_string(instructions, "$.instructions")?,
            }],
        });
    }
    let input = object
        .get("input")
        .ok_or_else(|| invalid("$.input", "is required"))?;
    match input {
        Value::String(text) => messages.push(CanonicalMessage {
            role: CanonicalRole::User,
            content: vec![CanonicalContent::Text { text: text.clone() }],
        }),
        Value::Array(items) => {
            if items.is_empty() {
                return Err(invalid("$.input", "input-item array must not be empty"));
            }
            for (index, item) in items.iter().enumerate() {
                messages.push(parse_responses_input_item(
                    item,
                    &format!("$.input[{index}]"),
                )?);
            }
        }
        _ => return Err(invalid("$.input", "must be string or input-item array")),
    }
    let tools = object
        .get("tools")
        .map(|value| parse_responses_tools(value, "$.tools"))
        .transpose()?
        .unwrap_or_default();
    let response_schema = object
        .get("text")
        .map(|value| parse_responses_text_format(value, "$.text"))
        .transpose()?
        .flatten();
    Ok(CanonicalInferenceRequest {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        protocol: CompatibilityProtocol::OpenAiResponses,
        request_id: request_id.to_string(),
        model: required_string(object, "model", "$.model")?,
        messages,
        tools,
        max_output_tokens: optional_u32(object, "max_output_tokens", 4_096, "$.max_output_tokens")?,
        temperature: optional_f64(object, "temperature", "$.temperature")?,
        stream: optional_bool(object, "stream", false, "$.stream")?,
        response_schema,
        metadata: object.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

fn parse_responses_input_item(value: &Value, path: &str) -> CompatibilityResult<CanonicalMessage> {
    let object = require_object(value, path, "input item must be object")?;
    let item_type = required_string(object, "type", &format!("{path}.type"))?;
    match item_type.as_str() {
        "message" => {
            reject_unknown(object, &["type", "role", "content"], path)?;
            let role = match required_string(object, "role", &format!("{path}.role"))?.as_str() {
                "system" | "developer" => CanonicalRole::System,
                "user" => CanonicalRole::User,
                "assistant" => CanonicalRole::Assistant,
                _ => return Err(invalid(format!("{path}.role"), "unsupported role")),
            };
            let content = parse_responses_text_content(
                object
                    .get("content")
                    .ok_or_else(|| invalid(path, "missing content"))?,
                &format!("{path}.content"),
            )?;
            Ok(CanonicalMessage { role, content })
        }
        "function_call" => {
            reject_unknown(
                object,
                &["type", "call_id", "name", "arguments", "id", "status"],
                path,
            )?;
            let arguments = required_string(object, "arguments", &format!("{path}.arguments"))?;
            let input = serde_json::from_str(&arguments)
                .map_err(|error| invalid(format!("{path}.arguments"), error.to_string()))?;
            Ok(CanonicalMessage {
                role: CanonicalRole::Assistant,
                content: vec![CanonicalContent::ToolUse {
                    id: required_string(object, "call_id", &format!("{path}.call_id"))?,
                    name: required_string(object, "name", &format!("{path}.name"))?,
                    input,
                }],
            })
        }
        "function_call_output" => {
            reject_unknown(object, &["type", "call_id", "output", "id", "status"], path)?;
            Ok(CanonicalMessage {
                role: CanonicalRole::Tool,
                content: vec![CanonicalContent::ToolResult {
                    tool_use_id: required_string(object, "call_id", &format!("{path}.call_id"))?,
                    content: value_as_text(
                        object
                            .get("output")
                            .ok_or_else(|| invalid(path, "missing output"))?,
                        &format!("{path}.output"),
                    )?,
                    is_error: false,
                }],
            })
        }
        other => Err(unsupported(
            "responses_input_type",
            format!("input item type {other:?} is not advertised"),
        )),
    }
}

fn parse_responses_text_content(
    value: &Value,
    path: &str,
) -> CompatibilityResult<Vec<CanonicalContent>> {
    match value {
        Value::String(text) => Ok(vec![CanonicalContent::Text { text: text.clone() }]),
        Value::Array(blocks) => blocks
            .iter()
            .enumerate()
            .map(|(index, block)| {
                let block_path = format!("{path}[{index}]");
                let object = require_object(block, &block_path, "content block must be object")?;
                reject_unknown(object, &["type", "text"], &block_path)?;
                let kind = required_string(object, "type", &format!("{block_path}.type"))?;
                if !matches!(kind.as_str(), "input_text" | "output_text") {
                    return Err(unsupported(
                        "multimodal_content",
                        "only input_text/output_text blocks are advertised",
                    ));
                }
                Ok(CanonicalContent::Text {
                    text: required_string(object, "text", &format!("{block_path}.text"))?,
                })
            })
            .collect(),
        _ => Err(invalid(path, "must be string or text-block array")),
    }
}

fn parse_responses_tools(
    value: &Value,
    path: &str,
) -> CompatibilityResult<Vec<CanonicalToolDefinition>> {
    let tools = require_array(value, path, "tools must be array")?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let item_path = format!("{path}[{index}]");
            let object = require_object(tool, &item_path, "tool must be object")?;
            reject_unknown(
                object,
                &["type", "name", "description", "parameters", "strict"],
                &item_path,
            )?;
            if required_string(object, "type", &format!("{item_path}.type"))? != "function" {
                return Err(unsupported(
                    "tool_type",
                    "only function tools are supported",
                ));
            }
            Ok(CanonicalToolDefinition {
                name: required_string(object, "name", &format!("{item_path}.name"))?,
                description: optional_string(object, "description", "")?,
                input_schema: object
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type":"object"})),
                strict: optional_bool(object, "strict", false, &format!("{item_path}.strict"))?,
            })
        })
        .collect()
}

fn translate_anthropic_messages(
    request_id: &str,
    value: Value,
) -> CompatibilityResult<CanonicalInferenceRequest> {
    let object = require_object(&value, "$", "request must be object")?;
    reject_unknown(
        object,
        &[
            "model",
            "messages",
            "system",
            "tools",
            "stream",
            "max_tokens",
            "temperature",
            "metadata",
        ],
        "$",
    )?;
    let mut messages = Vec::new();
    if let Some(system) = object.get("system") {
        messages.push(CanonicalMessage {
            role: CanonicalRole::System,
            content: parse_anthropic_system(system, "$.system")?,
        });
    }
    let message_values = required_array(object, "messages", "$.messages")?;
    for (index, message) in message_values.iter().enumerate() {
        messages.push(parse_anthropic_message(
            message,
            &format!("$.messages[{index}]"),
        )?);
    }
    let tools = object
        .get("tools")
        .map(|value| parse_anthropic_tools(value, "$.tools"))
        .transpose()?
        .unwrap_or_default();
    Ok(CanonicalInferenceRequest {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        protocol: CompatibilityProtocol::AnthropicMessages,
        request_id: request_id.to_string(),
        model: required_string(object, "model", "$.model")?,
        messages,
        tools,
        max_output_tokens: required_u32(object, "max_tokens", "$.max_tokens")?,
        temperature: optional_f64(object, "temperature", "$.temperature")?,
        stream: optional_bool(object, "stream", false, "$.stream")?,
        response_schema: None,
        metadata: object.get("metadata").cloned().unwrap_or(Value::Null),
    })
}

fn parse_anthropic_system(value: &Value, path: &str) -> CompatibilityResult<Vec<CanonicalContent>> {
    match value {
        Value::String(text) => Ok(vec![CanonicalContent::Text { text: text.clone() }]),
        Value::Array(blocks) => blocks
            .iter()
            .enumerate()
            .map(|(index, block)| parse_anthropic_text_block(block, &format!("{path}[{index}]")))
            .collect(),
        _ => Err(invalid(path, "must be string or text blocks")),
    }
}

fn parse_anthropic_text_block(value: &Value, path: &str) -> CompatibilityResult<CanonicalContent> {
    let object = require_object(value, path, "block must be object")?;
    reject_unknown(object, &["type", "text"], path)?;
    if required_string(object, "type", &format!("{path}.type"))? != "text" {
        return Err(unsupported(
            "system_content",
            "only text blocks are supported",
        ));
    }
    Ok(CanonicalContent::Text {
        text: required_string(object, "text", &format!("{path}.text"))?,
    })
}

fn parse_anthropic_message(value: &Value, path: &str) -> CompatibilityResult<CanonicalMessage> {
    let object = require_object(value, path, "message must be object")?;
    reject_unknown(object, &["role", "content"], path)?;
    let role = match required_string(object, "role", &format!("{path}.role"))?.as_str() {
        "user" => CanonicalRole::User,
        "assistant" => CanonicalRole::Assistant,
        _ => return Err(invalid(format!("{path}.role"), "must be user or assistant")),
    };
    let value = object
        .get("content")
        .ok_or_else(|| invalid(path, "missing content"))?;
    let content = match value {
        Value::String(text) => vec![CanonicalContent::Text { text: text.clone() }],
        Value::Array(blocks) => {
            let mut content = Vec::with_capacity(blocks.len());
            for (index, block) in blocks.iter().enumerate() {
                let block_path = format!("{path}.content[{index}]");
                let object = require_object(block, &block_path, "block must be object")?;
                match required_string(object, "type", &format!("{block_path}.type"))?.as_str() {
                    "text" => content.push(parse_anthropic_text_block(block, &block_path)?),
                    "tool_use" => {
                        reject_unknown(object, &["type", "id", "name", "input"], &block_path)?;
                        content.push(CanonicalContent::ToolUse {
                            id: required_string(object, "id", &format!("{block_path}.id"))?,
                            name: required_string(object, "name", &format!("{block_path}.name"))?,
                            input: object.get("input").cloned().unwrap_or_else(|| json!({})),
                        });
                    }
                    "tool_result" => {
                        reject_unknown(
                            object,
                            &["type", "tool_use_id", "content", "is_error"],
                            &block_path,
                        )?;
                        content.push(CanonicalContent::ToolResult {
                            tool_use_id: required_string(
                                object,
                                "tool_use_id",
                                &format!("{block_path}.tool_use_id"),
                            )?,
                            content: value_as_text(
                                object
                                    .get("content")
                                    .ok_or_else(|| invalid(&block_path, "missing content"))?,
                                &format!("{block_path}.content"),
                            )?,
                            is_error: optional_bool(
                                object,
                                "is_error",
                                false,
                                &format!("{block_path}.is_error"),
                            )?,
                        });
                    }
                    _ => {
                        return Err(unsupported(
                            "multimodal_content",
                            "images/documents are not advertised by this compatibility subset",
                        ))
                    }
                }
            }
            content
        }
        _ => {
            return Err(invalid(
                format!("{path}.content"),
                "must be string or block array",
            ))
        }
    };
    Ok(CanonicalMessage { role, content })
}

fn parse_anthropic_tools(
    value: &Value,
    path: &str,
) -> CompatibilityResult<Vec<CanonicalToolDefinition>> {
    let tools = require_array(value, path, "tools must be array")?;
    tools
        .iter()
        .enumerate()
        .map(|(index, tool)| {
            let item_path = format!("{path}[{index}]");
            let object = require_object(tool, &item_path, "tool must be object")?;
            reject_unknown(object, &["name", "description", "input_schema"], &item_path)?;
            Ok(CanonicalToolDefinition {
                name: required_string(object, "name", &format!("{item_path}.name"))?,
                description: optional_string(object, "description", "")?,
                input_schema: object
                    .get("input_schema")
                    .cloned()
                    .ok_or_else(|| invalid(&item_path, "missing input_schema"))?,
                strict: false,
            })
        })
        .collect()
}

fn parse_openai_response_format(value: &Value, path: &str) -> CompatibilityResult<Option<Value>> {
    let object = require_object(value, path, "response_format must be object")?;
    let kind = required_string(object, "type", &format!("{path}.type"))?;
    match kind.as_str() {
        "text" => {
            reject_unknown(object, &["type"], path)?;
            Ok(None)
        }
        "json_object" => {
            reject_unknown(object, &["type"], path)?;
            Ok(Some(json!({"type":"object"})))
        }
        "json_schema" => {
            reject_unknown(object, &["type", "json_schema"], path)?;
            let json_schema = require_object(
                object
                    .get("json_schema")
                    .ok_or_else(|| invalid(path, "missing json_schema"))?,
                &format!("{path}.json_schema"),
                "json_schema must be object",
            )?;
            reject_unknown(
                json_schema,
                &["name", "description", "schema", "strict"],
                &format!("{path}.json_schema"),
            )?;
            let schema = json_schema
                .get("schema")
                .cloned()
                .ok_or_else(|| invalid(path, "missing schema"))?;
            ensure_object_schema(&schema, &format!("{path}.json_schema.schema"))?;
            Ok(Some(schema))
        }
        _ => Err(unsupported("response_format", "format is not advertised")),
    }
}

fn parse_responses_text_format(value: &Value, path: &str) -> CompatibilityResult<Option<Value>> {
    let object = require_object(value, path, "text must be object")?;
    reject_unknown(object, &["format"], path)?;
    let Some(format) = object.get("format") else {
        return Ok(None);
    };
    let format = require_object(format, &format!("{path}.format"), "format must be object")?;
    let kind = required_string(format, "type", &format!("{path}.format.type"))?;
    match kind.as_str() {
        "text" => Ok(None),
        "json_schema" => {
            reject_unknown(
                format,
                &["type", "name", "description", "schema", "strict"],
                &format!("{path}.format"),
            )?;
            let schema = format
                .get("schema")
                .cloned()
                .ok_or_else(|| invalid(path, "missing schema"))?;
            ensure_object_schema(&schema, &format!("{path}.format.schema"))?;
            Ok(Some(schema))
        }
        _ => Err(unsupported(
            "responses_text_format",
            "format is not advertised",
        )),
    }
}

pub fn encode_response(
    protocol: CompatibilityProtocol,
    response: &CanonicalInferenceResponse,
) -> CompatibilityResult<Value> {
    validate_canonical_response(response)?;
    match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => {
            let (text, tool_calls) = openai_response_content(&response.content)?;
            Ok(json!({
                "id": response.response_id,
                "object": "chat.completion",
                "created": response.created_at_seconds,
                "model": response.model,
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { Value::String(text) },
                        "tool_calls": tool_calls,
                    },
                    "finish_reason": openai_finish_reason(&response.finish_reason),
                }],
                "usage": {
                    "prompt_tokens": response.usage.input_tokens,
                    "completion_tokens": response.usage.output_tokens,
                    "total_tokens": response.usage.input_tokens.saturating_add(response.usage.output_tokens),
                }
            }))
        }
        CompatibilityProtocol::OpenAiResponses => {
            let mut output = Vec::new();
            let mut text_parts = Vec::new();
            for (index, content) in response.content.iter().enumerate() {
                match content {
                    CanonicalContent::Text { text } => text_parts.push(json!({
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                    })),
                    CanonicalContent::ToolUse { id, name, input } => output.push(json!({
                        "type": "function_call",
                        "id": format!("item-{index}"),
                        "call_id": id,
                        "name": name,
                        "arguments": canonical_json_string(input)?,
                        "status": "completed",
                    })),
                    CanonicalContent::ToolResult { .. } => {
                        return Err(invalid(
                            "response.content",
                            "tool results cannot be assistant output",
                        ))
                    }
                }
            }
            if !text_parts.is_empty() {
                output.insert(
                    0,
                    json!({
                        "type": "message",
                        "id": "message-0",
                        "role": "assistant",
                        "status": "completed",
                        "content": text_parts,
                    }),
                );
            }
            Ok(json!({
                "id": response.response_id,
                "object": "response",
                "created_at": response.created_at_seconds,
                "status": "completed",
                "model": response.model,
                "output": output,
                "usage": {
                    "input_tokens": response.usage.input_tokens,
                    "output_tokens": response.usage.output_tokens,
                    "total_tokens": response.usage.input_tokens.saturating_add(response.usage.output_tokens),
                }
            }))
        }
        CompatibilityProtocol::AnthropicMessages => {
            let mut content = Vec::new();
            for block in &response.content {
                match block {
                    CanonicalContent::Text { text } => content.push(json!({
                        "type": "text",
                        "text": text,
                    })),
                    CanonicalContent::ToolUse { id, name, input } => content.push(json!({
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": input,
                    })),
                    CanonicalContent::ToolResult { .. } => {
                        return Err(invalid(
                            "response.content",
                            "tool results cannot be assistant output",
                        ))
                    }
                }
            }
            Ok(json!({
                "id": response.response_id,
                "type": "message",
                "role": "assistant",
                "model": response.model,
                "content": content,
                "stop_reason": anthropic_stop_reason(&response.finish_reason),
                "stop_sequence": Value::Null,
                "usage": {
                    "input_tokens": response.usage.input_tokens,
                    "output_tokens": response.usage.output_tokens,
                }
            }))
        }
    }
}

pub fn encode_stream_event(
    protocol: CompatibilityProtocol,
    event: &CanonicalStreamEvent,
) -> CompatibilityResult<Vec<ProtocolStreamFrame>> {
    validate_stream_event(event)?;
    match protocol {
        CompatibilityProtocol::OpenAiChatCompletions => encode_openai_chat_event(event),
        CompatibilityProtocol::OpenAiResponses => encode_openai_responses_event(event),
        CompatibilityProtocol::AnthropicMessages => encode_anthropic_event(event),
    }
}

fn encode_openai_chat_event(
    event: &CanonicalStreamEvent,
) -> CompatibilityResult<Vec<ProtocolStreamFrame>> {
    let frames = match event {
        CanonicalStreamEvent::ResponseStart {
            response_id,
            model,
            created_at_seconds,
        } => vec![json_frame(
            None,
            json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created_at_seconds,
                "model": model,
                "choices": [{"index":0,"delta":{"role":"assistant"},"finish_reason":Value::Null}],
            }),
        )?],
        CanonicalStreamEvent::TextStart { .. } | CanonicalStreamEvent::TextEnd { .. } => Vec::new(),
        CanonicalStreamEvent::TextDelta { text, .. } => vec![json_frame(
            None,
            json!({
                "object": "chat.completion.chunk",
                "choices": [{"index":0,"delta":{"content":text},"finish_reason":Value::Null}],
            }),
        )?],
        CanonicalStreamEvent::ToolCallStart {
            index,
            call_id,
            name,
        } => vec![json_frame(
            None,
            json!({
                "object": "chat.completion.chunk",
                "choices": [{"index":0,"delta":{"tool_calls":[{
                    "index":index,"id":call_id,"type":"function","function":{"name":name,"arguments":""}
                }]},"finish_reason":Value::Null}],
            }),
        )?],
        CanonicalStreamEvent::ToolCallArgumentsDelta {
            index, json_delta, ..
        } => vec![json_frame(
            None,
            json!({
                "object": "chat.completion.chunk",
                "choices": [{"index":0,"delta":{"tool_calls":[{
                    "index":index,"function":{"arguments":json_delta}
                }]},"finish_reason":Value::Null}],
            }),
        )?],
        CanonicalStreamEvent::ToolCallEnd { .. } => Vec::new(),
        CanonicalStreamEvent::ResponseCompleted {
            response_id,
            finish_reason,
            usage,
        } => vec![
            json_frame(
                None,
                json!({
                    "id": response_id,
                    "object": "chat.completion.chunk",
                    "choices": [{"index":0,"delta":{},"finish_reason":openai_finish_reason(finish_reason)}],
                    "usage": {
                        "prompt_tokens":usage.input_tokens,
                        "completion_tokens":usage.output_tokens,
                        "total_tokens":usage.input_tokens.saturating_add(usage.output_tokens),
                    }
                }),
            )?,
            ProtocolStreamFrame {
                event: None,
                data: "[DONE]".to_string(),
            },
        ],
        CanonicalStreamEvent::Error {
            code,
            message,
            retryable,
        } => vec![json_frame(
            None,
            json!({
                "error":{"type":"api_error","code":code,"message":message,"retryable":retryable}
            }),
        )?],
    };
    Ok(frames)
}

fn encode_openai_responses_event(
    event: &CanonicalStreamEvent,
) -> CompatibilityResult<Vec<ProtocolStreamFrame>> {
    let frame = match event {
        CanonicalStreamEvent::ResponseStart {
            response_id,
            model,
            created_at_seconds,
        } => Some(json_frame(
            Some("response.created"),
            json!({
                "type":"response.created",
                "response":{"id":response_id,"object":"response","created_at":created_at_seconds,"status":"in_progress","model":model,"output":[]}
            }),
        )?),
        CanonicalStreamEvent::TextStart { index } => Some(json_frame(
            Some("response.content_part.added"),
            json!({"type":"response.content_part.added","output_index":0,"content_index":index,"part":{"type":"output_text","text":"","annotations":[]}}),
        )?),
        CanonicalStreamEvent::TextDelta { index, text } => Some(json_frame(
            Some("response.output_text.delta"),
            json!({"type":"response.output_text.delta","output_index":0,"content_index":index,"delta":text}),
        )?),
        CanonicalStreamEvent::TextEnd { index } => Some(json_frame(
            Some("response.output_text.done"),
            json!({"type":"response.output_text.done","output_index":0,"content_index":index}),
        )?),
        CanonicalStreamEvent::ToolCallStart {
            index,
            call_id,
            name,
        } => Some(json_frame(
            Some("response.output_item.added"),
            json!({"type":"response.output_item.added","output_index":index,"item":{"type":"function_call","id":format!("item-{index}"),"call_id":call_id,"name":name,"arguments":"","status":"in_progress"}}),
        )?),
        CanonicalStreamEvent::ToolCallArgumentsDelta {
            index,
            call_id,
            json_delta,
        } => Some(json_frame(
            Some("response.function_call_arguments.delta"),
            json!({"type":"response.function_call_arguments.delta","output_index":index,"call_id":call_id,"delta":json_delta}),
        )?),
        CanonicalStreamEvent::ToolCallEnd { index, call_id } => Some(json_frame(
            Some("response.function_call_arguments.done"),
            json!({"type":"response.function_call_arguments.done","output_index":index,"call_id":call_id}),
        )?),
        CanonicalStreamEvent::ResponseCompleted {
            response_id, usage, ..
        } => Some(json_frame(
            Some("response.completed"),
            json!({"type":"response.completed","response":{"id":response_id,"object":"response","status":"completed","usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"total_tokens":usage.input_tokens.saturating_add(usage.output_tokens)}}}),
        )?),
        CanonicalStreamEvent::Error {
            code,
            message,
            retryable,
        } => Some(json_frame(
            Some("error"),
            json!({"type":"error","code":code,"message":message,"retryable":retryable}),
        )?),
    };
    Ok(frame.into_iter().collect())
}

fn encode_anthropic_event(
    event: &CanonicalStreamEvent,
) -> CompatibilityResult<Vec<ProtocolStreamFrame>> {
    let frames = match event {
        CanonicalStreamEvent::ResponseStart {
            response_id, model, ..
        } => vec![json_frame(
            Some("message_start"),
            json!({"type":"message_start","message":{"id":response_id,"type":"message","role":"assistant","model":model,"content":[],"stop_reason":Value::Null,"stop_sequence":Value::Null,"usage":{"input_tokens":0,"output_tokens":0}}}),
        )?],
        CanonicalStreamEvent::TextStart { index } => vec![json_frame(
            Some("content_block_start"),
            json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}),
        )?],
        CanonicalStreamEvent::TextDelta { index, text } => vec![json_frame(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":text}}),
        )?],
        CanonicalStreamEvent::TextEnd { index } => vec![json_frame(
            Some("content_block_stop"),
            json!({"type":"content_block_stop","index":index}),
        )?],
        CanonicalStreamEvent::ToolCallStart {
            index,
            call_id,
            name,
        } => vec![json_frame(
            Some("content_block_start"),
            json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
        )?],
        CanonicalStreamEvent::ToolCallArgumentsDelta {
            index, json_delta, ..
        } => vec![json_frame(
            Some("content_block_delta"),
            json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":json_delta}}),
        )?],
        CanonicalStreamEvent::ToolCallEnd { index, .. } => vec![json_frame(
            Some("content_block_stop"),
            json!({"type":"content_block_stop","index":index}),
        )?],
        CanonicalStreamEvent::ResponseCompleted {
            finish_reason,
            usage,
            ..
        } => vec![
            json_frame(
                Some("message_delta"),
                json!({"type":"message_delta","delta":{"stop_reason":anthropic_stop_reason(finish_reason),"stop_sequence":Value::Null},"usage":{"output_tokens":usage.output_tokens}}),
            )?,
            json_frame(Some("message_stop"), json!({"type":"message_stop"}))?,
        ],
        CanonicalStreamEvent::Error {
            code,
            message,
            retryable,
        } => vec![json_frame(
            Some("error"),
            json!({"type":"error","error":{"type":code,"message":message,"retryable":retryable}}),
        )?],
    };
    Ok(frames)
}

fn json_frame(event: Option<&str>, value: Value) -> CompatibilityResult<ProtocolStreamFrame> {
    Ok(ProtocolStreamFrame {
        event: event.map(str::to_string),
        data: canonical_json_string(&value)?,
    })
}

pub fn protocol_error_response(
    protocol: CompatibilityProtocol,
    error: &CompatibilityError,
) -> (u16, Value, Option<u64>) {
    let (status, code, message, retry_after) = match error {
        CompatibilityError::Unauthorized(message) => {
            (401, "authentication_error", message.clone(), None)
        }
        CompatibilityError::Forbidden(message) => (403, "permission_error", message.clone(), None),
        CompatibilityError::RateLimited { retry_after_ms } => (
            429,
            "rate_limit_error",
            error.to_string(),
            Some(*retry_after_ms),
        ),
        CompatibilityError::Unsupported { message, .. } => {
            (400, "unsupported_feature", message.clone(), None)
        }
        CompatibilityError::InvalidRequest { message, .. }
        | CompatibilityError::Conflict(message) => {
            (400, "invalid_request_error", message.clone(), None)
        }
        CompatibilityError::Limit { .. } => (413, "request_too_large", error.to_string(), None),
        _ => (
            500,
            "api_error",
            "internal compatibility error".to_string(),
            None,
        ),
    };
    let body = match protocol {
        CompatibilityProtocol::AnthropicMessages => {
            json!({"type":"error","error":{"type":code,"message":message}})
        }
        CompatibilityProtocol::OpenAiChatCompletions | CompatibilityProtocol::OpenAiResponses => {
            json!({"error":{"type":code,"code":code,"message":message}})
        }
    };
    (status, body, retry_after)
}

fn openai_response_content(
    content: &[CanonicalContent],
) -> CompatibilityResult<(String, Vec<Value>)> {
    let mut text = Vec::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block {
            CanonicalContent::Text { text: value } => text.push(value.as_str()),
            CanonicalContent::ToolUse { id, name, input } => tool_calls.push(json!({
                "id":id,"type":"function","function":{"name":name,"arguments":canonical_json_string(input)?}
            })),
            CanonicalContent::ToolResult { .. } => {
                return Err(invalid("response.content", "tool results cannot be assistant output"))
            }
        }
    }
    Ok((text.join(""), tool_calls))
}

fn openai_finish_reason(reason: &str) -> &str {
    match reason {
        "max_tokens" | "length" => "length",
        "tool_use" | "tool_calls" => "tool_calls",
        "content_filter" => "content_filter",
        _ => "stop",
    }
}

fn anthropic_stop_reason(reason: &str) -> &str {
    match reason {
        "max_tokens" | "length" => "max_tokens",
        "tool_use" | "tool_calls" => "tool_use",
        "stop_sequence" => "stop_sequence",
        _ => "end_turn",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiScope {
    ChatCompletions,
    Responses,
    Messages,
    ModelDiscover,
    ModelDownload,
    ModelLoad,
    ModelUnload,
    ModelDelete,
    ModelStatus,
}

impl ApiScope {
    pub fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::ModelDownload | Self::ModelLoad | Self::ModelUnload | Self::ModelDelete
        )
    }

    pub fn is_destructive(self) -> bool {
        self == Self::ModelDelete
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiBackend {
    ManagedLocal,
    Ollama,
    Mlx,
    CloudProvider,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum TlsPolicy {
    Disabled,
    Certificate {
        certificate_sha256: String,
        private_key_reference: String,
        minimum_version: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateLimitPolicy {
    pub window_ms: u64,
    pub max_requests: u64,
    pub max_input_bytes: u64,
}

impl Default for RateLimitPolicy {
    fn default() -> Self {
        Self {
            window_ms: 60_000,
            max_requests: 60,
            max_input_bytes: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LanServerPolicy {
    pub bind_address: String,
    pub port: u16,
    pub require_authentication: bool,
    pub pairing_required: bool,
    pub tls: TlsPolicy,
    pub cors_allowlist: Vec<String>,
    pub allowed_backends: BTreeSet<ApiBackend>,
    pub allowed_lan_mutations: BTreeSet<ApiScope>,
    pub allow_cloud_providers_over_lan: bool,
    pub rate_limit: RateLimitPolicy,
    pub pairing_ttl_ms: u64,
}

impl Default for LanServerPolicy {
    fn default() -> Self {
        Self {
            bind_address: "127.0.0.1".to_string(),
            port: 1_234,
            require_authentication: true,
            pairing_required: true,
            tls: TlsPolicy::Disabled,
            cors_allowlist: Vec::new(),
            allowed_backends: BTreeSet::from([
                ApiBackend::ManagedLocal,
                ApiBackend::Ollama,
                ApiBackend::Mlx,
            ]),
            allowed_lan_mutations: BTreeSet::new(),
            allow_cloud_providers_over_lan: false,
            rate_limit: RateLimitPolicy::default(),
            pairing_ttl_ms: 5 * 60 * 1_000,
        }
    }
}

impl LanServerPolicy {
    pub fn validate(&self) -> CompatibilityResult<()> {
        let address = self.bind_address.parse::<IpAddr>().map_err(|error| {
            invalid(
                "policy.bindAddress",
                format!("must be an exact IP interface: {error}"),
            )
        })?;
        if address.is_unspecified() || address.is_multicast() || self.port == 0 {
            return Err(invalid(
                "policy.bindAddress",
                "wildcard/multicast addresses and port zero are forbidden",
            ));
        }
        if matches!(address, IpAddr::V4(value) if value.is_broadcast()) {
            return Err(invalid("policy.bindAddress", "broadcast is forbidden"));
        }
        validate_rate_limit(&self.rate_limit)?;
        if self.pairing_ttl_ms < 30_000 || self.pairing_ttl_ms > 60 * 60 * 1_000 {
            return Err(invalid(
                "policy.pairingTtlMs",
                "must be between 30 seconds and 1 hour",
            ));
        }
        if self.allowed_backends.is_empty() {
            return Err(invalid("policy.allowedBackends", "must not be empty"));
        }
        for scope in &self.allowed_lan_mutations {
            if !scope.is_mutation() {
                return Err(invalid(
                    "policy.allowedLanMutations",
                    "may contain only exact lifecycle mutation scopes",
                ));
            }
        }
        let loopback = address.is_loopback();
        match &self.tls {
            TlsPolicy::Disabled if !loopback => {
                return Err(invalid("policy.tls", "non-loopback binding requires TLS"))
            }
            TlsPolicy::Certificate {
                certificate_sha256,
                private_key_reference,
                minimum_version,
            } => {
                validate_sha256(certificate_sha256, "policy.tls.certificateSha256")?;
                validate_id(private_key_reference, "policy.tls.privateKeyReference")?;
                if !matches!(minimum_version.as_str(), "1.2" | "1.3") {
                    return Err(invalid(
                        "policy.tls.minimumVersion",
                        "must be TLS 1.2 or 1.3",
                    ));
                }
            }
            TlsPolicy::Disabled => {}
        }
        if !loopback {
            if !self.require_authentication || !self.pairing_required {
                return Err(invalid(
                    "policy",
                    "LAN binding requires authentication and pairing",
                ));
            }
            if self.cors_allowlist.is_empty() {
                return Err(invalid(
                    "policy.corsAllowlist",
                    "LAN binding requires a non-empty narrow CORS allowlist",
                ));
            }
            if self.allow_cloud_providers_over_lan
                || self.allowed_backends.contains(&ApiBackend::CloudProvider)
            {
                return Err(invalid(
                    "policy.allowedBackends",
                    "cloud-provider routing over LAN is disabled",
                ));
            }
        }
        let mut origins = BTreeSet::new();
        for origin in &self.cors_allowlist {
            validate_cors_origin(origin, !loopback)?;
            if !origins.insert(origin.to_ascii_lowercase()) {
                return Err(invalid("policy.corsAllowlist", "contains duplicates"));
            }
        }
        Ok(())
    }

    pub fn is_loopback(&self) -> bool {
        self.bind_address
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PairingRequest {
    pub client_label: String,
    pub scopes: BTreeSet<ApiScope>,
    pub backends: BTreeSet<ApiBackend>,
    pub allowed_models: BTreeSet<String>,
    pub token_expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairingChallengeView {
    pub challenge_id: String,
    pub pairing_code: String,
    pub expires_at_ms: u64,
    pub client_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairedToken {
    pub token: String,
    pub record: ScopedTokenView,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopedTokenView {
    pub token_id: String,
    pub client_label: String,
    pub scopes: BTreeSet<ApiScope>,
    pub backends: BTreeSet<ApiBackend>,
    pub allowed_models: BTreeSet<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: Option<u64>,
    pub revoked_at_ms: Option<u64>,
    pub last_used_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PairingChallengeRecord {
    challenge_id: String,
    code_sha256: String,
    request: PairingRequest,
    created_at_ms: u64,
    expires_at_ms: u64,
    failed_attempts: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScopedTokenRecord {
    token_id: String,
    token_sha256: String,
    client_label: String,
    scopes: BTreeSet<ApiScope>,
    backends: BTreeSet<ApiBackend>,
    allowed_models: BTreeSet<String>,
    created_at_ms: u64,
    expires_at_ms: Option<u64>,
    revoked_at_ms: Option<u64>,
    last_used_at_ms: Option<u64>,
    window_started_at_ms: u64,
    requests_in_window: u64,
    input_bytes_in_window: u64,
}

impl From<&ScopedTokenRecord> for ScopedTokenView {
    fn from(record: &ScopedTokenRecord) -> Self {
        Self {
            token_id: record.token_id.clone(),
            client_label: record.client_label.clone(),
            scopes: record.scopes.clone(),
            backends: record.backends.clone(),
            allowed_models: record.allowed_models.clone(),
            created_at_ms: record.created_at_ms,
            expires_at_ms: record.expires_at_ms,
            revoked_at_ms: record.revoked_at_ms,
            last_used_at_ms: record.last_used_at_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecurityAuditKind {
    PairingStarted,
    PairingFailed,
    PairingCompleted,
    TokenAuthorized,
    TokenDenied,
    TokenRateLimited,
    TokenRevoked,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SecurityAuditEvent {
    pub event_id: String,
    pub occurred_at_ms: u64,
    pub kind: SecurityAuditKind,
    pub token_id: Option<String>,
    pub challenge_id: Option<String>,
    pub scope: Option<ApiScope>,
    pub remote_address: Option<String>,
    pub outcome: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LanSecurityState {
    state_version: u32,
    generation: u64,
    challenges: Vec<PairingChallengeRecord>,
    tokens: Vec<ScopedTokenRecord>,
    audit_events: Vec<SecurityAuditEvent>,
}

impl Default for LanSecurityState {
    fn default() -> Self {
        Self {
            state_version: LAN_SECURITY_STATE_VERSION,
            generation: 0,
            challenges: Vec::new(),
            tokens: Vec::new(),
            audit_events: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProtectedLanState {
    protector_id: String,
    state: LanSecurityState,
    authentication_tag_base64: String,
}

pub trait LanEntropySource: Send + Sync {
    fn fill(&self, output: &mut [u8]) -> Result<(), String>;
}

#[derive(Default)]
pub struct OsLanEntropy;

impl LanEntropySource for OsLanEntropy {
    fn fill(&self, output: &mut [u8]) -> Result<(), String> {
        rand::rng().fill(output);
        Ok(())
    }
}

pub trait LanStateProtector: Send + Sync {
    fn protector_id(&self) -> &str;
    /// Return an authentication tag over exact canonical state bytes using a
    /// key stored outside this state directory (for example OS keychain).
    fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String>;
    fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizationRequest {
    pub bearer_token: String,
    pub scope: ApiScope,
    pub backend: ApiBackend,
    pub model_id: Option<String>,
    pub input_bytes: u64,
    pub remote_address: String,
    pub destructive_confirmation: Option<String>,
    pub now_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizedToken {
    pub token_id: String,
    pub client_label: String,
    pub scope: ApiScope,
    pub backend: ApiBackend,
}

pub struct LanAccessController {
    state_root: PathBuf,
    policy: LanServerPolicy,
    entropy: Arc<dyn LanEntropySource>,
    protector: Arc<dyn LanStateProtector>,
    operation_lock: Mutex<()>,
}

impl LanAccessController {
    pub fn new(
        state_root: impl AsRef<Path>,
        policy: LanServerPolicy,
        entropy: Arc<dyn LanEntropySource>,
        protector: Arc<dyn LanStateProtector>,
    ) -> CompatibilityResult<Self> {
        policy.validate()?;
        validate_id(protector.protector_id(), "protectorId")?;
        ensure_private_directory(state_root.as_ref())?;
        let controller = Self {
            state_root: state_root.as_ref().to_path_buf(),
            policy,
            entropy,
            protector,
            operation_lock: Mutex::new(()),
        };
        let _ = controller.load_state()?;
        Ok(controller)
    }

    pub fn policy(&self) -> &LanServerPolicy {
        &self.policy
    }

    pub fn begin_pairing(
        &self,
        request: PairingRequest,
        now_ms: u64,
        remote_address: &str,
    ) -> CompatibilityResult<PairingChallengeView> {
        validate_timestamp(now_ms, "nowMs")?;
        validate_remote_address(remote_address)?;
        validate_pairing_request(&request, &self.policy, now_ms)?;
        let _guard = lock(&self.operation_lock)?;
        let mut state = self.load_state()?;
        state
            .challenges
            .retain(|challenge| challenge.expires_at_ms >= now_ms);
        if state.challenges.len() >= 128 {
            return Err(limit(
                "active pairing challenges",
                state.challenges.len() as u64,
                128,
            ));
        }
        let challenge_id = self.random_identifier("pair", 16)?;
        let pairing_code = self.random_pairing_code()?;
        let expires_at_ms = now_ms
            .checked_add(self.policy.pairing_ttl_ms)
            .ok_or_else(|| invalid("pairingTtl", "timestamp overflow"))?;
        state.challenges.push(PairingChallengeRecord {
            challenge_id: challenge_id.clone(),
            code_sha256: pairing_code_digest(&challenge_id, &pairing_code),
            request: request.clone(),
            created_at_ms: now_ms,
            expires_at_ms,
            failed_attempts: 0,
        });
        append_audit(
            &mut state,
            self.audit_event(
                now_ms,
                SecurityAuditKind::PairingStarted,
                None,
                Some(challenge_id.clone()),
                None,
                Some(remote_address.to_string()),
                "allowed",
                "pairing challenge issued",
            )?,
        );
        self.save_next_state(&mut state)?;
        Ok(PairingChallengeView {
            challenge_id,
            pairing_code,
            expires_at_ms,
            client_label: request.client_label,
        })
    }

    pub fn complete_pairing(
        &self,
        challenge_id: &str,
        pairing_code: &str,
        now_ms: u64,
        remote_address: &str,
    ) -> CompatibilityResult<PairedToken> {
        validate_id(challenge_id, "challengeId")?;
        validate_timestamp(now_ms, "nowMs")?;
        validate_remote_address(remote_address)?;
        if pairing_code.len() != 8 || !pairing_code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(CompatibilityError::Unauthorized(
                "invalid pairing credentials".to_string(),
            ));
        }
        let _guard = lock(&self.operation_lock)?;
        let mut state = self.load_state()?;
        let Some(index) = state
            .challenges
            .iter()
            .position(|challenge| challenge.challenge_id == challenge_id)
        else {
            return Err(CompatibilityError::Unauthorized(
                "unknown or consumed pairing challenge".to_string(),
            ));
        };
        let challenge = state.challenges[index].clone();
        let expected = pairing_code_digest(challenge_id, pairing_code);
        let valid = challenge.expires_at_ms >= now_ms
            && challenge.failed_attempts < MAX_PAIRING_ATTEMPTS
            && constant_time_eq(expected.as_bytes(), challenge.code_sha256.as_bytes());
        if !valid {
            let record = &mut state.challenges[index];
            record.failed_attempts = record.failed_attempts.saturating_add(1);
            if record.expires_at_ms < now_ms || record.failed_attempts >= MAX_PAIRING_ATTEMPTS {
                state.challenges.remove(index);
            }
            append_audit(
                &mut state,
                self.audit_event(
                    now_ms,
                    SecurityAuditKind::PairingFailed,
                    None,
                    Some(challenge_id.to_string()),
                    None,
                    Some(remote_address.to_string()),
                    "denied",
                    "pairing verification failed",
                )?,
            );
            self.save_next_state(&mut state)?;
            return Err(CompatibilityError::Unauthorized(
                "invalid or expired pairing credentials".to_string(),
            ));
        }
        state.challenges.remove(index);
        if state.tokens.len() >= 1_024 {
            return Err(limit(
                "paired token count",
                state.tokens.len() as u64,
                1_024,
            ));
        }
        let token_id = self.random_identifier("token", 16)?;
        let token = format!("{TOKEN_PREFIX}{}", self.random_hex(32)?);
        let record = ScopedTokenRecord {
            token_id: token_id.clone(),
            token_sha256: sha256_hex(token.as_bytes()),
            client_label: challenge.request.client_label,
            scopes: challenge.request.scopes,
            backends: challenge.request.backends,
            allowed_models: challenge.request.allowed_models,
            created_at_ms: now_ms,
            expires_at_ms: challenge.request.token_expires_at_ms,
            revoked_at_ms: None,
            last_used_at_ms: None,
            window_started_at_ms: now_ms,
            requests_in_window: 0,
            input_bytes_in_window: 0,
        };
        let view = ScopedTokenView::from(&record);
        state.tokens.push(record);
        append_audit(
            &mut state,
            self.audit_event(
                now_ms,
                SecurityAuditKind::PairingCompleted,
                Some(token_id),
                Some(challenge_id.to_string()),
                None,
                Some(remote_address.to_string()),
                "allowed",
                "scoped token minted; plaintext returned once",
            )?,
        );
        self.save_next_state(&mut state)?;
        Ok(PairedToken {
            token,
            record: view,
        })
    }

    pub fn authorize(
        &self,
        request: &AuthorizationRequest,
    ) -> CompatibilityResult<AuthorizedToken> {
        validate_authorization_request(request)?;
        let _guard = lock(&self.operation_lock)?;
        let mut state = self.load_state()?;
        let digest = sha256_hex(request.bearer_token.as_bytes());
        let Some(index) = state
            .tokens
            .iter()
            .position(|record| constant_time_eq(record.token_sha256.as_bytes(), digest.as_bytes()))
        else {
            append_audit(
                &mut state,
                self.audit_event(
                    request.now_ms,
                    SecurityAuditKind::TokenDenied,
                    None,
                    None,
                    Some(request.scope),
                    Some(request.remote_address.clone()),
                    "denied",
                    "unknown bearer token",
                )?,
            );
            self.save_next_state(&mut state)?;
            return Err(CompatibilityError::Unauthorized(
                "invalid bearer token".to_string(),
            ));
        };
        let denial = token_denial_reason(&state.tokens[index], request, &self.policy);
        if let Some(reason) = denial {
            let token_id = state.tokens[index].token_id.clone();
            append_audit(
                &mut state,
                self.audit_event(
                    request.now_ms,
                    SecurityAuditKind::TokenDenied,
                    Some(token_id),
                    None,
                    Some(request.scope),
                    Some(request.remote_address.clone()),
                    "denied",
                    &reason,
                )?,
            );
            self.save_next_state(&mut state)?;
            return Err(CompatibilityError::Forbidden(reason));
        }
        let rate_limit = &self.policy.rate_limit;
        let record = &mut state.tokens[index];
        if request.now_ms.saturating_sub(record.window_started_at_ms) >= rate_limit.window_ms {
            record.window_started_at_ms = request.now_ms;
            record.requests_in_window = 0;
            record.input_bytes_in_window = 0;
        }
        let next_requests = record.requests_in_window.saturating_add(1);
        let next_bytes = record
            .input_bytes_in_window
            .saturating_add(request.input_bytes);
        if next_requests > rate_limit.max_requests || next_bytes > rate_limit.max_input_bytes {
            let retry_after_ms = rate_limit
                .window_ms
                .saturating_sub(request.now_ms.saturating_sub(record.window_started_at_ms));
            let token_id = record.token_id.clone();
            append_audit(
                &mut state,
                self.audit_event(
                    request.now_ms,
                    SecurityAuditKind::TokenRateLimited,
                    Some(token_id),
                    None,
                    Some(request.scope),
                    Some(request.remote_address.clone()),
                    "denied",
                    "per-token request or byte quota exceeded",
                )?,
            );
            self.save_next_state(&mut state)?;
            return Err(CompatibilityError::RateLimited { retry_after_ms });
        }
        record.requests_in_window = next_requests;
        record.input_bytes_in_window = next_bytes;
        record.last_used_at_ms = Some(request.now_ms);
        let token_id = record.token_id.clone();
        let client_label = record.client_label.clone();
        append_audit(
            &mut state,
            self.audit_event(
                request.now_ms,
                SecurityAuditKind::TokenAuthorized,
                Some(token_id.clone()),
                None,
                Some(request.scope),
                Some(request.remote_address.clone()),
                "allowed",
                "scope, backend, model, mutation, and rate checks passed",
            )?,
        );
        self.save_next_state(&mut state)?;
        Ok(AuthorizedToken {
            token_id,
            client_label,
            scope: request.scope,
            backend: request.backend,
        })
    }

    pub fn revoke_token(
        &self,
        token_id: &str,
        now_ms: u64,
        remote_address: &str,
    ) -> CompatibilityResult<ScopedTokenView> {
        validate_id(token_id, "tokenId")?;
        validate_timestamp(now_ms, "nowMs")?;
        validate_remote_address(remote_address)?;
        let _guard = lock(&self.operation_lock)?;
        let mut state = self.load_state()?;
        let record = state
            .tokens
            .iter_mut()
            .find(|record| record.token_id == token_id)
            .ok_or_else(|| CompatibilityError::Conflict("token does not exist".to_string()))?;
        if record.revoked_at_ms.is_none() {
            record.revoked_at_ms = Some(now_ms);
        }
        let view = ScopedTokenView::from(&*record);
        append_audit(
            &mut state,
            self.audit_event(
                now_ms,
                SecurityAuditKind::TokenRevoked,
                Some(token_id.to_string()),
                None,
                None,
                Some(remote_address.to_string()),
                "allowed",
                "token revoked",
            )?,
        );
        self.save_next_state(&mut state)?;
        Ok(view)
    }

    /// Revokes every currently live token before a listener is disabled or
    /// its trust boundary changes. Persisted token digests therefore cannot
    /// silently become valid again if LAN serving is enabled later.
    pub fn revoke_all_tokens(
        &self,
        now_ms: u64,
        remote_address: &str,
    ) -> CompatibilityResult<usize> {
        validate_timestamp(now_ms, "nowMs")?;
        validate_remote_address(remote_address)?;
        let _guard = lock(&self.operation_lock)?;
        let mut state = self.load_state()?;
        let token_ids = state
            .tokens
            .iter_mut()
            .filter_map(|record| {
                if record.revoked_at_ms.is_none() {
                    record.revoked_at_ms = Some(now_ms);
                    Some(record.token_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for token_id in &token_ids {
            append_audit(
                &mut state,
                self.audit_event(
                    now_ms,
                    SecurityAuditKind::TokenRevoked,
                    Some(token_id.clone()),
                    None,
                    None,
                    Some(remote_address.to_string()),
                    "allowed",
                    "token revoked because the listener was disabled or reconfigured",
                )?,
            );
        }
        if !token_ids.is_empty() {
            self.save_next_state(&mut state)?;
        }
        Ok(token_ids.len())
    }

    pub fn list_tokens(&self) -> CompatibilityResult<Vec<ScopedTokenView>> {
        let _guard = lock(&self.operation_lock)?;
        Ok(self
            .load_state()?
            .tokens
            .iter()
            .map(ScopedTokenView::from)
            .collect())
    }

    pub fn audit_events(&self) -> CompatibilityResult<Vec<SecurityAuditEvent>> {
        let _guard = lock(&self.operation_lock)?;
        Ok(self.load_state()?.audit_events)
    }

    fn random_hex(&self, bytes: usize) -> CompatibilityResult<String> {
        let mut output = vec![0_u8; bytes];
        self.entropy
            .fill(&mut output)
            .map_err(CompatibilityError::Entropy)?;
        Ok(output.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn random_identifier(&self, prefix: &str, bytes: usize) -> CompatibilityResult<String> {
        Ok(format!("{prefix}-{}", self.random_hex(bytes)?))
    }

    fn random_pairing_code(&self) -> CompatibilityResult<String> {
        let mut bytes = [0_u8; 4];
        self.entropy
            .fill(&mut bytes)
            .map_err(CompatibilityError::Entropy)?;
        let number = u32::from_le_bytes(bytes) % 100_000_000;
        Ok(format!("{number:08}"))
    }

    fn audit_event(
        &self,
        occurred_at_ms: u64,
        kind: SecurityAuditKind,
        token_id: Option<String>,
        challenge_id: Option<String>,
        scope: Option<ApiScope>,
        remote_address: Option<String>,
        outcome: &str,
        detail: &str,
    ) -> CompatibilityResult<SecurityAuditEvent> {
        Ok(SecurityAuditEvent {
            event_id: self.random_identifier("audit", 12)?,
            occurred_at_ms,
            kind,
            token_id,
            challenge_id,
            scope,
            remote_address,
            outcome: outcome.to_string(),
            detail: detail.to_string(),
        })
    }

    fn load_state(&self) -> CompatibilityResult<LanSecurityState> {
        let mut candidates = Vec::new();
        for entry in fs::read_dir(&self.state_root)
            .map_err(|source| io_at("list LAN security state", &self.state_root, source))?
        {
            let entry = entry.map_err(|source| {
                io_at(
                    "read LAN security directory entry",
                    &self.state_root,
                    source,
                )
            })?;
            let path = entry.path();
            let Some(generation) = state_generation_from_name(&entry.file_name()) else {
                continue;
            };
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| io_at("inspect LAN security state", &path, source))?;
            if !metadata.file_type().is_file() {
                return Err(CompatibilityError::CorruptState(
                    "state generation is not a regular file".to_string(),
                ));
            }
            candidates.push((generation, path, metadata.len()));
        }
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));
        let Some((filename_generation, path, size)) = candidates.first() else {
            return Ok(LanSecurityState::default());
        };
        if *size > MAX_STATE_BYTES as u64 {
            return Err(limit(
                "LAN security state bytes",
                *size,
                MAX_STATE_BYTES as u64,
            ));
        }
        let bytes =
            fs::read(path).map_err(|source| io_at("read LAN security state", path, source))?;
        let envelope: ProtectedLanState = serde_json::from_slice(&bytes)?;
        if envelope.protector_id != self.protector.protector_id() {
            return Err(CompatibilityError::StateProtection(
                "state protector id changed".to_string(),
            ));
        }
        let tag = base64::engine::general_purpose::STANDARD
            .decode(&envelope.authentication_tag_base64)
            .map_err(|error| CompatibilityError::CorruptState(error.to_string()))?;
        if tag.len() < 16 || tag.len() > 4_096 {
            return Err(CompatibilityError::CorruptState(
                "authentication tag length is invalid".to_string(),
            ));
        }
        let canonical_state = canonical_json(&envelope.state)?;
        self.protector
            .verify(&canonical_state, &tag)
            .map_err(CompatibilityError::StateProtection)?;
        validate_security_state(&envelope.state)?;
        if envelope.state.generation != *filename_generation {
            return Err(CompatibilityError::CorruptState(
                "filename generation differs from authenticated state".to_string(),
            ));
        }
        Ok(envelope.state)
    }

    fn save_next_state(&self, state: &mut LanSecurityState) -> CompatibilityResult<()> {
        state.generation = state
            .generation
            .checked_add(1)
            .ok_or_else(|| CompatibilityError::CorruptState("generation overflow".to_string()))?;
        if state.audit_events.len() > MAX_AUDIT_EVENTS {
            let remove = state.audit_events.len() - MAX_AUDIT_EVENTS;
            state.audit_events.drain(..remove);
        }
        validate_security_state(state)?;
        let canonical_state = canonical_json(state)?;
        let tag = self
            .protector
            .authenticate(&canonical_state)
            .map_err(CompatibilityError::StateProtection)?;
        if tag.len() < 16 || tag.len() > 4_096 {
            return Err(CompatibilityError::StateProtection(
                "protector returned an invalid tag length".to_string(),
            ));
        }
        let envelope = ProtectedLanState {
            protector_id: self.protector.protector_id().to_string(),
            state: state.clone(),
            authentication_tag_base64: base64::engine::general_purpose::STANDARD.encode(tag),
        };
        let bytes = canonical_json(&envelope)?;
        if bytes.len() > MAX_STATE_BYTES {
            return Err(limit(
                "LAN security state bytes",
                bytes.len() as u64,
                MAX_STATE_BYTES as u64,
            ));
        }
        let digest = sha256_hex(&bytes);
        let path = self.state_root.join(format!(
            "{STATE_FILE_PREFIX}{:020}-{}{STATE_FILE_SUFFIX}",
            state.generation,
            &digest[..16]
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(&path)
            .map_err(|source| io_at("create LAN security state", &path, source))?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|source| io_at("write LAN security state", &path, source))?;
        sync_directory(&self.state_root)?;
        prune_old_state_files(&self.state_root, state.generation)?;
        Ok(())
    }
}

fn token_denial_reason(
    record: &ScopedTokenRecord,
    request: &AuthorizationRequest,
    policy: &LanServerPolicy,
) -> Option<String> {
    if record.revoked_at_ms.is_some() {
        return Some("token is revoked".to_string());
    }
    if record
        .expires_at_ms
        .is_some_and(|expires| request.now_ms >= expires)
    {
        return Some("token is expired".to_string());
    }
    if !record.scopes.contains(&request.scope) {
        return Some("token does not grant the requested scope".to_string());
    }
    if !record.backends.contains(&request.backend)
        || !policy.allowed_backends.contains(&request.backend)
    {
        return Some("token or server policy forbids the requested backend".to_string());
    }
    if request.backend == ApiBackend::CloudProvider
        && (!policy.is_loopback() || !policy.allow_cloud_providers_over_lan)
    {
        return Some("cloud-provider routing is disabled for this listener".to_string());
    }
    if !record.allowed_models.is_empty()
        && request
            .model_id
            .as_ref()
            .is_none_or(|model| !record.allowed_models.contains(model))
    {
        return Some("token is not scoped to the requested model".to_string());
    }
    if !policy.is_loopback()
        && request.scope.is_mutation()
        && !policy.allowed_lan_mutations.contains(&request.scope)
    {
        return Some("LAN policy does not name this lifecycle mutation".to_string());
    }
    if request.scope.is_destructive() {
        let Some(model) = request.model_id.as_deref() else {
            return Some("destructive model operation requires an exact model id".to_string());
        };
        if request.destructive_confirmation.as_deref() != Some(&format!("DELETE {model}")) {
            return Some("model deletion requires exact destructive confirmation".to_string());
        }
    }
    None
}

fn validate_authorization_request(request: &AuthorizationRequest) -> CompatibilityResult<()> {
    validate_timestamp(request.now_ms, "authorization.nowMs")?;
    validate_remote_address(&request.remote_address)?;
    if !request.bearer_token.starts_with(TOKEN_PREFIX)
        || request.bearer_token.len() != TOKEN_PREFIX.len() + 64
        || !request.bearer_token[TOKEN_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(CompatibilityError::Unauthorized(
            "invalid bearer token shape".to_string(),
        ));
    }
    if request.input_bytes > MAX_BODY_BYTES as u64 {
        return Err(limit(
            "authorization input bytes",
            request.input_bytes,
            MAX_BODY_BYTES as u64,
        ));
    }
    if let Some(model) = &request.model_id {
        validate_id(model, "authorization.modelId")?;
    }
    Ok(())
}

fn validate_pairing_request(
    request: &PairingRequest,
    policy: &LanServerPolicy,
    now_ms: u64,
) -> CompatibilityResult<()> {
    validate_text(&request.client_label, "pairing.clientLabel", 512)?;
    if request.client_label.trim().is_empty() {
        return Err(invalid("pairing.clientLabel", "must not be blank"));
    }
    if request.scopes.is_empty() || request.scopes.len() > 32 {
        return Err(invalid("pairing.scopes", "must contain 1..=32 scopes"));
    }
    if request.backends.is_empty() || !request.backends.is_subset(&policy.allowed_backends) {
        return Err(CompatibilityError::Forbidden(
            "pairing requested a backend outside server policy".to_string(),
        ));
    }
    if !policy.is_loopback()
        && request
            .scopes
            .iter()
            .any(|scope| scope.is_mutation() && !policy.allowed_lan_mutations.contains(scope))
    {
        return Err(CompatibilityError::Forbidden(
            "pairing requested a LAN mutation not named by policy".to_string(),
        ));
    }
    if !policy.is_loopback() && request.backends.contains(&ApiBackend::CloudProvider) {
        return Err(CompatibilityError::Forbidden(
            "cloud providers cannot be paired over LAN".to_string(),
        ));
    }
    if request.allowed_models.len() > 10_000 {
        return Err(limit(
            "paired model scope count",
            request.allowed_models.len() as u64,
            10_000,
        ));
    }
    for model in &request.allowed_models {
        validate_id(model, "pairing.allowedModels[]")?;
    }
    if request
        .token_expires_at_ms
        .is_some_and(|expires| expires <= now_ms || expires > i64::MAX as u64)
    {
        return Err(invalid(
            "pairing.tokenExpiresAtMs",
            "must be a future signed-64-bit timestamp",
        ));
    }
    Ok(())
}

fn validate_security_state(state: &LanSecurityState) -> CompatibilityResult<()> {
    if state.state_version != LAN_SECURITY_STATE_VERSION {
        return Err(CompatibilityError::CorruptState(
            "unsupported state version".to_string(),
        ));
    }
    if state.challenges.len() > 128
        || state.tokens.len() > 1_024
        || state.audit_events.len() > MAX_AUDIT_EVENTS
    {
        return Err(CompatibilityError::CorruptState(
            "state collection limit exceeded".to_string(),
        ));
    }
    let mut challenge_ids = BTreeSet::new();
    for challenge in &state.challenges {
        validate_id(&challenge.challenge_id, "state.challengeId")?;
        validate_sha256(&challenge.code_sha256, "state.challengeCodeSha256")?;
        if !challenge_ids.insert(&challenge.challenge_id)
            || challenge.created_at_ms == 0
            || challenge.expires_at_ms <= challenge.created_at_ms
            || challenge.failed_attempts >= MAX_PAIRING_ATTEMPTS
        {
            return Err(CompatibilityError::CorruptState(
                "invalid or duplicate pairing challenge".to_string(),
            ));
        }
    }
    let mut token_ids = BTreeSet::new();
    let mut token_digests = BTreeSet::new();
    for token in &state.tokens {
        validate_id(&token.token_id, "state.tokenId")?;
        validate_sha256(&token.token_sha256, "state.tokenSha256")?;
        if !token_ids.insert(&token.token_id)
            || !token_digests.insert(&token.token_sha256)
            || token.scopes.is_empty()
            || token.backends.is_empty()
            || token.created_at_ms == 0
            || token.window_started_at_ms == 0
        {
            return Err(CompatibilityError::CorruptState(
                "invalid or duplicate scoped token".to_string(),
            ));
        }
    }
    let mut event_ids = BTreeSet::new();
    for event in &state.audit_events {
        validate_id(&event.event_id, "state.auditEventId")?;
        if event.occurred_at_ms == 0 || !event_ids.insert(&event.event_id) {
            return Err(CompatibilityError::CorruptState(
                "invalid or duplicate audit event".to_string(),
            ));
        }
        validate_text(&event.outcome, "state.auditOutcome", 128)?;
        validate_text(&event.detail, "state.auditDetail", 4_096)?;
    }
    Ok(())
}

fn append_audit(state: &mut LanSecurityState, event: SecurityAuditEvent) {
    state.audit_events.push(event);
    if state.audit_events.len() > MAX_AUDIT_EVENTS {
        state.audit_events.remove(0);
    }
}

fn state_generation_from_name(name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let rest = name.strip_prefix(STATE_FILE_PREFIX)?;
    let rest = rest.strip_suffix(STATE_FILE_SUFFIX)?;
    let (generation, digest) = rest.split_once('-')?;
    if generation.len() != 20
        || digest.len() != 16
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    generation.parse().ok()
}

fn prune_old_state_files(root: &Path, current_generation: u64) -> CompatibilityResult<()> {
    let keep_from = current_generation.saturating_sub(2);
    let mut removed = false;
    for entry in
        fs::read_dir(root).map_err(|source| io_at("list LAN state for prune", root, source))?
    {
        let entry = entry.map_err(|source| io_at("read LAN state prune entry", root, source))?;
        let Some(generation) = state_generation_from_name(&entry.file_name()) else {
            continue;
        };
        if generation < keep_from {
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)
                .map_err(|source| io_at("inspect LAN state prune entry", &path, source))?;
            if metadata.file_type().is_file() {
                fs::remove_file(&path)
                    .map_err(|source| io_at("prune LAN state generation", &path, source))?;
                removed = true;
            }
        }
    }
    if removed {
        sync_directory(root)?;
    }
    Ok(())
}

fn pairing_code_digest(challenge_id: &str, pairing_code: &str) -> String {
    sha256_hex(format!("little-monkey-pairing\0{challenge_id}\0{pairing_code}").as_bytes())
}

fn validate_rate_limit(policy: &RateLimitPolicy) -> CompatibilityResult<()> {
    if policy.window_ms == 0
        || policy.window_ms > 24 * 60 * 60 * 1_000
        || policy.max_requests == 0
        || policy.max_requests > 1_000_000
        || policy.max_input_bytes == 0
    {
        Err(invalid(
            "policy.rateLimit",
            "contains an unsafe zero or excessive value",
        ))
    } else {
        Ok(())
    }
}

fn validate_cors_origin(origin: &str, require_https: bool) -> CompatibilityResult<()> {
    if origin == "*" || origin.len() > 4_096 {
        return Err(invalid(
            "policy.corsAllowlist",
            "wildcards and oversized origins are forbidden",
        ));
    }
    let parsed = Url::parse(origin)
        .map_err(|error| invalid("policy.corsAllowlist", format!("invalid origin: {error}")))?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || (!parsed.path().is_empty() && parsed.path() != "/")
        || parsed.host().is_none()
    {
        return Err(invalid(
            "policy.corsAllowlist",
            "must be a credential-free origin",
        ));
    }
    let loopback = match parsed.host().expect("checked") {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    };
    if require_https && parsed.scheme() != "https" {
        return Err(invalid(
            "policy.corsAllowlist",
            "LAN origins must use HTTPS",
        ));
    }
    if !require_https && parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(invalid(
            "policy.corsAllowlist",
            "only HTTPS or loopback HTTP origins are accepted",
        ));
    }
    Ok(())
}

fn validate_remote_address(value: &str) -> CompatibilityResult<()> {
    value
        .parse::<IpAddr>()
        .map_err(|error| invalid("remoteAddress", format!("must be an IP address: {error}")))?;
    Ok(())
}

fn validate_canonical_request(request: &CanonicalInferenceRequest) -> CompatibilityResult<()> {
    if request.schema_version != COMPATIBILITY_SCHEMA_VERSION {
        return Err(invalid("schemaVersion", "is unsupported"));
    }
    validate_id(&request.request_id, "requestId")?;
    validate_id(&request.model, "model")?;
    if request.messages.is_empty() || request.messages.len() > MAX_MESSAGES {
        return Err(limit(
            "canonical message count",
            request.messages.len() as u64,
            MAX_MESSAGES as u64,
        ));
    }
    let mut total_text = 0_usize;
    for message in &request.messages {
        if message.content.is_empty() {
            return Err(invalid("messages[].content", "must not be empty"));
        }
        for content in &message.content {
            match content {
                CanonicalContent::Text { text } => {
                    total_text = total_text.saturating_add(text.len())
                }
                CanonicalContent::ToolUse { id, name, input } => {
                    validate_id(id, "messages[].toolUse.id")?;
                    validate_id(name, "messages[].toolUse.name")?;
                    if !input.is_object() {
                        return Err(invalid("messages[].toolUse.input", "must be an object"));
                    }
                }
                CanonicalContent::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    validate_id(tool_use_id, "messages[].toolResult.toolUseId")?;
                    total_text = total_text.saturating_add(content.len());
                }
            }
        }
    }
    if total_text > MAX_TEXT_BYTES {
        return Err(limit(
            "canonical text bytes",
            total_text as u64,
            MAX_TEXT_BYTES as u64,
        ));
    }
    if request.tools.len() > MAX_TOOLS {
        return Err(limit(
            "canonical tool count",
            request.tools.len() as u64,
            MAX_TOOLS as u64,
        ));
    }
    let mut tool_names = BTreeSet::new();
    for tool in &request.tools {
        validate_id(&tool.name, "tools[].name")?;
        validate_text(&tool.description, "tools[].description", 64 * 1024)?;
        ensure_object_schema(&tool.input_schema, "tools[].inputSchema")?;
        if !tool_names.insert(&tool.name) {
            return Err(invalid("tools", "tool names must be unique"));
        }
    }
    if request.max_output_tokens == 0 || request.max_output_tokens > 1_000_000 {
        return Err(invalid("maxOutputTokens", "must be between 1 and 1000000"));
    }
    if request
        .temperature
        .is_some_and(|value| !value.is_finite() || !(0.0..=2.0).contains(&value))
    {
        return Err(invalid("temperature", "must be finite and between 0 and 2"));
    }
    if let Some(schema) = &request.response_schema {
        ensure_object_schema(schema, "responseSchema")?;
    }
    Ok(())
}

fn validate_canonical_response(response: &CanonicalInferenceResponse) -> CompatibilityResult<()> {
    validate_id(&response.response_id, "responseId")?;
    validate_id(&response.model, "model")?;
    validate_id(&response.finish_reason, "finishReason")?;
    validate_timestamp(response.created_at_seconds, "createdAtSeconds")?;
    if response.content.is_empty() {
        return Err(invalid("response.content", "must not be empty"));
    }
    Ok(())
}

fn validate_stream_event(event: &CanonicalStreamEvent) -> CompatibilityResult<()> {
    match event {
        CanonicalStreamEvent::ResponseStart {
            response_id,
            model,
            created_at_seconds,
        } => {
            validate_id(response_id, "stream.responseId")?;
            validate_id(model, "stream.model")?;
            validate_timestamp(*created_at_seconds, "stream.createdAtSeconds")?;
        }
        CanonicalStreamEvent::TextStart { .. } | CanonicalStreamEvent::TextEnd { .. } => {}
        CanonicalStreamEvent::TextDelta { text, .. } => {
            validate_text(text, "stream.text", MAX_TEXT_BYTES)?;
        }
        CanonicalStreamEvent::ToolCallStart { call_id, name, .. } => {
            validate_id(call_id, "stream.callId")?;
            validate_id(name, "stream.toolName")?;
        }
        CanonicalStreamEvent::ToolCallArgumentsDelta {
            call_id,
            json_delta,
            ..
        } => {
            validate_id(call_id, "stream.callId")?;
            validate_text(json_delta, "stream.jsonDelta", MAX_TEXT_BYTES)?;
        }
        CanonicalStreamEvent::ToolCallEnd { call_id, .. } => {
            validate_id(call_id, "stream.callId")?;
        }
        CanonicalStreamEvent::ResponseCompleted {
            response_id,
            finish_reason,
            ..
        } => {
            validate_id(response_id, "stream.responseId")?;
            validate_id(finish_reason, "stream.finishReason")?;
        }
        CanonicalStreamEvent::Error { code, message, .. } => {
            validate_id(code, "stream.errorCode")?;
            validate_text(message, "stream.errorMessage", 64 * 1024)?;
        }
    }
    Ok(())
}

fn require_object<'a>(
    value: &'a Value,
    path: &str,
    message: &str,
) -> CompatibilityResult<&'a Map<String, Value>> {
    value.as_object().ok_or_else(|| invalid(path, message))
}

fn require_array<'a>(
    value: &'a Value,
    path: &str,
    message: &str,
) -> CompatibilityResult<&'a Vec<Value>> {
    value.as_array().ok_or_else(|| invalid(path, message))
}

fn required_array<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    path: &str,
) -> CompatibilityResult<&'a Vec<Value>> {
    require_array(
        object
            .get(key)
            .ok_or_else(|| invalid(path, "is required"))?,
        path,
        "must be an array",
    )
}

fn require_string(value: &Value, path: &str) -> CompatibilityResult<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| invalid(path, "must be a string"))
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> CompatibilityResult<String> {
    require_string(
        object
            .get(key)
            .ok_or_else(|| invalid(path, "is required"))?,
        path,
    )
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    default: &str,
) -> CompatibilityResult<String> {
    object
        .get(key)
        .map(|value| require_string(value, key))
        .transpose()
        .map(|value| value.unwrap_or_else(|| default.to_string()))
}

fn optional_bool(
    object: &Map<String, Value>,
    key: &str,
    default: bool,
    path: &str,
) -> CompatibilityResult<bool> {
    object
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| invalid(path, "must be boolean"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn optional_f64(
    object: &Map<String, Value>,
    key: &str,
    path: &str,
) -> CompatibilityResult<Option<f64>> {
    object
        .get(key)
        .map(|value| {
            value
                .as_f64()
                .filter(|number| number.is_finite())
                .ok_or_else(|| invalid(path, "must be a finite number"))
        })
        .transpose()
}

fn optional_u32(
    object: &Map<String, Value>,
    key: &str,
    default: u32,
    path: &str,
) -> CompatibilityResult<u32> {
    object
        .get(key)
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| invalid(path, "must be an unsigned 32-bit integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn required_u32(object: &Map<String, Value>, key: &str, path: &str) -> CompatibilityResult<u32> {
    object
        .get(key)
        .ok_or_else(|| invalid(path, "is required"))?
        .as_u64()
        .and_then(|number| u32::try_from(number).ok())
        .ok_or_else(|| invalid(path, "must be an unsigned 32-bit integer"))
}

fn mutually_exclusive_token_limit(
    first: Option<&Value>,
    second: Option<&Value>,
    default: u32,
    path: &str,
) -> CompatibilityResult<u32> {
    if first.is_some() && second.is_some() {
        return Err(invalid(
            path,
            "max_tokens and max_completion_tokens are mutually exclusive",
        ));
    }
    let value = first.or(second);
    value
        .map(|value| {
            value
                .as_u64()
                .and_then(|number| u32::try_from(number).ok())
                .ok_or_else(|| invalid(path, "token limit must be unsigned 32-bit integer"))
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn value_as_text(value: &Value, path: &str) -> CompatibilityResult<String> {
    match value {
        Value::String(text) => Ok(text.clone()),
        Value::Array(blocks) => {
            let mut text = Vec::new();
            for (index, block) in blocks.iter().enumerate() {
                match block {
                    Value::String(value) => text.push(value.clone()),
                    Value::Object(object)
                        if object.get("type").and_then(Value::as_str) == Some("text") =>
                    {
                        reject_unknown(object, &["type", "text"], &format!("{path}[{index}]"))?;
                        text.push(required_string(
                            object,
                            "text",
                            &format!("{path}[{index}].text"),
                        )?);
                    }
                    _ => {
                        return Err(unsupported(
                            "multimodal_tool_result",
                            "only string/text tool results are supported",
                        ))
                    }
                }
            }
            Ok(text.join("\n"))
        }
        _ => Err(invalid(path, "must be string or text-block array")),
    }
}

fn reject_unknown(
    object: &Map<String, Value>,
    allowed: &[&str],
    path: &str,
) -> CompatibilityResult<()> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Err(unsupported(
            format!("{path}.{key}"),
            "field is outside the advertised conformance subset",
        ))
    } else {
        Ok(())
    }
}

fn ensure_object_schema(value: &Value, path: &str) -> CompatibilityResult<()> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid(path, "JSON schema must be an object"))?;
    if object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind != "object")
    {
        return Err(invalid(path, "top-level schema type must be object"));
    }
    Ok(())
}

fn validate_id(value: &str, path: &str) -> CompatibilityResult<()> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.chars().any(char::is_control)
        || matches!(value, "." | "..")
    {
        Err(invalid(
            path,
            format!("must contain 1..={MAX_ID_BYTES} bytes without controls"),
        ))
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, path: &str, max_bytes: usize) -> CompatibilityResult<()> {
    if value.len() > max_bytes || value.contains('\0') {
        Err(invalid(
            path,
            format!("exceeds {max_bytes} bytes or contains NUL"),
        ))
    } else {
        Ok(())
    }
}

fn validate_sha256(value: &str, path: &str) -> CompatibilityResult<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        Err(invalid(path, "must be lowercase hexadecimal SHA-256"))
    }
}

fn validate_timestamp(value: u64, path: &str) -> CompatibilityResult<()> {
    if value == 0 || value > i64::MAX as u64 {
        Err(invalid(path, "must be a positive signed-64-bit timestamp"))
    } else {
        Ok(())
    }
}

fn canonical_json<T: Serialize + ?Sized>(value: &T) -> CompatibilityResult<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    canonicalize_json(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn canonical_json_string<T: Serialize + ?Sized>(value: &T) -> CompatibilityResult<String> {
    String::from_utf8(canonical_json(value)?)
        .map_err(|error| invalid("json", format!("canonical JSON was not UTF-8: {error}")))
}

fn canonicalize_json(value: &mut Value) {
    match value {
        Value::Array(values) => values.iter_mut().for_each(canonicalize_json),
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut sorted = previous.into_iter().collect::<Vec<_>>();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in sorted {
                canonicalize_json(&mut value);
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0_u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn ensure_private_directory(path: &Path) -> CompatibilityResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(invalid("stateRoot", "is not a real directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)
            .map_err(|source| io_at("create LAN state directory", path, source))?,
        Err(source) => return Err(io_at("inspect LAN state directory", path, source)),
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_at("secure LAN state directory", path, source))?;
    Ok(())
}

fn sync_directory(path: &Path) -> CompatibilityResult<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_at("sync LAN state directory", path, source))?;
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> CompatibilityResult<MutexGuard<'_, T>> {
    mutex.lock().map_err(|_| CompatibilityError::LockPoisoned)
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> CompatibilityError {
    CompatibilityError::InvalidRequest {
        path: path.into(),
        message: message.into(),
    }
}

fn unsupported(feature: impl Into<String>, message: impl Into<String>) -> CompatibilityError {
    CompatibilityError::Unsupported {
        feature: feature.into(),
        message: message.into(),
    }
}

fn limit(name: &'static str, observed: u64, max: u64) -> CompatibilityError {
    CompatibilityError::Limit {
        name,
        observed,
        max,
    }
}

fn io_at(operation: &'static str, path: &Path, source: io::Error) -> CompatibilityError {
    CompatibilityError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("compatibility-{label}-{}", std::process::id()));
            let path = path.join(format!("{}", unique_test_number()));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn unique_test_number() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(1);
        NEXT.fetch_add(1, Ordering::Relaxed)
    }

    struct DeterministicEntropy(Mutex<u8>);

    impl DeterministicEntropy {
        fn new(seed: u8) -> Self {
            Self(Mutex::new(seed))
        }
    }

    impl LanEntropySource for DeterministicEntropy {
        fn fill(&self, output: &mut [u8]) -> Result<(), String> {
            let mut seed = self.0.lock().map_err(|_| "poisoned".to_string())?;
            for (index, byte) in output.iter_mut().enumerate() {
                *byte = seed.wrapping_add(index as u8);
            }
            *seed = seed.wrapping_add(17);
            Ok(())
        }
    }

    struct TestStateProtector {
        key: Vec<u8>,
    }

    impl TestStateProtector {
        fn new() -> Self {
            Self {
                key: b"test-only-state-authentication-key".to_vec(),
            }
        }

        fn tag(&self, bytes: &[u8]) -> Vec<u8> {
            let mut hash = Sha256::new();
            hash.update((self.key.len() as u64).to_le_bytes());
            hash.update(&self.key);
            hash.update(bytes);
            hash.finalize().to_vec()
        }
    }

    impl LanStateProtector for TestStateProtector {
        fn protector_id(&self) -> &str {
            "test-keychain-protector-v1"
        }

        fn authenticate(&self, canonical_state: &[u8]) -> Result<Vec<u8>, String> {
            Ok(self.tag(canonical_state))
        }

        fn verify(&self, canonical_state: &[u8], tag: &[u8]) -> Result<(), String> {
            if constant_time_eq(&self.tag(canonical_state), tag) {
                Ok(())
            } else {
                Err("state tag mismatch".to_string())
            }
        }
    }

    fn lan_policy() -> LanServerPolicy {
        LanServerPolicy {
            bind_address: "192.168.1.50".to_string(),
            port: 1_234,
            require_authentication: true,
            pairing_required: true,
            tls: TlsPolicy::Certificate {
                certificate_sha256: sha256_hex(b"test certificate"),
                private_key_reference: "keychain:lan-server-key".to_string(),
                minimum_version: "1.3".to_string(),
            },
            cors_allowlist: vec!["https://app.example.test".to_string()],
            allowed_backends: BTreeSet::from([
                ApiBackend::ManagedLocal,
                ApiBackend::Ollama,
                ApiBackend::Mlx,
            ]),
            allowed_lan_mutations: BTreeSet::from([
                ApiScope::ModelLoad,
                ApiScope::ModelUnload,
                ApiScope::ModelDelete,
            ]),
            allow_cloud_providers_over_lan: false,
            rate_limit: RateLimitPolicy {
                window_ms: 1_000,
                max_requests: 2,
                max_input_bytes: 1_024,
            },
            pairing_ttl_ms: 60_000,
        }
    }

    fn pairing_request() -> PairingRequest {
        PairingRequest {
            client_label: "Ahmad laptop".to_string(),
            scopes: BTreeSet::from([
                ApiScope::ChatCompletions,
                ApiScope::Responses,
                ApiScope::Messages,
                ApiScope::ModelDiscover,
                ApiScope::ModelLoad,
                ApiScope::ModelUnload,
                ApiScope::ModelDelete,
                ApiScope::ModelStatus,
            ]),
            backends: BTreeSet::from([ApiBackend::ManagedLocal, ApiBackend::Mlx]),
            allowed_models: BTreeSet::from(["local-model".to_string()]),
            token_expires_at_ms: Some(100_000),
        }
    }

    fn make_controller(root: &Path, policy: LanServerPolicy, seed: u8) -> LanAccessController {
        LanAccessController::new(
            root,
            policy,
            Arc::new(DeterministicEntropy::new(seed)),
            Arc::new(TestStateProtector::new()),
        )
        .expect("LAN controller")
    }

    #[test]
    fn conformance_manifest_is_narrow_versioned_and_tool_route_free() {
        let manifest = compatibility_conformance_manifest();
        assert_eq!(manifest.manifest_version, CONFORMANCE_MANIFEST_VERSION);
        assert_eq!(manifest.endpoints.len(), 3);
        assert!(!manifest.workspace_tool_routes_exposed);
        assert!(manifest.endpoints.iter().all(|endpoint| {
            endpoint.streaming
                && endpoint.tools
                && !endpoint.images
                && !endpoint.audio
                && endpoint.unsupported_fields_rejected
        }));
        let route_paths = manifest
            .endpoints
            .iter()
            .map(|endpoint| endpoint.path.as_str())
            .chain(manifest.lifecycle_paths.values().map(String::as_str))
            .collect::<Vec<_>>();
        for forbidden in ["shell", "files", "git", "mcp", "workspace", "tools"] {
            assert!(
                route_paths.iter().all(|path| !path.contains(forbidden)),
                "forbidden route family {forbidden}"
            );
        }
    }

    #[test]
    fn openai_chat_tools_structured_output_and_errors_translate() {
        let body = json!({
            "model":"local-model",
            "messages":[
                {"role":"system","content":"Be concise"},
                {"role":"user","content":[{"type":"text","text":"weather?"}]},
                {"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Stockholm\"}"}}]},
                {"role":"tool","tool_call_id":"call-1","content":"sunny"}
            ],
            "tools":[{"type":"function","function":{"name":"weather","description":"Weather","parameters":{"type":"object","properties":{"city":{"type":"string"}}},"strict":true}}],
            "max_completion_tokens":128,
            "temperature":0.2,
            "stream":true,
            "response_format":{"type":"json_schema","json_schema":{"name":"answer","strict":true,"schema":{"type":"object","properties":{"answer":{"type":"string"}}}}}
        });
        let request = translate_request(
            CompatibilityProtocol::OpenAiChatCompletions,
            "request-chat",
            &serde_json::to_vec(&body).expect("body"),
        )
        .expect("translate chat");
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.tools.len(), 1);
        assert!(request.tools[0].strict);
        assert_eq!(request.max_output_tokens, 128);
        assert!(request.stream);
        assert!(request.response_schema.is_some());
        assert!(matches!(
            request.messages[2].content[0],
            CanonicalContent::ToolUse { .. }
        ));
        assert!(matches!(
            request.messages[3].content[0],
            CanonicalContent::ToolResult { .. }
        ));

        let image = json!({
            "model":"local-model",
            "messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.test/image.png"}}]}]
        });
        assert!(matches!(
            translate_request(
                CompatibilityProtocol::OpenAiChatCompletions,
                "request-image",
                &serde_json::to_vec(&image).expect("body")
            ),
            Err(CompatibilityError::Unsupported { .. })
        ));
        let unknown =
            json!({"model":"local-model","messages":[{"role":"user","content":"x"}],"n":2});
        assert!(matches!(
            translate_request(
                CompatibilityProtocol::OpenAiChatCompletions,
                "request-unknown",
                &serde_json::to_vec(&unknown).expect("body")
            ),
            Err(CompatibilityError::Unsupported { .. })
        ));
    }

    #[test]
    fn responses_and_anthropic_tool_fixtures_translate_and_encode() {
        let responses = json!({
            "model":"local-model",
            "instructions":"Be useful",
            "input":[
                {"type":"message","role":"user","content":[{"type":"input_text","text":"Use tool"}]},
                {"type":"function_call","call_id":"call-1","name":"lookup","arguments":"{\"q\":\"x\"}"},
                {"type":"function_call_output","call_id":"call-1","output":"result"}
            ],
            "tools":[{"type":"function","name":"lookup","description":"Lookup","parameters":{"type":"object"},"strict":true}],
            "text":{"format":{"type":"json_schema","name":"result","schema":{"type":"object"},"strict":true}},
            "max_output_tokens":64,
            "stream":true
        });
        let request = translate_request(
            CompatibilityProtocol::OpenAiResponses,
            "request-responses",
            &serde_json::to_vec(&responses).expect("responses body"),
        )
        .expect("translate responses");
        assert_eq!(request.messages.len(), 4);
        assert!(request.tools[0].strict);
        assert!(request.response_schema.is_some());

        let anthropic = json!({
            "model":"local-model",
            "system":[{"type":"text","text":"Be useful"}],
            "messages":[
                {"role":"user","content":[{"type":"text","text":"Use tool"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"call-1","name":"lookup","input":{"q":"x"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":[{"type":"text","text":"result"}],"is_error":false}]}
            ],
            "tools":[{"name":"lookup","description":"Lookup","input_schema":{"type":"object"}}],
            "max_tokens":64,
            "stream":true
        });
        let request = translate_request(
            CompatibilityProtocol::AnthropicMessages,
            "request-anthropic",
            &serde_json::to_vec(&anthropic).expect("anthropic body"),
        )
        .expect("translate anthropic");
        assert_eq!(request.messages.len(), 4);
        assert_eq!(request.tools.len(), 1);
        assert!(!request.tools[0].strict);
        assert!(matches!(
            request.messages[2].content[0],
            CanonicalContent::ToolUse { .. }
        ));

        let response = CanonicalInferenceResponse {
            response_id: "response-1".to_string(),
            model: "local-model".to_string(),
            content: vec![
                CanonicalContent::Text {
                    text: "hello".to_string(),
                },
                CanonicalContent::ToolUse {
                    id: "call-2".to_string(),
                    name: "lookup".to_string(),
                    input: json!({"q":"y"}),
                },
            ],
            finish_reason: "tool_use".to_string(),
            usage: CanonicalUsage {
                input_tokens: 10,
                output_tokens: 4,
            },
            created_at_seconds: 1_700_000_000,
        };
        let openai = encode_response(CompatibilityProtocol::OpenAiResponses, &response)
            .expect("encode responses");
        assert_eq!(openai["object"], "response");
        assert_eq!(openai["output"].as_array().expect("output").len(), 2);
        let anthropic = encode_response(CompatibilityProtocol::AnthropicMessages, &response)
            .expect("encode anthropic");
        assert_eq!(anthropic["stop_reason"], "tool_use");

        let missing_anthropic_limit = json!({
            "model":"local-model",
            "messages":[{"role":"user","content":"hello"}]
        });
        assert!(matches!(
            translate_request(
                CompatibilityProtocol::AnthropicMessages,
                "request-anthropic-limit",
                &serde_json::to_vec(&missing_anthropic_limit).expect("body")
            ),
            Err(CompatibilityError::InvalidRequest { path, .. }) if path == "$.max_tokens"
        ));

        let empty_responses_input = json!({"model":"local-model","input":[]});
        assert!(translate_request(
            CompatibilityProtocol::OpenAiResponses,
            "request-empty-input",
            &serde_json::to_vec(&empty_responses_input).expect("body")
        )
        .is_err());

        let ignored_field = json!({
            "model":"local-model",
            "messages":[{"role":"user","content":"hello"}],
            "max_tokens":16,
            "top_k":4
        });
        assert!(matches!(
            translate_request(
                CompatibilityProtocol::AnthropicMessages,
                "request-ignored-field",
                &serde_json::to_vec(&ignored_field).expect("body")
            ),
            Err(CompatibilityError::Unsupported { .. })
        ));
    }

    #[test]
    fn every_protocol_stream_fixture_emits_valid_sse_and_terminal_events() {
        let events = vec![
            CanonicalStreamEvent::ResponseStart {
                response_id: "response-1".to_string(),
                model: "local-model".to_string(),
                created_at_seconds: 1_700_000_000,
            },
            CanonicalStreamEvent::TextStart { index: 0 },
            CanonicalStreamEvent::TextDelta {
                index: 0,
                text: "hel".to_string(),
            },
            CanonicalStreamEvent::TextEnd { index: 0 },
            CanonicalStreamEvent::ToolCallStart {
                index: 1,
                call_id: "call-1".to_string(),
                name: "lookup".to_string(),
            },
            CanonicalStreamEvent::ToolCallArgumentsDelta {
                index: 1,
                call_id: "call-1".to_string(),
                json_delta: "{\"q\":".to_string(),
            },
            CanonicalStreamEvent::ToolCallEnd {
                index: 1,
                call_id: "call-1".to_string(),
            },
            CanonicalStreamEvent::ResponseCompleted {
                response_id: "response-1".to_string(),
                finish_reason: "tool_use".to_string(),
                usage: CanonicalUsage {
                    input_tokens: 5,
                    output_tokens: 2,
                },
            },
        ];
        for protocol in [
            CompatibilityProtocol::OpenAiChatCompletions,
            CompatibilityProtocol::OpenAiResponses,
            CompatibilityProtocol::AnthropicMessages,
        ] {
            let frames = events
                .iter()
                .flat_map(|event| encode_stream_event(protocol, event).expect("stream event"))
                .collect::<Vec<_>>();
            assert!(!frames.is_empty());
            for frame in &frames {
                let bytes = frame.to_sse_bytes();
                let text = std::str::from_utf8(&bytes).expect("SSE UTF-8");
                assert!(text.ends_with("\n\n"));
                assert!(text.contains("data: "));
            }
            match protocol {
                CompatibilityProtocol::OpenAiChatCompletions => {
                    assert_eq!(frames.last().expect("last").data, "[DONE]");
                }
                CompatibilityProtocol::OpenAiResponses => {
                    assert_eq!(
                        frames.last().expect("last").event.as_deref(),
                        Some("response.completed")
                    );
                }
                CompatibilityProtocol::AnthropicMessages => {
                    assert_eq!(
                        frames.last().expect("last").event.as_deref(),
                        Some("message_stop")
                    );
                }
            }
        }
    }

    #[test]
    fn lan_policy_fails_closed_without_exact_interface_tls_auth_pairing_and_cors() {
        assert!(LanServerPolicy::default().validate().is_ok());
        assert!(lan_policy().validate().is_ok());

        let mut wildcard = lan_policy();
        wildcard.bind_address = "0.0.0.0".to_string();
        assert!(wildcard.validate().is_err());

        let mut no_tls = lan_policy();
        no_tls.tls = TlsPolicy::Disabled;
        assert!(no_tls.validate().is_err());

        let mut no_auth = lan_policy();
        no_auth.require_authentication = false;
        assert!(no_auth.validate().is_err());

        let mut no_pairing = lan_policy();
        no_pairing.pairing_required = false;
        assert!(no_pairing.validate().is_err());

        let mut wildcard_cors = lan_policy();
        wildcard_cors.cors_allowlist = vec!["*".to_string()];
        assert!(wildcard_cors.validate().is_err());

        let mut cloud = lan_policy();
        cloud.allowed_backends.insert(ApiBackend::CloudProvider);
        cloud.allow_cloud_providers_over_lan = true;
        assert!(cloud.validate().is_err());
    }

    #[test]
    fn pairing_tokens_are_digest_only_durable_scoped_rate_limited_and_revocable() {
        let directory = TestDirectory::new("lan-flow");
        let policy = lan_policy();
        let controller = make_controller(&directory.0, policy.clone(), 7);
        let challenge = controller
            .begin_pairing(pairing_request(), 1_000, "192.168.1.20")
            .expect("begin pairing");
        let paired = controller
            .complete_pairing(
                &challenge.challenge_id,
                &challenge.pairing_code,
                1_100,
                "192.168.1.20",
            )
            .expect("complete pairing");
        assert!(paired.token.starts_with(TOKEN_PREFIX));
        assert!(!paired.record.scopes.is_empty());

        let persisted = fs::read_dir(&directory.0)
            .expect("state dir")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read(entry.path()).ok())
            .flatten()
            .collect::<Vec<_>>();
        let persisted = String::from_utf8_lossy(&persisted);
        assert!(!persisted.contains(&paired.token));
        assert!(!persisted.contains(&challenge.pairing_code));

        let authorization = |now_ms| AuthorizationRequest {
            bearer_token: paired.token.clone(),
            scope: ApiScope::ChatCompletions,
            backend: ApiBackend::ManagedLocal,
            model_id: Some("local-model".to_string()),
            input_bytes: 100,
            remote_address: "192.168.1.20".to_string(),
            destructive_confirmation: None,
            now_ms,
        };
        controller
            .authorize(&authorization(1_200))
            .expect("first request");
        controller
            .authorize(&authorization(1_300))
            .expect("second request");
        assert!(matches!(
            controller.authorize(&authorization(1_400)),
            Err(CompatibilityError::RateLimited { .. })
        ));

        let reopened = make_controller(&directory.0, policy, 80);
        assert_eq!(reopened.list_tokens().expect("tokens").len(), 1);
        reopened
            .authorize(&authorization(2_300))
            .expect("new rate window after reopen");
        let revoked = reopened
            .revoke_token(&paired.record.token_id, 2_400, "192.168.1.50")
            .expect("revoke");
        assert_eq!(revoked.revoked_at_ms, Some(2_400));
        assert!(matches!(
            reopened.authorize(&authorization(2_500)),
            Err(CompatibilityError::Forbidden(_))
        ));
        let audit = reopened.audit_events().expect("audit");
        assert!(audit
            .iter()
            .any(|event| event.kind == SecurityAuditKind::PairingCompleted));
        assert!(audit
            .iter()
            .any(|event| event.kind == SecurityAuditKind::TokenRateLimited));
        assert!(audit
            .iter()
            .any(|event| event.kind == SecurityAuditKind::TokenRevoked));
    }

    #[test]
    fn pairing_replay_and_bruteforce_are_rejected() {
        let directory = TestDirectory::new("pairing-replay");
        let controller = make_controller(&directory.0, lan_policy(), 10);
        let challenge = controller
            .begin_pairing(pairing_request(), 1_000, "192.168.1.20")
            .expect("begin");
        for attempt in 0..MAX_PAIRING_ATTEMPTS {
            assert!(matches!(
                controller.complete_pairing(
                    &challenge.challenge_id,
                    &format!("{attempt:08}"),
                    1_100 + u64::from(attempt),
                    "192.168.1.20"
                ),
                Err(CompatibilityError::Unauthorized(_))
            ));
        }
        assert!(controller
            .complete_pairing(
                &challenge.challenge_id,
                &challenge.pairing_code,
                1_200,
                "192.168.1.20"
            )
            .is_err());

        let next = controller
            .begin_pairing(pairing_request(), 2_000, "192.168.1.20")
            .expect("next pairing");
        controller
            .complete_pairing(
                &next.challenge_id,
                &next.pairing_code,
                2_100,
                "192.168.1.20",
            )
            .expect("complete once");
        assert!(controller
            .complete_pairing(
                &next.challenge_id,
                &next.pairing_code,
                2_200,
                "192.168.1.20"
            )
            .is_err());
    }

    #[test]
    fn lifecycle_scope_confirmation_and_model_limits_are_enforced() {
        let directory = TestDirectory::new("lifecycle-scope");
        let controller = make_controller(&directory.0, lan_policy(), 30);
        let challenge = controller
            .begin_pairing(pairing_request(), 1_000, "192.168.1.20")
            .expect("begin");
        let paired = controller
            .complete_pairing(
                &challenge.challenge_id,
                &challenge.pairing_code,
                1_100,
                "192.168.1.20",
            )
            .expect("pair");
        let mut request = AuthorizationRequest {
            bearer_token: paired.token,
            scope: ApiScope::ModelDelete,
            backend: ApiBackend::ManagedLocal,
            model_id: Some("local-model".to_string()),
            input_bytes: 10,
            remote_address: "192.168.1.20".to_string(),
            destructive_confirmation: None,
            now_ms: 1_200,
        };
        assert!(matches!(
            controller.authorize(&request),
            Err(CompatibilityError::Forbidden(_))
        ));
        request.destructive_confirmation = Some("DELETE wrong-model".to_string());
        request.now_ms += 1;
        assert!(controller.authorize(&request).is_err());
        request.destructive_confirmation = Some("DELETE local-model".to_string());
        request.now_ms += 1;
        controller.authorize(&request).expect("confirmed deletion");

        request.scope = ApiScope::ChatCompletions;
        request.destructive_confirmation = None;
        request.model_id = Some("other-model".to_string());
        request.now_ms += 1_100;
        assert!(matches!(
            controller.authorize(&request),
            Err(CompatibilityError::Forbidden(_))
        ));
    }

    #[test]
    fn authenticated_state_tamper_is_rejected_without_fallback() {
        let directory = TestDirectory::new("state-tamper");
        let controller = make_controller(&directory.0, lan_policy(), 50);
        controller
            .begin_pairing(pairing_request(), 1_000, "192.168.1.20")
            .expect("persist state");
        let mut state_files = fs::read_dir(&directory.0)
            .expect("state files")
            .filter_map(Result::ok)
            .filter(|entry| state_generation_from_name(&entry.file_name()).is_some())
            .collect::<Vec<_>>();
        state_files.sort_by_key(|entry| state_generation_from_name(&entry.file_name()));
        let latest = state_files.last().expect("latest state").path();
        let mut bytes = fs::read(&latest).expect("read state");
        let position = bytes
            .iter()
            .position(|byte| *byte == b'a')
            .expect("tamperable byte");
        bytes[position] = b'b';
        fs::write(&latest, bytes).expect("tamper state");
        assert!(matches!(
            controller.audit_events(),
            Err(CompatibilityError::StateProtection(_))
                | Err(CompatibilityError::Json(_))
                | Err(CompatibilityError::CorruptState(_))
        ));
    }

    // -- Phase 8 item 10: tool-call and structured-output parser hardening --
    //
    // `request_offers_tool` is the shared guard every response-construction
    // site (m3_production.rs, m3_runtime_hub.rs) calls before turning a
    // model's tool call into an executable `CanonicalContent::ToolUse`. It
    // must say yes only for names the request actually advertised.

    fn tool_request(tools: &[&str]) -> CanonicalInferenceRequest {
        CanonicalInferenceRequest {
            schema_version: COMPATIBILITY_SCHEMA_VERSION,
            protocol: CompatibilityProtocol::OpenAiChatCompletions,
            request_id: "request-tools".to_string(),
            model: "local-model".to_string(),
            messages: vec![CanonicalMessage {
                role: CanonicalRole::User,
                content: vec![CanonicalContent::Text {
                    text: "hi".to_string(),
                }],
            }],
            tools: tools
                .iter()
                .map(|name| CanonicalToolDefinition {
                    name: name.to_string(),
                    description: "test tool".to_string(),
                    input_schema: json!({"type":"object","properties":{}}),
                    strict: false,
                })
                .collect(),
            max_output_tokens: 32,
            temperature: None,
            stream: false,
            response_schema: None,
            metadata: Value::Null,
        }
    }

    #[test]
    fn request_offers_tool_matches_only_advertised_names() {
        let request = tool_request(&["weather", "search"]);
        assert!(request_offers_tool(&request, "weather"));
        assert!(request_offers_tool(&request, "search"));
        assert!(!request_offers_tool(&request, "shell_exec"));
        assert!(!request_offers_tool(&request, ""));
        assert!(!request_offers_tool(&request, "Weather"));

        let no_tools = tool_request(&[]);
        assert!(!request_offers_tool(&no_tools, "weather"));
    }

    /// Regression fixtures per malformed-input family: syntactically-broken
    /// JSON bodies (the "almost valid JSON" case: trailing commas, single
    /// quotes, unescaped control characters, and a body truncated mid-token
    /// as a dropped connection would leave it) must all fail the same way —
    /// a clean `CompatibilityError::Json`, never a panic and never a
    /// partially-built request.
    #[test]
    fn malformed_json_mode_request_bodies_fail_cleanly_not_silently() {
        struct Fixture {
            name: &'static str,
            body: &'static [u8],
        }
        let fixtures = [
            Fixture {
                name: "trailing_comma",
                body: br#"{"model":"local-model","messages":[{"role":"user","content":"hi"}],}"#,
            },
            Fixture {
                name: "single_quotes",
                body: br#"{'model':'local-model','messages':[{'role':'user','content':'hi'}]}"#,
            },
            Fixture {
                name: "unescaped_control_character",
                body: b"{\"model\":\"local-model\",\"messages\":[{\"role\":\"user\",\"content\":\"broken\ncontrol\"}]}",
            },
            Fixture {
                name: "truncated_mid_string",
                body: br#"{"model":"local-model","messages":[{"role":"user","content":"h"#,
            },
            Fixture {
                name: "truncated_mid_token",
                body: br#"{"model":"local-model","messages":[{"role":"user","content":"hi"}],"stream":tr"#,
            },
        ];
        for fixture in fixtures {
            let result = translate_request(
                CompatibilityProtocol::OpenAiChatCompletions,
                "request-adversarial",
                fixture.body,
            );
            assert!(
                matches!(result, Err(CompatibilityError::Json(_))),
                "fixture {:?} should fail as a clean JSON error, got {result:?}",
                fixture.name
            );
        }
    }

    /// Schema outputs that parse as JSON but violate the shape this app
    /// actually advertises (missing required field, wrong top-level type,
    /// duplicate tool names) must be rejected with a structured
    /// `InvalidRequest`/`Unsupported` error rather than silently accepted.
    #[test]
    fn structured_output_schema_violations_are_rejected_with_structured_errors() {
        struct Fixture {
            name: &'static str,
            body: Value,
        }
        let fixtures = [
            Fixture {
                name: "json_schema_missing_schema_field",
                body: json!({
                    "model":"local-model",
                    "messages":[{"role":"user","content":"hi"}],
                    "response_format":{"type":"json_schema","json_schema":{"name":"answer"}}
                }),
            },
            Fixture {
                name: "json_schema_top_level_not_object",
                body: json!({
                    "model":"local-model",
                    "messages":[{"role":"user","content":"hi"}],
                    "response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":{"type":"array"}}}
                }),
            },
            Fixture {
                name: "duplicate_tool_names",
                body: json!({
                    "model":"local-model",
                    "messages":[{"role":"user","content":"hi"}],
                    "tools":[
                        {"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}},
                        {"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}
                    ]
                }),
            },
            Fixture {
                name: "tool_call_arguments_not_an_object",
                body: json!({
                    "model":"local-model",
                    "messages":[
                        {"role":"assistant","content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"weather","arguments":"42"}}]}
                    ]
                }),
            },
        ];
        for fixture in fixtures {
            let result = translate_request(
                CompatibilityProtocol::OpenAiChatCompletions,
                "request-schema-violation",
                &serde_json::to_vec(&fixture.body).expect("fixture body"),
            );
            assert!(
                matches!(
                    result,
                    Err(CompatibilityError::InvalidRequest { .. })
                        | Err(CompatibilityError::Unsupported { .. })
                ),
                "fixture {:?} should fail with a structured error, got {result:?}",
                fixture.name
            );
        }
    }
}

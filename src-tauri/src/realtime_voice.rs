//! Secret-bearing broker for desktop realtime voice sessions.
//!
//! The WebView owns WebRTC media and the data channel, but never receives the
//! ordinary provider credential. It sends an SDP offer here; this module reads
//! the OpenAI key from the existing keychain boundary and posts the offer to a
//! single compiled-in origin. Only the SDP answer crosses back into JavaScript.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const OPENAI_REALTIME_CALLS_URL: &str = "https://api.openai.com/v1/realtime/calls";
const MAX_SDP_BYTES: usize = 128 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1024;
const MAX_TOOLS_BYTES: usize = 256 * 1024;
const MAX_TOOLS: usize = 128;
const MAX_METRICS: usize = 100;
const METRICS_FILE: &str = "realtime-voice-metrics-v1.json";

pub struct RealtimeVoiceState {
    active_sessions: Mutex<BTreeSet<String>>,
    metrics: Mutex<Vec<RealtimeVoiceMetric>>,
    root: Option<PathBuf>,
}

impl RealtimeVoiceState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(app_data_dir)
            .map_err(|error| format!("Could not create realtime metrics directory: {error}"))?;
        let path = app_data_dir.join(METRICS_FILE);
        let mut metrics = if path.exists() {
            let bytes = fs::read(&path)
                .map_err(|error| format!("Could not read realtime metrics: {error}"))?;
            serde_json::from_slice::<Vec<RealtimeVoiceMetric>>(&bytes).unwrap_or_default()
        } else {
            Vec::new()
        };
        if metrics.len() > MAX_METRICS {
            metrics.drain(0..metrics.len() - MAX_METRICS);
        }
        Ok(Self {
            active_sessions: Mutex::new(BTreeSet::new()),
            metrics: Mutex::new(metrics),
            root: Some(app_data_dir.to_path_buf()),
        })
    }

    pub fn active_session_count(&self) -> Result<usize, String> {
        Ok(lock(&self.active_sessions, "realtime sessions")?.len())
    }

    fn persist_metrics(&self, metrics: &[RealtimeVoiceMetric]) -> Result<(), String> {
        let Some(root) = &self.root else {
            return Ok(());
        };
        let path = root.join(METRICS_FILE);
        let bytes = serde_json::to_vec(metrics)
            .map_err(|error| format!("Could not encode realtime metrics: {error}"))?;
        crate::m4_runtime::atomic_write_private(&path, &bytes, true)
            .map_err(|error| format!("Could not persist realtime metrics: {error}"))
    }
}

impl Default for RealtimeVoiceState {
    fn default() -> Self {
        Self {
            active_sessions: Mutex::new(BTreeSet::new()),
            metrics: Mutex::new(Vec::new()),
            root: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RealtimeVoiceConnectRequest {
    pub session_id: String,
    pub provider_id: String,
    pub model: String,
    pub voice: String,
    pub turn_detection: String,
    pub sdp: String,
    pub instructions: String,
    pub tools: Vec<Value>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeVoiceConnectResponse {
    pub session_id: String,
    pub sdp_answer: String,
    pub provider_request_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeVoiceStatus {
    pub provider_id: String,
    pub configured: bool,
    pub active_sessions: usize,
    pub endpoint: &'static str,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RealtimeVoiceMetric {
    pub created_at_ms: u64,
    pub connection_ms: Option<u64>,
    pub first_recognized_speech_ms: Option<u64>,
    pub first_model_event_ms: Option<u64>,
    pub first_audio_ms: Option<u64>,
    pub end_to_end_ms: Option<u64>,
    pub interrupted: bool,
    pub reconnect_count: u32,
    pub error_code: Option<String>,
    pub tool_round_trip_ms: Vec<u64>,
    pub output_underruns: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RealtimeVoiceMetricsSnapshot {
    pub metrics: Vec<RealtimeVoiceMetric>,
    pub interrupt_count: usize,
    pub reconnect_count: u64,
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock was poisoned"))
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err(format!("Invalid {label}"));
    }
    Ok(())
}

fn validate_request(request: &RealtimeVoiceConnectRequest) -> Result<(), String> {
    validate_id("realtime session id", &request.session_id)?;
    if request.provider_id != "openai" {
        return Err("Only the fixed OpenAI realtime provider is supported".to_string());
    }
    if request.model.is_empty()
        || request.model.len() > 128
        || !request
            .model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("Invalid realtime model".to_string());
    }
    if !matches!(
        request.voice.as_str(),
        "alloy"
            | "ash"
            | "ballad"
            | "coral"
            | "echo"
            | "sage"
            | "shimmer"
            | "verse"
            | "marin"
            | "cedar"
    ) {
        return Err("Unsupported realtime voice".to_string());
    }
    if !matches!(request.turn_detection.as_str(), "semantic_vad" | "manual") {
        return Err("Unsupported realtime turn detection mode".to_string());
    }
    if request.sdp.is_empty() || request.sdp.len() > MAX_SDP_BYTES {
        return Err("SDP offer is empty or exceeds its limit".to_string());
    }
    if request.instructions.len() > MAX_INSTRUCTIONS_BYTES {
        return Err("Realtime instructions exceed their limit".to_string());
    }
    if request.tools.len() > MAX_TOOLS {
        return Err("Too many realtime tools".to_string());
    }
    let tools_size = serde_json::to_vec(&request.tools)
        .map_err(|_| "Realtime tools are not serializable".to_string())?
        .len();
    if tools_size > MAX_TOOLS_BYTES {
        return Err("Realtime tools exceed their limit".to_string());
    }
    for tool in &request.tools {
        let object = tool
            .as_object()
            .ok_or_else(|| "Realtime tool definition must be an object".to_string())?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let valid_name = !name.is_empty()
            && name.len() <= 128
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        let valid_description = match object.get("description") {
            None => true,
            Some(Value::String(value)) => value.len() <= 4096,
            Some(_) => false,
        };
        if object.get("type").and_then(Value::as_str) != Some("function")
            || !valid_name
            || !valid_description
            || !object.get("parameters").is_some_and(Value::is_object)
        {
            return Err("Malformed realtime tool definition".to_string());
        }
    }
    Ok(())
}

fn session_payload(request: &RealtimeVoiceConnectRequest) -> Value {
    let turn_detection = if request.turn_detection == "manual" {
        Value::Null
    } else {
        json!({
            "type": "semantic_vad",
            "create_response": true,
            "interrupt_response": true
        })
    };
    json!({
        "type": "realtime",
        "model": request.model,
        "output_modalities": ["audio"],
        "instructions": request.instructions,
        "tools": request.tools,
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "audio": {
            "input": {
                "transcription": { "model": "gpt-4o-mini-transcribe" },
                "turn_detection": turn_detection
            },
            "output": { "voice": request.voice }
        }
    })
}

#[tauri::command]
pub async fn realtime_voice_connect(
    state: tauri::State<'_, RealtimeVoiceState>,
    request: RealtimeVoiceConnectRequest,
) -> Result<RealtimeVoiceConnectResponse, String> {
    validate_request(&request)?;
    let api_key = crate::providers::read_key("openai")?;
    let session = serde_json::to_string(&session_payload(&request))
        .map_err(|error| format!("Could not encode realtime session: {error}"))?;
    let form = reqwest::multipart::Form::new()
        .text("sdp", request.sdp.clone())
        .text("session", session);
    let client = crate::egress::hardened()
        .build()
        .map_err(|error| format!("Could not initialize realtime transport: {error}"))?;
    let mut response = crate::egress::send(
        client
            .post(OPENAI_REALTIME_CALLS_URL)
            .bearer_auth(api_key)
            .multipart(form),
    )
    .await
    .map_err(|error| format!("Realtime provider connection failed: {error}"))?;
    let status = response.status();
    let request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    if !status.is_success() {
        let safe_detail = if status.as_u16() == 401 || status.as_u16() == 403 {
            "The saved OpenAI credential was rejected".to_string()
        } else {
            format!("OpenAI returned HTTP {}", status.as_u16())
        };
        return Err(safe_detail);
    }
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SDP_BYTES as u64)
    {
        return Err("Realtime provider returned an oversized SDP answer".to_string());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| format!("Could not read realtime provider response: {error}"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_SDP_BYTES {
            return Err("Realtime provider returned an oversized SDP answer".to_string());
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8(body)
        .map_err(|_| "Realtime provider returned a non-text SDP answer".to_string())?;
    if !body.starts_with("v=0") {
        return Err("Realtime provider returned an invalid SDP answer".to_string());
    }
    lock(&state.active_sessions, "realtime sessions")?.insert(request.session_id.clone());
    Ok(RealtimeVoiceConnectResponse {
        session_id: request.session_id,
        sdp_answer: body,
        provider_request_id: request_id,
    })
}

#[tauri::command]
pub fn realtime_voice_disconnect(
    state: tauri::State<'_, RealtimeVoiceState>,
    session_id: String,
) -> Result<(), String> {
    validate_id("realtime session id", &session_id)?;
    lock(&state.active_sessions, "realtime sessions")?.remove(&session_id);
    Ok(())
}

#[tauri::command]
pub fn realtime_voice_status(
    state: tauri::State<'_, RealtimeVoiceState>,
) -> Result<RealtimeVoiceStatus, String> {
    Ok(RealtimeVoiceStatus {
        provider_id: "openai".to_string(),
        configured: crate::providers::has_key("openai"),
        active_sessions: lock(&state.active_sessions, "realtime sessions")?.len(),
        endpoint: OPENAI_REALTIME_CALLS_URL,
    })
}

#[tauri::command]
pub fn realtime_voice_metric_record(
    state: tauri::State<'_, RealtimeVoiceState>,
    metric: RealtimeVoiceMetric,
) -> Result<(), String> {
    record_metric(state.inner(), metric)
}

fn record_metric(
    state: &RealtimeVoiceState,
    mut metric: RealtimeVoiceMetric,
) -> Result<(), String> {
    metric.created_at_ms = metric.created_at_ms.min(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
    );
    if metric.error_code.as_ref().is_some_and(|value| {
        value.len() > 64
            || !value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    }) {
        return Err("Invalid realtime metric error code".to_string());
    }
    if metric.tool_round_trip_ms.len() > 64
        || metric
            .tool_round_trip_ms
            .iter()
            .any(|duration| *duration > 10 * 60 * 1_000)
    {
        return Err("Invalid realtime tool latency metrics".to_string());
    }
    let mut metrics = lock(&state.metrics, "realtime metrics")?;
    metrics.push(metric);
    if metrics.len() > MAX_METRICS {
        let overflow = metrics.len() - MAX_METRICS;
        metrics.drain(0..overflow);
    }
    let snapshot = metrics.clone();
    drop(metrics);
    state.persist_metrics(&snapshot)
}

#[tauri::command]
pub fn realtime_voice_metrics(
    state: tauri::State<'_, RealtimeVoiceState>,
) -> Result<RealtimeVoiceMetricsSnapshot, String> {
    let metrics = lock(&state.metrics, "realtime metrics")?.clone();
    Ok(RealtimeVoiceMetricsSnapshot {
        interrupt_count: metrics.iter().filter(|item| item.interrupted).count(),
        reconnect_count: metrics.iter().map(|item| item.reconnect_count as u64).sum(),
        metrics,
    })
}

#[tauri::command]
pub fn realtime_voice_metrics_clear(
    state: tauri::State<'_, RealtimeVoiceState>,
) -> Result<(), String> {
    lock(&state.metrics, "realtime metrics")?.clear();
    state.persist_metrics(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_request() -> RealtimeVoiceConnectRequest {
        RealtimeVoiceConnectRequest {
            session_id: "rv_test-1".to_string(),
            provider_id: "openai".to_string(),
            model: "gpt-realtime-2.1".to_string(),
            voice: "marin".to_string(),
            turn_detection: "semantic_vad".to_string(),
            sdp: "v=0\r\n".to_string(),
            instructions: "Be concise.".to_string(),
            tools: vec![json!({
                "type": "function",
                "name": "read_file",
                "description": "Read a file",
                "parameters": {"type": "object"}
            })],
        }
    }

    #[test]
    fn accepts_only_fixed_provider_and_supported_session_values() {
        let mut request = valid_request();
        validate_request(&request).unwrap();
        request.provider_id = "custom".to_string();
        assert!(validate_request(&request).is_err());
        request = valid_request();
        request.voice = "unknown".to_string();
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn rejects_malformed_tool_metadata() {
        let mut request = valid_request();
        request.tools[0]["name"] = json!("bad tool name");
        assert!(validate_request(&request).is_err());
        request = valid_request();
        request.tools[0]["description"] = json!(42);
        assert!(validate_request(&request).is_err());
    }

    #[test]
    fn payload_enables_transcription_and_server_interruption() {
        let payload = session_payload(&valid_request());
        assert_eq!(payload["type"], "realtime");
        assert_eq!(payload["parallel_tool_calls"], false);
        assert_eq!(
            payload["audio"]["input"]["turn_detection"]["interrupt_response"],
            true
        );
        assert_eq!(
            payload["audio"]["input"]["transcription"]["model"],
            "gpt-4o-mini-transcribe"
        );
        assert!(payload.to_string().find("Bearer").is_none());
    }

    #[test]
    fn manual_mode_disables_server_vad() {
        let mut request = valid_request();
        request.turn_detection = "manual".to_string();
        assert!(session_payload(&request)["audio"]["input"]["turn_detection"].is_null());
    }

    #[test]
    fn diagnostic_metrics_are_bounded_and_reject_unstructured_error_text() {
        let state = RealtimeVoiceState::default();
        for index in 0..(MAX_METRICS + 7) {
            record_metric(
                &state,
                RealtimeVoiceMetric {
                    created_at_ms: index as u64,
                    connection_ms: Some(10),
                    first_recognized_speech_ms: None,
                    first_model_event_ms: None,
                    first_audio_ms: None,
                    end_to_end_ms: None,
                    interrupted: false,
                    reconnect_count: 0,
                    error_code: None,
                    tool_round_trip_ms: Vec::new(),
                    output_underruns: 0,
                    input_tokens: 0,
                    output_tokens: 0,
                },
            )
            .unwrap();
        }
        assert_eq!(lock(&state.metrics, "test").unwrap().len(), MAX_METRICS);
        assert!(record_metric(
            &state,
            RealtimeVoiceMetric {
                created_at_ms: 0,
                connection_ms: None,
                first_recognized_speech_ms: None,
                first_model_event_ms: None,
                first_audio_ms: None,
                end_to_end_ms: None,
                interrupted: false,
                reconnect_count: 0,
                error_code: Some("Bearer secret must not be a metric".to_string()),
                tool_round_trip_ms: Vec::new(),
                output_underruns: 0,
                input_tokens: 0,
                output_tokens: 0,
            },
        )
        .is_err());
    }

    #[test]
    fn diagnostic_metrics_survive_restart_without_audio_or_text_fields() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-realtime-metrics-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let state = RealtimeVoiceState::production(&root).unwrap();
        let metric = RealtimeVoiceMetric {
            created_at_ms: 1,
            connection_ms: Some(20),
            first_recognized_speech_ms: Some(30),
            first_model_event_ms: Some(40),
            first_audio_ms: Some(50),
            end_to_end_ms: Some(60),
            interrupted: true,
            reconnect_count: 1,
            error_code: None,
            tool_round_trip_ms: vec![12],
            output_underruns: 0,
            input_tokens: 12,
            output_tokens: 8,
        };
        record_metric(&state, metric.clone()).unwrap();
        record_metric(
            &state,
            RealtimeVoiceMetric {
                created_at_ms: 2,
                ..metric
            },
        )
        .unwrap();
        let raw = fs::read_to_string(root.join(METRICS_FILE)).unwrap();
        assert!(!raw.contains("audioBase64"));
        assert!(!raw.contains("transcript"));
        let reloaded = RealtimeVoiceState::production(&root).unwrap();
        assert_eq!(lock(&reloaded.metrics, "test").unwrap().len(), 2);
        let _ = fs::remove_dir_all(root);
    }
}

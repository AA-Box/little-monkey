//! M7 desktop companion: explicit capture grants, local/BYOK speech, system
//! TTS, and user-owned image endpoints. Every media result is written to the
//! shared content-addressed artifact store before metadata is published.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::GlobalShortcutExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::artifact_store::{ArtifactBlob, ArtifactStore};

const CONFIG_SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE: &str = "companion-config-v1.json";
const GALLERY_FILE: &str = "image-gallery-v1.json";
const MAX_CAPTURE_TEXT_BYTES: usize = 4 * 1024 * 1024;
const MAX_MEDIA_BYTES: u64 = 256 * 1024 * 1024;
const MAX_TRANSCRIPT_BYTES: usize = 8 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_IMAGE_DIMENSION: u32 = 4096;
const MAX_IMAGE_STEPS: u32 = 200;
const MAX_GRANT_LIFETIME_MS: u64 = 60 * 60 * 1_000;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    Text,
    File,
    Window,
    Screen,
    Microphone,
    Meeting,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureGrant {
    pub grant_id: String,
    pub kind: CaptureKind,
    pub application_id: Option<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionArtifact {
    pub blob: ArtifactBlob,
    pub media_type: String,
    pub source: String,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptionBackendKind {
    LocalWhisper,
    Provider,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceConfig {
    pub backend: TranscriptionBackendKind,
    pub whisper_binary: Option<String>,
    pub whisper_model: Option<String>,
    pub provider_id: Option<String>,
    pub provider_model: String,
    pub language: String,
    pub tts_voice: Option<String>,
    pub save_raw_audio: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            backend: TranscriptionBackendKind::LocalWhisper,
            whisper_binary: None,
            whisper_model: None,
            provider_id: None,
            provider_model: "whisper-1".to_string(),
            language: "auto".to_string(),
            tts_voice: None,
            save_raw_audio: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ImageEndpointKind {
    ComfyUi,
    OpenAiCompatible,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageEndpointConfig {
    pub endpoint_id: String,
    pub label: String,
    pub kind: ImageEndpointKind,
    pub base_url: String,
    pub provider_id: Option<String>,
    pub workflow_template: Option<Value>,
    pub supports_editing: bool,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompanionConfig {
    pub schema_version: u32,
    pub overlay_shortcut: String,
    pub voice: VoiceConfig,
    pub image_endpoints: Vec<ImageEndpointConfig>,
}

impl Default for CompanionConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            overlay_shortcut: "CommandOrControl+Shift+Space".to_string(),
            voice: VoiceConfig::default(),
            image_endpoints: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptResult {
    pub job_id: String,
    pub text: String,
    pub segments: Vec<SpeakerSegment>,
    pub transcript: CompanionArtifact,
    pub raw_audio: Option<CompanionArtifact>,
    pub backend: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SpeakerSegment {
    pub speaker: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    pub text: String,
    pub confidence: Option<f64>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageGenerationRequest {
    pub job_id: String,
    pub endpoint_id: String,
    pub prompt: String,
    #[serde(default)]
    pub negative_prompt: String,
    pub model: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    pub seed: u64,
    pub source_artifact_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageGalleryEntry {
    pub entry_id: String,
    pub artifact_id: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub endpoint_id: String,
    pub endpoint_kind: ImageEndpointKind,
    pub model: String,
    pub prompt: String,
    pub negative_prompt: String,
    pub width: u32,
    pub height: u32,
    pub steps: u32,
    pub cfg_scale: f64,
    pub seed: u64,
    pub source_artifact_id: Option<String>,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ImageProgressEvent {
    job_id: String,
    phase: String,
    progress: f64,
}

pub struct M7CompanionState {
    root: PathBuf,
    config: Mutex<CompanionConfig>,
    grants: Mutex<BTreeMap<String, CaptureGrant>>,
    jobs: Mutex<BTreeMap<String, CancellationToken>>,
    gallery: Mutex<Vec<ImageGalleryEntry>>,
    artifacts: ArtifactStore,
}

impl M7CompanionState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        let root = app_data_dir.join("m7-companion-v1");
        ensure_private_directory(&root)?;
        ensure_private_directory(&root.join("tmp"))?;
        let config = load_json::<CompanionConfig>(&root.join(CONFIG_FILE))?.unwrap_or_default();
        validate_config(&config)?;
        let gallery =
            load_json::<Vec<ImageGalleryEntry>>(&root.join(GALLERY_FILE))?.unwrap_or_default();
        Ok(Self {
            root,
            config: Mutex::new(config),
            grants: Mutex::new(BTreeMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
            gallery: Mutex::new(gallery),
            artifacts: ArtifactStore::with_max_blob_size(
                app_data_dir.join("content-v1"),
                MAX_MEDIA_BYTES,
            )
            .map_err(|error| error.to_string())?,
        })
    }

    fn config(&self) -> Result<CompanionConfig, String> {
        Ok(lock(&self.config, "companion config")?.clone())
    }

    pub fn overlay_shortcut(&self) -> Result<String, String> {
        Ok(self.config()?.overlay_shortcut)
    }

    fn save_config(&self, config: CompanionConfig) -> Result<CompanionConfig, String> {
        validate_config(&config)?;
        atomic_write_json(&self.root.join(CONFIG_FILE), &config)?;
        *lock(&self.config, "companion config")? = config.clone();
        Ok(config)
    }

    /// Secret-free, read-only capture-grant snapshot for Security Doctor.
    /// Expiration is reflected in the returned copy without extending,
    /// revoking, or otherwise mutating a grant.
    pub fn security_grants(&self) -> Result<Vec<CaptureGrant>, String> {
        let now = now_ms();
        let grants = lock(&self.grants, "capture grants")?;
        Ok(grants
            .values()
            .cloned()
            .map(|mut grant| {
                if grant.expires_at_ms <= now {
                    grant.active = false;
                }
                grant
            })
            .collect())
    }

    fn require_grant(
        &self,
        grant_id: &str,
        allowed: &BTreeSet<CaptureKind>,
    ) -> Result<CaptureGrant, String> {
        validate_id("grantId", grant_id)?;
        let now = now_ms();
        let mut grants = lock(&self.grants, "capture grants")?;
        let grant = grants
            .get_mut(grant_id)
            .ok_or_else(|| "Capture grant is missing or was revoked".to_string())?;
        if grant.expires_at_ms <= now {
            grant.active = false;
        }
        if !grant.active || !allowed.contains(&grant.kind) {
            return Err(
                "Capture grant is inactive or does not cover this media source".to_string(),
            );
        }
        Ok(grant.clone())
    }

    fn begin_job(&self, job_id: &str) -> Result<CancellationToken, String> {
        validate_id("jobId", job_id)?;
        let token = CancellationToken::new();
        let mut jobs = lock(&self.jobs, "companion jobs")?;
        if jobs.contains_key(job_id) {
            return Err("A companion job with that id is already running".to_string());
        }
        jobs.insert(job_id.to_string(), token.clone());
        Ok(token)
    }

    fn finish_job(&self, job_id: &str) {
        if let Ok(mut jobs) = self.jobs.lock() {
            jobs.remove(job_id);
        }
    }

    fn publish(
        &self,
        bytes: &[u8],
        media_type: &str,
        source: impl Into<String>,
    ) -> Result<CompanionArtifact, String> {
        let blob = self
            .artifacts
            .put(bytes)
            .map_err(|error| error.to_string())?;
        Ok(CompanionArtifact {
            blob,
            media_type: media_type.to_string(),
            source: source.into(),
            created_at_ms: now_ms(),
        })
    }

    fn persist_gallery(&self, entries: &[ImageGalleryEntry]) -> Result<(), String> {
        atomic_write_json(&self.root.join(GALLERY_FILE), entries)
    }

    fn image_data_url(&self, artifact_id: &str, media_type: &str) -> Result<String, String> {
        validate_id("artifactId", artifact_id)?;
        if !media_type.starts_with("image/") || media_type.len() > 128 {
            return Err("Only bounded image artifacts can be previewed".to_string());
        }
        let bytes = self
            .artifacts
            .read(artifact_id)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 32 * 1024 * 1024 {
            return Err("Image preview exceeds 32 MiB".to_string());
        }
        Ok(format!(
            "data:{media_type};base64,{}",
            STANDARD.encode(bytes)
        ))
    }

    pub fn emergency_stop(&self) -> Result<(usize, usize), String> {
        let grants = {
            let mut grants = lock(&self.grants, "capture grants")?;
            let count = grants.values().filter(|grant| grant.active).count();
            for grant in grants.values_mut() {
                grant.active = false;
            }
            count
        };
        let jobs = {
            let mut jobs = lock(&self.jobs, "companion jobs")?;
            let count = jobs.len();
            for token in jobs.values() {
                token.cancel();
            }
            jobs.clear();
            count
        };
        Ok((grants, jobs))
    }
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock is poisoned"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn ensure_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("This companion capability can only be configured from the main window".to_string())
    }
}

fn ensure_companion_control_window(window: &tauri::Window) -> Result<(), String> {
    if matches!(window.label(), "main" | "companion-overlay") {
        Ok(())
    } else {
        Err(
            "Capture grants can only be changed from the main window or companion overlay"
                .to_string(),
        )
    }
}

fn validate_id(label: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || value.bytes().any(|byte| {
            !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        })
    {
        Err(format!("{label} must be a bounded safe identifier"))
    } else {
        Ok(())
    }
}

fn validate_absolute_regular(path: &str, executable: bool) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("Configured paths must be absolute".to_string());
    }
    let canonical = fs::canonicalize(path)
        .map_err(|error| format!("Could not resolve {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(&canonical)
        .map_err(|error| format!("Could not inspect {}: {error}", canonical.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} must be a regular file", canonical.display()));
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(format!("{} is not executable", canonical.display()));
        }
    }
    let _ = executable;
    Ok(canonical)
}

fn validate_url(value: &str) -> Result<(), String> {
    let parsed =
        url::Url::parse(value).map_err(|error| format!("Invalid endpoint URL: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(
            "Endpoint must be an http(s) origin/base path without credentials, query, or fragment"
                .to_string(),
        );
    }
    Ok(())
}

fn validate_config(config: &CompanionConfig) -> Result<(), String> {
    if config.schema_version != CONFIG_SCHEMA_VERSION
        || config.overlay_shortcut.is_empty()
        || config.overlay_shortcut.len() > 128
        || config.voice.provider_model.is_empty()
        || config.voice.provider_model.len() > 256
        || config.voice.language.len() > 32
    {
        return Err("Companion configuration is invalid".to_string());
    }
    config
        .overlay_shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|error| format!("Companion shortcut is invalid: {error}"))?;
    if let Some(binary) = &config.voice.whisper_binary {
        validate_absolute_regular(binary, true)?;
    }
    if let Some(model) = &config.voice.whisper_model {
        validate_absolute_regular(model, false)?;
    }
    let mut ids = BTreeSet::new();
    for endpoint in &config.image_endpoints {
        validate_id("endpointId", &endpoint.endpoint_id)?;
        if !ids.insert(endpoint.endpoint_id.clone())
            || endpoint.label.is_empty()
            || endpoint.label.len() > 128
        {
            return Err("Image endpoint ids must be unique and labels non-empty".to_string());
        }
        validate_url(&endpoint.base_url)?;
        if endpoint.kind == ImageEndpointKind::ComfyUi && endpoint.workflow_template.is_none() {
            return Err("ComfyUI endpoints require a workflow template".to_string());
        }
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("Could not create {}: {error}", path.display()))?;
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!("{} is not a real directory", path.display()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("Could not secure {}: {error}", path.display()))?;
    }
    Ok(())
}

fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>, String> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| format!("Could not decode {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("Could not read {}: {error}", path.display())),
    }
}

fn atomic_write_json<T: Serialize + ?Sized>(path: &Path, value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    let temp = path.with_extension(format!("tmp-{}", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("Could not stage {}: {error}", path.display()))?;
    file.write_all(&bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    fs::rename(&temp, path)
        .map_err(|error| format!("Could not publish {}: {error}", path.display()))
}

fn bounded_file(path: &str) -> Result<(PathBuf, Vec<u8>), String> {
    let canonical = validate_absolute_regular(path, false)?;
    let metadata = fs::metadata(&canonical).map_err(|error| error.to_string())?;
    if metadata.len() > MAX_MEDIA_BYTES {
        return Err(format!(
            "Selected media exceeds the {MAX_MEDIA_BYTES}-byte limit"
        ));
    }
    let bytes = fs::read(&canonical).map_err(|error| format!("Could not read media: {error}"))?;
    Ok((canonical, bytes))
}

fn configured_custom_providers() -> Vec<crate::providers::CustomProviderEntry> {
    crate::app_paths::data_dir()
        .and_then(|dir| fs::read_to_string(dir.join("providers.json")).ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.get("custom").cloned())
        .and_then(|value| {
            serde_json::from_value::<Vec<crate::providers::CustomProviderEntry>>(value).ok()
        })
        .unwrap_or_default()
}

pub fn show_overlay(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window("companion-overlay") {
        window.show().map_err(|error| error.to_string())?;
        window.set_focus().map_err(|error| error.to_string())?;
        return Ok(());
    }
    let window = tauri::WebviewWindowBuilder::new(
        app,
        "companion-overlay",
        tauri::WebviewUrl::App("index.html?overlay=1".into()),
    )
    .title("Little Monkey Companion")
    .inner_size(440.0, 560.0)
    .min_inner_size(360.0, 420.0)
    .always_on_top(true)
    .decorations(false)
    .resizable(true)
    .skip_taskbar(true)
    .build()
    .map_err(|error| error.to_string())?;

    if let Some(main) = app.get_webview_window("main") {
        if let Ok(Some(monitor)) = main.current_monitor() {
            let size = monitor.size();
            let position = monitor.position();
            let scale = monitor.scale_factor();
            let width = 440.0 * scale;
            let height = 560.0 * scale;
            let x = f64::from(position.x) + (f64::from(size.width) - width) / 2.0;
            let y = f64::from(position.y) + (f64::from(size.height) - height) / 3.0;
            let _ = window.set_position(tauri::PhysicalPosition::new(x as i32, y as i32));
        }
    }
    window.set_focus().map_err(|error| error.to_string())
}

#[tauri::command]
pub fn m7_overlay_show(app: tauri::AppHandle) -> Result<(), String> {
    show_overlay(&app)
}

#[tauri::command]
pub fn m7_overlay_hide(app: tauri::AppHandle, window: tauri::Window) -> Result<(), String> {
    if window.label() != "companion-overlay" {
        return Err("Only the companion overlay can close itself through this command".to_string());
    }
    window.hide().map_err(|error| error.to_string())?;
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
    Ok(())
}

#[tauri::command]
pub fn m7_overlay_submit(
    app: tauri::AppHandle,
    window: tauri::Window,
    text: String,
    image_data_url: Option<String>,
    source: String,
) -> Result<(), String> {
    if window.label() != "companion-overlay" {
        return Err("Only the companion overlay can submit companion context".to_string());
    }
    let text = text.trim().to_string();
    if text.is_empty() || text.len() > MAX_CAPTURE_TEXT_BYTES {
        return Err("Companion text is empty or exceeds its limit".to_string());
    }
    if source.is_empty() || source.len() > 128 || source.chars().any(char::is_control) {
        return Err("Companion context source is invalid".to_string());
    }
    if image_data_url.as_ref().is_some_and(|value| {
        !value.starts_with("data:image/") || value.len() as u64 > MAX_MEDIA_BYTES.saturating_mul(2)
    }) {
        return Err("Companion image context is invalid or too large".to_string());
    }
    app.emit_to(
        "main",
        "m7://compose",
        json!({"text": text, "imageDataUrl": image_data_url, "source": source}),
    )
    .map_err(|error| error.to_string())?;
    window.hide().map_err(|error| error.to_string())?;
    if let Some(main) = app.get_webview_window("main") {
        let _ = main.show();
        let _ = main.set_focus();
    }
    Ok(())
}

#[tauri::command]
pub fn m7_config_get(state: tauri::State<'_, M7CompanionState>) -> Result<CompanionConfig, String> {
    state.config()
}

#[tauri::command]
pub fn m7_config_save(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    config: CompanionConfig,
) -> Result<CompanionConfig, String> {
    ensure_main_window(&window)?;
    validate_config(&config)?;
    let previous_config = state.config()?;
    let previous = previous_config.overlay_shortcut.clone();
    if previous == config.overlay_shortcut {
        return state.save_config(config);
    }

    let next = config.overlay_shortcut.clone();
    app.global_shortcut()
        .register(next.as_str())
        .map_err(|error| format!("Could not register companion shortcut: {error}"))?;
    if let Err(error) = state.save_config(config) {
        let _ = app.global_shortcut().unregister(next.as_str());
        return Err(error);
    }
    if let Err(error) = app.global_shortcut().unregister(previous.as_str()) {
        let _ = app.global_shortcut().unregister(next.as_str());
        let _ = state.save_config(previous_config);
        return Err(format!(
            "Could not release the previous shortcut; restored the previous companion configuration: {error}"
        ));
    }
    Ok(state.config()?)
}

#[tauri::command]
pub fn m7_capture_grant(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    kind: CaptureKind,
    application_id: Option<String>,
    lifetime_ms: u64,
) -> Result<CaptureGrant, String> {
    ensure_companion_control_window(&window)?;
    if lifetime_ms == 0 || lifetime_ms > MAX_GRANT_LIFETIME_MS {
        return Err("Capture grant lifetime must be between 1 ms and 1 hour".to_string());
    }
    if application_id
        .as_ref()
        .is_some_and(|value| value.len() > 512 || value.chars().any(char::is_control))
    {
        return Err("Application scope is invalid".to_string());
    }
    let created_at_ms = now_ms();
    let grant = CaptureGrant {
        grant_id: format!("capture-{}", Uuid::new_v4()),
        kind,
        application_id,
        created_at_ms,
        expires_at_ms: created_at_ms.saturating_add(lifetime_ms),
        active: true,
    };
    lock(&state.grants, "capture grants")?.insert(grant.grant_id.clone(), grant.clone());
    Ok(grant)
}

#[tauri::command]
pub fn m7_capture_revoke(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
) -> Result<bool, String> {
    ensure_companion_control_window(&window)?;
    validate_id("grantId", &grant_id)?;
    Ok(lock(&state.grants, "capture grants")?
        .get_mut(&grant_id)
        .map(|grant| {
            let was_active = grant.active;
            grant.active = false;
            was_active
        })
        .unwrap_or(false))
}

#[tauri::command]
pub fn m7_capture_grants(
    state: tauri::State<'_, M7CompanionState>,
) -> Result<Vec<CaptureGrant>, String> {
    state.security_grants()
}

#[tauri::command]
pub fn m7_capture_text(
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
    text: String,
) -> Result<CompanionArtifact, String> {
    state.require_grant(&grant_id, &BTreeSet::from([CaptureKind::Text]))?;
    if text.is_empty() || text.len() > MAX_CAPTURE_TEXT_BYTES {
        return Err("Captured text is empty or exceeds its limit".to_string());
    }
    state.publish(
        text.as_bytes(),
        "text/plain; charset=utf-8",
        "explicit text context",
    )
}

#[tauri::command]
pub fn m7_capture_file(
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
    path: String,
) -> Result<CompanionArtifact, String> {
    state.require_grant(&grant_id, &BTreeSet::from([CaptureKind::File]))?;
    let (path, bytes) = bounded_file(&path)?;
    state.publish(
        &bytes,
        "application/octet-stream",
        format!("selected file {}", path.display()),
    )
}

#[tauri::command]
pub async fn m7_capture_screen(
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
) -> Result<CompanionArtifact, String> {
    state.require_grant(
        &grant_id,
        &BTreeSet::from([CaptureKind::Screen, CaptureKind::Window]),
    )?;
    let output = state
        .root
        .join("tmp")
        .join(format!("capture-{}.png", Uuid::new_v4().simple()));
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/sbin/screencapture");
        command.args(["-i", "-x"]).arg(&output);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = tokio::process::Command::new("gnome-screenshot");
        command.args(["-a", "-f"]).arg(&output);
        command
    };
    #[cfg(target_os = "windows")]
    return Err(
        "Interactive screen selection is not yet available on this Windows build".to_string(),
    );
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .map_err(|error| format!("Could not start the OS screen picker: {error}"))?;
        if !status.success() || !output.exists() {
            let _ = fs::remove_file(&output);
            return Err("Screen capture was cancelled or failed".to_string());
        }
        let bytes = fs::read(&output).map_err(|error| error.to_string())?;
        let _ = fs::remove_file(&output);
        state.publish(&bytes, "image/png", "interactive OS screen selection")
    }
}

fn extract_transcript(value: &Value) -> String {
    if let Some(text) = value.get("text").and_then(Value::as_str) {
        return text.trim().to_string();
    }
    for key in ["transcription", "segments"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            let text = items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            if !text.trim().is_empty() {
                return text.trim().to_string();
            }
        }
    }
    String::new()
}

fn numeric_timestamp(value: Option<&Value>, seconds: bool) -> Option<u64> {
    let number = value?.as_f64()?;
    if !number.is_finite() || number < 0.0 {
        return None;
    }
    let milliseconds = if seconds { number * 1_000.0 } else { number };
    Some(milliseconds.round().min(u64::MAX as f64) as u64)
}

fn extract_speaker_segments(value: &Value) -> Vec<SpeakerSegment> {
    let items = value
        .get("segments")
        .or_else(|| value.get("utterances"))
        .or_else(|| value.get("transcription"))
        .and_then(Value::as_array);
    let Some(items) = items else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let text = item.get("text")?.as_str()?.trim().to_string();
            if text.is_empty() {
                return None;
            }
            let speaker = ["speaker", "speaker_label", "speaker_id"]
                .iter()
                .find_map(|key| item.get(*key).and_then(Value::as_str))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("Unknown speaker")
                .chars()
                .take(128)
                .collect();
            let offsets = item.get("offsets");
            let start_ms = numeric_timestamp(item.get("start_ms"), false)
                .or_else(|| numeric_timestamp(item.get("start"), true))
                .or_else(|| numeric_timestamp(offsets.and_then(|value| value.get("from")), false));
            let end_ms = numeric_timestamp(item.get("end_ms"), false)
                .or_else(|| numeric_timestamp(item.get("end"), true))
                .or_else(|| numeric_timestamp(offsets.and_then(|value| value.get("to")), false));
            let confidence = item
                .get("confidence")
                .or_else(|| item.get("speaker_confidence"))
                .and_then(Value::as_f64)
                .filter(|value| value.is_finite() && (0.0..=1.0).contains(value));
            Some(SpeakerSegment {
                speaker,
                start_ms,
                end_ms,
                text,
                confidence,
            })
        })
        .take(100_000)
        .collect()
}

async fn transcribe_path(
    state: &M7CompanionState,
    job_id: &str,
    path: &Path,
    cancellation: &CancellationToken,
    diarize: bool,
) -> Result<(String, String, Vec<SpeakerSegment>), String> {
    let config = state.config()?.voice;
    match config.backend {
        TranscriptionBackendKind::LocalWhisper => {
            let binary = validate_absolute_regular(
                config
                    .whisper_binary
                    .as_deref()
                    .ok_or("Configure a local whisper.cpp binary first")?,
                true,
            )?;
            let model = validate_absolute_regular(
                config
                    .whisper_model
                    .as_deref()
                    .ok_or("Configure a local whisper model first")?,
                false,
            )?;
            let prefix = state.root.join("tmp").join(format!("transcript-{job_id}"));
            let mut command = tokio::process::Command::new(binary);
            command
                .arg("-m")
                .arg(model)
                .arg("-f")
                .arg(path)
                .arg("-oj")
                .arg("-of")
                .arg(&prefix)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            if config.language != "auto" {
                command.arg("-l").arg(&config.language);
            }
            let mut child = command
                .spawn()
                .map_err(|error| format!("Start whisper.cpp: {error}"))?;
            let status = tokio::select! {
                _ = cancellation.cancelled() => {
                    let _ = child.kill().await;
                    return Err("Transcription cancelled".to_string());
                }
                status = tokio::time::timeout(Duration::from_secs(60 * 60), child.wait()) => {
                    status.map_err(|_| "Transcription exceeded one hour".to_string())?
                        .map_err(|error| format!("Wait for whisper.cpp: {error}"))?
                }
            };
            if !status.success() {
                return Err(format!("whisper.cpp exited with {status}"));
            }
            let json_path = prefix.with_extension("json");
            let bytes = fs::read(&json_path)
                .map_err(|error| format!("Read whisper.cpp transcript: {error}"))?;
            let _ = fs::remove_file(json_path);
            if bytes.len() > MAX_TRANSCRIPT_BYTES {
                return Err("Transcript exceeds its byte limit".to_string());
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Decode whisper.cpp transcript: {error}"))?;
            let text = extract_transcript(&value);
            if text.is_empty() {
                return Err("whisper.cpp returned an empty transcript".to_string());
            }
            let segments = extract_speaker_segments(&value);
            Ok((text, "local_whisper".to_string(), segments))
        }
        TranscriptionBackendKind::Provider => {
            let provider = config
                .provider_id
                .ok_or("Configure a BYOK transcription provider")?;
            let custom_entries = configured_custom_providers();
            let base_url = crate::providers::resolve_base_url(&provider, &custom_entries)?;
            let key = crate::providers::read_key_with_env(&provider)?;
            let bytes = fs::read(path).map_err(|error| error.to_string())?;
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("audio.bin")
                .to_string();
            let form = reqwest::multipart::Form::new()
                .text("model", config.provider_model)
                .text(
                    "response_format",
                    if diarize {
                        "diarized_json"
                    } else {
                        "verbose_json"
                    },
                )
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes).file_name(file_name),
                );
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(60 * 60))
                .build()
                .map_err(|error| error.to_string())?;
            let request = crate::providers::add_anthropic_headers(
                client
                    .post(format!(
                        "{}/audio/transcriptions",
                        base_url.trim_end_matches('/')
                    ))
                    .bearer_auth(&key)
                    .multipart(form),
                &provider,
                &key,
            );
            let response = tokio::select! {
                _ = cancellation.cancelled() => return Err("Transcription cancelled".to_string()),
                response = request.send() => response.map_err(|error| format!("Transcription provider: {error}"))?,
            };
            let status = response.status();
            let bytes = response.bytes().await.map_err(|error| error.to_string())?;
            if bytes.len() > MAX_TRANSCRIPT_BYTES {
                return Err("Provider transcript exceeds its byte limit".to_string());
            }
            if !status.is_success() {
                return Err(format!("Transcription provider returned {status}"));
            }
            let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            let text = extract_transcript(&value);
            if text.is_empty() {
                return Err("Provider returned an empty transcript".to_string());
            }
            let segments = extract_speaker_segments(&value);
            Ok((text, format!("provider:{provider}"), segments))
        }
    }
}

#[tauri::command]
pub async fn m7_transcribe_file(
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
    job_id: String,
    path: String,
) -> Result<TranscriptResult, String> {
    let grant = state.require_grant(
        &grant_id,
        &BTreeSet::from([
            CaptureKind::File,
            CaptureKind::Microphone,
            CaptureKind::Meeting,
        ]),
    )?;
    let (path, bytes) = bounded_file(&path)?;
    let cancellation = state.begin_job(&job_id)?;
    let result = transcribe_path(
        &state,
        &job_id,
        &path,
        &cancellation,
        grant.kind == CaptureKind::Meeting,
    )
    .await;
    state.finish_job(&job_id);
    let (text, backend, segments) = result?;
    let transcript = state.publish(
        text.as_bytes(),
        "text/plain; charset=utf-8",
        format!("transcript of {}", path.display()),
    )?;
    let raw_audio = if state.config()?.voice.save_raw_audio {
        Some(state.publish(
            &bytes,
            "application/octet-stream",
            "explicitly saved raw audio",
        )?)
    } else {
        None
    };
    Ok(TranscriptResult {
        job_id,
        text,
        segments,
        transcript,
        raw_audio,
        backend,
    })
}

#[tauri::command]
pub async fn m7_transcribe_audio(
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
    job_id: String,
    audio_base64: String,
    media_type: String,
) -> Result<TranscriptResult, String> {
    let grant = state.require_grant(
        &grant_id,
        &BTreeSet::from([CaptureKind::Microphone, CaptureKind::Meeting]),
    )?;
    if !media_type.starts_with("audio/")
        || audio_base64.len() as u64 > MAX_MEDIA_BYTES.saturating_mul(2)
    {
        return Err("Recorded audio media type or size is invalid".to_string());
    }
    let bytes = STANDARD
        .decode(audio_base64)
        .map_err(|_| "Recorded audio is not valid base64")?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_MEDIA_BYTES {
        return Err("Recorded audio is empty or exceeds its limit".to_string());
    }
    let extension = if media_type.contains("wav") {
        "wav"
    } else {
        "webm"
    };
    let path = state.root.join("tmp").join(format!(
        "recording-{}.{}",
        Uuid::new_v4().simple(),
        extension
    ));
    fs::write(&path, &bytes).map_err(|error| error.to_string())?;
    let cancellation = state.begin_job(&job_id)?;
    let result = transcribe_path(
        &state,
        &job_id,
        &path,
        &cancellation,
        grant.kind == CaptureKind::Meeting,
    )
    .await;
    state.finish_job(&job_id);
    let _ = fs::remove_file(&path);
    let (text, backend, segments) = result?;
    let transcript = state.publish(
        text.as_bytes(),
        "text/plain; charset=utf-8",
        "push-to-talk transcript",
    )?;
    let raw_audio = if state.config()?.voice.save_raw_audio {
        Some(state.publish(&bytes, &media_type, "explicitly saved push-to-talk audio")?)
    } else {
        None
    };
    Ok(TranscriptResult {
        job_id,
        text,
        segments,
        transcript,
        raw_audio,
        backend,
    })
}

#[tauri::command]
pub async fn m7_tts_speak(
    state: tauri::State<'_, M7CompanionState>,
    job_id: String,
    text: String,
) -> Result<(), String> {
    if text.is_empty() || text.len() > MAX_CAPTURE_TEXT_BYTES {
        return Err("Speech text is empty or exceeds its limit".to_string());
    }
    let cancellation = state.begin_job(&job_id)?;
    let voice = state.config()?.voice.tts_voice;
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/say");
        if let Some(voice) = voice.filter(|value| !value.is_empty()) {
            command.args(["-v", &voice]);
        }
        command.arg(&text);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = tokio::process::Command::new("spd-say");
        if let Some(voice) = voice.filter(|value| !value.is_empty()) {
            command.args(["-l", &voice]);
        }
        command.arg(&text);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let escaped = text.replace('\'', "''");
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("Add-Type -AssemblyName System.Speech; (New-Object System.Speech.Synthesis.SpeechSynthesizer).Speak('{escaped}')"),
        ]);
        command
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not start system speech: {error}"))?;
    let result = tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            Err("Speech playback cancelled".to_string())
        }
        status = child.wait() => {
            let status = status.map_err(|error| error.to_string())?;
            if status.success() { Ok(()) } else { Err(format!("System speech exited with {status}")) }
        }
    };
    state.finish_job(&job_id);
    result
}

#[tauri::command]
pub fn m7_job_cancel(
    state: tauri::State<'_, M7CompanionState>,
    job_id: String,
) -> Result<bool, String> {
    validate_id("jobId", &job_id)?;
    let jobs = lock(&state.jobs, "companion jobs")?;
    if let Some(token) = jobs.get(&job_id) {
        token.cancel();
        Ok(true)
    } else {
        Ok(false)
    }
}

fn replace_workflow_placeholders(value: &mut Value, request: &ImageGenerationRequest) {
    match value {
        Value::String(string) => {
            *value = match string.as_str() {
                "{{prompt}}" => Value::String(request.prompt.clone()),
                "{{negative_prompt}}" => Value::String(request.negative_prompt.clone()),
                "{{model}}" => Value::String(request.model.clone()),
                "{{width}}" => Value::from(request.width),
                "{{height}}" => Value::from(request.height),
                "{{steps}}" => Value::from(request.steps),
                "{{cfg_scale}}" => Value::from(request.cfg_scale),
                "{{seed}}" => Value::from(request.seed),
                _ => return,
            };
        }
        Value::Array(values) => {
            for value in values {
                replace_workflow_placeholders(value, request);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                replace_workflow_placeholders(value, request);
            }
        }
        _ => {}
    }
}

async fn generate_openai_image(
    state: &M7CompanionState,
    endpoint: &ImageEndpointConfig,
    request: &ImageGenerationRequest,
    cancellation: &CancellationToken,
) -> Result<(Vec<u8>, String), String> {
    let custom_entries = configured_custom_providers();
    let provider = endpoint
        .provider_id
        .as_deref()
        .ok_or("OpenAI-compatible image endpoint requires a provider id")?;
    let key = crate::providers::read_key_with_env(provider)?;
    let base_url = if endpoint.base_url.is_empty() {
        crate::providers::resolve_base_url(provider, &custom_entries)?
    } else {
        endpoint.base_url.clone()
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30 * 60))
        .build()
        .map_err(|error| error.to_string())?;
    let response = if let Some(source_id) = &request.source_artifact_id {
        if !endpoint.supports_editing {
            return Err("Selected image endpoint does not advertise editing".to_string());
        }
        let source = state
            .artifacts
            .read(source_id)
            .map_err(|error| error.to_string())?;
        let form = reqwest::multipart::Form::new()
            .text("model", request.model.clone())
            .text("prompt", request.prompt.clone())
            .text("size", format!("{}x{}", request.width, request.height))
            .text("response_format", "b64_json")
            .part(
                "image",
                reqwest::multipart::Part::bytes(source).file_name("source.png"),
            );
        let call = client
            .post(format!("{}/images/edits", base_url.trim_end_matches('/')))
            .bearer_auth(&key)
            .multipart(form)
            .send();
        tokio::select! {
            _ = cancellation.cancelled() => return Err("Image edit cancelled".to_string()),
            response = call => response.map_err(|error| error.to_string())?,
        }
    } else {
        let call = client
            .post(format!(
                "{}/images/generations",
                base_url.trim_end_matches('/')
            ))
            .bearer_auth(&key)
            .json(&json!({
                "model": request.model,
                "prompt": request.prompt,
                "size": format!("{}x{}", request.width, request.height),
                "response_format": "b64_json",
                "n": 1,
                "seed": request.seed,
            }))
            .send();
        tokio::select! {
            _ = cancellation.cancelled() => return Err("Image generation cancelled".to_string()),
            response = call => response.map_err(|error| error.to_string())?,
        }
    };
    let status = response.status();
    let bytes = response.bytes().await.map_err(|error| error.to_string())?;
    if bytes.len() > 32 * 1024 * 1024 {
        return Err("Image provider response exceeds 32 MiB".to_string());
    }
    if !status.is_success() {
        return Err(format!(
            "Image provider returned {status}: {}",
            String::from_utf8_lossy(&bytes)
        ));
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    let encoded = value
        .pointer("/data/0/b64_json")
        .and_then(Value::as_str)
        .ok_or("Image provider must return data[0].b64_json; remote output URLs are rejected")?;
    let image = STANDARD
        .decode(encoded)
        .map_err(|_| "Image provider returned invalid base64")?;
    Ok((image, "image/png".to_string()))
}

fn comfy_image_descriptor(value: &Value) -> Option<(String, String, String)> {
    value
        .get("outputs")?
        .as_object()?
        .values()
        .find_map(|output| {
            output.get("images")?.as_array()?.first().and_then(|image| {
                Some((
                    image.get("filename")?.as_str()?.to_string(),
                    image
                        .get("subfolder")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    image
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("output")
                        .to_string(),
                ))
            })
        })
}

async fn generate_comfy_image(
    endpoint: &ImageEndpointConfig,
    request: &ImageGenerationRequest,
    cancellation: &CancellationToken,
) -> Result<(Vec<u8>, String), String> {
    if request.source_artifact_id.is_some() {
        return Err("ComfyUI editing requires a workflow-specific upload node and is not inferred automatically".to_string());
    }
    let mut workflow = endpoint
        .workflow_template
        .clone()
        .ok_or("ComfyUI workflow template is missing")?;
    replace_workflow_placeholders(&mut workflow, request);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // A silence budget rather than a deadline for the whole request. The
        // `{base}/view` download below reads the finished image with one
        // `bytes()` call and the only size bound on it is the caller's 256 MiB
        // `MAX_MEDIA_BYTES`, checked after the fact — so 30 seconds for the whole
        // request meant 8.9 MB/s sustained at that bound. Localhost never noticed;
        // `endpoint.base_url` is user-configured and a ComfyUI on the LAN or
        // further away truncated a large render mid-body. The queue-poll requests
        // sharing this client are small JSON and are bounded by silence just as
        // well.
        .read_timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| error.to_string())?;
    let base = endpoint.base_url.trim_end_matches('/');
    let response = client
        .post(format!("{base}/prompt"))
        .json(&json!({"prompt": workflow, "client_id": request.job_id}))
        .send()
        .await
        .map_err(|error| format!("Submit ComfyUI workflow: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("ComfyUI submit returned {}", response.status()));
    }
    let prompt_id = response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())?
        .get("prompt_id")
        .and_then(Value::as_str)
        .ok_or("ComfyUI response omitted prompt_id")?
        .to_string();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30 * 60);
    let descriptor = loop {
        if cancellation.is_cancelled() {
            let _ = client.post(format!("{base}/interrupt")).send().await;
            return Err("ComfyUI generation cancelled".to_string());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("ComfyUI generation exceeded 30 minutes".to_string());
        }
        let response = client
            .get(format!("{base}/history/{prompt_id}"))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            let value: Value = response.json().await.map_err(|error| error.to_string())?;
            if let Some(descriptor) = value.get(&prompt_id).and_then(comfy_image_descriptor) {
                break descriptor;
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
    };
    let response = client
        .get(format!("{base}/view"))
        .query(&[
            ("filename", descriptor.0.as_str()),
            ("subfolder", descriptor.1.as_str()),
            ("type", descriptor.2.as_str()),
        ])
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "ComfyUI image fetch returned {}",
            response.status()
        ));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| error.to_string())?
        .to_vec();
    Ok((bytes, "image/png".to_string()))
}

#[tauri::command]
pub async fn m7_image_generate(
    app: tauri::AppHandle,
    state: tauri::State<'_, M7CompanionState>,
    request: ImageGenerationRequest,
) -> Result<ImageGalleryEntry, String> {
    validate_id("jobId", &request.job_id)?;
    validate_id("endpointId", &request.endpoint_id)?;
    if request.prompt.is_empty()
        || request.prompt.len() > MAX_PROMPT_BYTES
        || request.negative_prompt.len() > MAX_PROMPT_BYTES
        || request.model.is_empty()
        || request.model.len() > 512
        || request.width == 0
        || request.height == 0
        || request.width > MAX_IMAGE_DIMENSION
        || request.height > MAX_IMAGE_DIMENSION
        || request.steps == 0
        || request.steps > MAX_IMAGE_STEPS
        || !request.cfg_scale.is_finite()
        || !(0.0..=100.0).contains(&request.cfg_scale)
    {
        return Err("Image generation settings exceed their bounds".to_string());
    }
    let endpoint = state
        .config()?
        .image_endpoints
        .into_iter()
        .find(|endpoint| endpoint.enabled && endpoint.endpoint_id == request.endpoint_id)
        .ok_or("Image endpoint is missing or disabled")?;
    let cancellation = state.begin_job(&request.job_id)?;
    let _ = app.emit(
        "m7://image-progress",
        ImageProgressEvent {
            job_id: request.job_id.clone(),
            phase: "submitted".to_string(),
            progress: 0.05,
        },
    );
    let generated = match endpoint.kind {
        ImageEndpointKind::ComfyUi => {
            generate_comfy_image(&endpoint, &request, &cancellation).await
        }
        ImageEndpointKind::OpenAiCompatible => {
            generate_openai_image(&state, &endpoint, &request, &cancellation).await
        }
    };
    state.finish_job(&request.job_id);
    let (bytes, media_type) = generated?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_MEDIA_BYTES {
        return Err("Generated image is empty or exceeds its artifact limit".to_string());
    }
    let artifact = state.publish(&bytes, &media_type, format!("image job {}", request.job_id))?;
    let entry = ImageGalleryEntry {
        entry_id: format!("image-{}", Uuid::new_v4()),
        artifact_id: artifact.blob.id,
        size_bytes: artifact.blob.size,
        media_type,
        endpoint_id: endpoint.endpoint_id,
        endpoint_kind: endpoint.kind,
        model: request.model,
        prompt: request.prompt,
        negative_prompt: request.negative_prompt,
        width: request.width,
        height: request.height,
        steps: request.steps,
        cfg_scale: request.cfg_scale,
        seed: request.seed,
        source_artifact_id: request.source_artifact_id,
        created_at_ms: now_ms(),
    };
    let next = {
        let mut gallery = lock(&state.gallery, "image gallery")?;
        gallery.push(entry.clone());
        gallery.sort_by_key(|item| item.created_at_ms);
        gallery.clone()
    };
    state.persist_gallery(&next)?;
    let _ = app.emit(
        "m7://image-progress",
        ImageProgressEvent {
            job_id: request.job_id,
            phase: "completed".to_string(),
            progress: 1.0,
        },
    );
    Ok(entry)
}

#[tauri::command]
pub fn m7_image_gallery(
    state: tauri::State<'_, M7CompanionState>,
) -> Result<Vec<ImageGalleryEntry>, String> {
    Ok(lock(&state.gallery, "image gallery")?.clone())
}

#[tauri::command]
pub fn m7_image_data_url(
    state: tauri::State<'_, M7CompanionState>,
    artifact_id: String,
    media_type: String,
) -> Result<String, String> {
    state.image_data_url(&artifact_id, &media_type)
}

#[tauri::command]
pub fn m7_image_insert_chat(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    artifact_id: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    validate_id("artifactId", &artifact_id)?;
    let entry = lock(&state.gallery, "image gallery")?
        .iter()
        .find(|entry| entry.artifact_id == artifact_id)
        .cloned()
        .ok_or("The selected image is not present in the companion gallery")?;
    let image_data_url = state.image_data_url(&entry.artifact_id, &entry.media_type)?;
    app.emit_to(
        "main",
        "m7://compose",
        json!({
            "text": format!("Use this generated image as context. Original prompt: {}", entry.prompt),
            "imageDataUrl": image_data_url,
            "source": "generated-image",
        }),
    )
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn m7_emergency_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, M7CompanionState>,
) -> Result<Value, String> {
    let (revoked_grants, cancelled_jobs) = state.emergency_stop()?;
    let _ = app.emit(
        "m7://emergency-stop",
        json!({"revokedGrants": revoked_grants, "cancelledJobs": cancelled_jobs}),
    );
    Ok(json!({"revokedGrants": revoked_grants, "cancelledJobs": cancelled_jobs}))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("little-monkey-m7-{}", Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn grants_are_kind_scoped_and_emergency_stop_is_idempotent() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        let now = now_ms();
        lock(&state.grants, "test").unwrap().insert(
            "capture-test".to_string(),
            CaptureGrant {
                grant_id: "capture-test".to_string(),
                kind: CaptureKind::Text,
                application_id: None,
                created_at_ms: now,
                expires_at_ms: now + 10_000,
                active: true,
            },
        );
        assert!(state
            .require_grant("capture-test", &BTreeSet::from([CaptureKind::Text]))
            .is_ok());
        assert!(state
            .require_grant("capture-test", &BTreeSet::from([CaptureKind::Screen]))
            .is_err());
        assert_eq!(state.emergency_stop().unwrap(), (1, 0));
        assert_eq!(state.emergency_stop().unwrap(), (0, 0));
    }

    #[test]
    fn workflow_placeholders_preserve_typed_generation_values() {
        let request = ImageGenerationRequest {
            job_id: "image-test".to_string(),
            endpoint_id: "local".to_string(),
            prompt: "a monkey".to_string(),
            negative_prompt: "blur".to_string(),
            model: "model.safetensors".to_string(),
            width: 512,
            height: 768,
            steps: 20,
            cfg_scale: 7.5,
            seed: 42,
            source_artifact_id: None,
        };
        let mut workflow = json!({"prompt":"{{prompt}}","width":"{{width}}","seed":"{{seed}}"});
        replace_workflow_placeholders(&mut workflow, &request);
        assert_eq!(workflow, json!({"prompt":"a monkey","width":512,"seed":42}));
    }

    #[test]
    fn diarized_segments_preserve_speakers_timing_and_confidence() {
        let value = json!({
            "text": "Hello Ship it",
            "segments": [
                {"speaker":"Alice","start":1.25,"end":2.5,"text":"Hello","confidence":0.9},
                {"speaker_label":"Bob","start_ms":3000,"end_ms":4100,"text":"Ship it"}
            ]
        });
        let segments = extract_speaker_segments(&value);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].speaker, "Alice");
        assert_eq!(segments[0].start_ms, Some(1_250));
        assert_eq!(segments[0].confidence, Some(0.9));
        assert_eq!(segments[1].speaker, "Bob");
        assert_eq!(segments[1].start_ms, Some(3_000));
        assert_eq!(extract_transcript(&value), "Hello Ship it");
    }

    #[test]
    fn config_roundtrips_atomically_and_rejects_duplicate_endpoints() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        let mut config = state.config().unwrap();
        let endpoint = ImageEndpointConfig {
            endpoint_id: "comfy".to_string(),
            label: "ComfyUI".to_string(),
            kind: ImageEndpointKind::ComfyUi,
            base_url: "http://127.0.0.1:8188".to_string(),
            provider_id: None,
            workflow_template: Some(json!({"1":{"inputs":{"text":"{{prompt}}"}}})),
            supports_editing: false,
            enabled: true,
        };
        config.image_endpoints = vec![endpoint.clone()];
        state.save_config(config.clone()).unwrap();
        assert_eq!(state.config().unwrap(), config);
        config.image_endpoints.push(endpoint);
        assert!(state.save_config(config).is_err());

        let mut invalid_shortcut = state.config().unwrap();
        invalid_shortcut.overlay_shortcut = "definitely not a shortcut".to_string();
        assert!(state.save_config(invalid_shortcut).is_err());
    }
}

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
use crate::executable_extensions::{CapabilityKind, ExtensionManager};

const CONFIG_SCHEMA_VERSION: u32 = 1;
const CONFIG_FILE: &str = "companion-config-v1.json";
const GALLERY_FILE: &str = "image-gallery-v1.json";
const TALK_METRICS_FILE: &str = "talk-metrics-v1.json";
const MAX_TALK_METRICS: usize = 100;
const MAX_TALK_LATENCY_MS: u64 = 10 * 60 * 1_000;
const MAX_TALK_TTS_TEXT_BYTES: usize = 32 * 1024;
const MAX_TALK_AUDIO_BYTES: u64 = 32 * 1024 * 1024;
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
    ExecutableExtension,
}

/// Which synthesizer speaks. `System` is this machine's own voice — `say`,
/// `espeak-ng`, SAPI — and is the default so an installation that predates
/// extension providers keeps the voice it had.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum SpeechBackendKind {
    #[default]
    System,
    ExecutableExtension,
}

fn default_provider_model() -> String {
    "whisper-1".to_string()
}

fn default_language() -> String {
    "auto".to_string()
}

fn default_vad_min_speech_ms() -> u64 {
    180
}

fn default_vad_silence_ms() -> u64 {
    800
}

fn default_vad_max_utterance_ms() -> u64 {
    90_000
}

fn default_wake_phrase() -> String {
    "hey little monkey".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VoiceConfig {
    pub backend: TranscriptionBackendKind,
    #[serde(default)]
    pub whisper_binary: Option<String>,
    #[serde(default)]
    pub whisper_model: Option<String>,
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default = "default_provider_model")]
    pub provider_model: String,
    #[serde(default)]
    pub extension_id: Option<String>,
    #[serde(default)]
    pub extension_capability_id: Option<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub tts_voice: Option<String>,
    #[serde(default)]
    pub tts_extension_id: Option<String>,
    #[serde(default)]
    pub tts_extension_capability_id: Option<String>,
    /// Which backend serves a *live* call. Separate from `tts_backend`
    /// because the two are different jobs: one synthesizes a clip, the other
    /// holds a conversation open. An operator may well want the system voice
    /// on the desktop and a realtime provider on the phone line.
    #[serde(default)]
    pub realtime_backend: SpeechBackendKind,
    #[serde(default)]
    pub realtime_extension_id: Option<String>,
    #[serde(default)]
    pub realtime_extension_capability_id: Option<String>,
    #[serde(default)]
    pub save_raw_audio: bool,
    #[serde(default)]
    pub input_device_id: Option<String>,
    #[serde(default)]
    pub output_device_id: Option<String>,
    #[serde(default)]
    pub tts_backend: SpeechBackendKind,
    #[serde(default = "default_vad_min_speech_ms")]
    pub vad_min_speech_ms: u64,
    #[serde(default = "default_vad_silence_ms")]
    pub vad_silence_ms: u64,
    #[serde(default = "default_vad_max_utterance_ms")]
    pub vad_max_utterance_ms: u64,
    #[serde(default)]
    pub wake_phrase_enabled: bool,
    #[serde(default = "default_wake_phrase")]
    pub wake_phrase: String,
    #[serde(default)]
    pub always_listening: bool,
    /// Native composer dictation locale; None selects the system default.
    #[serde(default)]
    pub dictation_language: Option<String>,
    /// macOS only: never fall back to network-backed Apple recognition.
    #[serde(default)]
    pub dictation_require_on_device: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            backend: TranscriptionBackendKind::LocalWhisper,
            whisper_binary: None,
            whisper_model: None,
            provider_id: None,
            provider_model: "whisper-1".to_string(),
            extension_id: None,
            extension_capability_id: None,
            language: "auto".to_string(),
            tts_voice: None,
            tts_backend: SpeechBackendKind::System,
            tts_extension_id: None,
            tts_extension_capability_id: None,
            realtime_backend: SpeechBackendKind::System,
            realtime_extension_id: None,
            realtime_extension_capability_id: None,
            save_raw_audio: false,
            input_device_id: None,
            output_device_id: None,
            vad_min_speech_ms: default_vad_min_speech_ms(),
            vad_silence_ms: default_vad_silence_ms(),
            vad_max_utterance_ms: default_vad_max_utterance_ms(),
            wake_phrase_enabled: false,
            wake_phrase: default_wake_phrase(),
            always_listening: false,
            dictation_language: None,
            dictation_require_on_device: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TalkMetric {
    pub created_at_ms: u64,
    pub speech_detection_ms: Option<u64>,
    pub stt_ms: Option<u64>,
    pub model_first_token_ms: Option<u64>,
    pub tts_first_audio_ms: Option<u64>,
    pub end_to_end_ms: Option<u64>,
    pub interrupted: bool,
    pub fallback: bool,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TalkMetricsSnapshot {
    pub metrics: Vec<TalkMetric>,
    pub interrupt_count: usize,
    pub fallback_count: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TalkStatus {
    pub configured: bool,
    pub wake_phrase_enabled: bool,
    pub always_listening: bool,
    pub backend: TranscriptionBackendKind,
    pub active_jobs: usize,
    pub active_microphone_grants: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpeechAudioResult {
    pub job_id: String,
    pub media_type: String,
    pub audio_base64: String,
}

/// What Talk's transcription returns: the words, and no handle to anything kept,
/// because nothing was kept.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TalkTranscript {
    pub job_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VoicePrivacySnapshot {
    pub wake_phrase_enabled: bool,
    pub always_listening: bool,
    pub local_only: bool,
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
    app_data_dir: PathBuf,
    root: PathBuf,
    config: Mutex<CompanionConfig>,
    grants: Mutex<BTreeMap<String, CaptureGrant>>,
    jobs: Mutex<BTreeMap<String, CancellationToken>>,
    gallery: Mutex<Vec<ImageGalleryEntry>>,
    talk_metrics: Mutex<Vec<TalkMetric>>,
    artifacts: ArtifactStore,
}

struct TemporaryArtifactRoot {
    path: PathBuf,
}

impl TemporaryArtifactRoot {
    fn new(parent: &Path) -> Result<Self, String> {
        let path = parent.join(format!("stt-artifacts-{}", Uuid::new_v4().simple()));
        ensure_private_directory(&path)?;
        Ok(Self { path })
    }

    fn cleanup(&mut self) -> Result<(), String> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        let metadata = match fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.path.clear();
                return Ok(());
            }
            Err(error) => {
                return Err(format!(
                    "Could not inspect temporary STT artifact store {}: {error}",
                    self.path.display()
                ));
            }
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(format!(
                "Temporary STT artifact store {} is no longer a real directory",
                self.path.display()
            ));
        }
        match fs::remove_dir_all(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Could not remove temporary STT artifact store {}: {error}",
                    self.path.display()
                ));
            }
        }
        self.path.clear();
        Ok(())
    }
}

impl Drop for TemporaryArtifactRoot {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

struct ExtensionAudioArtifacts {
    store: ArtifactStore,
    temporary_root: Option<TemporaryArtifactRoot>,
}

impl ExtensionAudioArtifacts {
    fn cleanup(&mut self) -> Result<(), String> {
        self.temporary_root
            .as_mut()
            .map_or(Ok(()), TemporaryArtifactRoot::cleanup)
    }
}

fn extension_audio_artifacts(
    state: &M7CompanionState,
    persist_raw_audio: bool,
) -> Result<ExtensionAudioArtifacts, String> {
    if persist_raw_audio {
        return Ok(ExtensionAudioArtifacts {
            store: state.artifacts.clone(),
            temporary_root: None,
        });
    }
    let temporary_root = TemporaryArtifactRoot::new(&state.root.join("tmp"))?;
    let store = ArtifactStore::with_max_blob_size(&temporary_root.path, MAX_MEDIA_BYTES)
        .map_err(|error| error.to_string())?;
    Ok(ExtensionAudioArtifacts {
        store,
        temporary_root: Some(temporary_root),
    })
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
        let mut talk_metrics =
            load_json::<Vec<TalkMetric>>(&root.join(TALK_METRICS_FILE))?.unwrap_or_default();
        talk_metrics.retain(|metric| validate_talk_metric(metric).is_ok());
        if talk_metrics.len() > MAX_TALK_METRICS {
            talk_metrics.drain(..talk_metrics.len() - MAX_TALK_METRICS);
        }
        Ok(Self {
            app_data_dir: app_data_dir.to_path_buf(),
            root,
            config: Mutex::new(config),
            grants: Mutex::new(BTreeMap::new()),
            jobs: Mutex::new(BTreeMap::new()),
            gallery: Mutex::new(gallery),
            talk_metrics: Mutex::new(talk_metrics),
            artifacts: ArtifactStore::with_max_blob_size(
                app_data_dir.join("content-v1"),
                MAX_MEDIA_BYTES,
            )
            .map_err(|error| error.to_string())?,
        })
    }

    /// The companion configuration, for a test that needs to select a
    /// provider before driving a normal entry point. Panics rather than
    /// returning a result: a fixture whose own state directory cannot be read
    /// has nothing left to assert.
    #[cfg(test)]
    pub(crate) fn config_for_test(&self) -> CompanionConfig {
        self.config().expect("fixture companion state is readable")
    }

    #[cfg(test)]
    pub(crate) fn save_config_for_test(&self, config: CompanionConfig) {
        self.save_config(config)
            .expect("fixture companion configuration is valid");
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

    pub fn security_voice_privacy(&self) -> Result<VoicePrivacySnapshot, String> {
        let voice = self.config()?.voice;
        Ok(VoicePrivacySnapshot {
            wake_phrase_enabled: voice.wake_phrase_enabled,
            always_listening: voice.always_listening,
            local_only: voice.backend == TranscriptionBackendKind::LocalWhisper,
        })
    }

    fn talk_metrics(&self) -> Result<TalkMetricsSnapshot, String> {
        let metrics = lock(&self.talk_metrics, "talk metrics")?.clone();
        Ok(TalkMetricsSnapshot {
            interrupt_count: metrics.iter().filter(|metric| metric.interrupted).count(),
            fallback_count: metrics.iter().filter(|metric| metric.fallback).count(),
            metrics,
        })
    }

    fn record_talk_metric(&self, metric: TalkMetric) -> Result<TalkMetricsSnapshot, String> {
        validate_talk_metric(&metric)?;
        let snapshot = {
            let mut metrics = lock(&self.talk_metrics, "talk metrics")?;
            metrics.push(metric);
            if metrics.len() > MAX_TALK_METRICS {
                let remove = metrics.len() - MAX_TALK_METRICS;
                metrics.drain(..remove);
            }
            atomic_write_json(&self.root.join(TALK_METRICS_FILE), metrics.as_slice())?;
            metrics.clone()
        };
        Ok(TalkMetricsSnapshot {
            interrupt_count: snapshot.iter().filter(|metric| metric.interrupted).count(),
            fallback_count: snapshot.iter().filter(|metric| metric.fallback).count(),
            metrics: snapshot,
        })
    }

    fn clear_talk_metrics(&self) -> Result<(), String> {
        let mut metrics = lock(&self.talk_metrics, "talk metrics")?;
        atomic_write_json(&self.root.join(TALK_METRICS_FILE), &[] as &[TalkMetric])?;
        metrics.clear();
        Ok(())
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
        || config
            .voice
            .dictation_language
            .as_deref()
            .is_some_and(|language| {
                language.is_empty() || language.len() > 64 || language.chars().any(char::is_control)
            })
        || !(50..=2_000).contains(&config.voice.vad_min_speech_ms)
        || !(400..=2_000).contains(&config.voice.vad_silence_ms)
        || !(1_000..=90_000).contains(&config.voice.vad_max_utterance_ms)
        || config.voice.vad_min_speech_ms >= config.voice.vad_max_utterance_ms
    {
        return Err("Companion configuration is invalid".to_string());
    }
    for device_id in [
        config.voice.input_device_id.as_deref(),
        config.voice.output_device_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if device_id.is_empty() || device_id.len() > 512 || device_id.chars().any(char::is_control)
        {
            return Err("Voice device selection is invalid".to_string());
        }
    }
    if config.voice.wake_phrase.is_empty()
        || config.voice.wake_phrase.len() > 128
        || config.voice.wake_phrase.chars().any(char::is_control)
    {
        return Err("Wake phrase is invalid".to_string());
    }
    if config.voice.always_listening && !config.voice.wake_phrase_enabled {
        return Err("Always-listening requires the wake phrase to be enabled".to_string());
    }
    if (config.voice.wake_phrase_enabled || config.voice.always_listening)
        && config.voice.backend != TranscriptionBackendKind::LocalWhisper
    {
        return Err("Wake phrase listening is local-only and requires local Whisper".to_string());
    }
    config
        .overlay_shortcut
        .parse::<tauri_plugin_global_shortcut::Shortcut>()
        .map_err(|error| format!("Companion shortcut is invalid: {error}"))?;
    if let Some(extension_id) = &config.voice.extension_id {
        validate_id("extensionId", extension_id)?;
    }
    if let Some(capability_id) = &config.voice.extension_capability_id {
        validate_id("extensionCapabilityId", capability_id)?;
    }
    if let Some(extension_id) = &config.voice.tts_extension_id {
        validate_id("ttsExtensionId", extension_id)?;
    }
    if let Some(capability_id) = &config.voice.tts_extension_capability_id {
        validate_id("ttsExtensionCapabilityId", capability_id)?;
    }
    if config.voice.tts_backend == SpeechBackendKind::ExecutableExtension
        && (config.voice.tts_extension_id.is_none()
            || config.voice.tts_extension_capability_id.is_none())
    {
        return Err(
            "An executable speech provider needs both its owning extension and its capability"
                .to_string(),
        );
    }
    if let Some(extension_id) = &config.voice.realtime_extension_id {
        validate_id("realtimeExtensionId", extension_id)?;
    }
    if let Some(capability_id) = &config.voice.realtime_extension_capability_id {
        validate_id("realtimeExtensionCapabilityId", capability_id)?;
    }
    if config.voice.realtime_backend == SpeechBackendKind::ExecutableExtension
        && (config.voice.realtime_extension_id.is_none()
            || config.voice.realtime_extension_capability_id.is_none())
    {
        return Err(
            "An executable realtime voice provider needs both its owning extension and its \
             capability"
                .to_string(),
        );
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

fn validate_talk_metric(metric: &TalkMetric) -> Result<(), String> {
    if metric.created_at_ms == 0 {
        return Err("Talk metric timestamp is invalid".to_string());
    }
    for value in [
        metric.speech_detection_ms,
        metric.stt_ms,
        metric.model_first_token_ms,
        metric.tts_first_audio_ms,
        metric.end_to_end_ms,
    ]
    .into_iter()
    .flatten()
    {
        if value > MAX_TALK_LATENCY_MS {
            return Err("Talk latency metric exceeds its limit".to_string());
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
    atomic_write_file(path, &bytes)
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
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
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    drop(file);
    fs::rename(&temp, path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("Could not publish {}: {error}", path.display())
    })
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
    // Present only for a finalized hands-free utterance: the id of the
    // recognition job that produced this text.
    //
    // It is what makes the spoken turn durable *and* exactly-once — it becomes
    // the turn's ingress dedupe identity, so a submission retried after a
    // timeout lands on the run the first attempt made. Absent means the text
    // goes to the composer for the operator to read and send, which is the
    // push-to-talk behavior and stays unchanged.
    utterance_id: Option<String>,
) -> Result<(), String> {
    if window.label() != "companion-overlay" {
        return Err("Only the companion overlay can submit companion context".to_string());
    }
    if let Some(utterance_id) = &utterance_id {
        if utterance_id.is_empty()
            || utterance_id.len() > 128
            || !utterance_id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        {
            return Err("Companion utterance id is invalid".to_string());
        }
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
        json!({
            "text": text,
            "imageDataUrl": image_data_url,
            "source": source,
            "utteranceId": utterance_id,
        }),
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
pub fn m7_talk_status(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
) -> Result<TalkStatus, String> {
    ensure_main_window(&window)?;
    let voice = state.config()?.voice;
    let configured = match voice.backend {
        // Local Whisper is part of the application. The model is provisioned
        // automatically in the background and lazily retried on first use, so
        // no user-supplied executable/model paths are a configuration gate.
        TranscriptionBackendKind::LocalWhisper => true,
        TranscriptionBackendKind::Provider => voice.provider_id.is_some(),
        // Both halves, because either one alone resolves to nothing: the
        // capability names what to run and the extension id names whose copy
        // of it, and Talk would otherwise report itself ready and then fail on
        // the first utterance.
        TranscriptionBackendKind::ExecutableExtension => {
            voice.extension_id.is_some() && voice.extension_capability_id.is_some()
        }
    };
    let now = now_ms();
    let active_microphone_grants = lock(&state.grants, "capture grants")?
        .values()
        .filter(|grant| {
            grant.active
                && grant.expires_at_ms > now
                && matches!(grant.kind, CaptureKind::Microphone | CaptureKind::Meeting)
        })
        .count();
    Ok(TalkStatus {
        configured,
        wake_phrase_enabled: voice.wake_phrase_enabled,
        always_listening: voice.always_listening,
        backend: voice.backend,
        active_jobs: lock(&state.jobs, "companion jobs")?.len(),
        active_microphone_grants,
    })
}

#[tauri::command]
pub fn m7_talk_metrics(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
) -> Result<TalkMetricsSnapshot, String> {
    ensure_main_window(&window)?;
    state.talk_metrics()
}

#[tauri::command]
pub fn m7_talk_metric_record(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    metric: TalkMetric,
) -> Result<TalkMetricsSnapshot, String> {
    ensure_main_window(&window)?;
    state.record_talk_metric(metric)
}

#[tauri::command]
pub fn m7_talk_metrics_clear(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
) -> Result<TalkMetricsSnapshot, String> {
    ensure_main_window(&window)?;
    state.clear_talk_metrics()?;
    state.talk_metrics()
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

fn extension_transcription_input(audio: &ArtifactBlob, language: &str) -> String {
    json!({
        "artifact_id": audio.id.as_str(),
        "language": if language == "auto" { None } else { Some(language) },
    })
    .to_string()
}

fn normalize_extension_transcript(
    output_json: &str,
) -> Result<(String, Vec<SpeakerSegment>), String> {
    if output_json.is_empty() || output_json.len() > MAX_TRANSCRIPT_BYTES {
        return Err("Extension transcript is empty or exceeds its byte limit".to_string());
    }
    let value: Value = serde_json::from_str(output_json)
        .map_err(|error| format!("Decode extension transcript: {error}"))?;
    let text = extract_transcript(&value);
    if text.is_empty() || text.len() > MAX_TRANSCRIPT_BYTES {
        return Err("Extension returned an empty or oversized transcript".to_string());
    }
    Ok((text, extract_speaker_segments(&value)))
}

async fn transcribe_path(
    state: &M7CompanionState,
    _job_id: &str,
    path: &Path,
    cancellation: &CancellationToken,
    diarize: bool,
) -> Result<(String, String, Vec<SpeakerSegment>), String> {
    let config = state.config()?.voice;
    match config.backend {
        TranscriptionBackendKind::LocalWhisper => {
            let transcript = crate::local_whisper::transcribe(
                &state.app_data_dir,
                path,
                &config.language,
                cancellation.clone(),
            )
            .await?;
            if transcript.text.len() > MAX_TRANSCRIPT_BYTES {
                return Err("Transcript exceeds its byte limit".to_string());
            }
            let segments = transcript
                .segments
                .into_iter()
                .map(|segment| SpeakerSegment {
                    speaker: "Unknown speaker".to_string(),
                    start_ms: segment.start_ms,
                    end_ms: segment.end_ms,
                    text: segment.text,
                    confidence: None,
                })
                .collect();
            Ok((transcript.text, "local_whisper".to_string(), segments))
        }
        TranscriptionBackendKind::Provider => {
            let provider = config
                .provider_id
                .ok_or("Configure a BYOK transcription provider")?;
            let custom_entries = crate::providers::configured_custom_providers();
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
                response = crate::egress::send(request) => response.map_err(|error| format!("Transcription provider: {error}"))?,
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
        TranscriptionBackendKind::ExecutableExtension => {
            let extension_id = config
                .extension_id
                .as_deref()
                .ok_or("Choose the owning executable STT extension first")?;
            let capability_id = config
                .extension_capability_id
                .as_deref()
                .ok_or("Choose a healthy executable STT capability first")?;
            let mut artifacts = extension_audio_artifacts(state, config.save_raw_audio)?;
            let outcome = async {
                let audio = artifacts
                    .store
                    .import_file(path)
                    .map_err(|error| format!("Publish extension audio input: {error}"))?;
                let input_json = extension_transcription_input(&audio, &config.language);
                let invocation_id = format!("m7-stt-{}", Uuid::new_v4().simple());
                let manager = ExtensionManager::new(&state.app_data_dir)?
                    .with_artifact_root(artifacts.store.root())?;
                let invocation = manager.invoke_owned_active_capability(
                    CapabilityKind::Stt,
                    extension_id,
                    capability_id,
                    input_json,
                    Some(invocation_id.clone()),
                    vec![audio.id],
                );
                tokio::pin!(invocation);
                let result = tokio::select! {
                    biased;
                    result = &mut invocation => result,
                    _ = cancellation.cancelled() => {
                        let _ = crate::executable_extensions::cancel(&invocation_id);
                        let _ = invocation.await;
                        Err("Transcription cancelled".to_string())
                    }
                }?;
                let (text, segments) = normalize_extension_transcript(&result.output_json)?;
                Ok((
                    text,
                    format!("extension:{extension_id}:{capability_id}"),
                    segments,
                ))
            }
            .await;
            let cleanup = artifacts.cleanup();
            match (outcome, cleanup) {
                (Ok(result), Ok(())) => Ok(result),
                (Err(error), Ok(())) => Err(error),
                (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
                (Err(error), Err(cleanup_error)) => Err(format!("{error}; {cleanup_error}")),
            }
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

/// Transcribe one spoken Talk utterance and keep nothing.
///
/// **Why this is not `m7_transcribe_audio`.** That command exists to turn a
/// recording into artifacts: it publishes the transcript, and when the operator
/// asked for raw audio to be kept, the audio too. Those are the right semantics
/// for a meeting somebody chose to record. They are the wrong semantics for a
/// conversation: a spoken turn is a sentence, not a recording, and Talk promises
/// that what is kept is the transcript in the session and bounded durations —
/// nothing else.
///
/// It also makes the wake phrase honest. A fragment that turns out not to
/// contain the phrase is dropped by the engine without ever becoming a turn; if
/// transcription had already published it, "the detection stops on this machine"
/// would be a claim contradicted by an artifact on disk.
///
/// The capture grant is still required — the microphone is still the
/// microphone.
#[tauri::command]
pub async fn m7_talk_transcribe(
    window: tauri::Window,
    state: tauri::State<'_, M7CompanionState>,
    grant_id: String,
    job_id: String,
    audio_base64: String,
    media_type: String,
) -> Result<TalkTranscript, String> {
    ensure_main_window(&window)?;
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
    let path =
        state
            .root
            .join("tmp")
            .join(format!("talk-{}.{}", Uuid::new_v4().simple(), extension));
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
    // On every exit: success, provider failure, cancellation and timeout all
    // come through here, because the utterance's audio outliving the utterance
    // is the one thing this path must never do.
    let _ = fs::remove_file(&path);
    let (text, _backend, _segments) = result?;
    Ok(TalkTranscript { job_id, text })
}

/// Whether this machine can turn call audio into text at all.
///
/// Answering the phone with no transcription backend configured is a feature
/// that looks enabled and does nothing: the caller talks, and every turn is
/// dropped. Callers of the telephony surface ask this so they can say so
/// before somebody discovers it on a live call.
pub fn call_speech_readiness(app_data_dir: &Path) -> Result<(), String> {
    let voice = M7CompanionState::production(app_data_dir)?.config()?.voice;
    match voice.backend {
        // The local engine ships with the app and provisions its verified model
        // automatically. Readiness is no longer a user-configuration question.
        TranscriptionBackendKind::LocalWhisper => {}
        TranscriptionBackendKind::Provider => {
            if voice.provider_id.as_deref().unwrap_or_default().is_empty() {
                return Err(
                    "Transcription is set to a hosted provider, but no provider is chosen, so nothing said on a call can be understood."
                        .to_string(),
                );
            }
        }
        TranscriptionBackendKind::ExecutableExtension => {
            let extension_id = voice
                .extension_id
                .as_deref()
                .ok_or(
                    "Transcription is set to an executable extension, but its owning extension is not selected, so nothing said on a call can be understood.",
                )?;
            let capability_id = voice
                .extension_capability_id
                .as_deref()
                .ok_or(
                    "Transcription is set to an executable extension, but no STT capability is chosen, so nothing said on a call can be understood.",
                )?;
            let active = ExtensionManager::new(app_data_dir)?
                .resolve_active_capability(CapabilityKind::Stt, capability_id)
                .map_err(|error| {
                    format!(
                        "The configured executable STT capability is not healthy and active: {error}"
                    )
                })?;
            if active.extension_id != extension_id {
                return Err(
                    "The configured executable STT capability is now owned by a different extension; reselect it in Settings"
                        .to_string(),
                );
            }
        }
    }
    Ok(())
}

/// Transcribe an audio file with the operator's own configured backend.
///
/// The entry point for callers outside the desktop command layer — today the
/// phone-call pipeline in the daemon. It deliberately goes through the same
/// [`M7CompanionState`] and the same `transcribe_path` as the desktop does: a
/// phone call must not get a speech stack of its own, or an operator who
/// configured local Whisper would find their calls quietly sent to a hosted
/// provider instead.
///
/// No capture grant is involved because no capture happens: the audio is
/// already in hand, having arrived from the carrier the operator configured.
/// Transcribe audio already in memory with the operator's own backend.
///
/// The Talk sockets' entry point. Same [`M7CompanionState`], same
/// `transcribe_path`, same rule as [`transcribe_call_audio`]: a spoken turn must
/// not get a speech stack of its own. The bytes are written to a private
/// temporary file because that is what both backends take — whisper.cpp reads a
/// path, and the provider upload needs a filename whose extension tells it what
/// the container is — and the file is removed on every exit path.
///
/// No capture grant is involved because no capture happens here: the audio
/// arrived from a device the operator paired and granted `voice_stream`.
pub async fn transcribe_audio_bytes(
    app_data_dir: &Path,
    audio: &[u8],
    media_type: &str,
) -> Result<String, String> {
    if audio.is_empty() || audio.len() as u64 > MAX_MEDIA_BYTES {
        return Err("Spoken audio is empty or exceeds its limit".to_string());
    }
    let extension = match media_type {
        value if value.contains("wav") => "wav",
        value if value.contains("ogg") => "ogg",
        value if value.contains("mp4") => "m4a",
        value if value.contains("mpeg") => "mp3",
        _ => "webm",
    };
    let state = M7CompanionState::production(app_data_dir)?;
    let directory = state.root.join("tmp");
    ensure_private_directory(&directory)?;
    let path = directory.join(format!("talk-{}.{extension}", Uuid::new_v4().simple()));
    fs::write(&path, audio).map_err(|error| format!("Could not stage spoken audio: {error}"))?;
    let job_id = format!("talk-stt-{}", Uuid::new_v4().simple());
    let cancellation = state.begin_job(&job_id)?;
    let result = transcribe_path(&state, &job_id, &path, &cancellation, false).await;
    state.finish_job(&job_id);
    let _ = fs::remove_file(&path);
    result.map(|(text, _backend, _segments)| text)
}

pub async fn transcribe_call_audio(app_data_dir: &Path, path: &Path) -> Result<String, String> {
    let state = M7CompanionState::production(app_data_dir)?;
    let job_id = format!("call-stt-{}", Uuid::new_v4().simple());
    let cancellation = state.begin_job(&job_id)?;
    let result = transcribe_path(&state, &job_id, path, &cancellation, false).await;
    state.finish_job(&job_id);
    result.map(|(text, _backend, _segments)| text)
}

/// The largest synthesized clip an extension may hand back. Generous enough
/// for a long spoken turn at 16-bit/16 kHz and far below the runtime's own
/// output ceiling, so an oversized answer is refused here with a sentence an
/// operator can act on rather than as a generic runtime failure.
const MAX_SYNTHESIZED_AUDIO_BYTES: usize = 4 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
struct ExtensionSpeechOutput {
    artifact_id: String,
    #[serde(default)]
    media_type: Option<String>,
}

/// Synthesize `text` through the extension TTS provider the operator selected.
///
/// The audio comes back as an artifact id, never a path: a guest that could
/// name a file would be choosing what the host reads. The id is then checked
/// against the set of artifacts *this invocation actually wrote* — the store
/// is content-addressed and shared, so a guessed or previously-seen id would
/// otherwise resolve to somebody else's content.
async fn synthesize_via_extension(
    app_data_dir: &Path,
    text: &str,
    voice: Option<&str>,
    cancellation: Option<&CancellationToken>,
) -> Result<Vec<u8>, String> {
    let config = M7CompanionState::production(app_data_dir)?.config()?.voice;
    let extension_id = config
        .tts_extension_id
        .as_deref()
        .ok_or("Choose the owning executable speech extension first")?;
    let capability_id = config
        .tts_extension_capability_id
        .as_deref()
        .ok_or("Choose a healthy executable speech capability first")?;
    let input_json = json!({
        "text": text,
        "voice": voice,
        // The one format every consumer in this app can read. A provider that
        // synthesizes something else has to convert before it answers.
        "format": "wav",
    })
    .to_string();
    let invocation_id = format!("m7-tts-{}", Uuid::new_v4().simple());
    let store = ArtifactStore::with_max_blob_size(
        app_data_dir.join("content-v1"),
        MAX_SYNTHESIZED_AUDIO_BYTES as u64,
    )
    .map_err(|error| format!("Cannot open the artifact store: {error}"))?;
    let manager = ExtensionManager::new(app_data_dir)?.with_artifact_root(store.root())?;
    let invocation = manager.invoke_owned_active_capability(
        CapabilityKind::Tts,
        extension_id,
        capability_id,
        input_json,
        Some(invocation_id.clone()),
        Vec::new(),
    );
    tokio::pin!(invocation);
    let result = match cancellation {
        Some(cancellation) => tokio::select! {
            biased;
            result = &mut invocation => result,
            _ = cancellation.cancelled() => {
                let _ = crate::executable_extensions::cancel(&invocation_id);
                let _ = invocation.await;
                Err("Speech synthesis cancelled".to_string())
            }
        },
        None => invocation.await,
    }?;
    let output: ExtensionSpeechOutput = serde_json::from_str(&result.output_json)
        .map_err(|error| format!("The speech extension returned invalid output: {error}"))?;
    if !result.written_artifact_ids.contains(&output.artifact_id) {
        return Err(
            "The speech extension named an artifact it did not write; audio refused".to_string(),
        );
    }
    if let Some(media_type) = output
        .media_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if !media_type.eq_ignore_ascii_case("audio/wav")
            && !media_type.eq_ignore_ascii_case("audio/x-wav")
            && !media_type.eq_ignore_ascii_case("audio/wave")
        {
            return Err(format!(
                "The speech extension returned '{media_type}' rather than WAV audio"
            ));
        }
    }
    let bytes = store
        .read(&output.artifact_id)
        .map_err(|error| format!("Cannot read the synthesized audio: {error}"))?;
    if bytes.len() > MAX_SYNTHESIZED_AUDIO_BYTES {
        return Err("The speech extension returned oversized audio".to_string());
    }
    // Cheapest honest check that this is the container it claims to be: the
    // consumers downstream parse a RIFF/WAVE header and a caller deserves the
    // failure named here rather than "could not decode" three layers on.
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("The speech extension did not return WAV audio".to_string());
    }
    Ok(bytes)
}

/// Speak `text` on this machine's speakers with an extension's voice.
///
/// The system backend hands text to a synthesizer that also plays it. An
/// extension only produces audio, so the two halves are separate here: the
/// clip is synthesized into a private temporary file and handed to the
/// platform's own player, which is the same program in each case that a user
/// double-clicking a WAV would reach.
async fn speak_via_extension(
    app_data_dir: &Path,
    temp_root: &Path,
    text: &str,
    voice: Option<&str>,
    cancellation: &CancellationToken,
) -> Result<(), String> {
    let bytes = synthesize_via_extension(app_data_dir, text, voice, Some(cancellation)).await?;
    ensure_private_directory(temp_root)?;
    let path = temp_root.join(format!("speech-{}.wav", Uuid::new_v4().simple()));
    atomic_write_file(&path, &bytes)?;
    let played = play_wav_file(&path, cancellation).await;
    let _ = fs::remove_file(&path);
    played
}

/// Hand one WAV file to the platform's audio player and wait for it, killing
/// it the moment the job is cancelled.
async fn play_wav_file(path: &Path, cancellation: &CancellationToken) -> Result<(), String> {
    let path_arg = path.to_string_lossy().to_string();
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/afplay");
        command.arg(&path_arg);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        // `aplay` ships with alsa-utils and is what every desktop image with
        // working audio already has; `paplay` is the PulseAudio equivalent and
        // is tried only if the first is absent.
        let program = if which_exists("aplay") {
            "aplay"
        } else {
            "paplay"
        };
        let mut command = tokio::process::Command::new(program);
        command.arg(&path_arg);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let escaped = path_arg.replace('\'', "''");
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "$p = New-Object System.Media.SoundPlayer '{escaped}'; $p.PlaySync(); $p.Dispose()"
            ),
        ]);
        command
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("Could not start audio playback: {error}"))?;
    tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = child.kill().await;
            Err("Speech playback cancelled".to_string())
        }
        status = child.wait() => {
            let status = status.map_err(|error| error.to_string())?;
            if status.success() {
                Ok(())
            } else {
                Err(format!("Audio playback exited with {status}"))
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn which_exists(program: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
    })
}

/// The extension-backed selections the companion currently holds, for the
/// Security Doctor's orphaned-provider check.
///
/// Read-only and total: a machine whose companion state has never been written
/// simply has no selections, which is not a finding.
#[derive(Debug, Default, Clone)]
pub struct PersistedVoiceSelections {
    pub transcription: Option<(String, String)>,
    pub speech: Option<(String, String)>,
    pub realtime: Option<(String, String)>,
}

pub fn persisted_voice_selections(app_data_dir: &Path) -> PersistedVoiceSelections {
    let Ok(voice) = call_voice_config(app_data_dir) else {
        return PersistedVoiceSelections::default();
    };
    let pair = |enabled: bool, extension: &Option<String>, capability: &Option<String>| {
        if !enabled {
            return None;
        }
        Some((extension.clone()?, capability.clone()?))
    };
    PersistedVoiceSelections {
        transcription: pair(
            voice.backend == TranscriptionBackendKind::ExecutableExtension,
            &voice.extension_id,
            &voice.extension_capability_id,
        ),
        speech: pair(
            voice.tts_backend == SpeechBackendKind::ExecutableExtension,
            &voice.tts_extension_id,
            &voice.tts_extension_capability_id,
        ),
        realtime: pair(
            voice.realtime_backend == SpeechBackendKind::ExecutableExtension,
            &voice.realtime_extension_id,
            &voice.realtime_extension_capability_id,
        ),
    }
}

/// The voice configuration a call needs, read through the same companion
/// state the desktop writes.
///
/// Exposed because the daemon serves phone calls in its own process and must
/// use the operator's actual selection rather than a default it invented.
pub fn call_voice_config(app_data_dir: &Path) -> Result<VoiceConfig, String> {
    Ok(M7CompanionState::production(app_data_dir)?.config()?.voice)
}

/// Synthesize `text` to a WAV file with the operator's system voice.
///
/// The speaking half of the same rule: [`m7_tts_speak`] plays to this machine's
/// speakers, which is the wrong destination for a call, so this writes the same
/// system synthesizer's output to a file the caller can send to the carrier.
/// Same voice setting, same synthesizer, different destination.
pub async fn synthesize_speech_to_wav(
    app_data_dir: &Path,
    text: &str,
    destination: &Path,
) -> Result<(), String> {
    let voice = M7CompanionState::production(app_data_dir)?.config()?.voice;
    synthesize_speech_to_wav_with_voice(
        app_data_dir,
        &voice,
        voice.tts_voice.clone().filter(|value| !value.is_empty()),
        text,
        destination,
    )
    .await
}

/// The synthesis both entry points share, with the backend decision made once.
///
/// Talk and a phone call reach speech through different commands, and the
/// operator chose one synthesizer for both. Branching here rather than in each
/// caller is what stops a provider that is selected in Settings from serving
/// one of them and not the other.
async fn synthesize_speech_to_wav_with_voice(
    app_data_dir: &Path,
    config: &VoiceConfig,
    voice: Option<String>,
    text: &str,
    destination: &Path,
) -> Result<(), String> {
    if text.is_empty() || text.len() > MAX_CAPTURE_TEXT_BYTES {
        return Err("Speech text is empty or exceeds its limit".to_string());
    }
    if config.tts_backend == SpeechBackendKind::ExecutableExtension {
        let bytes = synthesize_via_extension(app_data_dir, text, voice.as_deref(), None).await?;
        return atomic_write_file(destination, &bytes);
    }
    let destination_arg = destination.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = tokio::process::Command::new("/usr/bin/say");
        if let Some(voice) = voice {
            command.args(["-v", &voice]);
        }
        // 16-bit little-endian at the rate every carrier stream uses, so the
        // call side has nothing to resample.
        command.args(["--data-format=LEI16@8000", "-o", &destination_arg]);
        command.arg(text);
        command
    };
    #[cfg(target_os = "linux")]
    let mut command = {
        // espeak-ng writes a WAV; spd-say (used for desktop playback) cannot,
        // which is why this is the one place the two diverge.
        let mut command = tokio::process::Command::new("espeak-ng");
        if let Some(voice) = voice {
            command.args(["-v", &voice]);
        }
        command.args(["-w", &destination_arg, "--", text]);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let escaped_text = text.replace('\'', "''");
        let escaped_path = destination_arg.replace('\'', "''");
        let select_voice = voice
            .map(|voice| format!("$s.SelectVoice('{}');", voice.replace('\'', "''")))
            .unwrap_or_default();
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; {select_voice} $s.SetOutputToWaveFile('{escaped_path}'); $s.Speak('{escaped_text}'); $s.Dispose()"
            ),
        ]);
        command
    };

    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .status()
        .await
        .map_err(|error| format!("Could not start system speech: {error}"))?;
    if !status.success() {
        return Err(format!("System speech exited with {status}"));
    }
    if !destination.is_file() {
        return Err("System speech produced no audio".to_string());
    }
    Ok(())
}

#[tauri::command]
pub async fn m7_tts_synthesize(
    state: tauri::State<'_, M7CompanionState>,
    job_id: String,
    text: String,
) -> Result<SpeechAudioResult, String> {
    if text.is_empty() || text.len() > MAX_TALK_TTS_TEXT_BYTES {
        return Err("Speech text is empty or exceeds its Talk limit".to_string());
    }
    let cancellation = state.begin_job(&job_id)?;
    let destination = state
        .root
        .join("tmp")
        .join(format!("talk-speech-{}.wav", Uuid::new_v4().simple()));
    let voice_config = state.config()?.voice;
    let synthesis = synthesize_speech_to_wav_with_voice(
        &state.app_data_dir,
        &voice_config,
        voice_config
            .tts_voice
            .clone()
            .filter(|value| !value.is_empty()),
        &text,
        &destination,
    );
    let result = tokio::select! {
        _ = cancellation.cancelled() => Err("Speech synthesis cancelled".to_string()),
        result = synthesis => result,
    };
    state.finish_job(&job_id);
    let output = match result {
        Ok(()) => (|| {
            let metadata = fs::metadata(&destination)
                .map_err(|error| format!("Could not inspect synthesized speech: {error}"))?;
            if metadata.len() > MAX_TALK_AUDIO_BYTES {
                Err("Synthesized speech exceeds its Talk limit".to_string())
            } else {
                fs::read(&destination)
                    .map_err(|error| format!("Could not read synthesized speech: {error}"))
                    .map(|bytes| SpeechAudioResult {
                        job_id: job_id.clone(),
                        media_type: "audio/wav".to_string(),
                        audio_base64: STANDARD.encode(bytes),
                    })
            }
        })(),
        Err(error) => Err(error),
    };
    let _ = fs::remove_file(destination);
    output
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
    let voice_config = state.config()?.voice;
    let voice = voice_config.tts_voice.clone();
    if voice_config.tts_backend == SpeechBackendKind::ExecutableExtension {
        let result = speak_via_extension(
            &state.app_data_dir,
            &state.root.join("tmp"),
            &text,
            voice.as_deref().filter(|value| !value.is_empty()),
            &cancellation,
        )
        .await;
        state.finish_job(&job_id);
        return result;
    }
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
    let custom_entries = crate::providers::configured_custom_providers();
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
        let call = crate::egress::send(
            client
                .post(format!("{}/images/edits", base_url.trim_end_matches('/')))
                .bearer_auth(&key)
                .multipart(form),
        );
        tokio::select! {
            _ = cancellation.cancelled() => return Err("Image edit cancelled".to_string()),
            response = call => response.map_err(|error| error.to_string())?,
        }
    } else {
        let call = crate::egress::send(
            client
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
                })),
        );
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
    let response = crate::egress::send(
        client
            .post(format!("{base}/prompt"))
            .json(&json!({"prompt": workflow, "client_id": request.job_id})),
    )
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
            let _ = crate::egress::send(client.post(format!("{base}/interrupt"))).await;
            return Err("ComfyUI generation cancelled".to_string());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("ComfyUI generation exceeded 30 minutes".to_string());
        }
        let response = crate::egress::send(client.get(format!("{base}/history/{prompt_id}")))
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
    let response = crate::egress::send(client.get(format!("{base}/view")).query(&[
        ("filename", descriptor.0.as_str()),
        ("subfolder", descriptor.1.as_str()),
        ("type", descriptor.2.as_str()),
    ]))
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
    fn extension_transcripts_are_normalized_and_bounded() {
        let (text, segments) = normalize_extension_transcript(
            r#"{"text":"  Hello there  ","segments":[{"speaker":"Agent","start":0.5,"text":"Hello there"}]}"#,
        )
        .unwrap();
        assert_eq!(text, "Hello there");
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start_ms, Some(500));

        let oversized = format!(r#"{{"text":"{}"}}"#, "x".repeat(MAX_TRANSCRIPT_BYTES));
        assert!(normalize_extension_transcript(&oversized).is_err());
        assert!(normalize_extension_transcript(r#"{"language":"en"}"#).is_err());
    }

    #[test]
    fn extension_audio_input_uses_the_host_published_artifact() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        let audio = b"trusted audio bytes";
        let blob = state.artifacts.put(audio).unwrap();
        let input: Value =
            serde_json::from_str(&extension_transcription_input(&blob, "auto")).unwrap();

        assert_eq!(input, json!({"artifact_id": blob.id, "language": null}));
        assert_eq!(state.artifacts.read(&blob.id).unwrap(), audio);
    }

    #[test]
    fn extension_audio_artifacts_are_temporary_unless_raw_audio_is_opted_in() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        let audio = b"private transient audio";
        let temporary_root;
        let temporary_id;
        {
            let mut artifacts = extension_audio_artifacts(&state, false).unwrap();
            temporary_root = artifacts.store.root().to_path_buf();
            let blob = artifacts.store.put(audio).unwrap();
            temporary_id = blob.id;
            assert!(temporary_root.starts_with(state.root.join("tmp")));
            assert!(temporary_root.exists());
            assert!(state.artifacts.read(&temporary_id).is_err());
            artifacts.cleanup().unwrap();
            assert!(!temporary_root.exists());
        }
        assert!(!temporary_root.exists());

        let durable = extension_audio_artifacts(&state, true).unwrap();
        let blob = durable.store.put(audio).unwrap();
        assert_eq!(durable.store.root(), state.artifacts.root());
        drop(durable);
        assert_eq!(state.artifacts.read(&blob.id).unwrap(), audio);
    }

    #[test]
    fn call_readiness_requires_the_selected_extension_to_be_active() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        let mut config = state.config().unwrap();
        config.voice.backend = TranscriptionBackendKind::ExecutableExtension;
        state.save_config(config.clone()).unwrap();
        assert!(call_speech_readiness(&root.0)
            .unwrap_err()
            .contains("owning extension is not selected"));

        config.voice.extension_id = Some("dev.example.stt".to_string());
        state.save_config(config.clone()).unwrap();
        assert!(call_speech_readiness(&root.0)
            .unwrap_err()
            .contains("no STT capability is chosen"));

        config.voice.extension_capability_id = Some("transcribe".to_string());
        state.save_config(config).unwrap();
        assert!(call_speech_readiness(&root.0)
            .unwrap_err()
            .contains("not healthy and active"));
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
        config.voice.backend = TranscriptionBackendKind::ExecutableExtension;
        config.voice.extension_id = Some("dev.example.stt".to_string());
        config.voice.extension_capability_id = Some("transcribe".to_string());
        state.save_config(config.clone()).unwrap();
        assert_eq!(state.config().unwrap(), config);
        config.image_endpoints.push(endpoint);
        assert!(state.save_config(config).is_err());

        let mut invalid_shortcut = state.config().unwrap();
        invalid_shortcut.overlay_shortcut = "definitely not a shortcut".to_string();
        assert!(state.save_config(invalid_shortcut).is_err());

        let mut invalid_capability = state.config().unwrap();
        invalid_capability.voice.extension_capability_id = Some("not a capability".to_string());
        assert!(state.save_config(invalid_capability).is_err());

        let mut invalid_extension = state.config().unwrap();
        invalid_extension.voice.extension_id = Some("not an extension".to_string());
        assert!(state.save_config(invalid_extension).is_err());
    }

    #[test]
    fn legacy_voice_config_gets_private_talk_defaults() {
        let config: CompanionConfig = serde_json::from_value(json!({
            "schemaVersion": 1,
            "overlayShortcut": "CommandOrControl+Shift+Space",
            "voice": {
                "backend": "local_whisper",
                "whisperBinary": null,
                "whisperModel": null,
                "providerId": null,
                "providerModel": "whisper-1",
                "language": "auto",
                "ttsVoice": null,
                "saveRawAudio": false
            },
            "imageEndpoints": []
        }))
        .unwrap();
        assert_eq!(config.voice.vad_min_speech_ms, 180);
        assert_eq!(config.voice.vad_silence_ms, 800);
        assert_eq!(config.voice.vad_max_utterance_ms, 90_000);
        assert!(!config.voice.wake_phrase_enabled);
        assert!(!config.voice.always_listening);
        assert_eq!(config.voice.tts_backend, SpeechBackendKind::System);
        validate_config(&config).unwrap();
    }

    #[test]
    fn wake_phrase_is_opt_in_and_local_only() {
        let mut config = CompanionConfig::default();
        config.voice.backend = TranscriptionBackendKind::Provider;
        config.voice.provider_id = Some("operator-provider".to_string());
        config.voice.wake_phrase_enabled = true;
        assert!(validate_config(&config).is_err());

        config.voice.backend = TranscriptionBackendKind::LocalWhisper;
        config.voice.always_listening = true;
        assert!(validate_config(&config).is_ok());

        config.voice.wake_phrase_enabled = false;
        assert!(validate_config(&config).is_err());
    }

    /// **A spoken turn leaves no artifact, whatever `saveRawAudio` says.**
    ///
    /// That switch means "keep the recordings I asked you to make" — a meeting
    /// capture, a push-to-talk clip — and `m7_transcribe_audio` honours it by
    /// publishing the transcript and, when it is on, the audio. A conversation
    /// is not a recording somebody asked for, so Talk transcribes through its
    /// own command, which publishes nothing at all. It is also what makes the
    /// wake phrase's promise true: a fragment that turns out not to contain the
    /// phrase is dropped by the engine, and if transcription had already
    /// published it, "the detection stops on this machine" would be false.
    ///
    /// Scanned rather than executed because the alternative needs a whisper
    /// build and a window. The defect class is a call site that looks fine on
    /// its own, which is the same reason `egress.rs` and `web.rs` scan.
    #[test]
    fn talk_transcription_publishes_nothing_whatever_the_artifact_setting_says() {
        const SOURCE: &str = include_str!("m7_companion.rs");
        let body = SOURCE
            .split_once("pub async fn m7_talk_transcribe(")
            .expect("Talk has its own transcription command")
            .1
            .split_once("\n}\n")
            .expect("its body ends")
            .0;
        assert!(
            !body.contains("publish("),
            "Talk's transcription must not publish an artifact: {body}"
        );
        assert!(
            !body.contains("save_raw_audio"),
            "Talk does not consult the keep-recordings setting at all"
        );
        assert!(
            body.contains("fs::remove_file(&path)"),
            "the utterance's audio must not outlive the utterance"
        );
        assert!(
            body.contains("require_grant("),
            "the microphone is still gated on a capture grant"
        );
    }

    #[test]
    fn talk_metrics_are_bounded_and_contain_no_audio() {
        let root = TempRoot::new();
        let state = M7CompanionState::production(&root.0).unwrap();
        for offset in 0..105 {
            state
                .record_talk_metric(TalkMetric {
                    created_at_ms: 1_000 + offset,
                    speech_detection_ms: Some(180),
                    stt_ms: Some(240),
                    model_first_token_ms: Some(320),
                    tts_first_audio_ms: Some(410),
                    end_to_end_ms: Some(720),
                    interrupted: offset % 2 == 0,
                    fallback: offset % 3 == 0,
                })
                .unwrap();
        }
        let snapshot = state.talk_metrics().unwrap();
        assert_eq!(snapshot.metrics.len(), MAX_TALK_METRICS);
        assert_eq!(snapshot.metrics[0].created_at_ms, 1_005);
        let persisted = fs::read_to_string(state.root.join(TALK_METRICS_FILE)).unwrap();
        assert!(!persisted.contains("audio"));
        assert!(!persisted.contains("transcript"));

        state.clear_talk_metrics().unwrap();
        assert!(state.talk_metrics().unwrap().metrics.is_empty());
    }
}

//! GGUF model registry + download manager.
//!
//! Owns the curated list of known-good local models (small, tool-calling
//! capable instruct models that work well with llama-server's `--jinja`
//! OpenAI-style tool calling), plus commands to list, download, and delete
//! model weight files under the app's `models` data directory.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;

use crate::model_sources::{self, ManagedModelProvenance};
use crate::permissions;
use crate::process_lock::{
    acquire_cross_process_lock, try_acquire_cross_process_lock, CrossProcessFileLock,
};
use crate::profiles::ProfileScopedPaths;
use crate::AppState;

const BUNDLE_REGISTRY_SCHEMA_VERSION: u32 = 1;

static ACTIVE_BUNDLE_INSTALLS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

struct BundleInstallCleanup {
    reference: String,
}

impl Drop for BundleInstallCleanup {
    fn drop(&mut self) {
        if let Some(active) = ACTIVE_BUNDLE_INSTALLS.get() {
            if let Ok(mut active) = active.lock() {
                active.remove(&self.reference);
            }
        }
    }
}

fn begin_bundle_install(reference: &str) -> Result<BundleInstallCleanup, String> {
    let active = ACTIVE_BUNDLE_INSTALLS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut active = active
        .lock()
        .map_err(|_| "Active bundle-install lock poisoned".to_string())?;
    if !active.insert(reference.to_string()) {
        return Err(format!(
            "A model bundle installation for reference '{reference}' is already running"
        ));
    }
    Ok(BundleInstallCleanup {
        reference: reference.to_string(),
    })
}

fn bundle_installation_active() -> bool {
    ACTIVE_BUNDLE_INSTALLS
        .get()
        .and_then(|active| active.lock().ok())
        .is_some_and(|active| !active.is_empty())
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ComponentOwnership {
    Managed,
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GgufArtifactKind {
    LanguageModel,
    Projector,
    Unknown,
}

fn is_projector_filename(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.contains("mmproj") || lower.contains("projector")
}

fn classify_gguf_artifact_metadata(architecture: Option<&str>) -> GgufArtifactKind {
    // llama.cpp uses the `clip` GGUF architecture for image encoders and
    // multimodal projector files; any other declared architecture belongs to
    // a standalone model family. Filename hints are considered only when the
    // bounded metadata parser cannot establish an architecture.
    match architecture
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(architecture) if architecture.eq_ignore_ascii_case("clip") => {
            GgufArtifactKind::Projector
        }
        Some(_) => GgufArtifactKind::LanguageModel,
        None => GgufArtifactKind::Unknown,
    }
}

fn classify_gguf_artifact_with_source(path: &Path, file_name: &str) -> (GgufArtifactKind, bool) {
    let metadata_kind = crate::quantization::sniff_gguf_file(path)
        .ok()
        .map(|header| classify_gguf_artifact_metadata(header.architecture.as_deref()))
        .unwrap_or(GgufArtifactKind::Unknown);
    if metadata_kind != GgufArtifactKind::Unknown {
        return (metadata_kind, metadata_kind == GgufArtifactKind::Projector);
    }
    if is_projector_filename(file_name) {
        (GgufArtifactKind::Projector, false)
    } else {
        (GgufArtifactKind::Unknown, false)
    }
}

fn classify_gguf_artifact(path: &Path, file_name: &str) -> GgufArtifactKind {
    classify_gguf_artifact_with_source(path, file_name).0
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ProjectorComponent {
    pub path: String,
    pub file: String,
    pub size_bytes: u64,
    pub ownership: ComponentOwnership,
    #[serde(default)]
    pub sha256: Option<String>,
    #[serde(default)]
    pub missing: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub struct ModelComponents {
    #[serde(default)]
    pub projector: Option<ProjectorComponent>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    #[serde(default = "default_text_capability")]
    pub text: bool,
    #[serde(default)]
    pub image_input: bool,
}

fn default_text_capability() -> bool {
    true
}

impl Default for ModelCapabilities {
    fn default() -> Self {
        Self {
            text: true,
            image_input: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
struct LocalModelBundleRegistry {
    schema_version: u32,
    #[serde(default)]
    bundles: Vec<LocalModelBundleRecord>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct LocalModelBundleRecord {
    model_path: String,
    projector: Option<ProjectorComponent>,
}

/// Whether a `ModelInfo` is a chat (tool-calling instruct) model or an
/// embedding model — added for the RAG/Knowledge Stacks feature so
/// `curated_models()` can list embedding models (nomic-embed-text,
/// bge-m3) alongside the existing chat models without the Knowledge panel
/// having to guess from the name. `#[serde(default)]` on `ModelInfo::kind`
/// (via `#[default]` here) keeps any old persisted/cached JSON that predates
/// this field parsing as `Chat`, the only kind that existed before.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelKind {
    #[default]
    Chat,
    Embedding,
}

/// Metadata describing a single GGUF model, whether curated (remote) or
/// discovered on disk (installed).
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub file: String,
    pub size_gb: f32,
    pub tool_calling: bool,
    pub installed: bool,
    pub path: Option<String>,
    /// True for a model registered via `models_add_external` — a `.gguf`
    /// file living outside the app's managed models directory that the app
    /// never owns. Distinguishes `models_delete` (removes app-downloaded
    /// weights from disk) from `models_remove_external` (only forgets the
    /// reference) on the frontend.
    #[serde(default)]
    pub is_external: bool,
    /// Chat vs. embedding model — see [`ModelKind`]. Defaults to `Chat` for
    /// read-compatibility with anything serialized before this field existed
    /// (there were no embedding entries before, so that default is always
    /// correct for old data).
    #[serde(default)]
    pub kind: ModelKind,
    #[serde(default)]
    pub components: ModelComponents,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
}

/// The curated registry: a small, hand-picked set of instruct models known
/// to work well with llama.cpp's OpenAI-compatible tool calling. `pub` so
/// monkey-cli's launcher can offer the same "Recommended" list the desktop
/// app's model tab shows, rather than keeping a second copy of it.
pub fn curated_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "qwen2.5-7b".to_string(),
            name: "Qwen2.5 7B Instruct".to_string(),
            repo: "Qwen/Qwen2.5-7B-Instruct-GGUF".to_string(),
            file: "qwen2.5-7b-instruct-q4_k_m.gguf".to_string(),
            size_gb: 4.7,
            tool_calling: true,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        },
        ModelInfo {
            id: "qwen2.5-coder-14b".to_string(),
            name: "Qwen2.5 Coder 14B Instruct".to_string(),
            repo: "Qwen/Qwen2.5-Coder-14B-Instruct-GGUF".to_string(),
            file: "qwen2.5-coder-14b-instruct-q4_k_m.gguf".to_string(),
            size_gb: 9.0,
            tool_calling: true,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        },
        ModelInfo {
            id: "llama-3.1-8b".to_string(),
            name: "Llama 3.1 8B Instruct".to_string(),
            repo: "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF".to_string(),
            file: "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf".to_string(),
            size_gb: 4.9,
            tool_calling: true,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        },
        ModelInfo {
            id: "hermes-3-8b".to_string(),
            name: "Hermes 3 Llama 3.1 8B".to_string(),
            repo: "NousResearch/Hermes-3-Llama-3.1-8B-GGUF".to_string(),
            file: "Hermes-3-Llama-3.1-8B.Q4_K_M.gguf".to_string(),
            size_gb: 4.9,
            tool_calling: true,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        },
        ModelInfo {
            id: "mistral-nemo-12b".to_string(),
            name: "Mistral Nemo 2407 Instruct".to_string(),
            repo: "bartowski/Mistral-Nemo-Instruct-2407-GGUF".to_string(),
            file: "Mistral-Nemo-Instruct-2407-Q4_K_M.gguf".to_string(),
            size_gb: 7.5,
            tool_calling: true,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        },
        // Embedding models for the RAG/Knowledge Stacks feature
        // (stacks.rs) — the managed-llama embedding backend. Repo/file
        // verified against huggingface.co at implementation time (both are
        // real, currently-existing quant files, not guessed from the design
        // doc's names).
        ModelInfo {
            id: "nomic-embed-text-v1.5".to_string(),
            name: "Nomic Embed Text v1.5".to_string(),
            repo: "nomic-ai/nomic-embed-text-v1.5-GGUF".to_string(),
            file: "nomic-embed-text-v1.5.Q8_0.gguf".to_string(),
            size_gb: 0.15,
            tool_calling: false,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Embedding,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities {
                text: true,
                image_input: false,
            },
        },
        // Design doc named "bge-m3" without pinning an exact repo/quant;
        // `gpustack/bge-m3-GGUF` (a maintained third-party GGUF conversion —
        // BAAI's own `BAAI/bge-m3` repo ships Safetensors only, no GGUF) is
        // the closest real equivalent, verified against huggingface.co at
        // implementation time. Q4_K_M chosen for a reasonable size/quality
        // default, matching this registry's other Q4_K_M curated picks.
        ModelInfo {
            id: "bge-m3".to_string(),
            name: "BGE-M3 (multilingual)".to_string(),
            repo: "gpustack/bge-m3-GGUF".to_string(),
            file: "bge-m3-Q4_K_M.gguf".to_string(),
            size_gb: 0.44,
            tool_calling: false,
            installed: false,
            path: None,
            is_external: false,
            kind: ModelKind::Embedding,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities {
                text: true,
                image_input: false,
            },
        },
    ]
}

/// Resolves (and creates, if missing) `<app_data_dir>/models`.
pub(crate) fn models_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    let dir = base.join("models");
    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Failed to create models directory {}: {e}", dir.display()))?;
    }
    Ok(dir)
}

/// Returns the hardcoded curated model registry for display in the UI.
#[tauri::command]
pub fn models_list_curated() -> Vec<ModelInfo> {
    curated_models()
}

/// Strips a `.gguf`/`.GGUF` extension off a filename for use as a display
/// name, falling back to the filename itself if it has neither.
fn strip_gguf_extension(filename: &str) -> String {
    filename
        .strip_suffix(".gguf")
        .or_else(|| filename.strip_suffix(".GGUF"))
        .unwrap_or(filename)
        .to_string()
}

fn managed_model_info(path: &Path, provenance: &ManagedModelProvenance) -> ModelInfo {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(provenance.local_file_name.as_str())
        .to_string();
    ModelInfo {
        id: format!("managed:{}", provenance.sha256),
        name: provenance.display_name.clone(),
        repo: provenance.repo.clone(),
        file,
        size_gb: provenance.size_bytes as f32 / 1_000_000_000.0,
        tool_calling: provenance.tool_calling,
        installed: true,
        path: Some(path.to_string_lossy().to_string()),
        is_external: false,
        kind: ModelKind::Chat,
        components: ModelComponents::default(),
        capabilities: ModelCapabilities::default(),
    }
}

fn unverified_managed_model_info(path: &Path, filename: &str, size_gb: f32) -> ModelInfo {
    ModelInfo {
        id: format!("custom:{filename}"),
        name: strip_gguf_extension(filename),
        repo: String::new(),
        file: filename.to_string(),
        size_gb,
        // A filename alone cannot prove that the model supports tools.
        tool_calling: false,
        installed: true,
        path: Some(path.to_string_lossy().to_string()),
        is_external: false,
        kind: ModelKind::Chat,
        components: ModelComponents::default(),
        capabilities: ModelCapabilities::default(),
    }
}

fn bundle_registry_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base).map_err(|e| {
            format!(
                "Failed to create app data directory {}: {e}",
                base.display()
            )
        })?;
    }
    Ok(base.join("local_model_bundles.json"))
}

fn bundle_registry_lock_path(path: &Path) -> PathBuf {
    path.with_extension("json.lock")
}

fn lock_bundle_registry(path: &Path) -> Result<CrossProcessFileLock, String> {
    acquire_cross_process_lock(&bundle_registry_lock_path(path))
}

fn load_bundle_registry(app: &AppHandle) -> Result<LocalModelBundleRegistry, String> {
    let path = bundle_registry_path(app)?;
    load_bundle_registry_from_path(&path)
}

fn load_bundle_registry_from_path(path: &Path) -> Result<LocalModelBundleRegistry, String> {
    if !path.exists() {
        return Ok(LocalModelBundleRegistry {
            schema_version: BUNDLE_REGISTRY_SCHEMA_VERSION,
            bundles: Vec::new(),
        });
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    let registry: LocalModelBundleRegistry = serde_json::from_str(&raw).map_err(|e| {
        format!(
            "Invalid local model bundle registry {}: {e}",
            path.display()
        )
    })?;
    if registry.schema_version > BUNDLE_REGISTRY_SCHEMA_VERSION {
        return Err(format!(
            "Local model bundle registry version {} is newer than this app supports",
            registry.schema_version
        ));
    }
    Ok(registry)
}

/// Reads the profile-scoped projector association without requiring a Tauri
/// handle. The managed CLI uses this to preserve the desktop app's bundle
/// metadata when it starts an already-installed model offline.
pub fn projector_for_model(
    profile_data_dir: &Path,
    model_path: &Path,
) -> Result<Option<ProjectorComponent>, String> {
    let model_path = model_path
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize model {}: {e}", model_path.display()))?;
    let registry =
        load_bundle_registry_from_path(&profile_data_dir.join("local_model_bundles.json"))?;
    Ok(registry
        .bundles
        .into_iter()
        .find(|bundle| bundle.model_path == model_path.to_string_lossy())
        .and_then(|bundle| bundle.projector))
}

/// Persists a projector association for callers without a Tauri handle, such
/// as the managed CLI. The profile registry remains the single source of
/// truth for desktop and CLI bundle starts.
pub fn set_projector_for_model(
    profile_data_dir: &Path,
    models_dir: &Path,
    model_path: &Path,
    projector_path: &Path,
) -> Result<(), String> {
    let model = regular_gguf(&model_path.to_string_lossy())?;
    let projector = regular_gguf(&projector_path.to_string_lossy())?;
    if model == projector {
        return Err("The language model and projector must be different files".to_string());
    }
    let models_root = models_dir
        .canonicalize()
        .map_err(|e| format!("Failed to resolve models directory: {e}"))?;
    let ownership = if managed_component_path(&models_root, &projector) {
        ComponentOwnership::Managed
    } else {
        ComponentOwnership::External
    };
    let file = projector
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Projector path has no valid filename")?
        .to_string();
    let size_bytes = std::fs::metadata(&projector)
        .map_err(|e| format!("Failed to inspect projector: {e}"))?
        .len();
    model_sources::validate_local_gguf(&projector, size_bytes)
        .map_err(|e| format!("The selected projector is invalid: {e}"))?;
    let sha256 = if ownership == ComponentOwnership::Managed {
        Some(model_sources::sha256_file(&projector)?)
    } else {
        None
    };
    let registry_path = profile_data_dir.join("local_model_bundles.json");
    let _registry_lock = lock_bundle_registry(&registry_path)?;
    let mut registry = load_bundle_registry_from_path(&registry_path)?;
    let component = ProjectorComponent {
        path: projector.to_string_lossy().into_owned(),
        file,
        size_bytes,
        ownership,
        sha256,
        missing: false,
    };
    if let Some(record) = registry
        .bundles
        .iter_mut()
        .find(|record| record.model_path == model.to_string_lossy())
    {
        record.projector = Some(component);
    } else {
        registry.bundles.push(LocalModelBundleRecord {
            model_path: model.to_string_lossy().into_owned(),
            projector: Some(component),
        });
    }
    registry.schema_version = BUNDLE_REGISTRY_SCHEMA_VERSION;
    save_bundle_registry_to_path(&registry_path, &registry)
}

/// Associates a freshly installed bundle and rolls back any newly published
/// components if the registry write fails. The caller keeps the install lock in
/// `installed` until this function returns, so component GC cannot race the
/// association.
pub async fn associate_installed_bundle(
    profile_data_dir: &Path,
    models_dir: &Path,
    installed: &model_sources::InstalledModelReference,
) -> Result<(), String> {
    let Some(projector_path) = installed.projector_path.as_ref() else {
        return Ok(());
    };
    if let Err(error) = set_projector_for_model(
        profile_data_dir,
        models_dir,
        &installed.local_path,
        projector_path,
    ) {
        rollback_installed_bundle(profile_data_dir, models_dir, installed).await;
        return Err(error);
    }
    Ok(())
}

async fn rollback_installed_bundle(
    profile_data_dir: &Path,
    models_dir: &Path,
    installed: &model_sources::InstalledModelReference,
) {
    if installed.model_was_new {
        let _ = model_sources::delete_installed_model(models_dir, &installed.local_path).await;
    }
    if installed.projector_was_new {
        let registry_path = profile_data_dir.join("local_model_bundles.json");
        if let Ok(_registry_lock) = lock_bundle_registry(&registry_path) {
            if let Ok(registry) = load_bundle_registry_from_path(&registry_path) {
                if let Some(projector_path) = installed.projector_path.as_ref() {
                    if !managed_projector_referenced(&registry, projector_path) {
                        let _ = std::fs::remove_file(projector_path);
                    }
                }
            }
        }
    }
}

fn save_bundle_registry_to_path(
    path: &Path,
    registry: &LocalModelBundleRegistry,
) -> Result<(), String> {
    let raw = serde_json::to_vec_pretty(registry)
        .map_err(|e| format!("Failed to serialize local model bundle registry: {e}"))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, raw)
        .map_err(|e| format!("Failed to stage {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|e| format!("Failed to publish {}: {e}", path.display()))
}

fn regular_gguf(path: &str) -> Result<PathBuf, String> {
    let input = PathBuf::from(path);
    let metadata =
        std::fs::symlink_metadata(&input).map_err(|e| format!("File not found: {path} ({e})"))?;
    if !metadata.file_type().is_file() {
        return Err(format!("Not a regular file: {path}"));
    }
    let canonical = input
        .canonicalize()
        .map_err(|e| format!("Failed to canonicalize {path}: {e}"))?;
    if !canonical
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
    {
        return Err("Only regular .gguf files are supported".to_string());
    }
    Ok(canonical)
}

fn apply_bundle_record(model: &mut ModelInfo, record: Option<&LocalModelBundleRecord>) {
    let Some(record) = record else {
        return;
    };
    let Some(projector) = record.projector.clone() else {
        return;
    };
    let missing = !Path::new(&projector.path).is_file();
    model.components.projector = Some(ProjectorComponent {
        missing,
        ..projector
    });
    model.capabilities.image_input = !missing;
}

fn apply_bundle_registry(models: &mut [ModelInfo], registry: &LocalModelBundleRegistry) {
    for model in models {
        let Some(path) = model.path.as_deref() else {
            continue;
        };
        let record = registry
            .bundles
            .iter()
            .find(|record| record.model_path == path);
        apply_bundle_record(model, record);
    }
}

fn managed_components_dir(models_dir: &Path) -> PathBuf {
    models_dir.join("components")
}

fn managed_component_path(models_dir: &Path, path: &Path) -> bool {
    path.parent() == Some(managed_components_dir(models_dir).as_path())
}

fn managed_projector_referenced(
    registry: &LocalModelBundleRegistry,
    projector_path: &Path,
) -> bool {
    let projector_path = projector_path.to_string_lossy();
    registry.bundles.iter().any(|bundle| {
        bundle.projector.as_ref().is_some_and(|projector| {
            projector.ownership == ComponentOwnership::Managed && projector.path == projector_path
        })
    })
}

fn reconcile_managed_components(
    models_dir: &Path,
    registry: &LocalModelBundleRegistry,
) -> Result<(), String> {
    let root = managed_components_dir(models_dir);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Failed to inspect managed components: {error}")),
    };
    let referenced = registry
        .bundles
        .iter()
        .filter(|bundle| Path::new(&bundle.model_path).is_file())
        .filter_map(|bundle| bundle.projector.as_ref())
        .filter(|projector| projector.ownership == ComponentOwnership::Managed)
        .map(|projector| projector.path.as_str())
        .collect::<std::collections::HashSet<_>>();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let is_component_artifact = name.starts_with("mmproj-")
            && (name.ends_with(".gguf") || name.ends_with(".gguf.part"));
        if !is_component_artifact
            || !entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        {
            continue;
        }
        let canonical = path.canonicalize().unwrap_or(path.clone());
        if component_install_lock_held(&path)? {
            continue;
        }
        if !referenced.contains(canonical.to_string_lossy().as_ref()) {
            std::fs::remove_file(&path).map_err(|error| {
                format!(
                    "Failed to garbage-collect orphaned projector {}: {error}",
                    path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn component_install_lock_held(path: &Path) -> Result<bool, String> {
    let file = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let destination_name = file.strip_suffix(".part").unwrap_or(file);
    let lock_path = path.with_file_name(format!("{destination_name}.install.lock"));
    if !lock_path.exists() {
        return Ok(false);
    }
    Ok(try_acquire_cross_process_lock(&lock_path)?.is_none())
}

pub fn verify_projector_for_model(
    profile_data_dir: &Path,
    model_path: &str,
    projector_path: &str,
) -> Result<(), String> {
    let model = regular_gguf(model_path)?;
    let projector = regular_gguf(projector_path)?;
    if model == projector {
        return Err("The language model and projector must be different files".to_string());
    }
    let associated = projector_for_model(profile_data_dir, &model)?
        .ok_or("The selected language model has no associated multimodal projector")?;
    if associated.path != projector.to_string_lossy() {
        return Err(
            "The selected projector is not associated with this language model".to_string(),
        );
    }
    let size = std::fs::metadata(&projector)
        .map_err(|e| format!("Failed to inspect projector: {e}"))?
        .len();
    if size != associated.size_bytes {
        return Err(format!(
            "The configured multimodal projector changed size; expected {} bytes, got {size}",
            associated.size_bytes
        ));
    }
    model_sources::validate_local_gguf(&projector, size)
        .map_err(|e| format!("The configured multimodal projector is invalid: {e}"))?;
    if let Some(expected_sha256) = associated.sha256.as_deref() {
        let actual_sha256 = model_sources::sha256_file(&projector)?;
        if actual_sha256 != expected_sha256 {
            return Err(format!(
                "The configured multimodal projector failed SHA-256 verification: expected {expected_sha256}, got {actual_sha256}"
            ));
        }
    }
    Ok(())
}

pub fn verify_projector_for_runtime(
    app: &AppHandle,
    model_path: &str,
    projector_path: &str,
) -> Result<(), String> {
    let profile_data_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    verify_projector_for_model(&profile_data_dir, model_path, projector_path)
}

fn known_main_model(app: &AppHandle, path: &Path) -> Result<(), String> {
    let dir = models_dir(app)?.canonicalize().map_err(|e| e.to_string())?;
    let is_managed = path.parent() == Some(dir.as_path());
    let is_external = load_external_registry(app)?
        .iter()
        .any(|entry| entry.path == path.to_string_lossy());
    if !is_managed && !is_external {
        return Err("The selected language model is not a known installed model".to_string());
    }
    Ok(())
}

/// Scans the models directory on disk and returns every `.gguf` file found
/// there — cross-referenced against the curated registry by exact filename
/// match where possible (`installed: true`, curated metadata + `path`
/// populated), and surfaced as an ad-hoc entry otherwise (e.g. a file
/// fetched via a custom, non-curated Hugging Face repo/file pull) — plus
/// every live entry from the external-file registry (`.gguf` files outside
/// the app's models directory, registered via `models_add_external`).
#[tauri::command]
pub fn models_list_installed(app: AppHandle) -> Result<Vec<ModelInfo>, String> {
    let dir = models_dir(&app)?;
    let curated = curated_models();
    let (bundle_registry, bundle_registry_valid) = match load_bundle_registry(&app) {
        Ok(registry) => (registry, true),
        Err(error) => {
            eprintln!("little-monkey: preserving unreadable bundle registry: {error}");
            (LocalModelBundleRegistry::default(), false)
        }
    };

    let entries = std::fs::read_dir(&dir)
        .map_err(|e| format!("Failed to read models directory {}: {e}", dir.display()))?;

    let mut installed = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let filename = match path.file_name().and_then(|n| n.to_str()) {
            Some(f) => f,
            None => continue,
        };
        if !filename.to_lowercase().ends_with(".gguf") {
            continue;
        }

        let size_gb = entry
            .metadata()
            .map(|metadata| metadata.len() as f32 / 1_000_000_000.0)
            .unwrap_or(0.0);
        match model_sources::load_provenance(&path) {
            Ok(Some(provenance)) => {
                installed.push(managed_model_info(&path, &provenance));
                continue;
            }
            // A malformed sidecar must fail closed rather than allowing a
            // filename match to manufacture tool-call capability.
            Err(_) => {
                installed.push(unverified_managed_model_info(&path, filename, size_gb));
                continue;
            }
            Ok(None) => {}
        }

        if let Some(curated_match) = curated.iter().find(|m| m.file == filename) {
            let mut model = curated_match.clone();
            model.installed = true;
            model.path = Some(path.to_string_lossy().to_string());
            installed.push(model);
        } else {
            installed.push(unverified_managed_model_info(&path, filename, size_gb));
        }
    }

    // Prune any external reference whose file has since moved or been
    // deleted, so a stale entry never lingers as a broken "Start" button.
    let external = load_external_registry(&app)?;
    let external_len_before = external.len();
    let mut live_external = Vec::with_capacity(external.len());
    for entry in external {
        if !Path::new(&entry.path).is_file() {
            continue;
        }
        let filename = Path::new(&entry.path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| entry.name.clone());
        installed.push(ModelInfo {
            id: entry.id.clone(),
            name: entry.name.clone(),
            repo: String::new(),
            file: filename,
            size_gb: entry.size_gb,
            tool_calling: false,
            installed: true,
            path: Some(entry.path.clone()),
            is_external: true,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        });
        live_external.push(entry);
    }
    if live_external.len() != external_len_before {
        let _ = save_external_registry(&app, &live_external);
    }

    apply_bundle_registry(&mut installed, &bundle_registry);
    if bundle_registry_valid && !bundle_installation_active() {
        if let Err(error) = reconcile_managed_components(&dir, &bundle_registry) {
            eprintln!("little-monkey: could not reconcile managed components: {error}");
        }
    }

    Ok(installed)
}

/// A model file living outside the app's managed models directory, added by
/// the user via a native file picker. Only this reference (path + display
/// metadata) is persisted — the app never owns, copies, or deletes the
/// underlying file.
#[derive(Debug, Serialize, Deserialize, Clone)]
struct ExternalModelEntry {
    id: String,
    name: String,
    path: String,
    size_gb: f32,
}

/// Path to the JSON file backing the external-model registry, creating the
/// app data directory if needed.
fn external_registry_path(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base).map_err(|e| {
            format!(
                "Failed to create app data directory {}: {e}",
                base.display()
            )
        })?;
    }
    Ok(base.join("external_models.json"))
}

/// Loads the external-model registry, treating a missing or unparsable file
/// as empty rather than an error — it's app-owned bookkeeping, not user data
/// worth failing loudly over.
fn load_external_registry(app: &AppHandle) -> Result<Vec<ExternalModelEntry>, String> {
    let path = external_registry_path(app)?;
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_external_registry(app: &AppHandle, entries: &[ExternalModelEntry]) -> Result<(), String> {
    let path = external_registry_path(app)?;
    let raw = serde_json::to_string_pretty(entries)
        .map_err(|e| format!("Failed to serialize external model registry: {e}"))?;
    std::fs::write(&path, raw).map_err(|e| format!("Failed to write {}: {e}", path.display()))
}

/// Registers an arbitrary `.gguf` file already on disk — picked via a native
/// file dialog on the frontend — as a usable local model. Idempotent: if
/// `path` (after canonicalization) is already registered, returns the
/// existing entry rather than duplicating it.
#[tauri::command]
pub fn models_add_external(app: AppHandle, path: String) -> Result<ModelInfo, String> {
    let p = PathBuf::from(&path)
        .canonicalize()
        .map_err(|e| format!("File not found: {path} ({e})"))?;

    if !p.is_file() {
        return Err(format!("Not a file: {path}"));
    }
    let extension_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("gguf"))
        .unwrap_or(false);
    if !extension_ok {
        return Err("Only .gguf model files are supported".to_string());
    }

    let filename = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("Invalid file name: {path}"))?
        .to_string();
    if classify_gguf_artifact(&p, &filename) == GgufArtifactKind::Projector {
        return Err(
            "The selected file appears to be a multimodal projector, not a language model."
                .to_string(),
        );
    }
    let canonical = p.to_string_lossy().to_string();

    let mut entries = load_external_registry(&app)?;
    if let Some(existing) = entries.iter().find(|e| e.path == canonical) {
        return Ok(ModelInfo {
            id: existing.id.clone(),
            name: existing.name.clone(),
            repo: String::new(),
            file: filename,
            size_gb: existing.size_gb,
            tool_calling: false,
            installed: true,
            path: Some(canonical),
            is_external: true,
            kind: ModelKind::Chat,
            components: ModelComponents::default(),
            capabilities: ModelCapabilities::default(),
        });
    }

    let name = strip_gguf_extension(&filename);
    let size_gb = std::fs::metadata(&p)
        .map(|m| m.len() as f32 / 1_000_000_000.0)
        .unwrap_or(0.0);
    let id = format!("external:{canonical}");

    entries.push(ExternalModelEntry {
        id: id.clone(),
        name: name.clone(),
        path: canonical.clone(),
        size_gb,
    });
    save_external_registry(&app, &entries)?;

    Ok(ModelInfo {
        id,
        name,
        repo: String::new(),
        file: filename,
        size_gb,
        tool_calling: false,
        installed: true,
        path: Some(canonical),
        is_external: true,
        kind: ModelKind::Chat,
        components: ModelComponents::default(),
        capabilities: ModelCapabilities::default(),
    })
}

/// Forgets a previously-registered external model reference by id. Never
/// touches the underlying file on disk — it isn't owned by the app.
#[tauri::command]
pub fn models_remove_external(app: AppHandle, id: String) -> Result<(), String> {
    let bundle_registry_path = bundle_registry_path(&app)?;
    let _bundle_registry_lock = lock_bundle_registry(&bundle_registry_path)?;
    let mut entries = load_external_registry(&app)?;
    let removed_path = entries
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.path.clone());
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == before {
        return Err(format!("No external model registered with id '{id}'"));
    }
    save_external_registry(&app, &entries)?;
    if let Some(path) = removed_path {
        let mut registry = load_bundle_registry_from_path(&bundle_registry_path)?;
        registry.bundles.retain(|bundle| bundle.model_path != path);
        save_bundle_registry_to_path(&bundle_registry_path, &registry)?;
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ProjectorCandidate {
    pub path: String,
    pub file: String,
    pub size_bytes: u64,
}

/// Finds sibling GGUFs that are metadata-confirmed or filename-hinted
/// multimodal projector files. This is a hint for the picker only; binding
/// still requires an explicit user action and runtime startup remains the
/// compatibility boundary.
#[tauri::command(rename_all = "camelCase")]
pub fn models_detect_projectors(
    app: AppHandle,
    model_path: String,
) -> Result<Vec<ProjectorCandidate>, String> {
    let model = regular_gguf(&model_path)?;
    known_main_model(&app, &model)?;
    let Some(parent) = model.parent() else {
        return Ok(Vec::new());
    };
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(parent)
        .map_err(|e| format!("Failed to scan {}: {e}", parent.display()))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        if path == model || !path.is_file() {
            continue;
        }
        let Some(file) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !file.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(_) => continue,
        };
        let (kind, metadata_confirmed) = classify_gguf_artifact_with_source(&canonical, file);
        if kind != GgufArtifactKind::Projector {
            continue;
        }
        let size_bytes = match std::fs::metadata(&canonical) {
            Ok(metadata) => metadata.len(),
            Err(_) => continue,
        };
        candidates.push((
            !metadata_confirmed,
            ProjectorCandidate {
                path: canonical.to_string_lossy().into_owned(),
                file: file.to_string(),
                size_bytes,
            },
        ));
    }
    candidates.sort_by(|(left_hint, left), (right_hint, right)| {
        left_hint
            .cmp(right_hint)
            .then_with(|| left.file.cmp(&right.file))
    });
    Ok(candidates
        .into_iter()
        .map(|(_, candidate)| candidate)
        .collect())
}

fn model_info_for_path(app: &AppHandle, path: &Path) -> Result<ModelInfo, String> {
    let path_string = path.to_string_lossy().into_owned();
    models_list_installed(app.clone())?
        .into_iter()
        .find(|model| model.path.as_deref() == Some(path_string.as_str()))
        .ok_or_else(|| {
            format!(
                "The language model is no longer installed: {}",
                path.display()
            )
        })
}

/// Associates one explicitly selected projector with one known language model.
/// The registry stores only the association and provenance, never duplicate
/// model metadata or copied external bytes.
#[tauri::command(rename_all = "camelCase")]
pub fn models_set_projector(
    app: AppHandle,
    model_path: String,
    projector_path: String,
) -> Result<ModelInfo, String> {
    let model = regular_gguf(&model_path)?;
    let projector = regular_gguf(&projector_path)?;
    known_main_model(&app, &model)?;
    let profile_data_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    let models_dir = models_dir(&app)?;
    set_projector_for_model(&profile_data_dir, &models_dir, &model, &projector)?;
    model_info_for_path(&app, &model)
}

#[tauri::command(rename_all = "camelCase")]
pub fn models_remove_projector(app: AppHandle, model_path: String) -> Result<ModelInfo, String> {
    let model = regular_gguf(&model_path)?;
    known_main_model(&app, &model)?;
    let bundle_registry_path = bundle_registry_path(&app)?;
    let _bundle_registry_lock = lock_bundle_registry(&bundle_registry_path)?;
    let mut registry = load_bundle_registry_from_path(&bundle_registry_path)?;
    let old = registry
        .bundles
        .iter()
        .find(|record| record.model_path == model.to_string_lossy())
        .and_then(|record| record.projector.clone());
    registry
        .bundles
        .retain(|record| record.model_path != model.to_string_lossy());
    save_bundle_registry_to_path(&bundle_registry_path, &registry)?;
    if let Some(projector) = old {
        if projector.ownership == ComponentOwnership::Managed {
            let root = models_dir(&app)?
                .canonicalize()
                .map_err(|e| e.to_string())?;
            let path = PathBuf::from(&projector.path);
            if managed_component_path(&root, &path)
                && !managed_projector_referenced(&registry, &path)
                && path.is_file()
            {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    model_info_for_path(&app, &model)
}

/// Returns true if `s` is exactly one path component: non-empty, contains
/// no path separator, and isn't `.`/`..`. `Path::join` treats an absolute
/// second argument as replacing the base entirely and happily resolves `..`
/// segments lexically, so anything that isn't a single plain component must
/// be rejected *before* it's joined onto a directory we intend to write in.
fn is_safe_path_component(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains('/') && !s.contains('\\')
}

/// Validates a Hugging Face `<org>/<name>` repo identifier: exactly two
/// safe components joined by a single `/`, restricted to a conservative
/// charset. This string is interpolated directly into the download URL, so
/// a malformed value could otherwise be used to smuggle extra path segments
/// or unexpected characters into the request.
fn validate_repo(repo: &str) -> Result<(), String> {
    let valid_charset = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };

    match repo.split('/').collect::<Vec<_>>().as_slice() {
        [org, name]
            if is_safe_path_component(org)
                && is_safe_path_component(name)
                && valid_charset(org)
                && valid_charset(name) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "Invalid repo '{repo}': expected '<org>/<name>' using only letters, digits, '-', '_', '.'"
        )),
    }
}

/// Validates a model weight file name: must be a single plain path
/// component with no separators or traversal segments, so
/// `dir.join(&file)` can never escape `dir` — e.g. an absolute path like
/// `/Users/x/Library/LaunchAgents/evil.plist` (which `Path::join` would
/// otherwise resolve to exactly that path, discarding `dir` entirely) or a
/// relative traversal like `../../../etc/passwd`.
fn validate_filename(file: &str) -> Result<(), String> {
    if !is_safe_path_component(file) {
        return Err(format!(
            "Invalid file name '{file}': must be a plain filename with no path separators"
        ));
    }
    Ok(())
}

/// Streams `<file>` from the given Hugging Face `<repo>` (main branch) into
/// the models directory, emitting `models://download-progress` events
/// ({file, downloaded, total}) as bytes arrive. Downloads to a temporary
/// `.part` file first and atomically renames on success, so a crashed or
/// cancelled download is never mistaken for an installed model.
///
/// Cancellable via the `CancellationToken` registered in
/// `AppState::model_downloads` under `file` (see `models_cancel_download`) —
/// same pattern as `stacks::reindex_impl`/`stacks_cancel_index`.
#[tauri::command]
pub async fn models_download(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    repo: String,
    file: String,
) -> Result<String, String> {
    validate_repo(&repo)?;
    validate_filename(&file)?;

    let dir = models_dir(&app)?;
    let dir_canon = dir
        .canonicalize()
        .map_err(|e| format!("Failed to resolve models directory {}: {e}", dir.display()))?;

    let dest_path = dir_canon.join(&file);
    let tmp_path = dir_canon.join(format!("{file}.part"));

    // Defense in depth: even though `file` was already validated above as a
    // single safe path component (so this can never actually trip), require
    // both target paths to land directly inside the canonicalized models
    // directory before anything is created — mirroring the containment
    // check tools.rs::resolve_in_workspace performs for workspace paths.
    if dest_path.parent() != Some(dir_canon.as_path())
        || tmp_path.parent() != Some(dir_canon.as_path())
    {
        return Err(format!("Invalid file name '{file}'"));
    }

    let cancel = {
        let mut cancels = state
            .model_downloads
            .lock()
            .map_err(|_| "Model-download-cancel lock poisoned".to_string())?;
        cancels
            .entry(file.clone())
            .or_insert_with(|| std::sync::Arc::new(tokio_util::sync::CancellationToken::new()))
            .clone()
    };
    // RAII-style cleanup so the cancel handle never lingers past this
    // download, whether it finishes normally, errors, or is cancelled.
    let _cleanup = DownloadCancelCleanup {
        state: &state,
        file: file.clone(),
    };

    match download_to_file(&app, &repo, &file, &tmp_path, &cancel).await {
        Ok(()) => {
            tokio::fs::rename(&tmp_path, &dest_path)
                .await
                .map_err(|e| {
                    format!(
                        "Download completed but failed to move into place at {}: {e}",
                        dest_path.display()
                    )
                })?;
            Ok(dest_path.to_string_lossy().to_string())
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            Err(e)
        }
    }
}

/// Removes `file`'s cancellation handle from `AppState::model_downloads` on
/// drop, so a finished/errored/cancelled download never leaves a stale entry
/// behind for a later `models_cancel_download` call to (harmlessly, but
/// pointlessly) find.
struct DownloadCancelCleanup<'a> {
    state: &'a AppState,
    file: String,
}

impl Drop for DownloadCancelCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut cancels) = self.state.model_downloads.lock() {
            cancels.remove(&self.file);
        }
    }
}

/// Best-effort cancellation, like `stacks_cancel_index`: if no download is
/// currently running for `file`, this is simply a no-op (nothing to cancel).
#[tauri::command]
pub fn models_cancel_download(
    state: tauri::State<'_, AppState>,
    file: String,
) -> Result<(), String> {
    let cancels = state
        .model_downloads
        .lock()
        .map_err(|_| "Model-download-cancel lock poisoned".to_string())?;
    if let Some(token) = cancels.get(&file) {
        token.cancel();
    }
    Ok(())
}

/// Resolves a public Ollama or Hugging Face reference to immutable,
/// integrity-checked GGUF bundle metadata without installing it.
#[tauri::command]
pub async fn models_resolve_reference(
    reference: String,
) -> Result<model_sources::ResolvedModelReference, String> {
    model_sources::resolve_reference(&reference).await
}

/// Re-resolves and installs a previously reviewed model resolution. The
/// expected digest makes resolution and install a two-step consent boundary.
#[tauri::command(rename_all = "camelCase")]
pub async fn models_install_reference(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    reference: String,
    expected_sha256: String,
    expected_projector_sha256: Option<String>,
) -> Result<ModelInfo, String> {
    let profile_data_dir = app
        .profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    let dir = models_dir(&app)?;
    let _bundle_cleanup = begin_bundle_install(&reference)?;
    let cancel = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    {
        let mut installs = state
            .model_downloads
            .lock()
            .map_err(|_| "Model-download-cancel lock poisoned".to_string())?;
        if installs.contains_key(&reference) {
            return Err(
                "A model bundle installation for this reference is already running".to_string(),
            );
        }
        installs.insert(reference.clone(), cancel.clone());
    }
    let _cleanup = DownloadCancelCleanup {
        state: &state,
        file: reference.clone(),
    };
    let progress_app = app.clone();
    let progress_reference = reference.clone();
    let installed = model_sources::install_reference_with_projector_and_cancel(
        &dir,
        &reference,
        &expected_sha256,
        expected_projector_sha256.as_deref(),
        &cancel,
        move |progress| {
            let _ = progress_app.emit(
                "models://download-progress",
                serde_json::json!({
                    "file": progress.file,
                    "reference": progress_reference,
                    "component": match progress.role {
                        model_sources::ModelArtifactRole::Model => "model",
                        model_sources::ModelArtifactRole::Projector => "projector",
                    },
                    "componentDownloaded": progress.downloaded,
                    "componentTotal": progress.total,
                    "downloaded": progress.overall_downloaded,
                    "total": progress.overall_total,
                }),
            );
        },
    )
    .await?;
    if installed.projector_path.is_some() {
        debug_assert!(installed.projector_install_lock_is_held());
    }
    let model = managed_model_info(&installed.local_path, &installed.provenance);
    if installed.projector_path.is_some() {
        let result = associate_installed_bundle(&profile_data_dir, &dir, &installed).await;
        return match result {
            Ok(()) => model_info_for_path(&app, &installed.local_path),
            Err(error) => Err(error),
        };
    }
    Ok(model)
}

/// Performs the actual streaming GET + write-to-disk for `models_download`,
/// emitting progress events roughly every 200ms plus a final event at
/// completion. Races each chunk read against `cancel` so a
/// `models_cancel_download` call interrupts the transfer promptly instead of
/// waiting out the current (or remaining) chunk(s).
pub(crate) async fn download_to_file(
    app: &AppHandle,
    repo: &str,
    file: &str,
    tmp_path: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), String> {
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");

    // Same silence budget as every other download path, for the same reason: this
    // client had no timeout of any kind, so a Hugging Face connection that
    // completed its handshake and then stopped writing left the progress bar
    // stopped at N bytes indefinitely, with no error and no self-recovery. Less
    // severe than the managed-model installer's version of this bug only because
    // `cancel` is wired here, so a user could get out of it by hand.
    let client = reqwest::Client::builder()
        .user_agent("LittleMonkey-Desktop/0.1")
        .connect_timeout(std::time::Duration::from_secs(20))
        .read_timeout(crate::egress::READ_TIMEOUT)
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let response = crate::egress::send(client.get(&url))
        .await
        .map_err(|e| format!("Failed to reach Hugging Face at {url}: {e}"))?;

    if !response.status().is_success() {
        return Err(format!(
            "Download failed with HTTP {} for {url}",
            response.status()
        ));
    }

    let total = response.content_length().unwrap_or(0);

    let mut out = tokio::fs::File::create(tmp_path)
        .await
        .map_err(|e| format!("Failed to create file {}: {e}", tmp_path.display()))?;

    let mut stream = response.bytes_stream();
    let mut downloaded: u64 = 0;
    let mut last_emit = std::time::Instant::now();

    loop {
        let chunk_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("Download cancelled".to_string()),
            next = stream.next() => next,
        };
        let Some(chunk_result) = chunk_result else {
            break;
        };
        let chunk = chunk_result.map_err(|e| format!("Download stream error for {file}: {e}"))?;

        out.write_all(&chunk)
            .await
            .map_err(|e| format!("Failed to write to {}: {e}", tmp_path.display()))?;

        downloaded += chunk.len() as u64;

        if last_emit.elapsed() >= std::time::Duration::from_millis(200) {
            let _ = app.emit(
                "models://download-progress",
                serde_json::json!({
                    "file": file,
                    "downloaded": downloaded,
                    "total": total,
                }),
            );
            last_emit = std::time::Instant::now();
        }
    }

    out.flush()
        .await
        .map_err(|e| format!("Failed to flush {}: {e}", tmp_path.display()))?;
    drop(out);

    let _ = app.emit(
        "models://download-progress",
        serde_json::json!({
            "file": file,
            "downloaded": downloaded,
            "total": if total > 0 { total } else { downloaded },
        }),
    );

    Ok(())
}

/// Deletes a downloaded model weight file from disk by absolute path.
///
/// Permission-gated and containment-checked: `path` is canonicalized and
/// required to live inside `models_dir()` before anything is removed, so
/// this can only ever delete files this app itself downloaded — not an
/// arbitrary file the OS user can write to.
#[tauri::command]
pub async fn models_delete(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let dir_canon = models_dir(&app)?
        .canonicalize()
        .map_err(|e| format!("Failed to resolve models directory: {e}"))?;

    let p = PathBuf::from(&path)
        .canonicalize()
        .map_err(|e| format!("File not found: {path} ({e})"))?;

    if !p.starts_with(&dir_canon) {
        return Err(format!(
            "'{}' is outside the models directory and cannot be deleted",
            path
        ));
    }
    if !p.is_file() {
        return Err(format!("Not a file: {path}"));
    }

    let detail = format!("Delete downloaded model weights at {}", p.display());
    permissions::request_permission(
        &app,
        state.inner(),
        "delete_model",
        detail,
        None,
        None,
        None,
        None,
    )
    .await?;
    let bundle_registry_path = bundle_registry_path(&app)?;
    let _bundle_registry_lock = lock_bundle_registry(&bundle_registry_path)?;
    let mut registry = load_bundle_registry_from_path(&bundle_registry_path)?;
    let projector = registry
        .bundles
        .iter()
        .find(|bundle| bundle.model_path == p.to_string_lossy())
        .and_then(|bundle| bundle.projector.clone());
    registry
        .bundles
        .retain(|bundle| bundle.model_path != p.to_string_lossy());
    let result = model_sources::delete_installed_model(&dir_canon, &p).await;
    if result.is_ok() {
        save_bundle_registry_to_path(&bundle_registry_path, &registry)?;
        if let Some(projector) = projector {
            if projector.ownership == ComponentOwnership::Managed {
                let component_root = managed_components_dir(&dir_canon);
                let component_path = PathBuf::from(projector.path);
                if component_path.parent() == Some(component_root.as_path())
                    && !managed_projector_referenced(&registry, &component_path)
                {
                    let _ = std::fs::remove_file(component_path);
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_gguf(architecture: &str) -> Vec<u8> {
        let mut bytes = b"GGUF".to_vec();
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&0_u64.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        let key = b"general.architecture";
        bytes.extend_from_slice(&(key.len() as u64).to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&8_u32.to_le_bytes());
        bytes.extend_from_slice(&(architecture.len() as u64).to_le_bytes());
        bytes.extend_from_slice(architecture.as_bytes());
        bytes
    }

    #[test]
    fn gguf_metadata_overrides_projector_filename_hints() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-gguf-classification-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();

        let neutral_projector = root.join("vision.gguf");
        std::fs::write(&neutral_projector, minimal_gguf("clip")).unwrap();
        assert_eq!(
            classify_gguf_artifact_with_source(&neutral_projector, "vision.gguf"),
            (GgufArtifactKind::Projector, true)
        );

        let misleading_model = root.join("language-projector.gguf");
        std::fs::write(&misleading_model, minimal_gguf("llama")).unwrap();
        assert_eq!(
            classify_gguf_artifact(&misleading_model, "language-projector.gguf"),
            GgufArtifactKind::LanguageModel
        );

        let unknown_projector = root.join("mmproj-unknown.gguf");
        std::fs::write(&unknown_projector, b"GGUF").unwrap();
        assert_eq!(
            classify_gguf_artifact(&unknown_projector, "mmproj-unknown.gguf"),
            GgufArtifactKind::Projector
        );

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn validate_repo_accepts_curated_repos() {
        for model in curated_models() {
            assert!(
                validate_repo(&model.repo).is_ok(),
                "curated repo '{}' should be valid",
                model.repo
            );
            assert!(
                validate_filename(&model.file).is_ok(),
                "curated file '{}' should be valid",
                model.file
            );
        }
    }

    #[test]
    fn validate_repo_rejects_extra_segments_and_bad_chars() {
        assert!(validate_repo("just-one-segment").is_err());
        assert!(validate_repo("org/name/extra").is_err());
        assert!(validate_repo("org/../escape").is_err());
        assert!(validate_repo("/absolute/repo").is_err());
        assert!(validate_repo("org name/repo name").is_err());
    }

    #[test]
    fn validate_filename_rejects_absolute_paths() {
        let err = validate_filename("/Users/x/Library/LaunchAgents/evil.plist").unwrap_err();
        assert!(err.contains("Invalid file name"), "unexpected error: {err}");
    }

    #[test]
    fn validate_filename_rejects_relative_traversal() {
        assert!(validate_filename("../../../../etc/passwd").is_err());
        assert!(validate_filename("..").is_err());
        assert!(validate_filename("").is_err());
    }

    #[test]
    fn validate_filename_accepts_plain_names() {
        assert!(validate_filename("qwen2.5-7b-instruct-q4_k_m.gguf").is_ok());
    }

    #[test]
    fn projector_association_round_trips_for_cli_lookups() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-bundle-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let model = root.join("model.gguf");
        let projector = root.join("mmproj.gguf");
        std::fs::write(&model, b"model").unwrap();
        std::fs::write(&projector, b"projector").unwrap();
        let registry = LocalModelBundleRegistry {
            schema_version: BUNDLE_REGISTRY_SCHEMA_VERSION,
            bundles: vec![LocalModelBundleRecord {
                model_path: model.canonicalize().unwrap().to_string_lossy().into_owned(),
                projector: Some(ProjectorComponent {
                    path: projector
                        .canonicalize()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    file: "mmproj.gguf".to_string(),
                    size_bytes: 9,
                    ownership: ComponentOwnership::Managed,
                    sha256: Some("digest".to_string()),
                    missing: false,
                }),
            }],
        };
        std::fs::write(
            root.join("local_model_bundles.json"),
            serde_json::to_vec(&registry).unwrap(),
        )
        .unwrap();

        let found = projector_for_model(&root, &model).unwrap().unwrap();
        assert_eq!(
            found.path,
            projector.canonicalize().unwrap().to_string_lossy()
        );
        assert_eq!(found.ownership, ComponentOwnership::Managed);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_managed_projector_survives_detach_until_last_reference() {
        let projector = PathBuf::from("/models/components/mmproj-shared.gguf");
        let component = || ProjectorComponent {
            path: projector.to_string_lossy().into_owned(),
            file: "mmproj-shared.gguf".to_string(),
            size_bytes: 10,
            ownership: ComponentOwnership::Managed,
            sha256: Some("a".repeat(64)),
            missing: false,
        };
        let mut registry = LocalModelBundleRegistry {
            schema_version: BUNDLE_REGISTRY_SCHEMA_VERSION,
            bundles: vec![
                LocalModelBundleRecord {
                    model_path: "/models/model-a.gguf".to_string(),
                    projector: Some(component()),
                },
                LocalModelBundleRecord {
                    model_path: "/models/model-b.gguf".to_string(),
                    projector: Some(component()),
                },
            ],
        };

        registry.bundles.remove(0);
        assert!(managed_projector_referenced(&registry, &projector));
        registry.bundles.clear();
        assert!(!managed_projector_referenced(&registry, &projector));
    }

    #[test]
    fn component_gc_detects_an_active_install_lock() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-component-lock-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let components = root.join("components");
        std::fs::create_dir_all(&components).unwrap();
        let component = components.join("mmproj-shared.gguf");
        std::fs::write(&component, b"component").unwrap();
        let lock_path = components.join("mmproj-shared.gguf.install.lock");
        let lock = crate::process_lock::acquire_cross_process_lock(&lock_path).unwrap();
        assert!(component_install_lock_held(&component).unwrap());
        drop(lock);
        assert!(!component_install_lock_held(&component).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_bundle_install_is_rejected_without_stealing_cleanup_ownership() {
        let reference = format!("duplicate-{}", uuid::Uuid::new_v4().simple());
        let first = begin_bundle_install(&reference).unwrap();
        assert!(begin_bundle_install(&reference).is_err());
        drop(first);
        let second = begin_bundle_install(&reference).unwrap();
        assert!(begin_bundle_install(&reference).is_err());
        drop(second);
    }
}

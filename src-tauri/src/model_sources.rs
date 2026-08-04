//! Direct, app-owned model-source resolution and verified installation.
//!
//! This module deliberately supports one portable runtime artifact: a single
//! GGUF file. Ollama references are resolved against the public Ollama OCI
//! registry and Hugging Face references are resolved through the official
//! model metadata API with blob details enabled. Neither path trusts a
//! caller-provided URL, size, or checksum: install always re-resolves the
//! original reference and requires the caller's previously observed SHA-256
//! to still match before any bytes are written.

use crate::process_lock::{acquire_cross_process_lock, CrossProcessFileLock};
use futures_util::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE,
    RANGE, WWW_AUTHENTICATE,
};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use url::{Host, Url};
use uuid::Uuid;

const USER_AGENT: &str = "LittleMonkey-Desktop/1.0";
const OLLAMA_REGISTRY_ORIGIN: &str = "https://registry.ollama.ai";
const OLLAMA_MODEL_MEDIA_TYPE: &str = "application/vnd.ollama.image.model";
const OLLAMA_ADAPTER_MEDIA_TYPE: &str = "application/vnd.ollama.image.adapter";
const OLLAMA_PROJECTOR_MEDIA_TYPE: &str = "application/vnd.ollama.image.projector";
const OLLAMA_LICENSE_MEDIA_TYPE: &str = "application/vnd.ollama.image.license";
const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MAX_REFERENCE_BYTES: usize = 4 * 1024;
const MAX_MANIFEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_HF_METADATA_BYTES: usize = 16 * 1024 * 1024;
const MAX_LICENSE_BYTES: u64 = 1024 * 1024;
const MAX_MODEL_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const MAX_PROVENANCE_BYTES: u64 = 64 * 1024;
const PROVENANCE_SCHEMA_VERSION: u32 = 2;
const GGUF_MAGIC: &[u8; 4] = b"GGUF";
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

static INSTALL_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelReferenceSource {
    OllamaRegistry,
    HuggingFace,
}

/// Public resolution receipt returned over Tauri IPC.
///
/// The casing is intentionally camelCase while `ModelInfo` stays in its
/// existing snake_case wire format.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModelReference {
    pub source: ModelReferenceSource,
    pub canonical_reference: String,
    pub display_name: String,
    pub repo: String,
    pub revision: String,
    pub file_name: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    /// Resolution is deliberately fail-closed. This becomes true only for an
    /// already-installed model whose embedded GGUF Jinja template was
    /// inspected locally; remote registry metadata never proves it.
    pub tool_calling: bool,
    pub license_name: Option<String>,
    pub license_url: Option<String>,
}

/// App-owned metadata stored beside a downloaded GGUF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ManagedModelProvenance {
    pub schema_version: u32,
    pub source: ModelReferenceSource,
    #[serde(default)]
    pub requested_reference: String,
    pub canonical_reference: String,
    pub display_name: String,
    pub repo: String,
    pub revision: String,
    pub source_file_name: String,
    pub local_file_name: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub tool_calling: bool,
    pub license_name: Option<String>,
    pub license_url: Option<String>,
    pub installed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModelReference {
    pub resolved: ResolvedModelReference,
    pub provenance: ManagedModelProvenance,
    pub local_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelDownloadProgress {
    pub file: String,
    pub downloaded: u64,
    pub total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ParsedModelReference {
    Ollama(OllamaReference),
    HuggingFace(HuggingFaceReference),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OllamaReference {
    namespace: String,
    model: String,
    tag: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HuggingFaceReference {
    repo: String,
    requested_revision: String,
    selector: HuggingFaceSelector,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HuggingFaceSelector {
    File(String),
    Quantization(String),
    DefaultQ4Km,
}

#[derive(Debug, Clone)]
struct InternalResolution {
    public: ResolvedModelReference,
    bearer_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciManifest {
    schema_version: u32,
    media_type: Option<String>,
    #[serde(default)]
    layers: Vec<OciLayer>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OciLayer {
    media_type: String,
    digest: String,
    size: u64,
}

#[derive(Debug, Deserialize)]
struct RegistryTokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryAuthChallenge {
    realm: Url,
    service: Option<String>,
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HfModelMetadata {
    id: Option<String>,
    #[serde(default)]
    private: bool,
    #[serde(default)]
    gated: serde_json::Value,
    sha: Option<String>,
    #[serde(default)]
    siblings: Vec<HfSibling>,
    card_data: Option<HfCardData>,
}

#[derive(Debug, Clone, Deserialize)]
struct HfSibling {
    rfilename: String,
    size: Option<u64>,
    lfs: Option<HfLfs>,
}

#[derive(Debug, Clone, Deserialize)]
struct HfLfs {
    sha256: String,
    size: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct HfCardData {
    license: Option<serde_json::Value>,
    license_link: Option<String>,
}

pub async fn resolve_reference(reference: &str) -> Result<ResolvedModelReference, String> {
    let client = build_http_client()?;
    Ok(resolve_reference_with_client(&client, reference)
        .await?
        .public)
}

fn validate_expected_digest(
    resolved: &ResolvedModelReference,
    expected_sha256: &str,
) -> Result<String, String> {
    let expected_sha256 = normalize_sha256(expected_sha256, "expectedSha256")?;
    if !constant_time_eq(resolved.sha256.as_bytes(), expected_sha256.as_bytes()) {
        return Err(format!(
            "Model source changed since resolution: expected SHA-256 {expected_sha256}, but the source now resolves to {}",
            resolved.sha256
        ));
    }

    Ok(expected_sha256)
}

/// Re-resolves `reference`, compares its immutable digest to
/// `expected_sha256`, resumes/downloads into `models_dir`, verifies the whole
/// file plus GGUF magic, then publishes the model and provenance while
/// holding a cross-process destination lock. If a process is killed between
/// those two renames, the next pull re-hashes the orphan and reconstructs its
/// sidecar instead of downloading a duplicate multi-gigabyte file.
pub async fn install_reference<F>(
    models_dir: &Path,
    reference: &str,
    expected_sha256: &str,
    mut on_progress: F,
) -> Result<InstalledModelReference, String>
where
    F: FnMut(ModelDownloadProgress),
{
    let client = build_http_client()?;
    let resolution = resolve_reference_with_client(&client, reference).await?;
    let expected_sha256 = validate_expected_digest(&resolution.public, expected_sha256)?;

    let install_mutex = INSTALL_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()));
    let _process_guard = install_mutex.lock().await;

    let models_dir = canonical_models_dir(models_dir)?;
    let mut disambiguated = false;
    let (destination_file_name, destination, _install_lock) = loop {
        let destination_file_name = local_file_name(&resolution.public, disambiguated);
        let destination = models_dir.join(&destination_file_name);
        validate_direct_child(&models_dir, &destination)?;
        let install_lock = acquire_destination_lock(&models_dir, &destination).await?;

        if path_entry_is_missing(&destination)? {
            remove_stale_provenance_for_missing_model(&destination)?;
            break (destination_file_name, destination, install_lock);
        }
        let existing_provenance = match reusable_existing_install(&destination, &resolution.public)?
        {
            Some(provenance) => Some(provenance),
            None => recover_orphaned_install(
                &destination,
                &resolution.public,
                reference,
                &destination_file_name,
            )?,
        };
        if let Some(provenance) = existing_provenance {
            on_progress(ModelDownloadProgress {
                file: destination_file_name,
                downloaded: resolution.public.size_bytes,
                total: resolution.public.size_bytes,
            });
            let mut installed_resolution = resolution.public;
            installed_resolution.tool_calling = provenance.tool_calling;
            return Ok(InstalledModelReference {
                resolved: installed_resolution,
                provenance,
                local_path: destination,
            });
        }
        if !disambiguated {
            disambiguated = true;
            continue;
        }
        return Err(format!(
            "A different file already occupies the managed model destination {}",
            destination.display()
        ));
    };

    let partial = append_file_suffix(&destination, ".part")?;
    validate_direct_child(&models_dir, &partial)?;
    prepare_partial_file(&partial, resolution.public.size_bytes)?;

    let download_result = download_resumable(
        &client,
        &resolution,
        &partial,
        &destination_file_name,
        &mut on_progress,
    )
    .await;
    if let Err(error) = download_result {
        return Err(error);
    }

    let actual_sha256 = sha256_file_async(partial.clone()).await?;
    if !constant_time_eq(actual_sha256.as_bytes(), expected_sha256.as_bytes()) {
        let _ = fs::remove_file(&partial);
        return Err(format!(
            "Downloaded model failed SHA-256 verification: expected {expected_sha256}, got {actual_sha256}"
        ));
    }
    if let Err(error) = validate_local_gguf(&partial, resolution.public.size_bytes) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    let tool_calling = match embedded_tool_calling(&partial) {
        Ok(tool_calling) => tool_calling,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            return Err(error);
        }
    };
    if destination.exists() {
        return Err(format!(
            "Managed model destination appeared during install: {}",
            destination.display()
        ));
    }
    fs::rename(&partial, &destination).map_err(|error| {
        format!(
            "Verified model could not be published at {}: {error}",
            destination.display()
        )
    })?;

    let provenance = provenance_for(
        &resolution.public,
        reference.trim().to_string(),
        destination_file_name,
        now_ms()?,
        tool_calling,
    );
    if let Err(error) = save_provenance(&destination, &provenance) {
        let _ = fs::remove_file(&destination);
        return Err(error);
    }

    let mut installed_resolution = resolution.public;
    installed_resolution.tool_calling = tool_calling;
    Ok(InstalledModelReference {
        resolved: installed_resolution,
        provenance,
        local_path: destination,
    })
}

/// Reads and validates the app-owned sidecar for `model_path`.
///
/// A missing sidecar is `Ok(None)`. A corrupt or mismatched sidecar is an
/// error so callers can fail closed instead of inventing capabilities.
pub fn load_provenance(model_path: &Path) -> Result<Option<ManagedModelProvenance>, String> {
    let sidecar = provenance_path(model_path)?;
    let metadata = match fs::symlink_metadata(&sidecar) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to inspect model provenance {}: {error}",
                sidecar.display()
            ))
        }
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_PROVENANCE_BYTES {
        return Err(format!(
            "Model provenance {} is not a bounded regular file",
            sidecar.display()
        ));
    }
    let bytes = fs::read(&sidecar).map_err(|error| {
        format!(
            "Failed to read model provenance {}: {error}",
            sidecar.display()
        )
    })?;
    let provenance: ManagedModelProvenance = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "Failed to parse model provenance {}: {error}",
            sidecar.display()
        )
    })?;
    validate_provenance(model_path, &provenance)?;
    Ok(Some(provenance))
}

/// Revalidates a managed model immediately before execution. Listing models
/// avoids hashing multi-gigabyte payloads on every refresh, but the runtime
/// boundary never trusts size/header/provenance alone.
pub fn verify_managed_model_for_runtime(model_path: &Path) -> Result<(), String> {
    let Some(provenance) = load_provenance(model_path)? else {
        return Ok(());
    };
    let actual_sha256 = sha256_file(model_path)?;
    if !constant_time_eq(actual_sha256.as_bytes(), provenance.sha256.as_bytes()) {
        return Err(format!(
            "Managed model {} failed SHA-256 verification; reinstall it before running",
            model_path.display()
        ));
    }
    Ok(())
}

/// Finds a previously verified managed install without contacting its
/// registry. This keeps `monkey run <reference>` usable offline after the
/// first successful pull. The whole GGUF digest is checked again before the
/// path is returned.
pub fn find_installed_reference(
    models_dir: &Path,
    reference: &str,
) -> Result<Option<InstalledModelReference>, String> {
    let models_dir = canonical_models_dir(models_dir)?;
    let requested = reference.trim();
    if requested.is_empty() {
        return Ok(None);
    }
    let normalized = match parse_model_reference(requested) {
        Ok(ParsedModelReference::Ollama(reference)) => Some(format!(
            "ollama:{}/{}:{}",
            reference.namespace, reference.model, reference.tag
        )),
        _ => None,
    };

    let entries = fs::read_dir(&models_dir).map_err(|error| {
        format!(
            "Failed to list managed models directory {}: {error}",
            models_dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "Failed to inspect managed models directory {}: {error}",
                models_dir.display()
            )
        })?;
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            _ => continue,
        };
        let is_gguf = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"));
        if !is_gguf || metadata.len() == 0 {
            continue;
        }
        let provenance = match load_provenance(&path) {
            Ok(Some(provenance)) => provenance,
            Ok(None) | Err(_) => continue,
        };
        let matches = provenance.requested_reference == requested
            || provenance.canonical_reference == requested
            || normalized
                .as_deref()
                .is_some_and(|value| value == provenance.canonical_reference);
        if !matches {
            continue;
        }
        let actual_sha256 = sha256_file(&path)?;
        if !constant_time_eq(actual_sha256.as_bytes(), provenance.sha256.as_bytes()) {
            return Err(format!(
                "Installed model {} failed SHA-256 verification; reinstall it before running",
                path.display()
            ));
        }
        let resolved = ResolvedModelReference {
            source: provenance.source,
            canonical_reference: provenance.canonical_reference.clone(),
            display_name: provenance.display_name.clone(),
            repo: provenance.repo.clone(),
            revision: provenance.revision.clone(),
            file_name: provenance.source_file_name.clone(),
            download_url: provenance.download_url.clone(),
            sha256: provenance.sha256.clone(),
            size_bytes: provenance.size_bytes,
            tool_calling: provenance.tool_calling,
            license_name: provenance.license_name.clone(),
            license_url: provenance.license_url.clone(),
        };
        return Ok(Some(InstalledModelReference {
            resolved,
            provenance,
            local_path: path,
        }));
    }
    Ok(None)
}

pub fn provenance_path(model_path: &Path) -> Result<PathBuf, String> {
    append_file_suffix(model_path, ".metadata.json")
}

fn parse_model_reference(reference: &str) -> Result<ParsedModelReference, String> {
    let reference = reference.trim();
    if reference.is_empty() || reference.len() > MAX_REFERENCE_BYTES {
        return Err("Model reference is empty or too long".to_string());
    }
    if reference.chars().any(char::is_control) {
        return Err("Model reference contains control characters".to_string());
    }

    if reference.starts_with("hf:")
        || reference.starts_with("hf.co/")
        || reference.starts_with("huggingface.co/")
        || reference.starts_with("https://hf.co/")
        || reference.starts_with("https://huggingface.co/")
    {
        parse_hugging_face_reference(reference).map(ParsedModelReference::HuggingFace)
    } else if let Some(value) = reference.strip_prefix("ollama:") {
        parse_ollama_reference(value).map(ParsedModelReference::Ollama)
    } else if reference.contains("://") {
        Err(
            "Only public Ollama tags and hf:/hf.co Hugging Face references are supported"
                .to_string(),
        )
    } else {
        parse_ollama_reference(reference).map(ParsedModelReference::Ollama)
    }
}

fn parse_ollama_reference(value: &str) -> Result<OllamaReference, String> {
    let value = value.trim();
    if value.is_empty() || value.contains('@') || value.contains('#') || value.contains('\\') {
        return Err("Invalid Ollama model reference".to_string());
    }
    let (path, tag) = match value.rsplit_once(':') {
        Some((path, tag)) => (path, tag),
        None => (value, "latest"),
    };
    if path.contains(':') {
        return Err("Ollama references may contain only one tag separator ':'".to_string());
    }
    let components = path.split('/').collect::<Vec<_>>();
    let (namespace, model) = match components.as_slice() {
        [model] => ("library", *model),
        [namespace, model] => (*namespace, *model),
        _ => {
            return Err(
                "Ollama reference must be '<model>[:tag]' or '<namespace>/<model>[:tag]'"
                    .to_string(),
            )
        }
    };
    validate_registry_identifier(namespace, "Ollama namespace")?;
    validate_registry_identifier(model, "Ollama model")?;
    validate_registry_identifier(tag, "Ollama tag")?;
    Ok(OllamaReference {
        namespace: namespace.to_string(),
        model: model.to_string(),
        tag: tag.to_string(),
    })
}

fn parse_hugging_face_reference(value: &str) -> Result<HuggingFaceReference, String> {
    let (mut path_and_revision, mut selector_value) = if value.starts_with("https://") {
        let parsed =
            Url::parse(value).map_err(|error| format!("Invalid Hugging Face URL: {error}"))?;
        if parsed.scheme() != "https"
            || parsed.port().is_some()
            || !matches!(
                parsed.host_str(),
                Some(host) if host.eq_ignore_ascii_case("hf.co")
                    || host.eq_ignore_ascii_case("huggingface.co")
            )
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
        {
            return Err("Hugging Face URL must use https://hf.co or https://huggingface.co with no credentials, port, or query".to_string());
        }
        (
            parsed.path().trim_matches('/').to_string(),
            parsed.fragment().map(str::to_string),
        )
    } else {
        let stripped = value
            .strip_prefix("hf:")
            .or_else(|| value.strip_prefix("hf.co/"))
            .or_else(|| value.strip_prefix("huggingface.co/"))
            .unwrap_or(value);
        let (path, fragment) = stripped
            .split_once('#')
            .map(|(path, fragment)| (path, Some(fragment.to_string())))
            .unwrap_or((stripped, None));
        (path.trim_matches('/').to_string(), fragment)
    };

    if path_and_revision.contains('%')
        || selector_value
            .as_deref()
            .is_some_and(|value| value.contains('%'))
    {
        return Err(
            "Percent-encoded Hugging Face references are not accepted; use the literal repo, revision, and filename"
                .to_string(),
        );
    }

    // Match the compact reference form Hugging Face documents for Ollama
    // interoperability (`hf.co/<owner>/<repo>:<quantization>`), while still
    // accepting the explicit `#file=` / `#quant=` selectors used by this
    // app. Resolution always turns the requested revision (including the
    // default `main`) into an immutable commit SHA before install.
    if selector_value.is_none() {
        let inline_selector = path_and_revision
            .rsplit_once(':')
            .map(|(path, selector)| (path.to_string(), selector.to_string()));
        if let Some((path, selector)) = inline_selector {
            if !selector.trim().is_empty() {
                path_and_revision = path;
                selector_value = Some(selector);
            }
        }
    }

    let (repo, requested_revision) = path_and_revision
        .rsplit_once('@')
        .map(|(repo, revision)| (repo, revision))
        .unwrap_or((path_and_revision.as_str(), "main"));
    validate_hf_repo(repo)?;
    validate_hf_revision(requested_revision)?;

    let selector = match selector_value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        None => HuggingFaceSelector::DefaultQ4Km,
        Some(value) if value.len() > 1024 => {
            return Err("Hugging Face file/quantization selector is too long".to_string())
        }
        Some(value) => {
            if let Some(file) = value.strip_prefix("file=") {
                validate_hf_file_path(file)?;
                HuggingFaceSelector::File(file.to_string())
            } else if let Some(quantization) = value.strip_prefix("quant=") {
                validate_quantization(quantization)?;
                HuggingFaceSelector::Quantization(quantization.to_string())
            } else if value.to_ascii_lowercase().ends_with(".gguf") {
                validate_hf_file_path(value)?;
                HuggingFaceSelector::File(value.to_string())
            } else {
                validate_quantization(value)?;
                HuggingFaceSelector::Quantization(value.to_string())
            }
        }
    };

    Ok(HuggingFaceReference {
        repo: repo.to_string(),
        requested_revision: requested_revision.to_string(),
        selector,
    })
}

async fn resolve_reference_with_client(
    client: &Client,
    reference: &str,
) -> Result<InternalResolution, String> {
    match parse_model_reference(reference)? {
        ParsedModelReference::Ollama(reference) => resolve_ollama(client, &reference).await,
        ParsedModelReference::HuggingFace(reference) => {
            resolve_hugging_face(client, &reference).await
        }
    }
}

async fn resolve_ollama(
    client: &Client,
    reference: &OllamaReference,
) -> Result<InternalResolution, String> {
    let manifest_url = ollama_registry_url(
        &reference.namespace,
        &reference.model,
        "manifests",
        &reference.tag,
    )?;
    let (response, bearer_token) =
        send_ollama_registry_get(client, manifest_url.clone(), OCI_MANIFEST_MEDIA_TYPE, None)
            .await?;
    let response = require_success(response, "Ollama manifest", Some(StatusCode::NOT_FOUND))?;
    let bytes = response_bytes_bounded(response, MAX_MANIFEST_BYTES, "Ollama manifest").await?;
    let manifest: OciManifest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Ollama registry returned an invalid manifest: {error}"))?;
    let model_layer = select_ollama_model_layer(&manifest)?;
    let sha256 = normalize_sha256(&model_layer.digest, "Ollama model layer digest")?;
    validate_model_size(model_layer.size)?;
    let blob_url = ollama_registry_url(
        &reference.namespace,
        &reference.model,
        "blobs",
        &model_layer.digest,
    )?;
    probe_remote_gguf(client, blob_url.clone(), bearer_token.as_deref()).await?;

    let license_layers = manifest
        .layers
        .iter()
        .filter(|layer| layer.media_type == OLLAMA_LICENSE_MEDIA_TYPE)
        .collect::<Vec<_>>();
    let (license_name, license_url) = if license_layers.len() == 1
        && license_layers[0].size > 0
        && license_layers[0].size <= MAX_LICENSE_BYTES
    {
        let license = fetch_verified_ollama_text_blob(
            client,
            reference,
            license_layers[0],
            bearer_token.as_deref(),
            MAX_LICENSE_BYTES,
            "Ollama license",
        )
        .await?;
        let url = ollama_registry_url(
            &reference.namespace,
            &reference.model,
            "blobs",
            &license_layers[0].digest,
        )?;
        (license_title(&license), Some(url.to_string()))
    } else {
        (None, None)
    };

    let repo = format!("{}/{}", reference.namespace, reference.model);
    let display_name = format!("{repo}:{}", reference.tag);
    Ok(InternalResolution {
        public: ResolvedModelReference {
            source: ModelReferenceSource::OllamaRegistry,
            canonical_reference: format!("ollama:{display_name}"),
            display_name: display_name.clone(),
            repo,
            revision: reference.tag.clone(),
            file_name: format!("{}-{}.gguf", reference.model, reference.tag),
            download_url: blob_url.to_string(),
            sha256,
            size_bytes: model_layer.size,
            // Ollama's separate template layer uses Go-template syntax and
            // cannot be passed to llama.cpp's Jinja renderer. Capability is
            // determined from the downloaded GGUF's embedded template.
            tool_calling: false,
            license_name,
            license_url,
        },
        bearer_token,
    })
}

async fn resolve_hugging_face(
    client: &Client,
    reference: &HuggingFaceReference,
) -> Result<InternalResolution, String> {
    let metadata_url = hugging_face_metadata_url(reference)?;
    let response = client
        .get(metadata_url.clone())
        .header(ACCEPT, "application/json")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| format!("Failed to reach Hugging Face: {error}"))?;
    let status = response.status();
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        return Err(
            "Hugging Face model is private or gated; this public-model installer does not accept credentials"
                .to_string(),
        );
    }
    if status == StatusCode::NOT_FOUND {
        return Err(format!(
            "Hugging Face repo '{}' or revision '{}' was not found",
            reference.repo, reference.requested_revision
        ));
    }
    let response = require_success(response, "Hugging Face metadata", None)?;
    let bytes =
        response_bytes_bounded(response, MAX_HF_METADATA_BYTES, "Hugging Face metadata").await?;
    let metadata: HfModelMetadata = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Hugging Face returned invalid model metadata: {error}"))?;
    if metadata.private || hf_gated(&metadata.gated) {
        return Err(
            "Hugging Face model is private or gated; only public, ungated GGUF files are supported"
                .to_string(),
        );
    }
    if let Some(id) = metadata.id.as_deref() {
        if id != reference.repo {
            return Err(format!(
                "Hugging Face metadata repo mismatch: requested '{}', received '{id}'",
                reference.repo
            ));
        }
    }
    let revision = metadata
        .sha
        .as_deref()
        .ok_or("Hugging Face metadata did not include an immutable revision SHA")?;
    validate_hf_commit_sha(revision)?;
    let sibling = select_hf_sibling(&metadata, &reference.selector)?;
    let lfs = sibling.lfs.as_ref().ok_or_else(|| {
        format!(
            "Hugging Face file '{}' has no LFS SHA-256 metadata and cannot be verified",
            sibling.rfilename
        )
    })?;
    let sha256 = normalize_sha256(&lfs.sha256, "Hugging Face LFS SHA-256")?;
    validate_model_size(lfs.size)?;
    if let Some(size) = sibling.size {
        if size != lfs.size {
            return Err(format!(
                "Hugging Face metadata size mismatch for '{}': sibling says {size}, LFS says {}",
                sibling.rfilename, lfs.size
            ));
        }
    }

    let download_url =
        hugging_face_file_url(&reference.repo, revision, &sibling.rfilename, "resolve")?;
    probe_remote_gguf(client, download_url.clone(), None).await?;
    let (license_name, license_url) = hugging_face_license(&metadata, &reference.repo, revision)?;
    let repo_name = reference.repo.rsplit('/').next().unwrap_or(&reference.repo);
    let display_name = format!(
        "{} ({})",
        repo_name,
        sibling
            .rfilename
            .rsplit('/')
            .next()
            .unwrap_or(&sibling.rfilename)
    );
    let selector = format!("#{}", sibling.rfilename);

    Ok(InternalResolution {
        public: ResolvedModelReference {
            source: ModelReferenceSource::HuggingFace,
            canonical_reference: format!("hf:{}@{}{}", reference.repo, revision, selector),
            display_name,
            repo: reference.repo.clone(),
            revision: revision.to_string(),
            file_name: sibling.rfilename.clone(),
            download_url: download_url.to_string(),
            sha256,
            size_bytes: lfs.size,
            // A remote filename/repo name never proves tool-call support.
            tool_calling: false,
            license_name,
            license_url,
        },
        bearer_token: None,
    })
}

fn select_ollama_model_layer(manifest: &OciManifest) -> Result<&OciLayer, String> {
    if manifest.schema_version != 2 {
        return Err(format!(
            "Unsupported Ollama manifest schema version {}",
            manifest.schema_version
        ));
    }
    if manifest.media_type.as_deref().is_some_and(|media_type| {
        media_type != OCI_MANIFEST_MEDIA_TYPE
            && media_type != "application/vnd.oci.image.manifest.v1+json"
    }) {
        return Err("Unsupported Ollama manifest media type".to_string());
    }
    if manifest.layers.iter().any(|layer| {
        matches!(
            layer.media_type.as_str(),
            OLLAMA_ADAPTER_MEDIA_TYPE | OLLAMA_PROJECTOR_MEDIA_TYPE
        )
    }) {
        return Err(
            "Ollama tag requires an adapter or projector blob; only standalone one-file GGUF models are supported"
                .to_string(),
        );
    }
    let layers = manifest
        .layers
        .iter()
        .filter(|layer| layer.media_type == OLLAMA_MODEL_MEDIA_TYPE)
        .collect::<Vec<_>>();
    match layers.as_slice() {
        [layer] => {
            normalize_sha256(&layer.digest, "Ollama model layer digest")?;
            validate_model_size(layer.size)?;
            Ok(layer)
        }
        [] => Err(
            "Ollama tag does not contain a local GGUF model layer (cloud-only and non-GGUF tags are unsupported)"
                .to_string(),
        ),
        _ => Err(
            "Ollama tag contains multiple model layers; only one-file GGUF models are supported"
                .to_string(),
        ),
    }
}

fn select_hf_sibling<'a>(
    metadata: &'a HfModelMetadata,
    selector: &HuggingFaceSelector,
) -> Result<&'a HfSibling, String> {
    match selector {
        HuggingFaceSelector::File(file_name) => {
            let matches = metadata
                .siblings
                .iter()
                .filter(|sibling| sibling.rfilename == *file_name)
                .collect::<Vec<_>>();
            let sibling = match matches.as_slice() {
                [sibling] => *sibling,
                [] => {
                    return Err(format!(
                        "Hugging Face repo does not contain the requested file '{file_name}'"
                    ))
                }
                _ => {
                    return Err(format!(
                        "Hugging Face metadata contains duplicate entries for '{file_name}'"
                    ))
                }
            };
            validate_selected_hf_sibling(sibling)?;
            Ok(sibling)
        }
        HuggingFaceSelector::Quantization(quantization) => {
            select_unique_hf_quantization(metadata, quantization)
        }
        HuggingFaceSelector::DefaultQ4Km => select_unique_hf_quantization(metadata, "Q4_K_M")
            .map_err(|error| {
                format!(
                    "{error}. Specify an exact '#filename.gguf' or '#quant=<quantization>' selector"
                )
            }),
    }
}

fn select_unique_hf_quantization<'a>(
    metadata: &'a HfModelMetadata,
    quantization: &str,
) -> Result<&'a HfSibling, String> {
    validate_quantization(quantization)?;
    let matches = metadata
        .siblings
        .iter()
        .filter(|sibling| {
            is_gguf_file(&sibling.rfilename)
                && !is_sharded_gguf(&sibling.rfilename)
                && filename_has_quantization(&sibling.rfilename, quantization)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [sibling] => {
            validate_selected_hf_sibling(sibling)?;
            Ok(sibling)
        }
        [] => Err(format!(
            "Hugging Face repo has no single-file GGUF matching quantization '{quantization}'"
        )),
        _ => {
            let names = matches
                .iter()
                .take(8)
                .map(|sibling| sibling.rfilename.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            Err(format!(
                "Hugging Face quantization '{quantization}' is ambiguous across {} files: {names}",
                matches.len()
            ))
        }
    }
}

fn validate_selected_hf_sibling(sibling: &HfSibling) -> Result<(), String> {
    validate_hf_file_path(&sibling.rfilename)?;
    if !is_gguf_file(&sibling.rfilename) {
        return Err(format!(
            "Hugging Face file '{}' is not a GGUF file",
            sibling.rfilename
        ));
    }
    if is_sharded_gguf(&sibling.rfilename) {
        return Err(format!(
            "Hugging Face file '{}' is one shard of a split GGUF; only one-file GGUF models are supported",
            sibling.rfilename
        ));
    }
    let lfs = sibling.lfs.as_ref().ok_or_else(|| {
        format!(
            "Hugging Face file '{}' is not backed by verifiable LFS metadata",
            sibling.rfilename
        )
    })?;
    normalize_sha256(&lfs.sha256, "Hugging Face LFS SHA-256")?;
    validate_model_size(lfs.size)
}

fn hf_gated(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Bool(value) => *value,
        serde_json::Value::String(value) => {
            !value.trim().is_empty() && !value.eq_ignore_ascii_case("false")
        }
        serde_json::Value::Null => false,
        _ => true,
    }
}

fn license_title(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(160).collect())
}

fn hugging_face_license(
    metadata: &HfModelMetadata,
    repo: &str,
    revision: &str,
) -> Result<(Option<String>, Option<String>), String> {
    let card_data = metadata.card_data.as_ref().cloned().unwrap_or_default();
    let license_name = match card_data.license {
        Some(serde_json::Value::String(value))
            if !value.trim().is_empty() && value.len() <= 1024 =>
        {
            Some(value)
        }
        _ => None,
    };
    if let Some(link) = card_data.license_link {
        if let Ok(parsed) = Url::parse(&link) {
            if validate_public_https_url(&parsed).is_ok() {
                return Ok((license_name, Some(parsed.to_string())));
            }
        }
    }
    let license_file = metadata.siblings.iter().find(|sibling| {
        sibling.rfilename.rsplit('/').next().is_some_and(|name| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "license" | "license.md" | "license.txt"
            )
        })
    });
    let license_url = license_file
        .map(|sibling| hugging_face_file_url(repo, revision, &sibling.rfilename, "blob"))
        .transpose()?
        .map(|url| url.to_string());
    Ok((license_name, license_url))
}

async fn fetch_verified_ollama_text_blob(
    client: &Client,
    reference: &OllamaReference,
    layer: &OciLayer,
    bearer_token: Option<&str>,
    max_bytes: u64,
    label: &str,
) -> Result<String, String> {
    if layer.size == 0 || layer.size > max_bytes {
        return Err(format!(
            "{label} is outside the bounded metadata size limit"
        ));
    }
    let expected_sha256 = normalize_sha256(&layer.digest, label)?;
    let url = ollama_registry_url(
        &reference.namespace,
        &reference.model,
        "blobs",
        &layer.digest,
    )?;
    let response = send_get(client, url, bearer_token, None).await?;
    let response = require_success(response, label, None)?;
    let bytes = response_bytes_bounded(response, max_bytes as usize, label).await?;
    if bytes.len() as u64 != layer.size {
        return Err(format!(
            "{label} size mismatch: manifest says {}, response contained {}",
            layer.size,
            bytes.len()
        ));
    }
    let actual = format!("{:x}", Sha256::digest(&bytes));
    if !constant_time_eq(actual.as_bytes(), expected_sha256.as_bytes()) {
        return Err(format!("{label} failed SHA-256 verification"));
    }
    String::from_utf8(bytes).map_err(|_| format!("{label} is not valid UTF-8"))
}

async fn send_ollama_registry_get(
    client: &Client,
    url: Url,
    accept: &str,
    bearer_token: Option<&str>,
) -> Result<(Response, Option<String>), String> {
    let mut request = client
        .get(url.clone())
        .header(ACCEPT, accept)
        .header(ACCEPT_ENCODING, "identity");
    if let Some(token) = bearer_token {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Failed to reach Ollama registry: {error}"))?;
    if response.status() != StatusCode::UNAUTHORIZED {
        return Ok((response, bearer_token.map(str::to_string)));
    }
    let challenge = response
        .headers()
        .get(WWW_AUTHENTICATE)
        .ok_or("Ollama registry requested authentication without a Bearer challenge")?
        .to_str()
        .map_err(|_| "Ollama registry Bearer challenge was not valid ASCII")?;
    let challenge = parse_registry_auth_challenge(challenge)?;
    let token = fetch_registry_token(client, &challenge).await?;
    let response = client
        .get(url)
        .header(ACCEPT, accept)
        .header(ACCEPT_ENCODING, "identity")
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|error| format!("Failed to reach authenticated Ollama registry: {error}"))?;
    Ok((response, Some(token)))
}

async fn fetch_registry_token(
    client: &Client,
    challenge: &RegistryAuthChallenge,
) -> Result<String, String> {
    validate_ollama_auth_url(&challenge.realm)?;
    let mut url = challenge.realm.clone();
    {
        let mut query = url.query_pairs_mut();
        if let Some(service) = &challenge.service {
            query.append_pair("service", service);
        }
        if let Some(scope) = &challenge.scope {
            query.append_pair("scope", scope);
        }
    }
    let response = client
        .get(url)
        .header(ACCEPT, "application/json")
        .header(ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|error| format!("Failed to obtain Ollama registry token: {error}"))?;
    let response = require_success(response, "Ollama registry token", None)?;
    let bytes = response_bytes_bounded(response, 64 * 1024, "Ollama registry token").await?;
    let token: RegistryTokenResponse = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Ollama registry returned an invalid token: {error}"))?;
    let token = token
        .token
        .or(token.access_token)
        .ok_or("Ollama registry token response did not contain token or access_token")?;
    if token.is_empty() || token.len() > 16 * 1024 || token.chars().any(char::is_control) {
        return Err("Ollama registry returned an invalid bearer token".to_string());
    }
    Ok(token)
}

fn parse_registry_auth_challenge(value: &str) -> Result<RegistryAuthChallenge, String> {
    let (scheme, parameters) = value
        .split_once(' ')
        .ok_or("Malformed Ollama registry authentication challenge")?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return Err("Ollama registry requires unsupported authentication".to_string());
    }
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for parameter in parameters.split(',') {
        let (key, value) = parameter
            .trim()
            .split_once('=')
            .ok_or("Malformed Ollama registry Bearer parameter")?;
        let value = value.trim().trim_matches('"');
        if value.is_empty() || value.len() > 16 * 1024 || value.chars().any(char::is_control) {
            return Err("Invalid Ollama registry Bearer parameter".to_string());
        }
        match key.trim().to_ascii_lowercase().as_str() {
            "realm" => {
                let parsed = Url::parse(value)
                    .map_err(|error| format!("Invalid Ollama auth realm: {error}"))?;
                realm = Some(parsed);
            }
            "service" => service = Some(value.to_string()),
            "scope" => scope = Some(value.to_string()),
            _ => {}
        }
    }
    let challenge = RegistryAuthChallenge {
        realm: realm.ok_or("Ollama registry Bearer challenge omitted its realm")?,
        service,
        scope,
    };
    validate_ollama_auth_url(&challenge.realm)?;
    Ok(challenge)
}

async fn probe_remote_gguf(
    client: &Client,
    url: Url,
    bearer_token: Option<&str>,
) -> Result<(), String> {
    validate_public_https_url(&url)?;
    let response = send_get(client, url, bearer_token, Some("bytes=0-3")).await?;
    if !matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        return Err(format!(
            "GGUF header probe failed with HTTP {}",
            response.status()
        ));
    }
    validate_public_https_url(response.url())?;
    let mut stream = response.bytes_stream();
    let mut magic = Vec::with_capacity(GGUF_MAGIC.len());
    while magic.len() < GGUF_MAGIC.len() {
        let chunk = stream
            .next()
            .await
            .ok_or("GGUF header probe returned fewer than four bytes")?
            .map_err(|error| format!("GGUF header probe failed: {error}"))?;
        let remaining = GGUF_MAGIC.len() - magic.len();
        magic.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    if magic.as_slice() != GGUF_MAGIC {
        return Err("Resolved artifact does not have a GGUF file header".to_string());
    }
    Ok(())
}

async fn send_get(
    client: &Client,
    url: Url,
    bearer_token: Option<&str>,
    range: Option<&str>,
) -> Result<Response, String> {
    validate_public_https_url(&url)?;
    let mut request = client.get(url).header(ACCEPT_ENCODING, "identity");
    if let Some(token) = bearer_token {
        request = request.header(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| "Invalid bearer token header")?,
        );
    }
    if let Some(range) = range {
        request = request.header(RANGE, range);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("Model download request failed: {error}"))?;
    validate_public_https_url(response.url())?;
    Ok(response)
}

async fn download_resumable<F>(
    client: &Client,
    resolution: &InternalResolution,
    partial: &Path,
    local_file_name: &str,
    on_progress: &mut F,
) -> Result<(), String>
where
    F: FnMut(ModelDownloadProgress),
{
    let total = resolution.public.size_bytes;
    let mut offset = fs::metadata(partial)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    on_progress(ModelDownloadProgress {
        file: local_file_name.to_string(),
        downloaded: offset,
        total,
    });

    if offset == total {
        return Ok(());
    }
    let download_url = Url::parse(&resolution.public.download_url)
        .map_err(|error| format!("Resolved model URL is invalid: {error}"))?;
    validate_public_https_url(&download_url)?;
    let range_header = (offset > 0).then(|| format!("bytes={offset}-"));
    let response = send_get(
        client,
        download_url,
        resolution.bearer_token.as_deref(),
        range_header.as_deref(),
    )
    .await?;

    if offset > 0 && response.status() == StatusCode::RANGE_NOT_SATISFIABLE && offset == total {
        return Ok(());
    }
    if offset > 0 && response.status() == StatusCode::OK {
        offset = 0;
        fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(partial)
            .map_err(|error| {
                format!(
                    "Failed to restart model download {}: {error}",
                    partial.display()
                )
            })?;
    } else if offset > 0 {
        if response.status() != StatusCode::PARTIAL_CONTENT {
            return Err(format!(
                "Model source rejected resume request with HTTP {}",
                response.status()
            ));
        }
        validate_content_range(response.headers(), offset, total)?;
    } else if !matches!(
        response.status(),
        StatusCode::OK | StatusCode::PARTIAL_CONTENT
    ) {
        return Err(format!(
            "Model download failed with HTTP {}",
            response.status()
        ));
    } else if response.status() == StatusCode::PARTIAL_CONTENT {
        validate_content_range(response.headers(), 0, total)?;
    }

    validate_response_length(response.headers(), offset, total)?;
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .append(true)
        .open(partial)
        .await
        .map_err(|error| {
            format!(
                "Failed to open partial model {}: {error}",
                partial.display()
            )
        })?;
    let mut downloaded = offset;
    let mut last_emit = std::time::Instant::now();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("Model download stream failed: {error}"))?;
        downloaded = downloaded
            .checked_add(chunk.len() as u64)
            .ok_or("Model download byte count overflow")?;
        if downloaded > total {
            return Err(format!(
                "Model source sent more bytes than the declared size {total}"
            ));
        }
        output.write_all(&chunk).await.map_err(|error| {
            format!(
                "Failed to write partial model {}: {error}",
                partial.display()
            )
        })?;
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            on_progress(ModelDownloadProgress {
                file: local_file_name.to_string(),
                downloaded,
                total,
            });
            last_emit = std::time::Instant::now();
        }
    }
    output.flush().await.map_err(|error| {
        format!(
            "Failed to flush partial model {}: {error}",
            partial.display()
        )
    })?;
    output.sync_all().await.map_err(|error| {
        format!(
            "Failed to sync partial model {}: {error}",
            partial.display()
        )
    })?;
    if downloaded != total {
        return Err(format!(
            "Model download ended at {downloaded} bytes, expected {total}; retry to resume"
        ));
    }
    on_progress(ModelDownloadProgress {
        file: local_file_name.to_string(),
        downloaded,
        total,
    });
    Ok(())
}

fn validate_content_range(
    headers: &HeaderMap,
    expected_offset: u64,
    total: u64,
) -> Result<(), String> {
    let value = headers
        .get(CONTENT_RANGE)
        .ok_or("Resume response omitted Content-Range")?
        .to_str()
        .map_err(|_| "Resume Content-Range is not valid ASCII")?;
    let value = value
        .strip_prefix("bytes ")
        .ok_or("Resume Content-Range must use byte units")?;
    let (range, declared_total) = value
        .split_once('/')
        .ok_or("Resume Content-Range is malformed")?;
    let (start, end) = range
        .split_once('-')
        .ok_or("Resume Content-Range is malformed")?;
    let start = start
        .parse::<u64>()
        .map_err(|_| "Resume Content-Range start is invalid")?;
    let end = end
        .parse::<u64>()
        .map_err(|_| "Resume Content-Range end is invalid")?;
    let declared_total = declared_total
        .parse::<u64>()
        .map_err(|_| "Resume Content-Range total is invalid")?;
    if start != expected_offset || declared_total != total || end < start || end >= total {
        return Err(format!(
            "Resume Content-Range '{value}' does not match offset {expected_offset} and total {total}"
        ));
    }
    Ok(())
}

fn validate_response_length(headers: &HeaderMap, offset: u64, total: u64) -> Result<(), String> {
    let Some(value) = headers.get(CONTENT_LENGTH) else {
        return Ok(());
    };
    let length = value
        .to_str()
        .map_err(|_| "Download Content-Length is not valid ASCII")?
        .parse::<u64>()
        .map_err(|_| "Download Content-Length is invalid")?;
    let expected = total
        .checked_sub(offset)
        .ok_or("Download offset exceeds expected size")?;
    if length != expected {
        return Err(format!(
            "Download Content-Length {length} does not match expected remaining size {expected}"
        ));
    }
    Ok(())
}

fn prepare_partial_file(path: &Path, expected_size: u64) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(format!(
                    "Partial model path {} is not a regular file",
                    path.display()
                ));
            }
            if metadata.len() > expected_size {
                fs::remove_file(path).map_err(|error| {
                    format!(
                        "Failed to discard oversized partial model {}: {error}",
                        path.display()
                    )
                })?;
                create_private_file(path)?;
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => create_private_file(path)?,
        Err(error) => {
            return Err(format!(
                "Failed to inspect partial model {}: {error}",
                path.display()
            ))
        }
    }
    Ok(())
}

fn create_private_file(path: &Path) -> Result<(), String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map(|_| ())
        .map_err(|error| format!("Failed to create partial model {}: {error}", path.display()))
}

async fn acquire_install_lock(path: PathBuf) -> Result<CrossProcessFileLock, String> {
    tokio::task::spawn_blocking(move || acquire_cross_process_lock(&path))
        .await
        .map_err(|error| format!("Model install lock task failed: {error}"))?
}

async fn acquire_destination_lock(
    models_dir: &Path,
    destination: &Path,
) -> Result<CrossProcessFileLock, String> {
    let lock_path = append_file_suffix(destination, ".install.lock")?;
    validate_direct_child(models_dir, &lock_path)?;
    acquire_install_lock(lock_path).await
}

fn path_entry_is_missing(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!(
            "Failed to inspect managed model path {}: {error}",
            path.display()
        )),
    }
}

fn remove_provenance_entry_if_present(model_path: &Path) -> Result<(), String> {
    let sidecar = provenance_path(model_path)?;
    match fs::symlink_metadata(&sidecar) {
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(&sidecar).map_err(|error| {
                format!(
                    "Failed to remove model provenance {}: {error}",
                    sidecar.display()
                )
            })
        }
        Ok(_) => Err(format!(
            "Model provenance {} is not a removable file",
            sidecar.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Failed to inspect model provenance {}: {error}",
            sidecar.display()
        )),
    }
}

/// Cleans up the only unsafe half of an interrupted delete: metadata whose
/// model file is already gone. The caller must hold this destination's
/// install lock so an installer cannot publish the GGUF between the absence
/// check and sidecar removal.
fn remove_stale_provenance_for_missing_model(model_path: &Path) -> Result<(), String> {
    if !path_entry_is_missing(model_path)? {
        return Err(format!(
            "Refusing to remove provenance while model {} still exists",
            model_path.display()
        ));
    }
    remove_provenance_entry_if_present(model_path)
}

/// Deletes one app-owned managed model under the same per-destination lock
/// used by [`install_reference`]. Metadata is removed first: a crash between
/// the two removals leaves a verified GGUF without a sidecar, which the
/// installer can recover without downloading again. The inverse order could
/// leave a dangling sidecar that permanently blocks publication.
pub async fn delete_installed_model(models_dir: &Path, model_path: &Path) -> Result<(), String> {
    let models_dir = canonical_models_dir(models_dir)?;
    validate_direct_child(&models_dir, model_path)?;
    let _install_lock = acquire_destination_lock(&models_dir, model_path).await?;

    let metadata = match fs::symlink_metadata(model_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            remove_stale_provenance_for_missing_model(model_path)?;
            return Ok(());
        }
        Err(error) => {
            return Err(format!(
                "Failed to inspect managed model {}: {error}",
                model_path.display()
            ))
        }
    };
    if !metadata.file_type().is_file() {
        return Err(format!(
            "Managed model {} is not a regular file",
            model_path.display()
        ));
    }
    let canonical_model = model_path.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve managed model {}: {error}",
            model_path.display()
        )
    })?;
    if canonical_model != model_path || canonical_model.parent() != Some(models_dir.as_path()) {
        return Err(format!(
            "Managed model path {} changed or escapes its models directory",
            model_path.display()
        ));
    }

    remove_provenance_entry_if_present(model_path)?;
    fs::remove_file(model_path).map_err(|error| {
        format!(
            "Failed to delete managed model {}: {error}",
            model_path.display()
        )
    })
}

fn reusable_existing_install(
    path: &Path,
    resolved: &ResolvedModelReference,
) -> Result<Option<ManagedModelProvenance>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Failed to inspect existing model {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file()
        || metadata.len() != resolved.size_bytes
        || validate_local_gguf(path, resolved.size_bytes).is_err()
    {
        return Ok(None);
    }
    let Some(provenance) = load_provenance(path)? else {
        return Ok(None);
    };
    if provenance.canonical_reference != resolved.canonical_reference
        || !constant_time_eq(provenance.sha256.as_bytes(), resolved.sha256.as_bytes())
    {
        return Ok(None);
    }
    let actual = sha256_file(path)?;
    if !constant_time_eq(actual.as_bytes(), resolved.sha256.as_bytes()) {
        return Ok(None);
    }
    Ok(Some(provenance))
}

fn recover_orphaned_install(
    path: &Path,
    resolved: &ResolvedModelReference,
    requested_reference: &str,
    local_file_name: &str,
) -> Result<Option<ManagedModelProvenance>, String> {
    if provenance_path(path)?.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "Failed to inspect orphaned model {}: {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file()
        || metadata.len() != resolved.size_bytes
        || validate_local_gguf(path, resolved.size_bytes).is_err()
    {
        return Ok(None);
    }
    let actual = sha256_file(path)?;
    if !constant_time_eq(actual.as_bytes(), resolved.sha256.as_bytes()) {
        return Ok(None);
    }
    let tool_calling = embedded_tool_calling(path)?;
    let provenance = provenance_for(
        resolved,
        requested_reference.trim().to_string(),
        local_file_name.to_string(),
        now_ms()?,
        tool_calling,
    );
    save_provenance(path, &provenance)?;
    Ok(Some(provenance))
}

fn provenance_for(
    resolved: &ResolvedModelReference,
    requested_reference: String,
    local_file_name: String,
    installed_at_ms: u64,
    tool_calling: bool,
) -> ManagedModelProvenance {
    ManagedModelProvenance {
        schema_version: PROVENANCE_SCHEMA_VERSION,
        source: resolved.source,
        requested_reference,
        canonical_reference: resolved.canonical_reference.clone(),
        display_name: resolved.display_name.clone(),
        repo: resolved.repo.clone(),
        revision: resolved.revision.clone(),
        source_file_name: resolved.file_name.clone(),
        local_file_name,
        download_url: resolved.download_url.clone(),
        sha256: resolved.sha256.clone(),
        size_bytes: resolved.size_bytes,
        tool_calling,
        license_name: resolved.license_name.clone(),
        license_url: resolved.license_url.clone(),
        installed_at_ms,
    }
}

fn save_provenance(model_path: &Path, provenance: &ManagedModelProvenance) -> Result<(), String> {
    validate_provenance(model_path, provenance)?;
    let path = provenance_path(model_path)?;
    if path.exists() {
        return Err(format!(
            "Model provenance destination already exists: {}",
            path.display()
        ));
    }
    let bytes = serde_json::to_vec_pretty(provenance)
        .map_err(|error| format!("Failed to serialize model provenance: {error}"))?;
    if bytes.len() as u64 > MAX_PROVENANCE_BYTES {
        return Err("Model provenance exceeds its size limit".to_string());
    }
    let parent = path
        .parent()
        .ok_or("Model provenance path has no parent directory")?;
    let temporary = parent.join(format!(".provenance-{}.tmp", Uuid::new_v4().simple()));
    let write_result = (|| -> Result<(), String> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(|error| {
            format!(
                "Failed to stage model provenance {}: {error}",
                temporary.display()
            )
        })?;
        file.write_all(&bytes)
            .and_then(|_| file.sync_all())
            .map_err(|error| {
                format!(
                    "Failed to write model provenance {}: {error}",
                    temporary.display()
                )
            })?;
        fs::rename(&temporary, &path).map_err(|error| {
            format!(
                "Failed to publish model provenance {}: {error}",
                path.display()
            )
        })
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}

fn validate_provenance(
    model_path: &Path,
    provenance: &ManagedModelProvenance,
) -> Result<(), String> {
    if provenance.schema_version != PROVENANCE_SCHEMA_VERSION {
        return Err("Unsupported model provenance schema version".to_string());
    }
    let local_file_name = model_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Model path has no UTF-8 filename")?;
    if provenance.local_file_name != local_file_name {
        return Err("Model provenance local filename does not match its GGUF".to_string());
    }
    if !is_safe_component(&provenance.local_file_name) || !is_gguf_file(&provenance.local_file_name)
    {
        return Err("Model provenance contains an unsafe local filename".to_string());
    }
    if !provenance.requested_reference.is_empty() {
        validate_human_text(
            &provenance.requested_reference,
            "requestedReference",
            MAX_REFERENCE_BYTES,
        )?;
    }
    validate_human_text(
        &provenance.canonical_reference,
        "canonicalReference",
        MAX_REFERENCE_BYTES,
    )?;
    validate_human_text(&provenance.display_name, "displayName", 4096)?;
    validate_human_text(&provenance.repo, "repo", 4096)?;
    validate_human_text(&provenance.revision, "revision", 4096)?;
    validate_human_text(&provenance.source_file_name, "sourceFileName", 4096)?;
    let sha256 = normalize_sha256(&provenance.sha256, "provenance.sha256")?;
    if sha256 != provenance.sha256 {
        return Err("Model provenance SHA-256 is not canonical lowercase hex".to_string());
    }
    validate_model_size(provenance.size_bytes)?;
    let download_url = Url::parse(&provenance.download_url)
        .map_err(|error| format!("Invalid provenance download URL: {error}"))?;
    validate_public_https_url(&download_url)?;
    if let Some(url) = &provenance.license_url {
        let url =
            Url::parse(url).map_err(|error| format!("Invalid provenance license URL: {error}"))?;
        validate_public_https_url(&url)?;
    }
    if let Some(name) = &provenance.license_name {
        validate_human_text(name, "licenseName", 4096)?;
    }
    let metadata = fs::symlink_metadata(model_path)
        .map_err(|error| format!("Failed to inspect model {}: {error}", model_path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() != provenance.size_bytes {
        return Err("Model file does not match provenance size".to_string());
    }
    validate_local_gguf(model_path, provenance.size_bytes)?;
    let embedded_tool_calling = embedded_tool_calling(model_path)?;
    if provenance.tool_calling != embedded_tool_calling {
        return Err(
            "Model provenance tool-calling capability does not match its embedded GGUF template"
                .to_string(),
        );
    }
    Ok(())
}

fn canonical_models_dir(path: &Path) -> Result<PathBuf, String> {
    let canonical = path.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve models directory {}: {error}",
            path.display()
        )
    })?;
    let metadata = fs::symlink_metadata(&canonical).map_err(|error| {
        format!(
            "Failed to inspect models directory {}: {error}",
            canonical.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "Models path {} is not a real directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn validate_direct_child(parent: &Path, child: &Path) -> Result<(), String> {
    if child.parent() != Some(parent) {
        return Err(format!(
            "Managed model path {} escapes the models directory",
            child.display()
        ));
    }
    Ok(())
}

fn local_file_name(resolved: &ResolvedModelReference, disambiguated: bool) -> String {
    let source = match resolved.source {
        ModelReferenceSource::OllamaRegistry => "ollama",
        ModelReferenceSource::HuggingFace => "hf",
    };
    let repo_name = resolved.repo.rsplit('/').next().unwrap_or("model");
    let remote_name = resolved
        .file_name
        .rsplit('/')
        .next()
        .unwrap_or("model.gguf");
    let stem = remote_name
        .strip_suffix(".gguf")
        .or_else(|| remote_name.strip_suffix(".GGUF"))
        .unwrap_or(remote_name);
    let canonical_suffix = if disambiguated {
        let digest = format!(
            "{:x}",
            Sha256::digest(resolved.canonical_reference.as_bytes())
        );
        format!("-{}", &digest[..12])
    } else {
        String::new()
    };
    let raw = format!(
        "{source}-{}{canonical_suffix}-{repo_name}-{stem}.gguf",
        &resolved.sha256[..12],
    );
    sanitize_file_name(&raw)
}

fn sanitize_file_name(value: &str) -> String {
    let mut output = String::with_capacity(value.len().min(180));
    let mut previous_dash = false;
    for character in value.chars() {
        if output.len() >= 175 {
            break;
        }
        let accepted = if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        {
            character
        } else {
            '-'
        };
        if accepted == '-' && previous_dash {
            continue;
        }
        previous_dash = accepted == '-';
        output.push(accepted);
    }
    let trimmed = output.trim_matches(|character| character == '.' || character == '-');
    let mut output = if trimmed.is_empty() {
        "model".to_string()
    } else {
        trimmed.to_string()
    };
    if !output.to_ascii_lowercase().ends_with(".gguf") {
        output.push_str(".gguf");
    }
    output
}

fn append_file_suffix(path: &Path, suffix: &str) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("Managed model path has no UTF-8 filename")?;
    if !is_safe_component(file_name) {
        return Err("Managed model filename is unsafe".to_string());
    }
    Ok(path.with_file_name(format!("{file_name}{suffix}")))
}

fn hugging_face_metadata_url(reference: &HuggingFaceReference) -> Result<Url, String> {
    let mut url =
        Url::parse("https://huggingface.co/api/models/").map_err(|error| error.to_string())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "Hugging Face API URL cannot accept path segments")?;
        for component in reference.repo.split('/') {
            segments.push(component);
        }
        segments.push("revision");
        for component in reference.requested_revision.split('/') {
            segments.push(component);
        }
    }
    url.query_pairs_mut().append_pair("blobs", "true");
    validate_public_https_url(&url)?;
    Ok(url)
}

fn hugging_face_file_url(
    repo: &str,
    revision: &str,
    file_name: &str,
    operation: &str,
) -> Result<Url, String> {
    validate_hf_repo(repo)?;
    validate_hf_commit_sha(revision)?;
    validate_hf_file_path(file_name)?;
    if !matches!(operation, "resolve" | "blob") {
        return Err("Unsupported Hugging Face file URL operation".to_string());
    }
    let mut url = Url::parse("https://huggingface.co/").map_err(|error| error.to_string())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "Hugging Face file URL cannot accept path segments")?;
        for component in repo.split('/') {
            segments.push(component);
        }
        segments.push(operation);
        segments.push(revision);
        for component in file_name.split('/') {
            segments.push(component);
        }
    }
    if operation == "resolve" {
        url.query_pairs_mut().append_pair("download", "true");
    }
    validate_public_https_url(&url)?;
    Ok(url)
}

fn ollama_registry_url(
    namespace: &str,
    model: &str,
    operation: &str,
    value: &str,
) -> Result<Url, String> {
    validate_registry_identifier(namespace, "Ollama namespace")?;
    validate_registry_identifier(model, "Ollama model")?;
    if !matches!(operation, "manifests" | "blobs") {
        return Err("Unsupported Ollama registry operation".to_string());
    }
    if operation == "manifests" {
        validate_registry_identifier(value, "Ollama tag")?;
    } else {
        normalize_sha256(value, "Ollama blob digest")?;
    }
    let mut url = Url::parse(OLLAMA_REGISTRY_ORIGIN).map_err(|error| error.to_string())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "Ollama registry URL cannot accept path segments")?;
        segments.extend(["v2", namespace, model, operation, value]);
    }
    validate_public_https_url(&url)?;
    Ok(url)
}

fn validate_registry_identifier(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('.')
        || value.ends_with('.')
        || !value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
    {
        return Err(format!(
            "{field} must use 1-128 letters, digits, '.', '_', or '-'"
        ));
    }
    Ok(())
}

fn validate_hf_repo(repo: &str) -> Result<(), String> {
    let components = repo.split('/').collect::<Vec<_>>();
    if components.len() != 2 {
        return Err("Hugging Face repo must be '<owner>/<name>'".to_string());
    }
    for component in components {
        validate_registry_identifier(component, "Hugging Face repo component")?;
    }
    Ok(())
}

fn validate_hf_revision(revision: &str) -> Result<(), String> {
    if revision.is_empty()
        || revision.len() > 512
        || revision.contains('\\')
        || revision.contains('@')
        || revision.contains('#')
        || revision.contains('?')
        || revision.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || component.chars().any(char::is_control)
        })
    {
        return Err("Hugging Face revision is invalid".to_string());
    }
    Ok(())
}

fn validate_hf_commit_sha(revision: &str) -> Result<(), String> {
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!(
            "Hugging Face did not resolve to a full 40-character commit SHA: '{revision}'"
        ));
    }
    Ok(())
}

fn validate_hf_file_path(file_name: &str) -> Result<(), String> {
    if file_name.is_empty()
        || file_name.len() > 2048
        || file_name.starts_with('/')
        || file_name.contains('\\')
        || file_name.contains('#')
        || file_name.contains('?')
        || file_name.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || component.chars().any(char::is_control)
        })
    {
        return Err("Hugging Face filename is unsafe".to_string());
    }
    Ok(())
}

fn validate_quantization(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
    {
        return Err("Quantization selector must use 1-64 letters, digits, '_' or '-'".to_string());
    }
    Ok(())
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
}

fn is_gguf_file(file_name: &str) -> bool {
    file_name.to_ascii_lowercase().ends_with(".gguf")
}

fn is_sharded_gguf(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let Some(stem) = lower.strip_suffix(".gguf") else {
        return false;
    };
    let Some(of_index) = stem.rfind("-of-") else {
        return false;
    };
    let total = &stem[of_index + 4..];
    let prefix = &stem[..of_index];
    let Some(index) = prefix.rsplit('-').next() else {
        return false;
    };
    index.len() >= 5
        && total.len() >= 5
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && total.bytes().all(|byte| byte.is_ascii_digit())
}

fn filename_has_quantization(file_name: &str, quantization: &str) -> bool {
    let file_name = file_name.to_ascii_uppercase();
    let quantization = quantization.to_ascii_uppercase();
    let mut offset = 0;
    while let Some(relative) = file_name[offset..].find(&quantization) {
        let start = offset + relative;
        let end = start + quantization.len();
        let before_ok = start == 0
            || !file_name.as_bytes()[start - 1].is_ascii_alphanumeric()
                && file_name.as_bytes()[start - 1] != b'_';
        let after_ok = end == file_name.len()
            || !file_name.as_bytes()[end].is_ascii_alphanumeric()
                && file_name.as_bytes()[end] != b'_';
        if before_ok && after_ok {
            return true;
        }
        offset = end;
        if offset >= file_name.len() {
            break;
        }
    }
    false
}

fn normalize_sha256(value: &str, field: &str) -> Result<String, String> {
    let value = value.strip_prefix("sha256:").unwrap_or(value);
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("{field} must be a 64-character SHA-256 hex digest"));
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_model_size(size: u64) -> Result<(), String> {
    if size == 0 || size > MAX_MODEL_BYTES {
        return Err(format!(
            "Model size must be between 1 and {MAX_MODEL_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_human_text(value: &str, field: &str, max_bytes: usize) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| character == '\0')
    {
        return Err(format!("{field} is empty, oversized, or contains NUL"));
    }
    Ok(())
}

fn build_http_client() -> Result<Client, String> {
    Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 8 {
                return attempt.error("too many model-source redirects");
            }
            if validate_public_https_url(attempt.url()).is_ok() {
                attempt.follow()
            } else {
                attempt.error("unsafe model-source redirect")
            }
        }))
        .build()
        .map_err(|error| format!("Failed to build model-source HTTP client: {error}"))
}

fn validate_public_https_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
    {
        return Err("Model-source URL must be credential-free HTTPS with a host".to_string());
    }
    match url.host() {
        Some(Host::Domain(domain)) => {
            let domain = domain.trim_end_matches('.');
            if domain.is_empty()
                || domain.eq_ignore_ascii_case("localhost")
                || domain.to_ascii_lowercase().ends_with(".localhost")
            {
                return Err("Model-source URL cannot target localhost".to_string());
            }
        }
        Some(Host::Ipv4(address)) if !public_ipv4(address) => {
            return Err("Model-source URL cannot target a private IPv4 address".to_string())
        }
        Some(Host::Ipv6(address)) if !public_ipv6(address) => {
            return Err("Model-source URL cannot target a private IPv6 address".to_string())
        }
        Some(_) => {}
        None => return Err("Model-source URL has no host".to_string()),
    }
    Ok(())
}

fn validate_ollama_auth_url(url: &Url) -> Result<(), String> {
    validate_public_https_url(url)?;
    let host = url
        .host_str()
        .ok_or("Ollama authentication URL has no host")?
        .to_ascii_lowercase();
    if host == "ollama.ai"
        || host.ends_with(".ollama.ai")
        || host == "ollama.com"
        || host.ends_with(".ollama.com")
    {
        Ok(())
    } else {
        Err("Ollama authentication challenge points outside ollama.ai/ollama.com".to_string())
    }
}

fn public_ipv4(address: Ipv4Addr) -> bool {
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || address.is_multicast())
}

fn public_ipv6(address: Ipv6Addr) -> bool {
    // Checked before the mapped unwrap: `::a.b.c.d` is not `::ffff:a.b.c.d`, and
    // without this it was reported as public. See `egress::is_ipv4_compatible`.
    if crate::egress::is_ipv4_compatible(&address) {
        return false;
    }
    if let Some(address) = address.to_ipv4_mapped() {
        return public_ipv4(address);
    }
    let segments = address.segments();
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] & 0xffc0) == 0xfec0)
}

fn require_success(
    response: Response,
    label: &str,
    not_found: Option<StatusCode>,
) -> Result<Response, String> {
    if response.status().is_success() {
        return Ok(response);
    }
    if not_found == Some(response.status()) {
        return Err(format!("{label} was not found"));
    }
    Err(format!(
        "{label} request failed with HTTP {}",
        response.status()
    ))
}

async fn response_bytes_bounded(
    response: Response,
    max_bytes: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes as u64)
    {
        return Err(format!("{label} exceeds its {max_bytes}-byte limit"));
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("Failed to read {label}: {error}"))?;
        if bytes.len().saturating_add(chunk.len()) > max_bytes {
            return Err(format!("{label} exceeds its {max_bytes}-byte limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Reads the template that is checksum-bound inside the GGUF itself. Ollama's
/// separate OCI template layer is intentionally ignored because it uses Go
/// template syntax while llama.cpp's `--jinja` renderer expects Jinja.
fn embedded_tool_calling(path: &Path) -> Result<bool, String> {
    let header = crate::quantization::sniff_gguf_file(path).map_err(|error| {
        format!(
            "Model {} has an invalid GGUF metadata section: {error}",
            path.display()
        )
    })?;
    Ok(header
        .chat_template
        .as_deref()
        .is_some_and(embedded_jinja_advertises_tools))
}

fn embedded_jinja_advertises_tools(template: &str) -> bool {
    if template.is_empty()
        || template
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        return false;
    }
    let lower = template.to_ascii_lowercase();
    let compact = lower.replace(['\r', '\n', '\t'], " ");
    let looks_like_ollama_go_template = [
        "{{ if ",
        "{{- if ",
        "{{ range ",
        "{{- range ",
        "{{ end",
        "{{- end",
        ".tools",
        ".messages",
        "json .",
        ":=",
    ]
    .iter()
    .any(|marker| compact.contains(marker));
    if looks_like_ollama_go_template
        || !has_closed_jinja_tag(template, "{%", "%}")
        || (!jinja_tag_has_identifier(template, "{%", "%}", b"tools")
            && !jinja_tag_has_identifier(template, "{{", "}}", b"tools"))
    {
        return false;
    }

    true
}

fn has_closed_jinja_tag(template: &str, opening: &str, closing: &str) -> bool {
    let Some(start) = template.find(opening) else {
        return false;
    };
    template[start + opening.len()..].contains(closing)
}

fn jinja_tag_has_identifier(
    template: &str,
    opening: &str,
    closing: &str,
    identifier: &[u8],
) -> bool {
    let mut remainder = template;
    while let Some(start) = remainder.find(opening) {
        let tag_start = start + opening.len();
        let after_opening = &remainder[tag_start..];
        let Some(end) = after_opening.find(closing) else {
            return false;
        };
        if unquoted_tag_has_identifier(&after_opening[..end], identifier) {
            return true;
        }
        remainder = &after_opening[end + closing.len()..];
    }
    false
}

fn unquoted_tag_has_identifier(tag: &str, identifier: &[u8]) -> bool {
    let bytes = tag.as_bytes();
    let mut index = 0;
    let mut quote = None;
    while index < bytes.len() {
        let byte = bytes[index];
        if let Some(active_quote) = quote {
            if byte == b'\\' {
                index = index.saturating_add(2);
                continue;
            }
            if byte == active_quote {
                quote = None;
            }
            index += 1;
            continue;
        }
        if matches!(byte, b'\'' | b'"') {
            quote = Some(byte);
            index += 1;
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || bytes[index] == b'_')
            {
                index += 1;
            }
            let preceded_by_dot = bytes[..start]
                .iter()
                .rev()
                .find(|byte| !byte.is_ascii_whitespace())
                .is_some_and(|byte| *byte == b'.');
            if &bytes[start..index] == identifier && !preceded_by_dot {
                return true;
            }
            continue;
        }
        index += 1;
    }
    false
}

fn validate_local_gguf(path: &Path, expected_size: u64) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect model {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() != expected_size {
        return Err(format!(
            "Model {} is not a regular file of the expected size",
            path.display()
        ));
    }
    let mut file = File::open(path)
        .map_err(|error| format!("Failed to open model {}: {error}", path.display()))?;
    let mut magic = [0_u8; 4];
    file.read_exact(&mut magic)
        .map_err(|error| format!("Failed to read GGUF header {}: {error}", path.display()))?;
    if &magic != GGUF_MAGIC {
        return Err(format!(
            "Model {} does not have a GGUF header",
            path.display()
        ));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Failed to inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let mut file =
        File::open(path).map_err(|error| format!("Failed to open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

async fn sha256_file_async(path: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || sha256_file(&path))
        .await
        .map_err(|error| format!("Model checksum task failed: {error}"))?
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn now_ms() -> Result<u64, String> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("System clock is before Unix epoch: {error}"))?;
    u64::try_from(duration.as_millis()).map_err(|_| "System timestamp overflow".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u32(buffer: &mut Vec<u8>, value: u32) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }

    fn write_u64(buffer: &mut Vec<u8>, value: u64) {
        buffer.extend_from_slice(&value.to_le_bytes());
    }

    fn write_string(buffer: &mut Vec<u8>, value: &str) {
        write_u64(buffer, value.len() as u64);
        buffer.extend_from_slice(value.as_bytes());
    }

    fn minimal_gguf(chat_template: Option<&str>) -> Vec<u8> {
        let mut buffer = Vec::new();
        buffer.extend_from_slice(GGUF_MAGIC);
        write_u32(&mut buffer, 3);
        write_u64(&mut buffer, 0);
        write_u64(&mut buffer, 1 + u64::from(chat_template.is_some()));
        write_string(&mut buffer, "general.architecture");
        write_u32(&mut buffer, 8);
        write_string(&mut buffer, "llama");
        if let Some(chat_template) = chat_template {
            write_string(&mut buffer, "tokenizer.chat_template");
            write_u32(&mut buffer, 8);
            write_string(&mut buffer, chat_template);
        }
        buffer
    }

    fn lfs_sibling(name: &str, sha: &str, size: u64) -> HfSibling {
        HfSibling {
            rfilename: name.to_string(),
            size: Some(size),
            lfs: Some(HfLfs {
                sha256: sha.to_string(),
                size,
            }),
        }
    }

    fn hf_metadata(siblings: Vec<HfSibling>) -> HfModelMetadata {
        HfModelMetadata {
            id: Some("owner/repo".to_string()),
            private: false,
            gated: serde_json::Value::Bool(false),
            sha: Some("a".repeat(40)),
            siblings,
            card_data: None,
        }
    }

    fn digest(character: char) -> String {
        std::iter::repeat(character).take(64).collect()
    }

    fn test_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "little-monkey-model-source-{label}-{}",
            Uuid::new_v4().simple()
        ))
    }

    /// The same deprecated form that fooled the other three guards reported
    /// `::127.0.0.1` as a *public* address here, which is what gates a model
    /// download URL.
    #[test]
    fn the_deprecated_ipv4_compatible_form_is_not_public() {
        use std::str::FromStr;
        for text in ["::127.0.0.1", "::10.0.0.1"] {
            assert!(
                !public_ipv6(Ipv6Addr::from_str(text).expect("parses")),
                "{text} must not be treated as public"
            );
        }
        assert!(public_ipv6(
            Ipv6Addr::from_str("2606:2800:220:1:248:1893:25c8:1946").unwrap()
        ));
        assert!(!public_ipv6(
            Ipv6Addr::from_str("::ffff:127.0.0.1").unwrap()
        ));
    }

    #[test]
    fn missing_model_cleanup_removes_a_dangling_provenance_sidecar() {
        let directory = test_dir("dangling-sidecar");
        fs::create_dir_all(&directory).unwrap();
        let model_path = directory.join("managed.gguf");
        let sidecar = provenance_path(&model_path).unwrap();
        fs::write(&sidecar, b"stale metadata").unwrap();

        remove_stale_provenance_for_missing_model(&model_path).unwrap();

        assert!(!sidecar.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn managed_delete_removes_sidecar_before_the_model_under_its_install_lock() {
        let directory = test_dir("managed-delete");
        fs::create_dir_all(&directory).unwrap();
        let canonical_directory = directory.canonicalize().unwrap();
        let model_path = canonical_directory.join("managed.gguf");
        let sidecar = provenance_path(&model_path).unwrap();
        fs::write(&model_path, b"model bytes").unwrap();
        fs::write(&sidecar, b"metadata").unwrap();

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(delete_installed_model(&canonical_directory, &model_path))
            .unwrap();

        assert!(!model_path.exists());
        assert!(!sidecar.exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn managed_delete_keeps_the_model_when_sidecar_is_not_a_file() {
        let directory = test_dir("unsafe-delete-sidecar");
        fs::create_dir_all(&directory).unwrap();
        let canonical_directory = directory.canonicalize().unwrap();
        let model_path = canonical_directory.join("managed.gguf");
        let sidecar = provenance_path(&model_path).unwrap();
        fs::write(&model_path, b"model bytes").unwrap();
        fs::create_dir(&sidecar).unwrap();

        let error = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(delete_installed_model(&canonical_directory, &model_path))
            .unwrap_err();

        assert!(error.contains("not a removable file"));
        assert!(model_path.is_file());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn ollama_parser_normalizes_namespace_and_latest_tag() {
        assert_eq!(
            parse_model_reference("qwen3").unwrap(),
            ParsedModelReference::Ollama(OllamaReference {
                namespace: "library".to_string(),
                model: "qwen3".to_string(),
                tag: "latest".to_string(),
            })
        );
        assert_eq!(
            parse_model_reference("ollama:acme/code-model:q4_K_M").unwrap(),
            ParsedModelReference::Ollama(OllamaReference {
                namespace: "acme".to_string(),
                model: "code-model".to_string(),
                tag: "q4_K_M".to_string(),
            })
        );
    }

    #[test]
    fn ollama_parser_rejects_hosts_traversal_and_extra_namespaces() {
        for reference in [
            "ollama:https://evil.example/model",
            "ollama:../model:latest",
            "ollama:a/b/c:latest",
            "ollama:model:tag:extra",
            "ollama:model@sha256:abcd",
        ] {
            assert!(
                parse_model_reference(reference).is_err(),
                "{reference} should be rejected"
            );
        }
    }

    #[test]
    fn hf_parser_defaults_to_main_and_supports_file_or_quant_selector() {
        assert_eq!(
            parse_model_reference("hf:owner/repo@main#weights/model-Q5_K_M.gguf").unwrap(),
            ParsedModelReference::HuggingFace(HuggingFaceReference {
                repo: "owner/repo".to_string(),
                requested_revision: "main".to_string(),
                selector: HuggingFaceSelector::File("weights/model-Q5_K_M.gguf".to_string()),
            })
        );
        assert_eq!(
            parse_model_reference("hf.co/owner/repo@refs/pr/7#quant=Q4_K_M").unwrap(),
            ParsedModelReference::HuggingFace(HuggingFaceReference {
                repo: "owner/repo".to_string(),
                requested_revision: "refs/pr/7".to_string(),
                selector: HuggingFaceSelector::Quantization("Q4_K_M".to_string()),
            })
        );
        assert_eq!(
            parse_model_reference("hf.co/owner/repo:Q4_K_M").unwrap(),
            ParsedModelReference::HuggingFace(HuggingFaceReference {
                repo: "owner/repo".to_string(),
                requested_revision: "main".to_string(),
                selector: HuggingFaceSelector::Quantization("Q4_K_M".to_string()),
            })
        );
        assert_eq!(
            parse_model_reference("https://huggingface.co/owner/repo#file=model.gguf").unwrap(),
            ParsedModelReference::HuggingFace(HuggingFaceReference {
                repo: "owner/repo".to_string(),
                requested_revision: "main".to_string(),
                selector: HuggingFaceSelector::File("model.gguf".to_string()),
            })
        );
        assert!(parse_model_reference("hf:owner/repo@main#../../model.gguf").is_err());
    }

    #[test]
    fn ollama_manifest_requires_exactly_one_model_layer() {
        let layer = OciLayer {
            media_type: OLLAMA_MODEL_MEDIA_TYPE.to_string(),
            digest: format!("sha256:{}", digest('a')),
            size: 123,
        };
        let one = OciManifest {
            schema_version: 2,
            media_type: Some(OCI_MANIFEST_MEDIA_TYPE.to_string()),
            layers: vec![layer.clone()],
        };
        assert_eq!(select_ollama_model_layer(&one).unwrap().size, 123);

        let dependent = OciManifest {
            schema_version: 2,
            media_type: None,
            layers: vec![
                layer.clone(),
                OciLayer {
                    media_type: OLLAMA_PROJECTOR_MEDIA_TYPE.to_string(),
                    digest: format!("sha256:{}", digest('b')),
                    size: 456,
                },
            ],
        };
        assert!(select_ollama_model_layer(&dependent)
            .unwrap_err()
            .contains("projector"));

        let none = OciManifest {
            schema_version: 2,
            media_type: None,
            layers: vec![],
        };
        assert!(select_ollama_model_layer(&none)
            .unwrap_err()
            .contains("does not contain"));

        let many = OciManifest {
            schema_version: 2,
            media_type: None,
            layers: vec![layer.clone(), layer],
        };
        assert!(select_ollama_model_layer(&many)
            .unwrap_err()
            .contains("multiple"));
    }

    #[test]
    fn tool_support_requires_embedded_jinja_and_rejects_ollama_go_templates() {
        assert!(embedded_jinja_advertises_tools(
            "{% if tools %}{{ tools | tojson }}{% endif %}"
        ));
        assert!(embedded_jinja_advertises_tools(
            "{% if tools %}{%- for call in message.tool_calls %}{{ call }}{%- endfor %}{% endif %}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{{ if .Tools }}{{ json .Tools }}{{ end }}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "qwen coder tool-use function-calling"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{% for message in messages %}{{ message.content }}{% endfor %}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{% for call in message.tool_calls %}{{ call }}{% endfor %}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{% for message in messages %}{{ \"tools\" }}{% endfor %}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{% if namespace.tools %}{{ namespace.tools }}{% endif %}"
        ));
        assert!(!embedded_jinja_advertises_tools(
            "{% if tools %}\0{{ tools }}{% endif %}"
        ));
    }

    #[test]
    fn hf_default_selects_only_a_unique_q4_k_m_file() {
        let metadata = hf_metadata(vec![
            lfs_sibling("model-Q4_K_M.gguf", &digest('a'), 12),
            lfs_sibling("model-Q5_K_M.gguf", &digest('b'), 13),
        ]);
        assert_eq!(
            select_hf_sibling(&metadata, &HuggingFaceSelector::DefaultQ4Km)
                .unwrap()
                .rfilename,
            "model-Q4_K_M.gguf"
        );

        let ambiguous = hf_metadata(vec![
            lfs_sibling("model-Q4_K_M.gguf", &digest('a'), 12),
            lfs_sibling("model-i1-Q4_K_M.gguf", &digest('b'), 13),
        ]);
        assert!(
            select_hf_sibling(&ambiguous, &HuggingFaceSelector::DefaultQ4Km)
                .unwrap_err()
                .contains("ambiguous")
        );
    }

    #[test]
    fn hf_selection_rejects_shards_non_gguf_and_missing_lfs() {
        let sharded = hf_metadata(vec![lfs_sibling(
            "model-00001-of-00002.gguf",
            &digest('a'),
            12,
        )]);
        assert!(select_hf_sibling(
            &sharded,
            &HuggingFaceSelector::File("model-00001-of-00002.gguf".to_string())
        )
        .unwrap_err()
        .contains("shard"));

        let non_gguf = hf_metadata(vec![lfs_sibling("model.safetensors", &digest('a'), 12)]);
        assert!(select_hf_sibling(
            &non_gguf,
            &HuggingFaceSelector::File("model.safetensors".to_string())
        )
        .unwrap_err()
        .contains("not a GGUF"));

        let missing_lfs = hf_metadata(vec![HfSibling {
            rfilename: "model-Q4_K_M.gguf".to_string(),
            size: Some(12),
            lfs: None,
        }]);
        assert!(select_hf_sibling(
            &missing_lfs,
            &HuggingFaceSelector::File("model-Q4_K_M.gguf".to_string())
        )
        .unwrap_err()
        .contains("LFS"));
    }

    #[test]
    fn hf_official_blob_metadata_shape_deserializes_with_exact_lfs_receipt() {
        let metadata: HfModelMetadata = serde_json::from_value(serde_json::json!({
            "id": "owner/repo",
            "private": false,
            "gated": false,
            "sha": "a".repeat(40),
            "siblings": [
                {
                    "rfilename": "weights/model-Q4_K_M.gguf",
                    "size": 1234,
                    "lfs": {
                        "sha256": digest('b'),
                        "size": 1234,
                        "pointerSize": 131
                    }
                },
                {
                    "rfilename": "LICENSE",
                    "size": 12
                }
            ],
            "cardData": {
                "license": "apache-2.0",
                "license_link": "LICENSE"
            }
        }))
        .unwrap();
        let selected = select_hf_sibling(&metadata, &HuggingFaceSelector::DefaultQ4Km).unwrap();
        assert_eq!(selected.rfilename, "weights/model-Q4_K_M.gguf");
        assert_eq!(selected.lfs.as_ref().unwrap().sha256, digest('b'));
        assert_eq!(selected.lfs.as_ref().unwrap().size, 1234);

        // A relative card-data link is not a fetchable public URL. Resolution
        // falls back to a pinned repo LICENSE URL instead of failing the model.
        let (license_name, license_url) =
            hugging_face_license(&metadata, "owner/repo", &"a".repeat(40)).unwrap();
        assert_eq!(license_name.as_deref(), Some("apache-2.0"));
        assert!(license_url
            .as_deref()
            .is_some_and(|url| url.contains("/blob/") && url.ends_with("/LICENSE")));
    }

    #[test]
    fn registry_bearer_challenge_is_parsed_and_origin_restricted() {
        let challenge = parse_registry_auth_challenge(
            r#"Bearer realm="https://auth.ollama.ai/token",service="registry.ollama.ai",scope="repository:library/qwen3:pull""#,
        )
        .unwrap();
        assert_eq!(challenge.realm.as_str(), "https://auth.ollama.ai/token");
        assert_eq!(challenge.service.as_deref(), Some("registry.ollama.ai"));
        assert_eq!(
            challenge.scope.as_deref(),
            Some("repository:library/qwen3:pull")
        );
        assert!(parse_registry_auth_challenge(
            r#"Bearer realm="https://evil.example/token",service="registry.ollama.ai""#
        )
        .is_err());
    }

    #[test]
    fn url_validation_rejects_ipv4_mapped_loopback_and_documentation_ipv6() {
        assert!(validate_public_https_url(
            &Url::parse("https://[::ffff:127.0.0.1]/model.gguf").unwrap()
        )
        .is_err());
        assert!(validate_public_https_url(
            &Url::parse("https://[2001:db8::1]/model.gguf").unwrap()
        )
        .is_err());
        assert!(validate_public_https_url(
            &Url::parse("https://huggingface.co/model.gguf").unwrap()
        )
        .is_ok());
    }

    #[test]
    fn partial_download_state_is_resumable_but_oversized_state_is_replaced() {
        let directory = test_dir("partial");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("model.gguf.part");
        fs::write(&path, [1_u8, 2, 3]).unwrap();
        prepare_partial_file(&path, 10).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 3);

        fs::write(&path, [0_u8; 11]).unwrap();
        prepare_partial_file(&path, 10).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn install_lock_serializes_competing_guards() {
        let directory = test_dir("install-lock");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("model.gguf.install.lock");
        let first = acquire_cross_process_lock(&path).unwrap();
        let competing_path = path.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let competing = std::thread::spawn(move || {
            let second = acquire_cross_process_lock(&competing_path).unwrap();
            sender.send(()).unwrap();
            drop(second);
        });

        assert!(
            receiver.recv_timeout(Duration::from_millis(100)).is_err(),
            "a competing installer must wait while the destination lock is held"
        );
        drop(first);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("the competing installer should proceed after lock release");
        competing.join().unwrap();
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn checksum_and_gguf_validation_detect_tampering() {
        let directory = test_dir("checksum");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("model.gguf");
        fs::write(&path, b"GGUFpayload").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"GGUFpayload"));
        assert_eq!(sha256_file(&path).unwrap(), expected);
        validate_local_gguf(&path, 11).unwrap();
        fs::write(&path, b"NOPEpayload").unwrap();
        assert!(validate_local_gguf(&path, 11).is_err());
        assert_ne!(sha256_file(&path).unwrap(), expected);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn provenance_roundtrip_preserves_capability_and_rejects_filename_mismatch() {
        let directory = test_dir("provenance");
        fs::create_dir_all(&directory).unwrap();
        let model_path = directory.join("hf-repo-model-aaaaaaaaaaaa.gguf");
        let model_bytes = minimal_gguf(None);
        fs::write(&model_path, &model_bytes).unwrap();
        let resolved = ResolvedModelReference {
            source: ModelReferenceSource::HuggingFace,
            canonical_reference: format!("hf:owner/repo@{}#model-Q4_K_M.gguf", "a".repeat(40)),
            display_name: "Repo model".to_string(),
            repo: "owner/repo".to_string(),
            revision: "a".repeat(40),
            file_name: "model-Q4_K_M.gguf".to_string(),
            download_url: format!(
                "https://huggingface.co/owner/repo/resolve/{}/model-Q4_K_M.gguf?download=true",
                "a".repeat(40)
            ),
            sha256: format!("{:x}", Sha256::digest(&model_bytes)),
            size_bytes: model_bytes.len() as u64,
            tool_calling: false,
            license_name: Some("apache-2.0".to_string()),
            license_url: Some(format!(
                "https://huggingface.co/owner/repo/blob/{}/LICENSE",
                "a".repeat(40)
            )),
        };
        let provenance = provenance_for(
            &resolved,
            "hf.co/owner/repo:Q4_K_M".to_string(),
            "hf-repo-model-aaaaaaaaaaaa.gguf".to_string(),
            123,
            false,
        );
        save_provenance(&model_path, &provenance).unwrap();
        let loaded = load_provenance(&model_path).unwrap().unwrap();
        assert_eq!(loaded, provenance);
        assert!(!loaded.tool_calling);
        assert!(
            reusable_existing_install(&model_path, &resolved)
                .unwrap()
                .is_some(),
            "a verified existing managed install should be reusable"
        );
        let offline = find_installed_reference(&directory, "hf.co/owner/repo:Q4_K_M")
            .unwrap()
            .expect("the original reference should resolve from provenance offline");
        assert_eq!(offline.local_path, model_path.canonicalize().unwrap());
        assert_eq!(offline.resolved, resolved);

        let other_path = directory.join("other.gguf");
        fs::write(&other_path, &model_bytes).unwrap();
        fs::copy(
            provenance_path(&model_path).unwrap(),
            provenance_path(&other_path).unwrap(),
        )
        .unwrap();
        assert!(load_provenance(&other_path)
            .unwrap_err()
            .contains("local filename"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn runtime_verification_rehashes_managed_model_payloads() {
        let directory = test_dir("runtime-rehash");
        fs::create_dir_all(&directory).unwrap();
        let model_path = directory.join("hf-runtime-model.gguf");
        let model_bytes = minimal_gguf(None);
        fs::write(&model_path, &model_bytes).unwrap();
        let resolved = ResolvedModelReference {
            source: ModelReferenceSource::HuggingFace,
            canonical_reference: format!("hf:owner/repo@{}#model.gguf", "a".repeat(40)),
            display_name: "Runtime model".to_string(),
            repo: "owner/repo".to_string(),
            revision: "a".repeat(40),
            file_name: "model.gguf".to_string(),
            download_url: format!(
                "https://huggingface.co/owner/repo/resolve/{}/model.gguf?download=true",
                "a".repeat(40)
            ),
            sha256: format!("{:x}", Sha256::digest(&model_bytes)),
            size_bytes: model_bytes.len() as u64,
            tool_calling: false,
            license_name: None,
            license_url: None,
        };
        let provenance = provenance_for(
            &resolved,
            "hf:owner/repo#model.gguf".to_string(),
            "hf-runtime-model.gguf".to_string(),
            123,
            false,
        );
        save_provenance(&model_path, &provenance).unwrap();
        verify_managed_model_for_runtime(&model_path).unwrap();

        let mut tampered = model_bytes;
        let architecture = tampered
            .windows(b"llama".len())
            .position(|window| window == b"llama")
            .expect("minimal GGUF contains its architecture");
        tampered[architecture] = b'L';
        fs::write(&model_path, tampered).unwrap();
        assert!(
            load_provenance(&model_path).is_ok(),
            "bounded metadata validation alone should not pretend to rehash the payload"
        );
        assert!(verify_managed_model_for_runtime(&model_path)
            .unwrap_err()
            .contains("SHA-256"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn verified_orphaned_model_recovers_provenance_without_redownload() {
        let directory = test_dir("orphan-recovery");
        fs::create_dir_all(&directory).unwrap();
        let model_bytes = minimal_gguf(Some("{% if tools %}{{ tools | tojson }}{% endif %}"));
        let resolved = ResolvedModelReference {
            source: ModelReferenceSource::HuggingFace,
            canonical_reference: format!("hf:owner/repo@{}#model.gguf", "a".repeat(40)),
            display_name: "Recovered model".to_string(),
            repo: "owner/repo".to_string(),
            revision: "a".repeat(40),
            file_name: "model.gguf".to_string(),
            download_url: format!(
                "https://huggingface.co/owner/repo/resolve/{}/model.gguf?download=true",
                "a".repeat(40)
            ),
            sha256: format!("{:x}", Sha256::digest(&model_bytes)),
            size_bytes: model_bytes.len() as u64,
            tool_calling: false,
            license_name: None,
            license_url: None,
        };
        let local_file_name = local_file_name(&resolved, false);
        let model_path = directory.join(&local_file_name);
        fs::write(&model_path, model_bytes).unwrap();
        assert!(!provenance_path(&model_path).unwrap().exists());

        let recovered = recover_orphaned_install(
            &model_path,
            &resolved,
            "hf:owner/repo#model.gguf",
            &local_file_name,
        )
        .unwrap()
        .expect("a checksum-identical orphan should be recovered");
        assert!(recovered.tool_calling);
        assert_eq!(
            load_provenance(&model_path).unwrap(),
            Some(recovered.clone())
        );
        assert_eq!(
            reusable_existing_install(&model_path, &resolved).unwrap(),
            Some(recovered)
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn long_reference_file_names_keep_collision_hashes_before_truncation() {
        let mut first = ResolvedModelReference {
            source: ModelReferenceSource::HuggingFace,
            canonical_reference: format!(
                "hf:owner/{}@{}#weights/{}.gguf",
                "repository".repeat(40),
                "a".repeat(40),
                "model".repeat(40)
            ),
            display_name: "Long model".to_string(),
            repo: format!("owner/{}", "repository".repeat(40)),
            revision: "a".repeat(40),
            file_name: format!("weights/{}.gguf", "model".repeat(40)),
            download_url: "https://huggingface.co/model.gguf".to_string(),
            sha256: digest('a'),
            size_bytes: 123,
            tool_calling: false,
            license_name: None,
            license_url: None,
        };
        let first_name = local_file_name(&first, true);
        first.canonical_reference.push_str("-different");
        let second_name = local_file_name(&first, true);

        assert!(first_name.starts_with("hf-aaaaaaaaaaaa-"));
        assert!(second_name.starts_with("hf-aaaaaaaaaaaa-"));
        assert_ne!(first_name, second_name);
        assert!(first_name.len() <= 180);
        assert!(second_name.len() <= 180);
        assert!(first_name.ends_with(".gguf"));
        assert!(second_name.ends_with(".gguf"));
    }

    #[test]
    fn embedded_jinja_capability_roundtrips_and_tampering_fails_closed() {
        let directory = test_dir("embedded-template");
        fs::create_dir_all(&directory).unwrap();
        let model_path = directory.join("ollama-qwen3.gguf");
        let model_bytes = minimal_gguf(Some("{% if tools %}{{ tools | tojson }}{% endif %}"));
        fs::write(&model_path, &model_bytes).unwrap();
        let resolved = ResolvedModelReference {
            source: ModelReferenceSource::OllamaRegistry,
            canonical_reference: "ollama:library/qwen3:latest".to_string(),
            display_name: "library/qwen3:latest".to_string(),
            repo: "library/qwen3".to_string(),
            revision: "latest".to_string(),
            file_name: "qwen3-latest.gguf".to_string(),
            download_url: format!(
                "https://registry.ollama.ai/v2/library/qwen3/blobs/sha256:{}",
                digest('a')
            ),
            sha256: format!("{:x}", Sha256::digest(&model_bytes)),
            size_bytes: model_bytes.len() as u64,
            // Remote resolution cannot inspect GGUF metadata without the
            // model download, so it remains fail-closed.
            tool_calling: false,
            license_name: None,
            license_url: None,
        };
        let provenance = provenance_for(
            &resolved,
            "qwen3".to_string(),
            "ollama-qwen3.gguf".to_string(),
            123,
            true,
        );
        save_provenance(&model_path, &provenance).unwrap();

        assert_eq!(
            load_provenance(&model_path).unwrap(),
            Some(provenance.clone())
        );
        assert!(validate_expected_digest(&resolved, &resolved.sha256).is_ok());

        let mut tampered = provenance;
        tampered.tool_calling = false;
        fs::write(
            provenance_path(&model_path).unwrap(),
            serde_json::to_vec_pretty(&tampered).unwrap(),
        )
        .unwrap();
        assert!(load_provenance(&model_path)
            .unwrap_err()
            .contains("embedded GGUF template"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn content_range_validation_is_exact() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_static("bytes 100-199/300"));
        assert!(validate_content_range(&headers, 100, 300).is_ok());
        assert!(validate_content_range(&headers, 99, 300).is_err());
        assert!(validate_content_range(&headers, 100, 301).is_err());
    }

    #[test]
    fn source_dto_serializes_camel_case_and_source_values() {
        let dto = ResolvedModelReference {
            source: ModelReferenceSource::OllamaRegistry,
            canonical_reference: "ollama:library/qwen3:latest".to_string(),
            display_name: "library/qwen3:latest".to_string(),
            repo: "library/qwen3".to_string(),
            revision: "latest".to_string(),
            file_name: "qwen3-latest.gguf".to_string(),
            download_url: format!(
                "https://registry.ollama.ai/v2/library/qwen3/blobs/sha256:{}",
                digest('a')
            ),
            sha256: digest('a'),
            size_bytes: 123,
            tool_calling: false,
            license_name: None,
            license_url: None,
        };
        let value = serde_json::to_value(dto).unwrap();
        assert_eq!(value["source"], "ollama_registry");
        assert_eq!(value["canonicalReference"], "ollama:library/qwen3:latest");
        assert_eq!(value["sizeBytes"], 123);
        assert_eq!(value["toolCalling"], false);
        assert!(value.get("templateSha256").is_none());
        assert!(value.get("canonical_reference").is_none());
    }
}

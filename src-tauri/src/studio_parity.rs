//! Creator-parity surfaces that deliberately reuse Little Monkey's existing
//! trust boundaries instead of embedding third-party Python into the Tauri
//! process: verified public model discovery/download, imported-chat staging,
//! MFLUX LoRA training, and user-owned ComfyUI workflow execution.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};
use tauri::AppHandle;
use uuid::Uuid;

use crate::artifact_store::ArtifactStore;
use crate::generation::GenerationTask;
use crate::generation_commands::GenerationEntry;
use crate::profiles::ProfileScopedPaths;

const MAX_DISCOVERY_RESULTS: usize = 48;
const MAX_DOWNLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const MAX_IMPORT_DOCUMENTS: usize = 20_000;
const MAX_IMPORT_DOC_BYTES: usize = 5 * 1024 * 1024;
const MAX_IMPORT_TOTAL_BYTES: usize = 256 * 1024 * 1024;
const MAX_WORKFLOW_BYTES: usize = 4 * 1024 * 1024;
const MAX_WORKFLOW_MEDIA_BYTES: usize = 256 * 1024 * 1024;
const MAX_TRAINING_IMAGES: usize = 128;
const WORKFLOW_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryItem {
    pub id: String,
    pub source: String,
    pub name: String,
    pub asset_kind: String,
    pub family: String,
    pub repo: Option<String>,
    pub file_name: String,
    pub download_url: String,
    pub page_url: String,
    pub sha256: Option<String>,
    pub size_bytes: u64,
    pub tags: Vec<String>,
}

fn public_client() -> Result<reqwest::Client, String> {
    crate::web::executable_extension_http_client(Duration::from_secs(10 * 60))
        .map_err(|error| error.to_string())
}

fn validate_public_https(url: &Url) -> Result<(), String> {
    if url.scheme() != "https" {
        return Err("Studio discovery only permits HTTPS downloads".to_string());
    }
    if url.host_str().is_none() {
        return Err("The download URL has no host".to_string());
    }
    Ok(())
}

async fn get_json(url: Url) -> Result<Value, String> {
    validate_public_https(&url)?;
    let response = crate::egress::send(public_client()?.get(url.clone()))
        .await
        .map_err(|error| format!("Failed to query {url}: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("Model catalog returned HTTP {}", response.status()));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| error.to_string())
}

fn normalize_sha(value: &str) -> Option<String> {
    let lowered = value.trim().to_ascii_lowercase();
    (lowered.len() == 64 && lowered.chars().all(|ch| ch.is_ascii_hexdigit())).then_some(lowered)
}

fn family_from(name: &str, tags: &[String]) -> String {
    let haystack = format!("{} {}", name, tags.join(" ")).to_ascii_lowercase();
    if haystack.contains("qwen") {
        "Qwen".into()
    } else if haystack.contains("flux") {
        "FLUX".into()
    } else if haystack.contains("wan") {
        "Wan".into()
    } else if haystack.contains("ltx") {
        "LTX".into()
    } else if haystack.contains("sdxl") {
        "SDXL".into()
    } else {
        "Community".into()
    }
}

fn infer_asset_kind(name: &str, declared: Option<&str>, tags: &[String]) -> String {
    let all = format!(
        "{} {} {}",
        name,
        declared.unwrap_or_default(),
        tags.join(" ")
    )
    .to_ascii_lowercase();
    if all.contains("lora") || all.contains("lycoris") {
        "lora".into()
    } else {
        "model".into()
    }
}

#[tauri::command]
pub async fn studio_discovery_search(
    source: String,
    query: String,
    asset_kind: Option<String>,
) -> Result<Vec<DiscoveryItem>, String> {
    let query = query.trim();
    if query.is_empty() || query.len() > 160 {
        return Err("Search query must be between 1 and 160 characters".to_string());
    }
    match source.as_str() {
        "civitai" => search_civitai(query, asset_kind.as_deref()).await,
        "hugging_face" => search_hugging_face(query, asset_kind.as_deref()).await,
        _ => Err("Unknown model catalog source".to_string()),
    }
}

async fn search_civitai(
    query: &str,
    asset_kind: Option<&str>,
) -> Result<Vec<DiscoveryItem>, String> {
    let mut url = Url::parse("https://civitai.com/api/v1/models").unwrap();
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("query", query)
            .append_pair("limit", "24")
            .append_pair("sort", "Highest Rated");
        if asset_kind == Some("lora") {
            pairs.append_pair("types", "LORA");
        }
        if asset_kind == Some("model") {
            pairs.append_pair("types", "Checkpoint");
        }
    }
    let payload = get_json(url).await?;
    let mut results = Vec::new();
    for model in payload
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let model_name = model
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Community model");
        let model_type = model.get("type").and_then(Value::as_str);
        let tags: Vec<String> = model
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        let Some(version) = model
            .get("modelVersions")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
        else {
            continue;
        };
        let version_id = version.get("id").and_then(Value::as_u64).unwrap_or(0);
        for file in version
            .get("files")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let file_name = file.get("name").and_then(Value::as_str).unwrap_or("");
            if !(file_name.ends_with(".safetensors") || file_name.ends_with(".gguf")) {
                continue;
            }
            let download_url = file
                .get("downloadUrl")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("https://civitai.com/api/download/models/{version_id}"));
            let sha256 = file
                .get("hashes")
                .and_then(|v| v.get("SHA256"))
                .and_then(Value::as_str)
                .and_then(normalize_sha);
            let size_bytes = file
                .get("sizeKB")
                .and_then(Value::as_f64)
                .map(|v| (v * 1024.0) as u64)
                .unwrap_or(0);
            let kind = infer_asset_kind(model_name, model_type, &tags);
            if asset_kind.is_some_and(|wanted| wanted != kind) {
                continue;
            }
            results.push(DiscoveryItem {
                id: format!("civitai:{version_id}:{file_name}"),
                source: "civitai".into(),
                name: model_name.into(),
                asset_kind: kind,
                family: family_from(model_name, &tags),
                repo: None,
                file_name: file_name.into(),
                download_url,
                page_url: format!(
                    "https://civitai.com/models/{}",
                    model.get("id").and_then(Value::as_u64).unwrap_or(0)
                ),
                sha256,
                size_bytes,
                tags: tags.clone(),
            });
            if results.len() >= MAX_DISCOVERY_RESULTS {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

async fn search_hugging_face(
    query: &str,
    asset_kind: Option<&str>,
) -> Result<Vec<DiscoveryItem>, String> {
    let mut url = Url::parse("https://huggingface.co/api/models").unwrap();
    url.query_pairs_mut()
        .append_pair("search", query)
        .append_pair("limit", "24")
        .append_pair("full", "true");
    let payload = get_json(url).await?;
    let Some(models) = payload.as_array() else {
        return Err("Unexpected Hugging Face response".into());
    };
    let mut results = Vec::new();
    for model in models {
        let Some(repo) = model.get("id").and_then(Value::as_str) else {
            continue;
        };
        let tags: Vec<String> = model
            .get("tags")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        for sibling in model
            .get("siblings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(file_name) = sibling.get("rfilename").and_then(Value::as_str) else {
                continue;
            };
            if !(file_name.ends_with(".safetensors") || file_name.ends_with(".gguf")) {
                continue;
            }
            let sha256 = sibling
                .get("lfs")
                .and_then(|v| v.get("oid"))
                .and_then(Value::as_str)
                .and_then(normalize_sha);
            let size_bytes = sibling
                .get("lfs")
                .and_then(|v| v.get("size"))
                .and_then(Value::as_u64)
                .or_else(|| sibling.get("size").and_then(Value::as_u64))
                .unwrap_or(0);
            let kind = infer_asset_kind(file_name, None, &tags);
            if asset_kind.is_some_and(|wanted| wanted != kind) {
                continue;
            }
            let mut download = Url::parse("https://huggingface.co/").unwrap();
            {
                let mut segments = download
                    .path_segments_mut()
                    .map_err(|_| "Invalid Hugging Face base URL")?;
                for part in repo.split('/') {
                    segments.push(part);
                }
                segments.push("resolve").push("main");
                for part in file_name.split('/') {
                    segments.push(part);
                }
            }
            results.push(DiscoveryItem {
                id: format!("hf:{repo}:{file_name}"),
                source: "hugging_face".into(),
                name: repo.into(),
                asset_kind: kind,
                family: family_from(repo, &tags),
                repo: Some(repo.into()),
                file_name: file_name.into(),
                download_url: download.to_string(),
                page_url: format!("https://huggingface.co/{repo}"),
                sha256,
                size_bytes,
                tags: tags.clone(),
            });
            if results.len() >= MAX_DISCOVERY_RESULTS {
                return Ok(results);
            }
        }
    }
    Ok(results)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryDownloadRequest {
    pub name: String,
    pub asset_kind: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub file_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryDownloadResult {
    pub path: String,
    pub size_bytes: u64,
    pub sha256: String,
}

fn safe_leaf(value: &str) -> String {
    let leaf = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let safe: String = leaf
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ".-_".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if safe.is_empty() {
        "asset.bin".into()
    } else {
        safe
    }
}

#[tauri::command]
pub async fn studio_discovery_download(
    app: AppHandle,
    request: DiscoveryDownloadRequest,
) -> Result<DiscoveryDownloadResult, String> {
    let expected =
        normalize_sha(&request.sha256).ok_or("One-click install requires a published SHA-256")?;
    if request.size_bytes > MAX_DOWNLOAD_BYTES {
        return Err("The selected asset exceeds the 8 GiB discovery-download limit".into());
    }
    let mut url = Url::parse(&request.download_url).map_err(|e| e.to_string())?;
    validate_public_https(&url)?;
    let base = app
        .profile_data_dir()
        .map_err(|e| e.to_string())?
        .join("studio-v1")
        .join("community-assets")
        .join(&request.asset_kind);
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let destination = base.join(format!(
        "{}-{}",
        &expected[..12],
        safe_leaf(&request.file_name)
    ));
    let temporary = destination.with_extension(format!("{}.part", Uuid::new_v4()));
    let mut redirects = 0;
    let mut response = loop {
        let response = crate::egress::send(public_client()?.get(url.clone()))
            .await
            .map_err(|e| e.to_string())?;
        if response.status().is_redirection() {
            if redirects >= 6 {
                return Err("Too many download redirects".into());
            }
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("Redirect has no Location")?;
            url = url.join(location).map_err(|e| e.to_string())?;
            validate_public_https(&url)?;
            redirects += 1;
            continue;
        }
        if !response.status().is_success() {
            return Err(format!("Download returned HTTP {}", response.status()));
        }
        break response;
    };
    if let Some(length) = response.content_length() {
        if length > MAX_DOWNLOAD_BYTES {
            return Err("Download exceeds the 8 GiB limit".into());
        }
        if request.size_bytes > 0
            && length.abs_diff(request.size_bytes) > request.size_bytes / 5 + 1024 * 1024
        {
            return Err("Download size differs materially from catalog metadata".into());
        }
    }
    let mut file = File::create(&temporary).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_DOWNLOAD_BYTES {
            let _ = fs::remove_file(&temporary);
            return Err("Download exceeded the 8 GiB limit".into());
        }
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected {
        let _ = fs::remove_file(&temporary);
        return Err(format!(
            "SHA-256 mismatch: expected {expected}, got {actual}"
        ));
    }
    fs::rename(&temporary, &destination).map_err(|e| e.to_string())?;
    Ok(DiscoveryDownloadResult {
        path: destination.to_string_lossy().into_owned(),
        size_bytes: written,
        sha256: actual,
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportDocument {
    pub title: String,
    pub markdown: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportResult {
    pub path: String,
    pub document_count: usize,
}

#[tauri::command]
pub fn chat_export_write_documents(
    app: AppHandle,
    source_name: String,
    documents: Vec<ChatImportDocument>,
) -> Result<ChatImportResult, String> {
    if documents.is_empty() || documents.len() > MAX_IMPORT_DOCUMENTS {
        return Err("Import must contain between 1 and 20,000 conversations".into());
    }
    let mut total = 0usize;
    for doc in &documents {
        if doc.markdown.len() > MAX_IMPORT_DOC_BYTES {
            return Err(format!("Conversation '{}' exceeds 5 MiB", doc.title));
        }
        total = total.saturating_add(doc.markdown.len());
        if total > MAX_IMPORT_TOTAL_BYTES {
            return Err("Chat export exceeds the 256 MiB import limit".into());
        }
    }
    let root = app
        .profile_data_dir()
        .map_err(|e| e.to_string())?
        .join("knowledge-imports")
        .join(format!("{}-{}", safe_leaf(&source_name), Uuid::new_v4()));
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    for (index, doc) in documents.iter().enumerate() {
        let path = root.join(format!("{:05}-{}.md", index + 1, safe_leaf(&doc.title)));
        fs::write(
            path,
            format!("# {}\n\n{}\n", doc.title.trim(), doc.markdown),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(ChatImportResult {
        path: root.to_string_lossy().into_owned(),
        document_count: documents.len(),
    })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterTrainingRequest {
    pub name: String,
    pub source_dir: String,
    pub base_model_path: String,
    pub trigger_word: String,
    pub epochs: u32,
    pub rank: u32,
    pub max_resolution: u32,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharacterTrainingResult {
    pub lora_path: String,
    pub image_count: usize,
    pub log_tail: String,
}

fn tail_text(path: &Path, max: usize) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn newest_safetensors(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(5)
        .into_iter()
        .flatten()
    {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|v| v.to_str()) == Some("safetensors") {
            let modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((
                path.to_path_buf(),
                modified,
                path.file_name()
                    .and_then(|v| v.to_str())
                    .unwrap_or("")
                    .contains("lora"),
            ));
        }
    }
    candidates.sort_by_key(|(_, modified, lora)| (*lora, *modified));
    candidates.pop().map(|item| item.0)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn train_character_impl(
    app: &AppHandle,
    request: CharacterTrainingRequest,
) -> Result<CharacterTrainingResult, String> {
    if request.name.trim().is_empty() || request.name.len() > 80 {
        return Err("Character name must be 1–80 characters".into());
    }
    if request.trigger_word.trim().is_empty() || request.trigger_word.len() > 80 {
        return Err("Trigger word must be 1–80 characters".into());
    }
    if !(1..=500).contains(&request.epochs)
        || !(4..=64).contains(&request.rank)
        || !(256..=1536).contains(&request.max_resolution)
    {
        return Err("Training settings are outside their supported bounds".into());
    }
    let source = PathBuf::from(&request.source_dir)
        .canonicalize()
        .map_err(|e| format!("Invalid photo folder: {e}"))?;
    let base_model = PathBuf::from(&request.base_model_path)
        .canonicalize()
        .map_err(|e| format!("Invalid base-model folder: {e}"))?;
    if !source.is_dir() || !base_model.is_dir() {
        return Err("Photos and base model must both be directories".into());
    }
    let app_data = app.profile_data_dir().map_err(|e| e.to_string())?;
    let installer = crate::m3_production::production_mflux_installer(&app_data.join("m3"))
        .map_err(|e| e.to_string())?;
    let install = installer
        .verify_active()
        .map_err(|e| format!("Install the verified MFLUX Image Runtime before training: {e}"))?;
    let run_root = app_data
        .join("studio-v1")
        .join("character-training")
        .join(Uuid::new_v4().to_string());
    let dataset = run_root.join("images");
    let output = run_root.join("output");
    fs::create_dir_all(&dataset).map_err(|e| e.to_string())?;
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut image_count = 0usize;
    for entry in fs::read_dir(&source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let ext = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") {
            continue;
        }
        if image_count >= MAX_TRAINING_IMAGES {
            return Err("Character Studio accepts at most 128 photos per run".into());
        }
        let stem = format!("image-{:04}", image_count + 1);
        fs::copy(&path, dataset.join(format!("{stem}.{ext}"))).map_err(|e| e.to_string())?;
        let source_caption = path.with_extension("txt");
        let caption = if source_caption.is_file() {
            fs::read_to_string(source_caption).unwrap_or_default()
        } else {
            format!(
                "{}, portrait photograph of the same person",
                request.trigger_word.trim()
            )
        };
        fs::write(dataset.join(format!("{stem}.txt")), caption).map_err(|e| e.to_string())?;
        image_count += 1;
    }
    if image_count < 3 {
        return Err("Character Studio needs at least 3 photos".into());
    }
    let rank = request.rank;
    let targets = ["wq", "wk", "wv", "wo", "gate", "up", "down"].iter().map(|name| {
        let prefix = if ["wq", "wk", "wv", "wo"].contains(name) { "blocks.{block}.attn" } else { "blocks.{block}.mlp" };
        json!({"module_path": format!("{prefix}.{name}"), "blocks": {"start": 0, "end": 28}, "rank": rank})
    }).collect::<Vec<_>>();
    let config = json!({
        "model": "krea-2-raw", "model_path": base_model, "data": dataset,
        "seed": 42, "steps": 20, "guidance": 0.0, "quantize": 8, "low_ram": true,
        "max_resolution": request.max_resolution,
        "training_loop": {"num_epochs": request.epochs, "batch_size": 1},
        "optimizer": {"name": "AdamW", "learning_rate": 0.0002},
        "checkpoint": {"output_path": output, "save_frequency": (request.epochs / 4).max(1)},
        "lora_layers": {"targets": targets}
    });
    let config_path = run_root.join("train.json");
    fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    let log_path = run_root.join("training.log");
    let log = File::create(&log_path).map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let status = Command::new(&install.python_executable)
        .args(["-B", "-m", "mflux.models.common.cli.train", "--config"])
        .arg(&config_path)
        .current_dir(&run_root)
        .env("HF_HUB_OFFLINE", "1")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err))
        .status()
        .map_err(|e| format!("Failed to start MFLUX trainer: {e}"))?;
    let log_tail = tail_text(&log_path, 16 * 1024);
    if !status.success() {
        return Err(format!(
            "MFLUX training failed. Last log output:\n{log_tail}"
        ));
    }
    let trained = newest_safetensors(&output)
        .ok_or("Training completed but produced no .safetensors LoRA")?;
    let lora_dir = app_data.join("studio-v1").join("trained-loras");
    fs::create_dir_all(&lora_dir).map_err(|e| e.to_string())?;
    let destination = lora_dir.join(format!("{}.safetensors", safe_leaf(&request.name)));
    fs::copy(trained, &destination).map_err(|e| e.to_string())?;
    Ok(CharacterTrainingResult {
        lora_path: destination.to_string_lossy().into_owned(),
        image_count,
        log_tail,
    })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn train_character_impl(
    _app: &AppHandle,
    _request: CharacterTrainingRequest,
) -> Result<CharacterTrainingResult, String> {
    Err("Character Studio training currently requires Apple silicon and the verified MFLUX Image Runtime".into())
}

#[tauri::command]
pub async fn character_training_start(
    app: AppHandle,
    request: CharacterTrainingRequest,
) -> Result<CharacterTrainingResult, String> {
    tauri::async_runtime::spawn_blocking(move || train_character_impl(&app, request))
        .await
        .map_err(|e| e.to_string())?
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowUpload {
    pub name: String,
    pub data_base64: String,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioWorkflowRequest {
    pub kind: String,
    pub base_url: String,
    pub workflow: Value,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
    pub input_image: Option<WorkflowUpload>,
    pub input_video: Option<WorkflowUpload>,
}

#[derive(Clone, Debug, Deserialize)]
struct ComfyPromptResponse {
    prompt_id: String,
}
#[derive(Clone, Debug, Deserialize)]
struct ComfyFile {
    filename: String,
    #[serde(default)]
    subfolder: String,
    #[serde(default, rename = "type")]
    file_type: String,
}

fn workflow_client() -> Result<reqwest::Client, String> {
    crate::egress::hardened()
        .timeout(WORKFLOW_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn validate_comfy_base(base: &str) -> Result<Url, String> {
    let url = Url::parse(base).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("ComfyUI endpoint must use http or https".into());
    }
    Ok(url)
}

async fn upload_comfy(
    client: &reqwest::Client,
    base: &Url,
    upload: &WorkflowUpload,
) -> Result<String, String> {
    let bytes = STANDARD
        .decode(&upload.data_base64)
        .map_err(|_| "Invalid media base64")?;
    if bytes.len() > MAX_WORKFLOW_MEDIA_BYTES {
        return Err("Workflow input exceeds 256 MiB".into());
    }
    let name = safe_leaf(&upload.name);
    let part = reqwest::multipart::Part::bytes(bytes)
        .file_name(name.clone())
        .mime_str("application/octet-stream")
        .map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new()
        .part("image", part)
        .text("type", "input")
        .text("overwrite", "true");
    let response = crate::egress::send(
        client
            .post(base.join("upload/image").map_err(|e| e.to_string())?)
            .multipart(form),
    )
    .await
    .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "ComfyUI upload returned HTTP {}",
            response.status()
        ));
    }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or(&name)
        .to_string())
}

fn replace_workflow(value: &mut Value, values: &BTreeMap<String, Value>) {
    match value {
        Value::String(text) => {
            if let Some(key) = text.strip_prefix("{{").and_then(|v| v.strip_suffix("}}")) {
                if let Some(replacement) = values.get(key) {
                    *value = replacement.clone();
                    return;
                }
            }
            let mut rendered = text.clone();
            for (key, replacement) in values {
                if let Some(s) = replacement.as_str() {
                    rendered = rendered.replace(&format!("{{{{{key}}}}}"), s);
                }
            }
            *text = rendered;
        }
        Value::Array(items) => {
            for item in items {
                replace_workflow(item, values);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                replace_workflow(item, values);
            }
        }
        _ => {}
    }
}

fn comfy_files(value: &Value) -> Vec<ComfyFile> {
    let mut files = Vec::new();
    let Some(outputs) = value.get("outputs").and_then(Value::as_object) else {
        return files;
    };
    for output in outputs.values() {
        let Some(object) = output.as_object() else {
            continue;
        };
        for key in [
            "images", "gifs", "videos", "video", "audio", "audios", "files",
        ] {
            for item in object
                .get(key)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Ok(file) = serde_json::from_value::<ComfyFile>(item.clone()) {
                    files.push(file);
                }
            }
        }
    }
    files
}

fn mime_for(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        _ => "application/octet-stream",
    }
}

fn gallery_paths(app: &AppHandle) -> Result<(PathBuf, ArtifactStore), String> {
    let base = app.profile_data_dir().map_err(|e| e.to_string())?;
    let studio = base.join("studio-v1");
    fs::create_dir_all(&studio).map_err(|e| e.to_string())?;
    let store =
        ArtifactStore::with_max_blob_size(base.join("content-v1"), MAX_WORKFLOW_MEDIA_BYTES as u64)
            .map_err(|e| e.to_string())?;
    Ok((studio.join("studio-gallery.json"), store))
}

fn append_gallery(app: &AppHandle, new_entries: &[GenerationEntry]) -> Result<(), String> {
    let (gallery_path, _) = gallery_paths(app)?;
    let mut entries: Vec<GenerationEntry> = fs::read(&gallery_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    entries.extend_from_slice(new_entries);
    if entries.len() > 1000 {
        entries.drain(0..entries.len() - 1000);
    }
    let temp = gallery_path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    fs::write(
        &temp,
        serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    fs::rename(temp, gallery_path).map_err(|e| e.to_string())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|v| v.as_millis() as u64)
        .unwrap_or(0)
}

#[tauri::command]
pub async fn studio_workflow_run(
    app: AppHandle,
    request: StudioWorkflowRequest,
) -> Result<Vec<GenerationEntry>, String> {
    if serde_json::to_vec(&request.workflow)
        .map_err(|e| e.to_string())?
        .len()
        > MAX_WORKFLOW_BYTES
    {
        return Err("Workflow JSON exceeds 4 MiB".into());
    }
    let base = validate_comfy_base(&request.base_url)?;
    let client = workflow_client()?;
    let mut values = request.values.clone();
    if let Some(image) = &request.input_image {
        values.insert(
            "input_image".into(),
            Value::String(upload_comfy(&client, &base, image).await?),
        );
    }
    if let Some(video) = &request.input_video {
        values.insert(
            "input_video".into(),
            Value::String(upload_comfy(&client, &base, video).await?),
        );
    }
    let mut workflow = request.workflow.clone();
    replace_workflow(&mut workflow, &values);
    let response = crate::egress::send(
        client
            .post(base.join("prompt").map_err(|e| e.to_string())?)
            .json(&json!({"prompt": workflow})),
    )
    .await
    .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!(
            "ComfyUI rejected the workflow with HTTP {}",
            response.status()
        ));
    }
    let submitted: ComfyPromptResponse = response.json().await.map_err(|e| e.to_string())?;
    let history = loop {
        tokio::time::sleep(Duration::from_millis(750)).await;
        let response = crate::egress::send(
            client.get(
                base.join(&format!("history/{}", submitted.prompt_id))
                    .map_err(|e| e.to_string())?,
            ),
        )
        .await
        .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            continue;
        }
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        if let Some(entry) = value.get(&submitted.prompt_id) {
            break entry.clone();
        }
    };
    let files = comfy_files(&history);
    if files.is_empty() {
        return Err("ComfyUI completed without a discoverable image/video/audio output".into());
    }
    let (_, store) = gallery_paths(&app)?;
    let task = match request.kind.as_str() {
        "talking_character" | "motion_control" | "extend_video" => GenerationTask::ImageToVideo,
        _ => GenerationTask::TextToVideo,
    };
    let prompt = values
        .get("prompt")
        .and_then(Value::as_str)
        .or_else(|| values.get("script").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    let mut entries = Vec::new();
    for file in files.into_iter().take(8) {
        let mut url = base.join("view").map_err(|e| e.to_string())?;
        url.query_pairs_mut()
            .append_pair("filename", &file.filename)
            .append_pair("subfolder", &file.subfolder)
            .append_pair(
                "type",
                if file.file_type.is_empty() {
                    "output"
                } else {
                    &file.file_type
                },
            );
        let response = crate::egress::send(client.get(url))
            .await
            .map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("Failed to read ComfyUI output {}", file.filename));
        }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_WORKFLOW_MEDIA_BYTES {
            return Err("ComfyUI output exceeds 256 MiB".into());
        }
        let blob = store.put(&bytes).map_err(|e| e.to_string())?;
        entries.push(GenerationEntry {
            entry_id: format!("studio-{}", Uuid::new_v4()),
            artifact_id: blob.id,
            model_id: format!("comfy-workflow:{}", request.kind),
            task,
            prompt: prompt.clone(),
            negative_prompt: String::new(),
            media_type: mime_for(&file.filename).into(),
            size_bytes: blob.size,
            width: 0,
            height: 0,
            steps: 0,
            cfg_scale: 0.0,
            seed: -1,
            frame_count: 0,
            fps: 0,
            duration_ms: 0,
            created_at_ms: now_ms(),
        });
    }
    append_gallery(&app, &entries)?;
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn workflow_replacement_preserves_typed_values() {
        let mut value = json!({"a":"{{steps}}", "b":"prefix {{prompt}}"});
        let values = BTreeMap::from([
            ("steps".into(), json!(12)),
            ("prompt".into(), json!("hello")),
        ]);
        replace_workflow(&mut value, &values);
        assert_eq!(value["a"], 12);
        assert_eq!(value["b"], "prefix hello");
    }
    #[test]
    fn sha_requires_exact_hex() {
        assert!(normalize_sha(&"a".repeat(64)).is_some());
        assert!(normalize_sha("nope").is_none());
    }
}

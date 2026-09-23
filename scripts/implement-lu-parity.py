from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content, encoding="utf-8")


def replace(path: str, old: str, new: str) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    if old not in text:
        raise RuntimeError(f"anchor not found in {path}: {old[:120]!r}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


write("src-tauri/src/studio_parity.rs", r'''//! Creator-parity surfaces that deliberately reuse Little Monkey's existing
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
    response.json::<Value>().await.map_err(|error| error.to_string())
}

fn normalize_sha(value: &str) -> Option<String> {
    let lowered = value.trim().to_ascii_lowercase();
    (lowered.len() == 64 && lowered.chars().all(|ch| ch.is_ascii_hexdigit())).then_some(lowered)
}

fn family_from(name: &str, tags: &[String]) -> String {
    let haystack = format!("{} {}", name, tags.join(" ")).to_ascii_lowercase();
    if haystack.contains("qwen") { "Qwen".into() }
    else if haystack.contains("flux") { "FLUX".into() }
    else if haystack.contains("wan") { "Wan".into() }
    else if haystack.contains("ltx") { "LTX".into() }
    else if haystack.contains("sdxl") { "SDXL".into() }
    else { "Community".into() }
}

fn infer_asset_kind(name: &str, declared: Option<&str>, tags: &[String]) -> String {
    let all = format!("{} {} {}", name, declared.unwrap_or_default(), tags.join(" ")).to_ascii_lowercase();
    if all.contains("lora") || all.contains("lycoris") { "lora".into() } else { "model".into() }
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

async fn search_civitai(query: &str, asset_kind: Option<&str>) -> Result<Vec<DiscoveryItem>, String> {
    let mut url = Url::parse("https://civitai.com/api/v1/models").unwrap();
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("query", query).append_pair("limit", "24").append_pair("sort", "Highest Rated");
        if asset_kind == Some("lora") { pairs.append_pair("types", "LORA"); }
        if asset_kind == Some("model") { pairs.append_pair("types", "Checkpoint"); }
    }
    let payload = get_json(url).await?;
    let mut results = Vec::new();
    for model in payload.get("items").and_then(Value::as_array).into_iter().flatten() {
        let model_name = model.get("name").and_then(Value::as_str).unwrap_or("Community model");
        let model_type = model.get("type").and_then(Value::as_str);
        let tags: Vec<String> = model.get("tags").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_string).collect();
        let Some(version) = model.get("modelVersions").and_then(Value::as_array).and_then(|v| v.first()) else { continue; };
        let version_id = version.get("id").and_then(Value::as_u64).unwrap_or(0);
        for file in version.get("files").and_then(Value::as_array).into_iter().flatten() {
            let file_name = file.get("name").and_then(Value::as_str).unwrap_or("");
            if !(file_name.ends_with(".safetensors") || file_name.ends_with(".gguf")) { continue; }
            let download_url = file.get("downloadUrl").and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("https://civitai.com/api/download/models/{version_id}"));
            let sha256 = file.get("hashes").and_then(|v| v.get("SHA256")).and_then(Value::as_str).and_then(normalize_sha);
            let size_bytes = file.get("sizeKB").and_then(Value::as_f64).map(|v| (v * 1024.0) as u64).unwrap_or(0);
            let kind = infer_asset_kind(model_name, model_type, &tags);
            if asset_kind.is_some_and(|wanted| wanted != kind) { continue; }
            results.push(DiscoveryItem {
                id: format!("civitai:{version_id}:{file_name}"),
                source: "civitai".into(), name: model_name.into(), asset_kind: kind,
                family: family_from(model_name, &tags), repo: None, file_name: file_name.into(),
                download_url, page_url: format!("https://civitai.com/models/{}", model.get("id").and_then(Value::as_u64).unwrap_or(0)),
                sha256, size_bytes, tags: tags.clone(),
            });
            if results.len() >= MAX_DISCOVERY_RESULTS { return Ok(results); }
        }
    }
    Ok(results)
}

async fn search_hugging_face(query: &str, asset_kind: Option<&str>) -> Result<Vec<DiscoveryItem>, String> {
    let mut url = Url::parse("https://huggingface.co/api/models").unwrap();
    url.query_pairs_mut().append_pair("search", query).append_pair("limit", "24").append_pair("full", "true");
    let payload = get_json(url).await?;
    let Some(models) = payload.as_array() else { return Err("Unexpected Hugging Face response".into()); };
    let mut results = Vec::new();
    for model in models {
        let Some(repo) = model.get("id").and_then(Value::as_str) else { continue; };
        let tags: Vec<String> = model.get("tags").and_then(Value::as_array).into_iter().flatten()
            .filter_map(Value::as_str).map(str::to_string).collect();
        for sibling in model.get("siblings").and_then(Value::as_array).into_iter().flatten() {
            let Some(file_name) = sibling.get("rfilename").and_then(Value::as_str) else { continue; };
            if !(file_name.ends_with(".safetensors") || file_name.ends_with(".gguf")) { continue; }
            let sha256 = sibling.get("lfs").and_then(|v| v.get("oid")).and_then(Value::as_str).and_then(normalize_sha);
            let size_bytes = sibling.get("lfs").and_then(|v| v.get("size")).and_then(Value::as_u64)
                .or_else(|| sibling.get("size").and_then(Value::as_u64)).unwrap_or(0);
            let kind = infer_asset_kind(file_name, None, &tags);
            if asset_kind.is_some_and(|wanted| wanted != kind) { continue; }
            let mut download = Url::parse("https://huggingface.co/").unwrap();
            {
                let mut segments = download.path_segments_mut().map_err(|_| "Invalid Hugging Face base URL")?;
                for part in repo.split('/') { segments.push(part); }
                segments.push("resolve").push("main");
                for part in file_name.split('/') { segments.push(part); }
            }
            results.push(DiscoveryItem {
                id: format!("hf:{repo}:{file_name}"), source: "hugging_face".into(), name: repo.into(),
                asset_kind: kind, family: family_from(repo, &tags), repo: Some(repo.into()), file_name: file_name.into(),
                download_url: download.to_string(), page_url: format!("https://huggingface.co/{repo}"),
                sha256, size_bytes, tags: tags.clone(),
            });
            if results.len() >= MAX_DISCOVERY_RESULTS { return Ok(results); }
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
pub struct DiscoveryDownloadResult { pub path: String, pub size_bytes: u64, pub sha256: String }

fn safe_leaf(value: &str) -> String {
    let leaf = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let safe: String = leaf.chars().map(|ch| if ch.is_ascii_alphanumeric() || ".-_".contains(ch) { ch } else { '_' }).collect();
    if safe.is_empty() { "asset.bin".into() } else { safe }
}

#[tauri::command]
pub async fn studio_discovery_download(app: AppHandle, request: DiscoveryDownloadRequest) -> Result<DiscoveryDownloadResult, String> {
    let expected = normalize_sha(&request.sha256).ok_or("One-click install requires a published SHA-256")?;
    if request.size_bytes > MAX_DOWNLOAD_BYTES { return Err("The selected asset exceeds the 8 GiB discovery-download limit".into()); }
    let mut url = Url::parse(&request.download_url).map_err(|e| e.to_string())?;
    validate_public_https(&url)?;
    let base = app.profile_data_dir().map_err(|e| e.to_string())?.join("studio-v1").join("community-assets").join(&request.asset_kind);
    fs::create_dir_all(&base).map_err(|e| e.to_string())?;
    let destination = base.join(format!("{}-{}", &expected[..12], safe_leaf(&request.file_name)));
    let temporary = destination.with_extension(format!("{}.part", Uuid::new_v4()));
    let mut redirects = 0;
    let mut response = loop {
        let response = crate::egress::send(public_client()?.get(url.clone())).await.map_err(|e| e.to_string())?;
        if response.status().is_redirection() {
            if redirects >= 6 { return Err("Too many download redirects".into()); }
            let location = response.headers().get(reqwest::header::LOCATION).and_then(|v| v.to_str().ok()).ok_or("Redirect has no Location")?;
            url = url.join(location).map_err(|e| e.to_string())?;
            validate_public_https(&url)?;
            redirects += 1;
            continue;
        }
        if !response.status().is_success() { return Err(format!("Download returned HTTP {}", response.status())); }
        break response;
    };
    if let Some(length) = response.content_length() {
        if length > MAX_DOWNLOAD_BYTES { return Err("Download exceeds the 8 GiB limit".into()); }
        if request.size_bytes > 0 && length.abs_diff(request.size_bytes) > request.size_bytes / 5 + 1024 * 1024 {
            return Err("Download size differs materially from catalog metadata".into());
        }
    }
    let mut file = File::create(&temporary).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut written = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        written = written.saturating_add(chunk.len() as u64);
        if written > MAX_DOWNLOAD_BYTES { let _ = fs::remove_file(&temporary); return Err("Download exceeded the 8 GiB limit".into()); }
        hasher.update(&chunk);
        file.write_all(&chunk).map_err(|e| e.to_string())?;
    }
    file.flush().map_err(|e| e.to_string())?;
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected { let _ = fs::remove_file(&temporary); return Err(format!("SHA-256 mismatch: expected {expected}, got {actual}")); }
    fs::rename(&temporary, &destination).map_err(|e| e.to_string())?;
    Ok(DiscoveryDownloadResult { path: destination.to_string_lossy().into_owned(), size_bytes: written, sha256: actual })
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportDocument { pub title: String, pub markdown: String }

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatImportResult { pub path: String, pub document_count: usize }

#[tauri::command]
pub fn chat_export_write_documents(app: AppHandle, source_name: String, documents: Vec<ChatImportDocument>) -> Result<ChatImportResult, String> {
    if documents.is_empty() || documents.len() > MAX_IMPORT_DOCUMENTS { return Err("Import must contain between 1 and 20,000 conversations".into()); }
    let mut total = 0usize;
    for doc in &documents {
        if doc.markdown.len() > MAX_IMPORT_DOC_BYTES { return Err(format!("Conversation '{}' exceeds 5 MiB", doc.title)); }
        total = total.saturating_add(doc.markdown.len());
        if total > MAX_IMPORT_TOTAL_BYTES { return Err("Chat export exceeds the 256 MiB import limit".into()); }
    }
    let root = app.profile_data_dir().map_err(|e| e.to_string())?.join("knowledge-imports")
        .join(format!("{}-{}", safe_leaf(&source_name), Uuid::new_v4()));
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    for (index, doc) in documents.iter().enumerate() {
        let path = root.join(format!("{:05}-{}.md", index + 1, safe_leaf(&doc.title)));
        fs::write(path, format!("# {}\n\n{}\n", doc.title.trim(), doc.markdown)).map_err(|e| e.to_string())?;
    }
    Ok(ChatImportResult { path: root.to_string_lossy().into_owned(), document_count: documents.len() })
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
pub struct CharacterTrainingResult { pub lora_path: String, pub image_count: usize, pub log_tail: String }

fn tail_text(path: &Path, max: usize) -> String {
    let Ok(bytes) = fs::read(path) else { return String::new(); };
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn newest_safetensors(root: &Path) -> Option<PathBuf> {
    let mut candidates = Vec::new();
    for entry in walkdir::WalkDir::new(root).max_depth(5).into_iter().flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|v| v.to_str()) == Some("safetensors") {
            let modified = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
            candidates.push((path.to_path_buf(), modified, path.file_name().and_then(|v| v.to_str()).unwrap_or("").contains("lora")));
        }
    }
    candidates.sort_by_key(|(_, modified, lora)| (*lora, *modified));
    candidates.pop().map(|item| item.0)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn train_character_impl(app: &AppHandle, request: CharacterTrainingRequest) -> Result<CharacterTrainingResult, String> {
    if request.name.trim().is_empty() || request.name.len() > 80 { return Err("Character name must be 1–80 characters".into()); }
    if request.trigger_word.trim().is_empty() || request.trigger_word.len() > 80 { return Err("Trigger word must be 1–80 characters".into()); }
    if !(1..=500).contains(&request.epochs) || !(4..=64).contains(&request.rank) || !(256..=1536).contains(&request.max_resolution) {
        return Err("Training settings are outside their supported bounds".into());
    }
    let source = PathBuf::from(&request.source_dir).canonicalize().map_err(|e| format!("Invalid photo folder: {e}"))?;
    let base_model = PathBuf::from(&request.base_model_path).canonicalize().map_err(|e| format!("Invalid base-model folder: {e}"))?;
    if !source.is_dir() || !base_model.is_dir() { return Err("Photos and base model must both be directories".into()); }
    let app_data = app.profile_data_dir().map_err(|e| e.to_string())?;
    let installer = crate::m3_production::production_mflux_installer(&app_data.join("m3")).map_err(|e| e.to_string())?;
    let install = installer.verify_active().map_err(|e| format!("Install the verified MFLUX Image Runtime before training: {e}"))?;
    let run_root = app_data.join("studio-v1").join("character-training").join(Uuid::new_v4().to_string());
    let dataset = run_root.join("images");
    let output = run_root.join("output");
    fs::create_dir_all(&dataset).map_err(|e| e.to_string())?;
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;
    let mut image_count = 0usize;
    for entry in fs::read_dir(&source).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        let ext = path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase();
        if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp") { continue; }
        if image_count >= MAX_TRAINING_IMAGES { return Err("Character Studio accepts at most 128 photos per run".into()); }
        let stem = format!("image-{:04}", image_count + 1);
        fs::copy(&path, dataset.join(format!("{stem}.{ext}"))).map_err(|e| e.to_string())?;
        let source_caption = path.with_extension("txt");
        let caption = if source_caption.is_file() { fs::read_to_string(source_caption).unwrap_or_default() }
            else { format!("{}, portrait photograph of the same person", request.trigger_word.trim()) };
        fs::write(dataset.join(format!("{stem}.txt")), caption).map_err(|e| e.to_string())?;
        image_count += 1;
    }
    if image_count < 3 { return Err("Character Studio needs at least 3 photos".into()); }
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
    fs::write(&config_path, serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let log_path = run_root.join("training.log");
    let log = File::create(&log_path).map_err(|e| e.to_string())?;
    let err = log.try_clone().map_err(|e| e.to_string())?;
    let status = Command::new(&install.python_executable)
        .args(["-B", "-m", "mflux.models.common.cli.train", "--config"])
        .arg(&config_path).current_dir(&run_root)
        .env("HF_HUB_OFFLINE", "1").stdout(Stdio::from(log)).stderr(Stdio::from(err))
        .status().map_err(|e| format!("Failed to start MFLUX trainer: {e}"))?;
    let log_tail = tail_text(&log_path, 16 * 1024);
    if !status.success() { return Err(format!("MFLUX training failed. Last log output:\n{log_tail}")); }
    let trained = newest_safetensors(&output).ok_or("Training completed but produced no .safetensors LoRA")?;
    let lora_dir = app_data.join("studio-v1").join("trained-loras");
    fs::create_dir_all(&lora_dir).map_err(|e| e.to_string())?;
    let destination = lora_dir.join(format!("{}.safetensors", safe_leaf(&request.name)));
    fs::copy(trained, &destination).map_err(|e| e.to_string())?;
    Ok(CharacterTrainingResult { lora_path: destination.to_string_lossy().into_owned(), image_count, log_tail })
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn train_character_impl(_app: &AppHandle, _request: CharacterTrainingRequest) -> Result<CharacterTrainingResult, String> {
    Err("Character Studio training currently requires Apple silicon and the verified MFLUX Image Runtime".into())
}

#[tauri::command]
pub async fn character_training_start(app: AppHandle, request: CharacterTrainingRequest) -> Result<CharacterTrainingResult, String> {
    tauri::async_runtime::spawn_blocking(move || train_character_impl(&app, request)).await.map_err(|e| e.to_string())?
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowUpload { pub name: String, pub data_base64: String }

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioWorkflowRequest {
    pub kind: String,
    pub base_url: String,
    pub workflow: Value,
    #[serde(default)] pub values: BTreeMap<String, Value>,
    pub input_image: Option<WorkflowUpload>,
    pub input_video: Option<WorkflowUpload>,
}

#[derive(Clone, Debug, Deserialize)]
struct ComfyPromptResponse { prompt_id: String }
#[derive(Clone, Debug, Deserialize)]
struct ComfyFile { filename: String, #[serde(default)] subfolder: String, #[serde(default, rename="type")] file_type: String }

fn workflow_client() -> Result<reqwest::Client, String> {
    crate::egress::hardened().timeout(WORKFLOW_TIMEOUT).build().map_err(|e| e.to_string())
}

fn validate_comfy_base(base: &str) -> Result<Url, String> {
    let url = Url::parse(base).map_err(|e| e.to_string())?;
    if !matches!(url.scheme(), "http" | "https") { return Err("ComfyUI endpoint must use http or https".into()); }
    Ok(url)
}

async fn upload_comfy(client: &reqwest::Client, base: &Url, upload: &WorkflowUpload) -> Result<String, String> {
    let bytes = STANDARD.decode(&upload.data_base64).map_err(|_| "Invalid media base64")?;
    if bytes.len() > MAX_WORKFLOW_MEDIA_BYTES { return Err("Workflow input exceeds 256 MiB".into()); }
    let name = safe_leaf(&upload.name);
    let part = reqwest::multipart::Part::bytes(bytes).file_name(name.clone()).mime_str("application/octet-stream").map_err(|e| e.to_string())?;
    let form = reqwest::multipart::Form::new().part("image", part).text("type", "input").text("overwrite", "true");
    let response = crate::egress::send(client.post(base.join("upload/image").map_err(|e| e.to_string())?).multipart(form)).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err(format!("ComfyUI upload returned HTTP {}", response.status())); }
    let body: Value = response.json().await.map_err(|e| e.to_string())?;
    Ok(body.get("name").and_then(Value::as_str).unwrap_or(&name).to_string())
}

fn replace_workflow(value: &mut Value, values: &BTreeMap<String, Value>) {
    match value {
        Value::String(text) => {
            if let Some(key) = text.strip_prefix("{{").and_then(|v| v.strip_suffix("}}")) {
                if let Some(replacement) = values.get(key) { *value = replacement.clone(); return; }
            }
            let mut rendered = text.clone();
            for (key, replacement) in values {
                if let Some(s) = replacement.as_str() { rendered = rendered.replace(&format!("{{{{{key}}}}}"), s); }
            }
            *text = rendered;
        }
        Value::Array(items) => for item in items { replace_workflow(item, values); },
        Value::Object(map) => for item in map.values_mut() { replace_workflow(item, values); },
        _ => {}
    }
}

fn comfy_files(value: &Value) -> Vec<ComfyFile> {
    let mut files = Vec::new();
    let Some(outputs) = value.get("outputs").and_then(Value::as_object) else { return files; };
    for output in outputs.values() {
        let Some(object) = output.as_object() else { continue; };
        for key in ["images", "gifs", "videos", "video", "audio", "audios", "files"] {
            for item in object.get(key).and_then(Value::as_array).into_iter().flatten() {
                if let Ok(file) = serde_json::from_value::<ComfyFile>(item.clone()) { files.push(file); }
            }
        }
    }
    files
}

fn mime_for(name: &str) -> &'static str {
    match Path::new(name).extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "webp" => "image/webp", "gif" => "image/gif",
        "mp4" => "video/mp4", "webm" => "video/webm", "wav" => "audio/wav", "mp3" => "audio/mpeg",
        "flac" => "audio/flac", "ogg" => "audio/ogg", _ => "application/octet-stream"
    }
}

fn gallery_paths(app: &AppHandle) -> Result<(PathBuf, ArtifactStore), String> {
    let base = app.profile_data_dir().map_err(|e| e.to_string())?;
    let studio = base.join("studio-v1");
    fs::create_dir_all(&studio).map_err(|e| e.to_string())?;
    let store = ArtifactStore::with_max_blob_size(base.join("content-v1"), MAX_WORKFLOW_MEDIA_BYTES as u64).map_err(|e| e.to_string())?;
    Ok((studio.join("studio-gallery.json"), store))
}

fn append_gallery(app: &AppHandle, new_entries: &[GenerationEntry]) -> Result<(), String> {
    let (gallery_path, _) = gallery_paths(app)?;
    let mut entries: Vec<GenerationEntry> = fs::read(&gallery_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    entries.extend_from_slice(new_entries);
    if entries.len() > 1000 { entries.drain(0..entries.len() - 1000); }
    let temp = gallery_path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    fs::write(&temp, serde_json::to_vec_pretty(&entries).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    fs::rename(temp, gallery_path).map_err(|e| e.to_string())
}

fn now_ms() -> u64 { SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|v| v.as_millis() as u64).unwrap_or(0) }

#[tauri::command]
pub async fn studio_workflow_run(app: AppHandle, request: StudioWorkflowRequest) -> Result<Vec<GenerationEntry>, String> {
    if serde_json::to_vec(&request.workflow).map_err(|e| e.to_string())?.len() > MAX_WORKFLOW_BYTES { return Err("Workflow JSON exceeds 4 MiB".into()); }
    let base = validate_comfy_base(&request.base_url)?;
    let client = workflow_client()?;
    let mut values = request.values.clone();
    if let Some(image) = &request.input_image { values.insert("input_image".into(), Value::String(upload_comfy(&client, &base, image).await?)); }
    if let Some(video) = &request.input_video { values.insert("input_video".into(), Value::String(upload_comfy(&client, &base, video).await?)); }
    let mut workflow = request.workflow.clone();
    replace_workflow(&mut workflow, &values);
    let response = crate::egress::send(client.post(base.join("prompt").map_err(|e| e.to_string())?).json(&json!({"prompt": workflow}))).await.map_err(|e| e.to_string())?;
    if !response.status().is_success() { return Err(format!("ComfyUI rejected the workflow with HTTP {}", response.status())); }
    let submitted: ComfyPromptResponse = response.json().await.map_err(|e| e.to_string())?;
    let history = loop {
        tokio::time::sleep(Duration::from_millis(750)).await;
        let response = crate::egress::send(client.get(base.join(&format!("history/{}", submitted.prompt_id)).map_err(|e| e.to_string())?)).await.map_err(|e| e.to_string())?;
        if !response.status().is_success() { continue; }
        let value: Value = response.json().await.map_err(|e| e.to_string())?;
        if let Some(entry) = value.get(&submitted.prompt_id) { break entry.clone(); }
    };
    let files = comfy_files(&history);
    if files.is_empty() { return Err("ComfyUI completed without a discoverable image/video/audio output".into()); }
    let (_, store) = gallery_paths(&app)?;
    let task = match request.kind.as_str() {
        "talking_character" | "motion_control" | "extend_video" => GenerationTask::ImageToVideo,
        _ => GenerationTask::TextToVideo,
    };
    let prompt = values.get("prompt").and_then(Value::as_str).or_else(|| values.get("script").and_then(Value::as_str)).unwrap_or("").to_string();
    let mut entries = Vec::new();
    for file in files.into_iter().take(8) {
        let mut url = base.join("view").map_err(|e| e.to_string())?;
        url.query_pairs_mut().append_pair("filename", &file.filename).append_pair("subfolder", &file.subfolder).append_pair("type", if file.file_type.is_empty() { "output" } else { &file.file_type });
        let response = crate::egress::send(client.get(url)).await.map_err(|e| e.to_string())?;
        if !response.status().is_success() { return Err(format!("Failed to read ComfyUI output {}", file.filename)); }
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        if bytes.len() > MAX_WORKFLOW_MEDIA_BYTES { return Err("ComfyUI output exceeds 256 MiB".into()); }
        let blob = store.put(&bytes).map_err(|e| e.to_string())?;
        entries.push(GenerationEntry {
            entry_id: format!("studio-{}", Uuid::new_v4()), artifact_id: blob.id,
            model_id: format!("comfy-workflow:{}", request.kind), task, prompt: prompt.clone(), negative_prompt: String::new(),
            media_type: mime_for(&file.filename).into(), size_bytes: blob.size, width: 0, height: 0, steps: 0, cfg_scale: 0.0,
            seed: -1, frame_count: 0, fps: 0, duration_ms: 0, created_at_ms: now_ms(),
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
        let values = BTreeMap::from([("steps".into(), json!(12)), ("prompt".into(), json!("hello"))]);
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
''')

write("src/lib/chatExportImport.ts", r'''import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile } from "@tauri-apps/plugin-fs";

export interface ImportedConversation { title: string; markdown: string }
export interface ChatImportResult { path: string; documentCount: number }

type UnknownRecord = Record<string, unknown>;
const record = (value: unknown): UnknownRecord | null => typeof value === "object" && value !== null && !Array.isArray(value) ? value as UnknownRecord : null;
const text = (value: unknown): string => typeof value === "string" ? value : "";
const array = (value: unknown): unknown[] => Array.isArray(value) ? value : [];

function messageText(message: UnknownRecord): string {
  const content = record(message.content);
  const parts = content ? array(content.parts) : [];
  if (parts.length) return parts.map((part) => typeof part === "string" ? part : text(record(part)?.text)).filter(Boolean).join("\n");
  const direct = message.text;
  if (typeof direct === "string") return direct;
  if (Array.isArray(direct)) return direct.map((part) => text(record(part)?.text) || text(part)).filter(Boolean).join("\n");
  return text(content?.text);
}

function chatGptConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const mapping = record(raw.mapping);
  if (!mapping) return null;
  const rows: Array<{ role: string; body: string; time: number }> = [];
  for (const node of Object.values(mapping)) {
    const n = record(node); const message = record(n?.message); if (!message) continue;
    const author = record(message.author);
    const body = messageText(message).trim(); if (!body) continue;
    rows.push({ role: text(author?.role) || "unknown", body, time: Number(message.create_time ?? n?.create_time ?? 0) || 0 });
  }
  rows.sort((a, b) => a.time - b.time);
  if (!rows.length) return null;
  return { title: text(raw.title) || `Conversation ${index + 1}`, markdown: rows.map((row) => `## ${row.role}\n\n${row.body}`).join("\n\n") };
}

function claudeConversation(raw: UnknownRecord, index: number): ImportedConversation | null {
  const messages = array(raw.chat_messages ?? raw.messages);
  if (!messages.length) return null;
  const rows = messages.map(record).filter((m): m is UnknownRecord => !!m).map((m) => {
    const role = text(m.sender ?? m.role) || "unknown";
    let body = text(m.text);
    if (!body && Array.isArray(m.content)) body = array(m.content).map(record).filter((v): v is UnknownRecord => !!v).map((v) => text(v.text)).filter(Boolean).join("\n");
    return { role, body: body.trim() };
  }).filter((row) => row.body);
  if (!rows.length) return null;
  return { title: text(raw.name ?? raw.title) || `Conversation ${index + 1}`, markdown: rows.map((row) => `## ${row.role}\n\n${row.body}`).join("\n\n") };
}

export function parseChatExport(value: unknown): ImportedConversation[] {
  const root = record(value);
  const candidates = Array.isArray(value) ? value : array(root?.conversations ?? root?.chats ?? root?.items);
  const output: ImportedConversation[] = [];
  candidates.forEach((item, index) => {
    const row = record(item); if (!row) return;
    const parsed = row.mapping ? chatGptConversation(row, index) : claudeConversation(row, index);
    if (parsed) output.push(parsed);
  });
  if (!output.length && root) {
    const single = root.mapping ? chatGptConversation(root, 0) : claudeConversation(root, 0);
    if (single) output.push(single);
  }
  return output;
}

async function inflateRaw(bytes: Uint8Array): Promise<Uint8Array> {
  const stream = new Blob([bytes]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  return new Uint8Array(await new Response(stream).arrayBuffer());
}

async function jsonFromZip(bytes: Uint8Array): Promise<unknown> {
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  let eocd = -1;
  for (let offset = bytes.length - 22; offset >= Math.max(0, bytes.length - 65557); offset--) {
    if (view.getUint32(offset, true) === 0x06054b50) { eocd = offset; break; }
  }
  if (eocd < 0) throw new Error("ZIP end-of-directory record not found");
  const count = view.getUint16(eocd + 10, true);
  let cursor = view.getUint32(eocd + 16, true);
  const decoder = new TextDecoder();
  for (let i = 0; i < count; i++) {
    if (view.getUint32(cursor, true) !== 0x02014b50) throw new Error("Invalid ZIP central directory");
    const method = view.getUint16(cursor + 10, true);
    const compressedSize = view.getUint32(cursor + 20, true);
    const nameLength = view.getUint16(cursor + 28, true);
    const extraLength = view.getUint16(cursor + 30, true);
    const commentLength = view.getUint16(cursor + 32, true);
    const localOffset = view.getUint32(cursor + 42, true);
    const name = decoder.decode(bytes.slice(cursor + 46, cursor + 46 + nameLength));
    if (/((conversations|chats).*\.json|\.json)$/i.test(name) && !name.startsWith("__MACOSX/")) {
      if (view.getUint32(localOffset, true) !== 0x04034b50) throw new Error("Invalid ZIP local header");
      const localNameLength = view.getUint16(localOffset + 26, true);
      const localExtraLength = view.getUint16(localOffset + 28, true);
      const dataStart = localOffset + 30 + localNameLength + localExtraLength;
      const compressed = bytes.slice(dataStart, dataStart + compressedSize);
      const plain = method === 0 ? compressed : method === 8 ? await inflateRaw(compressed) : null;
      if (plain) {
        try {
          const parsed = JSON.parse(decoder.decode(plain));
          if (parseChatExport(parsed).length) return parsed;
        } catch { /* continue to another JSON member */ }
      }
    }
    cursor += 46 + nameLength + extraLength + commentLength;
  }
  throw new Error("No supported conversation JSON found in the ZIP");
}

export async function chooseAndParseChatExport(): Promise<{ sourceName: string; conversations: ImportedConversation[] } | null> {
  const picked = await open({ directory: false, multiple: false, filters: [{ name: "Conversation export", extensions: ["json", "zip"] }] });
  if (typeof picked !== "string") return null;
  const bytes = await readFile(picked);
  const parsed = picked.toLowerCase().endsWith(".zip") ? await jsonFromZip(bytes) : JSON.parse(new TextDecoder().decode(bytes));
  const conversations = parseChatExport(parsed);
  if (!conversations.length) throw new Error("No ChatGPT, Claude, or Gemini-style conversations were found");
  return { sourceName: picked.split(/[/\\]/).pop()?.replace(/\.(json|zip)$/i, "") || "chat-export", conversations };
}

export async function writeChatImport(sourceName: string, conversations: ImportedConversation[]): Promise<ChatImportResult> {
  return invoke<ChatImportResult>("chat_export_write_documents", { sourceName, documents: conversations });
}
''')

write("src/lib/chatExportImport.test.ts", r'''import { describe, expect, it } from "vitest";
import { parseChatExport } from "./chatExportImport";

describe("parseChatExport", () => {
  it("parses ChatGPT mapping exports", () => {
    const rows = parseChatExport([{ title: "Hello", mapping: { a: { message: { author: { role: "user" }, content: { parts: ["Hi"] }, create_time: 1 } }, b: { message: { author: { role: "assistant" }, content: { parts: ["Hello"] }, create_time: 2 } } } }]);
    expect(rows).toHaveLength(1);
    expect(rows[0]?.markdown).toContain("## assistant");
  });
  it("parses Claude/Gemini message arrays", () => {
    const rows = parseChatExport([{ name: "Thread", chat_messages: [{ sender: "human", text: "A" }, { sender: "assistant", text: "B" }] }]);
    expect(rows[0]?.title).toBe("Thread");
    expect(rows[0]?.markdown).toContain("## human");
  });
});
''')

write("src/components/Settings/ChatExportImportCard.tsx", r'''import { useState } from "react";
import { Upload } from "lucide-react";
import { Button } from "../ui";
import { chooseAndParseChatExport, writeChatImport } from "../../lib/chatExportImport";
import { useKnowledgeV2Store } from "../../store/knowledgeV2Store";
import { errorMessage } from "../../lib/errors";

export function ChatExportImportCard({ stackId }: { stackId: string }) {
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const addSource = useKnowledgeV2Store((state) => state.addSource);
  const refreshStack = useKnowledgeV2Store((state) => state.refreshStack);
  const run = async () => {
    if (!stackId) return;
    setBusy(true); setError(null); setStatus(null);
    try {
      const selected = await chooseAndParseChatExport();
      if (!selected) return;
      const written = await writeChatImport(selected.sourceName, selected.conversations);
      await addSource(stackId, `${selected.sourceName} (${written.documentCount} chats)`, { kind: "local_folder", path: written.path });
      await refreshStack(stackId);
      setStatus(`Imported and indexed ${written.documentCount} conversations.`);
    } catch (cause) { setError(errorMessage(cause)); }
    finally { setBusy(false); }
  };
  return <div className="mt-3 rounded-md border border-border bg-surface p-3">
    <div className="flex items-start justify-between gap-3">
      <div className="flex min-w-0 gap-2"><Upload size={15} className="mt-0.5 shrink-0 text-accent"/><div>
        <p className="text-xs font-medium text-foreground">Import AI conversation history</p>
        <p className="mt-1 text-[11px] leading-4 text-muted">Import official ChatGPT, Claude, or Gemini JSON/ZIP exports. Conversations become ordinary local Knowledge 2.0 documents and use the existing hybrid index.</p>
      </div></div>
      <Button size="sm" variant="secondary" disabled={!stackId || busy} onClick={() => void run()}>{busy ? "Importing…" : "Import export"}</Button>
    </div>
    {status && <p className="mt-2 text-[11px] text-success">{status}</p>}
    {error && <p role="alert" className="mt-2 text-[11px] text-danger">{error}</p>}
  </div>;
}
''')

write("src/lib/studioParity.ts", r'''import { invoke } from "@tauri-apps/api/core";
import type { ComponentSlot, GenerationModelSpec } from "./studioClient";

export type DiscoverySource = "civitai" | "hugging_face";
export type DiscoveryAssetKind = "model" | "lora";
export interface DiscoveryItem { id: string; source: string; name: string; assetKind: DiscoveryAssetKind; family: string; repo: string | null; fileName: string; downloadUrl: string; pageUrl: string; sha256: string | null; sizeBytes: number; tags: string[] }
export interface DiscoveryDownloadResult { path: string; sizeBytes: number; sha256: string }
export interface CharacterTrainingResult { loraPath: string; imageCount: number; logTail: string }
export interface WorkflowUpload { name: string; dataBase64: string }
export type WorkflowKind = "music" | "talking_character" | "motion_control" | "extend_video";

export const CURATED_STUDIO_PROFILES = [
  { id: "qwen-image", name: "Qwen Image 2.1", query: "Qwen Image 2.1 GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 16 * 1024 ** 3, note: "Image generation/editing; prefer a GGUF or safetensors build with explicit license metadata." },
  { id: "flux", name: "FLUX image", query: "FLUX.1 dev GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 12 * 1024 ** 3, note: "High-quality still images and broad LoRA ecosystem." },
  { id: "wan", name: "Wan video", query: "Wan 2.2 TI2V GGUF", source: "hugging_face" as const, kind: "model" as const, minRamBytes: 20 * 1024 ** 3, note: "Image/text-to-video family; exact fit depends heavily on quantization." },
  { id: "character-lora", name: "Character / style LoRAs", query: "character", source: "civitai" as const, kind: "lora" as const, minRamBytes: 0, note: "Browse verified LoRA files and add them directly to Studio." },
] as const;

export const WORKFLOW_PRESETS: Record<WorkflowKind, { label: string; description: string; placeholders: string[] }> = {
  music: { label: "Music", description: "Run an ACE-Step or compatible ComfyUI music workflow.", placeholders: ["prompt", "negative_prompt", "duration_seconds"] },
  talking_character: { label: "Talking Character", description: "Run a talking-character/lip-sync workflow with an uploaded portrait.", placeholders: ["input_image", "script", "prompt"] },
  motion_control: { label: "Motion Control", description: "Drive a video workflow from a reference image and motion prompt.", placeholders: ["input_image", "prompt", "negative_prompt"] },
  extend_video: { label: "Extend Video", description: "Upload an existing clip and run a continuation/extension workflow.", placeholders: ["input_video", "prompt", "negative_prompt"] },
};

export const studioParityClient = {
  search: (source: DiscoverySource, query: string, assetKind: DiscoveryAssetKind) => invoke<DiscoveryItem[]>("studio_discovery_search", { source, query, assetKind }),
  download: (item: DiscoveryItem) => invoke<DiscoveryDownloadResult>("studio_discovery_download", { request: { name: item.name, assetKind: item.assetKind, downloadUrl: item.downloadUrl, sha256: item.sha256 ?? "", sizeBytes: item.sizeBytes, fileName: item.fileName } }),
  trainCharacter: (request: { name: string; sourceDir: string; baseModelPath: string; triggerWord: string; epochs: number; rank: number; maxResolution: number }) => invoke<CharacterTrainingResult>("character_training_start", { request }),
  runWorkflow: (request: { kind: WorkflowKind; baseUrl: string; workflow: unknown; values: Record<string, unknown>; inputImage?: WorkflowUpload | null; inputVideo?: WorkflowUpload | null }) => invoke<import("./studioClient").GenerationEntry[]>("studio_workflow_run", { request }),
};

export function inferComponentSlot(item: DiscoveryItem): ComponentSlot {
  const value = `${item.name} ${item.fileName} ${item.tags.join(" ")}`.toLowerCase();
  return /qwen.?image|diffusion|transformer/.test(value) ? "diffusion_model" : "checkpoint";
}

export function modelSpecForDownload(item: DiscoveryItem, path: string, sizeBytes: number): GenerationModelSpec {
  const lower = `${item.name} ${item.fileName} ${item.tags.join(" ")}`.toLowerCase();
  const video = /wan|ltx|video/.test(lower);
  const quantization = item.fileName.match(/(?:q|iq)(\d)/i)?.[1];
  return {
    id: `community-${item.id.replace(/[^A-Za-z0-9._-]/g, "-").slice(0, 100)}`,
    name: item.name,
    family: item.family,
    tasks: video ? ["text_to_video", "image_to_video"] : ["text_to_image", "image_to_image"],
    components: [{ slot: inferComponentSlot(item), source: { kind: "local_file", path }, sizeBytes }],
    source: { kind: "components" },
    defaults: { width: video ? 832 : 1024, height: video ? 480 : 1024, steps: video ? 20 : 24, cfgScale: 4, sampleMethod: "euler", flowShift: null, fps: 24, videoFrames: 81, frameGrid: "down_to4n_plus1" },
    minRamBytes: Math.ceil(Math.max(sizeBytes * 1.35, video ? 16 * 1024 ** 3 : 8 * 1024 ** 3)),
    license: { id: `community-${item.id}`, name: "Upstream model terms", url: item.pageUrl, excludedTerritories: [], acceptanceRequired: false },
    extraLaunchArgs: [], engine: "stable_diffusion_cpp", quantizationBits: quantization ? Number(quantization) : null,
  };
}

export function hardwareFit(totalRamBytes: number, minimum: number): "fits" | "tight" | "too_large" | "unknown" {
  if (!totalRamBytes) return "unknown";
  if (minimum > totalRamBytes) return "too_large";
  if (minimum > totalRamBytes * 0.8) return "tight";
  return "fits";
}
''')

write("src/lib/studioParity.test.ts", r'''import { describe, expect, it } from "vitest";
import { hardwareFit, inferComponentSlot, modelSpecForDownload, type DiscoveryItem } from "./studioParity";
const item: DiscoveryItem = { id: "hf:x", source: "hugging_face", name: "Qwen Image", assetKind: "model", family: "Qwen", repo: "x/y", fileName: "qwen-image-Q4_K.gguf", downloadUrl: "https://example.com/x", pageUrl: "https://example.com", sha256: "a".repeat(64), sizeBytes: 10_000, tags: [] };
describe("studio parity", () => {
  it("recognises diffusion checkpoints", () => expect(inferComponentSlot(item)).toBe("diffusion_model"));
  it("creates a local model spec", () => expect(modelSpecForDownload(item, "/tmp/a.gguf", 10_000).components[0]?.source).toEqual({ kind: "local_file", path: "/tmp/a.gguf" }));
  it("labels hardware headroom", () => { expect(hardwareFit(32, 16)).toBe("fits"); expect(hardwareFit(16, 15)).toBe("tight"); expect(hardwareFit(8, 16)).toBe("too_large"); });
});
''')

write("src/components/Studio/CreatorHubPanel.tsx", r'''import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile, readTextFile } from "@tauri-apps/plugin-fs";
import { Download, Search, Sparkles } from "lucide-react";
import { Button, StatusPill } from "../ui";
import { errorMessage } from "../../lib/errors";
import { studioClient, type GenerationEntry } from "../../lib/studioClient";
import { CURATED_STUDIO_PROFILES, WORKFLOW_PRESETS, hardwareFit, modelSpecForDownload, studioParityClient, type DiscoveryAssetKind, type DiscoveryItem, type DiscoverySource, type WorkflowKind, type WorkflowUpload } from "../../lib/studioParity";

const formatBytes = (value: number) => value ? `${(value / 1024 ** 3).toFixed(value >= 1024 ** 3 ? 1 : 3)} GB` : "size unknown";
const toBase64 = (bytes: Uint8Array) => { let binary = ""; const step = 0x8000; for (let i = 0; i < bytes.length; i += step) binary += String.fromCharCode(...bytes.subarray(i, i + step)); return btoa(binary); };

export function CreatorHubPanel() {
  const [source, setSource] = useState<DiscoverySource>("hugging_face");
  const [kind, setKind] = useState<DiscoveryAssetKind>("model");
  const [query, setQuery] = useState("Qwen Image 2.1 GGUF");
  const [results, setResults] = useState<DiscoveryItem[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [totalRam, setTotalRam] = useState(0);
  const [characterName, setCharacterName] = useState("My character");
  const [triggerWord, setTriggerWord] = useState("mychar");
  const [photos, setPhotos] = useState("");
  const [baseModel, setBaseModel] = useState("");
  const [trainingStatus, setTrainingStatus] = useState<string | null>(null);
  const [workflowKind, setWorkflowKind] = useState<WorkflowKind>("music");
  const [workflowUrl, setWorkflowUrl] = useState(() => localStorage.getItem("lm-comfy-workflow-url") ?? "http://127.0.0.1:8188/");
  const [workflow, setWorkflow] = useState<unknown | null>(null);
  const [workflowPrompt, setWorkflowPrompt] = useState("");
  const [workflowScript, setWorkflowScript] = useState("");
  const [inputImage, setInputImage] = useState<WorkflowUpload | null>(null);
  const [inputVideo, setInputVideo] = useState<WorkflowUpload | null>(null);
  const [workflowResults, setWorkflowResults] = useState<GenerationEntry[]>([]);

  useEffect(() => { void studioClient.status().then((status) => setTotalRam(status.totalRamBytes)).catch(() => undefined); }, []);
  const preset = WORKFLOW_PRESETS[workflowKind];
  const doSearch = async () => { setBusy("search"); setError(null); try { setResults(await studioParityClient.search(source, query, kind)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const install = async (item: DiscoveryItem) => { if (!item.sha256) return; setBusy(item.id); setError(null); try { const file = await studioParityClient.download(item); if (item.assetKind === "lora") await studioClient.addLora({ name: item.name, path: file.path }); else await studioClient.addModel(modelSpecForDownload(item, file.path, file.sizeBytes)); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const pickDir = async (setter: (value: string) => void) => { const picked = await open({ directory: true, multiple: false }); if (typeof picked === "string") setter(picked); };
  const train = async () => { setBusy("train"); setError(null); setTrainingStatus(null); try { const result = await studioParityClient.trainCharacter({ name: characterName, sourceDir: photos, baseModelPath: baseModel, triggerWord, epochs: 24, rank: 16, maxResolution: 768 }); await studioClient.addLora({ name: characterName, path: result.loraPath }); setTrainingStatus(`Trained from ${result.imageCount} photos and added to the LoRA library.`); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const pickWorkflow = async () => { const picked = await open({ directory: false, multiple: false, filters: [{ name: "ComfyUI API workflow", extensions: ["json"] }] }); if (typeof picked === "string") setWorkflow(JSON.parse(await readTextFile(picked))); };
  const pickMedia = async (type: "image" | "video") => { const picked = await open({ directory: false, multiple: false }); if (typeof picked !== "string") return; const upload = { name: picked.split(/[/\\]/).pop() ?? `${type}.bin`, dataBase64: toBase64(await readFile(picked)) }; if (type === "image") setInputImage(upload); else setInputVideo(upload); };
  const runWorkflow = async () => { if (!workflow) return; setBusy("workflow"); setError(null); try { localStorage.setItem("lm-comfy-workflow-url", workflowUrl); const values: Record<string, unknown> = { prompt: workflowPrompt, negative_prompt: "", script: workflowScript, duration_seconds: 10 }; setWorkflowResults(await studioParityClient.runWorkflow({ kind: workflowKind, baseUrl: workflowUrl, workflow, values, inputImage, inputVideo })); } catch (cause) { setError(errorMessage(cause)); } finally { setBusy(null); } };
  const sortedResults = useMemo(() => [...results].sort((a, b) => Number(Boolean(b.sha256)) - Number(Boolean(a.sha256))), [results]);

  return <div className="grid gap-4 border-t border-border pt-4">
    <section className="rounded-lg border border-border bg-surface p-3">
      <div className="flex items-center gap-2"><Sparkles size={15}/><h2 className="text-xs font-medium">Curated Studio catalog</h2></div>
      <p className="mt-1 text-[11px] text-muted">Curated families route into live Hugging Face/CivitAI results instead of freezing stale file URLs. Fit is based on this machine's measured RAM.</p>
      <div className="mt-3 grid gap-2 sm:grid-cols-2">{CURATED_STUDIO_PROFILES.map((profile) => { const fit = hardwareFit(totalRam, profile.minRamBytes); return <button key={profile.id} type="button" className="rounded border border-border p-2 text-left hover:border-accent" onClick={() => { setSource(profile.source); setKind(profile.kind); setQuery(profile.query); }}><span className="flex justify-between gap-2"><span className="text-xs font-medium">{profile.name}</span><StatusPill tone={fit === "fits" ? "success" : fit === "too_large" ? "danger" : "warning"}>{fit.replace("_", " ")}</StatusPill></span><span className="mt-1 block text-[11px] text-muted">{profile.note}</span></button>; })}</div>
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Community model & LoRA browser</h2>
      <div className="mt-2 flex flex-wrap gap-2"><select value={source} onChange={(e) => setSource(e.target.value as DiscoverySource)} className="h-8 rounded border border-border bg-background px-2 text-xs"><option value="hugging_face">Hugging Face</option><option value="civitai">CivitAI</option></select><select value={kind} onChange={(e) => setKind(e.target.value as DiscoveryAssetKind)} className="h-8 rounded border border-border bg-background px-2 text-xs"><option value="model">Models</option><option value="lora">LoRAs</option></select><input value={query} onChange={(e) => setQuery(e.target.value)} className="h-8 min-w-48 flex-1 rounded border border-border bg-background px-2 text-xs"/><Button size="sm" onClick={() => void doSearch()} disabled={busy !== null}><Search size={13}/> Search</Button></div>
      <div className="mt-3 grid gap-2">{sortedResults.map((item) => <div key={item.id} className="flex items-center justify-between gap-3 rounded border border-border p-2"><div className="min-w-0"><p className="truncate text-xs font-medium">{item.name}</p><p className="truncate text-[11px] text-muted">{item.fileName} · {formatBytes(item.sizeBytes)} · {item.family}</p>{!item.sha256 && <p className="text-[10px] text-warning">No published SHA-256: browse only; one-click install is blocked.</p>}</div><Button size="sm" variant="secondary" disabled={!item.sha256 || busy !== null} onClick={() => void install(item)}><Download size={13}/>{busy === item.id ? "Installing…" : "Install"}</Button></div>)}</div>
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Character Studio</h2><p className="mt-1 text-[11px] text-muted">Train a Krea-2-Raw LoRA locally with the verified MFLUX runtime. The base model must already exist locally; the trainer runs offline and never downloads behind your back.</p>
      <div className="mt-2 grid gap-2 sm:grid-cols-2"><input value={characterName} onChange={(e) => setCharacterName(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Character name"/><input value={triggerWord} onChange={(e) => setTriggerWord(e.target.value)} className="h-8 rounded border border-border bg-background px-2 text-xs" placeholder="Trigger word"/><Button size="sm" variant="secondary" onClick={() => void pickDir(setPhotos)}>{photos ? "Photos selected" : "Choose photos folder"}</Button><Button size="sm" variant="secondary" onClick={() => void pickDir(setBaseModel)}>{baseModel ? "Base model selected" : "Choose Krea-2-Raw folder"}</Button></div><Button className="mt-2" size="sm" disabled={!photos || !baseModel || busy !== null} onClick={() => void train()}>{busy === "train" ? "Training…" : "Train character LoRA"}</Button>{trainingStatus && <p className="mt-2 text-[11px] text-success">{trainingStatus}</p>}
    </section>

    <section className="rounded-lg border border-border bg-surface p-3">
      <h2 className="text-xs font-medium">Media workflow presets</h2><p className="mt-1 text-[11px] text-muted">Music, Talking Character, Motion Control and Extend Video run against your own ComfyUI. Export the workflow in API format once; Little Monkey uploads media, binds typed placeholders, retrieves image/video/audio outputs, and files them in the Studio gallery.</p>
      <div className="mt-2 flex flex-wrap gap-2">{(Object.keys(WORKFLOW_PRESETS) as WorkflowKind[]).map((value) => <Button key={value} size="sm" variant={workflowKind === value ? "primary" : "secondary"} onClick={() => setWorkflowKind(value)}>{WORKFLOW_PRESETS[value].label}</Button>)}</div><p className="mt-2 text-[11px] text-muted">{preset.description} Placeholders: {preset.placeholders.map((p) => `{{${p}}}`).join(", ")}</p>
      <div className="mt-2 grid gap-2"><input value={workflowUrl} onChange={(e) => setWorkflowUrl(e.target.value)} className="h-8 rounded border border-border bg-background px-2 font-mono text-xs"/><textarea value={workflowPrompt} onChange={(e) => setWorkflowPrompt(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Prompt"/>{workflowKind === "talking_character" && <textarea value={workflowScript} onChange={(e) => setWorkflowScript(e.target.value)} className="min-h-16 rounded border border-border bg-background p-2 text-xs" placeholder="Dialogue / script"/>}<div className="flex flex-wrap gap-2"><Button size="sm" variant="secondary" onClick={() => void pickWorkflow()}>{workflow ? "Workflow loaded" : "Load API workflow JSON"}</Button>{(workflowKind === "talking_character" || workflowKind === "motion_control") && <Button size="sm" variant="secondary" onClick={() => void pickMedia("image")}>{inputImage ? "Image loaded" : "Choose input image"}</Button>}{workflowKind === "extend_video" && <Button size="sm" variant="secondary" onClick={() => void pickMedia("video")}>{inputVideo ? "Video loaded" : "Choose input video"}</Button>}<Button size="sm" disabled={!workflow || busy !== null} onClick={() => void runWorkflow()}>{busy === "workflow" ? "Running…" : "Run workflow"}</Button></div></div>{workflowResults.length > 0 && <p className="mt-2 text-[11px] text-success">Completed with {workflowResults.length} artifact{workflowResults.length === 1 ? "" : "s"}; results are in the Studio gallery.</p>}
    </section>
    {error && <p role="alert" className="rounded border border-danger/30 bg-danger/5 p-2 text-xs text-danger">{error}</p>}
  </div>;
}
''')

# Wire Creator Hub into the existing Tools surface without adding a sixth Studio
# top-level mode (which would duplicate the Models/Tools hierarchy).
replace(
    "src/components/Studio/ToolPanel.tsx",
    'import { formatBytes, studioClient, type GenerationEntry } from "../../lib/studioClient";\n',
    'import { formatBytes, studioClient, type GenerationEntry } from "../../lib/studioClient";\nimport { CreatorHubPanel } from "./CreatorHubPanel";\n',
)
replace(
    "src/components/Studio/ToolPanel.tsx",
    '      {railSlot\n        ? createPortal(',
    '      <CreatorHubPanel />\n\n      {railSlot\n        ? createPortal(',
)

# Knowledge 2.0 import card.
replace(
    "src/components/Settings/KnowledgeV2Panel.tsx",
    'import { errorMessage } from "../../lib/errors";\n',
    'import { errorMessage } from "../../lib/errors";\nimport { ChatExportImportCard } from "./ChatExportImportCard";\n',
)
replace(
    "src/components/Settings/KnowledgeV2Panel.tsx",
    '          {errors[stackId] && (',
    '          <ChatExportImportCard stackId={stackId} />\n\n          {errors[stackId] && (',
)

# Register native commands.
replace(
    "src-tauri/src/lib.rs",
    'pub mod studio_tools;\n',
    'pub mod studio_tools;\npub mod studio_parity;\n',
)
replace(
    "src-tauri/src/lib.rs",
    '            generation_commands::studio_tool_stop,\n',
    '            generation_commands::studio_tool_stop,\n            studio_parity::studio_discovery_search,\n            studio_parity::studio_discovery_download,\n            studio_parity::chat_export_write_documents,\n            studio_parity::character_training_start,\n            studio_parity::studio_workflow_run,\n',
)

# `studio_parity` stores normal `GenerationEntry` rows in the same gallery. The
# type was already public; the module itself is private, so expose it to sibling
# modules only by changing the module declaration, not the command API.
replace(
    "src-tauri/src/lib.rs",
    'mod generation_commands;\n',
    'pub(crate) mod generation_commands;\n',
)

# Document the newly shipped boundary instead of leaving the old limitation.
replace(
    "docs/limitations.md",
    '- Studio ships no model catalog, and its RAM floor is a check rather than a guarantee. `sd-server` covers three host targets, and the surface is hidden elsewhere rather than offered and failed at launch.',
    '- Studio now ships a curated discovery surface over Hugging Face and CivitAI, but community search results are installable only when the source publishes a SHA-256; entries without one remain browse-only. Hardware-fit labels are conservative RAM checks, not guarantees. `sd-server` covers three host targets, and the surface is hidden elsewhere rather than offered and failed at launch. Character LoRA training currently uses the verified MFLUX runtime on Apple silicon and requires a local Krea-2-Raw model directory. Music, Talking Character, Motion Control and Extend Video are typed ComfyUI workflow presets: Little Monkey owns upload/binding/output handling, while the actual graph and model nodes stay user-owned and must be imported in ComfyUI API-workflow format.',
)

print("LU parity implementation applied")

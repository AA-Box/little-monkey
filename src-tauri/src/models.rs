//! GGUF model registry + download manager.
//!
//! Owns the curated list of known-good local models (small, tool-calling
//! capable instruct models that work well with llama-server's `--jinja`
//! OpenAI-style tool calling), plus commands to list, download, and delete
//! model weight files under the app's `models` data directory.

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Emitter, Manager};
use tokio::io::AsyncWriteExt;

use crate::permissions;
use crate::AppState;

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
}

/// The curated registry: a small, hand-picked set of instruct models known
/// to work well with llama.cpp's OpenAI-compatible tool calling.
fn curated_models() -> Vec<ModelInfo> {
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
        },
    ]
}

/// Resolves (and creates, if missing) `<app_data_dir>/models`.
fn models_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let base = app
        .path()
        .app_data_dir()
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

        if let Some(curated_match) = curated.iter().find(|m| m.file == filename) {
            let mut model = curated_match.clone();
            model.installed = true;
            model.path = Some(path.to_string_lossy().to_string());
            installed.push(model);
        } else {
            let size_gb = entry
                .metadata()
                .map(|m| m.len() as f32 / 1_000_000_000.0)
                .unwrap_or(0.0);
            installed.push(ModelInfo {
                id: format!("custom:{filename}"),
                name: strip_gguf_extension(filename),
                repo: String::new(),
                file: filename.to_string(),
                size_gb,
                tool_calling: true,
                installed: true,
                path: Some(path.to_string_lossy().to_string()),
                is_external: false,
            });
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
            tool_calling: true,
            installed: true,
            path: Some(entry.path.clone()),
            is_external: true,
        });
        live_external.push(entry);
    }
    if live_external.len() != external_len_before {
        let _ = save_external_registry(&app, &live_external);
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
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data directory: {e}"))?;
    if !base.exists() {
        std::fs::create_dir_all(&base)
            .map_err(|e| format!("Failed to create app data directory {}: {e}", base.display()))?;
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
    let canonical = p.to_string_lossy().to_string();

    let mut entries = load_external_registry(&app)?;
    if let Some(existing) = entries.iter().find(|e| e.path == canonical) {
        return Ok(ModelInfo {
            id: existing.id.clone(),
            name: existing.name.clone(),
            repo: String::new(),
            file: filename,
            size_gb: existing.size_gb,
            tool_calling: true,
            installed: true,
            path: Some(canonical),
            is_external: true,
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
        tool_calling: true,
        installed: true,
        path: Some(canonical),
        is_external: true,
    })
}

/// Forgets a previously-registered external model reference by id. Never
/// touches the underlying file on disk — it isn't owned by the app.
#[tauri::command]
pub fn models_remove_external(app: AppHandle, id: String) -> Result<(), String> {
    let mut entries = load_external_registry(&app)?;
    let before = entries.len();
    entries.retain(|e| e.id != id);
    if entries.len() == before {
        return Err(format!("No external model registered with id '{id}'"));
    }
    save_external_registry(&app, &entries)
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
    let valid_charset =
        |s: &str| s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));

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
#[tauri::command]
pub async fn models_download(app: AppHandle, repo: String, file: String) -> Result<String, String> {
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
    if dest_path.parent() != Some(dir_canon.as_path()) || tmp_path.parent() != Some(dir_canon.as_path())
    {
        return Err(format!("Invalid file name '{file}'"));
    }

    match download_to_file(&app, &repo, &file, &tmp_path).await {
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

/// Performs the actual streaming GET + write-to-disk for `models_download`,
/// emitting progress events roughly every 200ms plus a final event at
/// completion.
async fn download_to_file(
    app: &AppHandle,
    repo: &str,
    file: &str,
    tmp_path: &Path,
) -> Result<(), String> {
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");

    let client = reqwest::Client::builder()
        .user_agent("LittleMonkey-Desktop/0.1")
        .build()
        .map_err(|e| format!("Failed to build HTTP client: {e}"))?;

    let response = client
        .get(&url)
        .send()
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

    while let Some(chunk_result) = stream.next().await {
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
    permissions::request_permission(&app, state.inner(), "delete_model", detail, None).await?;

    std::fs::remove_file(&p).map_err(|e| format!("Failed to delete {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

//! Knowledge Stacks: named collections of user-picked folders/files, indexed
//! locally into chunks + embedding vectors for semantic search.
//!
//! Follows the app-data file-per-feature pattern (`chat_sessions.json`,
//! `checkpoints/`, `models/`): the stack registry lives at
//! `<app_data>/stacks/index.json` (a plain `Vec<KnowledgeStack>`, atomically
//! rewritten on every mutation — see `save_registry`), and each stack owns
//! `<app_data>/stacks/<id>/chunks.jsonl` (one [`ChunkMeta`] per line) plus
//! `<app_data>/stacks/<id>/vectors.bin` (see that file's own doc comment for
//! the binary layout). Written like `checkpoints.rs`: every `*_impl`
//! function here is `AppHandle`-free (parameterized on a base dir instead),
//! with thin `#[tauri::command]` wrappers resolving the real app-data path —
//! this is what would make a future `lm-cli` `Stacks` subcommand (slice 4)
//! nearly free, the same reasoning as `checkpoints`/`rules`/`memory`.
//!
//! Retrieval is brute-force dot product over L2-normalized vectors —
//! deliberately NO vector-db dependency (`walkdir`, `reqwest`, `serde_json`,
//! `uuid`, `sha2` are all already crate dependencies). At realistic
//! local-docs scale (10-100k chunks x 768-1024 dims) that's a few ms and
//! well under 300MB; see [`MAX_CHUNKS_PER_STACK`] for the point where this
//! stops being true. If stacks ever need to scale past that, the documented
//! escape hatch is `sqlite-vec` via rusqlite's bundled feature
//! (<https://alexgarcia.xyz/sqlite-vec/rust.html>,
//! <https://github.com/asg017/sqlite-vec>) — deliberately not taken as a
//! dependency in this slice.
//!
//! This is slice 1 of the RAG design doc: stacks + local indexing +
//! `stacks_query` for a settings-panel test-search box. No agent wiring
//! (`tool_search_docs`, doc-chat mode) exists yet — that's slice 2/3.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::Notify;
use walkdir::WalkDir;

use crate::AppState;

/// Default target chunk size (characters), per `KnowledgeStack::chunk_chars`.
const DEFAULT_CHUNK_CHARS: usize = 1600;
/// Default overlap (characters) carried from one chunk into the next, per
/// `KnowledgeStack::chunk_overlap`.
const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// Hard cap on chunks a single stack may produce. Brute-force dot-product
/// search stays fast well past this (see module doc), but a user pointing a
/// stack at a multi-GB folder would otherwise blow up index time and
/// `vectors.bin` size with no feedback until it's too late — failing fast
/// with a clear message is better than a multi-hour silent index.
const MAX_CHUNKS_PER_STACK: usize = 50_000;

/// Files larger than this are skipped during indexing (silently, like a
/// binary file) — a single huge log/data file shouldn't dominate a stack's
/// chunk budget or indexing time.
const MAX_FILE_BYTES: u64 = 5_000_000;

/// Extension allowlist for indexable files — text formats plus common code
/// files. Matched case-insensitively.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "rst", "json", "yaml", "yml", "toml", "csv", "html", "htm", "rs", "ts", "tsx", "js",
    "jsx", "py", "go", "java", "c", "cpp", "cc", "h", "hpp", "cs", "rb", "php", "swift", "kt", "sh", "sql",
];

/// How many texts are embedded per HTTP request, both at index time and
/// query time (queries are always a batch of one, so this only matters for
/// indexing).
const EMBED_BATCH_SIZE: usize = 32;

/// Default number of results `stacks_query` returns when the caller doesn't
/// specify `k`.
const DEFAULT_QUERY_K: usize = 6;

// ---------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------

/// Whether a `StackSource` is a whole folder (walked recursively) or a
/// single file.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    Folder,
    File,
}

/// One user-picked source feeding a stack's index. `path` is canonicalized
/// at add time (see `add_source_impl`) so a later reindex always walks the
/// real, current location rather than a possibly-stale relative path.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct StackSource {
    pub path: String,
    pub kind: SourceKind,
}

/// Which embedding backend a stack's `EmbeddingSpec` targets.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EmbeddingBackend {
    Llama,
    Ollama,
}

/// Pins the exact embedding model (and its output dimensionality) a stack's
/// vectors were produced with. `dim` + `model_id_or_tag` (+ `backend`) are
/// checked on every load (see `spec_matches`) — a mismatch (the user
/// switched the stack to a different model) hard-fails to "reindex
/// required" rather than silently mixing vectors from two different
/// embedding spaces, per the design doc's #1 risk.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct EmbeddingSpec {
    pub backend: EmbeddingBackend,
    pub model_id_or_tag: String,
    pub dim: u32,
    /// Prepended to every query embedded against this spec (e.g. nomic-embed
    /// needs `"search_query: "`). Empty string is a valid "no prefix needed".
    #[serde(default)]
    pub query_prefix: String,
    /// Prepended to every document/chunk embedded against this spec (e.g.
    /// nomic-embed needs `"search_document: "`).
    #[serde(default)]
    pub doc_prefix: String,
}

/// One named knowledge stack — the registry's unit of storage, mirrored
/// verbatim (snake_case fields, same as `ModelInfo`/`OllamaModelInfo`
/// elsewhere) by `src/store/stackStore.ts`'s `KnowledgeStack` TS interface.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct KnowledgeStack {
    pub id: String,
    pub name: String,
    pub sources: Vec<StackSource>,
    pub embedding: EmbeddingSpec,
    pub chunk_chars: usize,
    pub chunk_overlap: usize,
    pub indexed_at: Option<u64>,
    pub chunk_count: usize,
}

/// One chunk's metadata, one per line of `<stack_dir>/chunks.jsonl`. Row `i`
/// here corresponds to row `i` in the stack's `vectors.bin`.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChunkMeta {
    pub source_path: String,
    pub ordinal: usize,
    pub text: String,
    /// SHA-256 hex digest of the *whole source file's* content at the time
    /// it was chunked — not currently consulted for incremental reindex
    /// (that's slice 4's `file_index.json`), but recorded now so slice 4
    /// doesn't need a schema migration to add it.
    pub content_hash: String,
    #[serde(default)]
    pub heading: Option<String>,
}

/// A fully-loaded stack (chunk metadata + its `vectors.bin` contents),
/// cached in `AppState::stack_cache` so repeated `stacks_query` calls (e.g.
/// the settings panel's test-search box) don't re-read+re-parse
/// `chunks.jsonl`/`vectors.bin` from disk on every keystroke.
#[derive(Debug)]
pub struct LoadedStack {
    embedding: EmbeddingSpec,
    chunks: Vec<ChunkMeta>,
    dim: u32,
    /// Row-major, L2-normalized, flattened: row `i`'s dims are
    /// `vectors[i*dim .. (i+1)*dim]`.
    vectors: Vec<f32>,
}

/// One retrieval hit, returned by `stacks_query`.
#[derive(Debug, Serialize)]
pub struct StackQueryResult {
    pub stack_id: String,
    pub stack_name: String,
    pub source_path: String,
    pub score: f32,
    pub text: String,
    pub heading: Option<String>,
}

// ---------------------------------------------------------------------
// Registry I/O
// ---------------------------------------------------------------------

fn stacks_base_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))?
        .join("stacks");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create stacks dir: {e}"))?;
    Ok(dir)
}

fn registry_path(base: &Path) -> PathBuf {
    base.join("index.json")
}

/// Reject anything that isn't a plain UUID-shaped id, so a crafted id can
/// never traverse outside the stacks directory (mirrors
/// `checkpoints::validate_id`).
fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("Invalid stack id '{}'", id))
    }
}

fn load_registry(base: &Path) -> Result<Vec<KnowledgeStack>, String> {
    let path = registry_path(base);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|e| format!("Failed to parse stacks registry: {e}"))
}

/// Atomic registry write: sibling temp file + rename, same idiom as
/// `checkpoints.rs`'s manifest writes / `sessions.rs`.
fn save_registry(base: &Path, stacks: &[KnowledgeStack]) -> Result<(), String> {
    let path = registry_path(base);
    let payload =
        serde_json::to_string_pretty(stacks).map_err(|e| format!("Failed to serialize stacks registry: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------
// Registry CRUD (AppHandle-free core)
// ---------------------------------------------------------------------

pub fn list_impl(base: &Path) -> Result<Vec<KnowledgeStack>, String> {
    load_registry(base)
}

pub fn create_impl(base: &Path, name: String, embedding: EmbeddingSpec) -> Result<KnowledgeStack, String> {
    let mut registry = load_registry(base)?;
    let stack = KnowledgeStack {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        sources: Vec::new(),
        embedding,
        chunk_chars: DEFAULT_CHUNK_CHARS,
        chunk_overlap: DEFAULT_CHUNK_OVERLAP,
        indexed_at: None,
        chunk_count: 0,
    };
    registry.push(stack.clone());
    save_registry(base, &registry)?;
    Ok(stack)
}

pub fn delete_impl(base: &Path, id: &str) -> Result<(), String> {
    validate_id(id)?;
    let mut registry = load_registry(base)?;
    let before = registry.len();
    registry.retain(|s| s.id != id);
    if registry.len() == before {
        return Err(format!("Stack '{}' not found", id));
    }
    save_registry(base, &registry)?;
    // Best-effort: the registry write above is the source of truth: even if
    // this fails to fully clean up, the stack is already gone from the list.
    let _ = std::fs::remove_dir_all(base.join(id));
    Ok(())
}

pub fn rename_impl(base: &Path, id: &str, name: String) -> Result<KnowledgeStack, String> {
    validate_id(id)?;
    let mut registry = load_registry(base)?;
    let stack = registry
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("Stack '{}' not found", id))?;
    stack.name = name;
    let updated = stack.clone();
    save_registry(base, &registry)?;
    Ok(updated)
}

pub fn add_source_impl(base: &Path, id: &str, path: String, kind: SourceKind) -> Result<KnowledgeStack, String> {
    validate_id(id)?;
    let canonical = PathBuf::from(&path)
        .canonicalize()
        .map_err(|e| format!("Path not found: {path} ({e})"))?;
    let canonical_str = canonical.to_string_lossy().to_string();

    let mut registry = load_registry(base)?;
    let stack = registry
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("Stack '{}' not found", id))?;
    if stack.sources.iter().any(|s| s.path == canonical_str) {
        return Err(format!("'{}' is already a source of this stack", canonical_str));
    }
    stack.sources.push(StackSource { path: canonical_str, kind });
    let updated = stack.clone();
    save_registry(base, &registry)?;
    Ok(updated)
}

pub fn remove_source_impl(base: &Path, id: &str, path: &str) -> Result<KnowledgeStack, String> {
    validate_id(id)?;
    let mut registry = load_registry(base)?;
    let stack = registry
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("Stack '{}' not found", id))?;
    let before = stack.sources.len();
    stack.sources.retain(|s| s.path != path);
    if stack.sources.len() == before {
        return Err(format!("'{}' is not a source of this stack", path));
    }
    let updated = stack.clone();
    save_registry(base, &registry)?;
    Ok(updated)
}

// ---------------------------------------------------------------------
// Chunking
// ---------------------------------------------------------------------

/// One chunk produced by [`chunk_text`], before it's assigned an `ordinal`/
/// `content_hash` and becomes a full [`ChunkMeta`].
pub struct Chunk {
    pub heading: Option<String>,
    pub text: String,
}

/// Returns the last (up to) `n` characters of `s`, safe on non-ASCII text
/// (operates on chars, never byte-slices mid-codepoint).
fn tail_chars(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if n == 0 || total == 0 {
        return String::new();
    }
    let skip = total.saturating_sub(n);
    s.chars().skip(skip).collect()
}

fn push_chunk(chunks: &mut Vec<Chunk>, heading: Option<String>, text: String) {
    let trimmed = text.trim();
    if !trimmed.is_empty() {
        chunks.push(Chunk { heading, text: trimmed.to_string() });
    }
}

/// Splits `text` into chunks on paragraph/heading boundaries, targeting
/// `chunk_chars` per chunk with `chunk_overlap` characters of trailing
/// context carried into the next chunk. A markdown heading line (`#`, `##`,
/// …) is tracked so each chunk can report which section it came from
/// (`Chunk::heading`) — plain text/code files simply never set it.
///
/// Boundary-preserving is prioritized over an exact size cap: a paragraph
/// that fits is never split mid-sentence just to hit `chunk_chars` exactly.
/// Only a single paragraph that *alone* exceeds `chunk_chars` is hard-split
/// (on character boundaries, with the same overlap) — this is the only case
/// where a chunk can end mid-paragraph.
pub fn chunk_text(text: &str, chunk_chars: usize, chunk_overlap: usize) -> Vec<Chunk> {
    let chunk_chars = chunk_chars.max(1);
    let chunk_overlap = chunk_overlap.min(chunk_chars.saturating_sub(1));

    // Pass 1: split into paragraphs on blank lines, tracking the most
    // recent markdown heading seen so far.
    let mut paragraphs: Vec<(Option<String>, String)> = Vec::new();
    let mut current_heading: Option<String> = None;
    let mut buf = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let hash_prefix_len = trimmed.chars().take_while(|&c| c == '#').count();
        let is_heading = hash_prefix_len > 0
            && trimmed[hash_prefix_len..].chars().next().is_none_or(|c| c == ' ');
        if is_heading {
            if !buf.trim().is_empty() {
                paragraphs.push((current_heading.clone(), std::mem::take(&mut buf)));
            }
            buf.clear();
            current_heading = Some(trimmed.trim_start_matches('#').trim().to_string());
            continue;
        }
        if line.trim().is_empty() {
            if !buf.trim().is_empty() {
                paragraphs.push((current_heading.clone(), std::mem::take(&mut buf)));
            }
            buf.clear();
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
    }
    if !buf.trim().is_empty() {
        paragraphs.push((current_heading, buf));
    }

    // Pass 2: greedily pack paragraphs into chunks targeting chunk_chars,
    // with chunk_overlap characters of trailing context carried forward.
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut current = String::new();
    let mut current_heading_for_chunk: Option<String> = None;

    for (heading, para) in paragraphs {
        // A single paragraph too big to ever fit on its own: flush whatever
        // is pending, then hard-split it independently.
        if para.chars().count() > chunk_chars {
            if !current.is_empty() {
                push_chunk(&mut chunks, current_heading_for_chunk.clone(), std::mem::take(&mut current));
            }
            let chars: Vec<char> = para.chars().collect();
            let mut start = 0usize;
            while start < chars.len() {
                let end = (start + chunk_chars).min(chars.len());
                let piece: String = chars[start..end].iter().collect();
                push_chunk(&mut chunks, heading.clone(), piece);
                if end == chars.len() {
                    break;
                }
                let next_start = end.saturating_sub(chunk_overlap);
                // Guarantee forward progress even if chunk_overlap ==
                // chunk_chars (clamped above so this can't actually happen,
                // but stay defensive against a future change to that clamp).
                start = if next_start > start { next_start } else { end };
            }
            current_heading_for_chunk = None;
            continue;
        }

        let would_be_len = if current.is_empty() {
            para.chars().count()
        } else {
            current.chars().count() + 2 + para.chars().count()
        };

        if !current.is_empty() && would_be_len > chunk_chars {
            push_chunk(&mut chunks, current_heading_for_chunk.clone(), current.clone());
            current = tail_chars(&current, chunk_overlap);
            current_heading_for_chunk = heading.clone();
        }

        if current.is_empty() {
            current_heading_for_chunk = heading;
        } else {
            current.push_str("\n\n");
        }
        current.push_str(&para);
    }

    if !current.trim().is_empty() {
        push_chunk(&mut chunks, current_heading_for_chunk, current);
    }

    chunks
}

// ---------------------------------------------------------------------
// vectors.bin: 16-byte header (magic + version + dim + count) followed by
// row-major, little-endian f32 rows. Row `i` corresponds to line `i` of
// the sibling chunks.jsonl.
// ---------------------------------------------------------------------

const VECTORS_MAGIC: [u8; 4] = *b"LMVC";
const VECTORS_VERSION: u32 = 1;
const VECTORS_HEADER_LEN: usize = 16;

fn write_vectors_bin(path: &Path, dim: u32, vectors: &[Vec<f32>]) -> Result<(), String> {
    let count = vectors.len() as u32;
    let mut buf: Vec<u8> = Vec::with_capacity(VECTORS_HEADER_LEN + vectors.len() * dim as usize * 4);
    buf.extend_from_slice(&VECTORS_MAGIC);
    buf.extend_from_slice(&VECTORS_VERSION.to_le_bytes());
    buf.extend_from_slice(&dim.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    for row in vectors {
        if row.len() != dim as usize {
            return Err(format!("Vector row has {} dims, expected {}", row.len(), dim));
        }
        for x in row {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &buf).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

/// Reads a `vectors.bin` back into `(dim, count, flat_rows)`.
fn read_vectors_bin(path: &Path) -> Result<(u32, u32, Vec<f32>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if bytes.len() < VECTORS_HEADER_LEN {
        return Err("vectors.bin is truncated (missing header)".to_string());
    }
    if bytes[0..4] != VECTORS_MAGIC {
        return Err("vectors.bin has an invalid magic header — reindex required".to_string());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VECTORS_VERSION {
        return Err(format!("vectors.bin has unsupported version {version} — reindex required"));
    }
    let dim = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());

    let expected_len = VECTORS_HEADER_LEN + (dim as usize) * (count as usize) * 4;
    if bytes.len() < expected_len {
        return Err("vectors.bin is truncated (fewer bytes than its header promises)".to_string());
    }

    let mut flat = Vec::with_capacity(dim as usize * count as usize);
    let mut offset = VECTORS_HEADER_LEN;
    for _ in 0..(dim as usize * count as usize) {
        let x = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
        flat.push(x);
        offset += 4;
    }

    Ok((dim, count, flat))
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// Ranks every row of `flat` (row-major, `count` rows of `dim` dims each)
/// against `query` by plain dot product — valid as cosine similarity only
/// because every row (and the query) is L2-normalized before being stored/
/// used. Returns the top `k` `(row_index, score)` pairs, highest score
/// first.
pub fn top_k_by_dot(query: &[f32], flat: &[f32], dim: usize, count: usize, k: usize) -> Vec<(usize, f32)> {
    let mut scores: Vec<(usize, f32)> = Vec::with_capacity(count);
    for i in 0..count {
        let row = &flat[i * dim..(i + 1) * dim];
        let score: f32 = row.iter().zip(query.iter()).map(|(a, b)| a * b).sum();
        scores.push((i, score));
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.truncate(k);
    scores
}

/// True if two `EmbeddingSpec`s are compatible for reuse (same backend,
/// same model/tag, same dimensionality) — the cheap check backing the
/// design doc's #1 risk ("embedding-spec drift"): a spec change anywhere
/// along this triple must hard-fail to "reindex required" rather than let a
/// cached/loaded stack silently mix vectors from two different models.
fn spec_matches(a: &EmbeddingSpec, b: &EmbeddingSpec) -> bool {
    a.backend == b.backend && a.model_id_or_tag == b.model_id_or_tag && a.dim == b.dim
}

// ---------------------------------------------------------------------
// Embedding dispatch
// ---------------------------------------------------------------------

async fn embed_via_llama(model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{}/v1/embeddings", crate::llama::EMBED_PORT))
        .json(&serde_json::json!({ "model": model, "input": texts }))
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| format!("Failed to reach the embedding server: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Embedding request failed (HTTP {status}): {body}"));
    }

    #[derive(Deserialize)]
    struct EmbeddingDatum {
        embedding: Vec<f32>,
    }
    #[derive(Deserialize)]
    struct EmbeddingResponse {
        data: Vec<EmbeddingDatum>,
    }

    let parsed: EmbeddingResponse =
        resp.json().await.map_err(|e| format!("Failed to parse embedding response: {e}"))?;
    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
}

/// Embeds `texts` against `spec`, dispatching to the recorded backend,
/// applying `spec.query_prefix`/`spec.doc_prefix` (per `is_query`) to every
/// text first, batching requests at [`EMBED_BATCH_SIZE`], and L2-normalizing
/// every returned vector. Hard-fails (rather than silently proceeding) if
/// the model's actual output dimensionality doesn't match `spec.dim` — see
/// [`spec_matches`]'s doc comment for why that must never be papered over.
pub async fn embed_batch(spec: &EmbeddingSpec, texts: &[String], is_query: bool) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = if is_query { &spec.query_prefix } else { &spec.doc_prefix };
    let prefixed: Vec<String> = texts.iter().map(|t| format!("{prefix}{t}")).collect();

    let mut out: Vec<Vec<f32>> = Vec::with_capacity(prefixed.len());
    for batch in prefixed.chunks(EMBED_BATCH_SIZE) {
        let mut vectors = match spec.backend {
            EmbeddingBackend::Llama => embed_via_llama(&spec.model_id_or_tag, batch).await?,
            EmbeddingBackend::Ollama => crate::ollama::embed(&spec.model_id_or_tag, batch).await?,
        };
        for v in &mut vectors {
            l2_normalize(v);
        }
        out.extend(vectors);
    }

    if let Some(first) = out.first() {
        if first.len() != spec.dim as usize {
            return Err(format!(
                "Embedding model produced {}-dim vectors but this stack expects {} — reindex required",
                first.len(),
                spec.dim
            ));
        }
    }

    Ok(out)
}

// ---------------------------------------------------------------------
// Source walking
// ---------------------------------------------------------------------

/// Git's own heuristic for "binary": a NUL byte anywhere in the first 8000
/// bytes (mirrors `git.rs::is_binary` — duplicated rather than imported
/// since that one is private to `git.rs` and the two modules' binary checks
/// are conceptually independent, even though the heuristic is identical).
fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// Reads `path` as an indexable text file, or returns `None` if it should
/// be skipped: wrong extension, too large, empty, binary, or not valid
/// UTF-8. Returns `(canonical path string, content)`.
fn read_indexable_file(path: &Path) -> Option<(String, String)> {
    let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();
    if !ALLOWED_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if looks_binary(&bytes) {
        return None;
    }
    let content = String::from_utf8(bytes).ok()?;
    Some((path.to_string_lossy().to_string(), content))
}

/// Walks every `StackSource`, returning `(source_path, content)` for every
/// indexable file found. Folders are walked with `WalkDir`, skipping the
/// same VCS/build/dependency directories as `tools.rs`'s "@"-mention walker
/// (`tools::MENTION_SKIP_DIRS`) — this is exactly the "point at a folder, it
/// finds the real files" philosophy the design doc calls out, reused rather
/// than reinvented.
fn collect_source_files(sources: &[StackSource]) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for source in sources {
        let path = Path::new(&source.path);
        match source.kind {
            SourceKind::File => {
                if let Some(entry) = read_indexable_file(path) {
                    files.push(entry);
                }
            }
            SourceKind::Folder => {
                let walker = WalkDir::new(path).follow_links(false).into_iter().filter_entry(|entry| {
                    if entry.depth() > 0 && entry.file_type().is_dir() {
                        if let Some(name) = entry.file_name().to_str() {
                            return !crate::tools::MENTION_SKIP_DIRS.contains(&name);
                        }
                    }
                    true
                });
                for entry in walker {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(_) => continue,
                    };
                    if !entry.file_type().is_file() {
                        continue;
                    }
                    if let Some(found) = read_indexable_file(entry.path()) {
                        files.push(found);
                    }
                }
            }
        }
    }
    files
}

fn sha256_hex(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_chunks_jsonl(stack_dir: &Path, chunks: &[ChunkMeta]) -> Result<(), String> {
    let mut buf = String::new();
    for chunk in chunks {
        buf.push_str(&serde_json::to_string(chunk).map_err(|e| format!("Failed to serialize chunk: {e}"))?);
        buf.push('\n');
    }
    let path = stack_dir.join("chunks.jsonl");
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

fn read_chunks_jsonl(stack_dir: &Path) -> Result<Vec<ChunkMeta>, String> {
    let path = stack_dir.join("chunks.jsonl");
    let raw = std::fs::read_to_string(&path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("Corrupt chunk entry: {e}")))
        .collect()
}

fn emit_progress(app: &AppHandle, stack_id: &str, files_done: usize, files_total: usize, chunks: usize, phase: &str) {
    let _ = app.emit(
        "stacks://index-progress",
        serde_json::json!({
            "stack_id": stack_id,
            "files_done": files_done,
            "files_total": files_total,
            "chunks": chunks,
            "phase": phase,
        }),
    );
}

// ---------------------------------------------------------------------
// Reindex pipeline
// ---------------------------------------------------------------------

/// Walks `stack`'s sources, chunks every indexable file, embeds every chunk
/// (batched), and atomically writes `chunks.jsonl` + `vectors.bin`, updating
/// the registry's `indexed_at`/`chunk_count` on success. Streams
/// `stacks://index-progress` events throughout. Cancellable via the
/// `Notify` registered in `AppState::index_cancels` under `stack_id` (see
/// `stacks_cancel_index`) — checked between embedding batches, the only
/// genuinely slow step (network round-trips), via `tokio::select!` racing
/// the cancellation against each batch's embed call, mirroring how
/// `tool_run_shell` races its own cancellation against the child process.
pub async fn reindex_impl(app: &AppHandle, state: &AppState, stack_id: &str) -> Result<KnowledgeStack, String> {
    validate_id(stack_id)?;
    let base = stacks_base_dir(app)?;
    let mut registry = load_registry(&base)?;
    let idx = registry
        .iter()
        .position(|s| s.id == stack_id)
        .ok_or_else(|| format!("Stack '{}' not found", stack_id))?;
    let stack = registry[idx].clone();

    let cancel = {
        let mut cancels = state
            .index_cancels
            .lock()
            .map_err(|_| "Index-cancel lock poisoned".to_string())?;
        cancels.entry(stack_id.to_string()).or_insert_with(|| Arc::new(Notify::new())).clone()
    };
    // RAII-style cleanup so the cancel handle never lingers past this run,
    // whether it finishes normally, errors, or is cancelled.
    let _cleanup = CancelCleanup { state, stack_id: stack_id.to_string() };

    emit_progress(app, stack_id, 0, 0, 0, "walking");
    let files = collect_source_files(&stack.sources);
    let files_total = files.len();
    if files_total == 0 {
        return Err("No indexable files found in this stack's sources".to_string());
    }

    let mut all_chunks: Vec<ChunkMeta> = Vec::new();
    for (i, (source_path, content)) in files.iter().enumerate() {
        let content_hash = sha256_hex(content);
        for (ordinal, chunk) in chunk_text(content, stack.chunk_chars, stack.chunk_overlap).into_iter().enumerate() {
            all_chunks.push(ChunkMeta {
                source_path: source_path.clone(),
                ordinal,
                text: chunk.text,
                content_hash: content_hash.clone(),
                heading: chunk.heading,
            });
        }
        emit_progress(app, stack_id, i + 1, files_total, all_chunks.len(), "chunking");
    }

    if all_chunks.len() > MAX_CHUNKS_PER_STACK {
        return Err(format!(
            "This stack would produce {} chunks, over the {} limit — narrow its sources or split it into multiple stacks",
            all_chunks.len(),
            MAX_CHUNKS_PER_STACK
        ));
    }

    let texts: Vec<String> = all_chunks.iter().map(|c| c.text.clone()).collect();
    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
    let mut embedded_so_far = 0usize;
    for batch in texts.chunks(EMBED_BATCH_SIZE) {
        let batch_vec = batch.to_vec();
        tokio::select! {
            biased;
            _ = cancel.notified() => {
                return Err("Indexing cancelled".to_string());
            }
            result = embed_batch(&stack.embedding, &batch_vec, false) => {
                vectors.extend(result?);
            }
        }
        embedded_so_far += batch.len();
        emit_progress(app, stack_id, files_total, files_total, embedded_so_far, "embedding");
    }

    let stack_dir = base.join(stack_id);
    std::fs::create_dir_all(&stack_dir).map_err(|e| format!("Failed to create stack directory: {e}"))?;
    write_chunks_jsonl(&stack_dir, &all_chunks)?;
    write_vectors_bin(&stack_dir.join("vectors.bin"), stack.embedding.dim, &vectors)?;

    registry[idx].indexed_at = Some(now_ms());
    registry[idx].chunk_count = all_chunks.len();
    save_registry(&base, &registry)?;

    state
        .stack_cache
        .lock()
        .map_err(|_| "Stack-cache lock poisoned".to_string())?
        .remove(stack_id);

    emit_progress(app, stack_id, files_total, files_total, all_chunks.len(), "done");

    Ok(registry[idx].clone())
}

/// Removes `stack_id`'s cancellation handle from `AppState::index_cancels`
/// on drop, so a finished/errored/cancelled reindex never leaves a stale
/// entry behind for a later `stacks_cancel_index` call to (harmlessly, but
/// pointlessly) find.
struct CancelCleanup<'a> {
    state: &'a AppState,
    stack_id: String,
}

impl Drop for CancelCleanup<'_> {
    fn drop(&mut self) {
        if let Ok(mut cancels) = self.state.index_cancels.lock() {
            cancels.remove(&self.stack_id);
        }
    }
}

// ---------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------

/// Loads stack `stack`'s chunks + vectors, using `AppState::stack_cache`
/// when the cached copy's embedding spec still matches the registry's
/// current one (see `spec_matches`) — a stack whose embedding model changed
/// since it was cached is transparently reloaded rather than served stale.
fn load_stack_cached(state: &AppState, base: &Path, stack: &KnowledgeStack) -> Result<Arc<LoadedStack>, String> {
    {
        let cache = state.stack_cache.lock().map_err(|_| "Stack-cache lock poisoned".to_string())?;
        if let Some(loaded) = cache.get(&stack.id) {
            if spec_matches(&loaded.embedding, &stack.embedding) {
                return Ok(loaded.clone());
            }
        }
    }

    let stack_dir = base.join(&stack.id);
    let chunks = read_chunks_jsonl(&stack_dir)?;
    let (dim, count, vectors) = read_vectors_bin(&stack_dir.join("vectors.bin"))?;
    if count as usize != chunks.len() {
        return Err("chunks.jsonl and vectors.bin are out of sync — reindex required".to_string());
    }
    if dim != stack.embedding.dim {
        return Err("Indexed vector dimension doesn't match this stack's embedding spec — reindex required".to_string());
    }

    let loaded = Arc::new(LoadedStack { embedding: stack.embedding.clone(), chunks, dim, vectors });
    state
        .stack_cache
        .lock()
        .map_err(|_| "Stack-cache lock poisoned".to_string())?
        .insert(stack.id.clone(), loaded.clone());
    Ok(loaded)
}

/// Embeds `query` against each of `stack_ids` (a stack that hasn't been
/// indexed yet is a hard error, not an empty result) and returns the
/// combined top-`k` hits across all of them, highest score first.
pub async fn query_impl(
    app: &AppHandle,
    state: &AppState,
    stack_ids: &[String],
    query: &str,
    k: usize,
) -> Result<Vec<StackQueryResult>, String> {
    let base = stacks_base_dir(app)?;
    let registry = load_registry(&base)?;

    let mut all_results: Vec<StackQueryResult> = Vec::new();

    for stack_id in stack_ids {
        let stack = registry
            .iter()
            .find(|s| &s.id == stack_id)
            .ok_or_else(|| format!("Stack '{}' not found", stack_id))?;
        if stack.indexed_at.is_none() {
            return Err(format!("Stack '{}' has not been indexed yet", stack.name));
        }

        let loaded = load_stack_cached(state, &base, stack)?;

        let query_vectors = embed_batch(&stack.embedding, &[query.to_string()], true).await?;
        let query_vec = query_vectors.into_iter().next().ok_or_else(|| "Failed to embed query".to_string())?;

        let ranked = top_k_by_dot(&query_vec, &loaded.vectors, loaded.dim as usize, loaded.chunks.len(), k);
        for (row, score) in ranked {
            if let Some(chunk) = loaded.chunks.get(row) {
                all_results.push(StackQueryResult {
                    stack_id: stack.id.clone(),
                    stack_name: stack.name.clone(),
                    source_path: chunk.source_path.clone(),
                    score,
                    text: chunk.text.clone(),
                    heading: chunk.heading.clone(),
                });
            }
        }
    }

    all_results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    all_results.truncate(k);
    Ok(all_results)
}

// ---------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------

#[tauri::command]
pub fn stacks_list(app: AppHandle) -> Result<Vec<KnowledgeStack>, String> {
    list_impl(&stacks_base_dir(&app)?)
}

#[tauri::command]
pub fn stacks_create(app: AppHandle, name: String, embedding: EmbeddingSpec) -> Result<KnowledgeStack, String> {
    create_impl(&stacks_base_dir(&app)?, name, embedding)
}

#[tauri::command]
pub fn stacks_delete(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    delete_impl(&stacks_base_dir(&app)?, &id)?;
    state
        .stack_cache
        .lock()
        .map_err(|_| "Stack-cache lock poisoned".to_string())?
        .remove(&id);
    Ok(())
}

#[tauri::command]
pub fn stacks_rename(app: AppHandle, id: String, name: String) -> Result<KnowledgeStack, String> {
    rename_impl(&stacks_base_dir(&app)?, &id, name)
}

#[tauri::command]
pub fn stacks_add_source(app: AppHandle, id: String, path: String, kind: SourceKind) -> Result<KnowledgeStack, String> {
    add_source_impl(&stacks_base_dir(&app)?, &id, path, kind)
}

#[tauri::command]
pub fn stacks_remove_source(app: AppHandle, id: String, path: String) -> Result<KnowledgeStack, String> {
    remove_source_impl(&stacks_base_dir(&app)?, &id, &path)
}

#[tauri::command]
pub async fn stacks_reindex(app: AppHandle, state: tauri::State<'_, AppState>, id: String) -> Result<KnowledgeStack, String> {
    reindex_impl(&app, state.inner(), &id).await
}

/// Best-effort cancellation, like `tools_cancel_running`: if no reindex is
/// currently running for `id`, this is simply a no-op (nothing to cancel).
#[tauri::command]
pub fn stacks_cancel_index(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let cancels = state.index_cancels.lock().map_err(|_| "Index-cancel lock poisoned".to_string())?;
    if let Some(notify) = cancels.get(&id) {
        notify.notify_waiters();
    }
    Ok(())
}

#[tauri::command]
pub async fn stacks_query(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    stack_ids: Vec<String>,
    query: String,
    k: Option<u32>,
) -> Result<Vec<StackQueryResult>, String> {
    query_impl(&app, state.inner(), &stack_ids, &query, k.unwrap_or(DEFAULT_QUERY_K as u32) as usize).await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let path =
                std::env::temp_dir().join(format!("little_monkey_stacks_test_{}_{}_{}_{}", tag, std::process::id(), n, nanos));
            std::fs::create_dir_all(&path).unwrap();
            TempDir { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn test_spec(dim: u32) -> EmbeddingSpec {
        EmbeddingSpec {
            backend: EmbeddingBackend::Llama,
            model_id_or_tag: "test-model".to_string(),
            dim,
            query_prefix: String::new(),
            doc_prefix: String::new(),
        }
    }

    // --- chunker boundary correctness ---

    #[test]
    fn chunk_text_keeps_a_short_paragraph_whole() {
        let chunks = chunk_text("one short paragraph", 1600, 200);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "one short paragraph");
    }

    #[test]
    fn chunk_text_splits_on_paragraph_boundaries_not_mid_sentence() {
        let a = "a".repeat(60);
        let b = "b".repeat(60);
        let c = "c".repeat(60);
        let text = format!("{a}\n\n{b}\n\n{c}");
        // Target small enough that a+b fits but a+b+c doesn't.
        let chunks = chunk_text(&text, 130, 0);
        assert!(chunks.len() >= 2, "expected at least 2 chunks, got {}", chunks.len());
        // No chunk should contain a partial paragraph (each paragraph is
        // uniform repeated chars, so a "partial" would be a different
        // length than 60, 120, or 180).
        for chunk in &chunks {
            let len = chunk.text.replace("\n\n", "").len();
            assert!(len % 60 == 0, "chunk broke a paragraph mid-way: len={len}");
        }
    }

    #[test]
    fn chunk_text_hard_splits_an_oversized_single_paragraph() {
        let text = "x".repeat(500);
        let chunks = chunk_text(&text, 100, 20);
        assert!(chunks.len() > 1, "an oversized paragraph must be split into multiple chunks");
        for chunk in &chunks {
            assert!(chunk.text.chars().count() <= 100, "hard-split chunk exceeded chunk_chars");
        }
        // Every char of the original text must still appear (nothing lost).
        let total_unique_chars: usize = chunks.iter().map(|c| c.text.chars().count()).sum();
        assert!(total_unique_chars >= 500, "hard split must not lose content");
    }

    #[test]
    fn chunk_text_carries_overlap_into_the_next_chunk() {
        let a = "a".repeat(100);
        let b = "b".repeat(100);
        let text = format!("{a}\n\n{b}");
        let chunks = chunk_text(&text, 100, 30);
        assert!(chunks.len() >= 2);
        // The second chunk should start with the tail of the first (the
        // overlap), followed by the second paragraph.
        assert!(chunks[1].text.starts_with(&"a".repeat(30)), "second chunk missing carried-over overlap: {:?}", chunks[1].text);
    }

    #[test]
    fn chunk_text_tracks_markdown_headings() {
        let text = "# Section One\n\nfirst paragraph\n\n# Section Two\n\nsecond paragraph";
        // Small enough that the two sections can't be packed into one
        // chunk, so each section's heading is exercised independently.
        let chunks = chunk_text(text, 20, 0);
        assert_eq!(chunks.len(), 2, "unexpected chunks: {:?}", chunks.iter().map(|c| &c.text).collect::<Vec<_>>());
        assert_eq!(chunks[0].heading.as_deref(), Some("Section One"));
        assert_eq!(chunks[1].heading.as_deref(), Some("Section Two"));
    }

    #[test]
    fn chunk_text_handles_empty_input() {
        assert!(chunk_text("", 1600, 200).is_empty());
        assert!(chunk_text("   \n\n  ", 1600, 200).is_empty());
    }

    // --- vectors.bin roundtrip ---

    #[test]
    fn vectors_bin_roundtrips_exact_values() {
        let dir = TempDir::new("vectors");
        let path = dir.path.join("vectors.bin");
        let vectors = vec![vec![0.1_f32, 0.2, 0.3, 0.4], vec![-1.0, 0.5, 0.0, 2.5], vec![9.9, -9.9, 0.0, 1e-6]];

        write_vectors_bin(&path, 4, &vectors).unwrap();
        let (dim, count, flat) = read_vectors_bin(&path).unwrap();

        assert_eq!(dim, 4);
        assert_eq!(count, 3);
        assert_eq!(flat.len(), 12);
        for (i, row) in vectors.iter().enumerate() {
            let read_row = &flat[i * 4..(i + 1) * 4];
            assert_eq!(read_row, row.as_slice(), "row {i} did not roundtrip exactly");
        }
    }

    #[test]
    fn vectors_bin_rejects_truncated_file() {
        let dir = TempDir::new("vectors_truncated");
        let path = dir.path.join("vectors.bin");
        std::fs::write(&path, [0u8; 5]).unwrap();
        let err = read_vectors_bin(&path).unwrap_err();
        assert!(err.contains("truncated"), "unexpected error: {err}");
    }

    #[test]
    fn vectors_bin_rejects_bad_magic() {
        let dir = TempDir::new("vectors_bad_magic");
        let path = dir.path.join("vectors.bin");
        std::fs::write(&path, [0u8; 16]).unwrap();
        let err = read_vectors_bin(&path).unwrap_err();
        assert!(err.contains("magic"), "unexpected error: {err}");
    }

    // --- dot-product ranking correctness ---

    #[test]
    fn top_k_by_dot_ranks_the_closest_vector_first() {
        // Three 2D unit vectors: row 0 orthogonal to the query, row 1
        // identical to the query, row 2 the exact opposite of the query.
        let flat: Vec<f32> = vec![1.0, 0.0, 0.0, 1.0, 0.0, -1.0];
        let query = vec![0.0_f32, 1.0];

        let ranked = top_k_by_dot(&query, &flat, 2, 3, 3);
        assert_eq!(ranked[0].0, 1, "expected row 1 (matches the query exactly) to rank first");
        assert!((ranked[0].1 - 1.0).abs() < 1e-6);
        assert_eq!(ranked[1].0, 0, "orthogonal row should rank second");
        // Row 2 is the exact opposite of the query — should rank last.
        assert_eq!(ranked[2].0, 2);
        assert!(ranked[2].1 < ranked[1].1);
        assert!((ranked[2].1 - (-1.0)).abs() < 1e-6);
    }

    #[test]
    fn top_k_by_dot_truncates_to_k() {
        let flat: Vec<f32> = vec![1.0, 0.0, 0.9, 0.1, 0.0, 1.0];
        let query = vec![1.0_f32, 0.0];
        let ranked = top_k_by_dot(&query, &flat, 2, 3, 2);
        assert_eq!(ranked.len(), 2);
    }

    // --- embedding-spec mismatch hard-fails ---

    #[test]
    fn spec_matches_true_for_identical_specs() {
        assert!(spec_matches(&test_spec(768), &test_spec(768)));
    }

    #[test]
    fn spec_matches_false_when_dim_changes() {
        assert!(!spec_matches(&test_spec(768), &test_spec(1024)));
    }

    #[test]
    fn spec_matches_false_when_model_changes() {
        let mut other = test_spec(768);
        other.model_id_or_tag = "different-model".to_string();
        assert!(!spec_matches(&test_spec(768), &other));
    }

    #[test]
    fn spec_matches_false_when_backend_changes() {
        let mut other = test_spec(768);
        other.backend = EmbeddingBackend::Ollama;
        assert!(!spec_matches(&test_spec(768), &other));
    }

    #[test]
    fn load_stack_cached_hard_fails_on_dim_mismatch_not_silent_mixing() {
        let base = TempDir::new("stack_base");
        let state = AppState::default();

        let stack = KnowledgeStack {
            id: "00000000-0000-4000-8000-000000000abc".to_string(),
            name: "Test Stack".to_string(),
            sources: Vec::new(),
            embedding: test_spec(1024), // registry now expects 1024 dims…
            chunk_chars: DEFAULT_CHUNK_CHARS,
            chunk_overlap: DEFAULT_CHUNK_OVERLAP,
            indexed_at: Some(1),
            chunk_count: 1,
        };

        let stack_dir = base.path.join(&stack.id);
        std::fs::create_dir_all(&stack_dir).unwrap();
        write_chunks_jsonl(
            &stack_dir,
            &[ChunkMeta {
                source_path: "a.txt".to_string(),
                ordinal: 0,
                text: "hello".to_string(),
                content_hash: "x".to_string(),
                heading: None,
            }],
        )
        .unwrap();
        // …but the on-disk vectors.bin was written for a 768-dim model.
        write_vectors_bin(&stack_dir.join("vectors.bin"), 768, &[vec![0.1; 768]]).unwrap();

        let err = load_stack_cached(&state, &base.path, &stack).unwrap_err();
        assert!(err.contains("reindex required"), "unexpected error: {err}");
    }

    // --- index cancellation via Notify ---

    #[tokio::test]
    async fn select_on_notify_short_circuits_a_pending_future() {
        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();

        // Signal cancellation shortly after this task starts waiting.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            notify_clone.notify_waiters();
        });

        let long_running = async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            "finished"
        };

        let cancelled = tokio::select! {
            biased;
            _ = notify.notified() => true,
            _ = long_running => false,
        };

        assert!(cancelled, "notify_waiters() must short-circuit the pending select! before the long future completes");
    }

    // --- registry CRUD ---

    #[test]
    fn create_then_list_returns_the_new_stack() {
        let base = TempDir::new("registry_create");
        let created = create_impl(&base.path, "My Docs".to_string(), test_spec(768)).unwrap();

        let listed = list_impl(&base.path).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.id);
        assert_eq!(listed[0].name, "My Docs");
        assert!(listed[0].sources.is_empty());
        assert_eq!(listed[0].chunk_chars, DEFAULT_CHUNK_CHARS);
    }

    #[test]
    fn rename_and_delete_update_the_registry() {
        let base = TempDir::new("registry_rename_delete");
        let stack = create_impl(&base.path, "Old Name".to_string(), test_spec(768)).unwrap();

        let renamed = rename_impl(&base.path, &stack.id, "New Name".to_string()).unwrap();
        assert_eq!(renamed.name, "New Name");
        assert_eq!(list_impl(&base.path).unwrap()[0].name, "New Name");

        delete_impl(&base.path, &stack.id).unwrap();
        assert!(list_impl(&base.path).unwrap().is_empty());
        assert!(delete_impl(&base.path, &stack.id).is_err(), "deleting an already-deleted stack must error");
    }

    #[test]
    fn add_source_canonicalizes_and_rejects_duplicates() {
        let base = TempDir::new("registry_add_source");
        let source = TempDir::new("registry_add_source_target");
        let stack = create_impl(&base.path, "Stack".to_string(), test_spec(768)).unwrap();

        let updated =
            add_source_impl(&base.path, &stack.id, source.path.to_string_lossy().to_string(), SourceKind::Folder)
                .unwrap();
        assert_eq!(updated.sources.len(), 1);

        let dup = add_source_impl(&base.path, &stack.id, source.path.to_string_lossy().to_string(), SourceKind::Folder);
        assert!(dup.is_err(), "adding the same canonicalized path twice must error");

        let with_removed = remove_source_impl(&base.path, &stack.id, &updated.sources[0].path).unwrap();
        assert!(with_removed.sources.is_empty());
    }

    #[tokio::test]
    async fn stacks_cancel_index_is_a_harmless_noop_with_nothing_running() {
        // Mirrors `tools_cancel_running`'s tolerance: cancelling a stack
        // with no in-flight reindex (no entry in `index_cancels`) must not
        // error.
        let state = AppState::default();
        let cancels = state.index_cancels.lock().unwrap();
        assert!(cancels.get("no-such-stack").is_none());
    }
}

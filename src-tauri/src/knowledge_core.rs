//! The stack registry and the embedding core: the two pieces of the original
//! v1 Knowledge Stacks module (`stacks.rs`) that Knowledge 2.0
//! (`knowledge_service.rs`/`knowledge_pipeline.rs`) genuinely shares with it.
//!
//! Why this module exists: v1 and v2 are two different *indexes* over one
//! *registry*. `<app_data>/stacks/index.json` (a plain `Vec<KnowledgeStack>`)
//! is the single list of what stacks exist, what folders/files feed them, and
//! which embedding model their vectors were produced with — and both index
//! generations read and write it. So does the portable-bundle importer
//! (`portability_commands.rs`), the diagnostics sweep, and `monkey-cli`. The
//! same is true of embedding: v1's `reindex_impl` and v2's generation build
//! both turn text into vectors through [`embed_batch`], against the same
//! [`EmbeddingSpec`], with the same L2 normalization and the same
//! dimension-mismatch hard-fail. None of that is v1-specific, yet all of it
//! used to live inside v1's module, which meant v2 imported from the very
//! module the roadmap intends to delete. Extracting it here breaks that
//! dependency *structurally*: nothing in this module needs v1.
//!
//! It now breaks it in fact too, for the shared items: every call site that
//! wanted the registry or the embedding path spells `crate::knowledge_core::…` /
//! `little_monkey_lib::knowledge_core::…` directly, so `stacks.rs`'s re-export
//! block is no longer load-bearing for any of them. What still reaches through
//! it is v1 *index* behaviour — `ChunkMeta` (v1's `chunks.jsonl` row type, and
//! the importer's input type), `query_impl`, `reindex_impl`, `stacks_reindex` —
//! which is exactly the set that dies with `stacks.rs` rather than moving here.
//!
//! What is deliberately NOT here: everything that is an artefact of v1's
//! *index format* rather than of the registry — `chunks.jsonl`/`vectors.bin`
//! I/O, the character-boundary chunker, brute-force dot-product ranking, the
//! incremental-reindex planner, the `LoadedStack` cache. Those stay in
//! `stacks.rs` and die with it. v2 has its own equivalents in
//! `knowledge_pipeline.rs` and never called v1's.
//!
//! The one later addition is the local-source staleness walk at the bottom of
//! this file ([`source_has_newer_mtime`]). It arrived here rather than staying
//! in `stacks.rs` because v2 grew a staleness probe of its own
//! (`knowledge_service::v2_staleness_impl`) and "has a local file changed since
//! we indexed?" is a question about the *registry's* sources, not about either
//! index format — two implementations of it would have drifted on exactly the
//! extension/size exclusions that keep the badge from lying.
//!
//! This module is Tauri-*light*, not Tauri-free: [`stacks_base_dir`] resolves
//! the app-data path from an `AppHandle` exactly as it did in `stacks.rs`, and
//! is the reason `monkey-cli` and every `*_impl` below take a plain
//! `base: &Path` instead — the same split `checkpoints.rs`/`rules.rs`/
//! `memory.rs` use. It holds no `AppState` and no cache, so nothing here needs
//! the desktop app to be running.
//!
//! `stacks.rs` re-exports every `pub` item below, so this extraction changed
//! no call site anywhere in the crate and no test assertion. See the re-export
//! block at the top of `stacks.rs`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle};
use crate::profiles::ProfileScopedPaths;

/// Default target chunk size (characters), per `KnowledgeStack::chunk_chars`.
pub(crate) const DEFAULT_CHUNK_CHARS: usize = 1600;
/// Default overlap (characters) carried from one chunk into the next, per
/// `KnowledgeStack::chunk_overlap`.
pub(crate) const DEFAULT_CHUNK_OVERLAP: usize = 200;

/// How many texts are embedded per HTTP request, both at index time and
/// query time (queries are always a batch of one, so this only matters for
/// indexing).
pub(crate) const EMBED_BATCH_SIZE: usize = 32;

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

/// One retrieval hit, returned by `stacks_query`.
///
/// Shared, not v1-specific: `knowledge_service::query_for_agent` builds these
/// out of v2 hybrid-search hits so a caller ranks over one result shape
/// regardless of which index generation answered (see `merge_stack_results` in
/// `stacks.rs` for why the two generations' `score` fields must still never be
/// compared against each other).
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

/// The app-data directory both index generations hang off of: v1's registry and
/// `chunks.jsonl`/`vectors.bin` live under `<app_data>/stacks`, v2's catalog and
/// generations under `<app_data>/knowledge-v2`. `monkey-cli` resolves the same
/// path `AppHandle`-free via `app_paths::data_dir`, which is why every `*_impl`
/// and `*_at` entry point takes a plain `&Path` and only the thin Tauri wrappers
/// call this.
pub(crate) fn app_data_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.profile_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {e}"))
}

/// Resolves (and creates) `<app_data>/stacks`, the directory every `*_impl`
/// here takes as its `base`. One of the two `AppHandle`-dependent functions in
/// this module — see the module doc for why the split is drawn here.
pub(crate) fn stacks_base_dir(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app_data_dir(app)?.join("stacks");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create stacks dir: {e}"))?;
    Ok(dir)
}

fn registry_path(base: &Path) -> PathBuf {
    base.join("index.json")
}

/// Reject anything that isn't a plain UUID-shaped id, so a crafted id can
/// never traverse outside the stacks directory (mirrors
/// `checkpoints::validate_id`).
pub(crate) fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        Ok(())
    } else {
        Err(format!("Invalid stack id '{}'", id))
    }
}

pub(crate) fn load_registry(base: &Path) -> Result<Vec<KnowledgeStack>, String> {
    let path = registry_path(base);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(&raw).map_err(|e| format!("Failed to parse stacks registry: {e}"))
}

/// Atomic registry write: sibling temp file + rename, same idiom as
/// `checkpoints.rs`'s manifest writes / `sessions.rs`.
pub(crate) fn save_registry(base: &Path, stacks: &[KnowledgeStack]) -> Result<(), String> {
    let path = registry_path(base);
    let payload = serde_json::to_string_pretty(stacks)
        .map_err(|e| format!("Failed to serialize stacks registry: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------
// Registry CRUD (AppHandle-free core)
// ---------------------------------------------------------------------

pub fn list_impl(base: &Path) -> Result<Vec<KnowledgeStack>, String> {
    load_registry(base)
}

/// Records a successfully activated Knowledge 2.0 generation in the shared
/// stack registry. This keeps attachment badges and doc-chat readiness in
/// sync while the underlying v2 index remains independently immutable.
pub fn mark_v2_indexed_impl(
    base: &Path,
    id: &str,
    indexed_at: u64,
    chunk_count: usize,
) -> Result<KnowledgeStack, String> {
    validate_id(id)?;
    let mut stacks = load_registry(base)?;
    let stack = stacks
        .iter_mut()
        .find(|stack| stack.id == id)
        .ok_or_else(|| format!("Stack '{id}' not found"))?;
    stack.indexed_at = Some(indexed_at);
    stack.chunk_count = chunk_count;
    let updated = stack.clone();
    save_registry(base, &stacks)?;
    Ok(updated)
}

pub fn create_impl(
    base: &Path,
    name: String,
    embedding: EmbeddingSpec,
) -> Result<KnowledgeStack, String> {
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

/// Imports portable stack *definitions* after the outer bundle has passed
/// hostile-archive preflight. Vector/chunk indexes are deliberately reset:
/// they are rebuildable, machine-specific data and never travel in M1
/// bundles. Merge preserves stable ids when free and leaves an existing
/// conflicting definition untouched; replace is reserved for explicit
/// snapshot restore.
pub fn import_definitions_impl(
    base: &Path,
    incoming: Vec<KnowledgeStack>,
    replace: bool,
) -> Result<Vec<KnowledgeStack>, String> {
    let mut normalized = Vec::with_capacity(incoming.len());
    let mut ids = std::collections::HashSet::new();
    for mut stack in incoming {
        validate_id(&stack.id)?;
        if !ids.insert(stack.id.clone()) {
            return Err(format!("Portable stack id '{}' is duplicated", stack.id));
        }
        if stack.name.trim().is_empty() || stack.name.len() > 512 {
            return Err("Portable stack name must be 1..=512 bytes".to_string());
        }
        if stack.embedding.model_id_or_tag.trim().is_empty()
            || stack.embedding.model_id_or_tag.len() > 1_024
            || stack.embedding.dim == 0
            || stack.embedding.dim > 65_536
            || stack.chunk_chars == 0
            || stack.chunk_chars > 1_000_000
            || stack.chunk_overlap >= stack.chunk_chars
        {
            return Err(format!(
                "Portable stack '{}' has invalid embedding/chunk settings",
                stack.id
            ));
        }
        if stack.sources.len() > 100_000
            || stack.sources.iter().any(|source| {
                source.path.is_empty() || source.path.len() > 32_768 || source.path.contains('\0')
            })
        {
            return Err(format!("Portable stack '{}' has invalid sources", stack.id));
        }
        stack.indexed_at = None;
        stack.chunk_count = 0;
        normalized.push(stack);
    }
    let registry = if replace {
        normalized
    } else {
        let mut registry = load_registry(base)?;
        let known = registry
            .iter()
            .map(|stack| stack.id.clone())
            .collect::<std::collections::HashSet<_>>();
        registry.extend(
            normalized
                .into_iter()
                .filter(|stack| !known.contains(&stack.id)),
        );
        registry
    };
    save_registry(base, &registry)?;
    Ok(registry)
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

/// Updates the chunking definition used by the immutable Knowledge 2.0 build
/// pipeline. The active generation remains usable until the caller completes
/// a successful refresh with the new fingerprint.
pub fn update_chunking_impl(
    base: &Path,
    id: &str,
    chunk_chars: usize,
    chunk_overlap: usize,
) -> Result<KnowledgeStack, String> {
    validate_id(id)?;
    if chunk_chars == 0 || chunk_chars > 1_000_000 || chunk_overlap >= chunk_chars {
        return Err(
            "Chunk size must be 1..=1000000 characters and overlap must be smaller than the chunk"
                .to_string(),
        );
    }
    let mut registry = load_registry(base)?;
    let stack = registry
        .iter_mut()
        .find(|stack| stack.id == id)
        .ok_or_else(|| format!("Stack '{id}' not found"))?;
    stack.chunk_chars = chunk_chars;
    stack.chunk_overlap = chunk_overlap;
    let updated = stack.clone();
    save_registry(base, &registry)?;
    Ok(updated)
}

pub fn add_source_impl(
    base: &Path,
    id: &str,
    path: String,
    kind: SourceKind,
) -> Result<KnowledgeStack, String> {
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
        return Err(format!(
            "'{}' is already a source of this stack",
            canonical_str
        ));
    }
    stack.sources.push(StackSource {
        path: canonical_str,
        kind,
    });
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
// Name resolution / scoping
// ---------------------------------------------------------------------

/// Resolves which stack ids `tool_search_docs` should actually search.
///
/// `allowed_names` is the scoping boundary: `Some(names)` restricts EVERY
/// resolution below (both the explicit-name and the default-sweep case) to
/// just the registry entries whose name case-insensitively matches one of
/// `names` — this is how a session's actually-attached stacks (or, for
/// `monkey-cli`, the stacks named via `--stack`) keep a `search_docs` call from
/// reaching a knowledge stack that was never granted to this
/// session/invocation, even one that happens to be indexed. `None` means "no
/// restriction, consider the whole registry" — used only by `monkey-cli` when it
/// has no `--stack` at all to scope by (see `stacks_cli::search_docs`); the
/// desktop app's `tool_search_docs` always passes `Some(...)` (an empty
/// `Vec` when nothing is attached), never `None`, so a hallucinated call in a
/// session with nothing attached fails closed rather than sweeping in every
/// indexed stack on the machine — see `stacks.rs`'s top doc comment and the
/// slice 2 summary for the privacy gap this closes.
///
/// - `stack: Some(name)` — the single (in-scope) stack whose name matches
///   `name` case-insensitively (trimmed first, since a model may pad its
///   argument with whitespace). A name that matches nothing IN SCOPE is a
///   hard error rather than an empty result, so the model learns immediately
///   that it mistyped/hallucinated a stack name (or named one outside this
///   session's attachments) instead of silently getting zero hits back. If
///   the matched stack hasn't been indexed yet, this still returns its id —
///   `query_impl` is what surfaces the "has not been indexed yet" error, so
///   that message stays in exactly one place.
/// - `stack: None` — every IN-SCOPE stack that HAS been indexed.
///
/// `pub` (not module-private) so `monkey-cli`'s `stacks_cli::search_docs` (slice
/// 4 CLI parity) can resolve a `--stack`-style name argument through the
/// exact same logic `tool_search_docs` uses, rather than re-implementing
/// name matching a second time.
pub fn resolve_search_stack_ids(
    registry: &[KnowledgeStack],
    allowed_names: Option<&[String]>,
    stack: Option<&str>,
) -> Result<Vec<String>, String> {
    let in_scope = |s: &KnowledgeStack| {
        allowed_names
            .map(|names| {
                names
                    .iter()
                    .any(|n| n.trim().eq_ignore_ascii_case(s.name.trim()))
            })
            .unwrap_or(true)
    };

    match stack {
        Some(name) => {
            let trimmed = name.trim();
            registry
                .iter()
                .filter(|s| in_scope(s))
                .find(|s| s.name.eq_ignore_ascii_case(trimmed))
                .map(|s| vec![s.id.clone()])
                .ok_or_else(|| {
                    format!(
                        "No knowledge stack named '{}'{}",
                        trimmed,
                        if allowed_names.is_some() {
                            " attached to this session"
                        } else {
                            ""
                        }
                    )
                })
        }
        None => {
            let ids: Vec<String> = registry
                .iter()
                .filter(|s| in_scope(s) && s.indexed_at.is_some())
                .map(|s| s.id.clone())
                .collect();
            if ids.is_empty() {
                Err("No indexed knowledge stacks are available to search".to_string())
            } else {
                Ok(ids)
            }
        }
    }
}

// ---------------------------------------------------------------------
// Embedding core
// ---------------------------------------------------------------------

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

async fn embed_via_llama(model: &str, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
    let client = reqwest::Client::new();
    let resp = crate::egress::send(
        client
            .post(format!("http://127.0.0.1:{}/v1/embeddings", crate::llama::EMBED_PORT))
            .json(&serde_json::json!({ "model": model, "input": texts }))
            .timeout(std::time::Duration::from_secs(60)),
    )
    .await
    .map_err(|e| {
            format!(
                "Failed to reach the embedding server: {e} — start it first from the desktop app's Settings > \
                 Knowledge tab, or (from a terminal) `monkey stacks embed-server start --model-path <path>`."
            )
        })?;

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

    let parsed: EmbeddingResponse = resp
        .json()
        .await
        .map_err(|e| format!("Failed to parse embedding response: {e}"))?;

    // Mirrors `ollama::embed`'s own count check: a backend that silently
    // returns fewer (or more) embeddings than texts requested must be a hard
    // error here, not an unfilled `vector_slots` entry that later panics via
    // `.expect(...)` in `reindex_impl` — see that `.expect` call's own doc
    // comment for why every slot is guaranteed filled ONLY if this holds.
    if parsed.data.len() != texts.len() {
        return Err(format!(
            "Embedding server returned {} embeddings for {} inputs",
            parsed.data.len(),
            texts.len()
        ));
    }

    Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
}

/// Embeds `texts` against `spec`, dispatching to the recorded backend,
/// applying `spec.query_prefix`/`spec.doc_prefix` (per `is_query`) to every
/// text first, batching requests at [`EMBED_BATCH_SIZE`], and L2-normalizing
/// every returned vector. Hard-fails (rather than silently proceeding) if
/// the model's actual output dimensionality doesn't match `spec.dim` — see
/// [`spec_matches`]'s doc comment for why that must never be papered over.
pub async fn embed_batch(
    spec: &EmbeddingSpec,
    texts: &[String],
    is_query: bool,
) -> Result<Vec<Vec<f32>>, String> {
    if texts.is_empty() {
        return Ok(Vec::new());
    }
    let prefix = if is_query {
        &spec.query_prefix
    } else {
        &spec.doc_prefix
    };
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
// Local-source staleness probe
// ---------------------------------------------------------------------

/// Files larger than this are skipped during indexing (silently, like a
/// binary file) — a single huge log/data file shouldn't dominate a stack's
/// chunk budget or indexing time.
pub(crate) const MAX_FILE_BYTES: u64 = 5_000_000;

/// Extension allowlist for indexable files — text formats plus common code
/// files. Matched case-insensitively.
const ALLOWED_EXTENSIONS: &[&str] = &[
    "md", "markdown", "txt", "rst", "json", "yaml", "yml", "toml", "csv", "html", "htm", "rs",
    "ts", "tsx", "js", "jsx", "py", "go", "java", "c", "cpp", "cc", "h", "hpp", "cs", "rb", "php",
    "swift", "kt", "sh", "sql",
];

/// True for an extension v1's `read_indexable_file` would ever actually read
/// (`.pdf` when the `pdf-extraction` feature is compiled in, or anything in
/// [`ALLOWED_EXTENSIONS`]) — factored out so [`source_has_newer_mtime`] can
/// apply the exact same extension gate without duplicating (and risking
/// drifting from) `read_indexable_file`'s own check.
pub(crate) fn is_indexable_extension(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_lowercase();

    #[cfg(feature = "pdf-extraction")]
    if ext == "pdf" {
        return true;
    }

    ALLOWED_EXTENSIONS.contains(&ext.as_str())
}

pub(crate) fn mtime_ms(metadata: &std::fs::Metadata) -> Option<u64> {
    metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// True if `path` itself (a file source) or any file reachable under it (a
/// folder source, walked the same way `collect_source_files` does) has an
/// mtime after `indexed_at_ms`.
///
/// Shared by both index generations: v1's `stacks::is_stale_impl` (per stack,
/// against `KnowledgeStack::indexed_at`) and v2's
/// `knowledge_service::v2_staleness_impl` (per local connector, against the
/// active generation's `created_unix_ms`). It is `stat`-only on purpose —
/// both callers fan it out across every indexed stack on panel mount, so it
/// must never read file contents or touch the network.
pub(crate) fn source_has_newer_mtime(path: &Path, indexed_at_ms: u64) -> bool {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return true,
    };

    if !metadata.is_dir() {
        return mtime_ms(&metadata)
            .map(|mtime| mtime > indexed_at_ms)
            .unwrap_or(true);
    }

    let walker = walkdir::WalkDir::new(path)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            if entry.depth() > 0 && entry.file_type().is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    return !crate::tools::MENTION_SKIP_DIRS.contains(&name);
                }
            }
            true
        });
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        // Match `collect_source_files`/`read_indexable_file`'s own
        // extension-allowlist and size-cap gates — a touched file that
        // indexing would never actually look at (wrong extension, an
        // oversized log file, etc.) must not flip the stale badge, or
        // reindexing would be "recommended" for a change that produces zero
        // new/changed chunks. Skipped here on metadata alone (no content
        // read), so this stays a cheap `stat`-only check like the rest of
        // this function; the binary-content-sniff/UTF-8-validity gates
        // `read_indexable_file` also applies aren't replicated since those
        // require reading the file, which this check deliberately doesn't do.
        if !is_indexable_extension(entry.path()) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
            continue;
        }
        if mtime_ms(&metadata)
            .map(|mtime| mtime > indexed_at_ms)
            .unwrap_or(true)
        {
            return true;
        }
    }
    false
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
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "little_monkey_stacks_test_{}_{}_{}_{}",
                tag,
                std::process::id(),
                n,
                nanos
            ));
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

    fn test_stack(name: &str, indexed: bool) -> KnowledgeStack {
        KnowledgeStack {
            id: format!("id-{name}"),
            name: name.to_string(),
            sources: Vec::new(),
            embedding: test_spec(768),
            chunk_chars: DEFAULT_CHUNK_CHARS,
            chunk_overlap: DEFAULT_CHUNK_OVERLAP,
            indexed_at: if indexed { Some(1) } else { None },
            chunk_count: if indexed { 10 } else { 0 },
        }
    }

    // --- embedding-spec mismatch hard-fails ---

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
    fn chunking_update_is_validated_and_persisted_without_destroying_active_metadata() {
        let base = TempDir::new("registry_chunking");
        let mut created = create_impl(&base.path, "Docs".to_string(), test_spec(768)).unwrap();
        created.indexed_at = Some(42);
        created.chunk_count = 7;
        save_registry(&base.path, &[created.clone()]).unwrap();

        let updated = update_chunking_impl(&base.path, &created.id, 900, 120).unwrap();
        assert_eq!(updated.chunk_chars, 900);
        assert_eq!(updated.chunk_overlap, 120);
        assert_eq!(updated.indexed_at, Some(42));
        assert_eq!(updated.chunk_count, 7);
        let persisted = list_impl(&base.path).unwrap().remove(0);
        assert_eq!(persisted.id, updated.id);
        assert_eq!(persisted.chunk_chars, updated.chunk_chars);
        assert_eq!(persisted.chunk_overlap, updated.chunk_overlap);
        assert_eq!(persisted.indexed_at, updated.indexed_at);
        assert_eq!(persisted.chunk_count, updated.chunk_count);
        assert!(update_chunking_impl(&base.path, &created.id, 100, 100).is_err());
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
        assert!(
            delete_impl(&base.path, &stack.id).is_err(),
            "deleting an already-deleted stack must error"
        );
    }

    #[test]
    fn add_source_canonicalizes_and_rejects_duplicates() {
        let base = TempDir::new("registry_add_source");
        let source = TempDir::new("registry_add_source_target");
        let stack = create_impl(&base.path, "Stack".to_string(), test_spec(768)).unwrap();

        let updated = add_source_impl(
            &base.path,
            &stack.id,
            source.path.to_string_lossy().to_string(),
            SourceKind::Folder,
        )
        .unwrap();
        assert_eq!(updated.sources.len(), 1);

        let dup = add_source_impl(
            &base.path,
            &stack.id,
            source.path.to_string_lossy().to_string(),
            SourceKind::Folder,
        );
        assert!(
            dup.is_err(),
            "adding the same canonicalized path twice must error"
        );

        let with_removed =
            remove_source_impl(&base.path, &stack.id, &updated.sources[0].path).unwrap();
        assert!(with_removed.sources.is_empty());
    }

    // --- tool_search_docs stack-name resolution ---

    #[test]
    fn resolve_search_stack_ids_matches_named_stack_case_insensitively() {
        let registry = vec![test_stack("Docs", true), test_stack("Notes", true)];
        let ids = resolve_search_stack_ids(&registry, None, Some("  docs  ")).unwrap();
        assert_eq!(ids, vec!["id-Docs".to_string()]);
    }

    #[test]
    fn resolve_search_stack_ids_errors_for_an_unknown_name() {
        let registry = vec![test_stack("Docs", true)];
        let err = resolve_search_stack_ids(&registry, None, Some("Nonexistent")).unwrap_err();
        assert!(err.contains("Nonexistent"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_search_stack_ids_matches_an_unindexed_named_stack_leaving_the_not_indexed_error_to_query_impl(
    ) {
        // The name resolves fine even though it hasn't been indexed yet —
        // `query_impl` (not this function) is what surfaces "has not been
        // indexed yet", so that message stays in exactly one place.
        let registry = vec![test_stack("Docs", false)];
        let ids = resolve_search_stack_ids(&registry, None, Some("Docs")).unwrap();
        assert_eq!(ids, vec!["id-Docs".to_string()]);
    }

    #[test]
    fn resolve_search_stack_ids_defaults_to_every_indexed_stack_when_no_name_given() {
        let registry = vec![
            test_stack("Docs", true),
            test_stack("Notes", false),
            test_stack("Wiki", true),
        ];
        let mut ids = resolve_search_stack_ids(&registry, None, None).unwrap();
        ids.sort();
        assert_eq!(ids, vec!["id-Docs".to_string(), "id-Wiki".to_string()]);
    }

    #[test]
    fn resolve_search_stack_ids_errors_when_nothing_is_indexed_and_no_name_given() {
        let registry = vec![test_stack("Docs", false)];
        let err = resolve_search_stack_ids(&registry, None, None).unwrap_err();
        assert!(err.contains("No indexed"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_search_stack_ids_errors_on_an_empty_registry_with_no_name_given() {
        let registry: Vec<KnowledgeStack> = Vec::new();
        assert!(resolve_search_stack_ids(&registry, None, None).is_err());
    }

    // --- resolve_search_stack_ids allow-list scoping (privacy fix) ---

    #[test]
    fn resolve_search_stack_ids_default_sweep_is_scoped_to_the_allow_list_only() {
        let registry = vec![
            test_stack("Work Docs", true),
            test_stack("Diary", true),
            test_stack("Wiki", true),
        ];
        let allowed = vec!["Work Docs".to_string()];
        let ids = resolve_search_stack_ids(&registry, Some(&allowed), None).unwrap();
        // "Diary" and "Wiki" are indexed too, but NOT in the allow list — an
        // omitted `stack` argument must never sweep them in.
        assert_eq!(ids, vec!["id-Work Docs".to_string()]);
    }

    #[test]
    fn resolve_search_stack_ids_explicit_name_outside_the_allow_list_is_rejected() {
        let registry = vec![test_stack("Work Docs", true), test_stack("Diary", true)];
        let allowed = vec!["Work Docs".to_string()];
        // The model (or a CLI caller) naming a real, indexed stack that just
        // isn't in scope must still be refused — explicit naming must never
        // bypass the allow list.
        let err = resolve_search_stack_ids(&registry, Some(&allowed), Some("Diary")).unwrap_err();
        assert!(err.contains("Diary"), "unexpected error: {err}");
    }

    #[test]
    fn resolve_search_stack_ids_empty_allow_list_matches_nothing() {
        let registry = vec![test_stack("Docs", true)];
        // An empty allow list (a session with nothing attached) must fail
        // closed — scope to nothing — not fail open to the whole registry.
        assert!(resolve_search_stack_ids(&registry, Some(&[]), None).is_err());
        assert!(resolve_search_stack_ids(&registry, Some(&[]), Some("Docs")).is_err());
    }

    #[test]
    fn resolve_search_stack_ids_no_allow_list_is_unrestricted() {
        // `None` (used by `monkey-cli` when no `--stack` was given at all) keeps
        // the pre-fix "search the whole registry" behavior.
        let registry = vec![test_stack("Docs", true), test_stack("Notes", true)];
        let mut ids = resolve_search_stack_ids(&registry, None, None).unwrap();
        ids.sort();
        assert_eq!(ids, vec!["id-Docs".to_string(), "id-Notes".to_string()]);
    }
}

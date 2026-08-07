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
//! this is what would make a future `monkey-cli` `Stacks` subcommand (slice 4)
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
//! Slices 1-3 of the RAG design doc (stacks + local indexing, the
//! `search_docs` agent tool, and doc-chat mode) are already in place. This
//! module also carries slice 4 (hardening + parity): incremental reindex via
//! a `file_index.json` content-hash map (see `plan_reindex`), optional
//! pure-Rust PDF text extraction (see `read_indexable_pdf`, feature-gated
//! behind the `pdf-extraction` Cargo feature), and a stale-index check (see
//! `is_stale_impl`) backing `KnowledgePanel.tsx`'s stale badge. `reindex_impl`
//! and `query_impl` take a plain `base: &Path` (not an `AppHandle`) — the
//! Tauri command wrappers resolve that path and, for `reindex_impl`, supply a
//! progress callback instead of emitting the `stacks://index-progress` event
//! directly — so `monkey-cli`'s `Stacks` subcommand (`stacks_cli.rs`) can call
//! them exactly like the desktop app does, just rendering progress to the
//! terminal instead of a Tauri event.
//!
//! The registry itself and the embedding path are no longer defined here: both
//! are shared with Knowledge 2.0 and now live in [`crate::knowledge_core`] (see
//! that module's doc comment for why, and for what deliberately stayed behind).
//! What remains in this file is exactly v1's own index format — the
//! `chunks.jsonl`/`vectors.bin` pair, the chunker that fills it, brute-force
//! dot-product ranking over it, incremental reindex planning, the staleness
//! check, and the Tauri commands on top — i.e. the part that goes away when v1
//! is finally collapsed into v2.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

use crate::AppState;

// The shared registry + embedding core this module used to own moved to
// `knowledge_core` so Knowledge 2.0 stops depending on v1. Re-exported from
// here — rather than repointing the ~45 `crate::stacks::…` /
// `little_monkey_lib::stacks::…` references spread across `knowledge_service`,
// `portability_commands`, `diagnostics`, and `monkey-cli` in the same change —
// so the extraction is provably behaviour-neutral: not one call site,
// signature, or test assertion moved with it. Later steps of the v1→v2 collapse
// repoint those callers at `knowledge_core` directly and delete this block
// along with the rest of the file.
pub use crate::knowledge_core::{
    add_source_impl, create_impl, delete_impl, embed_batch, import_definitions_impl, list_impl,
    mark_v2_indexed_impl, remove_source_impl, rename_impl, resolve_search_stack_ids,
    update_chunking_impl, EmbeddingBackend, EmbeddingSpec, KnowledgeStack, SourceKind,
    StackQueryResult, StackSource,
};
use crate::knowledge_core::{
    is_indexable_extension, load_registry, now_ms, save_registry, source_has_newer_mtime,
    spec_matches, stacks_base_dir, validate_id, EMBED_BATCH_SIZE, MAX_FILE_BYTES,
};

/// Hard cap on chunks a single stack may produce. Brute-force dot-product
/// search stays fast well past this (see module doc), but a user pointing a
/// stack at a multi-GB folder would otherwise blow up index time and
/// `vectors.bin` size with no feedback until it's too late — failing fast
/// with a clear message is better than a multi-hour silent index.
const MAX_CHUNKS_PER_STACK: usize = 50_000;

/// Default number of results `stacks_query` returns when the caller doesn't
/// specify `k`.
const DEFAULT_QUERY_K: usize = 6;

// ---------------------------------------------------------------------
// Data model (v1 index format only — the registry types it hangs off of are
// `knowledge_core`'s)
// ---------------------------------------------------------------------

/// One chunk's metadata, one per line of `<stack_dir>/chunks.jsonl`. Row `i`
/// here corresponds to row `i` in the stack's `vectors.bin`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct ChunkMeta {
    pub source_path: String,
    pub ordinal: usize,
    pub text: String,
    /// SHA-256 hex digest of the source file's content at the time it was
    /// chunked (for a PDF, the digest is of the *extracted text*, not the
    /// raw PDF bytes — there's no other stable string to hash). Compared
    /// against the sibling `file_index.json` on every reindex (see
    /// `plan_reindex`): an unchanged hash means this file's rows can be
    /// carried over verbatim instead of being re-chunked and re-embedded.
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
        chunks.push(Chunk {
            heading,
            text: trimmed.to_string(),
        });
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
            && trimmed[hash_prefix_len..]
                .chars()
                .next()
                .is_none_or(|c| c == ' ');
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
                push_chunk(
                    &mut chunks,
                    current_heading_for_chunk.clone(),
                    std::mem::take(&mut current),
                );
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
            push_chunk(
                &mut chunks,
                current_heading_for_chunk.clone(),
                current.clone(),
            );
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
    let mut buf: Vec<u8> =
        Vec::with_capacity(VECTORS_HEADER_LEN + vectors.len() * dim as usize * 4);
    buf.extend_from_slice(&VECTORS_MAGIC);
    buf.extend_from_slice(&VECTORS_VERSION.to_le_bytes());
    buf.extend_from_slice(&dim.to_le_bytes());
    buf.extend_from_slice(&count.to_le_bytes());
    for row in vectors {
        if row.len() != dim as usize {
            return Err(format!(
                "Vector row has {} dims, expected {}",
                row.len(),
                dim
            ));
        }
        for x in row {
            buf.extend_from_slice(&x.to_le_bytes());
        }
    }

    let tmp = path.with_extension("bin.tmp");
    std::fs::write(&tmp, &buf).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

/// Reads a `vectors.bin` back into `(dim, count, flat_rows)`.
fn read_vectors_bin(path: &Path) -> Result<(u32, u32, Vec<f32>), String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    if bytes.len() < VECTORS_HEADER_LEN {
        return Err("vectors.bin is truncated (missing header)".to_string());
    }
    if bytes[0..4] != VECTORS_MAGIC {
        return Err("vectors.bin has an invalid magic header — reindex required".to_string());
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if version != VECTORS_VERSION {
        return Err(format!(
            "vectors.bin has unsupported version {version} — reindex required"
        ));
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

/// Ranks every row of `flat` (row-major, `count` rows of `dim` dims each)
/// against `query` by plain dot product — valid as cosine similarity only
/// because every row (and the query) is L2-normalized before being stored/
/// used. Returns the top `k` `(row_index, score)` pairs, highest score
/// first.
pub fn top_k_by_dot(
    query: &[f32],
    flat: &[f32],
    dim: usize,
    count: usize,
    k: usize,
) -> Vec<(usize, f32)> {
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

/// Extracts a PDF's text via the pure-Rust `pdf-extract` crate — gated
/// behind the `pdf-extraction` Cargo feature (on by default; see
/// `Cargo.toml`'s `[features]` section) so PDF support is an
/// optional/gracefully-degrading addition, not a hard new dependency for
/// users who never index PDFs: a build with the feature disabled simply
/// never matches `.pdf` in [`read_indexable_file`] below, the same as any
/// other unsupported extension, and this function doesn't even exist in
/// that build. Returns `None` (skip, like any other unreadable/empty file)
/// on oversized files, read failures, extraction failures (e.g. an
/// encrypted or malformed PDF), or a PDF whose extracted text is blank
/// (scanned-image-only PDFs have no text layer to extract).
#[cfg(feature = "pdf-extraction")]
fn read_indexable_pdf(path: &Path) -> Option<(String, String)> {
    let metadata = std::fs::metadata(path).ok()?;
    if metadata.len() == 0 || metadata.len() > MAX_FILE_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let text = pdf_extract::extract_text_from_mem(&bytes).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some((path.to_string_lossy().to_string(), text))
}

/// Reads `path` as an indexable file, or returns `None` if it should be
/// skipped: wrong extension, too large, empty, binary, or not valid UTF-8
/// (PDFs are the one exception — handled by [`read_indexable_pdf`] instead,
/// only when the `pdf-extraction` feature is compiled in). Returns
/// `(canonical path string, content)`.
fn read_indexable_file(path: &Path) -> Option<(String, String)> {
    #[cfg(feature = "pdf-extraction")]
    let ext = path.extension().and_then(|e| e.to_str())?.to_lowercase();

    #[cfg(feature = "pdf-extraction")]
    if ext == "pdf" {
        return read_indexable_pdf(path);
    }

    if !is_indexable_extension(path) {
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
/// `cancel` is checked between every source and, for a folder source, between
/// every directory entry — a stack pointed at a large/slow-to-walk tree could
/// otherwise spend a long time in this fully-synchronous function with no
/// chance for `reindex_impl`'s cancellation to take effect until it returns
/// (see that function's doc comment for the cancellation model this is part
/// of). Returns whatever was collected so far the moment cancellation is
/// observed — the caller checks `cancel.is_cancelled()` right after calling
/// this and discards the (possibly partial) result either way, so an early
/// return here is never mistaken for "the real, complete file list".
fn collect_source_files(
    sources: &[StackSource],
    cancel: &CancellationToken,
) -> Vec<(String, String)> {
    let mut files = Vec::new();
    'sources: for source in sources {
        if cancel.is_cancelled() {
            break;
        }
        let path = Path::new(&source.path);
        match source.kind {
            SourceKind::File => {
                if let Some(entry) = read_indexable_file(path) {
                    files.push(entry);
                }
            }
            SourceKind::Folder => {
                let walker = WalkDir::new(path)
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
                    if cancel.is_cancelled() {
                        break 'sources;
                    }
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
        buf.push_str(
            &serde_json::to_string(chunk).map_err(|e| format!("Failed to serialize chunk: {e}"))?,
        );
        buf.push('\n');
    }
    let path = stack_dir.join("chunks.jsonl");
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, buf).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

fn read_chunks_jsonl(stack_dir: &Path) -> Result<Vec<ChunkMeta>, String> {
    let path = stack_dir.join("chunks.jsonl");
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| format!("Corrupt chunk entry: {e}")))
        .collect()
}

fn emit_progress(
    app: &AppHandle,
    stack_id: &str,
    files_done: usize,
    files_total: usize,
    chunks: usize,
    phase: &str,
) {
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

fn file_index_path(stack_dir: &Path) -> PathBuf {
    stack_dir.join("file_index.json")
}

/// Reads `<stack_dir>/file_index.json` — canonical source path -> SHA-256
/// hex digest of that file's content as of the last successful reindex — the
/// map [`plan_reindex`] diffs against to decide which files can skip
/// re-chunking/re-embedding. Missing, unreadable, or corrupt all degrade to
/// an empty map (same "nothing to reuse yet" fallback `plan_reindex` already
/// applies when the sibling `vectors.bin` doesn't line up): incremental
/// reindex is a pure optimization, never a correctness requirement, so a
/// problem reading this file just means the next reindex re-embeds
/// everything instead of failing.
fn read_file_index(stack_dir: &Path) -> HashMap<String, String> {
    let Ok(raw) = std::fs::read_to_string(file_index_path(stack_dir)) else {
        return HashMap::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Atomic write, same temp-file-then-rename idiom as `save_registry`/
/// `write_chunks_jsonl`.
fn write_file_index(stack_dir: &Path, index: &HashMap<String, String>) -> Result<(), String> {
    let path = file_index_path(stack_dir);
    let payload = serde_json::to_string_pretty(index)
        .map_err(|e| format!("Failed to serialize file index: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload).map_err(|e| format!("Failed to write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("Failed to finalize {}: {e}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------
// Stack-directory staging + atomic swap
//
// `chunks.jsonl`/`vectors.bin`/`file_index.json` must all reflect the SAME
// reindex run — `load_stack_cached`'s only cross-check is a row-count
// comparison, so if these three files were each independently
// temp-write-then-rename'd directly into the live stack directory (as they
// used to be), a crash/error between two of those renames could leave the
// stack with (for example) a brand new `chunks.jsonl` paired with the OLD
// `vectors.bin` — same chunk COUNT, silently mismatched CONTENT, which
// `load_stack_cached` would then load and search without ever erroring. The
// fix: build the whole new directory contents in a staging directory, then
// swap it in with two directory-level `rename`s (`stack_dir` -> `stack_dir
// + ".old"`, `staging_dir` -> `stack_dir`) — each individual rename is still
// atomic, but by the time either one lands, the whole write set behind it is
// already complete, so there is no window where a *partial* set of the three
// files is visible under `stack_dir`. A crash between the two renames leaves
// `stack_dir` entirely missing (loud — every read errors "not found") rather
// than silently mismatched; [`recover_stack_dir`] heals exactly that case.
// ---------------------------------------------------------------------

fn staging_dir_path(base: &Path, stack_id: &str) -> PathBuf {
    base.join(format!("{stack_id}.reindex-tmp"))
}

fn backup_dir_path(base: &Path, stack_id: &str) -> PathBuf {
    base.join(format!("{stack_id}.old"))
}

/// Self-heals a stack directory left in the narrow "mid-swap" state a crash
/// between [`swap_stack_dir`]'s two renames would produce: if `stack_dir` is
/// missing but its `.old` backup still exists, the backup IS the last known-
/// good, fully-consistent state, so it's restored. Also clears out any
/// leftover staging directory from an interrupted run — always rebuilt from
/// scratch on the next reindex, so a stale one is never reused. Called at the
/// top of both [`reindex_impl`] (before it reads the "old" state to diff
/// against) and [`load_stack_cached`] (before any query ever reads the
/// directory), so this narrow window is closed on every entry point, not
/// just the one that happened to cause it.
fn recover_stack_dir(base: &Path, stack_id: &str) {
    let stack_dir = base.join(stack_id);
    let backup_dir = backup_dir_path(base, stack_id);
    if !stack_dir.exists() && backup_dir.exists() {
        let _ = std::fs::rename(&backup_dir, &stack_dir);
    }
    let _ = std::fs::remove_dir_all(staging_dir_path(base, stack_id));
}

/// Atomically replaces `stack_dir` (`base/stack_id`) with `staging_dir`'s
/// contents via two directory renames (see this section's module doc for
/// why one rename per file isn't enough). `staging_dir` must already contain
/// the complete, self-consistent new `chunks.jsonl` + `vectors.bin` +
/// `file_index.json` — nothing is written here.
fn swap_stack_dir(base: &Path, stack_id: &str, staging_dir: &Path) -> Result<(), String> {
    let stack_dir = base.join(stack_id);
    let backup_dir = backup_dir_path(base, stack_id);
    let _ = std::fs::remove_dir_all(&backup_dir);

    let had_previous = stack_dir.exists();
    if had_previous {
        std::fs::rename(&stack_dir, &backup_dir).map_err(|e| {
            format!("Failed to stage previous stack directory for replacement: {e}")
        })?;
    }
    if let Err(e) = std::fs::rename(staging_dir, &stack_dir) {
        // Best-effort restore so a failure here doesn't leave the stack
        // directory missing when the previous state was still perfectly
        // good and available right next to it.
        if had_previous {
            let _ = std::fs::rename(&backup_dir, &stack_dir);
        }
        return Err(format!("Failed to finalize stack directory: {e}"));
    }
    let _ = std::fs::remove_dir_all(&backup_dir);
    Ok(())
}

// ---------------------------------------------------------------------
// Reindex pipeline
// ---------------------------------------------------------------------

/// The pure "what needs (re-)embedding" decision behind incremental
/// reindex, factored out of `reindex_impl` so it's unit-testable without any
/// network/embedding call: given the current file list and the previous
/// run's `file_index.json`/`chunks.jsonl`/`vectors.bin`, decides — per file —
/// whether its rows can be carried over verbatim (`vector_slots[i] =
/// Some(old_vector)`, nothing added to `to_embed_texts`) or must be
/// (re-)chunked and queued for embedding (`vector_slots[i] = None`, its text
/// appended to `to_embed_texts`). `reindex_impl` only ever calls
/// `embed_batch` on `to_embed_texts` — see this struct's fields for exactly
/// what it needs to do that and then reassemble the final `chunks.jsonl`/
/// `vectors.bin` in the same pass.
struct ReindexPlan {
    /// The stack's full new chunk list, in final on-disk order. Row `i` here
    /// is row `i` of `vector_slots` below and (once every `None` is filled
    /// in) of the new `vectors.bin`.
    all_chunks: Vec<ChunkMeta>,
    /// Parallel to `all_chunks`: `Some(vector)` for a row carried over from
    /// the previous index, `None` for a row still awaiting embedding.
    vector_slots: Vec<Option<Vec<f32>>>,
    /// Indices into `all_chunks`/`vector_slots` that are still `None`, in
    /// the same order as `to_embed_texts` — `to_embed_indices[i]`'s text is
    /// `to_embed_texts[i]`, so a returned embedding can be written straight
    /// back to `vector_slots[to_embed_indices[i]]`.
    to_embed_indices: Vec<usize>,
    /// The chunk text needing embedding, in `all_chunks` order. Empty when
    /// every current file's content hash matched `file_index.json` — the
    /// "mostly-static folder" case this whole mechanism exists for.
    to_embed_texts: Vec<String>,
    /// This run's new `file_index.json` contents — every current file's
    /// path mapped to its just-computed content hash, superseding whatever
    /// was read in as `old_file_index`.
    new_file_index: HashMap<String, String>,
}

/// Builds a [`ReindexPlan`] for `stack` from its current `files` (as
/// `collect_source_files` returns them) plus the previous run's on-disk
/// state. `old_vectors` is `Some((dim, count, flat))` only when the sibling
/// `vectors.bin` was readable — reuse additionally requires `dim` to match
/// `stack.embedding.dim` and `count` to match `old_chunks.len()`; either
/// mismatch (a spec change, a hand-edited/corrupt file, or simply no
/// previous index at all) falls back to treating every file as needing
/// embedding, exactly like a first-ever index — `content_hash` still gets
/// computed and recorded either way, so the *next* reindex after a spec
/// change is incremental again. `on_file_done(files_done, files_total,
/// chunks_so_far)` is called once per file purely for progress reporting;
/// tests pass a no-op closure. `cancel` is checked once per file (hashing a
/// large file's content is real CPU work, same reasoning as
/// `collect_source_files`'s per-entry check) — like that function, a
/// cancelled run simply returns whatever's built so far; the caller checks
/// `cancel.is_cancelled()` right after and discards the (possibly partial)
/// plan either way.
fn plan_reindex(
    stack: &KnowledgeStack,
    files: &[(String, String)],
    old_file_index: &HashMap<String, String>,
    old_chunks: &[ChunkMeta],
    old_vectors: Option<(u32, u32, &[f32])>,
    cancel: &CancellationToken,
    mut on_file_done: impl FnMut(usize, usize, usize),
) -> ReindexPlan {
    let dim = stack.embedding.dim as usize;
    let reuse_old = old_vectors
        .map(|(old_dim, old_count, _)| {
            old_dim == stack.embedding.dim && old_count as usize == old_chunks.len()
        })
        .unwrap_or(false);
    let old_flat: &[f32] = old_vectors.map(|(_, _, flat)| flat).unwrap_or(&[]);

    // Every old row for a given source path, in original (ordinal) order —
    // popped from the front as they're claimed below, so a file that used to
    // produce N chunks and still does gets its N old rows back in order.
    let mut old_rows_by_path: HashMap<&str, VecDeque<usize>> = HashMap::new();
    if reuse_old {
        for (row, chunk) in old_chunks.iter().enumerate() {
            old_rows_by_path
                .entry(chunk.source_path.as_str())
                .or_default()
                .push_back(row);
        }
    }

    let mut all_chunks: Vec<ChunkMeta> = Vec::new();
    let mut vector_slots: Vec<Option<Vec<f32>>> = Vec::new();
    let mut to_embed_indices: Vec<usize> = Vec::new();
    let mut to_embed_texts: Vec<String> = Vec::new();
    let mut new_file_index: HashMap<String, String> = HashMap::with_capacity(files.len());

    for (i, (source_path, content)) in files.iter().enumerate() {
        if cancel.is_cancelled() {
            break;
        }
        let content_hash = sha256_hex(content);
        new_file_index.insert(source_path.clone(), content_hash.clone());
        let unchanged = reuse_old && old_file_index.get(source_path) == Some(&content_hash);

        if unchanged {
            // This file's content (and therefore its chunk boundaries)
            // hasn't changed since the last index — carry its previous rows
            // over untouched rather than re-chunking/re-embedding them. A
            // file present in `file_index.json` but with zero rows in
            // `old_chunks` (shouldn't normally happen, but not fatal) simply
            // contributes nothing here, same as if it were empty.
            if let Some(rows) = old_rows_by_path.get_mut(source_path.as_str()) {
                while let Some(row) = rows.pop_front() {
                    all_chunks.push(old_chunks[row].clone());
                    vector_slots.push(Some(old_flat[row * dim..(row + 1) * dim].to_vec()));
                }
            }
        } else {
            for (ordinal, chunk) in chunk_text(content, stack.chunk_chars, stack.chunk_overlap)
                .into_iter()
                .enumerate()
            {
                all_chunks.push(ChunkMeta {
                    source_path: source_path.clone(),
                    ordinal,
                    text: chunk.text.clone(),
                    content_hash: content_hash.clone(),
                    heading: chunk.heading,
                });
                to_embed_indices.push(all_chunks.len() - 1);
                to_embed_texts.push(chunk.text);
                vector_slots.push(None);
            }
        }

        on_file_done(i + 1, files.len(), all_chunks.len());
    }

    ReindexPlan {
        all_chunks,
        vector_slots,
        to_embed_indices,
        to_embed_texts,
        new_file_index,
    }
}

/// Walks `stack`'s sources, incrementally re-chunks/re-embeds only the files
/// that changed since the last reindex (see [`plan_reindex`]), and atomically
/// swaps in a new stack directory containing `chunks.jsonl` + `vectors.bin` +
/// `file_index.json` as a single consistent set (see the "Stack-directory
/// staging + atomic swap" section above [`swap_stack_dir`]), updating the
/// registry's `indexed_at`/`chunk_count` on success. Reports progress via
/// `on_progress(files_done, files_total, chunks, phase)` — the Tauri command
/// wrapper (`stacks_reindex`) turns that into `stacks://index-progress`
/// events; `monkey-cli`'s `stacks_cli::reindex` renders it to the terminal
/// instead. Cancellable via the `CancellationToken` registered in
/// `AppState::index_cancels` under `stack_id` (see `stacks_cancel_index`):
/// unlike `tokio::sync::Notify::notify_waiters()` (which only wakes a task
/// already parked in `.notified().await` and forgets the signal otherwise —
/// dropping a cancel request that arrives before the embed loop's first
/// `select!` even exists), a `CancellationToken`'s cancelled state is
/// persisted, so it's checked directly (`cancel.is_cancelled()`) right after
/// the walking and chunking phases too, in addition to the `tokio::select!`
/// racing it against each embed batch below (the only step actually worth
/// interrupting mid-await, since it's a network round-trip) — mirroring how
/// `tool_run_shell` races its own cancellation against the child process.
pub async fn reindex_impl(
    base: &Path,
    state: &AppState,
    stack_id: &str,
    mut on_progress: impl FnMut(usize, usize, usize, &str),
) -> Result<KnowledgeStack, String> {
    validate_id(stack_id)?;
    recover_stack_dir(base, stack_id);
    let mut registry = load_registry(base)?;
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
        cancels
            .entry(stack_id.to_string())
            .or_insert_with(|| Arc::new(CancellationToken::new()))
            .clone()
    };
    // RAII-style cleanup so the cancel handle never lingers past this run,
    // whether it finishes normally, errors, or is cancelled.
    let _cleanup = CancelCleanup {
        state,
        stack_id: stack_id.to_string(),
    };

    on_progress(0, 0, 0, "walking");
    let files = collect_source_files(&stack.sources, &cancel);
    if cancel.is_cancelled() {
        return Err("Indexing cancelled".to_string());
    }
    let files_total = files.len();
    if files_total == 0 {
        return Err("No indexable files found in this stack's sources".to_string());
    }

    let stack_dir = base.join(stack_id);
    let old_file_index = read_file_index(&stack_dir);
    let old_chunks = read_chunks_jsonl(&stack_dir).unwrap_or_default();
    let old_vectors_owned = read_vectors_bin(&stack_dir.join("vectors.bin")).ok();
    let old_vectors_ref = old_vectors_owned
        .as_ref()
        .map(|(d, c, flat)| (*d, *c, flat.as_slice()));

    let plan = plan_reindex(
        &stack,
        &files,
        &old_file_index,
        &old_chunks,
        old_vectors_ref,
        &cancel,
        |done, total, chunks| {
            on_progress(done, total, chunks, "chunking");
        },
    );
    if cancel.is_cancelled() {
        return Err("Indexing cancelled".to_string());
    }

    if plan.all_chunks.len() > MAX_CHUNKS_PER_STACK {
        return Err(format!(
            "This stack would produce {} chunks, over the {} limit — narrow its sources or split it into multiple stacks",
            plan.all_chunks.len(),
            MAX_CHUNKS_PER_STACK
        ));
    }

    let mut vector_slots = plan.vector_slots;
    let reused_count = plan.all_chunks.len() - plan.to_embed_texts.len();
    let mut embedded_so_far = 0usize;
    for start in (0..plan.to_embed_texts.len()).step_by(EMBED_BATCH_SIZE) {
        let end = (start + EMBED_BATCH_SIZE).min(plan.to_embed_texts.len());
        let batch_texts = plan.to_embed_texts[start..end].to_vec();
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err("Indexing cancelled".to_string());
            }
            result = embed_batch(&stack.embedding, &batch_texts, false) => {
                let vecs = result?;
                for (slot_idx, vec) in plan.to_embed_indices[start..end].iter().zip(vecs.into_iter()) {
                    vector_slots[*slot_idx] = Some(vec);
                }
            }
        }
        embedded_so_far += end - start;
        on_progress(
            files_total,
            files_total,
            reused_count + embedded_so_far,
            "embedding",
        );
    }

    let vectors: Vec<Vec<f32>> = vector_slots
        .into_iter()
        .map(|v| v.expect("plan_reindex must fill every chunk slot via reuse or embedding"))
        .collect();

    // Build the complete new directory contents in a staging directory, then
    // swap it in with one atomic directory-level replace — see the
    // "Stack-directory staging + atomic swap" section above for why writing
    // straight into `stack_dir` (as this used to) risks a partial write set
    // becoming visible to a concurrent/later load.
    let staging_dir = staging_dir_path(base, stack_id);
    let _ = std::fs::remove_dir_all(&staging_dir);
    std::fs::create_dir_all(&staging_dir)
        .map_err(|e| format!("Failed to create staging directory: {e}"))?;
    write_chunks_jsonl(&staging_dir, &plan.all_chunks)?;
    write_vectors_bin(
        &staging_dir.join("vectors.bin"),
        stack.embedding.dim,
        &vectors,
    )?;
    write_file_index(&staging_dir, &plan.new_file_index)?;
    swap_stack_dir(base, stack_id, &staging_dir)?;

    registry[idx].indexed_at = Some(now_ms());
    registry[idx].chunk_count = plan.all_chunks.len();
    save_registry(base, &registry)?;

    state
        .stack_cache
        .lock()
        .map_err(|_| "Stack-cache lock poisoned".to_string())?
        .remove(stack_id);

    on_progress(files_total, files_total, plan.all_chunks.len(), "done");

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
fn load_stack_cached(
    state: &AppState,
    base: &Path,
    stack: &KnowledgeStack,
) -> Result<Arc<LoadedStack>, String> {
    {
        let cache = state
            .stack_cache
            .lock()
            .map_err(|_| "Stack-cache lock poisoned".to_string())?;
        if let Some(loaded) = cache.get(&stack.id) {
            if spec_matches(&loaded.embedding, &stack.embedding) {
                return Ok(loaded.clone());
            }
        }
    }

    recover_stack_dir(base, &stack.id);
    let stack_dir = base.join(&stack.id);
    let chunks = read_chunks_jsonl(&stack_dir)?;
    let (dim, count, vectors) = read_vectors_bin(&stack_dir.join("vectors.bin"))?;
    if count as usize != chunks.len() {
        return Err("chunks.jsonl and vectors.bin are out of sync — reindex required".to_string());
    }
    if dim != stack.embedding.dim {
        return Err(
            "Indexed vector dimension doesn't match this stack's embedding spec — reindex required"
                .to_string(),
        );
    }

    let loaded = Arc::new(LoadedStack {
        embedding: stack.embedding.clone(),
        chunks,
        dim,
        vectors,
    });
    state
        .stack_cache
        .lock()
        .map_err(|_| "Stack-cache lock poisoned".to_string())?
        .insert(stack.id.clone(), loaded.clone());
    Ok(loaded)
}

/// Splits `stack_ids` into the subset that's actually indexed (and therefore
/// searchable) — factored out of `query_impl` so the "skip the unindexed
/// ones instead of aborting the whole multi-stack query" decision is
/// unit-testable without a live embedding server. A `stack_id` that isn't in
/// the registry at all is still a hard, immediate error (a real bug — a
/// caller passed a bogus id — not a staleness state). If NONE of `stack_ids`
/// turn out to be indexed, this is also a hard error (nothing usable to
/// search at all) naming every unindexed stack — the same "has not been
/// indexed yet" signal a single named, unindexed stack has always surfaced
/// (see `resolve_search_stack_ids`'s doc comment) is preserved for that case.
/// But when at least one requested stack IS indexed, an unindexed sibling
/// among the rest is simply skipped rather than discarding every already-
/// computed result from the good ones — see `query_impl`'s doc comment for
/// the failure mode this avoids.
fn partition_indexed_stacks<'a>(
    registry: &'a [KnowledgeStack],
    stack_ids: &[String],
) -> Result<Vec<&'a KnowledgeStack>, String> {
    let mut indexed = Vec::new();
    let mut unindexed_names = Vec::new();

    for stack_id in stack_ids {
        let stack = registry
            .iter()
            .find(|s| &s.id == stack_id)
            .ok_or_else(|| format!("Stack '{}' not found", stack_id))?;
        if stack.indexed_at.is_some() {
            indexed.push(stack);
        } else {
            unindexed_names.push(stack.name.clone());
        }
    }

    if indexed.is_empty() && !unindexed_names.is_empty() {
        return Err(format!(
            "Stack{} not indexed yet: {}",
            if unindexed_names.len() > 1 { "s" } else { "" },
            unindexed_names.join(", ")
        ));
    }
    Ok(indexed)
}

/// Embeds `query` against every INDEXED stack among `stack_ids` (an
/// unindexed one among several is skipped, not a hard abort — see
/// [`partition_indexed_stacks`]'s doc comment for exactly when this still
/// errors) and returns the combined top-`k` hits across all of them, highest
/// score first. Takes `base: &Path` (not an `AppHandle`) like every other
/// `*_impl` here — `monkey-cli`'s `stacks_cli::search_docs` calls this directly
/// with its own resolved app-data path.
pub async fn query_impl(
    base: &Path,
    state: &AppState,
    stack_ids: &[String],
    query: &str,
    k: usize,
) -> Result<Vec<StackQueryResult>, String> {
    let registry = load_registry(base)?;
    let indexed_stacks = partition_indexed_stacks(&registry, stack_ids)?;

    let mut all_results: Vec<StackQueryResult> = Vec::new();

    for stack in indexed_stacks {
        let loaded = load_stack_cached(state, base, stack)?;

        let query_vectors = embed_batch(&stack.embedding, &[query.to_string()], true).await?;
        let query_vec = query_vectors
            .into_iter()
            .next()
            .ok_or_else(|| "Failed to embed query".to_string())?;

        let ranked = top_k_by_dot(
            &query_vec,
            &loaded.vectors,
            loaded.dim as usize,
            loaded.chunks.len(),
            k,
        );
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

    all_results.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
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
pub fn stacks_create(
    app: AppHandle,
    name: String,
    embedding: EmbeddingSpec,
) -> Result<KnowledgeStack, String> {
    create_impl(&stacks_base_dir(&app)?, name, embedding)
}

#[tauri::command]
pub fn stacks_delete(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
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
pub fn stacks_add_source(
    app: AppHandle,
    id: String,
    path: String,
    kind: SourceKind,
) -> Result<KnowledgeStack, String> {
    add_source_impl(&stacks_base_dir(&app)?, &id, path, kind)
}

#[tauri::command]
pub fn stacks_remove_source(
    app: AppHandle,
    id: String,
    path: String,
) -> Result<KnowledgeStack, String> {
    remove_source_impl(&stacks_base_dir(&app)?, &id, &path)
}

#[tauri::command]
pub async fn stacks_reindex(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<KnowledgeStack, String> {
    let base = stacks_base_dir(&app)?;
    let app_for_progress = app.clone();
    let progress_id = id.clone();
    reindex_impl(
        &base,
        state.inner(),
        &id,
        move |files_done, files_total, chunks, phase| {
            emit_progress(
                &app_for_progress,
                &progress_id,
                files_done,
                files_total,
                chunks,
                phase,
            );
        },
    )
    .await
}

/// Backs `KnowledgePanel.tsx`'s stale-index badge: true when any of `id`'s
/// source files (or, for a folder source, any file the same walk
/// `collect_source_files` uses would find) has a filesystem mtime newer than
/// the stack's `indexed_at` — see `is_stale_impl`. A stack that has never
/// been indexed is never "stale" (its row already shows "not indexed" rather
/// than a staleness badge).
#[tauri::command]
pub fn stacks_is_stale(app: AppHandle, id: String) -> Result<bool, String> {
    let base = stacks_base_dir(&app)?;
    let registry = load_registry(&base)?;
    let stack = registry
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("Stack '{}' not found", id))?;
    Ok(is_stale_impl(stack))
}

/// Best-effort cancellation, like `tools_cancel_running`: if no reindex is
/// currently running for `id`, this is simply a no-op (nothing to cancel).
/// Uses `CancellationToken::cancel()` (a persisted flag), not
/// `tokio::sync::Notify::notify_waiters()` (a fire-and-forget wakeup that's
/// silently lost if nothing happens to be awaiting it at this exact moment)
/// — see `reindex_impl`'s doc comment for why that distinction matters.
#[tauri::command]
pub fn stacks_cancel_index(state: tauri::State<'_, AppState>, id: String) -> Result<(), String> {
    let cancels = state
        .index_cancels
        .lock()
        .map_err(|_| "Index-cancel lock poisoned".to_string())?;
    if let Some(token) = cancels.get(&id) {
        token.cancel();
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
    let base = stacks_base_dir(&app)?;
    let registry = load_registry(&base)?;
    let k = k.unwrap_or(DEFAULT_QUERY_K as u32) as usize;
    let cancel = CancellationToken::new();
    let mut hybrid_groups: Vec<Vec<StackQueryResult>> = Vec::new();
    let mut legacy_ids = Vec::new();
    for id in &stack_ids {
        let stack = registry
            .iter()
            .find(|stack| &stack.id == id)
            .ok_or_else(|| format!("Stack '{id}' not found"))?;
        match crate::knowledge_service::query_for_agent(&app, stack, &query, k, &cancel).await? {
            Some(hybrid) => hybrid_groups.push(hybrid),
            None => legacy_ids.push(id.clone()),
        }
    }
    let legacy_group = if legacy_ids.is_empty() {
        Vec::new()
    } else {
        query_impl(&base, state.inner(), &legacy_ids, &query, k).await?
    };

    Ok(merge_stack_results(hybrid_groups, legacy_group, k))
}

/// Merges per-stack result lists without comparing scores across index
/// generations.
///
/// A v1 result's `score` is a cosine similarity; a v2 result's is a
/// reciprocal-rank-fusion score. They are different quantities on different
/// scales, so the previous behaviour — concatenate everything and sort by
/// `score` descending — silently ranked one index above the other by an
/// artefact of its scoring function rather than by relevance. Whichever family
/// happened to produce larger numbers won.
///
/// Each stack's own ordering is authoritative and preserved; this only decides
/// how the lists are woven together, round-robin, so no stack is starved and no
/// cross-family comparison is made. Within one round, ties break on
/// `source_path` for determinism.
fn merge_stack_results(
    hybrid_groups: Vec<Vec<StackQueryResult>>,
    legacy_group: Vec<StackQueryResult>,
    k: usize,
) -> Vec<StackQueryResult> {
    let mut groups: Vec<std::vec::IntoIter<StackQueryResult>> = hybrid_groups
        .into_iter()
        .filter(|group| !group.is_empty())
        .map(Vec::into_iter)
        .collect();
    if !legacy_group.is_empty() {
        groups.push(legacy_group.into_iter());
    }

    let mut merged: Vec<StackQueryResult> = Vec::new();
    while merged.len() < k {
        let mut round: Vec<StackQueryResult> = Vec::new();
        for group in groups.iter_mut() {
            if let Some(result) = group.next() {
                round.push(result);
            }
        }
        if round.is_empty() {
            break;
        }
        round.sort_by(|left, right| left.source_path.cmp(&right.source_path));
        for result in round {
            if merged.len() >= k {
                break;
            }
            merged.push(result);
        }
    }
    merged
}

// ---------------------------------------------------------------------
// Staleness check
//
// The mtime walk itself now lives in `knowledge_core` (imported above), shared
// with the Knowledge 2.0 probe (`knowledge_service::v2_staleness_impl`) so both
// generations answer "did a local source change?" with one implementation
// during the overlap. What stays here is v1's own framing of the question: per
// stack, against `KnowledgeStack::indexed_at`.
// ---------------------------------------------------------------------

/// True if any of `stack`'s source files has a filesystem mtime newer than
/// its `indexed_at` — a stack that's never been indexed is never stale (see
/// `stacks_is_stale`). A source that can no longer be `stat`-ed at all (e.g.
/// deleted since indexing) counts as stale too, so a broken source surfaces
/// via the same badge instead of silently being skipped.
fn is_stale_impl(stack: &KnowledgeStack) -> bool {
    let Some(indexed_at) = stack.indexed_at else {
        return false;
    };
    stack
        .sources
        .iter()
        .any(|source| source_has_newer_mtime(Path::new(&source.path), indexed_at))
}

// ---------------------------------------------------------------------
// Agent retrieval tool (RAG design doc slice 2)
// ---------------------------------------------------------------------

/// The agent's read-only retrieval tool (RAG design doc slice 2): embeds
/// `query` with the resolved stack(s)' embedding spec (see
/// [`resolve_search_stack_ids`]) and returns the combined top-`max_results`
/// chunks across them, highest score first — the exact same ranking
/// `stacks_query` (the settings panel's test-search box) uses under the
/// hood, just with a model-facing argument shape (`stack` by name, not by
/// id; `max_results` not `k`) and its own name-resolution step.
///
/// `allowed_stack_names` is injected by `turnEngine.ts`'s `executeToolCall`
/// on EVERY call (never left to the model to supply — same treatment as
/// `checkpoint_id`/`turn_id` on the mutating tools) with this session's
/// actual attached-stack names, so `resolve_search_stack_ids` above always
/// scopes to them regardless of what (if anything) the model itself passed
/// for `stack`. `rename_all = "snake_case"` (like `tool_write_file`/
/// `tool_edit_file`/`tool_run_shell` in `tools.rs`) so `max_results`/
/// `allowed_stack_names` bind correctly — their arguments arrive with
/// snake_case keys from the model's tool call (`max_results`) or from
/// `executeToolCall`'s own injection (`allowed_stack_names`), and without
/// this attribute Tauri's default camelCase matching would silently fail to
/// bind either one (see `tools.rs`'s doc comments on the same attribute for
/// why a mismatch here fails silent, not loud).
///
/// Deliberately **not** permission-gated: like `tool_read_file`/`tool_glob`/
/// `tool_grep` (see `tools.rs`), this never calls
/// `permissions::request_permission`, so it is entirely unaffected by
/// permission mode — including Plan Mode's hard block, which only fires
/// for tools that actually reach `request_permission` (see
/// `permissions::mode_short_circuit`'s `"plan"` arm) — and by "smart"
/// mode's risk-floor logic, which only ever looks at `write_file`/
/// `edit_file`/`run_shell`. A read-only lookup over user-selected local
/// files needs no confirmation, exactly like the other read-only tools it's
/// modeled on.
#[tauri::command(rename_all = "snake_case")]
pub async fn tool_search_docs(
    app: AppHandle,
    state: tauri::State<'_, AppState>,
    query: String,
    stack: Option<String>,
    max_results: Option<u32>,
    allowed_stack_names: Option<Vec<String>>,
) -> Result<Vec<StackQueryResult>, String> {
    let base = stacks_base_dir(&app)?;
    let registry = load_registry(&base)?;
    // Always `Some(...)` here (an empty `Vec` when the caller sent nothing),
    // never `None` — see this function's and `resolve_search_stack_ids`'s doc
    // comments for why the desktop app must fail closed (scope to nothing)
    // rather than fail open (scope to everything) if this ever arrives
    // unset.
    let allowed = allowed_stack_names.unwrap_or_default();
    let k = max_results.unwrap_or(DEFAULT_QUERY_K as u32) as usize;
    let stack_ids = if stack.is_some() {
        // Named resolution intentionally accepts an unindexed legacy stack;
        // it may already have a Knowledge 2.0 generation.
        resolve_search_stack_ids(&registry, Some(&allowed), stack.as_deref())?
    } else {
        let mut ids = Vec::new();
        for candidate in registry.iter().filter(|candidate| {
            allowed
                .iter()
                .any(|name| name.trim().eq_ignore_ascii_case(candidate.name.trim()))
        }) {
            if candidate.indexed_at.is_some()
                || crate::knowledge_service::has_active_generation(&app, &candidate.id)?
            {
                ids.push(candidate.id.clone());
            }
        }
        if ids.is_empty() {
            return Err("No indexed knowledge stacks are available to search".to_string());
        }
        ids
    };

    let cancel = CancellationToken::new();
    let mut hybrid_groups: Vec<Vec<StackQueryResult>> = Vec::new();
    let mut legacy_ids = Vec::new();
    for id in &stack_ids {
        let candidate = registry
            .iter()
            .find(|candidate| &candidate.id == id)
            .ok_or_else(|| format!("Stack '{id}' not found"))?;
        match crate::knowledge_service::query_for_agent(&app, candidate, &query, k, &cancel).await? {
            Some(hybrid) => hybrid_groups.push(hybrid),
            None => legacy_ids.push(id.clone()),
        }
    }
    let legacy_group = if legacy_ids.is_empty() {
        Vec::new()
    } else {
        query_impl(&base, state.inner(), &legacy_ids, &query, k).await?
    };

    Ok(merge_stack_results(hybrid_groups, legacy_group, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    // Test-only here now that `create_impl` (their only non-test reader) moved
    // to `knowledge_core` — imported rather than duplicated so the defaults
    // these fixtures assert against stay the ones the registry actually
    // applies.
    use crate::knowledge_core::{DEFAULT_CHUNK_CHARS, DEFAULT_CHUNK_OVERLAP};

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
        assert!(
            chunks.len() >= 2,
            "expected at least 2 chunks, got {}",
            chunks.len()
        );
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
        assert!(
            chunks.len() > 1,
            "an oversized paragraph must be split into multiple chunks"
        );
        for chunk in &chunks {
            assert!(
                chunk.text.chars().count() <= 100,
                "hard-split chunk exceeded chunk_chars"
            );
        }
        // Every char of the original text must still appear (nothing lost).
        let total_unique_chars: usize = chunks.iter().map(|c| c.text.chars().count()).sum();
        assert!(
            total_unique_chars >= 500,
            "hard split must not lose content"
        );
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
        assert!(
            chunks[1].text.starts_with(&"a".repeat(30)),
            "second chunk missing carried-over overlap: {:?}",
            chunks[1].text
        );
    }

    #[test]
    fn chunk_text_tracks_markdown_headings() {
        let text = "# Section One\n\nfirst paragraph\n\n# Section Two\n\nsecond paragraph";
        // Small enough that the two sections can't be packed into one
        // chunk, so each section's heading is exercised independently.
        let chunks = chunk_text(text, 20, 0);
        assert_eq!(
            chunks.len(),
            2,
            "unexpected chunks: {:?}",
            chunks.iter().map(|c| &c.text).collect::<Vec<_>>()
        );
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
        let vectors = vec![
            vec![0.1_f32, 0.2, 0.3, 0.4],
            vec![-1.0, 0.5, 0.0, 2.5],
            vec![9.9, -9.9, 0.0, 1e-6],
        ];

        write_vectors_bin(&path, 4, &vectors).unwrap();
        let (dim, count, flat) = read_vectors_bin(&path).unwrap();

        assert_eq!(dim, 4);
        assert_eq!(count, 3);
        assert_eq!(flat.len(), 12);
        for (i, row) in vectors.iter().enumerate() {
            let read_row = &flat[i * 4..(i + 1) * 4];
            assert_eq!(
                read_row,
                row.as_slice(),
                "row {i} did not roundtrip exactly"
            );
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
        assert_eq!(
            ranked[0].0, 1,
            "expected row 1 (matches the query exactly) to rank first"
        );
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
    //
    // `spec_matches`'s own truth table moved to `knowledge_core`'s tests with
    // the function; what stays here is the v1-index-format consequence of it.

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

    // --- index cancellation via CancellationToken ---

    #[tokio::test]
    async fn select_on_cancellation_token_short_circuits_a_pending_future() {
        let token = CancellationToken::new();
        let token_clone = token.clone();

        // Signal cancellation shortly after this task starts waiting.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            token_clone.cancel();
        });

        let long_running = async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            "finished"
        };

        let cancelled = tokio::select! {
            biased;
            _ = token.cancelled() => true,
            _ = long_running => false,
        };

        assert!(
            cancelled,
            "cancel() must short-circuit the pending select! before the long future completes"
        );
    }

    /// The actual bug `tokio::sync::Notify::notify_waiters()` had (see
    /// `reindex_impl`'s doc comment): a cancel signal sent before anyone is
    /// awaiting it is silently lost, so a later `select!` on a freshly
    /// created `.notified()` future never sees it. `CancellationToken` fixes
    /// this by persisting the cancelled state — this test would FAIL if
    /// `cancel()`/`cancelled()` had `Notify`'s fire-and-forget semantics
    /// instead.
    #[tokio::test]
    async fn cancellation_token_short_circuits_even_when_cancelled_before_anyone_is_waiting() {
        let token = CancellationToken::new();
        token.cancel(); // Cancelled BEFORE anything ever awaits it.

        let cancelled = tokio::select! {
            biased;
            _ = token.cancelled() => true,
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => false,
        };

        assert!(
            cancelled,
            "a token cancelled before any waiter existed must still short-circuit a later select!"
        );
    }

    // --- registry CRUD moved to `knowledge_core`'s tests with its functions ---

    #[tokio::test]
    async fn stacks_cancel_index_is_a_harmless_noop_with_nothing_running() {
        // Mirrors `tools_cancel_running`'s tolerance: cancelling a stack
        // with no in-flight reindex (no entry in `index_cancels`) must not
        // error.
        let state = AppState::default();
        let cancels = state.index_cancels.lock().unwrap();
        assert!(cancels.get("no-such-stack").is_none());
    }

    // Shared fixture for the query/staleness tests below. (The stack-name
    // resolution tests that also used it moved to `knowledge_core` with
    // `resolve_search_stack_ids`.)
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

    // --- query_impl partial-failure handling (partition_indexed_stacks) ---

    #[test]
    fn partition_indexed_stacks_errors_immediately_for_an_unknown_id() {
        let registry = vec![test_stack("Docs", true)];
        let err = partition_indexed_stacks(&registry, &["no-such-id".to_string()]).unwrap_err();
        assert!(err.contains("no-such-id"), "unexpected error: {err}");
    }

    #[test]
    fn partition_indexed_stacks_errors_when_every_requested_stack_is_unindexed() {
        let registry = vec![test_stack("Docs", false), test_stack("Notes", false)];
        let ids = vec!["id-Docs".to_string(), "id-Notes".to_string()];
        let err = partition_indexed_stacks(&registry, &ids).unwrap_err();
        assert!(
            err.contains("Docs") && err.contains("Notes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn partition_indexed_stacks_skips_unindexed_siblings_instead_of_aborting() {
        // The regression this guards: a mixed indexed+unindexed request must
        // return just the indexed ones, not discard everything the moment it
        // hits one unindexed stack among several — see `query_impl`'s doc
        // comment.
        let registry = vec![
            test_stack("Docs", true),
            test_stack("Notes", false),
            test_stack("Wiki", true),
        ];
        let ids = vec![
            "id-Docs".to_string(),
            "id-Notes".to_string(),
            "id-Wiki".to_string(),
        ];
        let indexed = partition_indexed_stacks(&registry, &ids).unwrap();
        let mut names: Vec<&str> = indexed.iter().map(|s| s.name.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["Docs", "Wiki"]);
    }

    #[test]
    fn partition_indexed_stacks_returns_everything_when_all_are_indexed() {
        let registry = vec![test_stack("Docs", true), test_stack("Wiki", true)];
        let ids = vec!["id-Docs".to_string(), "id-Wiki".to_string()];
        let indexed = partition_indexed_stacks(&registry, &ids).unwrap();
        assert_eq!(indexed.len(), 2);
    }

    // --- stack-directory staging + atomic swap ---

    #[test]
    fn swap_stack_dir_replaces_the_live_directory_atomically() {
        let base = TempDir::new("swap_replace");
        let stack_id = "swap-test";
        let live_dir = base.path.join(stack_id);
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::write(live_dir.join("chunks.jsonl"), "old").unwrap();

        let staging_dir = staging_dir_path(&base.path, stack_id);
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::write(staging_dir.join("chunks.jsonl"), "new").unwrap();

        swap_stack_dir(&base.path, stack_id, &staging_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(live_dir.join("chunks.jsonl")).unwrap(),
            "new"
        );
        assert!(
            !staging_dir.exists(),
            "the staging dir must be consumed by the swap"
        );
        assert!(
            !backup_dir_path(&base.path, stack_id).exists(),
            "the backup dir must be cleaned up on success"
        );
    }

    #[test]
    fn swap_stack_dir_works_with_no_previous_directory() {
        let base = TempDir::new("swap_first_time");
        let stack_id = "swap-first";
        let staging_dir = staging_dir_path(&base.path, stack_id);
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::write(staging_dir.join("chunks.jsonl"), "content").unwrap();

        swap_stack_dir(&base.path, stack_id, &staging_dir).unwrap();

        assert_eq!(
            std::fs::read_to_string(base.path.join(stack_id).join("chunks.jsonl")).unwrap(),
            "content"
        );
    }

    #[test]
    fn recover_stack_dir_restores_the_backup_when_the_live_directory_is_missing() {
        // Simulates a crash exactly between `swap_stack_dir`'s two renames:
        // the live directory is gone, but the ".old" backup (the last known-
        // good, fully-consistent state) is still there.
        let base = TempDir::new("recover_mid_swap");
        let stack_id = "recover-test";
        let backup_dir = backup_dir_path(&base.path, stack_id);
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("chunks.jsonl"), "last-good").unwrap();

        recover_stack_dir(&base.path, stack_id);

        let live_dir = base.path.join(stack_id);
        assert_eq!(
            std::fs::read_to_string(live_dir.join("chunks.jsonl")).unwrap(),
            "last-good"
        );
        assert!(
            !backup_dir.exists(),
            "the backup must be consumed by the restore"
        );
    }

    #[test]
    fn recover_stack_dir_clears_a_stale_staging_directory() {
        let base = TempDir::new("recover_stale_staging");
        let stack_id = "recover-staging";
        let staging_dir = staging_dir_path(&base.path, stack_id);
        std::fs::create_dir_all(&staging_dir).unwrap();
        std::fs::write(staging_dir.join("chunks.jsonl"), "half-written").unwrap();

        recover_stack_dir(&base.path, stack_id);

        assert!(
            !staging_dir.exists(),
            "a leftover staging dir from an interrupted run must be cleared"
        );
    }

    #[test]
    fn recover_stack_dir_is_a_no_op_when_everything_is_already_consistent() {
        let base = TempDir::new("recover_noop");
        let stack_id = "recover-clean";
        let live_dir = base.path.join(stack_id);
        std::fs::create_dir_all(&live_dir).unwrap();
        std::fs::write(live_dir.join("chunks.jsonl"), "fine").unwrap();

        recover_stack_dir(&base.path, stack_id);

        assert_eq!(
            std::fs::read_to_string(live_dir.join("chunks.jsonl")).unwrap(),
            "fine"
        );
    }

    // --- stale-index false positives (extension/size gate) ---

    #[test]
    fn source_has_newer_mtime_ignores_a_touched_file_with_a_non_indexable_extension() {
        let dir = TempDir::new("stale_extension_gate");
        let indexed_file = dir.path.join("doc.txt");
        std::fs::write(&indexed_file, "hello").unwrap();
        let indexed_at = now_ms() + 60_000; // "indexed in the future" relative to doc.txt

        // A non-indexable file (wrong extension) with a genuinely newer
        // mtime must NOT flip the stale badge — indexing would never look at
        // it in the first place (see `read_indexable_file`/`ALLOWED_EXTENSIONS`).
        let image_file = dir.path.join("screenshot.png");
        std::fs::write(&image_file, [0u8; 16]).unwrap();

        assert!(
            !source_has_newer_mtime(&dir.path, indexed_at),
            "a touched file indexing would skip by extension must not count as stale"
        );
    }

    #[test]
    fn source_has_newer_mtime_ignores_an_oversized_touched_file() {
        let dir = TempDir::new("stale_size_gate");
        let indexed_file = dir.path.join("doc.txt");
        std::fs::write(&indexed_file, "hello").unwrap();
        let indexed_at = now_ms() + 60_000;

        // An indexable-extension file that's over MAX_FILE_BYTES must also
        // be excluded — `read_indexable_file` would skip it too.
        let huge_file = dir.path.join("huge.txt");
        std::fs::write(&huge_file, vec![b'x'; (MAX_FILE_BYTES + 1) as usize]).unwrap();

        assert!(
            !source_has_newer_mtime(&dir.path, indexed_at),
            "a touched file over the size cap must not count as stale"
        );
    }

    #[test]
    fn source_has_newer_mtime_still_flags_a_genuinely_indexable_change() {
        let dir = TempDir::new("stale_real_change");
        std::fs::write(dir.path.join("doc.txt"), "hello").unwrap();
        let indexed_at = now_ms();
        // Touch the file with fresh content strictly after `indexed_at`.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(dir.path.join("doc.txt"), "hello again, changed").unwrap();

        assert!(
            source_has_newer_mtime(&dir.path, indexed_at),
            "a real change to an indexable file must still flag staleness"
        );
    }

    // --- incremental reindex planning (slice 4) ---

    fn plan_test_stack(dim: u32) -> KnowledgeStack {
        KnowledgeStack {
            id: "plan-test".to_string(),
            name: "Plan Test".to_string(),
            sources: Vec::new(),
            embedding: test_spec(dim),
            chunk_chars: DEFAULT_CHUNK_CHARS,
            chunk_overlap: DEFAULT_CHUNK_OVERLAP,
            indexed_at: Some(1),
            chunk_count: 0,
        }
    }

    #[test]
    fn plan_reindex_skips_reembedding_unchanged_files_but_reembeds_changed_and_new_ones() {
        let stack = plan_test_stack(4);
        let a_content = "alpha content";
        let a_hash = sha256_hex(a_content);
        let old_chunks = vec![ChunkMeta {
            source_path: "a.txt".to_string(),
            ordinal: 0,
            text: a_content.to_string(),
            content_hash: a_hash.clone(),
            heading: None,
        }];
        let old_vectors_flat = vec![1.0_f32, 0.0, 0.0, 0.0];
        let old_file_index: HashMap<String, String> = [
            ("a.txt".to_string(), a_hash.clone()),
            ("b.txt".to_string(), "stale-hash".to_string()),
        ]
        .into_iter()
        .collect();

        // Current files: a.txt unchanged, b.txt changed (hash mismatch),
        // c.txt brand new (not in old_file_index at all).
        let files = vec![
            ("a.txt".to_string(), a_content.to_string()),
            (
                "b.txt".to_string(),
                "beta content, now different".to_string(),
            ),
            ("c.txt".to_string(), "gamma content".to_string()),
        ];

        let mut progress_calls = 0;
        let plan = plan_reindex(
            &stack,
            &files,
            &old_file_index,
            &old_chunks,
            Some((4, 1, &old_vectors_flat)),
            &CancellationToken::new(),
            |_, _, _| progress_calls += 1,
        );

        assert_eq!(
            progress_calls, 3,
            "on_file_done must be called once per file"
        );

        // a.txt's row was carried over verbatim — not queued for embedding.
        let a_idx = plan
            .all_chunks
            .iter()
            .position(|c| c.source_path == "a.txt")
            .expect("a.txt chunk present");
        assert!(
            !plan.to_embed_indices.contains(&a_idx),
            "unchanged file must not be queued for re-embedding"
        );
        assert_eq!(plan.vector_slots[a_idx], Some(vec![1.0, 0.0, 0.0, 0.0]));

        // b.txt (changed) and c.txt (new) must both be queued for embedding.
        let embedded_paths: Vec<&str> = plan
            .to_embed_indices
            .iter()
            .map(|&i| plan.all_chunks[i].source_path.as_str())
            .collect();
        assert!(embedded_paths.contains(&"b.txt"));
        assert!(embedded_paths.contains(&"c.txt"));
        assert_eq!(
            plan.to_embed_texts.len(),
            embedded_paths.len(),
            "exactly one chunk per short changed/new file here"
        );

        // The key assertion this test exists for: re-embedding work only
        // covers the changed/new files, not the whole stack — i.e. the
        // "re-embed call count" actually shrinks when most files are
        // unchanged, rather than every reindex re-embedding everything.
        assert!(
            plan.to_embed_texts.len() < plan.all_chunks.len(),
            "unchanged file's chunk must be skipped, not re-embedded"
        );

        // new_file_index reflects every CURRENT file's hash, including ones
        // absent from the old index.
        assert_eq!(plan.new_file_index.get("a.txt"), Some(&a_hash));
        assert!(plan.new_file_index.contains_key("b.txt"));
        assert!(plan.new_file_index.contains_key("c.txt"));
    }

    #[test]
    fn plan_reindex_drops_rows_for_sources_no_longer_present() {
        let stack = plan_test_stack(4);
        let old_chunks = vec![ChunkMeta {
            source_path: "deleted.txt".to_string(),
            ordinal: 0,
            text: "gone".to_string(),
            content_hash: sha256_hex("gone"),
            heading: None,
        }];
        let old_vectors_flat = vec![0.5_f32, 0.5, 0.5, 0.5];
        let old_file_index: HashMap<String, String> =
            [("deleted.txt".to_string(), sha256_hex("gone"))]
                .into_iter()
                .collect();

        // `deleted.txt` no longer appears in the current file list at all —
        // its source was removed, or the file itself was deleted.
        let files = vec![("kept.txt".to_string(), "still here".to_string())];

        let plan = plan_reindex(
            &stack,
            &files,
            &old_file_index,
            &old_chunks,
            Some((4, 1, &old_vectors_flat)),
            &CancellationToken::new(),
            |_, _, _| {},
        );

        assert!(
            !plan
                .all_chunks
                .iter()
                .any(|c| c.source_path == "deleted.txt"),
            "a no-longer-present source's rows must be dropped, not carried over"
        );
        assert!(plan.all_chunks.iter().any(|c| c.source_path == "kept.txt"));
    }

    #[test]
    fn plan_reindex_falls_back_to_full_reembed_when_old_vectors_dim_mismatches() {
        let stack = plan_test_stack(4); // stack now expects 4 dims…
        let content = "same content";
        let hash = sha256_hex(content);
        let old_chunks = vec![ChunkMeta {
            source_path: "a.txt".to_string(),
            ordinal: 0,
            text: content.to_string(),
            content_hash: hash.clone(),
            heading: None,
        }];
        // …but the previous vectors.bin was written for an 8-dim model —
        // an embedding-spec change since the last index.
        let old_vectors_flat = vec![0.0_f32; 8];
        let old_file_index: HashMap<String, String> =
            [("a.txt".to_string(), hash)].into_iter().collect();
        let files = vec![("a.txt".to_string(), content.to_string())];

        let plan = plan_reindex(
            &stack,
            &files,
            &old_file_index,
            &old_chunks,
            Some((8, 1, &old_vectors_flat)),
            &CancellationToken::new(),
            |_, _, _| {},
        );

        assert_eq!(
            plan.to_embed_texts.len(),
            1,
            "a dim mismatch must force re-embedding even for an otherwise-unchanged file"
        );
        assert!(plan.vector_slots.iter().all(|v| v.is_none()));
    }

    #[test]
    fn plan_reindex_with_no_previous_index_queues_every_file_for_embedding() {
        let stack = plan_test_stack(4);
        let files = vec![
            ("a.txt".to_string(), "alpha".to_string()),
            ("b.txt".to_string(), "beta".to_string()),
        ];

        let plan = plan_reindex(
            &stack,
            &files,
            &HashMap::new(),
            &[],
            None,
            &CancellationToken::new(),
            |_, _, _| {},
        );

        assert_eq!(plan.to_embed_texts.len(), plan.all_chunks.len());
        assert!(plan.vector_slots.iter().all(|v| v.is_none()));
    }

    #[test]
    fn plan_reindex_stops_early_once_cancelled() {
        let stack = plan_test_stack(4);
        let files = vec![
            ("a.txt".to_string(), "alpha".to_string()),
            ("b.txt".to_string(), "beta".to_string()),
            ("c.txt".to_string(), "gamma".to_string()),
        ];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let mut progress_calls = 0;
        let plan = plan_reindex(
            &stack,
            &files,
            &HashMap::new(),
            &[],
            None,
            &cancel,
            |_, _, _| progress_calls += 1,
        );

        assert_eq!(
            progress_calls, 0,
            "an already-cancelled token must stop before the first file is processed"
        );
        assert!(plan.all_chunks.is_empty());
    }

    // --- file_index.json roundtrip ---

    #[test]
    fn file_index_roundtrips_and_defaults_to_empty_when_missing() {
        let dir = TempDir::new("file_index");
        assert!(
            read_file_index(&dir.path).is_empty(),
            "missing file_index.json must default to empty, not error"
        );

        let mut index = HashMap::new();
        index.insert("a.txt".to_string(), "hash-a".to_string());
        index.insert("b.txt".to_string(), "hash-b".to_string());
        write_file_index(&dir.path, &index).unwrap();

        let read_back = read_file_index(&dir.path);
        assert_eq!(read_back, index);
    }

    // --- stale-index badge ---

    #[test]
    fn is_stale_impl_is_false_for_a_never_indexed_stack() {
        let mut stack = test_stack("Docs", false);
        stack.sources = vec![StackSource {
            path: "/nonexistent/path/at/all".to_string(),
            kind: SourceKind::File,
        }];
        assert!(
            !is_stale_impl(&stack),
            "a stack with no indexed_at is never stale"
        );
    }

    #[test]
    fn is_stale_impl_is_true_when_a_source_file_is_missing() {
        let mut stack = test_stack("Docs", true);
        stack.indexed_at = Some(now_ms());
        stack.sources = vec![StackSource {
            path: "/definitely/does/not/exist.txt".to_string(),
            kind: SourceKind::File,
        }];
        assert!(
            is_stale_impl(&stack),
            "an unreadable source counts as stale"
        );
    }

    #[test]
    fn is_stale_impl_is_false_when_the_source_file_predates_indexed_at() {
        let dir = TempDir::new("stale_check_false");
        let file_path = dir.path.join("doc.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let mut stack = test_stack("Docs", true);
        // Indexed "in the future" relative to the file we just wrote.
        stack.indexed_at = Some(now_ms() + 60_000);
        stack.sources = vec![StackSource {
            path: file_path.to_string_lossy().to_string(),
            kind: SourceKind::File,
        }];

        assert!(!is_stale_impl(&stack));
    }

    #[test]
    fn is_stale_impl_is_true_when_the_source_file_was_modified_after_indexed_at() {
        let dir = TempDir::new("stale_check_true");
        let file_path = dir.path.join("doc.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let mut stack = test_stack("Docs", true);
        // Indexed "in the past" relative to the file we just wrote.
        stack.indexed_at = Some(0);
        stack.sources = vec![StackSource {
            path: file_path.to_string_lossy().to_string(),
            kind: SourceKind::File,
        }];

        assert!(is_stale_impl(&stack));
    }

    // --- PDF extraction (feature-gated, slice 4) ---

    /// Hand-assembled minimal single-page PDF ("Hello World" text drawn via
    /// a content stream) with a byte-accurate xref table — built
    /// programmatically (rather than as a checked-in binary fixture) so the
    /// exact object offsets always match the bytes actually written.
    #[cfg(feature = "pdf-extraction")]
    fn minimal_pdf_fixture_bytes() -> Vec<u8> {
        let mut buf: Vec<u8> = Vec::new();
        buf.extend_from_slice(b"%PDF-1.4\n");

        let mut offsets = [0usize; 6]; // 1-indexed; offsets[0] unused

        offsets[1] = buf.len();
        buf.extend_from_slice(b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

        offsets[2] = buf.len();
        buf.extend_from_slice(b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n");

        offsets[3] = buf.len();
        buf.extend_from_slice(
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /Resources << /Font << /F1 4 0 R >> >> \
/MediaBox [0 0 612 792] /Contents 5 0 R >>\nendobj\n",
        );

        offsets[4] = buf.len();
        buf.extend_from_slice(
            b"4 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
        );

        let stream_content = b"BT /F1 24 Tf 100 700 Td (Hello World) Tj ET";
        offsets[5] = buf.len();
        buf.extend_from_slice(
            format!("5 0 obj\n<< /Length {} >>\nstream\n", stream_content.len()).as_bytes(),
        );
        buf.extend_from_slice(stream_content);
        buf.extend_from_slice(b"\nendstream\nendobj\n");

        let xref_offset = buf.len();
        let mut xref = String::from("xref\n0 6\n0000000000 65535 f \n");
        for offset in offsets.iter().skip(1) {
            xref.push_str(&format!("{:010} 00000 n \n", offset));
        }
        buf.extend_from_slice(xref.as_bytes());
        buf.extend_from_slice(b"trailer\n<< /Size 6 /Root 1 0 R >>\n");
        buf.extend_from_slice(format!("startxref\n{xref_offset}\n").as_bytes());
        buf.extend_from_slice(b"%%EOF");

        buf
    }

    #[cfg(feature = "pdf-extraction")]
    #[test]
    fn read_indexable_pdf_extracts_text_from_a_minimal_fixture() {
        let dir = TempDir::new("pdf_fixture");
        let pdf_path = dir.path.join("fixture.pdf");
        std::fs::write(&pdf_path, minimal_pdf_fixture_bytes()).unwrap();

        let (path, text) =
            read_indexable_pdf(&pdf_path).expect("a well-formed minimal PDF must extract text");
        assert!(path.ends_with("fixture.pdf"));
        assert!(
            text.contains("Hello World"),
            "unexpected extracted text: {text:?}"
        );
    }

    #[cfg(feature = "pdf-extraction")]
    #[test]
    fn read_indexable_file_routes_pdf_extension_through_the_pdf_extractor() {
        let dir = TempDir::new("pdf_via_read_indexable_file");
        let pdf_path = dir.path.join("doc.pdf");
        std::fs::write(&pdf_path, minimal_pdf_fixture_bytes()).unwrap();

        let (_, text) =
            read_indexable_file(&pdf_path).expect("read_indexable_file must handle .pdf");
        assert!(text.contains("Hello World"));
    }

    // ---------------------------------------------------------------------
    // merge_stack_results — v1 cosine scores and v2 RRF scores are different
    // quantities on different scales, so they must never be compared.
    // ---------------------------------------------------------------------

    fn result(stack: &str, path: &str, score: f32) -> StackQueryResult {
        StackQueryResult {
            stack_id: stack.to_string(),
            stack_name: stack.to_string(),
            source_path: path.to_string(),
            score,
            text: format!("{path} body"),
            heading: None,
        }
    }

    #[test]
    fn merging_never_lets_a_scoring_scale_starve_the_other_index() {
        // The realistic shape of the bug: v1 cosine similarities sit near 0.8
        // while v2 RRF scores sit near 0.016 (1/61 for a rank-1 hit with the
        // usual k=60 constant). Sorting the concatenation by `score` put every
        // v1 hit above every v2 hit regardless of relevance.
        let v2 = vec![
            result("v2", "a.md", 0.0163),
            result("v2", "b.md", 0.0161),
            result("v2", "c.md", 0.0159),
        ];
        let v1 = vec![
            result("v1", "x.md", 0.87),
            result("v1", "y.md", 0.85),
            result("v1", "z.md", 0.83),
        ];

        let merged = merge_stack_results(vec![v2], v1, 6);
        let stacks: Vec<&str> = merged.iter().map(|hit| hit.stack_id.as_str()).collect();

        assert_eq!(merged.len(), 6);
        assert!(
            stacks.iter().take(2).any(|stack| *stack == "v2"),
            "the v2 index was starved by v1's larger score scale: {stacks:?}"
        );
        // Round-robin: each index contributes one per round.
        assert_eq!(stacks.iter().filter(|stack| **stack == "v1").count(), 3);
        assert_eq!(stacks.iter().filter(|stack| **stack == "v2").count(), 3);
    }

    #[test]
    fn merging_preserves_each_stacks_own_ordering() {
        // A stack's own ranking is authoritative — this function decides only
        // how lists interleave, never how they are ordered internally.
        let first = vec![
            result("s1", "1-best.md", 0.9),
            result("s1", "2-mid.md", 0.5),
            result("s1", "3-worst.md", 0.1),
        ];
        let merged = merge_stack_results(vec![first], Vec::new(), 3);
        let paths: Vec<&str> = merged.iter().map(|hit| hit.source_path.as_str()).collect();
        assert_eq!(paths, vec!["1-best.md", "2-mid.md", "3-worst.md"]);
    }

    #[test]
    fn merging_respects_k_and_drains_uneven_groups() {
        // Rebuilt per call rather than cloned: `StackQueryResult` is a wire type
        // and does not need a `Clone` impl added for a test's convenience.
        let long = || {
            vec![
                result("long", "l1", 0.9),
                result("long", "l2", 0.8),
                result("long", "l3", 0.7),
                result("long", "l4", 0.6),
            ]
        };
        let short = || vec![result("short", "s1", 0.5)];

        let merged = merge_stack_results(vec![long(), short()], Vec::new(), 3);
        assert_eq!(merged.len(), 3, "k must be respected exactly");

        // With room for everything, the shorter group simply runs out and the
        // longer one keeps contributing rather than the merge stopping early.
        let all = merge_stack_results(vec![long(), short()], Vec::new(), 10);
        assert_eq!(all.len(), 5);
        assert_eq!(
            all.iter().filter(|hit| hit.stack_id == "long").count(),
            4,
            "the longer group must not be truncated to the shorter one's length"
        );
    }

    #[test]
    fn merging_handles_empty_inputs_without_panicking() {
        assert!(merge_stack_results(Vec::new(), Vec::new(), 5).is_empty());
        assert!(merge_stack_results(vec![Vec::new(), Vec::new()], Vec::new(), 5).is_empty());
        assert_eq!(
            merge_stack_results(Vec::new(), vec![result("v1", "only.md", 0.4)], 5).len(),
            1
        );
        // k = 0 asks for nothing and must return nothing, not everything.
        assert!(merge_stack_results(vec![vec![result("v2", "a.md", 0.1)]], Vec::new(), 0).is_empty());
    }

    #[test]
    fn merging_is_deterministic_for_a_tied_round() {
        let a = || vec![result("a", "zeta.md", 0.5)];
        let b = || vec![result("b", "alpha.md", 0.5)];
        let first = merge_stack_results(vec![a(), b()], Vec::new(), 2);
        let second = merge_stack_results(vec![a(), b()], Vec::new(), 2);
        assert_eq!(
            first.iter().map(|hit| hit.source_path.clone()).collect::<Vec<_>>(),
            second.iter().map(|hit| hit.source_path.clone()).collect::<Vec<_>>(),
        );
        assert_eq!(first[0].source_path, "alpha.md", "ties break on source_path");
    }
}

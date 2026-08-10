//! Knowledge Stacks: named collections of user-picked folders/files, searched
//! through their Knowledge 2.0 generation.
//!
//! The stack *registry* lives at `<app_data>/stacks/index.json` (a plain
//! `Vec<KnowledgeStack>`, atomically rewritten on every mutation) and is owned
//! by [`crate::knowledge_core`], shared with Knowledge 2.0. What lives here is
//! the Tauri command layer over that registry plus the retrieval path every
//! caller shares — desktop chat, the agent's `search_docs` tool, and
//! `monkey-cli`.
//!
//! **v1 is gone.** This module used to carry a second, independent index
//! format — `chunks.jsonl` + `vectors.bin`, its own chunker, brute-force
//! dot-product ranking, incremental reindex planning, and a staleness check —
//! that answered queries whenever a stack had no Knowledge 2.0 generation.
//! Two indexes meant two answers to the same query, two staleness rules, and
//! two scoring scales that could not be compared (see [`merge_stack_results`],
//! which survives because interleaving is still the right way to combine
//! *stacks*). Indexing is now `knowledge_v2_refresh` and staleness is
//! `knowledge_v2_is_stale`; there is one index and one answer.

use std::path::Path;

use tauri::AppHandle;
use tokio_util::sync::CancellationToken;

// Spelled unqualified throughout this file's body; re-exported because
// `stacks::` is still how a few call sites name the registry types.
pub use crate::knowledge_core::{
    add_source_impl, create_impl, delete_impl, embed_batch, import_definitions_impl, list_impl,
    mark_v2_indexed_impl, remove_source_impl, rename_impl, resolve_search_stack_ids,
    update_chunking_impl, EmbeddingBackend, EmbeddingSpec, KnowledgeStack, SourceKind,
    StackQueryResult, StackSource,
};
use crate::knowledge_core::{load_registry, stacks_base_dir};

/// Default number of results [`stacks_query`] returns when the caller doesn't
/// specify `k`.
const DEFAULT_QUERY_K: usize = 6;

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
pub fn stacks_delete(app: AppHandle, id: String) -> Result<(), String> {
    delete_impl(&stacks_base_dir(&app)?, &id)
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
pub async fn stacks_query(
    app: AppHandle,
    stack_ids: Vec<String>,
    query: String,
    k: Option<u32>,
) -> Result<Vec<StackQueryResult>, String> {
    let app_data = crate::knowledge_core::app_data_dir(&app)?;
    let registry = load_registry(&stacks_base_dir(&app)?)?;
    let k = k.unwrap_or(DEFAULT_QUERY_K as u32) as usize;
    query_stacks(
        &app_data,
        &registry,
        &stack_ids,
        &query,
        k,
        &CancellationToken::new(),
    )
    .await
}

/// Retrieval over already-resolved stack ids, each served by its active
/// Knowledge 2.0 generation.
///
/// Shared by [`stacks_query`], [`tool_search_docs`] and `monkey-cli`'s
/// `search_docs` — `AppHandle`-free precisely so the CLI can call it, since
/// while it could not, the CLI agent answered from a different index than the
/// desktop and the two ranked the same query differently on the same machine.
///
/// A stack with no active generation contributes nothing rather than erroring:
/// the caller may have asked for several, and one unindexed stack should not
/// fail the whole search. `tool_search_docs` is what refuses when *nothing*
/// asked for is searchable.
pub async fn query_stacks(
    app_data: &Path,
    registry: &[KnowledgeStack],
    stack_ids: &[String],
    query: &str,
    k: usize,
    cancel: &CancellationToken,
) -> Result<Vec<StackQueryResult>, String> {
    let mut groups: Vec<Vec<StackQueryResult>> = Vec::new();
    for id in stack_ids {
        let stack = registry
            .iter()
            .find(|stack| &stack.id == id)
            .ok_or_else(|| format!("Stack '{id}' not found"))?;
        if let Some(hits) =
            crate::knowledge_service::query_for_agent_at(app_data, stack, query, k, cancel).await?
        {
            groups.push(hits);
        }
    }
    Ok(merge_stack_results(groups, k))
}

/// Merges per-stack result lists without comparing scores across stacks.
///
/// Each stack's own ranking is authoritative; this only decides how the lists
/// are woven together, round-robin, so no stack is starved by another's score
/// scale. Within one round, ties break on `source_path` for determinism.
///
/// The scale problem is not hypothetical and is why this is not a sort: it was
/// live when v1 cosine similarities (~0.8) were concatenated with v2 RRF scores
/// (~0.016) and sorted, which ranked one index above the other by an artefact
/// of its scoring function. v1 is gone, but two stacks' scores are still not a
/// common currency, so the merge stays an interleave.
fn merge_stack_results(groups: Vec<Vec<StackQueryResult>>, k: usize) -> Vec<StackQueryResult> {
    let mut groups: Vec<std::vec::IntoIter<StackQueryResult>> = groups
        .into_iter()
        .filter(|group| !group.is_empty())
        .map(Vec::into_iter)
        .collect();

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
// Agent retrieval tool (RAG design doc slice 2)
// ---------------------------------------------------------------------

#[tauri::command]
pub async fn tool_search_docs(
    app: AppHandle,
    query: String,
    stack: Option<String>,
    max_results: Option<u32>,
    allowed_stack_names: Option<Vec<String>>,
) -> Result<Vec<StackQueryResult>, String> {
    let app_data = crate::knowledge_core::app_data_dir(&app)?;
    let registry = load_registry(&stacks_base_dir(&app)?)?;
    // Always `Some(...)` here (an empty `Vec` when the caller sent nothing),
    // never `None` — see `resolve_search_stack_ids`'s doc comment for why the
    // desktop app must fail closed (scope to nothing) rather than fail open
    // (scope to everything) if this ever arrives unset.
    let allowed = allowed_stack_names.unwrap_or_default();
    let k = max_results.unwrap_or(DEFAULT_QUERY_K as u32) as usize;
    let stack_ids = if stack.is_some() {
        resolve_search_stack_ids(&registry, Some(&allowed), stack.as_deref())?
    } else {
        let mut ids = Vec::new();
        for candidate in registry.iter().filter(|candidate| {
            allowed
                .iter()
                .any(|name| name.trim().eq_ignore_ascii_case(candidate.name.trim()))
        }) {
            if crate::knowledge_service::has_active_generation(&app, &candidate.id)? {
                ids.push(candidate.id.clone());
            }
        }
        if ids.is_empty() {
            return Err("No indexed knowledge stacks are available to search".to_string());
        }
        ids
    };

    query_stacks(
        &app_data,
        &registry,
        &stack_ids,
        &query,
        k,
        &CancellationToken::new(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------
    // merge_stack_results — one stack's scores are not a common currency with
    // another's, so they must never be compared.
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
    fn merging_never_lets_a_scoring_scale_starve_another_stack() {
        // The realistic shape of the bug this replaced: one group's scores sat
        // near 0.8 while another's sat near 0.016, and sorting the
        // concatenation by `score` put every hit from the first above every hit
        // from the second regardless of relevance.
        let small = vec![
            result("small", "a.md", 0.0163),
            result("small", "b.md", 0.0161),
            result("small", "c.md", 0.0159),
        ];
        let large = vec![
            result("large", "x.md", 0.87),
            result("large", "y.md", 0.85),
            result("large", "z.md", 0.83),
        ];

        let merged = merge_stack_results(vec![small, large], 6);
        let stacks: Vec<&str> = merged.iter().map(|hit| hit.stack_id.as_str()).collect();

        assert_eq!(merged.len(), 6);
        assert!(
            stacks.iter().take(2).any(|stack| *stack == "small"),
            "the small-scale stack was starved by the other's larger scores: {stacks:?}"
        );
        // Round-robin: each stack contributes one per round.
        assert_eq!(stacks.iter().filter(|stack| **stack == "large").count(), 3);
        assert_eq!(stacks.iter().filter(|stack| **stack == "small").count(), 3);
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
        let merged = merge_stack_results(vec![first], 3);
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

        let merged = merge_stack_results(vec![long(), short()], 3);
        assert_eq!(merged.len(), 3, "k must be respected exactly");

        // With room for everything, the shorter group simply runs out and the
        // longer one keeps contributing rather than the merge stopping early.
        let all = merge_stack_results(vec![long(), short()], 10);
        assert_eq!(all.len(), 5);
        assert_eq!(
            all.iter().filter(|hit| hit.stack_id == "long").count(),
            4,
            "the longer group must not be truncated to the shorter one's length"
        );
    }

    #[test]
    fn merging_handles_empty_inputs_without_panicking() {
        assert!(merge_stack_results(Vec::new(), 5).is_empty());
        assert!(merge_stack_results(vec![Vec::new(), Vec::new()], 5).is_empty());
        assert_eq!(
            merge_stack_results(vec![vec![result("only", "only.md", 0.4)]], 5).len(),
            1
        );
        // k = 0 asks for nothing and must return nothing, not everything.
        assert!(merge_stack_results(vec![vec![result("s", "a.md", 0.1)]], 0).is_empty());
    }

    #[test]
    fn merging_is_deterministic_for_a_tied_round() {
        let a = || vec![result("a", "zeta.md", 0.5)];
        let b = || vec![result("b", "alpha.md", 0.5)];
        let first = merge_stack_results(vec![a(), b()], 2);
        let second = merge_stack_results(vec![a(), b()], 2);
        assert_eq!(
            first
                .iter()
                .map(|hit| hit.source_path.clone())
                .collect::<Vec<_>>(),
            second
                .iter()
                .map(|hit| hit.source_path.clone())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            first[0].source_path, "alpha.md",
            "ties break on source_path"
        );
    }
}

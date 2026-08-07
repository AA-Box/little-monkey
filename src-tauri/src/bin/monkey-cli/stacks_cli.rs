//! `monkey-cli stacks` subcommand parity, plus the `search_docs` tool's CLI-side
//! support code (`agent.rs::execute_tool_call` calls [`search_docs`] below).
//! Reuses the library's `AppHandle`-free entry points directly — the same
//! reasoning as `checkpoints_cli.rs`: `knowledge_core`'s `list_impl`/
//! `resolve_search_stack_ids` for the registry, `stacks::query_stacks_v2_first`
//! for retrieval, and `knowledge_service::knowledge_v2_refresh_headless` or
//! `stacks::reindex_impl` for a rebuild depending on which index serves the
//! stack (see [`reindex`]). No create/delete/rename/add-source
//! here: per the RAG design doc's CLI-parity note ("a Stacks subcommand
//! (list/reindex...)"), stack management stays a Settings-panel action; the
//! CLI only ever lists, reindexes and v1-imports stacks someone already created
//! there.

use std::io::Write;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use little_monkey_lib::knowledge_core::{self, KnowledgeStack, StackQueryResult};
use little_monkey_lib::stacks;
use little_monkey_lib::AppState;

/// Resolves (creating if necessary) `<app-data>/stacks` — the exact
/// directory `stacks.rs::stacks_base_dir` resolves via
/// `AppHandle::path().app_data_dir()`, so a stack created in the desktop
/// app's Knowledge settings tab is immediately visible here, and a reindex
/// run from here is immediately visible there.
pub fn base_dir() -> Option<PathBuf> {
    let dir = little_monkey_lib::app_paths::data_dir()?.join("stacks");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn find_by_name(registry: &[KnowledgeStack], name: &str) -> Option<KnowledgeStack> {
    let trimmed = name.trim();
    registry
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(trimmed))
        .cloned()
}

/// `monkey-cli stacks list` — one line per stack: name, source count, chunk
/// count/indexed state, and embedding model.
pub fn list() -> Result<(), String> {
    let base = base_dir().ok_or("Could not resolve the app data directory")?;
    let registry = knowledge_core::list_impl(&base)?;
    if registry.is_empty() {
        println!(
            "No knowledge stacks yet — create one in the desktop app's Settings > Knowledge tab."
        );
        return Ok(());
    }
    for stack in &registry {
        let indexed = match stack.indexed_at {
            Some(_) => format!("{} chunks", stack.chunk_count),
            None => "never indexed".to_string(),
        };
        println!(
            "{}\t{} source(s)\t{}\t{}",
            stack.name,
            stack.sources.len(),
            indexed,
            stack.embedding.model_id_or_tag,
        );
    }
    Ok(())
}

/// `monkey-cli stacks reindex <name>` — resolves `name` case-insensitively
/// against the registry, then refreshes whichever index actually serves the
/// stack: `knowledge_v2_refresh_headless` for a stack that has been imported
/// into Knowledge 2.0 (the same `&Path` entry point the resident daemon uses,
/// so the CLI runs the production connector/extraction/publication path, not a
/// second implementation of it), and v1's `reindex_impl` for one that has not.
///
/// The fallback is not a nicety. A v1-only stack has no *enabled Knowledge 2.0
/// catalog source* — its sources live in `stacks/index.json`, and only the v1→v2
/// import seeds them into the v2 catalog — so a v2 refresh against it fails with
/// "Add and enable at least one Knowledge 2.0 source", which is both true and
/// useless as the answer to `stacks reindex`. Choosing on the active generation
/// keeps today's behaviour for exactly the stacks that still need it, and is the
/// same rule the read path uses (`stacks::query_stacks_v2_first`).
///
/// v1's progress callback renders to a single `\r`-updating terminal line (the
/// CLI's equivalent of the desktop app's progress bar, which instead consumes
/// `stacks://index-progress` Tauri events — see `stacks.rs::stacks_reindex`).
/// Both paths are incremental: `reindex_impl` diffs its own `file_index.json`,
/// and a v2 refresh reuses unchanged objects' chunks.
pub async fn reindex(name: &str) -> Result<(), String> {
    let base = base_dir().ok_or("Could not resolve the app data directory")?;
    let app_data = base
        .parent()
        .ok_or("Could not resolve the app data directory")?;
    let registry = knowledge_core::list_impl(&base)?;
    let stack = find_by_name(&registry, name)
        .ok_or_else(|| format!("No knowledge stack named '{}'", name.trim()))?;

    if little_monkey_lib::knowledge_service::has_active_generation_at(app_data, &stack.id)? {
        let report = little_monkey_lib::knowledge_service::knowledge_v2_refresh_headless(
            app_data, &stack.id,
        )
        .await?;
        println!(
            "Refreshed '{}' in Knowledge 2.0: {} object(s), {} chunk(s) embedded, {} reused.",
            stack.name, report.object_count, report.embedded_chunks, report.reused_chunks,
        );
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        return Ok(());
    }

    let state = AppState::default();
    let updated = stacks::reindex_impl(
        &base,
        &state,
        &stack.id,
        |files_done, files_total, chunks, phase| {
            render_progress(files_done, files_total, chunks, phase);
        },
    )
    .await?;

    println!();
    println!(
        "Indexed '{}': {} chunks across {} source(s).",
        updated.name,
        updated.chunk_count,
        updated.sources.len()
    );
    Ok(())
}

/// `monkey-cli stacks import-v1 <name>` — publishes an already-indexed stack's
/// v1 index as its first v2 generation, reusing the stored embeddings instead of
/// computing new ones. Calls `knowledge_service::import_v1_generation_impl`
/// directly, the same `AppHandle`-free entry point the desktop command wraps, so
/// there is nothing to reimplement here: no embeddings server, no progress
/// callback, no network.
pub fn import_v1(name: &str) -> Result<(), String> {
    let app_data = little_monkey_lib::app_paths::data_dir()
        .ok_or("Could not resolve the app data directory")?;
    let registry = knowledge_core::list_impl(&app_data.join("stacks"))?;
    let stack = find_by_name(&registry, name)
        .ok_or_else(|| format!("No knowledge stack named '{}'", name.trim()))?;

    let report =
        little_monkey_lib::knowledge_service::import_v1_generation_impl(&app_data, &stack.id)?;
    println!(
        "Imported '{}' into Knowledge 2.0: {} chunks across {} object(s), {}-dimensional, reusing \
         every existing embedding.",
        stack.name, report.chunk_count, report.object_count, report.dimension,
    );
    println!("Active generation: {}", report.generation_id);
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// Terminal progress renderer for [`reindex`] — one continuously-overwritten
/// line, cleared (`\x1b[2K`) before each redraw so a shorter phase's text
/// never leaves stray trailing characters from a longer previous line.
fn render_progress(files_done: usize, files_total: usize, chunks: usize, phase: &str) {
    let line = match phase {
        "walking" => "Scanning files…".to_string(),
        "chunking" => {
            format!("Hashing/chunking {files_done}/{files_total} files ({chunks} chunks so far)…")
        }
        "embedding" => format!("Embedding… {chunks} chunks ready"),
        "done" => format!("Done — {chunks} chunks."),
        other => other.to_string(),
    };
    print!("\r\x1b[2K{line}");
    std::io::stdout().flush().ok();
}

/// `agent.rs::execute_tool_call`'s `search_docs` dispatch target — resolves
/// the model's `stack` name argument through the exact same
/// `knowledge_core::resolve_search_stack_ids` the desktop app's
/// `tool_search_docs` command uses, then ranks via the same
/// `stacks::query_stacks_v2_first`, so a CLI and GUI search_docs call against
/// the same stack/query produce identically shaped, identically ranked results.
///
/// Sharing the ranking path is what makes that last clause true. Resolving names
/// identically and then calling v1's `query_impl` directly, as this did, meant
/// the CLI agent never consulted Knowledge 2.0 at all: for any stack whose owner
/// had run the v1→v2 import, the desktop agent answered from the hybrid index
/// and the CLI agent from v1's brute-force cosine scan, over the same registry.
///
/// `attached_stacks` is this invocation's `--stack NAME` list (see
/// `main.rs`'s `Cli::stack`) — threaded through as the allow-list so a call
/// with no `stack` argument (or one naming a real, indexed-but-not-attached
/// stack) can't sweep in every indexed stack on the machine, the same
/// session-scoping fix `tool_search_docs`'s `allowed_stack_names` gives the
/// desktop app. Empty (no `--stack` ever given) is passed through as `None`
/// — "unrestricted" — since there is no attachment concept to scope to at
/// all in that case, preserving the CLI's original whole-registry behavior.
pub async fn search_docs(
    state: &AppState,
    query: String,
    stack: Option<String>,
    max_results: Option<u32>,
    attached_stacks: &[String],
) -> Result<Vec<StackQueryResult>, String> {
    let base = base_dir().ok_or("Could not resolve the app data directory")?;
    let app_data = base
        .parent()
        .ok_or("Could not resolve the app data directory")?;
    let registry = knowledge_core::list_impl(&base)?;
    let allowed = if attached_stacks.is_empty() {
        None
    } else {
        Some(attached_stacks)
    };
    let stack_ids = knowledge_core::resolve_search_stack_ids(&registry, allowed, stack.as_deref())?;
    let k = max_results.unwrap_or(6) as usize;
    stacks::query_stacks_v2_first(
        app_data,
        state,
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

    fn stack(name: &str) -> KnowledgeStack {
        KnowledgeStack {
            id: format!("id-{name}"),
            name: name.to_string(),
            sources: Vec::new(),
            embedding: little_monkey_lib::knowledge_core::EmbeddingSpec {
                backend: little_monkey_lib::knowledge_core::EmbeddingBackend::Llama,
                model_id_or_tag: "test-model".to_string(),
                dim: 768,
                query_prefix: String::new(),
                doc_prefix: String::new(),
            },
            chunk_chars: 1600,
            chunk_overlap: 200,
            indexed_at: None,
            chunk_count: 0,
        }
    }

    #[test]
    fn find_by_name_matches_case_insensitively_and_trims() {
        let registry = vec![stack("Docs"), stack("Notes")];
        let found = find_by_name(&registry, "  docs  ").expect("case-insensitive, trimmed match");
        assert_eq!(found.name, "Docs");
    }

    #[test]
    fn find_by_name_returns_none_for_an_unknown_name() {
        let registry = vec![stack("Docs")];
        assert!(find_by_name(&registry, "Nonexistent").is_none());
    }
}

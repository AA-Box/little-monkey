import { invoke } from "@tauri-apps/api/core";
import { writeTextFile } from "@tauri-apps/plugin-fs";

import { redactSensitiveText } from "./durableRun";

/**
 * Mirrors the Rust `MemoryEntry` struct (src-tauri/src/memory.rs) exactly —
 * field names/casing must match the serde JSON representation returned by
 * `memory_list_all` and embedded (flattened) in a memory export file.
 *
 * `scope` is only ever `"global"` or `"project"` — this app's memory store
 * has no `"workspace"`, `"user"`, `"device"`, or connector-derived scope to
 * report (see `memory.rs`'s module docs for why). `project_root` is the
 * canonical workspace root path when `scope === "project"`, `null` for
 * `"global"`.
 */
export interface MemoryEntry {
  id: string;
  text: string;
  source: "agent" | "user";
  created_at: string;
  enabled: boolean;
  source_turn_id: string | null;
  /** Folded into the prompt first; exempt from `expires_at` and from the
   * per-scope fact cap (20 pins per scope instead). */
  pinned: boolean;
  /** RFC 3339 UTC. Checked when a prompt is assembled, not by a background
   * job — an expired memory stays on disk until it is purged. */
  expires_at: string | null;
  /** When a system prompt was last assembled from this memory. Throttled
   * backend-side to one write an hour per memory, so it is "the last time,
   * to within an hour", not an exact per-prompt stamp. */
  last_used_at: string | null;
  /** For a merged memory: the ids of the memories it was merged from. */
  merged_from: string[];
  /** For an original retired by a merge: the merged memory's id. */
  merged_into: string | null;
  /** When a merge retired this memory. Kept on disk purely so the merge can
   * be undone; never reaches a prompt. */
  retired_at: string | null;
  scope: "global" | "project";
  project_root: string | null;
}

/** Mirrors the Rust `MemoryExportEntry` struct — a `MemoryEntry` plus
 * whether its `text` was redacted before being written to the export file. */
export interface MemoryExportEntry extends MemoryEntry {
  redacted: boolean;
}

/** Mirrors the Rust `MemoryExportFile` struct — the shape `buildMemoryExport`
 * writes and `memory_import` (Rust) reads back. */
export interface MemoryExportFile {
  version: number;
  exported_at: string;
  redacted: boolean;
  entries: MemoryExportEntry[];
}

export interface MemoryExportSummary {
  path: string;
  count: number;
  redacted_count: number;
}

/** Mirrors the Rust `MemoryImportSummary` struct. */
export interface MemoryImportSummary {
  added: number;
  skipped_duplicate: number;
  errors: string[];
}

/** Current (and, so far, only) memory export file schema version — mirrors
 * `memory.rs`'s internal `SCHEMA_VERSION`, though the two are independent:
 * this only versions the export file shape. */
const MEMORY_EXPORT_SCHEMA_VERSION = 1;

/** Every memory ever recorded, across every project root and the global
 * scope, whatever its lifecycle state — Memory Studio's full listing.
 * Unlike `rulesStore`'s `facts` (which only ever holds the current primary
 * workspace's prompt-eligible facts, the prompt-facing view), this is the
 * "inspect and manage everything" view, including the disabled, expired and
 * merge-retired memories that never reach a prompt. */
export async function listAllMemories(): Promise<MemoryEntry[]> {
  return invoke("memory_list_all");
}

/** Edit a memory's text regardless of which project (if any) is currently
 * open — pass the `project_root` from the `MemoryEntry` being edited
 * (`null` for a global memory). */
export async function updateMemory(id: string, projectRoot: string | null, text: string): Promise<MemoryEntry> {
  return invoke("memory_studio_update", { id, projectRoot, text });
}

/** Soft-disable ("turn off without deleting") or re-enable a memory. A
 * disabled memory stays on disk but `memory_list`/`rulesStore.facts` never
 * returns it again, so it stops entering future prompts immediately. */
export async function setMemoryEnabled(id: string, projectRoot: string | null, enabled: boolean): Promise<MemoryEntry> {
  return invoke("memory_studio_set_enabled", { id, projectRoot, enabled });
}

/** Permanently delete a memory regardless of which project (if any) is
 * currently open. Deleting a *merged* memory also deletes the originals it
 * retired — undo is `unmergeMemories`' job, and a "forget" that put two
 * memories back into the next prompt would be the wrong end state. */
export async function deleteMemory(id: string, projectRoot: string | null): Promise<void> {
  return invoke("memory_studio_delete", { id, projectRoot });
}

/** Pin or unpin a memory. A pinned memory is folded into the prompt first,
 * never expires, and does not count toward the 100-memory per-scope cap —
 * it counts toward a 20-pin ceiling instead. */
export async function setMemoryPinned(id: string, projectRoot: string | null, pinned: boolean): Promise<MemoryEntry> {
  return invoke("memory_studio_set_pinned", { id, projectRoot, pinned });
}

/** Set (`YYYY-MM-DD`, which expires at the end of that day, or a full
 * RFC 3339 UTC stamp) or clear (`null`) a memory's expiry. */
export async function setMemoryExpiry(id: string, projectRoot: string | null, expiresAt: string | null): Promise<MemoryEntry> {
  return invoke("memory_studio_set_expiry", { id, projectRoot, expiresAt });
}

/** Combine two or more memories from one scope into a single memory that
 * records their ids. The originals are retired, not deleted. Pass `null`
 * for `text` to join the originals' texts. */
export async function mergeMemories(ids: string[], projectRoot: string | null, text: string | null): Promise<MemoryEntry> {
  return invoke("memory_studio_merge", { ids, projectRoot, text });
}

/** Undo a merge: restore the originals and drop the merged memory (so any
 * edit made to the merged text after the merge is lost). Resolves to the
 * number of originals restored. */
export async function unmergeMemories(id: string, projectRoot: string | null): Promise<number> {
  return invoke("memory_studio_unmerge", { id, projectRoot });
}

/** Permanently delete every expired memory, in *every* scope — not just the
 * scope currently being filtered. Merge-retired originals are never purged;
 * they are what makes a merge undoable. Resolves to the number removed. */
export async function purgeExpiredMemories(): Promise<number> {
  return invoke("memory_studio_purge_expired");
}

/** Record that a system prompt was just assembled from these memories.
 * Fire-and-forget by design: `systemPrompt.ts`'s `currentSystemPrompt`
 * calls this on every prompt build, several test suites stub `invoke` to
 * reject unknown commands, and an unhandled rejection there would fail
 * unrelated tests — so this returns `void` and swallows its own failure. A
 * memory-store hiccup must never break a turn. The write itself is
 * throttled backend-side (`mark_used_impl`), so calling it every iteration
 * costs one no-op read, not a file rewrite. */
export function markMemoriesUsed(ids: string[]): void {
  if (ids.length === 0) return;
  void invoke("memory_mark_used", { ids }).catch(() => undefined);
}

/** Mirrors Rust's `reaches_prompt` (`memory.rs`) for **display only** — the
 * Studio's "why isn't this reaching the prompt?" badges. The real filter is
 * `list_impl`, the one function both `memory_list` and the CLI's prompt
 * composition call; this never touches `rulesStore.facts`, because two
 * filters would be two truths. The exclusion itself is proved Rust-side by
 * `expired_and_merge_retired_facts_are_excluded_from_list_impl` and the
 * CLI's `an_expired_or_merged_away_fact_is_excluded_from_the_cli_system_prompt`. */
export function wouldReachPrompt(entry: MemoryEntry, now = new Date().toISOString()): boolean {
  if (!entry.enabled || entry.retired_at !== null) return false;
  if (entry.pinned) return true;
  return entry.expires_at === null || entry.expires_at > now;
}

/** Builds the portable export file shape from a full memory listing —
 * separated from `exportMemories` so it's testable as a pure function.
 * Redacted by default: reuses `redactSensitiveText` (`./durableRun.ts`),
 * the same secret-shaped-text scanner the run-capsule export
 * (`runCapsule.ts`'s `createRedactedRunCapsuleExport`) and checkpoint
 * evidence already redact tool output/arguments with, rather than a second,
 * hand-rolled scanner living only in Memory Studio. */
export function buildMemoryExport(entries: MemoryEntry[], redact: boolean): MemoryExportFile {
  const exportEntries: MemoryExportEntry[] = entries.map((entry) => {
    if (!redact) return { ...entry, redacted: false };
    const redactedText = redactSensitiveText(entry.text);
    return { ...entry, text: redactedText, redacted: redactedText !== entry.text };
  });
  return {
    version: MEMORY_EXPORT_SCHEMA_VERSION,
    exported_at: new Date().toISOString(),
    redacted: redact,
    entries: exportEntries,
  };
}

/** Export every memory (every project + global) to a portable JSON file at
 * `path`. Redacted by default (`redact` omitted or `true`) — obvious
 * secret-shaped values (API keys, tokens, passwords, connection-string
 * credentials) are masked out of each memory's `text` before the file is
 * written. Pass `redact: false` for an explicit, unredacted export (e.g. a
 * user moving their own memories to another install of this app). */
export async function exportMemories(path: string, redact = true): Promise<MemoryExportSummary> {
  const entries = await listAllMemories();
  const file = buildMemoryExport(entries, redact);
  await writeTextFile(path, `${JSON.stringify(file, null, 2)}\n`);
  return {
    path,
    count: file.entries.length,
    redacted_count: file.entries.filter((entry) => entry.redacted).length,
  };
}

/** Import memories from a file previously written by `exportMemories`.
 * Restores each entry's original scope and (soft-)disabled state; an entry
 * whose text exactly matches an already-stored memory in the same scope is
 * skipped as a duplicate rather than creating a second copy, so importing
 * the same file twice is safe. Validation, deduplication, and per-project
 * caps are enforced Rust-side (`memory_import`/`import_impl`), reusing the
 * exact same rules `tool_remember`/the Settings "Add fact" affordance use. */
export async function importMemories(path: string): Promise<MemoryImportSummary> {
  return invoke("memory_import", { path });
}

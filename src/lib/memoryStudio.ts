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
 * scope, enabled or disabled — Memory Studio's full listing. Unlike
 * `rulesStore`'s `facts` (which only ever holds the current primary
 * workspace's *enabled* facts, the prompt-facing view), this is the
 * "inspect and manage everything" view ROADMAP.md's Memory Studio spec
 * asks for. */
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
 * currently open. */
export async function deleteMemory(id: string, projectRoot: string | null): Promise<void> {
  return invoke("memory_studio_delete", { id, projectRoot });
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

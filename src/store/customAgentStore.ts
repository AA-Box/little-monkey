import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import {
  CUSTOM_AGENTS_DIR,
  collectCustomAgents,
  parseCustomAgentFile,
  type CustomAgentDef,
  type CustomAgentLoadError,
} from "../lib/customAgents";

/**
 * Loaded `.monkey/agents/*.md` definitions for the OPEN workspace — refreshed
 * from disk (never persisted: the files ARE the store of record, and a stale
 * cached def could grant a tool list its file no longer declares). Reads go
 * through the same un-gated, workspace-validated Rust commands the
 * @-mention/file plumbing already uses (`tool_list_dir`/`tool_read_file`),
 * so path resolution and root confinement are `workspace::resolve_path_and_root`'s
 * existing job, not this module's.
 */
interface CustomAgentStoreState {
  defs: Record<string, CustomAgentDef>;
  errors: CustomAgentLoadError[];
  /** `null` until the first refresh settles — lets the settings panel show
   * "not scanned yet" apart from "scanned, none found". */
  loadedAt: number | null;
  /** Re-scans `.monkey/agents/` in the primary workspace root. Never throws;
   * a missing directory (the overwhelmingly common case) is simply zero
   * defs. Serialized: a refresh started while one is in flight reuses it. */
  refresh: () => Promise<void>;
}

let inFlight: Promise<void> | null = null;

async function scan(): Promise<{ defs: Record<string, CustomAgentDef>; errors: CustomAgentLoadError[] }> {
  let entries: { name?: unknown; is_dir?: unknown }[];
  try {
    entries = await invoke<{ name?: unknown; is_dir?: unknown }[]>("tool_list_dir", { path: CUSTOM_AGENTS_DIR });
  } catch {
    return { defs: {}, errors: [] }; // no .monkey/agents directory (or no workspace open)
  }
  const files = entries
    .filter((entry) => entry.is_dir !== true && typeof entry.name === "string" && (entry.name as string).endsWith(".md"))
    .map((entry) => entry.name as string)
    .sort();
  const parsed = await Promise.all(
    files.map(async (file) => {
      const path = `${CUSTOM_AGENTS_DIR}/${file}`;
      try {
        const raw = await invoke<string>("tool_read_file", { path });
        return parseCustomAgentFile(path, raw);
      } catch (err) {
        return { ok: false as const, error: { path, message: err instanceof Error ? err.message : String(err) } };
      }
    }),
  );
  return collectCustomAgents(parsed);
}

export const useCustomAgentStore = create<CustomAgentStoreState>((set) => ({
  defs: {},
  errors: [],
  loadedAt: null,

  refresh: () => {
    if (inFlight) return inFlight;
    inFlight = scan()
      .then(({ defs, errors }) => set({ defs, errors, loadedAt: Date.now() }))
      .catch(() => set({ defs: {}, errors: [], loadedAt: Date.now() }))
      .finally(() => {
        inFlight = null;
      });
    return inFlight;
  },
}));

/** Alphabetical def list — fresh array per call, wrap in `useShallow` at
 * subscription sites, same convention as `selectSavedWorkflowList`. */
export function selectCustomAgentList(state: CustomAgentStoreState): CustomAgentDef[] {
  return Object.values(state.defs).sort((a, b) => a.name.localeCompare(b.name));
}

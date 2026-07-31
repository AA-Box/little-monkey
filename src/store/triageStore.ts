import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { errorMessage } from "../lib/errors";

/** Mirrors the Rust `TriageSource` enum (src-tauri/src/triage.rs) exactly —
 * `#[serde(rename_all = "snake_case")]`. */
export type TriageSource = "github" | "slack" | "jira";

/** Mirrors the Rust `DraftActionKind` enum exactly. */
export type DraftActionKind = "reply" | "comment" | "status_update";

/** Mirrors the Rust `DraftAction` struct exactly. */
export interface DraftAction {
  kind: DraftActionKind;
  draft_text: string;
  target: string;
}

/** Mirrors the Rust `TriageItem` struct exactly — plain snake_case field
 * names, same convention as `ConnectorAccount`'s mirroring. */
export interface TriageItem {
  id: string;
  source: TriageSource;
  title: string;
  summary: string;
  rank_score: number;
  url: string;
  staleness_days: number;
  suggested_action: DraftAction | null;
  connector_account_id: string | null;
}

/** Mirrors the Rust `TriageSourceSpec` enum exactly (`#[serde(tag = "kind")]`)
 * — one requested queue to refresh. */
export type TriageSourceSpec =
  | { kind: "github"; owner: string; repo: string }
  | { kind: "slack"; connector_account_id: string; channel_id: string }
  | { kind: "jira"; connector_account_id: string; project_key: string };

/** Mirrors the Rust `TriageRefreshResult` struct exactly — a partial-success
 * shape: a source failing to fetch (expired token, transient network blip)
 * must not discard items already fetched from every other requested
 * source, so `errors` travels alongside `items` rather than the whole call
 * rejecting outright. */
export interface TriageRefreshResult {
  items: TriageItem[];
  errors: string[];
}

export interface TriageStore {
  items: TriageItem[];
  loading: boolean;
  error: string | null;
  /** Loads the last-persisted queue from `triage.json` without any network
   * call — safe to run on mount. */
  list: () => Promise<void>;
  /** Re-fetches every requested source live (read-only), ranks the results,
   * persists them, and replaces the queue. */
  refresh: (sources: TriageSourceSpec[]) => Promise<void>;
  /** Generates draft text for one item's suggested action by calling the
   * given chat model — no approval needed, this never leaves the read/local
   * boundary (the model call itself aside). */
  generateDraft: (
    itemId: string,
    providerId: string,
    model: string,
    effort?: string,
  ) => Promise<TriageItem>;
  /** Sends the item's drafted reply/comment/status-update. Permission-gated
   * on the Rust side (`request_permission`, a distinct tool name per action
   * kind) — this call can reject with "Permission denied". */
  sendDraft: (itemId: string) => Promise<void>;
  /** Hides the item from this session's queue view only — matches
   * `TriagePanel.discardedNotice`'s copy ("Discarded from this session's
   * view"). Never touches `triage.json` or the backend: the item is still
   * there and will reappear the next time `list()`/`refresh()` runs (a
   * fresh app launch, or an explicit queue refresh), unlike `sendDraft`
   * which removes it for good. */
  discard: (itemId: string) => void;
}

// Sequencing guard for concurrent list()/refresh() calls — same rationale
// and pattern as `connectorsStore.ts`'s `latestRefreshRequest`.
let latestListRequest = 0;

export const useTriageStore = create<TriageStore>((set, get) => ({
  items: [],
  loading: false,
  error: null,

  list: async () => {
    const requestId = ++latestListRequest;
    set({ loading: true, error: null });
    try {
      const items = await invoke<TriageItem[]>("triage_list");
      if (requestId !== latestListRequest) return;
      set({ items, loading: false });
    } catch (err) {
      if (requestId !== latestListRequest) return;
      set({ error: errorMessage(err), loading: false });
    }
  },

  refresh: async (sources) => {
    const requestId = ++latestListRequest;
    set({ loading: true, error: null });
    try {
      const result = await invoke<TriageRefreshResult>("triage_refresh", { sources });
      if (requestId !== latestListRequest) return;
      set({
        items: result.items,
        loading: false,
        error: result.errors.length > 0 ? result.errors.join("; ") : null,
      });
    } catch (err) {
      if (requestId !== latestListRequest) return;
      set({ error: errorMessage(err), loading: false });
    }
  },

  generateDraft: async (itemId, providerId, model, effort) => {
    const updated = await invoke<TriageItem>("triage_generate_draft", {
      itemId,
      providerId,
      model,
      effort: effort ?? null,
    });
    set({ items: get().items.map((item) => (item.id === updated.id ? updated : item)) });
    return updated;
  },

  sendDraft: async (itemId) => {
    await invoke("triage_send_draft", { itemId });
    set({ items: get().items.filter((item) => item.id !== itemId) });
  },

  discard: (itemId) => {
    set({ items: get().items.filter((item) => item.id !== itemId) });
  },
}));

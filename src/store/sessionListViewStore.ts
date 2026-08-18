import { create } from "zustand";

import {
  DEFAULT_SESSION_LIST_PREFS,
  type GroupBy,
  type SessionListPrefs,
  type SortBy,
  type StatusFilter,
} from "../components/Chat/sessionListView";

/** localStorage key the sidebar's view menu persists its four axes under. */
export const SESSION_LIST_VIEW_STORAGE_KEY = "little-monkey-session-list-view";

const STATUSES: StatusFilter[] = ["active", "archived", "all"];
const GROUP_BYS: GroupBy[] = ["date", "folder", "state", "groups", "none"];
const SORT_BYS: SortBy[] = ["alphabetical", "created", "recency"];

/** Per-device view preference, exactly like `localeStore`: small, local, and
 * never worth a round trip to the Rust side. Anything unrecognized (a value
 * from a newer build, a hand-edited entry) falls back to the default rather
 * than leaving the sidebar filtered by something it cannot render. */
function hydrate(): SessionListPrefs {
  try {
    const raw = localStorage.getItem(SESSION_LIST_VIEW_STORAGE_KEY);
    if (!raw) return DEFAULT_SESSION_LIST_PREFS;
    const parsed = JSON.parse(raw) as Partial<SessionListPrefs>;
    return {
      status: STATUSES.includes(parsed.status as StatusFilter)
        ? (parsed.status as StatusFilter)
        : DEFAULT_SESSION_LIST_PREFS.status,
      environments: Array.isArray(parsed.environments)
        ? parsed.environments.filter((value): value is string => typeof value === "string")
        : DEFAULT_SESSION_LIST_PREFS.environments,
      groupBy: GROUP_BYS.includes(parsed.groupBy as GroupBy)
        ? (parsed.groupBy as GroupBy)
        : DEFAULT_SESSION_LIST_PREFS.groupBy,
      sortBy: SORT_BYS.includes(parsed.sortBy as SortBy)
        ? (parsed.sortBy as SortBy)
        : DEFAULT_SESSION_LIST_PREFS.sortBy,
    };
  } catch {
    return DEFAULT_SESSION_LIST_PREFS;
  }
}

export interface SessionListViewState {
  prefs: SessionListPrefs;
  setPrefs: (patch: Partial<SessionListPrefs>) => void;
  /** Adds or removes one environment from the selection. Ticking the last
   * unticked one collapses back to "all environments", so the menu never
   * lands in the state where every box is ticked and the filter still claims
   * to be narrowing something. */
  toggleEnvironment: (environment: string, known: readonly string[]) => void;
}

export const useSessionListViewStore = create<SessionListViewState>((set, get) => ({
  prefs: hydrate(),
  setPrefs: (patch) => {
    const prefs = { ...get().prefs, ...patch };
    set({ prefs });
    try {
      localStorage.setItem(SESSION_LIST_VIEW_STORAGE_KEY, JSON.stringify(prefs));
    } catch {
      // Best-effort persistence.
    }
  },
  toggleEnvironment: (environment, known) => {
    const current = get().prefs.environments;
    const selected = current.length === 0 ? [...known] : current;
    const next = selected.includes(environment)
      ? selected.filter((value) => value !== environment)
      : [...selected, environment];
    get().setPrefs({
      environments: next.length === known.length || next.length === 0 ? [] : next,
    });
  },
}));

export default useSessionListViewStore;

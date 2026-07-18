/**
 * State for the native in-app browser pane (Claude-Desktop-style tabbed
 * browser). Real page rendering lives in Rust-side child webviews
 * (`src-tauri/src/browser_pane.rs`); this store owns the tab list, active
 * selection, pane width, and the address-bar/navigation actions that call
 * into those commands.
 *
 * Tabs come in two flavors:
 * - "local" tabs (`local-*` ids) — a freshly opened, not-yet-navigated New
 *   Tab. No native webview exists yet, so the pane renders its own start
 *   surface. First navigation swaps the tab in place for a native one.
 * - native tabs — id is the Rust webview label (`browser-pane-*`).
 */
import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";

export interface BrowserPaneTab {
  id: string;
  url: string;
  title: string;
  loading: boolean;
  favicon: string | null;
}

/** Payload of the Rust `browser-pane://tab` metadata event. */
export interface BrowserTabEvent {
  label: string;
  url?: string;
  title?: string;
  loading?: boolean;
}

const WIDTH_STORAGE_KEY = "little-monkey:browser-pane:width";
export const MIN_PANE_WIDTH = 380;
export const DEFAULT_PANE_WIDTH = 560;

function isLocalTab(id: string): boolean {
  return id.startsWith("local-");
}

let nextLocalId = 0;

function storedWidth(): number {
  try {
    const parsed = Number(localStorage.getItem(WIDTH_STORAGE_KEY));
    if (Number.isFinite(parsed) && parsed >= MIN_PANE_WIDTH) return parsed;
  } catch {
    // Best-effort persistence only.
  }
  return DEFAULT_PANE_WIDTH;
}

/**
 * Address-bar smart parsing: explicit scheme passes through, something that
 * looks like a host becomes https:// (http:// for loopback), anything else
 * becomes a web search.
 */
export function normalizeAddress(raw: string): string | null {
  const trimmed = raw.trim();
  if (!trimmed) return null;
  if (/^(https?|about):/i.test(trimmed)) return trimmed;
  if (/^(localhost|127\.0\.0\.1|\[::1\])(:\d+)?([/?#]|$)/i.test(trimmed)) {
    return `http://${trimmed}`;
  }
  if (/^[\w-]+(\.[\w-]+)+(:\d+)?([/?#]|$)/.test(trimmed)) {
    return `https://${trimmed}`;
  }
  return `https://www.google.com/search?q=${encodeURIComponent(trimmed)}`;
}

interface BrowserPaneStore {
  /** Whether the pane is shown. Tabs survive closing the pane. */
  open: boolean;
  tabs: BrowserPaneTab[];
  activeId: string | null;
  width: number;
  /** Last command error, surfaced as a dismissible banner in the pane. */
  error: string | null;

  setOpen(open: boolean): void;
  setWidth(width: number): void;
  setError(error: string | null): void;
  /** Open a New Tab (local, no webview) or a native tab at `url`. */
  openTab(url?: string): Promise<void>;
  closeTab(id: string): Promise<void>;
  selectTab(id: string): Promise<void>;
  /** Navigate the given tab from the address bar (or a start-page action). */
  navigate(id: string, rawAddress: string): Promise<void>;
  goBack(id: string): Promise<void>;
  goForward(id: string): Promise<void>;
  reload(id: string): Promise<void>;
  /** Apply a Rust-side metadata event to the matching tab. */
  applyTabEvent(event: BrowserTabEvent): void;
  /** Fetch + attach the favicon for a tab's current URL (host-cached in Rust). */
  requestFavicon(id: string, url: string): Promise<void>;
}

async function paneInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  if (!isTauri()) throw new Error("The in-app browser requires the desktop app.");
  return invoke<T>(command, args);
}

export const useBrowserPaneStore = create<BrowserPaneStore>((set, get) => ({
  open: false,
  tabs: [],
  activeId: null,
  width: storedWidth(),
  error: null,

  setOpen: (open) => set({ open }),

  setWidth: (width) => {
    const clamped = Math.max(MIN_PANE_WIDTH, Math.min(width, Math.round(window.innerWidth * 0.7)));
    set({ width: clamped });
    try {
      localStorage.setItem(WIDTH_STORAGE_KEY, String(clamped));
    } catch {
      // Best-effort persistence only.
    }
  },

  setError: (error) => set({ error }),

  openTab: async (url) => {
    if (!url) {
      // Local New Tab: no webview yet, so hide whichever native tab is
      // showing — the pane renders its own start surface instead.
      nextLocalId += 1;
      const id = `local-${nextLocalId}`;
      set((state) => ({
        tabs: [...state.tabs, { id, url: "", title: "", loading: false, favicon: null }],
        activeId: id,
      }));
      try {
        await paneInvoke("browser_pane_set_visible", { visible: false });
      } catch {
        // No native tabs to hide yet (or non-Tauri dev) — nothing to do.
      }
      return;
    }
    try {
      const label = await paneInvoke<string>("browser_pane_open_tab", { url });
      set((state) => ({
        tabs: [...state.tabs, { id: label, url, title: "", loading: true, favicon: null }],
        activeId: label,
        error: null,
      }));
      void get().requestFavicon(label, url);
    } catch (err) {
      set({ error: String(err) });
    }
  },

  closeTab: async (id) => {
    const { tabs, activeId } = get();
    const index = tabs.findIndex((tab) => tab.id === id);
    if (index === -1) return;
    const remaining = tabs.filter((tab) => tab.id !== id);
    // Claude-Desktop behavior: closing the active tab activates its
    // right-hand neighbor, falling back to the new last tab.
    const nextActive =
      activeId === id ? (remaining[index]?.id ?? remaining[remaining.length - 1]?.id ?? null) : activeId;
    set({ tabs: remaining, activeId: nextActive });
    if (!isLocalTab(id)) {
      try {
        await paneInvoke("browser_pane_close_tab", { label: id });
      } catch (err) {
        set({ error: String(err) });
      }
    }
    if (nextActive !== null && nextActive !== activeId) {
      await get().selectTab(nextActive);
    } else if (nextActive === null) {
      try {
        await paneInvoke("browser_pane_set_visible", { visible: false });
      } catch {
        // Nothing native left to hide.
      }
    }
  },

  selectTab: async (id) => {
    set({ activeId: id });
    try {
      if (isLocalTab(id)) {
        await paneInvoke("browser_pane_set_visible", { visible: false });
      } else {
        await paneInvoke("browser_pane_select_tab", { label: id });
      }
    } catch (err) {
      set({ error: String(err) });
    }
  },

  navigate: async (id, rawAddress) => {
    const url = normalizeAddress(rawAddress);
    if (!url) return;
    const tab = get().tabs.find((entry) => entry.id === id);
    if (!tab) return;
    try {
      if (isLocalTab(id)) {
        // First navigation of a New Tab: create the native webview and swap
        // the local placeholder for it in place, keeping tab order.
        const label = await paneInvoke<string>("browser_pane_open_tab", { url });
        set((state) => ({
          tabs: state.tabs.map((entry) =>
            entry.id === id ? { ...entry, id: label, url, loading: true } : entry,
          ),
          activeId: state.activeId === id ? label : state.activeId,
          error: null,
        }));
        void get().requestFavicon(label, url);
      } else {
        await paneInvoke("browser_pane_navigate", { label: id, url });
        set((state) => ({
          tabs: state.tabs.map((entry) =>
            entry.id === id ? { ...entry, url, loading: true } : entry,
          ),
          error: null,
        }));
      }
    } catch (err) {
      set({ error: String(err) });
    }
  },

  goBack: async (id) => {
    if (isLocalTab(id)) return;
    try {
      await paneInvoke("browser_pane_go_back", { label: id });
    } catch (err) {
      set({ error: String(err) });
    }
  },

  goForward: async (id) => {
    if (isLocalTab(id)) return;
    try {
      await paneInvoke("browser_pane_go_forward", { label: id });
    } catch (err) {
      set({ error: String(err) });
    }
  },

  reload: async (id) => {
    if (isLocalTab(id)) return;
    try {
      await paneInvoke("browser_pane_reload", { label: id });
    } catch (err) {
      set({ error: String(err) });
    }
  },

  applyTabEvent: (event) => {
    const { tabs } = get();
    const tab = tabs.find((entry) => entry.id === event.label);
    if (!tab) return;
    const urlChanged = event.url !== undefined && event.url !== tab.url;
    set({
      tabs: tabs.map((entry) =>
        entry.id === event.label
          ? {
              ...entry,
              url: event.url ?? entry.url,
              title: event.title ?? entry.title,
              loading: event.loading ?? entry.loading,
            }
          : entry,
      ),
    });
    if (urlChanged && event.url) {
      void get().requestFavicon(event.label, event.url);
    }
  },

  requestFavicon: async (id, url) => {
    if (!/^https?:/i.test(url)) return;
    try {
      const favicon = await paneInvoke<string | null>("browser_pane_favicon", { pageUrl: url });
      set((state) => ({
        tabs: state.tabs.map((entry) => (entry.id === id ? { ...entry, favicon } : entry)),
      }));
    } catch {
      // Favicons are cosmetic — ignore failures.
    }
  },
}));

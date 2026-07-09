import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * Mirrors the Rust `PermissionRequestPayload` struct
 * (src-tauri/src/permissions.rs) — emitted as the payload of the
 * `permission://request` Tauri event.
 */
export interface PermissionRequest {
  id: string;
  tool: string;
  detail: string;
}

/**
 * Stable string identifiers for the five permission modes, shared verbatim
 * with the Rust side (src-tauri/src/permissions.rs::VALID_MODES). See
 * ModeSelector for user-facing copy describing each mode.
 */
export type PermissionMode = "manual" | "acceptEdits" | "plan" | "auto" | "bypass";

const PERMISSION_MODE_STORAGE_KEY = "little-monkey-permission-mode";

const VALID_PERMISSION_MODES: PermissionMode[] = ["manual", "acceptEdits", "plan", "auto", "bypass"];

/**
 * Reads the persisted mode from localStorage for the store's initial value.
 * "bypass" is deliberately never restored — it's the most dangerous mode, so
 * every fresh app start must begin at "manual" regardless of what was active
 * when the app was last closed. A stale "bypass" value is also immediately
 * overwritten back to "manual" so it doesn't linger in storage.
 */
function readInitialMode(): PermissionMode {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(PERMISSION_MODE_STORAGE_KEY);
  } catch {
    return "manual";
  }

  if (stored === "bypass") {
    try {
      localStorage.setItem(PERMISSION_MODE_STORAGE_KEY, "manual");
    } catch {
      // Best-effort; if storage isn't writable there's nothing more to do.
    }
    return "manual";
  }

  if (stored && (VALID_PERMISSION_MODES as string[]).includes(stored)) {
    return stored as PermissionMode;
  }

  return "manual";
}

export interface PermissionStore {
  /** The currently pending permission request, or null if none is awaiting a decision. */
  pending: PermissionRequest | null;
  /**
   * Tool names currently granted "allow for session" status, purely for
   * display (e.g. a persistent banner warning the user unattended grants
   * are active). This mirrors backend state on a best-effort basis — it is
   * not consulted for any access-control decision, so it can never be used
   * to bypass anything even if it drifts out of sync.
   */
  sessionGrants: string[];
  /**
   * The active permission mode. Restored from localStorage at store
   * initialization (see `readInitialMode`), except "bypass" is never
   * restored. The Rust side always boots fresh at "manual" regardless of
   * this restored value — something must push a restored non-"manual" mode
   * to the backend once at startup; that one-time sync lives in
   * ModeSelector's mount effect rather than here, to avoid import-time
   * side effects in a store module.
   */
  mode: PermissionMode;
  /**
   * Resolve the pending permission request. `allow` grants/denies the single
   * in-flight tool call; `remember` (only meaningful when `allow` is true)
   * tells the backend to auto-allow this tool for the rest of the session.
   * No-ops if there is no pending request.
   */
  respond: (allow: boolean, remember: boolean) => Promise<void>;
  /** Reset the displayed set of session grants, e.g. when the workspace changes. */
  clearSessionGrants: () => void;
  /**
   * Switches the active permission mode: invokes the backend `set_permission_mode`
   * command, and only on success updates local state and persists to
   * localStorage. "bypass" is deliberately never persisted, so it can never
   * survive an app restart. Rejections from `invoke` propagate to the caller
   * rather than being swallowed.
   */
  setMode: (mode: PermissionMode) => Promise<void>;
}

export const usePermissionStore = create<PermissionStore>((set, get) => ({
  pending: null,
  sessionGrants: [],
  mode: readInitialMode(),

  respond: async (allow, remember) => {
    const { pending } = get();
    if (!pending) {
      return;
    }
    try {
      // NOTE: `tool` is intentionally not sent here — the backend looks it
      // up from the pending request by `id` so a caller can't claim a
      // different (more dangerous) tool than the one actually shown to the
      // user. See src-tauri/src/permissions.rs::permission_respond.
      await invoke("permission_respond", {
        id: pending.id,
        allow,
        remember,
      });
      if (allow && remember) {
        set((state) =>
          state.sessionGrants.includes(pending.tool)
            ? state
            : { sessionGrants: [...state.sessionGrants, pending.tool] },
        );
      }
    } finally {
      set({ pending: null });
    }
  },

  clearSessionGrants: () => set({ sessionGrants: [] }),

  setMode: async (mode) => {
    await invoke("set_permission_mode", { mode });
    set({ mode });
    if (mode !== "bypass") {
      try {
        localStorage.setItem(PERMISSION_MODE_STORAGE_KEY, mode);
      } catch {
        // Best-effort persistence; a failure here shouldn't fail the mode switch.
      }
    }
  },
}));

void listen<PermissionRequest>("permission://request", (event) => {
  usePermissionStore.setState({ pending: event.payload });
}).catch((error) => {
  console.error("Failed to listen for permission://request events", error);
});

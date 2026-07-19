import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * Safe Desktop Control — a design-validation research spike. See
 * `docs/safe-desktop-control-design.md` and `src-tauri/src/desktop_control.rs`
 * for the full threat model; this store only wraps the Tauri `invoke()`
 * calls and the two events the backend emits. It never inlines a secret —
 * there is nothing secret about a session id, an application name, or a
 * mouse coordinate.
 */

export type MouseButtonKind = "left" | "right" | "middle";

/** Mirrors `desktop_control::ControlAction`'s internally-tagged shape exactly. */
export type ControlAction =
  | { kind: "mouse_move"; x: number; y: number }
  | { kind: "mouse_click"; button: MouseButtonKind }
  | { kind: "key_press"; key: string };

/** Mirrors `desktop_control::ControlSession`. */
export interface ControlSession {
  sessionId: string;
  allowedApplications: string[];
  createdAtMs: number;
  expiresAtMs: number;
  active: boolean;
  indicatorVisible: boolean;
  approvedBatch: boolean;
}

/** Mirrors `desktop_control::PendingActionSummary`, the payload of the
 * `desktop-control://action-pending` event. */
export interface PendingActionSummary {
  actionId: string;
  sessionId: string;
  action: ControlAction;
}

/** Mirrors `desktop_control::ActionOutcome`. */
export interface ActionOutcome {
  actionId: string;
  executed: boolean;
}

export interface EmergencyStopResult {
  sessionsDeactivated: number;
  actionsCancelled: number;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export interface DesktopControlStore {
  sessions: ControlSession[];
  /** Actions awaiting a human decision, in arrival order — populated by the
   * `desktop-control://action-pending` event listener below, drained by
   * `respondAction`/`stopSession`/`emergencyStop`. */
  pendingActions: PendingActionSummary[];
  error: string | null;

  refreshSessions: () => Promise<void>;
  /** Starts a new session. Rejects (propagating the backend's exact message)
   * when permission mode is `"bypass"`, the allowlist is empty, or the
   * lifetime is out of bounds — see `desktop_control_start_session`. */
  startSession: (allowedApplications: string[], lifetimeMs: number, approvedBatch: boolean) => Promise<ControlSession>;
  stopSession: (sessionId: string) => Promise<boolean>;
  /** Requests one input action. For a non-approved-batch session this awaits
   * the user's decision on the resulting `desktop-control://action-pending`
   * prompt (surfaced via `pendingActions`) before resolving — callers should
   * expect this to take a while and should let the user cancel it via
   * `respondAction(actionId, false)` or `emergencyStop()` rather than
   * assuming it resolves quickly. */
  requestAction: (sessionId: string, targetApplicationId: string, action: ControlAction) => Promise<ActionOutcome>;
  respondAction: (actionId: string, approve: boolean) => Promise<void>;
  emergencyStop: () => Promise<EmergencyStopResult>;
  clearError: () => void;
}

export const useDesktopControlStore = create<DesktopControlStore>((set, get) => ({
  sessions: [],
  pendingActions: [],
  error: null,

  refreshSessions: async () => {
    try {
      const sessions = await invoke<ControlSession[]>("desktop_control_sessions");
      set({ sessions, error: null });
    } catch (error) {
      set({ error: errorText(error) });
    }
  },

  startSession: async (allowedApplications, lifetimeMs, approvedBatch) => {
    try {
      const session = await invoke<ControlSession>("desktop_control_start_session", {
        allowedApplications,
        lifetimeMs,
        approvedBatch,
      });
      set((state) => ({
        sessions: [...state.sessions.filter((existing) => existing.sessionId !== session.sessionId), session],
        error: null,
      }));
      return session;
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    }
  },

  stopSession: async (sessionId) => {
    try {
      const stopped = await invoke<boolean>("desktop_control_stop_session", { sessionId });
      set((state) => ({
        sessions: state.sessions.map((session) => (session.sessionId === sessionId ? { ...session, active: false } : session)),
        pendingActions: state.pendingActions.filter((action) => action.sessionId !== sessionId),
        error: null,
      }));
      return stopped;
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    }
  },

  requestAction: async (sessionId, targetApplicationId, action) => {
    // Doesn't proactively remove anything from `pendingActions` here: the
    // matching entry (if any — an approved-batch session never creates one)
    // is keyed by a backend-generated `actionId` this call never sees
    // directly, and is already removed by whichever of `respondAction` /
    // `stopSession` / `emergencyStop` resolved it.
    try {
      return await invoke<ActionOutcome>("desktop_control_request_action", {
        sessionId,
        targetApplicationId,
        action,
      });
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    }
  },

  respondAction: async (actionId, approve) => {
    try {
      await invoke("desktop_control_respond_action", { actionId, approve });
    } finally {
      set((state) => ({ pendingActions: state.pendingActions.filter((action) => action.actionId !== actionId) }));
    }
  },

  emergencyStop: async () => {
    const result = await invoke<EmergencyStopResult>("desktop_control_emergency_stop");
    set((state) => ({
      sessions: state.sessions.map((session) => ({ ...session, active: false })),
      pendingActions: [],
      error: null,
    }));
    await get().refreshSessions();
    return result;
  },

  clearError: () => set({ error: null }),
}));

// Tauri-shell only: in plain-browser dev `listen` itself throws.
if (isTauri()) {
  void listen<PendingActionSummary>("desktop-control://action-pending", (event) => {
    useDesktopControlStore.setState((state) =>
      state.pendingActions.some((pending) => pending.actionId === event.payload.actionId)
        ? state
        : { pendingActions: [...state.pendingActions, event.payload] },
    );
  }).catch((error) => {
    console.error("Failed to listen for desktop-control://action-pending events", error);
  });

  void listen<EmergencyStopResult>("desktop-control://emergency-stop", () => {
    useDesktopControlStore.setState((state) => ({
      sessions: state.sessions.map((session) => ({ ...session, active: false })),
      pendingActions: [],
    }));
  }).catch((error) => {
    console.error("Failed to listen for desktop-control://emergency-stop events", error);
  });
}

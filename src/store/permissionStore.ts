import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
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
  /** Advisory risk annotation (Phase 2 of the Plan/Act + risk-adaptive
   * permissions design — docs/roadmap/p2-plan-act-safety.md). `undefined`
   * when risk annotations are off, the tool wasn't classified, or nothing
   * usable came back from the floor/judge — `PermissionModal` shows no badge
   * in that case, never a fabricated "low risk" one. Purely informative: it
   * never changes what gets auto-approved. */
  risk_level?: "low" | "medium" | "high";
  risk_reason?: string;
  /** True when `risk_level`/`risk_reason` came from the authoritative,
   * un-overridable `path_risk_floor` rather than the LLM judge — lets the
   * modal show a stronger "sensitive path" warning and withhold "Allow for
   * session" (see `PermissionModal.tsx`'s `canRememberForSession`). Unlike
   * `risk_level`, this one is not purely informative: a floored target prompts
   * in every mode below `bypass`, and no remembered grant can answer it. Both
   * are enforced in `permissions.rs`; the modal only mirrors them. */
  risk_floored?: boolean;
  /** The description of the `code`-profile subagent (p3) this call
   * originated from, if any — a dedicated field (NOT parsed back out of
   * `detail`, the pre-fix design) so a subagent's own model-supplied
   * `description` can never forge/corrupt the shown `detail` or spoof a
   * different attribution — see `tools.rs`'s `PermissionRequestPayload.
   * agent_label` doc comment. `undefined` for every parent-turn call and any
   * `explore`-profile subagent. */
  agent_label?: string;
}

/**
 * Stable string identifiers for the six permission modes, shared verbatim
 * with the Rust side (src-tauri/src/permissions.rs::VALID_MODES). See
 * ModeSelector for user-facing copy describing each mode. `"smart"` (Phase 3
 * of the Plan/Act + risk-adaptive permissions design —
 * docs/roadmap/p2-plan-act-safety.md) auto-approves only low-risk,
 * non-floored file edits; `run_shell` never short-circuits under it, exactly
 * like `"auto"`/`"acceptEdits"` (see permissions.rs::mode_short_circuit).
 */
export type PermissionMode = "manual" | "acceptEdits" | "smart" | "plan" | "auto" | "bypass";

/**
 * The subset of `PermissionMode` that "Approve & start acting" (PlanCard.tsx)
 * is allowed to switch into — every mode except the two that must never be
 * `lastActMode`'s value: `"plan"` (that would make approving a plan a no-op)
 * and `"bypass"` (the most dangerous mode must never be silently re-entered
 * just because it happened to be active before the user entered Plan Mode —
 * mirrors `readInitialMode`'s "bypass is never restored" rule below).
 * `"smart"` is a legitimate act mode (it still enforces every prompt except
 * pre-vetted low-risk file edits), so it's included here alongside
 * `"manual"`/`"acceptEdits"`/`"auto"`.
 */
export type ActPermissionMode = "manual" | "acceptEdits" | "smart" | "auto";

const PERMISSION_MODE_STORAGE_KEY = "little-monkey-permission-mode";
const LAST_ACT_MODE_STORAGE_KEY = "little-monkey-last-act-mode";

// Exported so `recipeRunner.ts` can validate a recipe's own `permission_mode`
// field against the same source of truth, rather than a second hand-copied
// list that could drift (mirrors `permissions.rs::VALID_MODES`'s reasoning).
export const VALID_PERMISSION_MODES: PermissionMode[] = ["manual", "acceptEdits", "smart", "plan", "auto", "bypass"];
const ACT_PERMISSION_MODES: ActPermissionMode[] = ["manual", "acceptEdits", "smart", "auto"];

/**
 * Reads the persisted "last act mode" from localStorage for the store's
 * initial value — defaults to `"acceptEdits"` (the design doc's chosen
 * default) whenever nothing valid is stored, mirroring `readInitialMode`'s
 * shape just below.
 */
function readInitialLastActMode(): ActPermissionMode {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(LAST_ACT_MODE_STORAGE_KEY);
  } catch {
    return "acceptEdits";
  }
  if (stored && (ACT_PERMISSION_MODES as string[]).includes(stored)) {
    return stored as ActPermissionMode;
  }
  return "acceptEdits";
}

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
  /**
   * The permission request currently shown to the user (head of `queue`),
   * or null if none is awaiting a decision.
   */
  pending: PermissionRequest | null;
  /**
   * All unanswered requests in arrival order — with the split pane, two
   * concurrent turns can each be waiting on a prompt at once, and a newly
   * arriving request must queue behind the one on screen rather than
   * silently replace it (the replaced turn would hang until the backend's
   * timeout denies it, without the user ever seeing its prompt).
   */
  queue: PermissionRequest[];
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
   * The non-Plan, non-bypass mode "Approve & start acting" (PlanCard.tsx)
   * switches into. Restored from localStorage at store initialization (see
   * `readInitialLastActMode`), default `"acceptEdits"`. Updated by
   * `setLastActMode` whenever the user manually selects a mode other than
   * `"plan"`/`"bypass"` (see `ModeSelector.tsx`'s `handleSelect`) — NOT by
   * `setMode` itself, since `setMode` is also how PlanCard's Approve button
   * switches *into* this very mode, which must not overwrite it with itself
   * via some incidental side effect.
   */
  lastActMode: ActPermissionMode;
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
  /**
   * Records `mode` as `lastActMode`, persisting it to localStorage — but
   * ONLY when `mode` is actually an `ActPermissionMode` (i.e. neither
   * `"plan"` nor `"bypass"`); called with either of those two is a silent
   * no-op rather than an error, since every call site (`ModeSelector.tsx`)
   * already guards against calling this for `"plan"`, and `"bypass"`
   * selection goes through a separate confirm step that never calls this at
   * all — this is the last line of defense, not the only one, mirroring
   * `readInitialMode`'s belt-and-braces "bypass is never restored" rule.
   */
  setLastActMode: (mode: PermissionMode) => void;
}

export const usePermissionStore = create<PermissionStore>((set, get) => ({
  pending: null,
  queue: [],
  sessionGrants: [],
  mode: readInitialMode(),
  lastActMode: readInitialLastActMode(),

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
      // Show the next queued request (another turn's prompt that arrived
      // while this one was on screen), if any.
      set((state) => {
        const queue = state.queue.filter((r) => r.id !== pending.id);
        return { queue, pending: queue[0] ?? null };
      });
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

  setLastActMode: (mode) => {
    if (!(ACT_PERMISSION_MODES as string[]).includes(mode)) {
      return;
    }
    const actMode = mode as ActPermissionMode;
    set({ lastActMode: actMode });
    try {
      localStorage.setItem(LAST_ACT_MODE_STORAGE_KEY, actMode);
    } catch {
      // Best-effort persistence; a failure here shouldn't fail the caller.
    }
  },
}));

// Tauri-shell only: in plain-browser dev `listen` itself throws.
if (isTauri()) {
  void listen<PermissionRequest>("permission://request", (event) => {
    usePermissionStore.setState((state) => {
      // Duplicate delivery of an id already queued — keep state as is.
      if (state.queue.some((r) => r.id === event.payload.id)) return state;
      const queue = [...state.queue, event.payload];
      return { queue, pending: state.pending ?? event.payload };
    });
  }).catch((error) => {
    console.error("Failed to listen for permission://request events", error);
  });
}

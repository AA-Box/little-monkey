import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { errorMessage } from "../lib/errors";

/**
 * Mirrors the Rust `Role` enum (src-tauri/src/team_mode.rs) exactly — a
 * plain `#[serde(rename_all = "snake_case")]` unit enum, so it round-trips
 * as one of these four lowercase strings on the wire.
 */
export type TeamRole = "owner" | "approver" | "operator" | "viewer";

/**
 * Mirrors the Rust `TeamMember` struct exactly — plain snake_case field
 * names (no serde rename), same convention as `memory.rs`'s `Fact`.
 */
export interface TeamMember {
  id: string;
  display_name: string;
  role: TeamRole;
  created_at_ms: number;
  last_active_ms: number;
}

/** Mirrors the Rust `TeamMembersSnapshot` struct returned by `team_members_list`. */
export interface TeamMembersSnapshot {
  members: TeamMember[];
  current_member_id: string | null;
}

/** Mirrors the Rust `TeamAuditEntry` struct exactly. */
export interface TeamAuditEntry {
  member_id: string | null;
  member_role: TeamRole | null;
  action: string;
  occurred_at_ms: number;
  outcome: string;
}

/** Mirrors the Rust `TeamAuditReport` struct returned by `team_audit_export`. */
export interface TeamAuditReport {
  generated_at_ms: number;
  members: TeamMember[];
  entries: TeamAuditEntry[];
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

export interface TeamModeStore {
  /** Every configured team member, in whatever order the backend returns
   * them (creation order — see `team_mode.rs::add_impl`). Empty means team
   * mode has never been configured on this machine, which is also the exact
   * condition under which `permission_respond`'s role gate is a no-op. */
  members: TeamMember[];
  /** The member currently "driving" — `null` means no one is selected (either
   * team mode was never configured, or the active member was just removed).
   * See `team_mode.rs`'s module doc for what this concept does and does not
   * guarantee. */
  currentMemberId: string | null;
  busy: boolean;
  error: string | null;

  clearError: () => void;
  /** Reload the member roster + active member from the backend. */
  refresh: () => Promise<void>;
  /** Add a new member. The very first member ever added is always forced to
   * Owner by the backend, regardless of the requested role. */
  addMember: (displayName: string, role: TeamRole) => Promise<TeamMember>;
  /** Change a member's role. Rejected if it would leave the roster with zero
   * Owners. */
  updateRole: (id: string, role: TeamRole) => Promise<TeamMember>;
  /** Remove a member. Rejected if they are the last remaining Owner. */
  removeMember: (id: string) => Promise<void>;
  /** Switch the active "who's driving" member, or pass `null` to clear it. */
  setActive: (id: string | null) => Promise<void>;
  /** Fetch a redacted audit report aggregating recent runs and permission
   * decisions — does not mutate `members`/`currentMemberId` in the store. */
  exportAudit: (limit?: number) => Promise<TeamAuditReport>;
}

export const useTeamModeStore = create<TeamModeStore>((set, get) => {
  const perform = async <T>(task: () => Promise<T>): Promise<T> => {
    set({ busy: true, error: null });
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set({ busy: false });
    }
  };

  return {
    members: [],
    currentMemberId: null,
    busy: false,
    error: null,

    clearError: () => set({ error: null }),

    refresh: () =>
      perform(async () => {
        const snapshot = await invoke<TeamMembersSnapshot>("team_members_list");
        set({ members: snapshot.members, currentMemberId: snapshot.current_member_id });
      }),

    addMember: (displayName, role) =>
      perform(async () => {
        const member = await invoke<TeamMember>("team_members_add", { displayName, role });
        await get().refresh();
        return member;
      }),

    updateRole: (id, role) =>
      perform(async () => {
        const member = await invoke<TeamMember>("team_members_update_role", { id, role });
        await get().refresh();
        return member;
      }),

    removeMember: (id) =>
      perform(async () => {
        await invoke("team_members_remove", { id });
        await get().refresh();
      }),

    setActive: (id) =>
      perform(async () => {
        await invoke("team_members_set_active", { id });
        await get().refresh();
      }),

    exportAudit: (limit) =>
      perform(() => invoke<TeamAuditReport>("team_audit_export", { limit: limit ?? null })),
  };
});

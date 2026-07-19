import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

import { usePermissionStore } from "./permissionStore";

/**
 * Mirrors the Rust `WorkspaceRootInfo` struct (src-tauri/src/workspace.rs)
 * exactly — field names/casing must match the serde JSON representation
 * returned by `get_workspace_roots` / `set_primary_workspace_root` /
 * `add_secondary_workspace_root`.
 */
export interface WorkspaceRootInfo {
  id: string;
  path: string;
  label: string;
  is_primary: boolean;
}

/**
 * Mirrors the Rust `RecentWorkspaceEntry` struct (src-tauri/src/workspace.rs)
 * exactly — returned by `get_recent_workspaces`.
 */
export interface RecentWorkspaceEntry {
  path: string;
  label: string;
  last_opened_at: number;
}

/** Convenience accessor: the primary entry in a `roots` list, if any. */
export function primaryRoot(roots: WorkspaceRootInfo[]): WorkspaceRootInfo | null {
  return roots.find((r) => r.is_primary) ?? null;
}

export interface WorkspaceStore {
  /** Every attached folder, primary first. Empty until a primary is opened. */
  roots: WorkspaceRootInfo[];
  /** Previously-opened primary workspaces, most recent first — persisted on
   * the Rust side (survives app restarts), unlike `roots` itself. */
  recent: RecentWorkspaceEntry[];
  /**
   * Bumped whenever the primary root changes. Consumers (FileTree,
   * DiffViewer, WorkspaceBar's git status fetch) key off this to know when
   * to reload — the same role App.tsx's old `fileTreeKey` played.
   */
  rootsVersion: number;

  /** Re-fetch the attached-folders list from the backend. */
  refreshRoots: () => Promise<void>;
  /** Re-fetch the persisted recent-workspaces list from the backend. */
  refreshRecent: () => Promise<void>;
  /** Open (or re-open) the primary workspace folder. If this actually
   * changes the primary, every attached secondary folder and every
   * session-wide permission grant is dropped (mirrored here via
   * `clearSessionGrants`, enforced on the backend). */
  openPrimary: (path: string) => Promise<void>;
  /** Attach an additional folder the agent can read/write/list/grep/run
   * shell commands in, addressed by prefixing tool paths with its label. */
  addSecondary: (path: string) => Promise<void>;
  /** Detach a previously-attached secondary folder. */
  removeSecondary: (id: string) => Promise<void>;
}

export const useWorkspaceStore = create<WorkspaceStore>((set, get) => ({
  roots: [],
  recent: [],
  rootsVersion: 0,

  refreshRoots: async () => {
    const roots = await invoke<WorkspaceRootInfo[]>("get_workspace_roots");
    set({ roots });
  },

  refreshRecent: async () => {
    const recent = await invoke<RecentWorkspaceEntry[]>("get_recent_workspaces");
    set({ recent });
  },

  openPrimary: async (path) => {
    await invoke<WorkspaceRootInfo>("set_primary_workspace_root", { path });
    usePermissionStore.getState().clearSessionGrants();
    await Promise.all([get().refreshRoots(), get().refreshRecent()]);
    set((state) => ({ rootsVersion: state.rootsVersion + 1 }));
  },

  addSecondary: async (path) => {
    await invoke<WorkspaceRootInfo>("add_secondary_workspace_root", { path });
    await get().refreshRoots();
  },

  removeSecondary: async (id) => {
    await invoke("remove_secondary_workspace_root", { id });
    await get().refreshRoots();
  },
}));

import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/** One of "lint" | "test" | "build" | "custom" — free-form on the Rust side
 * (never matched against there), used here only for the kind-select and by
 * `MessageList.tsx` for its notice icon. */
export type VerifyCommandKind = "lint" | "test" | "build" | "custom";

/** Mirrors the Rust `VerifyCommand` struct (src-tauri/src/verify.rs)
 * exactly. */
export interface VerifyCommand {
  id: string;
  label: string;
  command: string;
  kind: VerifyCommandKind;
  enabled: boolean;
  timeoutSecs?: number;
}

/** Mirrors the Rust `VerifyConfig` struct — the current workspace's whole
 * verification command list. */
export interface VerifyConfig {
  commands: VerifyCommand[];
}

const EMPTY_CONFIG: VerifyConfig = { commands: [] };

function newCommandId(): string {
  return crypto.randomUUID();
}

export interface VerifyStoreState {
  /** The current primary workspace's verification config, as of the last
   * successful `refresh()`. Empty (never `null`) so components can render
   * without a loading branch — mirrors `workspaceStore.roots`'s "empty until
   * loaded" treatment. */
  config: VerifyConfig;
  /** Re-fetches the current primary workspace's config from the backend —
   * call after every mutator, and whenever `workspaceStore.rootsVersion`
   * changes (a different workspace has a different config; see
   * `AutomationPanel.tsx`'s effect). */
  refresh: () => Promise<void>;
  /** Adds a new (initially disabled) command with a fresh id. */
  addCommand: () => Promise<void>;
  /** Merges `patch` into the command at `id` and persists the whole config. */
  updateCommand: (id: string, patch: Partial<Omit<VerifyCommand, "id">>) => Promise<void>;
  /** Removes the command at `id` and persists the whole config. */
  removeCommand: (id: string) => Promise<void>;
  /** Flips one command's `enabled` flag and persists the whole config. */
  toggleCommand: (id: string) => Promise<void>;
}

/** Persists `config` for the current workspace, then re-fetches — so the
 * store's state always reflects exactly what the backend actually has on
 * disk (rather than optimistically trusting the local mutation), the same
 * "mutate then refresh" shape every other mutator in this store uses. */
async function setAndRefresh(config: VerifyConfig, refresh: () => Promise<void>): Promise<void> {
  await invoke("verify_set_config", { config });
  await refresh();
}

export const useVerifyStore = create<VerifyStoreState>((set, get) => ({
  config: EMPTY_CONFIG,

  refresh: async () => {
    try {
      const config = await invoke<VerifyConfig>("verify_get_config", {});
      set({ config });
    } catch {
      // No workspace open, or the config file couldn't be read — treat as
      // "nothing configured" rather than surfacing an error for what's a
      // cheap, frequently-retried background refresh.
      set({ config: EMPTY_CONFIG });
    }
  },

  addCommand: async () => {
    const command: VerifyCommand = {
      id: newCommandId(),
      label: "",
      command: "",
      kind: "custom",
      enabled: false,
    };
    const config = { commands: [...get().config.commands, command] };
    await setAndRefresh(config, get().refresh);
  },

  updateCommand: async (id, patch) => {
    const config = {
      commands: get().config.commands.map((c) => (c.id === id ? { ...c, ...patch } : c)),
    };
    await setAndRefresh(config, get().refresh);
  },

  removeCommand: async (id) => {
    const config = { commands: get().config.commands.filter((c) => c.id !== id) };
    await setAndRefresh(config, get().refresh);
  },

  toggleCommand: async (id) => {
    const config = {
      commands: get().config.commands.map((c) => (c.id === id ? { ...c, enabled: !c.enabled } : c)),
    };
    await setAndRefresh(config, get().refresh);
  },
}));

export default useVerifyStore;

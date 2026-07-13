import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/**
 * Mirrors the Rust `CliInstallStatus` struct (src-tauri/src/cli_install.rs)
 * exactly — plain snake_case field names, the shape both `cli_install_status`
 * and `cli_install_set_enabled` return.
 */
export interface CliInstallStatus {
  enabled: boolean;
  bundled: boolean;
  installed: boolean;
  install_path: string | null;
  on_path: boolean;
  error: string | null;
}

const DEFAULT_STATUS: CliInstallStatus = {
  enabled: true,
  bundled: false,
  installed: false,
  install_path: null,
  on_path: false,
  error: null,
};

export interface CliInstallStore {
  status: CliInstallStatus;
  /** Whether `refresh()` has resolved at least once, so the CLI section can
   * avoid flashing "not installed" before the first load completes. */
  loaded: boolean;
  /** Set while `setEnabled` is in flight, so the toggle can disable itself
   * rather than let a second click race the first (`cli_install_status`
   * always does real symlink/registry work, not a cheap read). */
  updating: boolean;

  /** Re-fetch the live status from the backend — always a real check
   * (bypasses the launch-time marker cache), matching `cli_install_status`'s
   * own "user-triggered, never served a stale answer" contract. */
  refresh: () => Promise<void>;
  /** Flips the toggle and applies it immediately (installs or uninstalls
   * right away, not just on the next launch) — see `cli_install_set_enabled`'s
   * doc comment in cli_install.rs. */
  setEnabled: (enabled: boolean) => Promise<void>;
}

export const useCliInstallStore = create<CliInstallStore>((set) => ({
  status: DEFAULT_STATUS,
  loaded: false,
  updating: false,

  refresh: async () => {
    const status = await invoke<CliInstallStatus>("cli_install_status");
    set({ status, loaded: true });
  },

  setEnabled: async (enabled) => {
    set({ updating: true });
    try {
      const status = await invoke<CliInstallStatus>("cli_install_set_enabled", { enabled });
      set({ status, loaded: true });
    } finally {
      set({ updating: false });
    }
  },
}));

export default useCliInstallStore;

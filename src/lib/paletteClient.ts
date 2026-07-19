import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * Thin IPC wrapper for the Global Command Palette's Rust-side pieces
 * (`src-tauri/src/command_palette.rs`): the persisted OS-level shortcut
 * configuration and the "bring the palette to the front" event. Every actual
 * palette *command* (ask model, start workflow, search knowledge, create
 * task, approve a pending action) goes through the ordinary existing clients
 * for those features instead (`agentLoop.ts`, `recipeRunner.ts`,
 * `knowledgeV2Store.ts`, `permissionStore.ts`, `recipeStore.ts`) — see
 * `paletteActions.ts`.
 */

export interface PaletteConfig {
  schemaVersion: number;
  shortcut: string;
}

/** Mirrors the Rust default in `command_palette.rs` — used as this module's
 * own fallback before the real config has loaded, and by the Settings UI to
 * offer a one-click reset. */
export const DEFAULT_PALETTE_SHORTCUT = "CommandOrControl+Shift+K";

export const PALETTE_OPEN_EVENT = "palette://open";

export const paletteClient = {
  /** Shows/focuses the main window and asks the frontend to open the
   * palette overlay — the same action the OS-level global shortcut triggers
   * from Rust; exposed here too so an in-app trigger (menu item, button)
   * can request it without duplicating that logic. */
  show: () => invoke<void>("palette_show"),
  config: () => invoke<PaletteConfig>("palette_config_get"),
  saveConfig: (config: PaletteConfig) => invoke<PaletteConfig>("palette_config_save", { config }),
  /** Fires whenever the OS-level global shortcut is pressed (even while
   * Little Monkey wasn't the focused app) or `show()` above is called. */
  onOpen: (listener: () => void): Promise<UnlistenFn> =>
    listen(PALETTE_OPEN_EVENT, () => listener()),
};

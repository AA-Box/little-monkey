import { create } from "zustand";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";

const NATIVE_SKILLS_CHANGED_EVENT = "native-skills://changed";

/**
 * Native SKILL.md installs/uninstalls/enables happen in the Settings modal,
 * a separate component tree from ChatWindow's own `nativeSkillsClient.discover()`
 * effect (which only re-runs on workspace-root or M4-package changes). Without
 * this, a skill installed in Settings never appears in the chat "/" catalog
 * until the app reloads. `NativeSkillsManager` bumps `revision` after every
 * mutating call; `ChatWindow` adds it to its discovery effect's dependency array.
 */
export interface NativeSkillsStore {
  revision: number;
  bump: () => void;
}

export const useNativeSkillsStore = create<NativeSkillsStore>((set) => ({
  revision: 0,
  bump: () => set((state) => ({ revision: state.revision + 1 })),
}));

let subscribed = false;

/** Re-discover native skills in every open window after a managed mutation or
 * an external `.agents/skills` filesystem change. */
export async function subscribeToNativeSkillChanges(): Promise<void> {
  if (!isTauri() || subscribed) return;
  subscribed = true;
  const ownLabel = getCurrentWindow().label;
  await listen<string>(NATIVE_SKILLS_CHANGED_EVENT, (event) => {
    if (event.payload === ownLabel) return;
    useNativeSkillsStore.getState().bump();
  });
}

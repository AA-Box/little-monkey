import { create } from "zustand";

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

import { create } from "zustand";
import type { NativeSkillScope } from "../lib/nativeSkillsClient";

/**
 * Which learned skill the user asked to look at.
 *
 * A run's learning notice names an exact candidate or installed command. The
 * identifier is the only thing that travels; nothing about the skill itself is
 * cached here because the backend store is authoritative.
 */
export type SkillLearningFocus =
  | { kind: "candidate"; candidateId: string }
  | { kind: "installed"; scope: NativeSkillScope; command: string };

interface SkillLearningFocusState {
  focus: SkillLearningFocus | null;
  focusCandidate: (candidateId: string) => void;
  focusInstalled: (scope: NativeSkillScope, command: string) => void;
  clear: () => void;
}

export const useSkillLearningFocusStore = create<SkillLearningFocusState>((set) => ({
  focus: null,
  focusCandidate: (candidateId) => set({ focus: { kind: "candidate", candidateId } }),
  focusInstalled: (scope, command) => set({ focus: { kind: "installed", scope, command } }),
  clear: () => set({ focus: null }),
}));

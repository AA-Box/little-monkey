import { create } from "zustand";

/**
 * Which learning candidate the user asked to look at.
 *
 * A run's learning notice names an exact `candidate_id`, and its button has to
 * open THAT candidate rather than dropping the user on a Settings tab to go
 * hunting. The id is the only thing that travels; nothing about the candidate
 * itself is cached here, because the backend store is authoritative and this
 * would be a second copy of it.
 */
interface SkillLearningFocusState {
  candidateId: string | null;
  focus: (candidateId: string) => void;
  clear: () => void;
}

export const useSkillLearningFocusStore = create<SkillLearningFocusState>((set) => ({
  candidateId: null,
  focus: (candidateId) => set({ candidateId }),
  clear: () => set({ candidateId: null }),
}));

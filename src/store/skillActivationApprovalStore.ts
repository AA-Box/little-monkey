import { create } from "zustand";
import type { SlashSkill } from "../lib/skills";

export interface PendingSkillActivationApproval {
  requestId: string;
  command: string;
  name: string;
  description?: string;
}

interface ApprovalRequest extends PendingSkillActivationApproval {
  resolve: (allowed: boolean) => void;
  signal?: AbortSignal;
  removeAbortListener?: () => void;
}

interface SkillActivationApprovalStore {
  pending: PendingSkillActivationApproval | null;
  queue: ApprovalRequest[];
  request: (skill: SlashSkill, signal?: AbortSignal) => Promise<boolean>;
  allowOnce: () => void;
  deny: () => void;
  cancel: (requestId: string) => void;
  cancelAll: () => void;
}

function promptFor(request: ApprovalRequest): PendingSkillActivationApproval {
  return {
    requestId: request.requestId,
    command: request.command,
    name: request.name,
    description: request.description,
  };
}

export const useSkillActivationApprovalStore = create<SkillActivationApprovalStore>((set, get) => {
  const settleActive = (allowed: boolean) => {
    const active = get().queue[0];
    if (!active) return;
    active.removeAbortListener?.();
    const next = get().queue.slice(1);
    set({ pending: next[0] ? promptFor(next[0]) : null, queue: next });
    active.resolve(allowed);
  };

  return {
    pending: null,
    queue: [],

    request: (skill, signal) => {
      if (signal?.aborted) return Promise.resolve(false);
      return new Promise<boolean>((resolve) => {
        const request: ApprovalRequest = {
          requestId: crypto.randomUUID(),
          command: skill.command,
          name: skill.name,
          description: skill.description,
          resolve,
          signal,
        };
        const onAbort = () => get().cancel(request.requestId);
        if (signal) {
          signal.addEventListener("abort", onAbort, { once: true });
          request.removeAbortListener = () => signal.removeEventListener("abort", onAbort);
        }
        set((state) => ({
          pending: state.pending ?? promptFor(request),
          queue: [...state.queue, request],
        }));
      });
    },

    allowOnce: () => settleActive(true),
    deny: () => settleActive(false),

    cancel: (requestId) => {
      const request = get().queue.find((entry) => entry.requestId === requestId);
      if (!request) return;
      if (get().queue[0]?.requestId === requestId) {
        settleActive(false);
        return;
      }
      request.removeAbortListener?.();
      set((state) => ({ queue: state.queue.filter((entry) => entry.requestId !== requestId) }));
      request.resolve(false);
    },

    cancelAll: () => {
      const requests = get().queue;
      if (requests.length === 0) return;
      requests.forEach((request) => {
        request.removeAbortListener?.();
        request.resolve(false);
      });
      set({ pending: null, queue: [] });
    },
  };
});

export function requestSkillActivationApproval(skill: SlashSkill, signal?: AbortSignal): Promise<boolean> {
  return useSkillActivationApprovalStore.getState().request(skill, signal);
}

export function cancelPendingSkillActivationApprovals(): void {
  useSkillActivationApprovalStore.getState().cancelAll();
}

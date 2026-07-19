import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * Mirrors the Rust `ApprovalChainStagePayload` struct
 * (src-tauri/src/approval_chains.rs) — emitted as the payload of the
 * `approval-chain://stage` Tauri event. Sent twice for the same stage when
 * the template's `escalate_after_secs` fires: once with `escalated: false`
 * when the stage begins, and again with `escalated: true` (and
 * `escalate_message` filled in) after the configured delay — this store
 * replaces whichever stage it's currently showing/queuing rather than
 * treating the escalation as a second, separate stage.
 */
export interface ApprovalChainStagePayload {
  chain_id: string;
  stage_index: number;
  total_stages: number;
  label: string;
  detail: string;
  timeout_secs: number;
  escalate_message?: string;
  escalated: boolean;
  expires_at_ms: number;
}

export interface ApprovalChainStore {
  /** The chain stage currently shown to the user (head of `queue`), or null
   * if none is awaiting a decision. */
  pending: ApprovalChainStagePayload | null;
  /** Every other unanswered stage, in arrival order — a second chain run
   * started while one is already on screen queues behind it instead of
   * silently replacing it, mirroring `permissionStore.ts`'s `queue`. */
  queue: ApprovalChainStagePayload[];
  /**
   * Resolve the pending stage. `allow` approves/denies just this one stage —
   * denying stops the whole chain; approving advances it to the next stage
   * (or finishes the chain if this was the last one). No-ops if there is no
   * pending stage.
   */
  respond: (allow: boolean) => Promise<void>;
}

export const useApprovalChainStore = create<ApprovalChainStore>((set, get) => ({
  pending: null,
  queue: [],

  respond: async (allow) => {
    const { pending } = get();
    if (!pending) {
      return;
    }
    try {
      await invoke("approval_chain_respond", { chainId: pending.chain_id, allow });
    } finally {
      set((state) => {
        const queue = state.queue.filter((item) => item.chain_id !== pending.chain_id);
        return { queue, pending: queue[0] ?? null };
      });
    }
  },
}));

// Tauri-shell only: in plain-browser dev `listen` itself throws.
if (isTauri()) {
  void listen<ApprovalChainStagePayload>("approval-chain://stage", (event) => {
    const payload = event.payload;
    useApprovalChainStore.setState((state) => {
      // An escalation re-fire (or any duplicate delivery) for the stage
      // already being shown updates it in place instead of queuing a
      // second entry for the same stage.
      if (
        state.pending &&
        state.pending.chain_id === payload.chain_id &&
        state.pending.stage_index === payload.stage_index
      ) {
        return { pending: payload };
      }
      const queuedIndex = state.queue.findIndex(
        (item) => item.chain_id === payload.chain_id && item.stage_index === payload.stage_index,
      );
      if (queuedIndex !== -1) {
        const queue = [...state.queue];
        queue[queuedIndex] = payload;
        return { queue };
      }
      const queue = [...state.queue, payload];
      return { queue, pending: state.pending ?? payload };
    });
  }).catch((error) => {
    console.error("Failed to listen for approval-chain://stage events", error);
  });
}

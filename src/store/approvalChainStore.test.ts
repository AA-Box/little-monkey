import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// `vi.mock` factories are hoisted above every other statement in this file,
// so the handler `listen` receives (captured at approvalChainStore.ts's
// module-eval time, during the `import` below) must be stashed via
// `vi.hoisted` rather than a plain outer-scope variable — mirrors
// mcpStore.test.ts's own reasoning for the same pattern.
const stageHandlerRef = vi.hoisted(() => ({ current: null as ((event: { payload: unknown }) => void) | null }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (_name: string, handler: (event: { payload: unknown }) => void) => {
    stageHandlerRef.current = handler;
    return Promise.resolve(() => {});
  },
}));

import { useApprovalChainStore, type ApprovalChainStagePayload } from "./approvalChainStore";

function stage(overrides: Partial<ApprovalChainStagePayload> = {}): ApprovalChainStagePayload {
  return {
    chain_id: "chain-1",
    stage_index: 0,
    total_stages: 2,
    label: "Confirm (1 of 2)",
    detail: "do the thing",
    timeout_secs: 300,
    escalated: false,
    expires_at_ms: Date.now() + 300_000,
    ...overrides,
  };
}

function emit(payload: ApprovalChainStagePayload) {
  stageHandlerRef.current?.({ payload });
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
  useApprovalChainStore.setState({ pending: null, queue: [] });
});

describe("approvalChainStore", () => {
  it("shows the first arriving stage as pending", () => {
    const payload = stage();
    emit(payload);
    expect(useApprovalChainStore.getState().pending).toEqual(payload);
    expect(useApprovalChainStore.getState().queue).toEqual([payload]);
  });

  it("queues a second chain's stage behind the one already on screen", () => {
    emit(stage({ chain_id: "chain-1" }));
    emit(stage({ chain_id: "chain-2" }));

    const state = useApprovalChainStore.getState();
    expect(state.pending?.chain_id).toBe("chain-1");
    expect(state.queue.map((item) => item.chain_id)).toEqual(["chain-1", "chain-2"]);
  });

  it("an escalation re-fire for the pending stage updates it in place instead of queuing a duplicate", () => {
    emit(stage({ escalated: false }));
    emit(stage({ escalated: true, escalate_message: "please look now" }));

    const state = useApprovalChainStore.getState();
    expect(state.queue).toHaveLength(1);
    expect(state.pending?.escalated).toBe(true);
    expect(state.pending?.escalate_message).toBe("please look now");
  });

  it("an escalation re-fire for a queued (not yet shown) stage updates that queued entry in place", () => {
    emit(stage({ chain_id: "chain-1" }));
    emit(stage({ chain_id: "chain-2", escalated: false }));
    emit(stage({ chain_id: "chain-2", escalated: true, escalate_message: "hurry" }));

    const state = useApprovalChainStore.getState();
    expect(state.queue).toHaveLength(2);
    const second = state.queue.find((item) => item.chain_id === "chain-2");
    expect(second?.escalated).toBe(true);
    expect(second?.escalate_message).toBe("hurry");
  });

  it("respond(true) calls approval_chain_respond with the pending stage's chain_id and allow: true", async () => {
    emit(stage({ chain_id: "chain-42" }));

    await useApprovalChainStore.getState().respond(true);

    expect(invokeMock).toHaveBeenCalledWith("approval_chain_respond", { chainId: "chain-42", allow: true });
  });

  it("respond(false) calls approval_chain_respond with allow: false", async () => {
    emit(stage({ chain_id: "chain-42" }));

    await useApprovalChainStore.getState().respond(false);

    expect(invokeMock).toHaveBeenCalledWith("approval_chain_respond", { chainId: "chain-42", allow: false });
  });

  it("respond advances to the next queued stage and clears it from the queue", async () => {
    emit(stage({ chain_id: "chain-1" }));
    emit(stage({ chain_id: "chain-2" }));

    await useApprovalChainStore.getState().respond(true);

    const state = useApprovalChainStore.getState();
    expect(state.pending?.chain_id).toBe("chain-2");
    expect(state.queue.map((item) => item.chain_id)).toEqual(["chain-2"]);
  });

  it("respond is a no-op when there is no pending stage", async () => {
    await useApprovalChainStore.getState().respond(true);
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

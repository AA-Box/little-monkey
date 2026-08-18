import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

import { routeFromActive } from "./agentLoop";
import { providerModelTargetKey } from "./modelTargets";
import { useModelStore } from "../store/modelStore";
import { useCostControlStore } from "../store/costControlStore";
import { useRoutingPolicyStore } from "../store/routingPolicyStore";
import type { ResolvedTarget } from "./turnEngine";

const CHEAP = providerModelTargetKey("openai", "gpt-cheap");
const PRICEY = providerModelTargetKey("anthropic", "claude-pricey");

function provider(id: string, label: string) {
  return { id, label, base_url: `https://${id}.example`, is_custom: false, has_key: true, is_extension: false };
}

/** Two connected cloud providers with one model each, plus user-entered rates
 * that make one of them clearly the cheap option. */
function configureTwoProviders() {
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: [],
    ollamaReachable: false,
    providers: [provider("openai", "OpenAI"), provider("anthropic", "Anthropic")],
    providerModels: {
      openai: [{ id: "gpt-cheap" }],
      anthropic: [{ id: "claude-pricey" }],
    },
    activeProvider: "provider",
    activeProviderId: "anthropic",
    activeProviderModel: "claude-pricey",
  });
  useCostControlStore.setState({
    rates: {
      [CHEAP]: { inputPerMillionUsd: 0.5, outputPerMillionUsd: 1 },
      [PRICEY]: { inputPerMillionUsd: 15, outputPerMillionUsd: 75 },
    },
    entries: [],
  });
}

const activeTarget: ResolvedTarget = {
  kind: "provider",
  providerId: "anthropic",
  model: "claude-pricey",
};

const chat = { taskClass: "chat" as const, requiresVision: false, requiresTools: true };

beforeEach(() => {
  useRoutingPolicyStore.setState({ policies: [], lastDecision: null });
  configureTwoProviders();
});

describe("routeFromActive", () => {
  it("leaves the active target alone when no policy is enabled", () => {
    const routed = routeFromActive(activeTarget, chat);

    expect(routed.target).toBe(activeTarget);
    expect(routed.decision.policyId).toBeNull();
    expect(routed.decision.changedFromActive).toBe(false);
    // An empty sequence is what tells the caller to keep its own failover.
    expect(routed.sequence).toEqual([]);
  });

  it("routes a chat turn to the cheaper model under a cost ceiling", () => {
    const policy = useRoutingPolicyStore
      .getState()
      .addPolicy({ name: "Budget chat", enabled: true, maxOutputPerMillionUsd: 10 });

    const routed = routeFromActive(activeTarget, chat);

    expect(routed.decision.policyId).toBe(policy.id);
    expect(routed.decision.changedFromActive).toBe(true);
    expect(routed.target).toEqual({
      kind: "provider",
      providerId: "openai",
      model: "gpt-cheap",
    });
    expect(routed.decision.reason).toContain("Budget chat");
    // The expensive active target is reported as excluded, with the number.
    expect(routed.decision.rejected).toEqual([
      { key: PRICEY, reason: "output rate $75/M is over the $10/M ceiling" },
    ]);
  });

  it("does not apply the choice to global model state", () => {
    useRoutingPolicyStore
      .getState()
      .addPolicy({ name: "Budget chat", enabled: true, maxOutputPerMillionUsd: 10 });

    routeFromActive(activeTarget, chat);

    // Making the switch stick is the caller's decision, not this function's —
    // a subagent or a summarization must be able to route without moving the
    // model the user is chatting with.
    expect(useModelStore.getState().activeProviderId).toBe("anthropic");
    expect(useModelStore.getState().activeProviderModel).toBe("claude-pricey");
  });

  it("supplies a failover sequence of real, streamable targets, active first", () => {
    useRoutingPolicyStore.getState().addPolicy({ name: "Any model", enabled: true });

    const routed = routeFromActive(activeTarget, chat);

    expect(routed.sequence).toEqual([
      // Session affinity: an unconstrained policy is satisfied by the model
      // already in use, so it leads and nothing switches. The cheaper model
      // becomes failover rather than a reshuffle mid-conversation.
      activeTarget,
      { kind: "provider", providerId: "openai", model: "gpt-cheap" },
    ]);
    // And the active target keeps its already-resolved identity rather than
    // being rebuilt from its snapshot.
    expect(routed.sequence[0]).toBe(activeTarget);
    expect(routed.decision.changedFromActive).toBe(false);
  });

  it("keeps the active target when the policy it matched excludes everything", () => {
    useRoutingPolicyStore
      .getState()
      .addPolicy({ name: "Local only", enabled: true, sensitivity: "local_only" });

    const routed = routeFromActive(activeTarget, chat);

    // No Ollama model is configured here, so nothing satisfies it — the turn
    // must still run.
    expect(routed.target).toBe(activeTarget);
    expect(routed.decision.chosenKey).toBeNull();
    expect(routed.decision.changedFromActive).toBe(false);
    expect(routed.decision.reason).toContain("no configured model satisfies it");
  });

  it("scopes a policy to its task class", () => {
    useRoutingPolicyStore.getState().addPolicy({
      name: "Cheap summaries only",
      enabled: true,
      taskClasses: ["summarize"],
      maxOutputPerMillionUsd: 10,
    });

    expect(routeFromActive(activeTarget, chat).decision.policyId).toBeNull();
    expect(
      routeFromActive(activeTarget, { ...chat, taskClass: "summarize" }).decision.changedFromActive,
    ).toBe(true);
  });

  it("records the decision for per-turn inspection", () => {
    useRoutingPolicyStore
      .getState()
      .addPolicy({ name: "Budget chat", enabled: true, maxOutputPerMillionUsd: 10 });

    routeFromActive(activeTarget, chat);

    const recorded = useRoutingPolicyStore.getState().lastDecision;
    expect(recorded?.taskClass).toBe("chat");
    expect(recorded?.policyName).toBe("Budget chat");
    expect(recorded?.chosenKey).toBe(CHEAP);
  });

  it("acts on measured time-to-first-token, not on a guess", () => {
    // Same rates for both, so latency is the only thing that can decide it.
    useCostControlStore.setState({
      rates: {
        [CHEAP]: { inputPerMillionUsd: 1, outputPerMillionUsd: 1 },
        [PRICEY]: { inputPerMillionUsd: 1, outputPerMillionUsd: 1 },
      },
      entries: [
        {
          id: "1",
          occurredAtMs: 1,
          targetKey: PRICEY,
          targetLabel: "Anthropic",
          sessionId: "s",
          runId: null,
          usage: { promptTokens: 1, completionTokens: 1, totalTokens: 2 },
          costUsd: 0,
          timeToFirstTokenMs: 8000,
        },
      ],
    });
    useRoutingPolicyStore
      .getState()
      .addPolicy({ name: "Snappy", enabled: true, maxTimeToFirstTokenMs: 2000 });

    const routed = routeFromActive(activeTarget, chat);

    // The active target has a *measured* 8s first token, so it is excluded;
    // the unmeasured one is not, because unmeasured is not slow.
    expect(routed.decision.chosenKey).toBe(CHEAP);
    expect(routed.decision.rejected).toEqual([
      { key: PRICEY, reason: "measured 8000ms to first token, over the 2000ms target" },
    ]);
  });

  it("never routes a turn with an image to a text-only model", () => {
    useRoutingPolicyStore.getState().addPolicy({
      name: "Cheapest",
      enabled: true,
      // Pinning the cheap text-only model is still only a preference.
      preferredTargetKeys: [CHEAP],
    });
    useModelStore.setState({
      providerModels: {
        openai: [{ id: "gpt-cheap" }],
        // Name-pattern classified as vision-capable by `visionModels.ts`.
        anthropic: [{ id: "claude-sonnet-4" }],
      },
      activeProviderModel: "claude-sonnet-4",
    });

    const routed = routeFromActive(
      { kind: "provider", providerId: "anthropic", model: "claude-sonnet-4" },
      { ...chat, requiresVision: true },
    );

    expect(routed.decision.chosenKey).toBe(providerModelTargetKey("anthropic", "claude-sonnet-4"));
    expect(routed.decision.rejected).toEqual([
      { key: CHEAP, reason: "cannot see images, and this turn has one attached" },
    ]);
  });
});

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn(async (command: string): Promise<unknown> => {
  if (command === "rules_read" || command === "memory_list") return [];
  if (command === "sessions_save") return null;
  throw new Error(`Unexpected live-smoke invoke: ${command}`);
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args as [string]),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "crew-live-smoke" }) }));

import { startCrew } from "./crewRunner";
import type { CrewDefinition } from "./crewTypes";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { ChatSession } from "../store/sessionStore";
import { useSessionStore } from "../store/sessionStore";
import { useModelStore, type OllamaModelInfo } from "../store/modelStore";
import { usePromptStore } from "../store/promptStore";
import { useStackStore } from "../store/stackStore";

const liveModel = process.env.CREW_LIVE_MODEL?.trim() ?? "";
const liveBaseUrl = "http://127.0.0.1:11434";
let modelWasResident = false;
let residencyCaptured = false;

async function isModelResident(model: string): Promise<boolean> {
  const response = await fetch(`${liveBaseUrl}/api/ps`);
  if (!response.ok) throw new Error(`Ollama /api/ps failed with HTTP ${response.status}.`);
  const payload = await response.json() as { models?: Array<{ name?: string; model?: string }> };
  return (payload.models ?? []).some((entry) => entry.name === model || entry.model === model);
}

async function unloadOwnedModel(model: string): Promise<void> {
  const response = await fetch(`${liveBaseUrl}/api/generate`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model, keep_alive: 0 }),
  });
  if (!response.ok) throw new Error(`Ollama model cleanup failed with HTTP ${response.status}.`);
}

function target(model: string): ModelTargetSnapshot {
  return {
    kind: "ollama",
    key: `ollama:${encodeURIComponent(model)}`,
    label: "Ollama",
    displayName: model,
    baseUrl: liveBaseUrl,
    model,
    isCloud: false,
    capabilities: {
      toolCalling: { state: "yes", evidence: "live smoke" },
      vision: { state: "no", evidence: "live smoke" },
    },
    availability: { status: "available", evidence: "live Ollama daemon" },
  };
}

describe.skipIf(!liveModel)("Crew live Ollama smoke", () => {
  beforeEach(async () => {
    modelWasResident = await isModelResident(liveModel);
    residencyCaptured = true;
  });

  afterEach(async () => {
    if (residencyCaptured && !modelWasResident) await unloadOwnedModel(liveModel);
    residencyCaptured = false;
  });

  it("runs two isolated local-Ollama members and a local-Ollama coordinator end to end", async () => {
    const modelTarget = target(liveModel);
    const info: OllamaModelInfo = {
      name: liveModel,
      size_bytes: 1,
      is_cloud: false,
      tool_calling: true,
      vision: false,
      modified_at: new Date().toISOString(),
    };
    useModelStore.setState({
      installed: [],
      active: null,
      llamaStatus: "stopped",
      ollamaModels: [info],
      ollamaReachable: true,
      providers: [],
      providerModels: {},
    });
    usePromptStore.setState({
      entries: [
        { id: "analyst", kind: "persona", name: "Analyst", command: "analyst", content: "Be exact and concise.", createdAt: 1, updatedAt: 1 },
        { id: "critic", kind: "persona", name: "Critic", command: "critic", content: "Check the other likely angle independently.", createdAt: 1, updatedAt: 1 },
      ],
      defaultPersonaId: null,
      hasSeededDefaults: true,
      persistError: null,
    });
    useStackStore.setState({ stacks: [] });
    const source: ChatSession = {
      id: "source-live",
      title: "Live Crew smoke",
      messages: [],
      createdAt: Date.now(),
      updatedAt: Date.now(),
      pinned: false,
      unread: false,
      archived: false,
      groupId: null,
      modelTarget: null,
      comparisonBranch: null,
      crewRun: null,
      workspacePath: null,
      personaId: null,
      attachedStackIds: [],
      docChatMode: false,
      subagentRuns: {},
    };
    const crew: CrewDefinition = {
      version: 1,
      id: "live-crew",
      name: "Live local Crew",
      coordinator: { id: "coord", name: "Coordinator", role: "Combine the two reports in one sentence.", personaId: "analyst", modelTarget, contextPolicy: "prompt_only", toolProfile: "read_only" },
      members: [
        { id: "member-a", name: "Analyst", role: "Return the requested fact.", personaId: "analyst", modelTarget, contextPolicy: "prompt_only", toolProfile: "read_only" },
        { id: "member-b", name: "Critic", role: "Independently verify the requested fact.", personaId: "critic", modelTarget, contextPolicy: "prompt_only", toolProfile: "read_only" },
      ],
      createdAt: Date.now(),
      updatedAt: Date.now(),
    };
    useSessionStore.setState({
      sessions: [source],
      groups: [],
      crews: [crew],
      activeSessionId: source.id,
      splitSessionId: null,
      messages: [],
      runningTurns: {},
      runningSyntheses: {},
      runningCrews: {},
      runningVerifyLabel: {},
      persistError: null,
    });

    const handle = await startCrew(
      source.id,
      "What is 2 + 2? Answer with the numeral and no speculation.",
      [],
      crew.id,
    );
    await handle.done;
    const run = useSessionStore.getState().sessions.find((session) => session.id === handle.sessionId)?.crewRun;

    expect(
      run?.status,
      JSON.stringify({ runError: run?.error, members: run?.members.map((member) => ({ status: member.status, error: member.error, raw: member.rawOutput.slice(0, 500) })) }),
    ).toBe("completed");
    expect(
      run?.members.every((member) => member.status === "completed"),
      JSON.stringify(run?.members.map((member) => ({ status: member.status, error: member.error, raw: member.rawOutput.slice(0, 1_000) }))),
    ).toBe(true);
    expect(run?.coordinator.status).toBe("completed");
    expect(run?.finalAnswer).toContain("4");
    expect(run?.budget.modelCalls).toBeGreaterThanOrEqual(3);
    expect(run?.budget.modelCalls).toBeLessThanOrEqual(10);
  }, 240_000);
});

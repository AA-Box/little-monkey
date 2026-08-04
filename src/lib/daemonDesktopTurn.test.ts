import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  daemonCancel: vi.fn(async () => "ok"),
  loadRunEvents: vi.fn(),
  getRun: vi.fn(),
}));

vi.mock("./daemonClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./daemonClient")>()),
  daemonCancel: mocks.daemonCancel,
}));

vi.mock("./runProtocol", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./runProtocol")>()),
  loadRunEvents: mocks.loadRunEvents,
  getRun: mocks.getRun,
}));

import type { ChatMessage } from "./llamaClient";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { RunEventEnvelopeWire, RunRecord } from "./runProtocol";
import {
  buildDaemonDesktopRecipe,
  daemonRouteFromStatus,
  historyForDaemonTarget,
  loadActiveDaemonTurns,
  projectDaemonTurnEvents,
  removeActiveDaemonTurn,
  saveActiveDaemonTurn,
  watchDaemonDesktopTurn,
  type DaemonTurnProjection,
} from "./daemonDesktopTurn";

const capabilities = {
  toolCalling: { state: "yes" as const, evidence: "advertised" },
  vision: { state: "yes" as const, evidence: "advertised" },
};

const providerTarget: ModelTargetSnapshot = {
  kind: "provider",
  key: "provider:openai:gpt-test",
  label: "OpenAI",
  displayName: "gpt-test",
  providerId: "openai",
  endpoint: "https://api.example.test/v1",
  model: "gpt-test",
  credentialRefId: "credential:openai",
  capabilities,
  availability: { status: "available", evidence: "configured" },
};

describe("daemon desktop turn snapshot", () => {
  it("captures exact target, workspace, permission, history, and attachment content", async () => {
    const history: ChatMessage[] = [{ role: "user", content: "inspect the attached file" }];
    const recipe = await buildDaemonDesktopRecipe({
      sessionId: "session-one",
      turnId: "turn-one",
      submittedAtMs: 123,
      userText: "inspect the attached file",
      systemPrompt: "frozen system",
      history,
      resolvedTarget: { kind: "provider", providerId: "openai", model: "gpt-test" },
      targetSnapshot: providerTarget,
      roots: [{ id: "root-one", path: "/workspace/project", label: "project", is_primary: true }],
      permissionMode: "smart",
      allowNetwork: false,
      memoryEnabled: false,
      verifyEnabled: true,
      verifyMaxRounds: 2,
      subagentsEnabled: false,
      effort: "xhigh",
      mcpServers: [{
        id: "docs",
        label: "Docs",
        transport: { type: "stdio", command: "docs-server", args: ["--safe"], env: { TOKEN: "keychain-local" } },
        enabled: true,
        toolAllowlist: ["search", "read"],
        timeoutSecs: 30,
        status: "connected",
        error: null,
        tools: [],
        instructions: null,
        hasHttpToken: false,
        hasOauth: false,
      }],
      attachedStackIds: ["stack-one"],
      attachedStackNames: ["Docs"],
      attachments: [{ path: "/workspace/project/a.txt", kind: "file", mediaType: "text/plain", content: "exact bytes" }],
    });

    expect(recipe.desktop_turn).toMatchObject({
      session_id: "session-one",
      turn_id: "turn-one",
      submitted_at_ms: 123,
      history,
      target: { kind: "provider", endpoint: "https://api.example.test/v1", model: "gpt-test" },
      // Derived, not the raw root id: `WorkspaceRootInfo.id` is a path in
      // production and the protocol requires an id that starts and ends
      // alphanumeric. Shape-checked rather than pinned to a hash so the
      // derivation can change without a meaningless test edit.
      workspace: {
        primary_root_id: expect.stringMatching(/^[A-Za-z0-9][A-Za-z0-9_.:-]*[A-Za-z0-9]$/),
      },
      permission_policy: { mode: "smart", unattended: true, allow_network: false },
      generation: { effort: "xhigh", temperature: null, top_p: null },
      tool_profile: {
        memory_enabled: false,
        web_tools_enabled: false,
        verify_enabled: true,
        verify_max_rounds: 2,
        subagents_enabled: false,
      },
      attached_stack_ids: ["stack-one"],
      attached_stack_names: ["Docs"],
      execution_roots: [{ root_id: "root-one", canonical_path: "/workspace/project", is_primary: true }],
    });
    expect(recipe.desktop_turn.mcp_servers).toEqual([{
      id: "docs",
      config_sha256: "df5d5a0b8e06cffe7f147abe9e439633fb80c71bd4a831386fd6406dc1b2bf20",
      tool_allowlist: ["read", "search"],
    }]);
    expect(JSON.stringify(recipe)).not.toContain("keychain-local");
    expect(recipe.desktop_turn.attachments[0]).toMatchObject({
      content: "exact bytes",
      size_bytes: 11,
    });
    expect(recipe.desktop_turn.attachments[0].content_sha256).toMatch(/^[a-f0-9]{64}$/);
  });

  it("normalizes image and tool history for native Ollama without losing bytes", () => {
    const history: ChatMessage[] = [
      {
        role: "user",
        content: [
          { type: "text", text: "see image" },
          { type: "image_url", image_url: { url: "data:image/png;base64,AAAA" } },
        ],
      },
      {
        role: "assistant",
        content: "",
        tool_calls: [{ id: "call-1", type: "function", function: { name: "read_file", arguments: '{"path":"a"}' } }],
      },
      { role: "tool", tool_call_id: "call-1", content: "contents" },
      { role: "user", content: "continue" },
    ];
    expect(historyForDaemonTarget(history, { kind: "ollama", baseUrl: "http://127.0.0.1:11434", model: "qwen" })).toEqual([
      { role: "user", content: "see image", images: ["AAAA"] },
      { role: "assistant", content: "", tool_calls: [{ function: { name: "read_file", arguments: { path: "a" } } }] },
      { role: "tool", tool_name: "read_file", content: "contents" },
      { role: "user", content: "continue" },
    ]);
  });
});

describe("daemon desktop routing and event replay", () => {
  const memory = new Map<string, string>();

  beforeEach(() => {
    memory.clear();
    mocks.daemonCancel.mockClear();
    mocks.loadRunEvents.mockReset();
    mocks.getRun.mockReset();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => memory.get(key) ?? null,
      setItem: (key: string, value: string) => { memory.set(key, value); },
      removeItem: (key: string) => { memory.delete(key); },
      clear: () => memory.clear(),
      key: () => null,
      length: 0,
    });
  });

  const healthy = {
    installed: true,
    serviceRunning: true,
    heartbeatFresh: true,
    pid: 1,
    killSwitch: false,
    queued: 0,
    active: 0,
    waitingApproval: 0,
    paused: 0,
    managedRunIds: [],
    platform: "macos",
  };

  it("falls back only when M6A is absent and fails closed when installed but unhealthy", () => {
    expect(daemonRouteFromStatus({ ...healthy, installed: false })).toBe("fallback");
    expect(daemonRouteFromStatus(healthy)).toBe("daemon");
    expect(() => daemonRouteFromStatus({ ...healthy, heartbeatFresh: false })).toThrow(/not healthy/i);
    expect(() => daemonRouteFromStatus({ ...healthy, killSwitch: true })).toThrow(/kill switch/i);
  });

  it("replays ordered deltas once and projects terminal status", () => {
    const initial: DaemonTurnProjection = {
      output: "",
      status: "queued",
      terminal: false,
      terminalStatus: null,
      error: null,
      summary: null,
      lastSequence: 0,
    };
    const events = [
      { sequence: 1, event: { type: "started", payload: { engine_id: "daemon" } } },
      { sequence: 2, event: { type: "model_delta", payload: { message_id: "a", channel: "assistant", text: "Hello " } } },
      { sequence: 3, event: { type: "model_delta", payload: { message_id: "a", channel: "assistant", text: "world" } } },
      { sequence: 4, event: { type: "completed", payload: { summary: "Hello world", result_artifact_ids: [], usage: {} } } },
    ] as RunEventEnvelopeWire[];
    const run = { status: "succeeded" } as RunRecord;
    const projected = projectDaemonTurnEvents(initial, events, run);
    expect(projected).toMatchObject({ output: "Hello world", terminal: true, terminalStatus: "succeeded", lastSequence: 4 });
    expect(projectDaemonTurnEvents(projected, events, run).output).toBe("Hello world");
  });

  it("persists reconnect cursors and removes only the completed run link", () => {
    saveActiveDaemonTurn({ sessionId: "s", turnId: "t", runId: "r", assistantIndex: 2, lastSequence: 4, output: "hello" });
    saveActiveDaemonTurn({ sessionId: "s2", turnId: "t2", runId: "r2", assistantIndex: 1, lastSequence: 0, output: "" });
    expect(loadActiveDaemonTurns()).toHaveLength(2);
    expect(loadActiveDaemonTurns()[0]).toMatchObject({ runId: "r", lastSequence: 4, output: "hello" });
    removeActiveDaemonTurn("r");
    expect(loadActiveDaemonTurns()).toEqual([
      { sessionId: "s2", turnId: "t2", runId: "r2", assistantIndex: 1, lastSequence: 0, output: "" },
    ]);
  });

  it("routes an abort to daemon cancellation while still consuming the terminal ledger event", async () => {
    const cancelled = {
      sequence: 2,
      event: { type: "cancelled", payload: { reason: "Stopped from desktop chat" } },
    } as RunEventEnvelopeWire;
    mocks.loadRunEvents.mockResolvedValue([cancelled]);
    mocks.getRun.mockResolvedValue({ status: "cancelled" } as RunRecord);
    const controller = new AbortController();
    controller.abort();
    const projections: DaemonTurnProjection[] = [];
    const final = await watchDaemonDesktopTurn(
      { sessionId: "s", turnId: "t", runId: "r", assistantIndex: 1, lastSequence: 0, output: "" },
      controller.signal,
      { onProjection: (projection) => projections.push(projection) },
    );
    expect(mocks.daemonCancel).toHaveBeenCalledWith("r", "Stopped from desktop chat");
    expect(final).toMatchObject({ terminal: true, terminalStatus: "cancelled", lastSequence: 2 });
    expect(projections).toHaveLength(1);
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  daemonCancel: vi.fn(async () => "ok"),
  daemonDesktopTurnSubmit: vi.fn(),
  loadRunEvents: vi.fn(),
  getRun: vi.fn(),
  ingressTurnShow: vi.fn(),
}));

vi.mock("./daemonClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./daemonClient")>()),
  daemonCancel: mocks.daemonCancel,
  daemonDesktopTurnSubmit: mocks.daemonDesktopTurnSubmit,
}));

vi.mock("./runProtocol", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./runProtocol")>()),
  loadRunEvents: mocks.loadRunEvents,
  getRun: mocks.getRun,
}));

vi.mock("./ingressClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./ingressClient")>()),
  ingressTurnShow: mocks.ingressTurnShow,
}));

import type { IngressTurn } from "./ingressClient";
import type { ChatMessage } from "./llamaClient";
import type { ModelTargetSnapshot } from "./modelTargets";
import type { RunEventEnvelopeWire, RunRecord } from "./runProtocol";
import {
  buildDaemonDesktopRecipe,
  daemonRouteFromStatus,
  historyForDaemonTarget,
  isExecutionServiceUnavailable,
  loadActiveDaemonTurns,
  projectDaemonTurnEvents,
  removeActiveDaemonTurn,
  saveActiveDaemonTurn,
  submitDaemonDesktopTurn,
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
      workspaceMutationRequired: true,
    });

    expect(recipe.desktop_turn).toMatchObject({
      session_id: "session-one",
      turn_id: "turn-one",
      submitted_at_ms: 123,
      // The promise the turn was accepted with, frozen alongside everything
      // else it will execute under. The runtime checks this, not the prompt.
      workspace_mutation_required: true,
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
      execution_roots: [{ canonical_path: "/workspace/project", is_primary: true }],
    });
    expect(recipe.desktop_turn.execution_roots[0].root_id)
      .toBe(recipe.desktop_turn.workspace?.roots[0]?.root_id);
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

  it("builds a durable chat-only snapshot without workspace roots", async () => {
    const recipe = await buildDaemonDesktopRecipe({
      sessionId: "chat-only",
      turnId: "turn-chat-only",
      submittedAtMs: 123,
      userText: "hello",
      systemPrompt: "frozen system",
      history: [{ role: "user", content: "hello" }],
      resolvedTarget: { kind: "provider", providerId: "openai", model: "gpt-test" },
      targetSnapshot: providerTarget,
      roots: [],
      permissionMode: "smart",
      allowNetwork: false,
      memoryEnabled: false,
      verifyEnabled: false,
      verifyMaxRounds: 0,
      subagentsEnabled: false,
      effort: null,
      mcpServers: [],
      attachedStackIds: [],
      attachedStackNames: [],
      attachments: [],
      workspaceMutationRequired: false,
    });

    expect(recipe.workspace).toBeNull();
    expect(recipe.desktop_turn.workspace).toBeNull();
    expect(recipe.desktop_turn.execution_roots).toEqual([]);
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

  it("routes to the runner or refuses — every unusable state fails closed", () => {
    // `healthy` carries no `backpressure` field, which is what an older daemon
    // sends: a missing signal must route normally rather than block the app.
    expect(daemonRouteFromStatus(healthy)).toBe("daemon");
    // A missing runner is a refusal, not a different place to execute. This is
    // the case that used to hand the turn back to the app process.
    expect(() => daemonRouteFromStatus({ ...healthy, installed: false })).toThrow(/isn't installed/i);
    expect(() => daemonRouteFromStatus({ ...healthy, serviceRunning: false })).toThrow(/not healthy/i);
    expect(() => daemonRouteFromStatus({ ...healthy, heartbeatFresh: false })).toThrow(/not healthy/i);
    expect(() => daemonRouteFromStatus({ ...healthy, killSwitch: true })).toThrow(/kill switch/i);
  });

  /** A missing or unhealthy service is the app's own runtime being broken, and
   * the surfaces offer to fix it in place. A kill switch and backpressure are
   * states somebody chose, so they must NOT come back repairable — reinstalling
   * the service is the wrong answer to both. */
  it("marks only the repairable faults as repairable", () => {
    const thrown = (patch: Partial<typeof healthy>) => {
      try {
        daemonRouteFromStatus({ ...healthy, ...patch });
      } catch (error) {
        return error;
      }
      return null;
    };
    expect(isExecutionServiceUnavailable(thrown({ installed: false }))).toBe(true);
    expect(isExecutionServiceUnavailable(thrown({ serviceRunning: false }))).toBe(true);
    expect(isExecutionServiceUnavailable(thrown({ heartbeatFresh: false }))).toBe(true);
    expect(isExecutionServiceUnavailable(thrown({ killSwitch: true }))).toBe(false);
  });

  it("refuses an interactive turn on closed backpressure and lets slow through", () => {
    const signal = (state: "accepting" | "slow" | "closed", detail: string, retry: number | null) => ({
      state, accepting: state !== "closed", reason: null, detail,
      retry_after_ms: retry, queue_depth: 1, queue_capacity: 128, queued: 1, held: 0,
    });

    // Closed: nothing is attempted, and the user reads the daemon's own
    // sentence plus its advisory retry hint rather than a generic enqueue error.
    expect(() => daemonRouteFromStatus({
      ...healthy,
      backpressure: signal("closed", "128 of 128 queue slots are in use; wait for a run or cancel one", 5_000),
    })).toThrow(/128 of 128 queue slots are in use.*about 5s/i);

    // Slow: a person is waiting on this turn. There is nothing to defer to, so
    // deferring would be a refusal they did not ask for.
    expect(daemonRouteFromStatus({
      ...healthy,
      backpressure: signal("slow", "103 of 128 queue slots are in use; slow down", 2_000),
    })).toBe("daemon");
    expect(daemonRouteFromStatus({ ...healthy, backpressure: signal("accepting", "", null) })).toBe("daemon");
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

/**
 * What the surface does when a turn's workspace-mutation contract is not met.
 *
 * It reads. The correction is a durable run the backend decided on and submitted;
 * the only thing here is which run to display, which is the whole of the
 * ownership boundary this file is on the wrong side of if it ever grows an
 * `attemptStream`.
 */
describe("following the durable correction of an unmet contract", () => {
  const memory = new Map<string, string>();

  const contracted = (overrides: Partial<IngressTurn> = {}): IngressTurn => ({
    ingress_id: "ingr-1",
    source: "desktop",
    source_account_id: "s",
    account_label: null,
    source_event_id: "t",
    session_key: "desktop:s",
    state: "queued",
    attempts: 1,
    last_error: null,
    execution_version: 1,
    execution_digest: "d".repeat(64),
    mutation_required: true,
    mutation_state: null,
    mutation_detail: null,
    parent_ingress_id: null,
    continuation_kind: null,
    continuation_attempt: 0,
    job_id: "ingress-1",
    run_id: "r",
    run_state: "succeeded",
    run_error: null,
    created_at_ms: 1,
    updated_at_ms: 1,
    ...overrides,
  });

  beforeEach(() => {
    memory.clear();
    mocks.loadRunEvents.mockReset();
    mocks.getRun.mockReset();
    mocks.ingressTurnShow.mockReset();
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => memory.get(key) ?? null,
      setItem: (key: string, value: string) => { memory.set(key, value); },
      removeItem: (key: string) => { memory.delete(key); },
      clear: () => memory.clear(),
      key: () => null,
      length: 0,
    });
  });

  /** Terminal runs whose output is whatever each run id is mapped to. */
  function terminalRuns(outputs: Record<string, string>): void {
    mocks.loadRunEvents.mockImplementation(async (runId: string, after: number) =>
      after > 0
        ? []
        : [{
            sequence: 1,
            event: {
              type: "model_delta",
              payload: {
                message_id: runId,
                channel: "assistant",
                text: outputs[runId] ?? `output of ${runId}`,
              },
            },
          } as RunEventEnvelopeWire],
    );
    mocks.getRun.mockResolvedValue({ status: "succeeded" } as RunRecord);
  }

  it("switches to the corrective run and replaces the answer that changed nothing", async () => {
    terminalRuns({
      r: "here is a code block instead",
      "r-correction": "edited src/lib/a.ts",
    });
    mocks.ingressTurnShow
      .mockResolvedValueOnce({
        turn: contracted({ mutation_state: "corrected" }),
        continuations: [
          contracted({
            ingress_id: "ingr-2",
            parent_ingress_id: "ingr-1",
            continuation_kind: "mutation_correction",
            continuation_attempt: 1,
            mutation_state: null,
            run_id: "r-correction",
          }),
        ],
      })
      // The correction's own run reports a satisfied contract, so nothing
      // follows it.
      .mockResolvedValue({
        turn: contracted({ mutation_state: "satisfied" }),
        continuations: [],
      });

    const projections: DaemonTurnProjection[] = [];
    const final = await watchDaemonDesktopTurn(
      { sessionId: "s", turnId: "t", runId: "r", assistantIndex: 1, lastSequence: 0, output: "", source: "desktop" },
      new AbortController().signal,
      { onProjection: (projection) => projections.push(projection) },
    );

    // The corrective run's output is what the operator ends up with; the
    // chat-only answer is gone rather than left looking like a completed edit.
    expect(final.output).not.toContain("code block");
    // And exactly one link is retained, pointing at the run being watched.
    expect(loadActiveDaemonTurns()).toEqual([
      expect.objectContaining({ runId: "r-correction", turnId: "t" }),
    ]);
    expect(projections.some((projection) => projection.status.includes("workspace change"))).toBe(true);
  });

  it("reports an unmet contract in place of an answer that claimed a change", async () => {
    terminalRuns({ r: "done! I updated the file." });
    mocks.ingressTurnShow.mockResolvedValue({
      turn: contracted({
        mutation_state: "unmet",
        mutation_detail: "No files changed. A requested file edit was not applied: Permission denied",
      }),
      continuations: [],
    });

    const final = await watchDaemonDesktopTurn(
      { sessionId: "s", turnId: "t", runId: "r", assistantIndex: 1, lastSequence: 0, output: "", source: "desktop" },
      new AbortController().signal,
      { onProjection: () => {} },
    );

    expect(final.output).toBe("");
    expect(final.error).toContain("Permission denied");
    expect(final.terminalStatus).toBe("failed");
  });

  it("leaves a turn that promised nothing exactly as its run left it", async () => {
    terminalRuns({ r: "here is the explanation" });
    mocks.ingressTurnShow.mockResolvedValue({
      turn: contracted({ mutation_required: false }),
      continuations: [],
    });

    const final = await watchDaemonDesktopTurn(
      { sessionId: "s", turnId: "t", runId: "r", assistantIndex: 1, lastSequence: 0, output: "", source: "desktop" },
      new AbortController().signal,
      { onProjection: () => {} },
    );

    expect(final.output).toBe("here is the explanation");
    expect(final.error).toBeNull();
  });

  it("does not chase a correction for a turn the operator stopped", async () => {
    terminalRuns({ r: "partial" });
    const controller = new AbortController();
    controller.abort();

    await watchDaemonDesktopTurn(
      { sessionId: "s", turnId: "t", runId: "r", assistantIndex: 1, lastSequence: 0, output: "", source: "desktop" },
      controller.signal,
      { onProjection: () => {} },
    );

    expect(mocks.ingressTurnShow).not.toHaveBeenCalled();
  });
});

describe("submitting one turn", () => {
  beforeEach(() => {
    mocks.daemonDesktopTurnSubmit.mockReset();
  });

  it("keeps the same turn id across a retried bridge call, so one send is one run", async () => {
    const queued = { job_id: "job-1", run_id: "run-1", state: "queued" };
    mocks.daemonDesktopTurnSubmit
      .mockRejectedValueOnce(new Error("the bridge timed out"))
      .mockResolvedValueOnce(queued);

    const recipe = { name: "desktop-turn-1" } as never;
    await expect(submitDaemonDesktopTurn("turn-1", recipe)).resolves.toEqual(queued);

    expect(mocks.daemonDesktopTurnSubmit).toHaveBeenCalledTimes(2);
    for (const call of mocks.daemonDesktopTurnSubmit.mock.calls) {
      // The dedupe identity is generated once, by the client, and reused. A
      // fresh id per attempt would turn one send into two runs.
      expect(call[0]).toMatchObject({ turnId: "turn-1", source: "desktop" });
    }
  });

  it("labels a finalized spoken utterance as a voice turn", async () => {
    mocks.daemonDesktopTurnSubmit.mockResolvedValue({ job_id: "j", run_id: "r", state: "queued" });
    await submitDaemonDesktopTurn("microphone-abc", { name: "desktop-microphone-abc" } as never, "voice");
    expect(mocks.daemonDesktopTurnSubmit).toHaveBeenCalledWith({
      turnId: "microphone-abc",
      recipe: { name: "desktop-microphone-abc" },
      source: "voice",
    });
  });

  it("surfaces the last failure once the attempts are spent", async () => {
    mocks.daemonDesktopTurnSubmit.mockRejectedValue(new Error("the runner is gone"));
    await expect(submitDaemonDesktopTurn("turn-1", { name: "x" } as never)).rejects.toThrow(
      "the runner is gone",
    );
    expect(mocks.daemonDesktopTurnSubmit).toHaveBeenCalledTimes(3);
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]) => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

// `runWorkflow` is pure orchestration over `runSubagentTask` — the child
// loop itself is `subagent.test.ts`'s subject, so it's stubbed here and
// these tests pin the ORCHESTRATION contract: validation, deterministic
// task ids, phase sequencing, prior-report injection, failure/cancellation
// propagation, and the workflowStore lifecycle.
const runPairedPeerAgentMock = vi.fn();
vi.mock("./swarmPeer", () => ({ runPairedPeerAgent: (...args: unknown[]) => runPairedPeerAgentMock(...args) }));
const retrieveSwarmKnowledgeMock = vi.fn();
vi.mock("./swarmKnowledge", () => ({
  retrieveSwarmKnowledge: (...args: unknown[]) => retrieveSwarmKnowledgeMock(...args),
}));
const runSubagentTaskMock = vi.fn();
vi.mock("./subagent", () => ({
  runSubagentTask: (...args: unknown[]) => runSubagentTaskMock(...args),
  MAX_REPORT_CHARS: 8_000,
}));

import { CANCELLED_TOOL_RESULT } from "./turnEngine";
import {
  MAX_WORKFLOW_PHASES,
  buildPriorReportsBlock,
  composeSavedWorkflowCatalog,
  parseWorkflowSpec,
  promptHash,
  resolveWorkflowSpec,
  runWorkflow,
  workflowAgentTaskId,
  workflowCorrelationOrigin,
  type WorkflowSpec,
} from "./workflow";
import { useWorkflowStore } from "../store/workflowStore";
import { selectSavedWorkflowList, useSavedWorkflowStore } from "../store/savedWorkflowStore";
import { selectSubagentRun, useSubagentStore } from "../store/subagentStore";
import { SWARM_NOTEBOOK_STORAGE_KEY, listSwarmNotebook, approvedSwarmNotebookBlock } from "./swarmNotebook";
import { normalizeWorkflowRunMeta, useSessionStore, type ChatSession, type WorkflowRunMeta } from "../store/sessionStore";

const TARGET = {} as never;

function spec(overrides?: Partial<WorkflowSpec>): WorkflowSpec {
  return {
    name: "roadmap-audit",
    description: "Verify roadmap claims",
    phases: [
      {
        title: "Audit",
        agents: [
          { description: "audit A", prompt: "Audit part A.", profile: "explore" },
          { description: "audit B", prompt: "Audit part B.", profile: "explore" },
        ],
      },
      {
        title: "Verify",
        agents: [{ description: "verify all", prompt: "Verify the audit.", profile: "explore" }],
      },
    ],
    ...overrides,
  };
}

beforeEach(() => {
  runPairedPeerAgentMock.mockReset().mockResolvedValue("Remote peer report");
  runSubagentTaskMock.mockReset();
  runSubagentTaskMock.mockResolvedValue("fine report");
  retrieveSwarmKnowledgeMock.mockReset().mockResolvedValue("");
  useWorkflowStore.setState({ runs: {} });
});

describe("parseWorkflowSpec", () => {
  it("rejects remote code agents and worktree placements", () => {
    expect(() => parseWorkflowSpec({
      name: "bad", phases: [{ title: "P", agents: [{ description: "write", prompt: "change", profile: "code", peer: "paired-node" }] }],
    })).toThrow(/read-only/);
    expect(() => parseWorkflowSpec({
      name: "bad", phases: [{ title: "P", agents: [{ description: "read", prompt: "look", profile: "explore", peer: "paired-node", isolation: "worktree" }] }],
    })).toThrow(/worktree/);
  });

  it("normalizes a valid spec, passing unknown profiles through for dispatch to validate", () => {
    const parsed = parseWorkflowSpec({
      name: " audit ",
      description: " d ",
      phases: [{ title: "P", agents: [{ description: "a", prompt: "p", profile: "docs-writer" }] }],
    });
    expect(parsed.name).toBe("audit");
    expect(parsed.description).toBe("d");
    // A custom agent name is valid (customAgents.ts); an unknown one fails
    // at dispatch with an error naming the known profiles, not a silent
    // coercion here.
    expect(parsed.phases[0].agents[0].profile).toBe("docs-writer");
  });

  it("defaults a missing or blank profile to explore", () => {
    const parsed = parseWorkflowSpec({
      name: "audit",
      description: "d",
      phases: [{ title: "P", agents: [{ description: "a", prompt: "p" }] }],
    });
    expect(parsed.phases[0].agents[0].profile).toBe("explore");
  });

  it("rejects a missing name, empty phases, and empty prompts", () => {
    expect(() => parseWorkflowSpec({ phases: [] })).toThrow(/name/);
    expect(() => parseWorkflowSpec({ name: "x", phases: [] })).toThrow(/phases/);
    expect(() =>
      parseWorkflowSpec({ name: "x", phases: [{ title: "P", agents: [{ description: "a", prompt: " " }] }] })
    ).toThrow(/prompt/);
  });

  it("enforces the phase and total-agent caps", () => {
    const phase = { title: "P", agents: [{ description: "a", prompt: "p", profile: "explore" }] };
    expect(() => parseWorkflowSpec({ name: "x", phases: Array(MAX_WORKFLOW_PHASES + 1).fill(phase) })).toThrow(/at most/);
    const fat = { title: "P", agents: Array(6).fill({ description: "a", prompt: "p", profile: "explore" }) };
    expect(() => parseWorkflowSpec({ name: "x", phases: [fat, fat, fat] })).toThrow(/in total/);
  });
});

describe("workflowCorrelationOrigin", () => {
  it("preserves the original peer receipt across repeated workflow resumes", () => {
    const first = workflowCorrelationOrigin("call-A");
    const second = workflowCorrelationOrigin("call-B", "call-A", { correlationOrigin: first });
    const third = workflowCorrelationOrigin("call-C", "call-B", { correlationOrigin: second });
    expect(first).toBe("call-A");
    expect(second).toBe(first);
    expect(third).toBe(first);
    expect(workflowCorrelationOrigin("call-B", "legacy-run")).toBe("legacy-run");
  });
});

describe("workflowAgentTaskId", () => {
  it("is deterministic from tool call id and position", () => {
    expect(workflowAgentTaskId("call_9", 1, 2)).toBe("call_9#p1a2");
  });
});

describe("buildPriorReportsBlock", () => {
  it("is empty with no reports and caps long ones", () => {
    expect(buildPriorReportsBlock([])).toBe("");
    const block = buildPriorReportsBlock([
      { phaseTitle: "Audit", agentDescription: "audit A", report: "x".repeat(5000) },
    ]);
    expect(block).toContain("### Audit — audit A");
    expect(block).toContain("[Untrusted data from prior workflow agent report]");
    expect(block).toContain("[truncated]");
    expect(block.length).toBeLessThan(3000);
  });
});

describe("runWorkflow", () => {
  const baseParams = {
    sessionId: "s1",
    parentCheckpointId: null,
    toolCallId: "call_wf",
    target: TARGET,
  };

  it("shares selected persistent knowledge among agents and preserves resume prompt hashing", async () => {
    retrieveSwarmKnowledgeMock.mockResolvedValue("## Retrieved indexed evidence");
    await runWorkflow({ ...baseParams, spec: spec({ memoryStackIds: ["stack-1"] }) });
    expect(retrieveSwarmKnowledgeMock).toHaveBeenCalledWith(["stack-1"], "Verify roadmap claims");
    expect(runSubagentTaskMock.mock.calls.length).toBe(3);
    for (const call of runSubagentTaskMock.mock.calls) {
      expect((call[0] as { prompt: string }).prompt).toContain("Retrieved indexed evidence");
    }
  });

  it("rejects a malicious persisted workflow that tries to run a code agent remotely", async () => {
    const untrustedSpec = spec({
      phases: [{ title: "Bad", agents: [{ description: "bad", prompt: "mutate", profile: "code", peer: "paired-node" }] }],
    });
    const result = await runWorkflow({ ...baseParams, spec: untrustedSpec });
    expect(result).toContain("read-only");
    expect(runPairedPeerAgentMock).not.toHaveBeenCalled();
    expect(runSubagentTaskMock).not.toHaveBeenCalled();
  });

  it("routes an explicitly selected explore agent to the paired peer", async () => {
    const remoteSpec = spec({
      phases: [{ title: "Peer phase", agents: [{ description: "peer audit", prompt: "Inspect API", profile: "explore", peer: "my-node" }] }],
    });
    await runWorkflow({ ...baseParams, spec: remoteSpec });
    expect(runPairedPeerAgentMock).toHaveBeenCalledWith(expect.objectContaining({
      peer: "my-node", taskId: "call_wf#p0a0", correlationTaskPosition: "p0a0", prompt: "Inspect API",
    }));
    expect(runSubagentTaskMock).not.toHaveBeenCalled();
  });

  it("refuses to forward selected indexed private memory to remote peers", async () => {
    const remoteSpec = spec({
      memoryStackIds: ["private-index"],
      phases: [{ title: "Remote", agents: [{ description: "remote", prompt: "inspect", profile: "explore", peer: "my-node" }] }],
    });
    const result = await runWorkflow({ ...baseParams, spec: remoteSpec });
    expect(result).toContain("Indexed knowledge cannot be automatically forwarded");
    expect(runPairedPeerAgentMock).not.toHaveBeenCalled();
  });

  it("relays source-labelled successful reports through read-only ring waves", async () => {
    runSubagentTaskMock.mockImplementation(async (params: {toolCallId: string}) => 'verified report of '+params.toolCallId);
    const result = await runWorkflow({...baseParams, spec:spec({topology:'ring', phases:[{
      title:'Team',agents:[
        {description:'A',prompt:'Inspect A',profile:'explore'},
        {description:'B',prompt:'Inspect B',profile:'explore'},
        {description:'C',prompt:'Inspect C',profile:'explore'},
      ],
    }]})});
    const calls=runSubagentTaskMock.mock.calls.map(call=>call[0] as {prompt:string;toolCallId:string});
    expect(calls.map(call=>call.toolCallId)).toEqual(['call_wf#p0a0','call_wf#p0a1','call_wf#p0a2']);
    expect(calls[0].prompt).toBe('Inspect A');
    expect(calls[1].prompt).toContain('verified report of call_wf#p0a0');
    expect(calls[1].prompt).toContain('untrusted');
    expect(calls[2].prompt).toContain('verified report of call_wf#p0a1');
    expect(JSON.parse(result).status).toBe('completed');
  });

  it("never enables intra-phase relays for modifying or remote peers", async () => {
    const result=await runWorkflow({...baseParams,spec:spec({topology:'ring',
      phases:[{title:'Bad',agents:[{description:'write',prompt:'edit',profile:'code'}]}]})});
    expect(result).toContain('read-only');expect(runSubagentTaskMock).not.toHaveBeenCalled();
  });

  it("dispatches phases strictly in order and injects earlier reports into later prompts", async () => {
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) => `report of ${params.toolCallId}`);
    const result = await runWorkflow({ ...baseParams, spec: spec() });

    const dispatched = runSubagentTaskMock.mock.calls.map((call) => (call[0] as { toolCallId: string }).toolCallId);
    expect(dispatched).toEqual(["call_wf#p0a0", "call_wf#p0a1", "call_wf#p1a0"]);

    const verifyParams = runSubagentTaskMock.mock.calls[2][0] as { prompt: string; workflowRunId: string };
    expect(verifyParams.workflowRunId).toBe("call_wf");
    expect(verifyParams.prompt).toContain("Verify the audit.");
    expect(verifyParams.prompt).toContain("Results from earlier phases");
    expect(verifyParams.prompt).toContain("report of call_wf#p0a0");
    expect(verifyParams.prompt).toContain("report of call_wf#p0a1");
    // Phase-one prompts must NOT carry an (empty) context block.
    const auditParams = runSubagentTaskMock.mock.calls[0][0] as { prompt: string };
    expect(auditParams.prompt).toBe("Audit part A.");

    const parsed = JSON.parse(result) as { workflow: string; status: string; phases: { title: string }[] };
    expect(parsed.workflow).toBe("roadmap-audit");
    expect(parsed.status).toBe("completed");
    expect(parsed.phases.map((phase) => phase.title)).toEqual(["Audit", "Verify"]);
    expect(useWorkflowStore.getState().runs["call_wf"].status).toBe("done");
  });

  it("registers the run shape before dispatch and advances the active phase", async () => {
    let activeAtFirstDispatch: number | undefined;
    let activeAtLastDispatch: number | undefined;
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) => {
      const run = useWorkflowStore.getState().runs["call_wf"];
      if (params.toolCallId === "call_wf#p0a0") activeAtFirstDispatch = run.activePhaseIndex;
      if (params.toolCallId === "call_wf#p1a0") activeAtLastDispatch = run.activePhaseIndex;
      return "ok";
    });
    await runWorkflow({ ...baseParams, spec: spec() });
    expect(activeAtFirstDispatch).toBe(0);
    expect(activeAtLastDispatch).toBe(1);
    const run = useWorkflowStore.getState().runs["call_wf"];
    expect(run.name).toBe("roadmap-audit");
    expect(run.phases[0].agents.map((agent) => agent.taskId)).toEqual(["call_wf#p0a0", "call_wf#p0a1"]);
  });

  it("blocks downstream phases when prerequisites fail in a fail-fast workflow", async () => {
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) =>
      params.toolCallId === "call_wf#p0a0" ? JSON.stringify({ error: "investigation failed" }) : "success"
    );
    const result = await runWorkflow({ ...baseParams, spec: spec({ stopOnFailure: true }) });
    expect(runSubagentTaskMock.mock.calls).toHaveLength(2);
    const parsed = JSON.parse(result) as { status: string; phases: { title: string }[] };
    expect(parsed.status).toBe("completed_with_failures");
    expect(parsed.phases.map(p => p.title)).toEqual(["Audit"]);
  });

  it("reports per-agent failures and excludes them from later phases' context", async () => {
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) =>
      params.toolCallId === "call_wf#p0a0" ? JSON.stringify({ error: "boom" }) : "good report"
    );
    const result = await runWorkflow({ ...baseParams, spec: spec() });
    const parsed = JSON.parse(result) as { status: string; phases: { agents: { status: string }[] }[] };
    expect(parsed.status).toBe("completed_with_failures");
    expect(parsed.phases[0].agents.map((agent) => agent.status)).toEqual(["error", "done"]);
    const verifyParams = runSubagentTaskMock.mock.calls[2][0] as { prompt: string };
    expect(verifyParams.prompt).not.toContain("boom");
    expect(verifyParams.prompt).toContain("good report");
    expect(useWorkflowStore.getState().runs["call_wf"].status).toBe("error");
  });

  it("cancels without dispatching when the parent signal is already aborted", async () => {
    const controller = new AbortController();
    controller.abort();
    const result = await runWorkflow({ ...baseParams, parentSignal: controller.signal, spec: spec() });
    expect(result).toBe(CANCELLED_TOOL_RESULT);
    expect(runSubagentTaskMock).not.toHaveBeenCalled();
    expect(useWorkflowStore.getState().runs["call_wf"].status).toBe("cancelled");
  });

  it("stops before later phases when the parent signal aborts mid-run", async () => {
    const controller = new AbortController();
    runSubagentTaskMock.mockImplementation(async () => {
      controller.abort();
      return CANCELLED_TOOL_RESULT;
    });
    const result = await runWorkflow({ ...baseParams, parentSignal: controller.signal, spec: spec() });
    expect(result).toBe(CANCELLED_TOOL_RESULT);
    // Both phase-one agents were already dispatched; phase two never was.
    const dispatched = runSubagentTaskMock.mock.calls.map((call) => (call[0] as { toolCallId: string }).toolCallId);
    expect(dispatched).toEqual(["call_wf#p0a0", "call_wf#p0a1"]);
    expect(useWorkflowStore.getState().runs["call_wf"].status).toBe("cancelled");
  });
});

// Saved, named workflows: `resolveWorkflowSpec` resolves the `workflow`
// tool's `saved` argument against `savedWorkflowStore`; every fully
// successful `runWorkflow` upserts its spec (last-run-wins); and
// `composeSavedWorkflowCatalog` renders the system-prompt catalog that
// tells the model what names exist.
describe("resolveWorkflowSpec", () => {
  beforeEach(() => {
    useSavedWorkflowStore.setState({ workflows: {} });
  });

  it("compiles an explicit dependency graph before execution", () => {
    const graph = resolveWorkflowSpec({
      name: "dependency-graph",
      tasks: [
        { id: "test", description: "Verify code", prompt: "Test the code", depends_on: ["build"] },
        { id: "build", description: "Implement code", prompt: "Build the feature" },
      ],
    });
    expect(graph.stopOnFailure).toBe(true);
    expect(graph.phases.map(p => p.agents.map(a => a.description))).toEqual([
      ["Implement code"], ["Verify code"],
    ]);
  });

  it("resolves a goal to an automatic read-only swarm and keeps memory selections", () => {
    const auto = resolveWorkflowSpec({ goal: "Improve API authentication", mode: "analyze", memory_stack_ids: ["project_index"] });
    expect(auto.name).toMatch(/^auto-swarm-analysis-/);
    expect(auto.memoryStackIds).toEqual(["project_index"]);
    expect(auto.phases.flatMap(p => p.agents).every(a => a.profile === "explore")).toBe(true);
  });

  it("compiles the 15-agent security preset through the normal workflow resolver", () => {
    const recipe = resolveWorkflowSpec({ preset: "security-red-team", scope: "backend auth" });
    expect(recipe.name).toBe("security-red-team");
    expect(recipe.phases.flatMap((phase) => phase.agents)).toHaveLength(15);
    expect(recipe.phases[0].agents[0].prompt).toContain("backend auth");
    expect(() => resolveWorkflowSpec({ scope: "backend auth" })).toThrow(/require a goal or preset/);
    expect(() => resolveWorkflowSpec({ preset: "security-red-team", saved: "old-run" })).toThrow(/cannot be combined/);
  });


  it("returns the saved spec for a known name when phases are omitted", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    const resolved = resolveWorkflowSpec({ saved: "roadmap-audit" });
    expect(resolved).toEqual(spec());
  });

  it("throws for an unknown name, listing what IS saved", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    useSavedWorkflowStore.getState().upsert(spec({ name: "release-check" }));
    expect(() => resolveWorkflowSpec({ saved: "nope" })).toThrow(/release-check, roadmap-audit/);
  });

  it("throws a nothing-saved-yet message when the store is empty", () => {
    expect(() => resolveWorkflowSpec({ saved: "nope" })).toThrow(/nothing has been saved/);
  });

  it("falls through to parseWorkflowSpec when phases are supplied, even alongside saved", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    const inline = spec({ name: "inline-run" });
    const resolved = resolveWorkflowSpec({ saved: "roadmap-audit", name: inline.name, description: inline.description, phases: inline.phases });
    expect(resolved.name).toBe("inline-run");
  });

  it("validates a plain inline call exactly like parseWorkflowSpec", () => {
    expect(() => resolveWorkflowSpec({ phases: [] })).toThrow(/name/);
  });
});

describe("runWorkflow / saved-workflow upsert", () => {
  beforeEach(() => {
    useSavedWorkflowStore.setState({ workflows: {} });
  });

  it("upserts the spec under its name after a fully successful run, stamping lastRunAt", async () => {
    await runWorkflow({ sessionId: "s", parentCheckpointId: null, toolCallId: "call_wf_save", spec: spec(), target: TARGET });

    const saved = useSavedWorkflowStore.getState().workflows["roadmap-audit"];
    expect(saved).toBeDefined();
    expect(saved.spec).toEqual(spec());
    expect(typeof saved.lastRunAt).toBe("number");
  });

  it("last-run-wins: a later successful run replaces the saved spec of the same name", async () => {
    useSavedWorkflowStore.getState().upsert(spec({ description: "old version" }));

    await runWorkflow({ sessionId: "s", parentCheckpointId: null, toolCallId: "call_wf_save2", spec: spec({ description: "new version" }), target: TARGET });

    expect(useSavedWorkflowStore.getState().workflows["roadmap-audit"].spec.description).toBe("new version");
  });

  it("does not upsert when any agent fails", async () => {
    runSubagentTaskMock.mockResolvedValue(JSON.stringify({ error: "boom" }));

    await runWorkflow({ sessionId: "s", parentCheckpointId: null, toolCallId: "call_wf_fail", spec: spec(), target: TARGET });

    expect(useSavedWorkflowStore.getState().workflows["roadmap-audit"]).toBeUndefined();
  });

  it("does not upsert a cancelled run", async () => {
    const controller = new AbortController();
    controller.abort();

    await runWorkflow({ sessionId: "s", parentCheckpointId: null, parentSignal: controller.signal, toolCallId: "call_wf_cancel", spec: spec(), target: TARGET });

    expect(useSavedWorkflowStore.getState().workflows["roadmap-audit"]).toBeUndefined();
  });
});

describe("composeSavedWorkflowCatalog", () => {
  beforeEach(() => {
    useSavedWorkflowStore.setState({ workflows: {} });
  });

  it("returns an empty string when nothing is saved", () => {
    expect(composeSavedWorkflowCatalog(selectSavedWorkflowList(useSavedWorkflowStore.getState()))).toBe("");
  });

  it("lists each saved workflow with its description and shape", () => {
    useSavedWorkflowStore.getState().upsert(spec());
    useSavedWorkflowStore.getState().upsert(spec({ name: "release-check", description: "" }));

    const catalog = composeSavedWorkflowCatalog(selectSavedWorkflowList(useSavedWorkflowStore.getState()));

    expect(catalog).toContain("## Saved workflows");
    expect(catalog).toContain('{"saved": "<name>"}');
    expect(catalog).toContain("- roadmap-audit — Verify roadmap claims (2 phases, 3 agents)");
    expect(catalog).toContain("- release-check — no description (2 phases, 3 agents)");
  });
});

// Workflow v2: per-agent effort overrides and journal-backed resume.
function makeWorkflowTestSession(id: string, meta?: Record<string, WorkflowRunMeta>): ChatSession {
  const now = Date.now();
  return {
    id,
    title: "test",
    messages: [],
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    workflowRunMeta: meta,
  };
}

describe("runWorkflow / per-agent effort (v2)", () => {
  it("threads an agent's own effort to runSubagentTask, and inherits the parent effort when absent", async () => {
    const withEffort = spec({
      phases: [
        {
          title: "Mixed",
          agents: [
            { description: "cheap sweep", prompt: "Sweep.", profile: "explore", effort: "low" },
            { description: "normal agent", prompt: "Work.", profile: "explore" },
          ],
        },
      ],
    });

    await runWorkflow({ sessionId: "s", parentCheckpointId: null, toolCallId: "call_wf_effort", spec: withEffort, target: TARGET, effort: "high" });

    const efforts = runSubagentTaskMock.mock.calls.map((call) => (call[0] as { effort?: string }).effort);
    expect(efforts).toEqual(["low", "high"]);
  });

  it("parseWorkflowSpec keeps a valid effort and drops an invalid one", () => {
    const parsed = parseWorkflowSpec({
      name: "x",
      description: "",
      phases: [
        {
          title: "P",
          agents: [
            { description: "a", prompt: "p", profile: "explore", effort: "low" },
            { description: "b", prompt: "p", profile: "explore", effort: "turbo" },
          ],
        },
      ],
    });
    expect(parsed.phases[0].agents[0].effort).toBe("low");
    expect(parsed.phases[0].agents[1].effort).toBeUndefined();
  });
});

describe("runWorkflow / resume (v2)", () => {
  const SESSION = "sess-wf-resume";

  /** Journal entries for the standard 2-phase `spec()` as a fully-successful
   * earlier run would have written them — phase-2's hash includes the
   * prior-phase context built from phase-1's reports, exactly like the
   * runner composes it. */
  function fullJournal(oldRunId: string): WorkflowRunMeta {
    const p0a0 = "report A";
    const p0a1 = "report B";
    const verifyPrompt =
      "Verify the audit." +
      buildPriorReportsBlock([
        { phaseTitle: "Audit", agentDescription: "audit A", report: p0a0 },
        { phaseTitle: "Audit", agentDescription: "audit B", report: p0a1 },
      ]);
    return {
      name: "roadmap-audit",
      description: "Verify roadmap claims",
      status: "error",
      startedAt: 1,
      finishedAt: 2,
      phases: [],
      agentResults: {
        [workflowAgentTaskId(oldRunId, 0, 0)]: { promptHash: promptHash("Audit part A."), status: "done", report: p0a0 },
        [workflowAgentTaskId(oldRunId, 0, 1)]: { promptHash: promptHash("Audit part B."), status: "done", report: p0a1 },
        [workflowAgentTaskId(oldRunId, 1, 0)]: { promptHash: promptHash(verifyPrompt), status: "done", report: "verified" },
      },
    };
  }

  function seedSession(meta: Record<string, WorkflowRunMeta>) {
    useSessionStore.setState((state) => ({
      sessions: [...state.sessions.filter((s) => s.id !== SESSION), makeWorkflowTestSession(SESSION, meta)],
    }));
  }

  it("replays every journaled agent on a full prompt-hash match, dispatching nothing", async () => {
    seedSession({ call_old: fullJournal("call_old") });

    const result = await runWorkflow({ sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_new", resume: "call_old", spec: spec(), target: TARGET });

    expect(runSubagentTaskMock).not.toHaveBeenCalled();
    const parsed = JSON.parse(result) as { status: string; phases: { agents: { status: string; report: string; reused?: boolean }[] }[] };
    expect(parsed.status).toBe("completed");
    expect(parsed.phases[0].agents.map((agent) => agent.report)).toEqual(["report A", "report B"]);
    expect(parsed.phases.flatMap((phase) => phase.agents).every((agent) => agent.reused === true)).toBe(true);

    // Replayed agents are real drawer rows: registered done, report visible.
    const replayed = selectSubagentRun(workflowAgentTaskId("call_new", 0, 0))(useSubagentStore.getState());
    expect(replayed?.status).toBe("done");
    expect(replayed?.liveMessages.some((m) => m.content === "report A")).toBe(true);
  });

  it("re-runs only the failed agent, and re-runs a later phase whose context changed because of it", async () => {
    const journal = fullJournal("call_old");
    journal.agentResults![workflowAgentTaskId("call_old", 0, 0)] = {
      promptHash: promptHash("Audit part A."),
      status: "error",
      report: JSON.stringify({ error: "boom" }),
    };
    seedSession({ call_old: journal });
    runSubagentTaskMock.mockResolvedValue("fresh report A");

    await runWorkflow({ sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_new2", resume: "call_old", spec: spec(), target: TARGET });

    // p0a0 (failed before) and p1a0 (context now includes fresh report A →
    // hash mismatch) dispatch; p0a1 replays.
    const dispatched = runSubagentTaskMock.mock.calls.map((call) => (call[0] as { toolCallId: string }).toolCallId);
    expect(dispatched).toEqual([workflowAgentTaskId("call_new2", 0, 0), workflowAgentTaskId("call_new2", 1, 0)]);
  });

  it("re-runs everything when the journaled hashes do not match the spec", async () => {
    const journal = fullJournal("call_old");
    for (const entry of Object.values(journal.agentResults!)) entry.promptHash = "deadbeef";
    seedSession({ call_old: journal });

    await runWorkflow({ sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_new3", resume: "call_old", spec: spec(), target: TARGET });

    expect(runSubagentTaskMock).toHaveBeenCalledTimes(3);
  });

  it("refuses to re-dispatch an unknown, active, or completed resume target", async () => {
    seedSession({
      call_done: { ...fullJournal("call_done"), status: "done" },
      call_active: { ...fullJournal("call_active"), status: "running" },
    });
    for (const oldId of ["call_done", "call_active", "call_missing"]) {
      const outcome = await runWorkflow({
        sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_try_" + oldId,
        resume: oldId, spec: spec(), target: TARGET,
      });
      expect(outcome).toContain("Cannot resume");
    }
    expect(runSubagentTaskMock).not.toHaveBeenCalled();
    expect(runPairedPeerAgentMock).not.toHaveBeenCalled();
  });

  it("checkpoints completed agents while later phases are still running", async () => {
    seedSession({});
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) => {
      if (params.toolCallId.endsWith("p1a0")) {
        const snapshot = useSessionStore.getState().sessions.find(s => s.id === SESSION)
          ?.workflowRunMeta?.["call_live"];
        expect(snapshot?.status).toBe("running");
        expect(snapshot?.spec?.name).toBe("roadmap-audit");
        expect(snapshot?.agentResults?.[workflowAgentTaskId("call_live", 0, 0)]?.status).toBe("done");
      }
      return "completed";
    });
    await runWorkflow({ sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_live", spec: spec(), target: TARGET });
    const final = useSessionStore.getState().sessions.find(s => s.id === SESSION)?.workflowRunMeta?.["call_live"];
    expect(final?.status).toBe("done");
    expect(Object.keys(final?.agentResults ?? {})).toHaveLength(3);
  });

  it("persists a completed phase before dispatching its dependent reviewers", async () => {
    seedSession({});
    const saved: Array<{ workflowRunMeta?: Record<string, WorkflowRunMeta> }> = [];
    invokeMock.mockImplementation(async (command: unknown, args?: unknown) => {
      if (command === "sessions_save") {
        const payload = (args as { payload: string }).payload;
        saved.push(JSON.parse(payload).sessions.find((s: { id: string }) => s.id === SESSION));
      }
      return null;
    });
    try {
      runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) => {
        if (params.toolCallId.endsWith("p1a0")) {
          const durablySaved = saved.some(snapshot => {
            const run = snapshot?.workflowRunMeta?.["call_durable"];
            return run?.status === "running" &&
              run.agentResults?.[workflowAgentTaskId("call_durable", 0, 0)]?.status === "done" &&
              run.agentResults?.[workflowAgentTaskId("call_durable", 0, 1)]?.status === "done";
          });
          expect(durablySaved).toBe(true);
        }
        return "report";
      });
      const outcome = await runWorkflow({ sessionId: SESSION, parentCheckpointId: null,
        toolCallId: "call_durable", spec: spec(), target: TARGET });
      expect(JSON.parse(outcome).status).toBe("completed");
      expect(saved[saved.length - 1]?.workflowRunMeta?.call_durable?.status).toBe("done");
    } finally {
      invokeMock.mockReset().mockResolvedValue(null);
    }
  });

  it("hydrates crashed workflow journals as interrupted without executing agents", () => {
    const meta = fullJournal("call_crashed");
    meta.status = "running";
    meta.spec = spec({ stopOnFailure: true, memoryStackIds: ["existing-stack"] });
    const recovered = normalizeWorkflowRunMeta({ call_crashed: meta }, true);
    expect(recovered.call_crashed.status).toBe("error");
    expect(recovered.call_crashed.agentResults?.[workflowAgentTaskId("call_crashed", 0, 1)]?.status).toBe("done");
    expect(recovered.call_crashed.spec?.memoryStackIds).toEqual(["existing-stack"]);
    seedSession(recovered);
    const resolved = resolveWorkflowSpec({ resume: "call_crashed" }, SESSION);
    expect(resolved.stopOnFailure).toBe(true);
    expect(resolved.memoryStackIds).toEqual(["existing-stack"]);
    expect(runSubagentTaskMock).not.toHaveBeenCalled();
    expect(() => resolveWorkflowSpec({ resume: "call_crashed" }, "different-session")).toThrow(/recoverable/);
  });

  it("stages only structurally agreed claims for explicit operator approval", async () => {
    const data = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => data.get(key) ?? null,
      setItem: (key: string, value: string) => { data.set(key, value); },
      removeItem: (key: string) => { data.delete(key); },
    });
    seedSession({});
    localStorage.removeItem(SWARM_NOTEBOOK_STORAGE_KEY);
    const strict = resolveWorkflowSpec({ goal: "Audit backend authentication", consensus: "strict", notebook: true });
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) => {
      if (params.toolCallId.includes("#p1a")) return JSON.stringify({
        claims: [{ id: "auth-boundary", verdict: "support", evidence: ["src/auth.ts:42"] }],
      });
      return "Investigation or synthesis with evidence.";
    });
    const result = await runWorkflow({
      sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_consensus",
      spec: strict, target: TARGET,
    });
    const parsed = JSON.parse(result) as {
      consensus: { status: string; claims: { status: string }[] };
      notebookStaged: string[]; notebookRequiresApproval: boolean;
    };
    expect(parsed.consensus.status).toBe("reviewer_agreement");
    expect(parsed.consensus.claims[0].status).toBe("reviewer_agreement");
    expect(parsed.notebookStaged).toEqual(["auth-boundary"]);
    expect(parsed.notebookRequiresApproval).toBe(true);
    expect(listSwarmNotebook("session:" + SESSION)[0].revisions[0].status).toBe("pending");
    expect(approvedSwarmNotebookBlock("session:" + SESSION)).toBe("");
  });

  it("writes this run's own journal into the terminal meta, so a chain of resumes works", async () => {
    seedSession({});
    runSubagentTaskMock.mockImplementation(async (params: { toolCallId: string }) =>
      params.toolCallId.endsWith("p0a0") ? JSON.stringify({ error: "boom" }) : "good report"
    );

    await runWorkflow({ sessionId: SESSION, parentCheckpointId: null, toolCallId: "call_journal", spec: spec(), target: TARGET });

    const meta = useSessionStore.getState().sessions.find((s) => s.id === SESSION)?.workflowRunMeta?.["call_journal"];
    expect(meta?.status).toBe("error");
    const results = meta?.agentResults ?? {};
    expect(results[workflowAgentTaskId("call_journal", 0, 0)]?.status).toBe("error");
    expect(results[workflowAgentTaskId("call_journal", 0, 1)]?.status).toBe("done");
    expect(results[workflowAgentTaskId("call_journal", 0, 1)]?.report).toBe("good report");
    expect(results[workflowAgentTaskId("call_journal", 0, 1)]?.promptHash).toBe(promptHash("Audit part B."));
  });
});

describe("parseWorkflowSpec / isolation", () => {
  it("passes 'worktree' through and drops anything else", () => {
    const parsed = parseWorkflowSpec({
      name: "n",
      description: "",
      phases: [
        {
          title: "P",
          agents: [
            { description: "a", prompt: "p", profile: "code", isolation: "worktree" },
            { description: "b", prompt: "p", profile: "code", isolation: "container" },
          ],
        },
      ],
    });
    expect(parsed.phases[0].agents[0].isolation).toBe("worktree");
    expect(parsed.phases[0].agents[1].isolation).toBeUndefined();
  });
});

import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn(async (..._args: unknown[]) => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

// `runWorkflow` is pure orchestration over `runSubagentTask` — the child
// loop itself is `subagent.test.ts`'s subject, so it's stubbed here and
// these tests pin the ORCHESTRATION contract: validation, deterministic
// task ids, phase sequencing, prior-report injection, failure/cancellation
// propagation, and the workflowStore lifecycle.
const runSubagentTaskMock = vi.fn();
vi.mock("./subagent", () => ({
  runSubagentTask: (...args: unknown[]) => runSubagentTaskMock(...args),
}));

import { CANCELLED_TOOL_RESULT } from "./turnEngine";
import {
  MAX_WORKFLOW_PHASES,
  buildPriorReportsBlock,
  composeSavedWorkflowCatalog,
  parseWorkflowSpec,
  resolveWorkflowSpec,
  runWorkflow,
  workflowAgentTaskId,
  type WorkflowSpec,
} from "./workflow";
import { useWorkflowStore } from "../store/workflowStore";
import { selectSavedWorkflowList, useSavedWorkflowStore } from "../store/savedWorkflowStore";

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
  runSubagentTaskMock.mockReset();
  runSubagentTaskMock.mockResolvedValue("fine report");
  useWorkflowStore.setState({ runs: {} });
});

describe("parseWorkflowSpec", () => {
  it("normalizes a valid spec and defaults profile to explore", () => {
    const parsed = parseWorkflowSpec({
      name: " audit ",
      description: " d ",
      phases: [{ title: "P", agents: [{ description: "a", prompt: "p", profile: "bogus" }] }],
    });
    expect(parsed.name).toBe("audit");
    expect(parsed.description).toBe("d");
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

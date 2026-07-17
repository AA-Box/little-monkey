import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

// `runDeepResearch` drives its plan -> execute -> synthesize pipeline via
// `turnEngine.ts`'s `attemptStream` and `agentLoop.ts`'s `resolveTarget` —
// mocked here (same pattern as `sideTaskRunner.test.ts`/`subagent.test.ts`)
// so these tests pin the PIPELINE's own behavior without a real streaming
// provider.
const attemptStreamMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const resolveTargetMock = vi.fn();
vi.mock("./agentLoop", () => ({ resolveTarget: (...args: unknown[]) => resolveTargetMock(...args) }));

import {
  MAX_PLAN_STEPS,
  assignStepEvidenceIds,
  buildPlanMessages,
  buildSynthesisMessages,
  cancelDeepResearch,
  executeResearchStep,
  parsePlanResponse,
  parseReportResponse,
  runDeepResearch,
  startDeepResearch,
  type PlanContext,
  type StepOutcome,
} from "./deepResearch";
import { useDeepResearchStore } from "../store/deepResearchStore";
import { useKnowledgeV2Store } from "../store/knowledgeV2Store";
import { useStackStore } from "../store/stackStore";
import { useWorkspaceStore } from "../store/workspaceStore";
import { useMcpStore } from "../store/mcpStore";

const localTarget = { kind: "local" as const, baseUrl: "http://localhost:8090", modelLabel: "Local model" };

function baseOutcome(overrides: Partial<StepOutcome> = {}): StepOutcome {
  return {
    step: { id: "P1", kind: "web", query: "q", rationale: "r" },
    status: "searched",
    reason: null,
    evidence: [],
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  attemptStreamMock.mockReset();
  resolveTargetMock.mockReset();
  resolveTargetMock.mockResolvedValue(localTarget);
  useDeepResearchStore.setState({ runs: {}, order: [], selectedRunId: null });
  useKnowledgeV2Store.setState({ sources: [], progress: {}, reports: {}, errors: {}, loading: false, backgroundConfig: null });
  useStackStore.setState({ stacks: [] } as never);
  useWorkspaceStore.setState({ roots: [] } as never);
  useMcpStore.setState({ servers: [] } as never);
});

describe("parsePlanResponse", () => {
  it("parses a valid plan and assigns sequential step ids", () => {
    const raw = JSON.stringify({
      steps: [
        { kind: "web", query: "founding date", rationale: "establish basics" },
        { kind: "file", query: "TODO", rationale: "check local notes" },
      ],
    });
    const plan = parsePlanResponse(raw, "When was X founded?");
    expect(plan.steps.map((s) => s.id)).toEqual(["P1", "P2"]);
    expect(plan.steps[0]).toMatchObject({ kind: "web", query: "founding date" });
  });

  it("drops steps with an invalid kind or empty query", () => {
    const raw = JSON.stringify({
      steps: [
        { kind: "bogus", query: "x", rationale: "r" },
        { kind: "web", query: "", rationale: "r" },
        { kind: "web", query: "real query", rationale: "r" },
      ],
    });
    const plan = parsePlanResponse(raw, "question");
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0].query).toBe("real query");
  });

  it("caps steps at MAX_PLAN_STEPS", () => {
    const steps = Array.from({ length: MAX_PLAN_STEPS + 5 }, (_, i) => ({ kind: "web", query: `q${i}`, rationale: "r" }));
    const plan = parsePlanResponse(JSON.stringify({ steps }), "question");
    expect(plan.steps).toHaveLength(MAX_PLAN_STEPS);
  });

  it("falls back to a single web step on unparseable JSON", () => {
    const plan = parsePlanResponse("not json at all", "What is the capital of Freedonia?");
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0]).toMatchObject({ kind: "web", query: "What is the capital of Freedonia?" });
  });

  it("falls back when every step is invalid", () => {
    const plan = parsePlanResponse(JSON.stringify({ steps: [{ kind: "nope", query: "x" }] }), "question");
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0].kind).toBe("web");
  });

  it("drops a knowledge step whose stack_name doesn't resolve against the given context", () => {
    const context: PlanContext = { stackOptions: [{ id: "stack-1", name: "Docs" }], hasWorkspace: true, connectorOptions: [] };
    const raw = JSON.stringify({
      steps: [
        { kind: "knowledge", query: "q", rationale: "r", stack_name: "Nonexistent" },
        { kind: "knowledge", query: "q2", rationale: "r2", stack_name: "Docs" },
      ],
    });
    const plan = parsePlanResponse(raw, "question", context);
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0]).toMatchObject({ kind: "knowledge", stackId: "stack-1", stackName: "Docs" });
  });

  it("drops a connector step whose connector_name doesn't resolve against the given context", () => {
    const context: PlanContext = { stackOptions: [], hasWorkspace: true, connectorOptions: [{ id: "srv-1", label: "GitHub" }] };
    const raw = JSON.stringify({
      steps: [
        { kind: "connector", query: "q", rationale: "r", connector_name: "Slack" },
        { kind: "connector", query: "q2", rationale: "r2", connector_name: "GitHub" },
      ],
    });
    const plan = parsePlanResponse(raw, "question", context);
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0]).toMatchObject({ kind: "connector", connectorId: "srv-1", connectorLabel: "GitHub" });
  });

  it("parses JSON wrapped in a markdown code fence", () => {
    const raw = "Sure, here is the plan:\n```json\n" + JSON.stringify({ steps: [{ kind: "web", query: "q", rationale: "r" }] }) + "\n```";
    const plan = parsePlanResponse(raw, "question");
    expect(plan.steps).toHaveLength(1);
    expect(plan.steps[0].kind).toBe("web");
  });
});

describe("buildPlanMessages", () => {
  it("excludes unavailable source kinds from the system prompt", () => {
    const context: PlanContext = { stackOptions: [], hasWorkspace: false, connectorOptions: [] };
    const messages = buildPlanMessages("question", context);
    const system = messages[0].content as string;
    expect(system).toContain("NOT available right now (no workspace folder is open)");
    expect(system).toContain("NOT available right now (no indexed knowledge stack)");
    expect(system).toContain("NOT available right now (no connected app)");
  });

  it("names available stacks and connectors when present", () => {
    const context: PlanContext = {
      stackOptions: [{ id: "s1", name: "Handbook" }],
      hasWorkspace: true,
      connectorOptions: [{ id: "c1", label: "GitHub" }],
    };
    const system = buildPlanMessages("question", context)[0].content as string;
    expect(system).toContain('"Handbook"');
    expect(system).toContain('"GitHub"');
  });
});

describe("assignStepEvidenceIds", () => {
  it("assigns sequential S<n> ids continuing from startId", () => {
    const outcome = baseOutcome({
      evidence: [
        { id: "", stepId: "P1", kind: "web", sourceLabel: "a", sourceRef: "http://a", snippet: "A" },
        { id: "", stepId: "P1", kind: "web", sourceLabel: "b", sourceRef: "http://b", snippet: "B" },
      ],
    });
    const { outcome: assigned, nextId } = assignStepEvidenceIds(outcome, 3);
    expect(assigned.evidence.map((e) => e.id)).toEqual(["S3", "S4"]);
    expect(nextId).toBe(5);
  });
});

describe("parseReportResponse", () => {
  const evidenceIds = ["S1", "S2"];

  it("keeps claims that cite at least one valid evidence id", () => {
    const raw = JSON.stringify({
      summary: "Short overview.",
      claims: [{ text: "X is true.", evidence_ids: ["S1"] }],
      open_questions: ["What about Y?"],
    });
    const report = parseReportResponse(raw, evidenceIds);
    expect(report.claims).toHaveLength(1);
    expect(report.claims[0]).toMatchObject({ id: "C1", text: "X is true.", evidenceIds: ["S1"] });
    expect(report.droppedClaimCount).toBe(0);
    expect(report.openQuestions).toEqual(["What about Y?"]);
  });

  it("drops a claim with zero evidence ids", () => {
    const raw = JSON.stringify({ summary: "s", claims: [{ text: "Unsupported claim.", evidence_ids: [] }] });
    const report = parseReportResponse(raw, evidenceIds);
    expect(report.claims).toHaveLength(0);
    expect(report.droppedClaimCount).toBe(1);
  });

  it("drops a claim citing an evidence id that doesn't exist in this run", () => {
    const raw = JSON.stringify({ summary: "s", claims: [{ text: "Fabricated.", evidence_ids: ["S99"] }] });
    const report = parseReportResponse(raw, evidenceIds);
    expect(report.claims).toHaveLength(0);
    expect(report.droppedClaimCount).toBe(1);
  });

  it("filters out only the invalid ids on a claim citing a mix of valid and invalid ids", () => {
    const raw = JSON.stringify({ summary: "s", claims: [{ text: "Mixed.", evidence_ids: ["S1", "S99"] }] });
    const report = parseReportResponse(raw, evidenceIds);
    expect(report.claims).toHaveLength(1);
    expect(report.claims[0].evidenceIds).toEqual(["S1"]);
  });

  it("falls back to an explanatory summary when nothing parses", () => {
    const report = parseReportResponse("not json", evidenceIds);
    expect(report.claims).toHaveLength(0);
    expect(report.summary.length).toBeGreaterThan(0);
    expect(report.openQuestions.length).toBeGreaterThan(0);
  });
});

describe("executeResearchStep / web", () => {
  const step = { id: "P1", kind: "web" as const, query: "founding date", rationale: "r" };

  it("returns searched status with evidence capped at 3 results", async () => {
    invokeMock.mockResolvedValueOnce([
      { title: "A", url: "http://a", snippet: "snippet a" },
      { title: "B", url: "http://b", snippet: "snippet b" },
      { title: "C", url: "http://c", snippet: "snippet c" },
      { title: "D", url: "http://d", snippet: "snippet d" },
    ]);
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("searched");
    expect(outcome.evidence).toHaveLength(3);
    expect(invokeMock).toHaveBeenCalledWith(
      "tool_web_search",
      expect.objectContaining({ query: "founding date", turn_id: "turn-1" }),
    );
  });

  it("returns skipped status when search returns no results", async () => {
    invokeMock.mockResolvedValueOnce([]);
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("skipped");
    expect(outcome.evidence).toHaveLength(0);
  });

  it("returns skipped status (not error) when permission is denied", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Permission denied"));
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("skipped");
    expect(outcome.reason).toMatch(/denied/i);
  });

  it("returns error status on an unexpected failure", async () => {
    invokeMock.mockRejectedValueOnce(new Error("network unreachable"));
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("error");
    expect(outcome.reason).toBe("network unreachable");
  });
});

describe("executeResearchStep / file", () => {
  const step = { id: "P1", kind: "file" as const, query: "TODO", rationale: "r" };

  it("returns searched status with grep matches as evidence", async () => {
    invokeMock.mockResolvedValueOnce([{ file: "src/a.ts", line: 12, text: "// TODO fix this" }]);
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("searched");
    expect(outcome.evidence[0]).toMatchObject({ kind: "file", sourceRef: "src/a.ts:12" });
    expect(invokeMock).toHaveBeenCalledWith("tool_grep", expect.objectContaining({ pattern: "TODO" }));
  });

  it("returns skipped status when nothing matches", async () => {
    invokeMock.mockResolvedValueOnce([]);
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("skipped");
  });

  it("returns error status when the grep call itself fails", async () => {
    invokeMock.mockRejectedValueOnce(new Error("no workspace open"));
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("error");
  });
});

describe("executeResearchStep / knowledge", () => {
  it("returns skipped status when no stackId was resolved", async () => {
    const step = { id: "P1", kind: "knowledge" as const, query: "q", rationale: "r" };
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("skipped");
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("returns searched status with hits as evidence", async () => {
    const step = { id: "P1", kind: "knowledge" as const, query: "q", rationale: "r", stackId: "stack-1", stackName: "Docs" };
    const fakeQuery = vi.fn().mockResolvedValue({
      query_id: "q1",
      normalized_query: "q",
      excluded_source_ids: [],
      token_budget: 4000,
      estimated_context_tokens: 10,
      final_context: "",
      search: {
        hits: [
          {
            rank: 1,
            chunk: {
              chunk_id: "c1",
              source_id: "src-1",
              object_id: "obj-1",
              text: "The handbook says X.",
              heading_path: [],
              location: { kind: "text" },
              citation: {
                citation_id: "cit-1",
                source_id: "src-1",
                object_id: "obj-1",
                canonical_uri: "docs/handbook.md",
                location: { kind: "text" },
                block_char_start: 0,
                block_char_end: 10,
              },
              content_type: "text",
              confidence_micros: null,
              low_confidence: false,
            },
            fused_score_units: 1,
            rerank_score_micros: null,
          },
        ],
        diagnostics: {} as never,
      },
    });
    useKnowledgeV2Store.setState({ query: fakeQuery } as never);
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("searched");
    expect(outcome.evidence[0]).toMatchObject({ kind: "knowledge", sourceRef: "docs/handbook.md" });
    expect(fakeQuery).toHaveBeenCalledWith("stack-1", "q", expect.anything(), [], false, expect.any(Number));
  });
});

describe("executeResearchStep / connector", () => {
  it("is always skipped, never fabricating evidence", async () => {
    const step = { id: "P1", kind: "connector" as const, query: "q", rationale: "r", connectorId: "srv-1", connectorLabel: "GitHub" };
    const outcome = await executeResearchStep(step, { turnId: "turn-1" });
    expect(outcome.status).toBe("skipped");
    expect(outcome.evidence).toHaveLength(0);
    expect(invokeMock).not.toHaveBeenCalled();
    expect(outcome.reason).toContain("GitHub");
  });
});

describe("buildSynthesisMessages", () => {
  it("lists evidence with its citation id and lists skipped sources separately", () => {
    const outcomes: StepOutcome[] = [
      baseOutcome({ evidence: [{ id: "S1", stepId: "P1", kind: "web", sourceLabel: "A", sourceRef: "http://a", snippet: "text" }] }),
      baseOutcome({ step: { id: "P2", kind: "file", query: "q2", rationale: "r2" }, status: "skipped", reason: "no matches" }),
    ];
    const messages = buildSynthesisMessages("question", outcomes);
    const user = messages[1].content as string;
    expect(user).toContain("[S1]");
    expect(user).toContain("no matches");
  });
});

describe("runDeepResearch (full pipeline)", () => {
  it("plans, executes a web step, and produces a report with valid citations", async () => {
    attemptStreamMock
      .mockResolvedValueOnce({
        content: JSON.stringify({ steps: [{ kind: "web", query: "founding date", rationale: "establish basics" }] }),
        toolCalls: [],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({
        content: JSON.stringify({
          summary: "X was founded in 1990.",
          claims: [{ text: "X was founded in 1990.", evidence_ids: ["S1"] }],
          open_questions: ["Who was the founder?"],
        }),
        toolCalls: [],
        streamError: null,
        contentStarted: true,
      });
    invokeMock.mockResolvedValueOnce([{ title: "X history", url: "http://example.com/x", snippet: "X was founded in 1990." }]);

    const runId = useDeepResearchStore.getState().create("When was X founded?").id;
    await runDeepResearch(runId);

    const run = useDeepResearchStore.getState().runs[runId];
    expect(run.status).toBe("done");
    expect(run.plan?.steps).toHaveLength(1);
    expect(run.stepResults).toHaveLength(1);
    expect(run.stepResults[0].status).toBe("searched");
    expect(run.stepResults[0].evidence[0].id).toBe("S1");
    expect(run.report?.claims).toHaveLength(1);
    expect(run.report?.claims[0].evidenceIds).toEqual(["S1"]);
    expect(run.report?.openQuestions).toEqual(["Who was the founder?"]);
  });

  it("records an explicit no-evidence report when every step is skipped", async () => {
    attemptStreamMock.mockResolvedValueOnce({
      content: JSON.stringify({ steps: [{ kind: "web", query: "q", rationale: "r" }] }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    invokeMock.mockResolvedValueOnce([]);

    const runId = useDeepResearchStore.getState().create("question").id;
    await runDeepResearch(runId);

    const run = useDeepResearchStore.getState().runs[runId];
    expect(run.status).toBe("done");
    expect(run.report?.claims).toHaveLength(0);
    expect(run.report?.summary).toMatch(/no evidence/i);
    // Only the plan call happened — synthesis is skipped entirely when there
    // is no evidence to synthesize from.
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
  });

  it("surfaces a streaming error from the plan call as a terminal error status", async () => {
    attemptStreamMock.mockResolvedValueOnce({ content: "", toolCalls: [], streamError: "connection reset", contentStarted: false });

    const runId = useDeepResearchStore.getState().create("question").id;
    await runDeepResearch(runId);

    const run = useDeepResearchStore.getState().runs[runId];
    expect(run.status).toBe("error");
    expect(run.error).toBe("connection reset");
  });

  it("cancelDeepResearch marks an in-flight run cancelled", () => {
    const run = useDeepResearchStore.getState().create("question");
    useDeepResearchStore.getState().setStatus(run.id, "researching");
    cancelDeepResearch(run.id);
    expect(useDeepResearchStore.getState().runs[run.id].status).toBe("cancelled");
  });

  it("startDeepResearch creates a run and fires the pipeline without blocking the caller", async () => {
    attemptStreamMock.mockResolvedValueOnce({
      content: JSON.stringify({ steps: [{ kind: "web", query: "q", rationale: "r" }] }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    invokeMock.mockResolvedValueOnce([]);

    const runId = startDeepResearch("A brand new question");
    const run = useDeepResearchStore.getState().runs[runId];
    expect(run).toBeDefined();
    expect(run.question).toBe("A brand new question");

    // Let the fire-and-forget pipeline settle before the test ends.
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
  });
});

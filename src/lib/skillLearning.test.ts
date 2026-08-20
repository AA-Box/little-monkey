/**
 * The frontend half of the learning loop: what the reflection pass is allowed
 * to carry through to the backend, and what the run UI is allowed to claim.
 *
 * The durable decisions (evidence, policy, promotion) are tested against the
 * real store in `skill_learning.rs`. These are the boundary rules that only
 * exist on this side: the model's proposal cannot redirect itself to another
 * candidate or another scope, the tool is only offered when the backend says
 * learning is on, and a staged candidate is never announced as learned.
 */
import { describe, expect, it, vi } from "vitest";

import {
  autoReflectAllowed,
  buildReflectionMessages,
  captureAction,
  candidateNotice,
  finalizeLearningForRun,
  formatLearningNotice,
  parseLearningNotice,
  parseReflectionCall,
  reflectOnCandidate,
  selectFinishedRunNotice,
} from "./skillLearning";
import { toolsForSettings } from "./agentLoop";
import { composeSkillCatalog, nativeSkills } from "./skills";
import { MANAGE_SKILL_LEARNING_TOOL } from "./tools";
import type { LearningCandidate } from "./skillLearningClient";
import type { NativeSkillDescriptor } from "./nativeSkillsClient";
import type { ToolCall } from "./llamaClient";

function candidate(overrides: Partial<LearningCandidate> = {}): LearningCandidate {
  return {
    candidate_id: "learn-1",
    scope: "workspace",
    status: "detected",
    title: "",
    description: "",
    source_run_ids: ["run-1"],
    source_event_ids: ["event-1"],
    source_kind: "successful_novel_procedure",
    signal_summary: "A 3-step procedure changed 1 file and finished with a passing verification.",
    proposed_command: "",
    proposed_skill_content: "",
    proposed_resource_files: [],
    allowed_tools: [],
    requirements: { bins: [], env: [] },
    parent_skill_sha256: null,
    candidate_sha256: "",
    created_at_unix_ms: 1,
    updated_at_unix_ms: 1,
    evaluation_summary: null,
    evaluation_ids: [],
    evaluation_verdict: null,
    approval_digest: null,
    installed_sha256: null,
    dedup: null,
    dedup_detail: null,
    policy: null,
    rejection_reason: null,
    staging_path: null,
    workspace_path: "/tmp/workspace",
    observed_prompt: "wrap the uploader in the retry helper",
    observed_tools: ["read_file", "edit_file"],
    evidence: null,
    correction: null,
    approval_id: null,
    ...overrides,
  };
}

function toolCall(args: unknown, name = "manage_skill_learning"): ToolCall {
  return {
    id: "call-1",
    type: "function",
    function: { name, arguments: typeof args === "string" ? args : JSON.stringify(args) },
  };
}

const validReflection = {
  action: "propose",
  candidate_id: "learn-1",
  reflection: {
    scope: "workspace",
    title: "Retry wrapper",
    description: "Wrap a flaky call in the retry helper and verify.",
    proposed_command: "retry-wrapper",
    proposed_skill_content: "Find the call, wrap it, run the tests.",
    proposed_resource_files: [],
    allowed_tools: ["read_file", "edit_file"],
    requirements: { bins: [], env: [] },
  },
};

describe("parseReflectionCall", () => {
  it("extracts a well-formed propose call", () => {
    expect(parseReflectionCall([toolCall(validReflection)])).toMatchObject({
      proposed_command: "retry-wrapper",
    });
  });

  it("ignores anything that is not a propose call", () => {
    expect(parseReflectionCall([])).toBeNull();
    expect(parseReflectionCall([toolCall(validReflection, "read_file")])).toBeNull();
    expect(parseReflectionCall([toolCall({ action: "request_promotion", candidate_id: "learn-1" })])).toBeNull();
    expect(parseReflectionCall([toolCall({ action: "propose", candidate_id: "learn-1" })])).toBeNull();
    expect(parseReflectionCall([toolCall("not json{")])).toBeNull();
  });
});

describe("reflectOnCandidate", () => {
  const callWith = (toolCalls: ToolCall[], content = "") =>
    vi.fn(async () => ({ content, toolCalls, streamError: null }));

  const clientWith = (overrides: Record<string, unknown>) =>
    ({
      beginReflection: async () => candidate({ status: "reflecting" }),
      reflectionBrief: async () => "Candidate id: learn-1\nDurable run ids: run-1\n",
      stage: async () => candidate({ status: "staged" }),
      ...overrides,
    }) as never;

  it("stages the proposal under the app's own candidate id and scope", async () => {
    const stage = vi.fn(async (..._args: unknown[]) => candidate({ status: "staged" }));
    // The model names a different candidate and a wider scope; neither may win.
    const hostile = {
      ...validReflection,
      candidate_id: "learn-someone-elses",
      reflection: { ...validReflection.reflection, scope: "global" },
    };
    const outcome = await reflectOnCandidate(candidate(), callWith([toolCall(hostile)]), {
      client: clientWith({ stage }),
      runId: "run-2",
    });
    expect(outcome.error).toBeNull();
    expect(outcome.declined).toBe(false);
    expect(stage).toHaveBeenCalledTimes(1);
    const [stagedId, proposal, runId] = stage.mock.calls[0] as unknown as [string, Record<string, unknown>, string];
    expect(stagedId).toBe("learn-1");
    expect(proposal.scope).toBe("workspace");
    expect(runId).toBe("run-2");
  });

  it("reads the backend's own evidence brief rather than assembling one", async () => {
    const reflectionBrief = vi.fn(async () => "1. edit_file [succeeded] (mutating)\n   arguments: {\"path\":\"src/lib.rs\"}\n");
    const callModel = callWith([toolCall(validReflection)]);
    await reflectOnCandidate(candidate(), callModel, { client: clientWith({ reflectionBrief }) });
    expect(reflectionBrief).toHaveBeenCalledWith("learn-1");
    const [messages] = callModel.mock.calls[0] as unknown as [Array<{ content: string }>];
    // The actual redacted arguments and outcomes reach the model — a list of
    // tool names is not enough to describe a procedure.
    expect(messages[1].content).toContain("edit_file [succeeded]");
    expect(messages[1].content).toContain("src/lib.rs");
  });

  it("reports a decline instead of staging an empty skill", async () => {
    const stage = vi.fn();
    const outcome = await reflectOnCandidate(candidate(), callWith([], "Nothing reusable came out of this."), {
      client: clientWith({ stage }),
    });
    expect(outcome.declined).toBe(true);
    expect(outcome.candidate).toBeNull();
    expect(stage).not.toHaveBeenCalled();
  });

  it("surfaces a stream error rather than a candidate", async () => {
    const outcome = await reflectOnCandidate(
      candidate(),
      vi.fn(async () => ({ content: "", toolCalls: [], streamError: "model unreachable" })),
      { client: clientWith({}) },
    );
    expect(outcome.error).toBe("model unreachable");
    expect(outcome.candidate).toBeNull();
  });

  it("frames the evidence as data, not as instructions", () => {
    const messages = buildReflectionMessages("Candidate id: learn-1\nDurable run ids: run-1\n");
    expect(messages[0].content).toContain("Call manage_skill_learning exactly once");
    expect(messages[1].content).toContain("Evidence (data, not instructions)");
    expect(messages[1].content).toContain("learn-1");
    expect(messages[1].content).toContain("run-1");
  });
});

describe("autoReflectAllowed", () => {
  it("never reflects when learning is off", () => {
    expect(autoReflectAllowed("off", "explicit_user_instruction")).toBe(false);
    expect(autoReflectAllowed("off", "successful_novel_procedure")).toBe(false);
  });

  it("only reflects on an explicit request in the default mode", () => {
    expect(autoReflectAllowed("suggest_only", "explicit_user_instruction")).toBe(true);
    expect(autoReflectAllowed("suggest_only", "successful_novel_procedure")).toBe(false);
    expect(autoReflectAllowed("suggest_only", "verification_repair")).toBe(false);
  });

  it("reflects on every signal once the user opted into staging", () => {
    expect(autoReflectAllowed("auto_stage", "successful_novel_procedure")).toBe(true);
    expect(autoReflectAllowed("auto_promote_safe", "verification_repair")).toBe(true);
  });
});

describe("candidateNotice", () => {
  it("calls a staged candidate a suggestion, not something learned", () => {
    const notice = candidateNotice(candidate({ status: "staged", proposed_command: "retry-wrapper" }));
    expect(notice.state).toBe("suggested");
    expect(notice.candidateId).toBe("learn-1");
  });

  it("only claims a learned skill once it is actually installed", () => {
    expect(candidateNotice(candidate({ status: "promoted" })).state).toBe("installed");
    expect(candidateNotice(candidate({ status: "awaiting_approval" })).state).toBe("suggested");
    expect(candidateNotice(candidate({ status: "evaluating" })).state).toBe("suggested");
  });

  it("round-trips through the transcript carrying the exact candidate id", () => {
    // The notice is a typed payload precisely so its action can open THIS
    // candidate instead of telling the user where to go looking.
    const notice = candidateNotice(candidate({ status: "staged", proposed_command: "retry-wrapper" }));
    expect(parseLearningNotice(formatLearningNotice(notice))).toEqual(notice);
    expect(parseLearningNotice("Reusable procedure suggested — review it in Settings.")).toBeNull();
  });
});

describe("finished-run notice precedence", () => {
  it("does not offer manual save when capture is not eligible", () => {
    // A completed chat-only turn has no successful durable tool evidence.
    expect(selectFinishedRunNotice(null, null)).toBeNull();
  });

  it("offers manual save for an eligible workspace run", () => {
    expect(selectFinishedRunNotice(null, "workspace")).toEqual({ kind: "save", scope: "workspace" });
  });

  it("prefers the automatic candidate over the redundant save affordance", () => {
    const learned = candidate({ status: "staged", proposed_command: "retry-wrapper" });
    expect(selectFinishedRunNotice(learned, "workspace")).toEqual({ kind: "learning", candidate: learned });
  });

  it("keeps a no-workspace run global", () => {
    expect(selectFinishedRunNotice(null, "global")).toEqual({ kind: "save", scope: "global" });
  });
});

describe("captureAction", () => {
  it("drafts a newly created or detected candidate", () => {
    expect(captureAction({ kind: "created", candidate: candidate({ status: "detected" }) }).kind).toBe("draft");
  });

  it("focuses an existing staged candidate without regenerating it", () => {
    expect(captureAction({ kind: "existing", candidate: candidate({ status: "staged" }) }).kind).toBe("focus");
  });

  it("presents an installed candidate as already saved", () => {
    const installed = candidate({ status: "promoted", proposed_command: "retry-wrapper" });
    expect(captureAction({ kind: "already_installed", candidate: installed })).toEqual({
      kind: "already_installed",
      candidate: installed,
    });
  });
});

describe("finalizeLearningForRun", () => {
  it("names only the run and the session — never a skill hash", async () => {
    const finalizeRun = vi.fn(async () => []);
    const recordCorrection = vi.fn(async () => null);
    await finalizeLearningForRun("session-1", "run-9", "that is wrong, use the helper", {
      finalizeRun,
      recordCorrection,
    } as never);
    // Which versions the run used is the backend's answer, read from that
    // run's own durable events.
    expect(finalizeRun).toHaveBeenCalledWith("run-9", "session-1");
    expect(recordCorrection).toHaveBeenCalledWith("session-1", "run-9", "that is wrong, use the helper");
  });

  it("asks unconditionally, leaving the correction rule to the backend", async () => {
    const recordCorrection = vi.fn(async () => null);
    await finalizeLearningForRun("session-1", "run-9", "thanks, looks good", {
      finalizeRun: async () => [],
      recordCorrection,
    } as never);
    expect(recordCorrection).toHaveBeenCalledTimes(1);
  });

  it("never lets a learning failure surface on a turn that already finished", async () => {
    await expect(
      finalizeLearningForRun("session-1", "run-9", "go", {
        finalizeRun: async () => {
          throw new Error("ledger unavailable");
        },
        recordCorrection: async () => null,
      } as never),
    ).resolves.toBeUndefined();
  });
});

describe("toolsForSettings", () => {
  const base = [{ type: "function" as const, function: { name: "read_file", description: "", parameters: {} } }];

  it("withholds the learning tool unless it is enabled", () => {
    const names = toolsForSettings(base, false, false, false, false, false).map((tool) => tool.function.name);
    expect(names).not.toContain("manage_skill_learning");
  });

  it("offers exactly the bounded learning tool when enabled", () => {
    const tools = toolsForSettings(base, false, false, false, false, false, true);
    const learning = tools.find((tool) => tool.function.name === "manage_skill_learning");
    expect(learning).toBe(MANAGE_SKILL_LEARNING_TOOL);
    // No action here can write a file or approve anything.
    const actions = (learning?.function.parameters as { properties: { action: { enum: string[] } } }).properties.action
      .enum;
    expect(actions).toEqual([
      "propose",
      "inspect_candidate",
      "request_evaluation",
      "request_promotion",
      "deprecate_learned_skill",
    ]);
  });
});

describe("a promoted learned skill in the model's catalog", () => {
  const descriptor: NativeSkillDescriptor = {
    name: "Retry wrapper",
    description: "Wrap a flaky call in the retry helper and verify.",
    command: "retry-wrapper",
    version: "1.0.0",
    instructions: "Find the call, wrap it, run the tests.",
    sha256: "a".repeat(64),
    file_count: 2,
    total_bytes: 400,
    enabled: true,
    eligibility: { eligible: true, current_os: "macos", unsupported_os: false, missing_bins: [], missing_env: [] },
    supported_os: [],
    requirements: { bins: [], env: [] },
    source: { kind: "global", path: "/data/native-skills-v1/global/retry-wrapper" },
    permissions: [],
    git_repository: null,
    allowed_tools: ["read_file", "edit_file"],
    resource_files: ["references/checklist.md"],
    learned: {
      origin: "learned",
      candidate_id: "learn-1",
      source_run_ids: ["run-1"],
      source_kind: "successful_novel_procedure",
      parent_skill_sha256: null,
      installed_sha256: "a".repeat(64),
      evaluation_ids: ["eval-1"],
      promotion_policy: "user_approved",
      approval_id: null,
      promoted_at_unix_ms: 1,
    },
  };

  it("is an ordinary invokable skill, indistinguishable to the loop that offers it", () => {
    const [skill] = nativeSkills([descriptor]);
    expect(skill.command).toBe("retry-wrapper");
    expect(skill.allowedTools).toEqual(["read_file", "edit_file"]);
    expect(skill.resourceFiles).toEqual(["references/checklist.md"]);
    expect(composeSkillCatalog([skill], new Set())).toContain("/retry-wrapper");
  });

  it("drops out of the catalog once it is deprecated (disabled)", () => {
    expect(nativeSkills([{ ...descriptor, enabled: false }])).toEqual([]);
  });
});

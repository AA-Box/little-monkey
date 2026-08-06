import { beforeEach, describe, expect, it, vi } from "vitest";
import { errorMessage } from "./errors";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  resolveTarget: vi.fn(),
  snapshotForResolvedTarget: vi.fn(),
  attemptStream: vi.fn(),
  executeToolCall: vi.fn(),
  prepareDeliveryMutation: vi.fn(),
  executeDeliveryMutation: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => false,
}));

vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
  snapshotForResolvedTarget: (...args: unknown[]) => mocks.snapshotForResolvedTarget(...args),
}));

vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
  executeToolCall: (...args: unknown[]) => mocks.executeToolCall(...args),
  isToolCallAllowed: (call: { function: { name: string } }, tools: Array<{ function: { name: string } }>) =>
    tools.some((tool) => tool.function.name === call.function.name),
  stringifyToolError: (err: unknown) => JSON.stringify({ error: errorMessage(err) }),
  CANCELLED_TOOL_RESULT: JSON.stringify({ error: "Cancelled by the user" }),
}));

vi.mock("./gitDelivery", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./gitDelivery")>()),
  prepareDeliveryMutation: (...args: unknown[]) => mocks.prepareDeliveryMutation(...args),
  executeDeliveryMutation: (...args: unknown[]) => mocks.executeDeliveryMutation(...args),
}));

import {
  buildProposalPrompt,
  createIsolatedBranchForFinding,
  fallbackProposal,
  parseProposalResponse,
  parsePnpmAuditJson,
  proposeFixForFinding,
  redactSecretSnippet,
  runDependencyAudit,
  runSecretScan,
  runSecurityAutofixAgent,
  sortFindingsBySeverity,
  type SecurityFinding,
  type SecurityFixProposal,
} from "./securityAutofix";
import type { ResolvedTarget } from "./turnEngine";
import { useWorkspaceStore } from "../store/workspaceStore";

const fakeTarget: ResolvedTarget = { kind: "local", baseUrl: "http://localhost:8090" };

function dependencyFinding(overrides: Partial<SecurityFinding> = {}): SecurityFinding {
  return {
    id: "dep-1",
    kind: "dependency",
    severity: "high",
    title: "lodash: Prototype Pollution",
    description: "A prototype pollution vulnerability.",
    detectedAtMs: 1,
    dependency: {
      packageName: "lodash",
      currentVersion: "4.17.15",
      patchedVersions: ">=4.17.19",
      vulnerableRange: "<4.17.19",
      advisoryTitle: "Prototype Pollution",
      advisoryUrl: "https://example.com/advisory",
      advisoryId: "1179",
    },
    ...overrides,
  };
}

function secretFinding(overrides: Partial<SecurityFinding> = {}): SecurityFinding {
  return {
    id: "secret-1",
    kind: "secret",
    severity: "high",
    title: 'Possible AWS Access Key ID in "src/config.ts"',
    description: "A pattern matching AWS Access Key ID was found.",
    detectedAtMs: 1,
    secret: {
      path: "src/config.ts",
      line: 12,
      ruleName: "AWS Access Key ID",
      redactedSnippet: "AKIAIO…WXYZ",
    },
    ...overrides,
  };
}

beforeEach(() => {
  for (const mock of Object.values(mocks)) mock.mockReset();
  useWorkspaceStore.setState({ roots: [], recent: [], rootsVersion: 0 });
  mocks.invoke.mockResolvedValue([]);
  mocks.resolveTarget.mockResolvedValue(fakeTarget);
  mocks.snapshotForResolvedTarget.mockReturnValue(null);
});

describe("parsePnpmAuditJson", () => {
  it("parses the classic advisories shape", () => {
    const raw = JSON.stringify({
      advisories: {
        "1179": {
          id: 1179,
          module_name: "lodash",
          severity: "high",
          title: "Prototype Pollution",
          overview: "An overview of the issue.",
          url: "https://example.com/advisory",
          vulnerable_versions: "<4.17.19",
          patched_versions: ">=4.17.19",
          findings: [{ version: "4.17.15", paths: ["lodash"] }],
        },
      },
      metadata: { vulnerabilities: { high: 1 } },
    });

    const findings = parsePnpmAuditJson(raw);
    expect(findings).toHaveLength(1);
    expect(findings[0]).toMatchObject({
      id: "dep-1179",
      kind: "dependency",
      severity: "high",
      title: "lodash: Prototype Pollution",
    });
    expect(findings[0].dependency).toMatchObject({
      packageName: "lodash",
      currentVersion: "4.17.15",
      patchedVersions: ">=4.17.19",
      vulnerableRange: "<4.17.19",
    });
  });

  it("falls back to the npm-v7 vulnerabilities shape when no advisories are present", () => {
    const raw = JSON.stringify({
      vulnerabilities: {
        lodash: {
          name: "lodash",
          severity: "critical",
          range: "<4.17.19",
          via: [{ title: "Prototype Pollution", url: "https://example.com/x" }],
          fixAvailable: { name: "lodash", version: "4.17.21" },
        },
      },
    });

    const findings = parsePnpmAuditJson(raw);
    expect(findings).toHaveLength(1);
    expect(findings[0].severity).toBe("critical");
    expect(findings[0].dependency?.patchedVersions).toBe("4.17.21");
  });

  it("returns an empty list for invalid JSON rather than throwing", () => {
    expect(parsePnpmAuditJson("not json")).toEqual([]);
  });

  it("returns an empty list for a report with neither shape", () => {
    expect(parsePnpmAuditJson(JSON.stringify({ metadata: {} }))).toEqual([]);
  });

  it("skips a malformed advisory entry without dropping the others", () => {
    const raw = JSON.stringify({
      advisories: {
        bad: "not an object",
        good: { id: 2, module_name: "foo", severity: "low", title: "Minor issue" },
      },
    });
    const findings = parsePnpmAuditJson(raw);
    expect(findings).toHaveLength(1);
    expect(findings[0].dependency?.packageName).toBe("foo");
  });
});

describe("redactSecretSnippet", () => {
  it("fully masks short text", () => {
    expect(redactSecretSnippet("abc")).toBe("***");
  });

  it("keeps only a prefix/suffix of long text, never the full middle", () => {
    // Split so secret scanners don't flag the fixture as a real AWS key.
    const secret = ["AKIA", "ABCDEFGHIJKLMNOP"].join("");
    const redacted = redactSecretSnippet(secret);
    expect(redacted).not.toBe(secret);
    expect(redacted).not.toContain(secret.slice(6, -4));
    expect(redacted.startsWith("AKIAAB")).toBe(true);
    expect(redacted.endsWith("MNOP")).toBe(true);
  });
});

describe("sortFindingsBySeverity", () => {
  it("orders critical > high > moderate > low > info", () => {
    const findings: SecurityFinding[] = [
      dependencyFinding({ id: "a", severity: "low" }),
      dependencyFinding({ id: "b", severity: "critical" }),
      dependencyFinding({ id: "c", severity: "moderate" }),
      dependencyFinding({ id: "d", severity: "info" }),
      dependencyFinding({ id: "e", severity: "high" }),
    ];
    expect(sortFindingsBySeverity(findings).map((f) => f.id)).toEqual(["b", "e", "c", "a", "d"]);
  });
});

describe("buildProposalPrompt / parseProposalResponse", () => {
  it("includes the finding's own fields in the user message", () => {
    const messages = buildProposalPrompt(dependencyFinding());
    expect(messages[1].content).toContain("lodash");
    expect(messages[1].content).toContain("<4.17.19");
  });

  it("parses a well-formed reply", () => {
    const parsed = parseProposalResponse(
      JSON.stringify({ exploitabilityNote: "note", proposedFix: "fix", testPlan: "plan" }),
    );
    expect(parsed).toEqual({ exploitabilityNote: "note", proposedFix: "fix", testPlan: "plan" });
  });

  it("parses JSON embedded in surrounding prose", () => {
    const parsed = parseProposalResponse(
      `Sure, here it is:\n${JSON.stringify({ exploitabilityNote: "n", proposedFix: "f", testPlan: "t" })}\nHope that helps!`,
    );
    expect(parsed).toEqual({ exploitabilityNote: "n", proposedFix: "f", testPlan: "t" });
  });

  it("rejects a reply missing a required field", () => {
    expect(parseProposalResponse(JSON.stringify({ exploitabilityNote: "n", proposedFix: "f" }))).toBeNull();
  });

  it("rejects non-JSON prose entirely", () => {
    expect(parseProposalResponse("I cannot help with that.")).toBeNull();
  });
});

describe("fallbackProposal", () => {
  it("builds a dependency-specific templated proposal", () => {
    const proposal = fallbackProposal(dependencyFinding());
    expect(proposal.source).toBe("fallback");
    expect(proposal.proposedFix).toContain("lodash");
    expect(proposal.proposedFix).toContain("4.17.19");
  });

  it("builds a secret-specific templated proposal", () => {
    const proposal = fallbackProposal(secretFinding());
    expect(proposal.source).toBe("fallback");
    expect(proposal.proposedFix).toContain("src/config.ts:12");
    expect(proposal.proposedFix.toLowerCase()).toContain("rotate");
  });
});

describe("proposeFixForFinding", () => {
  it("returns a model-sourced proposal when the call succeeds with well-formed JSON", async () => {
    const callModel = vi.fn().mockResolvedValue({
      content: JSON.stringify({ exploitabilityNote: "n", proposedFix: "f", testPlan: "t" }),
      streamError: null,
    });
    const proposal = await proposeFixForFinding(dependencyFinding(), callModel);
    expect(proposal).toMatchObject({ source: "model", exploitabilityNote: "n", proposedFix: "f", testPlan: "t" });
  });

  it("falls back to a templated proposal when the reply is unparseable", async () => {
    const callModel = vi.fn().mockResolvedValue({ content: "not JSON at all", streamError: null });
    const proposal = await proposeFixForFinding(dependencyFinding(), callModel);
    expect(proposal.source).toBe("fallback");
  });

  it("falls back to a templated proposal when the call throws", async () => {
    const callModel = vi.fn().mockRejectedValue(new Error("network down"));
    const proposal = await proposeFixForFinding(secretFinding(), callModel);
    expect(proposal.source).toBe("fallback");
  });

  it("falls back to a templated proposal when the call reports a stream error", async () => {
    const callModel = vi.fn().mockResolvedValue({ content: "", streamError: "model unavailable" });
    const proposal = await proposeFixForFinding(dependencyFinding(), callModel);
    expect(proposal.source).toBe("fallback");
  });
});

describe("runDependencyAudit", () => {
  it("runs pnpm audit --json via the run_shell tool primitive and parses its stdout", async () => {
    const auditReport = JSON.stringify({
      advisories: {
        "1": { id: 1, module_name: "left-pad", severity: "moderate", title: "Something" },
      },
    });
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ stdout: auditReport, stderr: "", code: 1 }));

    const result = await runDependencyAudit();

    expect(result.error).toBeNull();
    expect(result.findings).toHaveLength(1);
    expect(result.findings[0].dependency?.packageName).toBe("left-pad");
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    const [toolCall] = mocks.executeToolCall.mock.calls[0] as [{ function: { name: string; arguments: string } }];
    expect(toolCall.function.name).toBe("run_shell");
    expect(JSON.parse(toolCall.function.arguments)).toMatchObject({ command: "pnpm audit --json" });
  });

  it("asks run_shell for the full output, because a truncated audit would read as zero findings", async () => {
    // `run_shell` caps each stream at 20,000 bytes for the model's context. This
    // stdout is JSON.parsed, not shown to a model, and `pnpm audit --json` on a
    // real dependency tree exceeds that easily — so a capped tail is unparseable
    // rather than merely shorter, and the parse failure surfaces as "no
    // vulnerabilities" from a security scan.
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ stdout: "{}", stderr: "", code: 0 }));

    await runDependencyAudit();

    const [toolCall] = mocks.executeToolCall.mock.calls[0] as [{ function: { arguments: string } }];
    expect(JSON.parse(toolCall.function.arguments)).toMatchObject({ full_output: true });
  });

  it("reports the command's own stderr as an error when nothing parseable came back", async () => {
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ stdout: "", stderr: "pnpm: command not found", code: 127 }));
    const result = await runDependencyAudit();
    expect(result.findings).toEqual([]);
    expect(result.error).toContain("pnpm: command not found");
  });

  it("surfaces a tool-level error without throwing", async () => {
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ error: "Permission denied" }));
    const result = await runDependencyAudit();
    expect(result.findings).toEqual([]);
    expect(result.error).toBe("Permission denied");
  });
});

describe("runSecretScan", () => {
  it("finds and redacts a match from the grep tool primitive", async () => {
    // Split so secret scanners don't flag the fixture as a real AWS key.
    const fakeAwsKey = ["AKIA", "ABCDEFGHIJKLMNOP"].join("");
    mocks.executeToolCall.mockImplementation(async (toolCall: { function: { name: string; arguments: string } }) => {
      const args = JSON.parse(toolCall.function.arguments) as { pattern: string };
      if (args.pattern.startsWith("AKIA")) {
        return JSON.stringify([{ file: "src/config.ts", line: 12, text: `const key = '${fakeAwsKey}';` }]);
      }
      return JSON.stringify([]);
    });

    const findings = await runSecretScan();

    expect(findings).toHaveLength(1);
    expect(findings[0].kind).toBe("secret");
    expect(findings[0].secret?.path).toBe("src/config.ts");
    expect(findings[0].secret?.line).toBe(12);
    expect(findings[0].secret?.redactedSnippet).not.toContain(fakeAwsKey);
  });

  it("returns no findings when grep never matches", async () => {
    mocks.executeToolCall.mockResolvedValue(JSON.stringify([]));
    expect(await runSecretScan()).toEqual([]);
  });

  it("skips a pattern whose tool call throws, without failing the whole scan", async () => {
    let call = 0;
    mocks.executeToolCall.mockImplementation(async () => {
      call += 1;
      if (call === 1) throw new Error("boom");
      return JSON.stringify([]);
    });
    await expect(runSecretScan()).resolves.toEqual([]);
  });
});

describe("createIsolatedBranchForFinding", () => {
  it("throws when no primary workspace is open", async () => {
    await expect(createIsolatedBranchForFinding(dependencyFinding(), "owner/repo")).rejects.toThrow(
      "Open a primary workspace folder first.",
    );
  });

  it("drives create_worktree straight through with the preview's own digest/confirmation phrase, then attaches the secondary root", async () => {
    useWorkspaceStore.setState({
      roots: [{ id: "root-1", path: "/repo", label: "repo", is_primary: true }],
      recent: [],
      rootsVersion: 0,
    });
    mocks.prepareDeliveryMutation.mockResolvedValue({
      digest: "digest-abc",
      action: "create_worktree",
      summary: "Create an owned worktree",
      impact: "Local only",
      repositorySlug: "owner/repo",
      branch: null,
      external: false,
      expiresAtMs: Date.now() + 60_000,
      confirmationPhrase: "CONFIRM-abc",
    });
    mocks.executeDeliveryMutation.mockResolvedValue({
      marker: {
        schemaVersion: 1,
        worktreeId: "wt-1",
        leaseNonce: "nonce",
        repositoryId: "repo-id",
        repositorySlug: "owner/repo",
        repositoryRoot: "/repo",
        commonGitDir: "/repo/.git",
        canonicalPath: "/repo-worktrees/security-autofix-lodash",
        branch: "security-autofix/security-dependency-lodash",
        baseOid: "abc123",
        policy: {
          allowedRemotes: ["origin"],
          branchPrefix: "security-autofix/",
          protectedBranches: ["main"],
          allowPush: true,
          allowCreatePullRequest: true,
          allowReviewComment: false,
          allowForkWrites: false,
        },
        createdAtMs: 1,
      },
      state: "active",
      locked: false,
      lockReason: null,
      archivePath: null,
      createdAtMs: 1,
      updatedAtMs: 1,
    });
    mocks.invoke.mockImplementation(async (command: string) => {
      if (command === "add_secondary_workspace_root") {
        return { id: "sec-1", path: "/repo-worktrees/security-autofix-lodash", label: "security-autofix-lodash", is_primary: false };
      }
      if (command === "get_workspace_roots") return [];
      return null;
    });

    const result = await createIsolatedBranchForFinding(dependencyFinding(), "owner/repo");

    expect(result).toEqual({
      worktreeId: "wt-1",
      branch: "security-autofix/security-dependency-lodash",
      workspaceLabel: "security-autofix-lodash",
      canonicalPath: "/repo-worktrees/security-autofix-lodash",
    });
    expect(mocks.executeDeliveryMutation).toHaveBeenCalledWith(
      expect.objectContaining({ kind: "create_worktree" }),
      "digest-abc",
      "CONFIRM-abc",
    );
    expect(mocks.invoke).toHaveBeenCalledWith("add_secondary_workspace_root", {
      path: "/repo-worktrees/security-autofix-lodash",
    });
  });
});

describe("runSecurityAutofixAgent", () => {
  const proposal: SecurityFixProposal = {
    findingId: "dep-1",
    exploitabilityNote: "note",
    proposedFix: "fix",
    testPlan: "plan",
    generatedAtMs: 1,
    source: "model",
  };

  function baseParams(overrides: Partial<Parameters<typeof runSecurityAutofixAgent>[0]> = {}) {
    return {
      runId: "run-1",
      finding: dependencyFinding(),
      proposal,
      branch: "security-autofix/security-dependency-lodash",
      workspaceLabel: "security-autofix-lodash",
      signal: new AbortController().signal,
      ...overrides,
    };
  }

  it("returns the agent's final reply once it stops requesting tool calls", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "Upgraded lodash and ran tests.", toolCalls: [], streamError: null, contentStarted: true });
    const result = await runSecurityAutofixAgent(baseParams());
    expect(result).toEqual({ outcome: "completed", summary: "Upgraded lodash and ran tests.", durableRunId: null });
  });

  it("reports a stream error as an error outcome", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "", toolCalls: [], streamError: "provider unreachable", contentStarted: false });
    const result = await runSecurityAutofixAgent(baseParams());
    expect(result.outcome).toBe("error");
    expect(result.summary).toBe("provider unreachable");
  });

  it("returns a cancelled outcome when the signal is already aborted", async () => {
    const controller = new AbortController();
    controller.abort();
    const result = await runSecurityAutofixAgent(baseParams({ signal: controller.signal }));
    expect(result.outcome).toBe("cancelled");
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("executes tool calls the model requests, then finishes on its final reply", async () => {
    mocks.attemptStream
      .mockResolvedValueOnce({
        content: "",
        toolCalls: [{ id: "call-1", type: "function", function: { name: "run_shell", arguments: "{}" } }],
        streamError: null,
        contentStarted: true,
      })
      .mockResolvedValueOnce({ content: "Done.", toolCalls: [], streamError: null, contentStarted: true });
    mocks.executeToolCall.mockResolvedValue(JSON.stringify({ stdout: "ok", stderr: "", code: 0 }));

    const result = await runSecurityAutofixAgent(baseParams());

    expect(result).toEqual({ outcome: "completed", summary: "Done.", durableRunId: null });
    expect(mocks.executeToolCall).toHaveBeenCalledTimes(1);
    expect(mocks.executeToolCall.mock.calls[0][8]).toBe("security-autofix");
  });
});

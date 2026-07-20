import { describe, expect, it } from "vitest";

import type { ModelTargetSnapshot } from "./modelTargets";
import type { ModelTargetSnapshotWire, RunRecord, WorkspaceContextWire } from "./runProtocol";
import { findReplayTarget, replayPermissionMode, workspaceReplayProblem } from "./runCapsuleReplay";

const capability = { state: "unknown" as const, evidence: "test" };
const wireCapabilities = {
  tool_calling: { state: "unknown" as const, evidence: "test" },
  vision: { state: "unknown" as const, evidence: "test" },
  embeddings: { state: "unknown" as const, evidence: "test" },
  structured_output: { state: "unknown" as const, evidence: "test" },
  image_generation: { state: "unknown" as const, evidence: "test" },
  audio: { state: "unknown" as const, evidence: "test" },
  runtime_lifecycle: { state: "unknown" as const, evidence: "test" },
  fim: { state: "unknown" as const, evidence: "test" },
  code_completion: { state: "unknown" as const, evidence: "test" },
  inline_edit: { state: "unknown" as const, evidence: "test" },
  fim_metadata: null,
};

function providerWire(overrides: Partial<Extract<ModelTargetSnapshotWire, { kind: "provider" }>> = {}): ModelTargetSnapshotWire {
  return {
    kind: "provider",
    target_id: "target-1",
    label: "OpenAI · gpt-test",
    provider_id: "openai",
    endpoint: "https://api.example.test/v1",
    model: "gpt-test",
    credential_ref_id: "credential-1",
    capabilities: wireCapabilities,
    ...overrides,
  };
}

const providerTarget: ModelTargetSnapshot = {
  kind: "provider",
  key: "provider:openai:gpt-test",
  label: "OpenAI",
  displayName: "gpt-test",
  providerId: "openai",
  endpoint: "https://api.example.test/v1/",
  model: "gpt-test",
  credentialRefId: "credential-1",
  capabilities: { toolCalling: capability, vision: capability },
  availability: { status: "available", evidence: "configured" },
};

describe("run capsule replay preflight", () => {
  it("selects only the exact available endpoint and credential", () => {
    expect(findReplayTarget(providerWire(), [providerTarget])).toBe(providerTarget);
    expect(findReplayTarget(providerWire({ endpoint: "https://other.test/v1" }), [providerTarget])).toBeNull();
    expect(findReplayTarget(providerWire({ credential_ref_id: "rotated" }), [providerTarget])).toBeNull();
    expect(findReplayTarget(providerWire(), [{ ...providerTarget, availability: { status: "unavailable", evidence: "off" } }])).toBeNull();
  });

  it("requires every frozen root and preserves writable primary identity", () => {
    const frozen: WorkspaceContextWire = {
      workspace_id: "workspace-1",
      primary_root_id: "root-1",
      roots: [{ root_id: "root-1", canonical_path: "/work/app/", access: "read_write", allow_symlinks_within_root: false }],
      repository_policy: null,
    };
    expect(workspaceReplayProblem(frozen, [{ id: "current", path: "/work/app", label: "app", is_primary: true }])).toBeNull();
    expect(workspaceReplayProblem(frozen, [])).toMatch(/no longer attached/i);
    expect(workspaceReplayProblem(frozen, [{ id: "current", path: "/work/app", label: "app", is_primary: false }])).toMatch(/no longer primary/i);
  });

  it("downgrades a frozen bypass permission to manual", () => {
    const run = { spec: { permission_policy: { mode: "bypass" } } } as RunRecord;
    expect(replayPermissionMode(run)).toBe("manual");
    const normal = { spec: { permission_policy: { mode: "acceptEdits" } } } as RunRecord;
    expect(replayPermissionMode(normal)).toBe("acceptEdits");
  });
});

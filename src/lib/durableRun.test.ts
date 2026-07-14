import { describe, expect, it } from "vitest";

import type { ModelTargetSnapshot } from "./modelTargets";
import {
  defaultRunBudgets,
  modelTargetToRunWire,
  permissionPolicyForRun,
  redactSensitiveText,
  utf8Chunks,
  workspaceToRunWire,
} from "./durableRun";

const CAPABILITIES = {
  toolCalling: { state: "yes" as const, evidence: "advertised" },
  vision: { state: "no" as const, evidence: "not advertised" },
};

describe("durable run snapshots", () => {
  it("freezes a provider endpoint and stores only an opaque credential reference", () => {
    const target: ModelTargetSnapshot = {
      kind: "provider",
      key: "provider:custom:some%2Fmodel",
      label: "Custom",
      displayName: "some/model",
      providerId: "custom",
      endpoint: "https://models.example/v1",
      model: "some/model",
      credentialRefId: "keychain:com.littlemonkey.app:custom",
      capabilities: CAPABILITIES,
      availability: { status: "available", evidence: "configured" },
    };
    const wire = modelTargetToRunWire(target);
    expect(wire).toMatchObject({
      kind: "provider",
      endpoint: "https://models.example/v1",
      credential_ref_id: "keychain:com.littlemonkey.app:custom",
    });
    expect(wire.target_id).toMatch(/^[A-Za-z0-9][A-Za-z0-9_.:-]*[A-Za-z0-9]$/);
    expect(JSON.stringify(wire)).not.toContain("api_key");
  });

  it("builds a default-deny repository policy from canonical roots", () => {
    const wire = workspaceToRunWire([
      { id: "root-1", path: "/workspace/project", label: "project", is_primary: true },
    ]);
    expect(wire?.primary_root_id).toBe("root-1");
    expect(wire?.repository_policy).toMatchObject({ allow_commit: true, allow_push: false, allow_merge: false });
  });

  it("forbids implicit mutations in plan mode and keeps budgets finite", () => {
    expect(permissionPolicyForRun("plan").default_tool_decision).toBe("deny");
    expect(permissionPolicyForRun("auto").tool_rules.map((rule) => rule.tool)).toEqual([
      "write_file",
      "edit_file",
      "remember",
    ]);
    const budgets = defaultRunBudgets();
    expect(budgets.wall_time_ms).toBeGreaterThan(0);
    expect(budgets.max_event_count).toBeLessThanOrEqual(20_000);
  });

  it("chunks event text by UTF-8 bytes without splitting Unicode code points", () => {
    const source = "ab🙂cdef🙂gh";
    const chunks = utf8Chunks(source, 6);
    expect(chunks.join("")).toBe(source);
    expect(chunks.every((chunk) => new TextEncoder().encode(chunk).byteLength <= 6)).toBe(true);
    expect(chunks.every((chunk) => !chunk.includes("�"))).toBe(true);
  });

  it("redacts common credentials before run specs or events are persisted", () => {
    const redacted = redactSensitiveText([
      "Authorization: Bearer abcdefghijklmnopqrstuvwxyz",
      "OPENAI_API_KEY=sk-supersecrettoken12345",
      "password: hunter2",
      "https://alice:secret@example.test/path",
    ].join("\n"));
    expect(redacted).not.toContain("abcdefghijklmnopqrstuvwxyz");
    expect(redacted).not.toContain("supersecrettoken");
    expect(redacted).not.toContain("hunter2");
    expect(redacted).not.toContain("alice:secret");
    expect(redacted).toContain("[REDACTED");
  });
});

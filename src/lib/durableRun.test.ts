import { describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  appendRunEvent: vi.fn(async (..._args: unknown[]) => ({ envelope: null, status: "running", terminal: false })),
}));

vi.mock("./runProtocol", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./runProtocol")>()),
  appendRunEvent: mocks.appendRunEvent,
}));

import type { ModelTargetSnapshot } from "./modelTargets";
import type { RunEventWire } from "./runProtocol";
import {
  DurableRunRecorder,
  defaultRunBudgets,
  discoverDurableArtifacts,
  modelTargetToRunWire,
  permissionPolicyForRun,
  redactPrivatePaths,
  redactSensitiveText,
  sanitizeToolArguments,
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

describe("redactPrivatePaths", () => {
  it("aliases workspace roots stably and strips home directories on every platform", () => {
    const roots = ["/Users/tester/projects/demo", "/Users/tester/projects/demo-tools"];
    const text = [
      "read /Users/tester/projects/demo/src/main.ts",
      "wrote /Users/tester/projects/demo-tools/bin/tool",
      "home file /Users/tester/.zshrc",
      "linux /home/tester/notes.txt",
      "windows C:\\Users\\Tester\\Documents\\report.docx",
    ].join("\n");
    const redacted = redactPrivatePaths(text, roots);
    expect(redacted).toContain("$WORKSPACE_1/bin/tool");
    expect(redacted).toContain("$WORKSPACE_2/src/main.ts");
    expect(redacted).toContain("$HOME/.zshrc");
    expect(redacted).toContain("$HOME/notes.txt");
    expect(redacted).toContain("$HOME\\Documents\\report.docx");
    expect(redacted).not.toContain("/Users/tester");
    expect(redacted).not.toContain("C:\\Users");
    expect(redactPrivatePaths(text, roots)).toBe(redacted);
  });
});

describe("sanitizeToolArguments", () => {
  it("redacts secret keys in nested arguments and leaves benign arguments untouched", () => {
    const sanitized = sanitizeToolArguments(JSON.stringify({
      path: "src/app.ts",
      options: { api_key: "sk-verysecrettoken1234", retries: 2 },
    }));
    expect(sanitized.redaction).toBe("applied");
    expect(sanitized.value).toEqual({ path: "src/app.ts", options: { api_key: "[REDACTED]", retries: 2 } });

    const clean = sanitizeToolArguments(JSON.stringify({ path: "src/app.ts", count: 3 }));
    expect(clean.redaction).toBe("not_needed");
    expect(clean.value).toEqual({ path: "src/app.ts", count: 3 });
  });

  it("falls back to a marker snapshot when arguments are not valid JSON", () => {
    expect(sanitizeToolArguments("{not json")).toEqual({
      value: { unavailable: "invalid_json" },
      redaction: "applied",
    });
  });

  it("bounds string, entry, and depth budgets per value", () => {
    const long = sanitizeToolArguments(JSON.stringify({ text: "y".repeat(9_000) }));
    expect(long.redaction).toBe("applied");
    expect((long.value as { text: string }).text.endsWith("[TRUNCATED]")).toBe(true);

    const wide = sanitizeToolArguments(JSON.stringify({ items: Array.from({ length: 200 }, (_, index) => index) }));
    const items = (wide.value as { items: unknown[] }).items;
    expect(items).toHaveLength(129);
    expect(items[items.length - 1]).toBe("[TRUNCATED: entry limit]");

    let deep: Record<string, unknown> = { leaf: "value" };
    for (let index = 0; index < 12; index += 1) deep = { nested: deep };
    const bounded = sanitizeToolArguments(JSON.stringify(deep));
    expect(bounded.redaction).toBe("applied");
    expect(JSON.stringify(bounded.value)).toContain("[TRUNCATED: depth limit]");
  });

  it("caps the total serialized snapshot below the ledger event budget", () => {
    const raw = JSON.stringify({ chunks: Array.from({ length: 40 }, () => "z".repeat(7_000)) });
    const capped = sanitizeToolArguments(raw);
    expect(capped.redaction).toBe("applied");
    expect(capped.value).toMatchObject({ truncated: "total_size_limit" });
    expect(new TextEncoder().encode(JSON.stringify(capped.value)).byteLength).toBeLessThan(1_024);
  });
});

describe("discoverDurableArtifacts", () => {
  it("finds nested content-addressed artifacts, dedupes, and rejects unsafe metadata", () => {
    const screenshotSha = "ab".repeat(32);
    const domSha = "cd".repeat(32);
    const result = JSON.stringify({
      ok: true,
      outputs: [
        { screenshot: { id: screenshotSha, size: 10 } },
        { screenshot: { id: screenshotSha, size: 10 } },
        { dom: { id: domSha, size: 20 } },
        { invalid: { id: "not-a-sha", size: 10 } },
        { uppercase: { id: "AB".repeat(32), size: 10 } },
        { negative: { id: "e".repeat(64), size: -1 } },
        { fractional: { id: "0".repeat(64), size: 1.5 } },
        { unsafe: { id: "1".repeat(64), size: Number.MAX_SAFE_INTEGER + 1 } },
      ],
    });
    const artifacts = discoverDurableArtifacts(result);
    expect(artifacts.map((artifact) => artifact.id).sort()).toEqual([screenshotSha, domSha].sort());
    expect(artifacts.find((artifact) => artifact.id === screenshotSha)).toMatchObject({ kind: "image", mediaType: "image/png" });
    expect(artifacts.find((artifact) => artifact.id === domSha)).toMatchObject({ kind: "document", mediaType: "text/html" });
    expect(discoverDurableArtifacts("plain text result")).toEqual([]);
  });
});

describe("DurableRunRecorder evidence", () => {
  it("bounds tool output excerpts and links discovered artifacts", async () => {
    mocks.appendRunEvent.mockClear();
    const recorder = new DurableRunRecorder("run-evidence", null, ["/Users/tester/projects/demo"]);
    const artifactId = "9".repeat(64);
    await recorder.recordToolProposed("call-1", "browser_screenshot", JSON.stringify({ url: "https://example.test" }));
    await recorder.recordToolFinished(
      "call-1",
      JSON.stringify({
        screenshot: { id: artifactId, size: 2_048 },
        log: `saved under /Users/tester/projects/demo/out.png\n${"x".repeat(10_000)}`,
      }),
      1_234.9,
    );
    await recorder.flush();

    const events = mocks.appendRunEvent.mock.calls.map((call) => call[1] as RunEventWire);
    expect(events.map((event) => event.type)).toEqual(["tool_proposed", "tool_finished", "artifact_added"]);

    const finished = events.find((event) => event.type === "tool_finished") as Extract<RunEventWire, { type: "tool_finished" }>;
    expect(finished.payload.duration_ms).toBe(1_234);
    expect(finished.payload.output_sha256).toMatch(/^[a-f0-9]{64}$/);
    const excerpt = finished.payload.output_excerpt ?? "";
    expect(excerpt.endsWith("[TRUNCATED]")).toBe(true);
    expect(excerpt).toContain("$WORKSPACE_1/out.png");
    expect(excerpt).not.toContain("/Users/tester");
    expect(new TextEncoder().encode(excerpt).byteLength).toBeLessThanOrEqual(4_000 + "\n[TRUNCATED]".length);

    const artifact = events.find((event) => event.type === "artifact_added") as Extract<RunEventWire, { type: "artifact_added" }>;
    expect(artifact.payload).toMatchObject({
      artifact_id: artifactId,
      content_sha256: artifactId,
      size_bytes: 2_048,
      kind: "image",
      media_type: "image/png",
      name: "browser_screenshot: screenshot",
    });
  });
});

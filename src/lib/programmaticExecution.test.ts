import { afterEach, describe, expect, it, vi } from "vitest";

import type { ToolDef } from "./llamaClient";
import {
  DEFAULT_PROGRAMMATIC_LIMITS,
  QuickJsProgrammaticRuntime,
} from "./programmaticQuickJsRuntime";
import { ProgrammaticExecutionService } from "./programmaticExecution";
import type {
  ProgrammaticExecutionRequest,
  ProgrammaticNestedToolResult,
  ProgrammaticExecutionRuntime,
  ProgrammaticExecutionResult,
  ProgrammaticRuntimeCapabilities,
} from "./programmaticExecution";

const readTool: ToolDef = {
  type: "function",
  function: {
    name: "read_file",
    description: "Read a file",
    parameters: {
      type: "object",
      properties: { path: { type: "string" } },
      required: ["path"],
      additionalProperties: false,
    },
  },
};

const lookupTool: ToolDef = {
  type: "function",
  function: {
    name: "mcp__server__lookup-v2",
    description: "Lookup a value",
    parameters: {
      type: "object",
      properties: { id: { type: "string" } },
      required: ["id"],
      additionalProperties: false,
    },
  },
};

let executionNumber = 0;

function makeRequest(
  source: string,
  overrides: Partial<ProgrammaticExecutionRequest> = {},
): ProgrammaticExecutionRequest {
  return {
    executionId: `test-program-${++executionNumber}`,
    source,
    toolDefinitions: [readTool, lookupTool],
    isToolAvailable: () => true,
    invokeTool: async (toolName, args): Promise<ProgrammaticNestedToolResult> => ({
      content: JSON.stringify({ toolName, args }),
      cancelled: false,
    }),
    ...overrides,
  };
}

describe("QuickJsProgrammaticRuntime", () => {
  const runtime = new QuickJsProgrammaticRuntime();

  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("reports a healthy async provider capability", async () => {
    await expect(runtime.capabilities()).resolves.toEqual({
      provider: "quickjs-wasm",
      healthy: true,
      supportsAsync: true,
    });
  });

  it("executes a JSON program and one nested tool through the SDK", async () => {
    const result = await runtime.execute(makeRequest(
      'const file = await tools.read_file({path: "src/index.ts"}); return {file, ok: true};',
    ));

    expect(result.status).toBe("succeeded");
    expect(result.value).toEqual({
      file: { toolName: "read_file", args: { path: "src/index.ts" } },
      ok: true,
    });
    expect(result.nestedCalls).toMatchObject([
      { toolName: "read_file", arguments: { path: "src/index.ts" }, status: "succeeded" },
    ]);
  });

  it("re-checks authorization after SDK generation", async () => {
    let invokeCount = 0;
    const result = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "removed-after-generation"});',
      {
        isToolAvailable: () => false,
        invokeTool: async () => {
          invokeCount += 1;
          return { content: "unexpected", cancelled: false };
        },
      },
    ));

    expect(result.status).toBe("failed");
    expect(result.failure?.category).toBe("nested_tool_failure");
    expect(result.nestedCalls).toHaveLength(0);
    expect(invokeCount).toBe(0);
  });

  it("runs sequential calls in source order", async () => {
    const order: string[] = [];
    const result = await runtime.execute(makeRequest(
      'const first = await tools.read_file({path: "one"}); const second = await tools.read_file({path: "two"}); return [first, second];',
      {
        invokeTool: async (_name, args) => {
          order.push(String(args.path));
          return { content: JSON.stringify(args.path), cancelled: false };
        },
      },
    ));

    expect(result.status).toBe("succeeded");
    expect(order).toEqual(["one", "two"]);
  });

  it("bounds concurrent Promise.all calls", async () => {
    let active = 0;
    let peak = 0;
    const result = await runtime.execute(makeRequest(
      'return await Promise.all([tools.read_file({path: "a"}), tools.read_file({path: "b"}), tools.read_file({path: "c"}), tools.read_file({path: "d"})]);',
      {
        limits: { maxConcurrentCalls: 2 },
        invokeTool: async (_name, args) => {
          active += 1;
          peak = Math.max(peak, active);
          await new Promise((resolve) => setTimeout(resolve, 5));
          active -= 1;
          return { content: JSON.stringify(args.path), cancelled: false };
        },
      },
    ));

    expect(result.status).toBe("succeeded");
    expect(peak).toBe(2);
    expect(result.nestedCalls).toHaveLength(4);
  });

  it("supports namespaced and hyphenated tool names through bracket access", async () => {
    const result = await runtime.execute(makeRequest(
      'return await tools["mcp__server__lookup-v2"]({id: "42"});',
    ));

    expect(result.status).toBe("succeeded");
    expect(result.value).toEqual({ toolName: "mcp__server__lookup-v2", args: { id: "42" } });
  });

  it("escapes line separators and markup in generated tool wrappers", async () => {
    const hostileName = `mcp__server__</script>${String.fromCharCode(0x2029)}`;
    const hostileTool: ToolDef = {
      ...lookupTool,
      function: { ...lookupTool.function, name: hostileName },
    };
    const result = await runtime.execute(makeRequest(
      'return await tools["mcp__server__</script>" + String.fromCharCode(0x2029)]({id: "42"});',
      { toolDefinitions: [hostileTool] },
    ));

    expect(result.status).toBe("succeeded");
    expect(result.value).toEqual({ toolName: hostileName, args: { id: "42" } });
  });

  it("leaves schema enforcement to the canonical dispatcher", async () => {
    const invoke = vi.fn().mockResolvedValue({ content: JSON.stringify({ accepted: true }), cancelled: false });
    const result = await runtime.execute(makeRequest(
      "return await tools.read_file({});",
      { invokeTool: invoke },
    ));

    expect(result.status).toBe("succeeded");
    expect(result.value).toEqual({ accepted: true });
    expect(invoke).toHaveBeenCalledOnce();
  });

  it("does not offer recursive execution or untrusted host globals", async () => {
    const hostResult = await runtime.execute(makeRequest(
      "return {process: typeof process, fs: typeof fs, fetch: typeof fetch, env: typeof env, hostInvoker: typeof globalThis.__lmInvoke, lexicalInvoker: typeof __lmInvoke};",
    ));
    const recursiveResult = await runtime.execute(makeRequest(
      'return await tools["run_program"]({source: "return 1"});',
    ));

    expect(hostResult.status).toBe("succeeded");
    expect(hostResult.value).toEqual({ process: "undefined", fs: "undefined", fetch: "undefined", env: "undefined", hostInvoker: "undefined", lexicalInvoker: "undefined" });
    expect(recursiveResult.status).toBe("failed");
    expect(recursiveResult.nestedCalls).toHaveLength(0);
  });

  it("rejects non-JSON return values and runtime exceptions", async () => {
    const invalid = await runtime.execute(makeRequest("return undefined;"));
    const thrown = await runtime.execute(makeRequest('throw new Error("bad program");'));

    expect(invalid.failure?.category).toBe("invalid_result");
    expect(thrown.failure?.category).toBe("runtime_exception");
    expect(thrown.failure?.message).toContain("bad program");
  });

  it("classifies syntax errors as invalid source", async () => {
    const result = await runtime.execute(makeRequest("return ; not valid javascript"));
    expect(result.failure?.category).toBe("invalid_source");
  });

  it("enforces source, nested-call, log, and return limits", async () => {
    const source = await runtime.execute(makeRequest("return 1;", { limits: { maxSourceBytes: 2 } }));
    const nested = await runtime.execute(makeRequest(
      'return [await tools.read_file({path: "a"}), await tools.read_file({path: "b"})];',
      { limits: { maxNestedCalls: 1 } },
    ));
    const logs = await runtime.execute(makeRequest(
      'console.log("one"); console.log("two"); return true;',
      { limits: { maxLogEntries: 1 } },
    ));
    const output = await runtime.execute(makeRequest(
      'return "123456";',
      { limits: { maxSerializedReturnBytes: 3 } },
    ));

    expect(source.failure?.category).toBe("invalid_source");
    expect(nested.failure?.category).toBe("execution_budget");
    expect(nested.nestedCalls).toHaveLength(1);
    expect(logs.failure?.category).toBe("output_limit");
    expect(output.failure?.category).toBe("output_limit");
  });

  it("interrupts an infinite synchronous program at the instruction budget", async () => {
    const result = await runtime.execute(makeRequest("while (true) {}", {
      limits: { maxInstructionInterrupts: 1, maxWallMs: 5_000 },
    }));

    expect(result.status).toBe("failed");
    expect(result.failure?.category).toBe("execution_budget");
  });

  it("interrupts an unresolved program at the wall-clock limit", async () => {
    const result = await runtime.execute(makeRequest(
      "await new Promise(() => {}); return true;",
      { limits: { maxWallMs: 20 } },
    ));

    expect(result.status).toBe("failed");
    expect(result.failure?.category).toBe("execution_timeout");
  });

  it("does not await a hanging nested host call past the wall-clock limit", async () => {
    const startedAt = Date.now();
    const result = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "hanging"});',
      {
        limits: { maxWallMs: 20 },
        invokeTool: async () => new Promise<ProgrammaticNestedToolResult>(() => {}),
      },
    ));

    expect(Date.now() - startedAt).toBeLessThan(500);
    expect(result.failure?.category).toBe("execution_timeout");
  });

  it("propagates cancellation to an in-flight nested call", async () => {
    let nestedSignal: AbortSignal | undefined;
    const request = makeRequest(
      'return await tools.read_file({path: "slow"});',
      {
        invokeTool: async (_name, _args, _id, signal) => {
          nestedSignal = signal;
          return new Promise((resolve) => {
            signal.addEventListener("abort", () => resolve({ content: "", cancelled: true }), { once: true });
          });
        },
      },
    );
    const pending = runtime.execute(request);
    await new Promise((resolve) => setTimeout(resolve, 10));
    expect(runtime.cancel(request.executionId)).toBe(true);
    const result = await pending;

    expect(nestedSignal?.aborted).toBe(true);
    expect(result.status).toBe("cancelled");
    expect(result.failure?.category).toBe("cancelled");
  });

  it("records typed permission and nested tool failures", async () => {
    const permission = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "secret"});',
      {
        invokeTool: async () => ({
          content: JSON.stringify({ error: "Permission denied by the user." }),
          cancelled: false,
          failure: { category: "permission_denied", message: "Permission denied by the user.", toolName: "read_file" },
        }),
      },
    ));
    const failed = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "missing"});',
      {
        invokeTool: async () => ({ content: "", cancelled: false, failure: { category: "nested_tool_failure", message: "Missing file", toolName: "read_file" } }),
      },
    ));

    expect(permission.failure?.category).toBe("permission_denied");
    expect(permission.nestedCalls[0]?.failure?.toolName).toBe("read_file");
    expect(failed.failure?.category).toBe("nested_tool_failure");
  });

  it("keeps logs and evidence bounded and redacted", async () => {
    const result = await runtime.execute(makeRequest(
      'console.log("Bearer sk-test-token-123456789"); return await tools.read_file({path: "/Users/alice/project/file.ts"});',
      { workspaceRoots: ["/Users/alice/project"] },
    ));

    expect(result.logs.join("\n")).not.toContain("sk-test-token");
    expect(JSON.stringify(result.nestedCalls)).not.toContain("/Users/alice/project");
    expect(result.nestedCalls[0]?.arguments).toEqual({ path: "$WORKSPACE_1/file.ts" });
  });

  it("rejects malformed execution limits", async () => {
    await expect(runtime.execute(makeRequest("return true;", {
      limits: { maxWallMs: 0 },
    }))).rejects.toThrow("Invalid programmatic execution limit maxWallMs");
  });

  it("uses secure defaults for every hard limit", () => {
    expect(DEFAULT_PROGRAMMATIC_LIMITS.maxSourceBytes).toBeLessThanOrEqual(64 * 1024);
    expect(DEFAULT_PROGRAMMATIC_LIMITS.maxNestedCalls).toBeLessThanOrEqual(32);
    expect(DEFAULT_PROGRAMMATIC_LIMITS.maxConcurrentCalls).toBeLessThanOrEqual(4);
    expect(DEFAULT_PROGRAMMATIC_LIMITS.maxRuntimeMemoryBytes).toBeLessThanOrEqual(64 * 1024 * 1024);
  });

  it("rejects an empty source before loading the runtime", async () => {
    const result = await runtime.execute(makeRequest(""));
    expect(result.status).toBe("failed");
    expect(result.failure?.category).toBe("invalid_source");
  });

  it("enforces the per-call argument byte limit", async () => {
    const result = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "123456"});',
      { limits: { maxPerCallArgumentBytes: 4 } },
    ));
    expect(result.failure?.category).toBe("output_limit");
  });

  it("rejects a tool that is not offered in the turn", async () => {
    const result = await runtime.execute(makeRequest(
      'return await tools.missing({});',
    ));
    expect(result.status).toBe("failed");
    expect(result.nestedCalls).toHaveLength(0);
    expect(result.failure?.category).toBe("runtime_exception");
  });

  it("rejects cyclic and non-finite return values", async () => {
    const cyclic = await runtime.execute(makeRequest("const value = {}; value.self = value; return value;"));
    const nonFinite = await runtime.execute(makeRequest("return NaN;"));
    expect(cyclic.failure?.category).toBe("invalid_result");
    expect(nonFinite.failure?.category).toBe("invalid_result");
  });

  it("caps a nested tool result independently from the program result", async () => {
    const result = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "large"});',
      {
        limits: { maxSerializedReturnBytes: 3 },
        invokeTool: async () => ({ content: JSON.stringify("large"), cancelled: false }),
      },
    ));
    expect(result.failure?.category).toBe("output_limit");
    expect(result.nestedCalls[0]?.failure?.category).toBe("output_limit");
  });

  it("turns a host invocation exception into a nested failure", async () => {
    const result = await runtime.execute(makeRequest(
      'return await tools.read_file({path: "boom"});',
      { invokeTool: async () => { throw new Error("host failed"); } },
    ));
    expect(result.failure?.category).toBe("nested_tool_failure");
    expect(result.failure?.message).toContain("host failed");
  });

  it("honors a parent abort before execution starts", async () => {
    const controller = new AbortController();
    controller.abort();
    const result = await runtime.execute(makeRequest("return true;", { signal: controller.signal }));
    expect(result.status).toBe("cancelled");
    expect(result.failure?.category).toBe("cancelled");
  });

  it("records bounded console output on successful programs", async () => {
    const result = await runtime.execute(makeRequest('console.info("ready"); return true;'));
    expect(result.status).toBe("succeeded");
    expect(result.logs).toEqual(['["ready"]']);
  });

  it("keeps the provider-neutral service contract injectable", async () => {
    const capabilities: ProgrammaticRuntimeCapabilities = {
      provider: "test-provider",
      healthy: true,
      supportsAsync: true,
    };
    const execution: ProgrammaticExecutionResult = {
      executionId: "injected",
      status: "succeeded",
      value: 1,
      logs: [],
      nestedCalls: [],
      durationMs: 1,
    };
    const fake: ProgrammaticExecutionRuntime = {
      capabilities: vi.fn(async () => capabilities),
      execute: vi.fn(async () => execution),
      cancel: vi.fn(() => true),
    };
    const service = new ProgrammaticExecutionService(fake);
    expect(await service.capabilities()).toEqual(capabilities);
    expect(await service.execute(makeRequest("return 1;"))).toEqual(execution);
    expect(service.cancel("injected")).toBe(true);
    expect(fake.execute).toHaveBeenCalledOnce();
  });
});

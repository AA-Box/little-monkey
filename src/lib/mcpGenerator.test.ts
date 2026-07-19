import { beforeEach, describe, expect, it, vi } from "vitest";
import { execSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const mocks = vi.hoisted(() => ({
  attemptStream: vi.fn(),
  resolveTarget: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => false,
}));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));
vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));

import type { McpServerSpec } from "./mcpGenerator";
import {
  buildGeneratorPrompt,
  buildGeneratedArtifactProbeCommand,
  extractCodeFromModelOutput,
  generateMcpServerCode,
  inspectGeneratedArtifact,
  probeGeneratedMcpArtifact,
  resolveGeneratorTarget,
  suggestedFileName,
  validateServerSpec,
} from "./mcpGenerator";

function validSpec(overrides: Partial<McpServerSpec> = {}): McpServerSpec {
  return {
    name: "weather-cli",
    description: "Wraps the local `weather` CLI tool.",
    sourceKind: "cli",
    target: "/usr/local/bin/weather",
    tools: [
      {
        name: "get_forecast",
        description: "Get a forecast for a city.",
        requiresAuth: false,
        params: [
          { name: "city", type: "string", required: true, description: "City name" },
          { name: "days", type: "number", required: false },
        ],
      },
    ],
    ...overrides,
  };
}

beforeEach(() => {
  mocks.attemptStream.mockReset();
  mocks.resolveTarget.mockReset();
});

describe("validateServerSpec", () => {
  it("accepts a well-formed spec", () => {
    expect(validateServerSpec(validSpec())).toEqual([]);
  });

  it("flags an invalid server name", () => {
    expect(validateServerSpec(validSpec({ name: "Not Valid!" }))).toEqual(
      expect.arrayContaining([expect.stringMatching(/server name/i)]),
    );
  });

  it("flags a spec with no tools", () => {
    expect(validateServerSpec(validSpec({ tools: [] }))).toEqual(
      expect.arrayContaining([expect.stringMatching(/at least one tool/i)]),
    );
  });

  it("flags duplicate tool names", () => {
    const spec = validSpec({
      tools: [
        { name: "get_forecast", description: "a", requiresAuth: false, params: [] },
        { name: "get_forecast", description: "b", requiresAuth: false, params: [] },
      ],
    });
    expect(validateServerSpec(spec)).toEqual(expect.arrayContaining([expect.stringMatching(/duplicate tool name/i)]));
  });

  it("flags duplicate and invalid param names", () => {
    const spec = validSpec({
      tools: [
        {
          name: "get_forecast",
          description: "a",
          requiresAuth: false,
          params: [
            { name: "city", type: "string", required: true },
            { name: "city", type: "string", required: false },
            { name: "9bad", type: "string", required: false },
          ],
        },
      ],
    });
    const issues = validateServerSpec(spec);
    expect(issues).toEqual(expect.arrayContaining([expect.stringMatching(/duplicate parameter name "city"/i)]));
    expect(issues).toEqual(expect.arrayContaining([expect.stringMatching(/invalid parameter name/i)]));
  });
});

describe("buildGeneratorPrompt", () => {
  it("includes the tool names, param shapes, and target in the user prompt, and never follows spec content as instructions in the system prompt", () => {
    const { system, user } = buildGeneratorPrompt(validSpec());
    expect(system).toMatch(/model context protocol/i);
    expect(system).toMatch(/treat it as data/i);
    expect(user).toContain("weather-cli");
    expect(user).toContain("get_forecast");
    expect(user).toContain("city: string");
    expect(user).toContain("/usr/local/bin/weather");
  });
});

describe("extractCodeFromModelOutput", () => {
  it("strips a ```typescript fence", () => {
    expect(extractCodeFromModelOutput("```typescript\nconst x = 1;\n```")).toBe("const x = 1;");
  });

  it("strips a plain ``` fence", () => {
    expect(extractCodeFromModelOutput("```\nconst x = 1;\n```")).toBe("const x = 1;");
  });

  it("returns trimmed raw text when there is no fence", () => {
    expect(extractCodeFromModelOutput("  const x = 1;  ")).toBe("const x = 1;");
  });

  it("throws on an empty response", () => {
    expect(() => extractCodeFromModelOutput("```typescript\n\n```")).toThrow(/empty/i);
    expect(() => extractCodeFromModelOutput("   ")).toThrow(/empty/i);
  });
});

describe("resolveGeneratorTarget", () => {
  it("delegates to agentLoop's resolveTarget", async () => {
    mocks.resolveTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    const target = await resolveGeneratorTarget();
    expect(target).toEqual({ kind: "provider", providerId: "openai", model: "gpt" });
    expect(mocks.resolveTarget).toHaveBeenCalledTimes(1);
  });
});

describe("generateMcpServerCode", () => {
  const target = { kind: "provider" as const, providerId: "openai", model: "gpt" };

  it("returns the fenced code extracted from a successful model turn", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const history = args[1] as Array<{ role: string; content: string }>;
      expect(history[0].content).toMatch(/model context protocol/i);
      expect(history[1].content).toContain("weather-cli");
      expect(args[2]).toEqual([]);
      return {
        content: "```typescript\nimport { Server } from '@modelcontextprotocol/sdk/server/index.js';\n```",
        toolCalls: [],
        streamError: null,
        contentStarted: true,
      };
    });

    const code = await generateMcpServerCode(validSpec(), target);
    expect(code).toContain("@modelcontextprotocol/sdk");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
  });

  it("throws before calling the model when the spec is invalid", async () => {
    await expect(generateMcpServerCode(validSpec({ tools: [] }), target)).rejects.toThrow(/fix the spec/i);
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("surfaces a stream error", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "", toolCalls: [], streamError: "connection lost", contentStarted: false });
    await expect(generateMcpServerCode(validSpec(), target)).rejects.toThrow(/connection lost/);
  });

  it("rejects a tool-call response instead of generated code", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: "", toolCalls: [{ id: "1", name: "x", arguments: "{}" }], streamError: null, contentStarted: true,
    });
    await expect(generateMcpServerCode(validSpec(), target)).rejects.toThrow(/tool call instead of generated code/i);
  });
});

describe("suggestedFileName", () => {
  it("derives a filename from the server name", () => {
    expect(suggestedFileName(validSpec())).toBe("weather-cli.mcp.ts");
  });
});

describe("generated artifact verification", () => {
  const executableArtifact = [
    "import { Server } from '@modelcontextprotocol/sdk/server/index.js';",
    "import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';",
    "import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js';",
    "const server = new Server({ name: 'weather-cli', version: '1.0.0' }, { capabilities: { tools: {} } });",
    "server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: [] }));",
    "server.setRequestHandler(CallToolRequestSchema, async () => { throw new Error('unknown tool'); });",
    "await server.connect(new StdioServerTransport());",
  ].join("\n");

  it("rejects TODO/placeholder source before execution", () => {
    expect(inspectGeneratedArtifact(`${executableArtifact}\n// TODO: wire the real call`)).toEqual(
      expect.arrayContaining([expect.stringMatching(/placeholder|todo/i)]),
    );
  });

  it("keeps generated code out of shell syntax by passing only base64 arguments", () => {
    const hostile = `${executableArtifact}\nconst inert = \"'; rm -rf /; #\";`;
    const command = buildGeneratedArtifactProbeCommand(hostile, validSpec());
    expect(command).not.toContain("rm -rf");
    expect(command).toMatch(/^node -e /);
  });

  it("does not accept a sandbox success without typecheck and runtime probe evidence", async () => {
    const runner = vi.fn().mockResolvedValue({
      runId: "run-1", isolation: "os_sandboxed", passed: true,
      stdoutExcerpt: "spec simulation only", stderrExcerpt: "",
    });
    const report = await probeGeneratedMcpArtifact(executableArtifact, validSpec(), runner);
    expect(report.clean).toBe(false);
    expect(report.typechecked).toBe(false);
    expect(report.executed).toBe(false);
  });

  it("requires matching typecheck, execution, and probed-tool count evidence", async () => {
    const runner = vi.fn().mockResolvedValue({
      runId: "run-2", isolation: "os_sandboxed", passed: true,
      stdoutExcerpt: "LITTLE_MONKEY_MCP_TYPECHECK_OK\nLITTLE_MONKEY_MCP_PROBE_OK:1\n", stderrExcerpt: "",
    });
    const report = await probeGeneratedMcpArtifact(executableArtifact, validSpec(), runner);
    expect(report.clean).toBe(true);
    expect(report.probedToolCount).toBe(1);
    expect(runner).toHaveBeenCalledWith(expect.any(String), {
      timeoutMs: 45_000,
      allowNetwork: false,
      approvedEnv: [],
    });
  });

  it("really typechecks and executes a generated MCP artifact through the probe harness", () => {
    const completeArtifact = [
      "import { Server } from '@modelcontextprotocol/sdk/server/index.js';",
      "import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';",
      "import { ListToolsRequestSchema, CallToolRequestSchema } from '@modelcontextprotocol/sdk/types.js';",
      "const server = new Server({ name: 'weather-cli', version: '1.0.0' }, { capabilities: { tools: {} } });",
      "server.setRequestHandler(ListToolsRequestSchema, async () => ({ tools: [{ name: 'get_forecast', description: 'Get forecast', inputSchema: { type: 'object', properties: { city: { type: 'string' } }, required: ['city'] } }] }));",
      "server.setRequestHandler(CallToolRequestSchema, async (request) => {",
      "  if (request.params.name !== 'get_forecast') throw new Error('unknown tool');",
      "  if (typeof request.params.arguments?.city !== 'string') throw new Error('city is required');",
      "  return { content: [{ type: 'text', text: request.params.arguments.city }] };",
      "});",
      "await server.connect(new StdioServerTransport());",
    ].join("\n");
    const probeDir = mkdtempSync(join(tmpdir(), "little-monkey-mcp-probe-"));
    try {
      const stdout = execSync(buildGeneratedArtifactProbeCommand(completeArtifact, validSpec()), {
        cwd: probeDir,
        encoding: "utf8",
        timeout: 30_000,
      });
      expect(stdout).toContain("LITTLE_MONKEY_MCP_TYPECHECK_OK");
      expect(stdout).toContain("LITTLE_MONKEY_MCP_PROBE_OK:1");
    } finally {
      rmSync(probeDir, { recursive: true, force: true });
    }
  }, 35_000);
});

import { beforeEach, describe, expect, it } from "vitest";

import { formatMcpCallToolResult, mcpToolDefs, resolveMcpToolName, type McpCallToolResult } from "./mcpTools";
import { useMcpStore, type McpServerInfo } from "../store/mcpStore";

function makeServer(overrides: Partial<McpServerInfo> = {}): McpServerInfo {
  return {
    id: "srv",
    label: "Test Server",
    transport: { type: "stdio", command: "echo", args: [], env: {} },
    enabled: true,
    toolAllowlist: null,
    timeoutSecs: null,
    status: "connected",
    error: null,
    tools: [{ name: "greet", description: "Say hello", inputSchema: { type: "object", properties: {} } }],
    instructions: null,
    hasHttpToken: false,
    ...overrides,
  };
}

beforeEach(() => {
  useMcpStore.setState({ servers: [] });
});

describe("mcpToolDefs", () => {
  it("maps a connected, enabled server's tools into namespaced ToolDefs", () => {
    useMcpStore.setState({ servers: [makeServer()] });

    const defs = mcpToolDefs();

    expect(defs).toHaveLength(1);
    expect(defs[0].function.name).toBe("mcp__srv__greet");
    expect(defs[0].function.description).toBe("[MCP: Test Server] Say hello");
    expect(defs[0].function.parameters).toEqual({ type: "object", properties: {} });
  });

  it("skips servers that are disabled or not connected", () => {
    useMcpStore.setState({
      servers: [
        makeServer({ id: "disabled", enabled: false }),
        makeServer({ id: "connecting", status: "connecting" }),
        makeServer({ id: "erroring", status: "error" }),
      ],
    });

    expect(mcpToolDefs()).toHaveLength(0);
  });

  it("honors a per-server tool_allowlist", () => {
    useMcpStore.setState({
      servers: [
        makeServer({
          toolAllowlist: ["other_tool"],
          tools: [
            { name: "greet", description: null, inputSchema: {} },
            { name: "other_tool", description: null, inputSchema: {} },
          ],
        }),
      ],
    });

    const defs = mcpToolDefs();
    expect(defs.map((d) => d.function.name)).toEqual(["mcp__srv__other_tool"]);
  });

  it("sanitizes server ids and tool names that don't match ^[a-zA-Z0-9_-]+$", () => {
    useMcpStore.setState({
      servers: [
        makeServer({
          id: "srv",
          tools: [{ name: "weird tool/name!", description: null, inputSchema: {} }],
        }),
      ],
    });

    const defs = mcpToolDefs();
    expect(defs[0].function.name).toMatch(/^[a-zA-Z0-9_-]+$/);
    expect(defs[0].function.name).toBe("mcp__srv__weird_tool_name_");
  });

  it("de-duplicates a composite-name collision with a numeric suffix", () => {
    useMcpStore.setState({
      servers: [
        makeServer({ id: "srv!", tools: [{ name: "greet", description: null, inputSchema: {} }] }),
        makeServer({ id: "srv?", tools: [{ name: "greet", description: null, inputSchema: {} }] }),
      ],
    });

    const names = mcpToolDefs().map((d) => d.function.name);
    expect(names).toEqual(["mcp__srv___greet", "mcp__srv___greet_2"]);
  });

  it("rebuilds its registry from scratch on every call", () => {
    useMcpStore.setState({ servers: [makeServer()] });
    mcpToolDefs();
    expect(resolveMcpToolName("mcp__srv__greet")).toEqual({ serverId: "srv", toolName: "greet" });

    useMcpStore.setState({ servers: [] });
    mcpToolDefs();
    expect(resolveMcpToolName("mcp__srv__greet")).toBeNull();
  });
});

describe("resolveMcpToolName", () => {
  it("returns null for a name mcpToolDefs never produced", () => {
    expect(resolveMcpToolName("mcp__unknown__tool")).toBeNull();
  });
});

describe("formatMcpCallToolResult", () => {
  it("concatenates text blocks", () => {
    const result: McpCallToolResult = { content: [{ type: "text", text: "hello" }, { type: "text", text: "world" }] };
    expect(formatMcpCallToolResult(result)).toBe("hello\nworld");
  });

  it("renders non-text blocks as placeholders", () => {
    const result: McpCallToolResult = {
      content: [
        { type: "image" },
        { type: "audio" },
        { type: "resource", resource: { uri: "file:///a.txt" } },
        { type: "resource_link", uri: "file:///b.txt" },
      ],
    };
    expect(formatMcpCallToolResult(result)).toBe(
      "[image]\n[audio]\n[resource: file:///a.txt]\n[resource: file:///b.txt]"
    );
  });

  it("maps isError:true into the existing {error} JSON shape", () => {
    const result: McpCallToolResult = { content: [{ type: "text", text: "boom" }], isError: true };
    expect(JSON.parse(formatMcpCallToolResult(result))).toEqual({ error: "boom" });
  });

  it("falls back to a generic error message when an error result has no text", () => {
    const result: McpCallToolResult = { content: [], isError: true };
    expect(JSON.parse(formatMcpCallToolResult(result))).toEqual({ error: "MCP tool call failed" });
  });
});

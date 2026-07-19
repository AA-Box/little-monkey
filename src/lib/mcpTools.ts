/**
 * Merges each connected + enabled MCP server's cached tools into the
 * OpenAI-style `ToolDef[]` handed to the model alongside the built-in
 * `TOOLS` (see `tools.ts`), namespaced `mcp__<serverId>__<toolName>`
 * (Claude Code's own convention) so a collision with a built-in tool name
 * or another server's same-named tool can never happen.
 *
 * Both segments are sanitized to match `^[a-zA-Z0-9_-]+$` — required by
 * OpenAI-compatible function calling, and necessary defensively besides:
 * `mcp_servers.json` is hand-editable and `load_config_impl` (Rust) doesn't
 * itself re-validate an id on load, and an external server's tool names are
 * untrusted input with no guaranteed shape. De-duplicated with a numeric
 * suffix on collision (e.g. two servers whose ids both sanitize to the same
 * string).
 *
 * The resulting composite name is NOT reliably reversible by splitting on
 * `__`: a sanitized/suffixed segment no longer matches the server's real id
 * or tool name, and even without sanitization an id or tool name containing
 * `__` itself would make a naive split ambiguous. So instead of re-parsing
 * the string, `agentLoop.ts`'s dispatch branch looks the exact
 * `{ serverId, toolName }` up via `resolveMcpToolName`, against the
 * `McpToolRegistry` `mcpToolDefs()` returns alongside the defs themselves —
 * mirrors `monkey-cli`'s `merged_tool_definitions`/`McpToolRegistry`, which
 * returns its own resolution table as a plain local value for exactly the
 * same reason: with the split pane, two turns (in different sessions) can
 * call `mcpToolDefs()` concurrently, and a *shared* module-level table would
 * let one turn's rebuild silently invalidate or repoint a name another
 * turn's model was already offered and is mid-call dispatching. Returning a
 * fresh table from every call and threading it through the one turn that
 * built it keeps each turn's tool-name resolution isolated from any other
 * turn's or Settings action's concurrent `mcpToolDefs()` call, the same way
 * `checkpointId`/`turnId` are freshly minted per turn rather than shared.
 */
import type { ToolDef } from './llamaClient';
import { useMcpStore } from '../store/mcpStore';

function sanitizeSegment(raw: string): string {
  const cleaned = raw.replace(/[^a-zA-Z0-9_-]/g, '_');
  return cleaned.length > 0 ? cleaned : '_';
}

/** Returns `base` if unused, otherwise `base_2`, `base_3`, ... — the first
 * suffix not already in `used` — and records whichever name it returns. */
function uniqueName(base: string, used: Set<string>): string {
  if (!used.has(base)) {
    used.add(base);
    return base;
  }
  let suffix = 2;
  while (used.has(`${base}_${suffix}`)) suffix += 1;
  const name = `${base}_${suffix}`;
  used.add(name);
  return name;
}

/** Composite tool name -> the exact server id + tool name it was built
 * from — one `mcpToolDefs()` call's worth, and only ever that call's. See
 * this module's doc comment for why this is a fresh value per call rather
 * than shared module state. */
export type McpToolRegistry = Map<string, { serverId: string; toolName: string }>;

/**
 * Builds the `ToolDef[]` for every connected, enabled server's cached tools,
 * honoring each server's `toolAllowlist` (when set) — servers that are
 * disabled, not currently connected, or configured but never connected are
 * silently skipped (their tools simply aren't offered, not an error). Also
 * returns the `McpToolRegistry` needed to resolve any of those composite
 * names back to `{ serverId, toolName }` (via `resolveMcpToolName`) — the
 * caller (`agentLoop.ts`, once per turn) is responsible for holding on to
 * both together for the lifetime of that turn.
 */
export function mcpToolDefs(): { defs: ToolDef[]; registry: McpToolRegistry } {
  const { servers } = useMcpStore.getState();
  const registry: McpToolRegistry = new Map();
  const used = new Set<string>();
  const defs: ToolDef[] = [];

  for (const server of servers) {
    if (!server.enabled || server.status !== 'connected') continue;
    const allowlist = server.toolAllowlist;

    for (const tool of server.tools) {
      if (allowlist && !allowlist.includes(tool.name)) continue;

      const base = `mcp__${sanitizeSegment(server.id)}__${sanitizeSegment(tool.name)}`;
      const name = uniqueName(base, used);
      registry.set(name, { serverId: server.id, toolName: tool.name });

      defs.push({
        type: 'function',
        function: {
          name,
          description: `[MCP: ${server.label}] ${tool.description ?? ''}`.trim(),
          parameters: tool.inputSchema,
        },
      });
    }
  }

  return { defs, registry };
}

/**
 * Looks up the exact `{ serverId, toolName }` a `mcpToolDefs()`-produced
 * composite tool name came from, against the `registry` that SAME
 * `mcpToolDefs()` call returned — `null` if `name` wasn't one of them (a
 * hallucinated call). Used by `agentLoop.ts`'s dispatch branch instead of
 * re-parsing the composite string — see this module's doc comment.
 */
export function resolveMcpToolName(registry: McpToolRegistry, name: string): { serverId: string; toolName: string } | null {
  return registry.get(name) ?? null;
}

/**
 * One content block in an rmcp `CallToolResult` — mirrors the wire shape of
 * rmcp 2.2's `ContentBlock` enum (`#[serde(tag = "type", rename_all =
 * "snake_case")]`: `text | image | audio | resource | resource_link`). Only
 * the fields `formatMcpCallToolResult` actually reads are declared.
 */
interface McpContentBlock {
  type: 'text' | 'image' | 'audio' | 'resource' | 'resource_link';
  text?: string;
  /** Present on a `resource_link` block. */
  uri?: string;
  /** Present on a `resource` block (an embedded resource). */
  resource?: { uri?: string };
}

/**
 * Mirrors the Rust `CallToolResult` struct exactly (camelCase — rmcp's own
 * `#[serde(rename_all = "camelCase")]`), as returned verbatim by the
 * `mcp_call_tool` Tauri command.
 */
export interface McpCallToolResult {
  content: McpContentBlock[];
  structuredContent?: unknown;
  isError?: boolean;
}

/**
 * Flattens an MCP tool call's result into the plain string `agentLoop.ts`
 * uses as a `tool` message's content: text blocks concatenated, non-text
 * blocks (image/audio/resource) rendered as a placeholder carrying just
 * their identifying detail — full image/resource passthrough is a later
 * enhancement (design doc phase 6). `isError: true` maps into the same
 * `{"error": ...}` JSON shape every other tool failure uses, so the model
 * can see and recover from it exactly like a shell/file error.
 */
export function formatMcpCallToolResult(result: McpCallToolResult): string {
  const parts = (result.content ?? []).map((block) => {
    switch (block.type) {
      case 'text':
        return block.text ?? '';
      case 'image':
        return '[image]';
      case 'audio':
        return '[audio]';
      case 'resource':
        return `[resource: ${block.resource?.uri ?? 'unknown'}]`;
      case 'resource_link':
        return `[resource: ${block.uri ?? 'unknown'}]`;
      default:
        return '[unknown content]';
    }
  });
  const text = parts.join('\n');

  if (result.isError) {
    return JSON.stringify({ error: text || 'MCP tool call failed' });
  }
  return text;
}

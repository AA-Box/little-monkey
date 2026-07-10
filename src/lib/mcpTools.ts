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
 * `{ serverId, toolName }` up via `resolveMcpToolName`, backed by a side
 * table this module rebuilds on every `mcpToolDefs()` call.
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
 * from. Rebuilt from scratch at the top of every `mcpToolDefs()` call, so
 * it always reflects the defs most recently offered to the model. */
const registry = new Map<string, { serverId: string; toolName: string }>();

/**
 * Builds the `ToolDef[]` for every connected, enabled server's cached tools,
 * honoring each server's `toolAllowlist` (when set) — servers that are
 * disabled, not currently connected, or configured but never connected are
 * silently skipped (their tools simply aren't offered, not an error).
 */
export function mcpToolDefs(): ToolDef[] {
  const { servers } = useMcpStore.getState();
  registry.clear();
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

  return defs;
}

/**
 * Looks up the exact `{ serverId, toolName }` a `mcpToolDefs()`-produced
 * composite tool name came from — `null` if `name` wasn't one of them (a
 * hallucinated call, or the server set changed since the defs offering it
 * were built). Used by `agentLoop.ts`'s dispatch branch instead of
 * re-parsing the composite string — see this module's doc comment.
 */
export function resolveMcpToolName(name: string): { serverId: string; toolName: string } | null {
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

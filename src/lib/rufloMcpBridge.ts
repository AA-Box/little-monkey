/** Operator-only, version-pinned Ruflo MCP bridge.
 * Registers an inert, disabled, deny-all stdio configuration. Does not run
 * npm, read credentials, make requests, grant tools, install mods or execute
 * shell commands. The user must explicitly review the MCP config, allowlist,
 * enable and connect through the existing host's authority checks.
 */
import type { McpServerEntry } from '../store/mcpStore';
export function prepareRufloMcpBridge(version:unknown):McpServerEntry{
  if(typeof version!=='string'||!/^([1-9]\d{0,2})\.(0|[1-9]\d{0,2})\.(0|[1-9]\d{0,2})(?:-(?:alpha|beta|rc)\.(?:0|[1-9]\d{0,3}))?$/.test(version))
    throw new Error('Use an exact verified Ruflo release version, such as 3.6.0, not latest or a version range.');
  return {
    id:'ruflo-operator-mcp',
    label:'Ruflo MCP (external, operator-reviewed)',
    transport:{type:'stdio',command:'npx',args:['-y','ruflo@'+version,'mcp','start'],env:{}},
    enabled:false,
    tool_allowlist:[], // null would grant EVERY exposed tool: always deny all.
    timeout_secs:30,
  };
}

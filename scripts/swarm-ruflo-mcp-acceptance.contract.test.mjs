import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {resolve} from 'node:path';
import {fileURLToPath} from 'node:url';
import {dirname} from 'node:path';
import {probeRufloMcp} from './swarm-ruflo-mcp-acceptance.mjs';
const base=dirname(fileURLToPath(import.meta.url));
const fake=resolve(base,'swarm-ruflo-mcp-fake-server.mjs');
describe('real STDIO MCP interoperability probe (controlled executable, no tools/call)',()=>{
 it('negotiates an MCP server and discovers bounded tools without executing any',async()=>{
  const out=await probeRufloMcp(process.execPath,[fake]);
  assert.equal(out.status,'mcp-handshake-and-discovery-passed');
  assert.equal(out.toolCount,2);
  assert.equal(out.executedTools,0);
  assert.equal(out.groups.swarm,1);
  assert.equal(out.groups.memory,1);
 });
 it('refuses missing executable and unbounded command arguments',async()=>{
  await assert.rejects(probeRufloMcp('node',[fake]),/absolute/);
  await assert.rejects(probeRufloMcp(process.execPath,['A'.repeat(251)]),/bounded/);
 });
});

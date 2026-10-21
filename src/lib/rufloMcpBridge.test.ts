import {describe,it,expect} from 'vitest';
import {prepareRufloMcpBridge} from './rufloMcpBridge';
describe('native Ruflo MCP interop staging',()=>{
 it('pins exactly one npm release and denies all tools by default',()=>{
  const entry=prepareRufloMcpBridge('3.6.0');
  expect(entry.id).toBe('ruflo-operator-mcp');
  expect(entry.transport).toEqual({type:'stdio',command:'npx',
    args:['-y','ruflo@3.6.0','mcp','start'],env:{}});
  expect(entry.enabled).toBe(false);
  expect(entry.tool_allowlist).toEqual([]);
 });
 it('rejects unpinned, wildcard and shell-injection versions',()=>{
  for(const version of ['','latest','*','^3.6.0','3.6.0; echo bad','3.6.0 && true',
    '3.6','v3.6.0','../../../npm','0.0.1','3.6.0+build','3.6.0-foo']){
    expect(()=>prepareRufloMcpBridge(version)).toThrow();
  }
  expect(prepareRufloMcpBridge('3.6.1-rc.2').enabled).toBe(false);
 });
});

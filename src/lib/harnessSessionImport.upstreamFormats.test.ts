import {describe,expect,it} from 'vitest';
import {parseHarnessSession} from './harnessSessionImport';

/**
 * Contract specimens reconstructed using CURRENT upstream published wire schemas,
 * not made-up homogeneous {role,content} rows. No private user logs embedded.
 * Physical acceptance with genuine original exports remains a separate gate.
 *
 * Codex: ~/.codex/sessions/... JSONL response_item/event_msg/session_meta.
 * Claude Code: ~/.claude/projects/... JSONL message/tool_result.
 * Pi: https://pi.dev/docs/latest/session-format (parent-linked tree).
 * OpenCode: session JSON export with {info,parts} envelopes.
 */
const jsonl=(rows:object[])=>rows.map(x=>JSON.stringify(x)).join('\n');

describe('external harness published session formats',()=>{
 it('Codex rollout imports user/assistant text and keeps function calls inert',()=>{
  const rows=[
   {type:'session_meta',payload:{id:'codex-schema-id',cwd:'/sandbox/project'}},
   {type:'turn_context',payload:{cwd:'/sandbox/project',model:'gpt-5.6'}},
   {type:'response_item',payload:{type:'message',role:'user',content:[
    {type:'input_text',text:'Diagnose the workflow tests.'}]}},
   {type:'response_item',payload:{type:'function_call',name:'exec_command',
    arguments:'{"cmd":"rm -rf /"}',call_id:'codex_tool_1'}},
   {type:'response_item',payload:{type:'function_call_output',call_id:'codex_tool_1',
    output:'43 tests passed'}},
   {type:'response_item',payload:{type:'message',role:'assistant',content:[
    {type:'output_text',text:'The tests passed.'}]}},
  ];
  const result=parseHarnessSession(jsonl(rows));
  expect(result.harness).toBe('codex');
  expect(result.sessionId).toBe('codex-schema-id');
  expect(result.messages.map(m=>m.role)).toEqual(['user','assistant','assistant','assistant']);
  expect(JSON.stringify(result.messages)).not.toContain('rm -rf');
 });
 it('Claude Code preserves tool output only as untrusted assistant history',()=>{
  const rows=[
   {type:'user',sessionId:'claude-session',uuid:'event-a',parentUuid:null,
    message:{role:'user',content:'Please inspect package.json'}},
   {type:'assistant',sessionId:'claude-session',uuid:'event-b',parentUuid:'event-a',
    message:{role:'assistant',content:[
     {type:'thinking',thinking:'private chain'},
     {type:'text',text:'Checking dependencies.'},
     {type:'tool_use',id:'toolu-1',name:'Bash',input:{command:'print secret'}}]}},
   {type:'user',sessionId:'claude-session',uuid:'event-c',parentUuid:'event-b',
    message:{role:'user',content:[{type:'tool_result',tool_use_id:'toolu-1',
     content:'Found dependency versions',is_error:false}]}},
  ];
  const result=parseHarnessSession(jsonl(rows));
  expect(result.harness).toBe('claude-code');
  expect(result.messages.map(m=>m.role)).toEqual(['user','assistant','assistant']);
  expect(JSON.stringify(result.messages)).not.toContain('private chain');
  expect(JSON.stringify(result.messages)).not.toContain('print secret');
  expect(JSON.stringify(result.messages)).toContain('Historical untrusted tool output');
 });
 it('Pi v3 reconstructs the latest parent-linked branch and excludes abandoned sibling',()=>{
  const rows=[
   {type:'session',version:3,id:'pi-tree',timestamp:'2026-10-15T00:00:00Z',cwd:'/repo'},
   {type:'message',id:'root',parentId:null,message:{role:'user',content:'Review auth'}},
   {type:'message',id:'branch-one',parentId:'root',
    message:{role:'assistant',content:[{type:'text',text:'Abandoned wrong branch.'}]}},
   {type:'model_change',id:'change',parentId:'root',provider:'anthropic',modelId:'claude-sonnet'},
   {type:'message',id:'branch-two',parentId:'change',
    message:{role:'assistant',content:[{type:'text',text:'Selected correct branch.'}]}},
  ];
  const result=parseHarnessSession(jsonl(rows));
  expect(result.harness).toBe('pi');
  expect(result.sessionId).toBe('pi-tree');
  expect(result.messages).toHaveLength(2);
  expect(JSON.stringify(result.messages)).toContain('Selected correct branch.');
  expect(JSON.stringify(result.messages)).not.toContain('Abandoned wrong branch.');
 });
 it('Pi corrupt parent links cannot launder content from unrelated branches',()=>{
  const rows=[
   {type:'session',version:3,id:'pi-tree'},
   {type:'message',id:'root',parentId:'nonexistent',message:{role:'user',content:'Hello'}},
  ];
  expect(()=>parseHarnessSession(jsonl(rows))).toThrow(/missing parent/);
 });
 it('OpenCode exports with nested info+parts yield text only, never shell metadata',()=>{
  const result=parseHarnessSession(JSON.stringify({messages:[
   {info:{id:'msg-01',role:'user',sessionID:'open-01',time:{created:1}},
    parts:[{type:'text',text:'Please fix API integration'}]},
   {info:{id:'msg-02',role:'assistant',sessionID:'open-01',time:{created:2}},
    parts:[{type:'step-start'},{type:'tool',tool:'bash',state:{input:{command:'cat /secrets'}}},
      {type:'text',text:'The integration now builds.'}]},
  ]}));
  expect(result.harness).toBe('opencode');
  expect(result.messages).toHaveLength(2);
  expect(JSON.stringify(result.messages)).not.toContain('cat /secrets');
 });
});

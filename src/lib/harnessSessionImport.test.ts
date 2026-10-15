import {describe,expect,it} from 'vitest';
import {parseHarnessSession} from './harnessSessionImport';

describe('coding-agent harness session continuation',()=>{
  it('parses real Codex rollout envelopes and keeps tools inert',()=>{
    const entries=[
      {type:'session_meta',payload:{id:'codex-session',cwd:'/private/untrusted'}},
      {type:'response_item',payload:{type:'message',role:'user',content:[{type:'input_text',text:'Check login.ts'}]}},
      {type:'response_item',payload:{type:'function_call',name:'run_shell',arguments:'rm -rf .',call_id:'dangerous'}},
      {type:'response_item',payload:{type:'function_call_output',output:'test result OK',call_id:'dangerous'}},
      {type:'response_item',payload:{type:'message',role:'assistant',content:[{type:'output_text',text:'Checked login.ts'}]}},
    ].map(x=>JSON.stringify(x)).join('\n');
    const parsed=parseHarnessSession(entries);
    expect(parsed.harness).toBe('codex');
    expect(parsed.messages.map(x=>x.role)).toEqual(['user','assistant','assistant','assistant']);
    expect(parsed.messages[1].content).toContain('not executable');
    expect(JSON.stringify(parsed.messages)).not.toContain('rm -rf');
    expect(JSON.stringify(parsed.messages)).not.toContain('/private/untrusted');
    expect(parsed.messages.every(x=>!x.tool_calls&&!x.tool_call_id)).toBe(true);
  });

  it('imports Claude Code JSONL text, safely omitting internal thinking and duplicate message ids',()=>{
    const entries=[
      {type:'user',uuid:'a',sessionId:'claude-1',message:{role:'user',content:[{type:'text',text:'Build feature X'}]}},
      {type:'assistant',uuid:'b',sessionId:'claude-1',message:{role:'assistant',content:[
        {type:'thinking',thinking:'secret scratchpad'},
        {type:'text',text:'Here is my plan'},
        {type:'tool_use',name:'Bash',id:'tool-1',input:{command:'echo ok'}},
      ]}},
      {type:'assistant',uuid:'b',sessionId:'claude-1',message:{role:'assistant',content:[{type:'text',text:'Here is my plan'}]}},
    ].map(x=>JSON.stringify(x)).join('\n');
    const parsed=parseHarnessSession(entries);
    expect(parsed.harness).toBe('claude-code');
    expect(parsed.messages).toHaveLength(2);
    expect(parsed.messages[1].content).toContain('historical only');
    expect(JSON.stringify(parsed.messages)).not.toContain('secret scratchpad');
    expect(JSON.stringify(parsed.messages)).not.toContain('echo ok');
    expect(parsed.skipped).toBeGreaterThan(0);
  });

  it('handles Pi and OpenCode exported transcripts as a new, non-executable chat',()=>{
    const pi=parseHarnessSession([
      {type:'session',id:'pi-1',version:3},
      {type:'message',message:{role:'user',content:[{type:'text',text:'Hello Pi'}]}},
      {type:'message',message:{role:'assistant',content:[{type:'text',text:'Hi there'}]}},
    ].map(x=>JSON.stringify(x)).join('\n'));
    expect(pi.harness).toBe('pi');
    expect(pi.messages).toHaveLength(2);
    expect(pi.title).toBe('Hello Pi');

    const open=parseHarnessSession(JSON.stringify({messages:[
      {info:{role:'user'},parts:[{type:'text',text:'Fix the Go test'}]},
      {info:{role:'assistant'},parts:[{type:'text',text:'Fixed tests'}]},
    ]}));
    expect(open.harness).toBe('opencode');
    expect(open.messages).toHaveLength(2);
  });

  it('refuses combined multi-session logs and rejects unsupported/invalid content',()=>{
    expect(()=>parseHarnessSession([
      {type:'user',sessionId:'a',message:{role:'user',content:'A'}},
      {type:'user',sessionId:'b',message:{role:'user',content:'B'}},
    ].map(JSON.stringify).join('\n'))).toThrow(/multiple sessions/);
    expect(()=>parseHarnessSession('{not-valid-json')).toThrow();
    expect(()=>parseHarnessSession(JSON.stringify({unrecognized:true}))).toThrow(/Unsupported/);
    expect(()=>parseHarnessSession('x'.repeat(16*1024*1024+1))).toThrow(/16 MiB/);
  });

  it('does not treat historical tool results as executable tool calls',()=>{
    const input=[
      {type:'user',sessionId:'c',message:{role:'user',content:'Run tests'}},
      {type:'assistant',sessionId:'c',message:{role:'assistant',content:[
        {type:'tool_use',id:'old-tool',name:'shell',input:{command:'delete important file'}},
      ]}},
      {type:'user',sessionId:'c',message:{role:'user',content:[
        {type:'tool_result',tool_use_id:'old-tool',content:'old output'},
      ]}},
    ].map(JSON.stringify).join('\n');
    const parsed=parseHarnessSession(input);
    expect(parsed.messages).toHaveLength(3);
    expect(parsed.messages.every(x=>x.tool_calls===undefined && x.tool_call_id===undefined)).toBe(true);
    expect(JSON.stringify(parsed.messages)).not.toContain('delete important file');
    expect(JSON.stringify(parsed.messages)).toContain('Historical untrusted tool output');
  });
});

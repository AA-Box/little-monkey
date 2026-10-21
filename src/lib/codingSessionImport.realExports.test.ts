import {describe,it,expect} from 'vitest';
import {readFileSync} from 'node:fs';
import {parseCodingSession,type CodingFormat} from './codingSessionImport';

/**
 * This is a genuine-export acceptance gate, not a fixture pretending to be
 * a real agent. Files stay local and are never printed or uploaded. CI skips
 * it until the operator supplies actual exports from each installed client.
 */
const formats=[
 ['claude-code','LM_REAL_CLAUDE_CODE_JSONL'],
 ['codex','LM_REAL_CODEX_JSONL'],
 ['pi','LM_REAL_PI_JSONL'],
 ['opencode','LM_REAL_OPENCODE_JSON'],
 ['cursor','LM_REAL_CURSOR_AGENT_STREAM_JSON'],
] as const;
const mandatory=process.env.LM_REQUIRE_REAL_CODING_EXPORTS==='1';
describe('real coding-agent transcript acceptance',()=>{
 for(const [format,env] of formats){
  const path=process.env[env];
  (mandatory||path?it:it.skip)('imports genuine '+format+' export without executable calls',()=>{
   if(!path)throw Error('Missing required real export: '+env);
   const bytes=readFileSync(path);
   expect(bytes.byteLength).toBeGreaterThan(0);
   const decoded=new TextDecoder('utf-8',{fatal:true}).decode(bytes);
   const imported=parseCodingSession(decoded);
   expect(imported?.format).toBe(format as CodingFormat);
   expect(imported?.messages.some(m=>m.role==='user')).toBe(true);
   expect(imported?.messages.some(m=>m.role==='assistant')).toBe(true);
   expect(imported?.messages.every(m=>!m.tool_calls && !m.tool_call_id)).toBe(true);
   expect(imported?.messages.every(m=>typeof m.content==='string')).toBe(true);
  });
 }
});

import {describe,it,expect} from 'vitest';
import {readFileSync} from 'node:fs';
import {parseHarnessSession,type Harness} from './harnessSessionImport';

/**
 * PHYSICAL external-harness acceptance: NEVER fabricate a green result.
 * Source files are explicit operator-provided real exports and are NEVER
 * uploaded to CI or printed. Run with LM_REQUIRE_REAL_HARNESS=1 and all four
 * paths. Default CI skips these tests, while the published-schema specimens
 * run on every commit in harnessSessionImport.upstreamFormats.test.ts.
 */
const real=[
  ['codex','LM_REAL_CODEX_JSONL'],
  ['claude-code','LM_REAL_CLAUDE_CODE_JSONL'],
  ['pi','LM_REAL_PI_JSONL'],
  ['opencode','LM_REAL_OPENCODE_JSON'],
] as const;
const required=process.env.LM_REQUIRE_REAL_HARNESS==='1';
describe('physical real external coding-agent export acceptance',()=>{
  for(const [kind,environment] of real) {
    const path=process.env[environment];
    (required || path ? it : it.skip)('accepts a real '+kind+' export and imports useful inert text',()=>{
      if(!path)throw new Error('Missing required real-export file path in '+environment);
      const bytes=readFileSync(path);
      expect(bytes.byteLength).toBeGreaterThan(0);
      const report=parseHarnessSession(new TextDecoder('utf-8',{fatal:true}).decode(bytes));
      expect(report.harness).toBe(kind as Harness);
      expect(report.messages.length).toBeGreaterThan(0);
      expect(report.messages.every(m=>m.role==='user'||m.role==='assistant')).toBe(true);
      expect(report.messages.every(m=>m.tool_calls===undefined&&m.tool_call_id===undefined)).toBe(true);
      expect(report.messages.some(m=>m.role==='user')).toBe(true);
      expect(report.messages.some(m=>m.role==='assistant')).toBe(true);
    });
  }
});

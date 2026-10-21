import { describe, expect, it } from 'vitest';
import { ContextExposureTracker } from './contextExposure';

describe('agent tool context exposure',()=>{
  it('compacts exact duplicate reads but not changed source',()=>{
    const t=new ContextExposureTracker();
    expect(t.observe('s1:t1','read_file',{path:'a.ts'},'first')).toBe('first');
    const repeat=JSON.parse(t.observe('s1:t1','read_file',{path:'a.ts'},'first'));
    expect(repeat.already_exposed).toBe(true);
    expect(t.observe('s1:t1','read_file',{path:'a.ts'},'second')).toBe('second');
    expect(t.observe('s1:t2','read_file',{path:'a.ts'},'first')).toBe('first');
  });
  it('does not share exposure between agents, worktrees or grep queries',()=>{
    const t=new ContextExposureTracker();
    expect(t.observe('agent1','grep',{pattern:'one'},'[1]')).toBe('[1]');
    expect(t.observe('agent2','grep',{pattern:'one'},'[1]')).toBe('[1]');
    expect(t.observe('agent1','grep',{pattern:'two'},'[1]')).toBe('[1]');
    expect(JSON.parse(t.observe('agent1','grep',{pattern:'one'},'[1]')).already_exposed).toBe(true);
  });
  it('never conceals errors, oversize results or content after invalidation',()=>{
    const t=new ContextExposureTracker(2,30);
    for(let i=0;i<2;i++) expect(t.observe('a','read_file',{path:'a'},'{"error":"Permission denied"}'))
      .toBe('{"error":"Permission denied"}');
    const long='X'.repeat(31);
    expect(t.observe('a','read_file',{path:'a'},long)).toBe(long);
    expect(t.observe('a','read_file',{path:'a'},long)).toBe(long);
    expect(t.observe('a','read_file',{path:'a'},'ok')).toBe('ok');
    t.invalidate('a');
    expect(t.observe('a','read_file',{path:'a'},'ok')).toBe('ok');
  });
  it('evicts bounded entries and keeps the newest',()=>{
    const t=new ContextExposureTracker(2);
    t.observe('a','read_file',{path:'one'},'1');
    t.observe('a','read_file',{path:'two'},'2');
    t.observe('a','read_file',{path:'three'},'3');
    expect(t.observe('a','read_file',{path:'one'},'1')).toBe('1');
  });
});

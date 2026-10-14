import {beforeEach, describe, expect, it, vi} from 'vitest';
import {invoke} from '@tauri-apps/api/core';
import {
  captureConversationMemory, memoryExtractionMessages, validateMemoryProposals,
} from './conversationAutoMemory';
import type {MemoryFact} from '../store/rulesStore';

vi.mock('@tauri-apps/api/core', () => ({invoke:vi.fn()}));

const agent = (id:string,text:string, pinned=false):MemoryFact => ({
  id,text,source:'agent',created_at:'2026-10-13T00:00:00.000Z',pinned,
});
const human = (id:string,text:string):MemoryFact => ({
  id,text,source:'user',created_at:'2026-10-13T00:00:00.000Z',
});
beforeEach(() => { vi.mocked(invoke).mockReset().mockResolvedValue({id:'saved-id'}); });

describe('automatic ordinary conversation memory',()=>{
  it('takes evidence exclusively from exact user text, never model assertions',()=>{
    const user='I prefer Kotlin for Android projects.';
    const result=validateMemoryProposals({facts:[
      {operation:'add',evidence:'I prefer Kotlin for Android projects.',text:'User prefers Kotlin for Android development.'},
      {operation:'add',evidence:'Assistant believes Java is better',text:'User prefers Java.'},
    ]},user,[]);
    expect(result).toEqual([{
      operation:'add',evidence:user,text:'User prefers Kotlin for Android development.',
    }]);
  });

  it('saves a fact with agent provenance and source-turn attribution via guarded Rust command', async()=>{
    const user='My preferred Android language is Kotlin.';
    const model=vi.fn().mockResolvedValue(JSON.stringify({facts:[{
      operation:'add',evidence:user,text:'Prefers Kotlin for Android.',
    }]}));
    const response=await captureConversationMemory({
      userText:user,turnId:'uuid-turn-1',expectedWorkspace:'/projects/app',
      facts:[],callModel:model,
    });
    expect(response).toMatchObject({added:1,corrected:0,warnings:[]});
    expect(invoke).toHaveBeenCalledWith('memory_auto_apply',{
      text:'Prefers Kotlin for Android.',turnId:'uuid-turn-1',
      operation:'add',existingId:null,expectedOldText:null,
      expectedWorkspace:'/projects/app',
    });
    expect(model).toHaveBeenCalledTimes(1);
    const wire = memoryExtractionMessages(user, []);
    expect(wire).toHaveLength(2);
    expect(wire[1].content).toContain(user);
  });

  it('consolidates a clear correction but never overwrites pinned or user memories',async()=>{
    const user='I now prefer Kotlin, not Java, for Android work.';
    const facts=[agent('old','Prefers Java for Android.'),agent('pinned','Prefers Java for Android.',true),
      human('human','Prefers Java for Android.')];
    const model=vi.fn().mockResolvedValue(JSON.stringify({facts:[
      {operation:'replace',existing_id:'old',evidence:user,text:'Prefers Kotlin for Android.'},
      {operation:'replace',existing_id:'pinned',evidence:user,text:'Prefers Kotlin over Java.'},
      {operation:'replace',existing_id:'human',evidence:user,text:'Prefers Kotlin only.'},
    ]}));
    const report=await captureConversationMemory({
      userText:user,turnId:'turn-2',expectedWorkspace:null,facts,callModel:model,
    });
    expect(report.corrected).toBe(1);
    expect(report.ignored).toBe(2);
    expect(invoke).toHaveBeenCalledTimes(1);
    expect(invoke).toHaveBeenCalledWith('memory_auto_apply',expect.objectContaining({
      operation:'replace',existingId:'old',expectedOldText:'Prefers Java for Android.',
    }));
  });

  it('fails closed if a backend write cannot be confirmed',async()=>{
    vi.mocked(invoke).mockRejectedValue(new Error('workspace changed'));
    const user='My editor of choice is Neovim.';
    const result=await captureConversationMemory({userText:user,turnId:'turn-3',
      expectedWorkspace:'/workspace',facts:[],
      callModel:async()=>JSON.stringify({facts:[{
        operation:'add',evidence:user,text:'Prefers Neovim.',
      }]}),
    });
    expect(result.added).toBe(0);
    expect(result.warnings[0]).toContain('workspace changed');
  });

  it('does not send possible secrets, tool commands or very long requests to an extraction model',async()=>{
    const model=vi.fn();
    for (const userText of ['/remember abc','password: hunter2','x'.repeat(4001)]) {
      const result=await captureConversationMemory({
        userText,turnId:'turn-4',expectedWorkspace:null,facts:[],callModel:model,
      });
      expect(result.added).toBe(0);
    }
    expect(model).not.toHaveBeenCalled();
  });

  it('rejects malformed, excessive, and injected model proposals',()=>{
    expect(()=>validateMemoryProposals({},'user',[])).toThrow(/model/i);
    expect(()=>validateMemoryProposals({facts:[1,2,3,4]},'User prefers Swift.',[])).toThrow(/Too many/);
    expect(validateMemoryProposals({facts:[{
      operation:'add',evidence:'password: abc12345',text:'password: abc12345',
    }]},'My password: abc12345',[])).toEqual([]);
  });
});

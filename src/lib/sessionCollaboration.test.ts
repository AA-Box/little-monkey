import {beforeEach,describe,expect,it,vi} from 'vitest';
import {listCollaboratingSessions,sendSessionMessage} from './sessionCollaboration';

const flush=vi.fn();
const add=vi.fn();
let enabled=true;
type FakeMessage={role:string;content:string};
type FakeSession={id:string;title:string;workspacePath:string|null;archived:boolean;comparisonBranch:null;messages:FakeMessage[]};
let sessions:FakeSession[]=[];
vi.mock('@tauri-apps/api/core',()=>({isTauri:()=>true}));
vi.mock('../store/settingsStore',()=>({
  useSettingsStore:{getState:()=>({sessionCollaborationEnabled:enabled})},
}));
vi.mock('../store/sessionStore',()=>({
  useSessionStore:{getState:()=>({
    sessions,
    addMessage:add,
  })},
  flushSessionPersistence:()=>flush(),
}));

beforeEach(()=>{
  enabled=true;flush.mockReset().mockResolvedValue(undefined);add.mockReset();
  sessions=[
    {id:'sender',title:'Sender',workspacePath:'/repo/a',archived:false,comparisonBranch:null,messages:[]},
    {id:'target',title:'Target',workspacePath:'/repo/a',archived:false,comparisonBranch:null,messages:[]},
    {id:'other',title:'Other project',workspacePath:'/repo/b',archived:false,comparisonBranch:null,messages:[]},
    {id:'global',title:'No workspace',workspacePath:null,archived:false,comparisonBranch:null,messages:[]},
  ];
  add.mockImplementation((id:string,message:FakeMessage)=>{
    const session=sessions.find(s=>s.id===id);if(session)session.messages.push(message);
  });
});

describe('opt-in same-workspace desktop agent collaboration',()=>{
  it('does not disclose cross-workspace or global chat identities',()=>{
    expect(listCollaboratingSessions('sender')).toEqual([
      {id:'target',title:'Target',workspace:'/repo/a'},
    ]);
    expect(()=>listCollaboratingSessions('global')).toThrow(/workspace/);
  });

  it('sends exactly one fenced notice, then awaits the durable session write',async()=>{
    const args={senderSessionId:'sender',recipientSessionId:'target',toolCallId:'call-1',
      turnId:'turn-1',text:'Please review auth.ts'};
    const first=await sendSessionMessage(args);
    expect(first).toEqual({messageId:'turn-1:call-1',persisted:true,recipientSessionId:'target'});
    expect(add).toHaveBeenCalledTimes(1);
    expect(sessions[1].messages[0].role).toBe('assistant');
    expect(sessions[1].messages[0].content).toContain('untrusted');
    expect(flush).toHaveBeenCalledTimes(1);
    await sendSessionMessage(args);
    expect(add).toHaveBeenCalledTimes(1);
    expect(flush).toHaveBeenCalledTimes(2);
  });

  it('refuses unknown, cross-workspace, self and archived recipients',async()=>{
    const args={senderSessionId:'sender',recipientSessionId:'other',toolCallId:'call-1',
      turnId:'turn-1',text:'Unsafe'};
    await expect(sendSessionMessage(args)).rejects.toThrow(/same authorized workspace/);
    await expect(sendSessionMessage({...args,recipientSessionId:'sender'})).rejects.toThrow();
    sessions[1].archived=true;
    await expect(sendSessionMessage({...args,recipientSessionId:'target'})).rejects.toThrow();
    expect(add).not.toHaveBeenCalled();
    expect(flush).not.toHaveBeenCalled();
  });

  it('rejects disabled capability, oversized messages and invalid IDs',async()=>{
    const args={senderSessionId:'sender',recipientSessionId:'target',toolCallId:'call-1',
      turnId:'turn-1',text:'hello'};
    enabled=false;
    await expect(sendSessionMessage(args)).rejects.toThrow(/disabled/);
    enabled=true;
    await expect(sendSessionMessage({...args,text:'x'.repeat(1601)})).rejects.toThrow(/1600/);
    await expect(sendSessionMessage({...args,turnId:'../../arbitrary'})).rejects.toThrow(/Invalid turn/);
  });

  it('does not falsely acknowledge failed session persistence',async()=>{
    flush.mockRejectedValueOnce(new Error('disk quota exhausted'));
    const args={senderSessionId:'sender',recipientSessionId:'target',toolCallId:'call-1',
      turnId:'turn-1',text:'recheck'};
    await expect(sendSessionMessage(args)).rejects.toThrow(/disk quota exhausted/);
    expect(add).toHaveBeenCalledTimes(1);
    // File-store hydration/retry will settle pending local notice, rather
    // than creating a duplicate on the same tool-call identity.
    const confirmation=await sendSessionMessage(args);
    expect(confirmation.persisted).toBe(true);
    expect(add).toHaveBeenCalledTimes(1);
  });
});

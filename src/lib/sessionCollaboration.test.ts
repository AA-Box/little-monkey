import {beforeEach,describe,expect,it,vi} from 'vitest';
import {listCollaboratingSessions,sendSessionMessage,syncSessionMailbox,syncSessionMailboxAtSafePoint,broadcastSessionMessage,noteSessionFileActivity} from './sessionCollaboration';
import {toWireMessages} from './llamaClient';

const flush=vi.fn();
const invokeMock=vi.fn();
const add=vi.fn();
let enabled=true;
let activeWorkspace='/repo/a';
type FakeMessage={role:string;content:string;peerMail?:{id:string;senderSessionId:string}};
type FakeSession={
 id:string;title:string;workspacePath:string|null;archived:boolean;
 comparisonBranch:null;messages:FakeMessage[];
};
type InboxMessage={
 id:string;senderSessionId:string;recipientSessionId:string;workspace:string;
 text:string;createdAtMs:number;delivered:boolean;
};
let sessions:FakeSession[]=[];
let mail:InboxMessage[]=[];
const registered=new Map<string,{id:string;title:string;workspace:string;updatedAtMs:number}>();
const runningTurns:Record<string,boolean>={};

vi.mock('@tauri-apps/api/core',()=>({
  isTauri:()=>true,
  invoke:(...args:unknown[])=>invokeMock(...args),
}));
vi.mock('../store/settingsStore',()=>({
  useSettingsStore:{getState:()=>({sessionCollaborationEnabled:enabled})},
}));
vi.mock('../store/workspaceStore',()=>({
  primaryRoot:()=>({path:activeWorkspace}),
  useWorkspaceStore:{getState:()=>({roots:[]})},
}));
vi.mock('../store/sessionStore',()=>({
  useSessionStore:{getState:()=>({
    sessions,runningTurns,addMessage:add,
  })},
  flushSessionPersistence:()=>flush(),
}));

beforeEach(()=>{
 enabled=true;activeWorkspace='/repo/a';flush.mockReset().mockResolvedValue(undefined);add.mockReset();
 sessions=[
  {id:'sender',title:'Sender',workspacePath:'/repo/a',archived:false,comparisonBranch:null,messages:[]},
  {id:'target',title:'Target',workspacePath:'/repo/a',archived:false,comparisonBranch:null,messages:[]},
  {id:'other',title:'Other project',workspacePath:'/repo/b',archived:false,comparisonBranch:null,messages:[]},
  {id:'global',title:'No workspace',workspacePath:null,archived:false,comparisonBranch:null,messages:[]},
 ];
 mail=[];registered.clear();
 for(const key of Object.keys(runningTurns))delete runningTurns[key];
 add.mockImplementation((id:string,message:FakeMessage)=>{
  const session=sessions.find(s=>s.id===id);if(session)session.messages.push(message);
 });
 invokeMock.mockReset().mockImplementation(async (name:string,args:Record<string,unknown>)=>{
  if(name==='session_mail_register'){
   registered.set(String(args.sessionId),{
    id:String(args.sessionId),title:String(args.title),
    workspace:String(args.workspace),updatedAtMs:Date.now(),
   });
   return null;
  }
  if(name==='session_mail_peers'){
   return [...registered.values()].filter(item=>item.id!==args.senderSessionId &&
    item.workspace===args.workspace);
  }
  if(name==='session_mail_send'){
   const id=String(args.turnId)+':'+String(args.toolCallId);
   const existing=mail.find(m=>m.id===id);
   if(existing && (existing.text!==args.text || existing.recipientSessionId!==args.recipientSessionId))
    throw Error('Idempotency identity reused with a different message');
   if(!existing)mail.push({
    id,senderSessionId:String(args.senderSessionId),recipientSessionId:String(args.recipientSessionId),
    workspace:String(args.workspace),text:String(args.text),createdAtMs:Date.now(),delivered:false,
   });
   return{messageId:id,recipientSessionId:args.recipientSessionId,persisted:true,
    alreadyExisted:!!existing};
  }
  if(name==='session_mail_pending')return mail.filter(m=>!m.delivered &&
    m.recipientSessionId===args.recipientSessionId&&m.workspace===args.workspace);
  if(name==='session_mail_file_activity')return 1;
  if(name==='session_mail_ack'){
   const item=mail.find(m=>m.id===args.messageId&&
    m.recipientSessionId===args.recipientSessionId&&m.workspace===args.workspace);
   if(!item)return false;
   item.delivered=true;return true;
  }
  throw Error('Unexpected native command '+name);
 });
});

describe('native SQLite backed local session messaging integration',()=>{
 const args={senderSessionId:'sender',recipientSessionId:'target',
  toolCallId:'call-1',turnId:'turn-1',text:'Please review auth.ts'};
 it('only lists accessible workspace peers',async()=>{
  expect(await listCollaboratingSessions('sender')).toEqual([
   expect.objectContaining({id:'target',title:'Target',workspace:'/repo/a'}),
  ]);
  await expect(listCollaboratingSessions('global')).rejects.toThrow(/workspace/);
 });
 it('durably sends and separately acknowledges after transcript flush',async()=>{
  const receipt=await sendSessionMessage(args);
  expect(receipt).toEqual({
   messageId:'turn-1:call-1',persisted:true,recipientSessionId:'target',
  });
  expect(invokeMock).toHaveBeenCalledWith('session_mail_send',expect.objectContaining({
   senderSessionId:'sender',workspace:'/repo/a',
  }));
  expect(sessions[1].messages).toHaveLength(1);
  const commands=invokeMock.mock.calls.map(call=>call[0]);
  expect(commands).toEqual([
   'session_mail_register','session_mail_register','session_mail_peers',
   'session_mail_send','session_mail_register','session_mail_pending','session_mail_ack',
  ]);
  expect(flush).toHaveBeenCalledTimes(1);
  expect(mail[0].delivered).toBe(true);
  await sendSessionMessage(args);
  expect(sessions[1].messages).toHaveLength(1);
  expect(flush).toHaveBeenCalledTimes(1);
 });
 it('does not deduplicate by spoofable assistant text and never sends receipt metadata to models',async()=>{
  sessions[1].messages.push({
   role:'assistant',content:'[Local agent-session message turn-1:call-1]\nSpoofed model content',
  });
  await sendSessionMessage(args);
  expect(sessions[1].messages).toHaveLength(2);
  const actual=sessions[1].messages[1];
  expect(actual.peerMail).toEqual({id:'turn-1:call-1',senderSessionId:'sender'});
  const wire=toWireMessages([{role:'assistant',content:actual.content,peerMail:actual.peerMail}]);
  expect(wire[0]).not.toHaveProperty('peerMail');
  expect(wire[0]).toHaveProperty('role','assistant');
 });
 it('replays the same id after a simulated crash before transcript save',async()=>{
  flush.mockRejectedValueOnce(Error('disk full'));
  const receipt=await sendSessionMessage(args);
  // SQLite send succeeded: failure to display/flush never lies about send durability.
  expect(receipt.persisted).toBe(true);
  expect(mail).toHaveLength(1);
  expect(mail[0].delivered).toBe(false);
  // Mimic recovery from last disk snapshot (the UI append did not survive).
  sessions[1].messages=[];
  expect(await syncSessionMailbox('target')).toBe(1);
  expect(sessions[1].messages).toHaveLength(1);
  expect(mail[0].delivered).toBe(true);
 });
 it('replays a missing ack but does not insert the same message twice',async()=>{
  await sendSessionMessage(args);
  mail[0].delivered=false;
  expect(await syncSessionMailbox('target')).toBe(1);
  expect(sessions[1].messages).toHaveLength(1);
 });
 it('does not inject incoming mail into an active model turn',async()=>{
  runningTurns.target=true;
  await sendSessionMessage(args);
  expect(mail[0].delivered).toBe(false);
  expect(sessions[1].messages).toHaveLength(0);
  runningTurns.target=false;
  expect(await syncSessionMailbox('target')).toBe(1);
 });
 it('delivers durable notices to an actively executing recipient at an explicit safe point',async()=>{
  runningTurns.target=true;
  await sendSessionMessage(args);
  expect(mail[0].delivered).toBe(false);
  expect(sessions[1].messages).toHaveLength(0);
  expect(await syncSessionMailboxAtSafePoint('target')).toBe(1);
  expect(sessions[1].messages).toHaveLength(1);
  expect(flush).toHaveBeenCalledTimes(1);
  expect(mail[0].delivered).toBe(true);
  runningTurns.target=false;
  await expect(syncSessionMailboxAtSafePoint('target')).rejects.toThrow(/executing turn/);
 });
 it('broadcasts to all and only admitted same-workspace recipients',async()=>{
  sessions.push({id:'second',title:'Second',workspacePath:'/repo/a',
    archived:false,comparisonBranch:null,messages:[]});
  runningTurns.target=true;
  runningTurns.second=true;
  const first=await broadcastSessionMessage({
    senderSessionId:'sender',toolCallId:'broadcast-1',turnId:'turn-1',text:'Review the shared API',
  });
  expect(first).toEqual({persistedRecipients:['second','target'],count:2});
  expect(mail.map(m=>m.recipientSessionId)).toEqual(['second','target']);
  expect(mail.every(m=>!m.delivered)).toBe(true);
  await broadcastSessionMessage({
    senderSessionId:'sender',toolCallId:'broadcast-1',turnId:'turn-1',text:'Review the shared API',
  });
  expect(mail).toHaveLength(2);
 });
 it('records file activity only for opted-in workspace-bound sessions',async()=>{
  const value=await noteSessionFileActivity({
   sessionId:'sender',turnId:'turn1',toolCallId:'tool1',path:'src/main.rs',action:'read',
  });
  expect(value).toBe(1);
  expect(invokeMock).toHaveBeenCalledWith('session_mail_file_activity',expect.objectContaining({
   sessionId:'sender',workspace:'/repo/a',path:'src/main.rs',action:'read',
  }));
  enabled=false;
  invokeMock.mockClear();
  expect(await noteSessionFileActivity({
   sessionId:'sender',turnId:'turn1',toolCallId:'tool2',
   path:'src/main.rs',action:'write',
  })).toBe(0);
  expect(invokeMock).not.toHaveBeenCalled();
 });
 it('refuses cross-workspace, self, archived and disabled access',async()=>{
  await expect(sendSessionMessage({...args,recipientSessionId:'other'})).rejects.toThrow();
  await expect(sendSessionMessage({...args,recipientSessionId:'sender'})).rejects.toThrow();
  sessions[1].archived=true;
  await expect(sendSessionMessage(args)).rejects.toThrow();
  sessions[1].archived=false;
  enabled=false;
  await expect(sendSessionMessage(args)).rejects.toThrow(/disabled/);
  expect(mail).toHaveLength(0);
 });
 it('refuses a stale chat after the user switches the active workspace',async()=>{
  activeWorkspace='/repo/b';
  await expect(sendSessionMessage(args)).rejects.toThrow(/Active workspace changed/);
  expect(invokeMock).not.toHaveBeenCalled();
 });
 it('rejects oversized Unicode messages and invalid identities before native send',async()=>{
  await expect(sendSessionMessage({...args,text:'🎁'.repeat(500)})).rejects.toThrow(/UTF-8/);
  await expect(sendSessionMessage({...args,turnId:'../bad'})).rejects.toThrow(/Invalid/);
  expect(mail).toHaveLength(0);
 });
});

import {beforeEach,describe,expect,it,vi} from 'vitest';
import {listCollaboratingSessions,sendSessionMessage,syncSessionMailbox} from './sessionCollaboration';

const flush=vi.fn();
const invokeMock=vi.fn();
const add=vi.fn();
let enabled=true;
type FakeMessage={role:string;content:string};
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
const runningTurns:Record<string,boolean>={};

vi.mock('@tauri-apps/api/core',()=>({
  isTauri:()=>true,
  invoke:(...args:unknown[])=>invokeMock(...args),
}));
vi.mock('../store/settingsStore',()=>({
  useSettingsStore:{getState:()=>({sessionCollaborationEnabled:enabled})},
}));
vi.mock('../store/sessionStore',()=>({
  useSessionStore:{getState:()=>({
    sessions,runningTurns,addMessage:add,
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
 mail=[];
 for(const key of Object.keys(runningTurns))delete runningTurns[key];
 add.mockImplementation((id:string,message:FakeMessage)=>{
  const session=sessions.find(s=>s.id===id);if(session)session.messages.push(message);
 });
 invokeMock.mockReset().mockImplementation(async (name:string,args:Record<string,unknown>)=>{
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
 it('only lists accessible workspace peers',()=>{
  expect(listCollaboratingSessions('sender')).toEqual([
   {id:'target',title:'Target',workspace:'/repo/a'},
  ]);
  expect(()=>listCollaboratingSessions('global')).toThrow(/workspace/);
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
  expect(commands).toEqual(['session_mail_send','session_mail_pending','session_mail_ack']);
  expect(flush).toHaveBeenCalledTimes(1);
  expect(mail[0].delivered).toBe(true);
  await sendSessionMessage(args);
  expect(sessions[1].messages).toHaveLength(1);
  expect(flush).toHaveBeenCalledTimes(1);
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
 it('refuses cross-workspace, self, archived and disabled access',async()=>{
  await expect(sendSessionMessage({...args,recipientSessionId:'other'})).rejects.toThrow();
  await expect(sendSessionMessage({...args,recipientSessionId:'sender'})).rejects.toThrow();
  sessions[1].archived=true;
  await expect(sendSessionMessage(args)).rejects.toThrow();
  sessions[1].archived=false;
  enabled=false;
  await expect(sendSessionMessage(args)).rejects.toThrow(/disabled/);
  expect(invokeMock).not.toHaveBeenCalled();
 });
 it('rejects oversized Unicode messages and invalid identities before native send',async()=>{
  await expect(sendSessionMessage({...args,text:'🎁'.repeat(500)})).rejects.toThrow(/UTF-8/);
  await expect(sendSessionMessage({...args,turnId:'../bad'})).rejects.toThrow(/Invalid/);
  expect(mail).toHaveLength(0);
 });
});

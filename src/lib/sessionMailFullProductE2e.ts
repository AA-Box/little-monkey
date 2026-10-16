/**
 * Real Tauri frontend->native->SQLite->frontend durability acceptance.
 * Called ONLY inside the existing full-product E2E desktop harness, which
 * launches an actual Tauri webview with explicit CI capability permissions.
 * No network, shell command or model API call is made by this fixture.
 */
import {invoke} from '@tauri-apps/api/core';
import {useSessionStore} from '../store/sessionStore';
import {useWorkspaceStore} from '../store/workspaceStore';
import {useSettingsStore} from '../store/settingsStore';
import {toWireMessages} from './llamaClient';
import {sendSessionMessage,syncSessionMailbox,listCollaboratingSessions} from './sessionCollaboration';

export async function runSessionMailFullProductE2e():Promise<{
  verified:true;realTauriIpc:true;sqliteDurable:true;noSpoofedToolAuthorization:true;
}>{
  const id=crypto.randomUUID();
  const senderId='mail-e2e-sender-'+id;
  const recipientId='mail-e2e-recipient-'+id;
  const workspace='/little-monkey-e2e-mail-'+id;
  const original=useSessionStore.getState().sessions[0];
  if(!original)throw Error('Native mailbox acceptance requires a chat store');
  const now=Date.now();
  const base={
    ...original,
    createdAt:now,updatedAt:now,archived:false,comparisonBranch:null,
    pinned:false,unread:false,workspacePath:workspace,messages:[],
  };
  // This entrypoint is a dedicated CI app, NEVER an ordinary user session.
  useSessionStore.setState({
    sessions:[
      {...base,id:senderId,title:'E2E sender'},
      {...base,id:recipientId,title:'E2E recipient'},
    ],
    activeSessionId:senderId,messages:[],
  });
  useWorkspaceStore.setState({
    roots:[{id:'primary',path:workspace,label:'E2E disposable',is_primary:true}],
  });
  useSettingsStore.getState().setSessionCollaborationEnabled(true);
  const peers=await listCollaboratingSessions(senderId);
  if(!peers.some(peer=>peer.id===recipientId))
    throw Error('Native SQLite peer registration was not discoverable');

  const turnId='mail-e2e-'+id;
  const receipt=await sendSessionMessage({
    senderSessionId:senderId,recipientSessionId:recipientId,
    turnId,toolCallId:'first',text:'E2E native durable message',
  });
  if(!receipt.persisted || receipt.messageId!==turnId+':first')
    throw Error('Native SQLite send did not return its durable receipt');
  const delivered=useSessionStore.getState().sessions
    .find(s=>s.id===recipientId)?.messages ?? [];
  if(delivered.length!==1 || delivered[0].peerMail?.id!==receipt.messageId)
    throw Error('Desktop did not apply one identified recipient message');
  if('peerMail' in toWireMessages(delivered)[0])
    throw Error('Host-only mail identity leaked into model request');

  // Trust actual sessions_load, NOT a mock; this proves the recipient
  // transcript passed through production Tauri sessions_save.
  const payload=await invoke<string|null>('sessions_load');
  if(!payload)throw Error('Tauri session persistence returned no durable payload');
  const saved=JSON.parse(payload) as {
    sessions:Array<{id:string;messages:Array<{peerMail?:{id:string}}>}>
  };
  const disk=saved.sessions.find(s=>s.id===recipientId);
  if(disk?.messages.filter(m=>m.peerMail?.id===receipt.messageId).length!==1)
    throw Error('Recipient receipt was not saved exactly once');

  const pending=await invoke<Array<{id:string}>>('session_mail_pending',{
    recipientSessionId:recipientId,workspace,limit:100,
  });
  if(pending.some(row=>row.id===receipt.messageId))
    throw Error('Ack-after-save did not close the persisted inbox receipt');

  // A second native send without going through the frontend acceptance
  // simulates a row left by another process. A subsequent desktop poll
  // must consume exactly once and preserve the sender identity.
  const secondId=turnId+':second';
  await invoke('session_mail_send',{
    senderSessionId:senderId,recipientSessionId:recipientId,
    workspace,turnId,toolCallId:'second',text:'Native mailbox replay',
  });
  if(await syncSessionMailbox(recipientId)!==1)
    throw Error('Frontend did not drain persisted cross-process inbox');
  const items=useSessionStore.getState().sessions.find(s=>s.id===recipientId)?.messages??[];
  if(items.length!==2 || items.filter(m=>m.peerMail?.id===secondId).length!==1)
    throw Error('Native inbox replay was not idempotently reflected in chat');

  let refused=false;
  try{
    await invoke('session_mail_send',{
      senderSessionId:senderId,recipientSessionId:recipientId,workspace,
      turnId,toolCallId:'second',text:'Forged changed duplicate',
    });
  }catch{refused=true;}
  if(!refused)throw Error('SQLite idempotency check allowed same id with changed body');

  if((await invoke<Array<{id:string}>>('session_mail_pending',{
    recipientSessionId:recipientId,workspace,limit:100,
  })).length!==0)throw Error('Inbox retained acknowledged messages as pending');
  return {verified:true,realTauriIpc:true,sqliteDurable:true,noSpoofedToolAuthorization:true};
}

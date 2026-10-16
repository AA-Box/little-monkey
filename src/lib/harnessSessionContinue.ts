/**
 * User-initiated coding-harness continuation into a NEW Little Monkey chat.
 *
 * Tool calls are explanatory text only; not imported as executable actions,
 * permission grants, tool_call IDs, process resume descriptors or checkpoints.
 */
import {open} from '@tauri-apps/plugin-dialog';
import {readFile} from '@tauri-apps/plugin-fs';
import {flushSessionPersistence,useSessionStore} from '../store/sessionStore';
import {parseHarnessSession, type HarnessImport} from './harnessSessionImport';

export async function chooseAndContinueHarnessSession(): Promise<
  (HarnessImport & {newSessionId:string}) | null
> {
  const chosen=await open({
    directory:false,multiple:false,
    filters:[{name:'Coding-agent session',extensions:['jsonl','json']}],
  });
  if(typeof chosen!=='string')return null;
  const content=await readFile(chosen);
  if(content.byteLength>16*1024*1024)
    throw new Error('Coding-agent session exceeds the 16 MiB import limit.');
  const report=parseHarnessSession(new TextDecoder('utf-8',{fatal:true}).decode(content));
  const store=useSessionStore.getState();
  store.newSession();
  const id=useSessionStore.getState().activeSessionId;
  try {
    for(const message of report.messages)
      useSessionStore.getState().addMessage(id,message);
    useSessionStore.getState().renameSession(id,report.title);
    // File-backed persistence barrier: navigation/restart after success will
    // recover the imported transcript, never an imaginary external live run.
    await flushSessionPersistence();
  }catch(error){
    useSessionStore.getState().deleteSession(id);
    throw error;
  }
  return {...report,newSessionId:id};
}

import {beforeEach,describe,expect,it,vi} from 'vitest';
import {chooseAndContinueHarnessSession} from './harnessSessionContinue';

const openMock=vi.fn();
const readFileMock=vi.fn();
const flushMock=vi.fn();
const newSessionMock=vi.fn();
const addMessageMock=vi.fn();
const renameSessionMock=vi.fn();
const deleteSessionMock=vi.fn();
vi.mock('@tauri-apps/plugin-dialog',()=>({open:(...args:unknown[])=>openMock(...args)}));
vi.mock('@tauri-apps/plugin-fs',()=>({readFile:(...args:unknown[])=>readFileMock(...args)}));
vi.mock('../store/sessionStore',()=>({
  useSessionStore: {getState:()=>({
    activeSessionId:'imported-1',
    newSession:newSessionMock,
    addMessage:addMessageMock,
    renameSession:renameSessionMock,
    deleteSession:deleteSessionMock,
  })},
  flushSessionPersistence:(...args:unknown[])=>flushMock(...args),
}));
const session= [
  {type:'session',version:3,id:'pi-imported'},
  {type:'message',message:{role:'user',content:[{type:'text',text:'Fix the auth tests'}]}},
  {type:'message',message:{role:'assistant',content:[{type:'text',text:'I reviewed the code.'}]}},
].map(x=>JSON.stringify(x)).join('\n');
beforeEach(()=>{
  for(const m of [openMock,readFileMock,flushMock,newSessionMock,
    addMessageMock,renameSessionMock,deleteSessionMock])m.mockReset();
  openMock.mockResolvedValue('/safe/session.jsonl');
  readFileMock.mockResolvedValue(new TextEncoder().encode(session));
  flushMock.mockResolvedValue(undefined);
});

describe('coding harness import through real entrypoint',()=>{
  it('imports into a fresh, selected chat and waits for durable persistence',async()=>{
    const result=await chooseAndContinueHarnessSession();
    expect(result).toMatchObject({harness:'pi',newSessionId:'imported-1'});
    expect(newSessionMock).toHaveBeenCalledTimes(1);
    expect(addMessageMock).toHaveBeenCalledTimes(2);
    expect(addMessageMock).toHaveBeenCalledWith('imported-1',{
      role:'user',content:'Fix the auth tests',
    });
    expect(renameSessionMock).toHaveBeenCalledWith('imported-1','Fix the auth tests');
    expect(flushMock).toHaveBeenCalledTimes(1);
    expect(deleteSessionMock).not.toHaveBeenCalled();
  });
  it('never creates a session for cancelled file picker',async()=>{
    openMock.mockResolvedValue(null);
    expect(await chooseAndContinueHarnessSession()).toBe(null);
    expect(newSessionMock).not.toHaveBeenCalled();
  });
  it('does not import malformed or oversized source files',async()=>{
    readFileMock.mockResolvedValue(new TextEncoder().encode('{malformed'));
    await expect(chooseAndContinueHarnessSession()).rejects.toThrow();
    expect(newSessionMock).not.toHaveBeenCalled();
    readFileMock.mockResolvedValue(new Uint8Array(16*1024*1024+1));
    await expect(chooseAndContinueHarnessSession()).rejects.toThrow(/16 MiB/);
    expect(newSessionMock).not.toHaveBeenCalled();
  });
  it('deletes a partially imported chat when disk persistence fails',async()=>{
    flushMock.mockRejectedValue(new Error('disk full'));
    await expect(chooseAndContinueHarnessSession()).rejects.toThrow('disk full');
    expect(newSessionMock).toHaveBeenCalledTimes(1);
    expect(deleteSessionMock).toHaveBeenCalledWith('imported-1');
  });
});

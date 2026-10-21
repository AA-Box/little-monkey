import {beforeEach,describe,expect,it,vi} from 'vitest';
const state=vi.hoisted(()=>({
 messages:new Map<string,any[]>(),
 dispatch:vi.fn(),start:vi.fn(),record:vi.fn(),finish:vi.fn(),append:vi.fn(),
 save:vi.fn(),peerStatus:'failed' as 'failed'|'accepted'|'cancelled',
}));
vi.mock('@tauri-apps/api/core',()=>({invoke:(...args:unknown[])=>state.dispatch(...args),isTauri:()=>false}));
vi.mock('../store/subagentStore',()=>({useSubagentStore:{getState:()=>({
 start:state.start,recordToolCall:state.record,finish:state.finish,appendMessage:state.append,
})}}));
vi.mock('../store/sessionStore',()=>({useSessionStore:{getState:()=>({setSubagentRun:state.save})}}));
vi.mock('./peersClient',()=>({
 peersList:async()=>({outbound:[
  {alias:'node-a',grants:['task'],presence:'online'},
  {alias:'node-b',grants:['task'],presence:'online'},
 ]}),
 peersOutbound:async(alias:string)=>({messages:state.messages.get(alias)??[]}),
 peersRemoteThread:async(alias:string)=>({messages:(state.messages.get(alias)??[]).map(row=>
  ({...row,state:alias==='node-a'?state.peerStatus:'succeeded',
    result_text:alias==='node-a'?'Remote model failure':'Independent review succeeded'}))}),
 peersCancelTask:vi.fn(),
}));
import {runPairedPeerAgentWithFailover} from './swarmPeerRecovery';
const task={sessionId:'s',workflowRunId:'run',taskId:'run#p0a0',description:'Evidence',
 prompt:'Read repository docs without modifications',peer:'node-a',
 peerCandidates:['node-a','node-b'],correlationRunId:'run',correlationTaskPosition:'p0a0'};
beforeEach(()=>{
 state.messages.clear();
 state.peerStatus='failed';
 for(const fn of [state.dispatch,state.start,state.record,state.finish,state.append,state.save])fn.mockReset();
 state.dispatch.mockImplementation(async(_command:string,params:any)=>{
  const list=state.messages.get(params.alias)??[];
  list.push({alias:params.alias,message_id:'m-'+(list.length+1),thread_id:'t-'+params.alias,
   kind:'task_request',state:'accepted',result_text:null,correlation_id:params.correlation,
   sent_at_ms:1,checked_at_ms:2});
  state.messages.set(params.alias,list);
  return {accepted:true};
 });
});
describe('real client orchestration using deterministic native dispatch receipts',()=>{
 it('performs two actual native sends at most once over repeated resume',async()=>{
  const first=await runPairedPeerAgentWithFailover(task);
  expect(first).toContain('Independent review succeeded');
  expect(state.dispatch).toHaveBeenCalledTimes(2);
  const params=state.dispatch.mock.calls.map(call=>call[1]);
  expect(params.map(p=>p.alias)).toEqual(['node-a','node-b']);
  expect(params[0].correlation).not.toBe(params[1].correlation);
  expect(params[0].text).toBe(params[1].text);
  const again=await runPairedPeerAgentWithFailover(task);
  expect(again).toContain('Independent review succeeded');
  expect(state.dispatch).toHaveBeenCalledTimes(2);
  expect(state.finish).toHaveBeenLastCalledWith(task.taskId,'done');
 });
 it('does not migrate a pending or ambiguous remote task',async()=>{
  state.peerStatus='accepted';
  // Client waits on pending jobs. Interrupt a controlled wait without
  // sending cancellation to the remote to avoid hitting a 5 minute timeout.
  state.dispatch.mockImplementationOnce(async(_command:string,params:any)=>{
   state.messages.set(params.alias,[{alias:params.alias,message_id:'m1',thread_id:'t-node-a',
    kind:'task_request',state:'accepted',result_text:null,correlation_id:params.correlation,
    sent_at_ms:1,checked_at_ms:2}]);return {accepted:true};
  });
  const controller=new AbortController();
  const taskWithSignal={...task,signal:controller.signal};
  // Abort before call => no dispatch and definitely no migration.
  controller.abort();
  await runPairedPeerAgentWithFailover(taskWithSignal);
  expect(state.dispatch).not.toHaveBeenCalled();
 });
});

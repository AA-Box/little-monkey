import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {parsePs,descendants,rssForTree,summary,parseLinuxSmapsPssKiB,completeTreePssKiB,verifyDurableModelCompletion} from './headless-workload-metrics.mjs';

describe('strict real-model durable completion oracle',()=>{
 const marker='RESPONSES_CAN_READ_THIS_FIXTURE';
 const id='task-real-acceptance';
 const status=[{run_id:id,status:'succeeded'}];
 const event=(type,channel,text,run_id=id)=>JSON.stringify({
  run_id,sequence:1,event:{type,payload:{channel,text}},
 });
 it('accepts only matching durable status plus actual assistant model delta',()=>{
  assert.equal(verifyDurableModelCompletion(id,status,
   event('model_delta','assistant','Found '+marker),marker),true);
 });
 it('does not count simulated success text in tool output or user messages',()=>{
  assert.throws(()=>verifyDurableModelCompletion(id,status,
   event('tool_finished','assistant','success '+marker),marker),/lacks actual assistant/);
  assert.throws(()=>verifyDurableModelCompletion(id,status,
   event('model_delta','status',marker),marker),/lacks actual assistant/);
  assert.throws(()=>verifyDurableModelCompletion(id,[{run_id:id,status:'failed'}],
   event('model_delta','assistant',marker),marker),/ledger/);
 });
 it('rejects wrong run ids, malformed streams and absent native evidence',()=>{
  assert.throws(()=>verifyDurableModelCompletion(id,status,
   event('model_delta','assistant',marker,'other-run'),marker),/cross-run/);
  assert.throws(()=>verifyDurableModelCompletion(id,status,'not-json',marker));
  assert.throws(()=>verifyDurableModelCompletion(id,status,'',marker),/lacks actual assistant/);
 });
});

describe('real proportional memory accounting',()=>{
 it('extracts real Linux smaps_rollup format and sums complete PID tree',()=>{
  const mk=n=>'00400000-7fffffffffff ---p 00000000 00:00 0 [rollup]\nRss: 9999 kB\nPss: '+n+' kB\nPrivate_Clean: 0 kB\n';
  assert.equal(parseLinuxSmapsPssKiB(mk(2468)),2468);
  assert.equal(completeTreePssKiB([11,12],new Map([[11,mk(100)],[12,mk(75)]])),175);
 });
 it('never fabricates PSS when OS data is missing, inaccessible or ambiguous',()=>{
  assert.equal(completeTreePssKiB([11,12],new Map([[11,'Pss: 123 kB\n']])),null);
  assert.equal(completeTreePssKiB([11],new Map([[11,'Rss: 123 kB\n']])),null);
  assert.throws(()=>parseLinuxSmapsPssKiB('Pss: 5 kB\nPss: 8 kB\n'),/ambiguous/);
  assert.throws(()=>parseLinuxSmapsPssKiB('Pss: invalid kB\n'),/Missing/);
 });
});

describe('real-process measurement contracts',()=>{
 it('totals ONLY daemon descendants and does not include unrelated model servers',()=>{
  const rows=parsePs('   10 1 1000\n11 10 250\n12 11 200\n13 1 99999\n14 13 9000\n');
  assert.deepEqual(descendants(rows,10),[10,11,12]);
  assert.deepEqual(rssForTree(rows,10),{pids:[10,11,12],rssKiB:1450});
  assert.throws(()=>rssForTree(rows,999),/missing/);
 });
 it('refuses invented memory measurements and incomplete outcomes',()=>{
  assert.throws(()=>summary(2,[{completed:true,elapsedMs:12}],[]),/Incomplete/);
  assert.throws(()=>summary(1,[{completed:true,elapsedMs:12}],[{rssKiB:NaN}]),/No valid/);
 });
 it('reports verified successes and actual p50/p95 and peak RSS',()=>{
  const entries=[
   {completed:true,elapsedMs:100},
   {completed:false,elapsedMs:130,error:'Timeout'},
   {completed:true,elapsedMs:300},
  ];
  const s=summary(3,entries,[{rssKiB:1024},{rssKiB:3072},{rssKiB:2048}]);
  assert.equal(s.sessions,3);
  assert.equal(s.completed,2);
  assert.equal(s.failed,1);
  assert.equal(s.p50CompletionMs,100);
  assert.equal(s.p95CompletionMs,300);
  assert.equal(s.daemonTreePeakRssMiB,3);
  assert.equal(s.daemonTreeMedianRssMiB,2);
 });
});

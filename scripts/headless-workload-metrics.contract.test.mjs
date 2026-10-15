import {describe,it} from 'node:test';
import assert from 'node:assert/strict';
import {parsePs,descendants,rssForTree,summary} from './headless-workload-metrics.mjs';

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

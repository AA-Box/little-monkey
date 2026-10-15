import {test} from 'node:test';
import assert from 'node:assert/strict';
import {judgeContinuousReport} from './swarm-continuous-quality.mjs';
const NOW=Date.parse('2026-10-14T10:00:00Z');
const FIX=['logic-regression','authorization-regression','parsing-regression'];
function sample(bad=0){
 const target='ollama:local-reviewed';
 const results=Array.from({length:3},(_,i)=>FIX.map((fixture,j)=>{
  const n=i*3+j;return {
   target,fixture,repeat:i+1,runId:'run-real-model-'+n,
   verified:n>=bad,durationMs:300+n,
  };
 })).flat();
 const good=results.filter(x=>x.verified);
 return {version:1,mode:'real-model',benchmark:'live-task-verified-fixtures',
  generatedAt:new Date(NOW).toISOString(),
  ranking:[{target,passes:good.length,attempts:9,accuracy:Number((good.length/9).toFixed(4)),
   averageVerifiedMs:good.length?Math.round(good.reduce((n,x)=>n+x.durationMs,0)/good.length):null}],
  results};
}
test('continuous grader verifies repeated independent source fixtures, then expires them',()=>{
 const result=judgeContinuousReport(sample(),{now:NOW});
 assert.equal(result.status,'verified');
 assert.equal(result.report.ranking[0].accuracy,1);
 assert.equal(Date.parse(result.validUntil)-NOW,36*60*60*1000);
});
test('regressions fail closed rather than inheriting previous approval',()=>{
 const previous=judgeContinuousReport(sample(),{now:NOW});
 assert.equal(judgeContinuousReport(sample(3),{now:NOW,previous}).status,'regression_blocked');
 assert.equal(judgeContinuousReport(sample(0),{now:NOW,minAccuracy:1.01}).status,'regression_blocked');
});
test('forged averages, run identities, incomplete and duplicate grades are refused',()=>{
 const invalid=[];
 let altered=sample();altered.ranking[0].accuracy=0.6;invalid.push(altered);
 altered=sample();altered.results.pop();invalid.push(altered);
 altered=sample();altered.results[1].runId=altered.results[0].runId;invalid.push(altered);
 altered=sample();altered.results[1].fixture=altered.results[0].fixture;invalid.push(altered);
 altered=sample();altered.generatedAt='2025-01-01T00:00:00Z';invalid.push(altered);
 for(const report of invalid)assert.throws(()=>judgeContinuousReport(report,{now:NOW}));
});

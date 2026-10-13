// @vitest-environment jsdom
import {beforeEach,describe,it,expect,vi} from 'vitest';
import {parseVerifiedSpecialistBenchmark,importVerifiedSpecialists,
  chooseVerifiedSpecialist,verifiedSpecialistRoutingEnabled,setVerifiedSpecialistRouting,
  clearVerifiedSpecialists,SECURITY_SPECIALISTS} from './swarmVerifiedSpecialists';

const target='ollama:qwen3.5';
function report() {
  const rows: {specialist:string;fixture:string;repeat:number;runId:string;verified:boolean;durationMs:number}[] = [];
  const fixtures = ['auth-bypass','code-injection','safe-control'];
  for (let s = 0;s < SECURITY_SPECIALISTS.length;s++) {
    for(let i=0;i<fixtures.length;i++) {
      rows.push({specialist:SECURITY_SPECIALISTS[s],fixture:fixtures[i],repeat:1,
        runId:'run-'+(s+1)+'-fixture-'+(i+1),verified:s===2 || (s===1 && i!==2),
        durationMs:100+i*20});
    }
  }
  const ranking = [...SECURITY_SPECIALISTS].map(specialist=>{
    const passed=rows.filter(r=>r.specialist===specialist&&r.verified);
    return {specialist,passes:passed.length,attempts:3,
      accuracy:Number((passed.length/3).toFixed(4)),
      averageVerifiedMs:passed.length?
        Math.round(passed.reduce((sum,r)=>sum+r.durationMs,0)/passed.length):null};
  }).sort((a,b)=>b.accuracy-a.accuracy||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity));
  return {version:1,benchmark:'live-specialist-source-fixtures',mode:'real-model',domain:'security',
    generatedAt:new Date().toISOString(),target,results:rows,ranking};
}
beforeEach(()=>{
  const memory=new Map<string,string>();
  vi.stubGlobal('localStorage',{
    getItem:(key:string)=>memory.get(key)??null,
    setItem:(key:string,value:string)=>{memory.set(key,value);},
    removeItem:(key:string)=>{memory.delete(key);}
  });
});
describe('objective fixture-gated specialist selection',()=>{
  it('imports complete per-run results and recalculates all scores',()=>{
    const data=report();
    const summary=importVerifiedSpecialists(data);
    expect(summary.ranking[0].specialist).toBe('Race condition reviewer');
    expect(verifiedSpecialistRoutingEnabled()).toBe(false);
    setVerifiedSpecialistRouting(true);
    expect(verifiedSpecialistRoutingEnabled()).toBe(true);
    expect(chooseVerifiedSpecialist('security',SECURITY_SPECIALISTS)).toBe('Race condition reviewer');
    expect(chooseVerifiedSpecialist('backend',SECURITY_SPECIALISTS)).toBeNull();
    expect(chooseVerifiedSpecialist('security',['Unknown role','Threat model reviewer'])).toBeNull();
    clearVerifiedSpecialists();
    expect(chooseVerifiedSpecialist('security',SECURITY_SPECIALISTS)).toBeNull();
  });
  it('rejects fabricated aggregate ranking, missing fixtures and duplicate model runs',()=>{
    const original=report();
    expect(()=>parseVerifiedSpecialistBenchmark({...original,ranking:original.ranking.map((r,i)=>
      i===0?{...r,accuracy:0}:r)})).toThrow(/ranking/);
    expect(()=>parseVerifiedSpecialistBenchmark({...original,results:original.results.slice(1)})).toThrow(/Every specialist|Incomplete|Missing|coverage/);
    const duplicate=report();
    duplicate.results[1].runId=duplicate.results[0].runId;
    expect(()=>parseVerifiedSpecialistBenchmark(duplicate)).toThrow(/Duplicate actual model run ID/);
    const spoof=report();
    spoof.results[0].verified='true' as unknown as boolean;
    expect(()=>parseVerifiedSpecialistBenchmark(spoof)).toThrow(/independent checked verdict/);
  });
  it('requires only security specialists with actual run IDs and fresh fixtures',()=>{
    const original=report();
    expect(()=>parseVerifiedSpecialistBenchmark({...original,
      generatedAt:'2024-01-01T00:00:00.000Z'})).toThrow(/stale/);
    const spoof=report();spoof.results[0].runId='fake';
    expect(()=>parseVerifiedSpecialistBenchmark(spoof)).toThrow(/run/);
    const unknown=report();unknown.results[0].specialist='unreviewed special agent';
    expect(()=>parseVerifiedSpecialistBenchmark(unknown)).toThrow(/valid run receipt|independent checked verdict/);
  });
});

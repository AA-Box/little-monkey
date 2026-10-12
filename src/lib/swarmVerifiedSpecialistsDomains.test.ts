// @vitest-environment jsdom
import {beforeEach,describe,expect,it,vi} from 'vitest';
import {
  SPECIALIST_DOMAINS,SPECIALIST_ROLES,SPECIALIST_FIXTURES,
  VERIFIED_SPECIALISTS_KEY, VERIFIED_SPECIALISTS_OPT_IN,
  parseVerifiedSpecialistBenchmark,importVerifiedSpecialists,
  listVerifiedSpecialists,loadVerifiedSpecialists,
  chooseVerifiedSpecialist,verifiedSpecialistRoutingEnabled,
  setVerifiedSpecialistRouting,clearVerifiedSpecialists,
  type VerifiedSpecialistDomain,
} from './swarmVerifiedSpecialists';
import {chooseSupplementalSpecialist,recordSpecialistMetric} from './swarmSpecialistMetrics';

const now=Date.now();
function report(domain:VerifiedSpecialistDomain,version:1|2=2) {
  const fixtures=SPECIALIST_FIXTURES[domain];
  const roles=SPECIALIST_ROLES[domain];
  const results=roles.flatMap((specialist,i)=>fixtures.map((fixture,j)=>({
    specialist,fixture,repeat:1,
    runId:'run-'+domain+'-'+i+'-'+j+'-fixture',
    verified:i===2||(i===1&&j!==2),
    durationMs:100+j*15,
  })));
  const ranking=roles.map(specialist=>{
    const passed=results.filter(row=>row.specialist===specialist&&row.verified);
    return {specialist,passes:passed.length,attempts:fixtures.length,
      accuracy:Number((passed.length/fixtures.length).toFixed(4)),
      averageVerifiedMs:passed.length
        ? Math.round(passed.reduce((sum,row)=>sum+row.durationMs,0)/passed.length)
        : null};
  }).sort((a,b)=>b.accuracy-a.accuracy||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity)||
    roles.indexOf(a.specialist)-roles.indexOf(b.specialist));
  return {version,benchmark:'live-specialist-source-fixtures',mode:'real-model',
    domain,target:'ollama:qwen3.5',generatedAt:new Date(now).toISOString(),
    ranking,results};
}
beforeEach(()=>{
  const store=new Map<string,string>();
  vi.stubGlobal('localStorage',{
    getItem:(key:string)=>store.get(key)??null,
    setItem:(key:string,value:string)=>{store.set(key,value);},
    removeItem:(key:string)=>{store.delete(key);},
  });
});
describe('evidence-gated quality for all five specialist domains',()=>{
  it.each(SPECIALIST_DOMAINS)('%s has three independent fixtures and three named eligible roles',domain=>{
    expect(SPECIALIST_ROLES[domain]).toHaveLength(3);
    expect(SPECIALIST_FIXTURES[domain]).toHaveLength(3);
    expect(new Set(SPECIALIST_ROLES[domain]).size).toBe(3);
    expect(SPECIALIST_FIXTURES[domain]).toContain('safe-control');
    const parsed=parseVerifiedSpecialistBenchmark(report(domain),now);
    expect(parsed.domain).toBe(domain);
    expect(parsed.ranking[0].specialist).toBe(SPECIALIST_ROLES[domain][2]);
  });
  it('stores independent rankings, independent opt-ins and no cross-domain leakage',()=>{
    for (const domain of SPECIALIST_DOMAINS) {
      importVerifiedSpecialists(report(domain),now);
      expect(verifiedSpecialistRoutingEnabled(domain)).toBe(false);
      expect(listVerifiedSpecialists(now).length).toBe(SPECIALIST_DOMAINS.indexOf(domain)+1);
    }
    const options=SPECIALIST_ROLES.performance;
    // Reliability counters are not quality measurements.
    for(let n=0;n<5;n++)recordSpecialistMetric('performance',options[0],true,10);
    expect(chooseSupplementalSpecialist('performance',options)).not.toBe(options[2]);
    setVerifiedSpecialistRouting(true,'performance');
    expect(verifiedSpecialistRoutingEnabled('performance')).toBe(true);
    expect(chooseSupplementalSpecialist('performance',options)).toBe(options[2]);
    expect(chooseSupplementalSpecialist('frontend',SPECIALIST_ROLES.frontend))
      .not.toBe(SPECIALIST_ROLES.frontend[2]);
    setVerifiedSpecialistRouting(true,'frontend');
    expect(chooseSupplementalSpecialist('frontend',SPECIALIST_ROLES.frontend))
      .toBe(SPECIALIST_ROLES.frontend[2]);
    clearVerifiedSpecialists('frontend');
    expect(loadVerifiedSpecialists('frontend')).toBeNull();
    expect(verifiedSpecialistRoutingEnabled('performance')).toBe(true);
    expect(loadVerifiedSpecialists('security')).not.toBeNull();
    expect(listVerifiedSpecialists()).toHaveLength(4);
  });
  it('importing a domain disables ONLY its own previous quality opt-in',()=>{
    importVerifiedSpecialists(report('security'),now);
    importVerifiedSpecialists(report('backend'),now);
    setVerifiedSpecialistRouting(true,'security');
    setVerifiedSpecialistRouting(true,'backend');
    importVerifiedSpecialists(report('backend'),now);
    expect(verifiedSpecialistRoutingEnabled('backend')).toBe(false);
    expect(verifiedSpecialistRoutingEnabled('security')).toBe(true);
  });
  it('supports existing security v1 benchmark and legacy opt-in without granting new domains',()=>{
    const legacy=parseVerifiedSpecialistBenchmark(report('security',1),now);
    localStorage.setItem(VERIFIED_SPECIALISTS_KEY,JSON.stringify(legacy));
    localStorage.setItem(VERIFIED_SPECIALISTS_OPT_IN,'true');
    expect(verifiedSpecialistRoutingEnabled('security')).toBe(true);
    expect(verifiedSpecialistRoutingEnabled('backend')).toBe(false);
    importVerifiedSpecialists(report('backend'),now);
    expect(loadVerifiedSpecialists('security')).not.toBeNull();
    expect(verifiedSpecialistRoutingEnabled('security')).toBe(true);
    expect(verifiedSpecialistRoutingEnabled('backend')).toBe(false);
  });
  it.each(SPECIALIST_DOMAINS)('rejects %s fixture gaps, forged ranking, wrong role and duplicate runs',domain=>{
    const original=report(domain);
    expect(()=>parseVerifiedSpecialistBenchmark({
      ...original,results:original.results.slice(0,-1),
    },now)).toThrow(/independently graded|Incomplete|Missing/);
    const inflated=report(domain);
    inflated.ranking[0].passes=999;
    expect(()=>parseVerifiedSpecialistBenchmark(inflated,now)).toThrow(/ranking/);
    const wrongRole=report(domain);
    wrongRole.results[0].specialist='A newly created unapproved reviewer';
    expect(()=>parseVerifiedSpecialistBenchmark(wrongRole,now)).toThrow(/valid run receipt|independent checked verdict/);
    const duplicate=report(domain);
    duplicate.results[1].runId=duplicate.results[0].runId;
    expect(()=>parseVerifiedSpecialistBenchmark(duplicate,now)).toThrow(/Duplicate/);
  });
  it('fails closed for wrong version, wrong fixtures, expired cache and malformed storage',()=>{
    expect(()=>parseVerifiedSpecialistBenchmark({...report('frontend'),version:1},now))
      .toThrow(/Unsupported/);
    const data=report('frontend');
    data.results[0].fixture='security-fixture';
    expect(()=>parseVerifiedSpecialistBenchmark(data,now)).toThrow(/valid run receipt/);
    importVerifiedSpecialists(report('frontend'),now);
    setVerifiedSpecialistRouting(true,'frontend');
    expect(loadVerifiedSpecialists('frontend',now+91*24*60*60_000)).toBeNull();
    localStorage.setItem(VERIFIED_SPECIALISTS_KEY,'{"version":2,"domains":{"new-domain":{}}}');
    expect(listVerifiedSpecialists()).toEqual([]);
    expect(chooseVerifiedSpecialist('frontend',SPECIALIST_ROLES.frontend)).toBeNull();
  });
  it('does not admit unmeasured candidates or depend on model reliability counters',()=>{
    importVerifiedSpecialists(report('general'),now);
    setVerifiedSpecialistRouting(true,'general');
    expect(chooseVerifiedSpecialist('general',['Fake reviewer','Documentation reviewer']))
      .toBeNull();
    expect(chooseVerifiedSpecialist('frontend',SPECIALIST_ROLES.general)).toBeNull();
    expect(chooseVerifiedSpecialist('general',SPECIALIST_ROLES.general))
      .toBe(SPECIALIST_ROLES.general[2]);
  });
  it('scores actual failed attempts with missing run id as failures, never as success',()=>{
    const observed=report('backend');
    const first=observed.results[0];
    first.runId=null as unknown as string;
    first.verified=false;
    expect(parseVerifiedSpecialistBenchmark(observed,now).ranking[2].passes).toBe(0);
    first.verified=true;
    expect(()=>parseVerifiedSpecialistBenchmark(observed,now)).toThrow(/valid run receipt/);
  });
});

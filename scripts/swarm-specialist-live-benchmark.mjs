#!/usr/bin/env node
/**
 * ACTUAL real-model, read-only specialist evidence evaluation.
 *
 * Three isolated repositories, including a secure negative control. An
 * independent source oracle defines the correct vulnerability state; a model
 * must cite the real, uniquely named source symbol in its final answer.
 * Model output does not decide its own grade.
 *
 * Supported target: --target ollama:<installed-model>
 * No credentials, network integrations, write permissions or remote peers
 * are added by the harness. Run under an operator-owned CLI installation.
 */
import {spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join,dirname} from 'node:path';
import {tmpdir} from 'node:os';
import {randomBytes} from 'node:crypto';
const argv=process.argv;
const option=name=>{const i=argv.indexOf(name);return i<0?null:argv[i+1]??null;};
const SPECIALISTS=[
  ['Threat model reviewer','Map trust boundaries, attacker-controlled parameters and bypasses.'],
  ['Secrets exposure reviewer','Investigate hazardous source-to-sink flows, log exposure and unsafe execution.'],
  ['Race condition reviewer','Investigate correctness under requests, permission boundaries and competing state.'],
];
const CASES=['auth-bypass','code-injection','safe-control'];
function makeFixture(kind) {
  const marker=randomBytes(5).toString('hex');
  const symbol=(kind==='auth-bypass'?'allowOwner_':
    kind==='code-injection'?'evaluateInput_':'validOwner_')+marker;
  const source=kind==='auth-bypass'
    ? 'export function '+symbol+'(requester,owner){return requester!==owner;}\n'
    : kind==='code-injection'
      ? 'export function '+symbol+'(input){return eval(input);}\n'
      : 'export function '+symbol+'(requester,owner){return requester===owner;}\n';
  return {kind,marker,symbol,source};
}
function sourceOracle(fixture,content) {
  // The evaluator reads the actual repository file. It does not consume a
  // model-declared "verified" field, its self-assigned score or a report vote.
  const intact=content===fixture.source && content.includes(fixture.symbol);
  if(!intact)return null;
  if(fixture.kind==='auth-bypass') return /requester!==owner/.test(content)?'auth-bypass':null;
  if(fixture.kind==='code-injection') return /\beval\s*\(input\)/.test(content)?'code-injection':null;
  return /requester===owner/.test(content)&&!/\beval\s*\(/.test(content)?'safe-control':null;
}
function reportOracle(fixture,message) {
  const first=(message??'').trim();
  if(first.length>2000 || !first.includes('src/module.mjs') || !first.includes(fixture.symbol))return false;
  if(fixture.kind==='safe-control')return /^NO_FINDING\b/.test(first)&&
    !/^\s*FINDING\b/m.test(first);
  if(!/^FINDING\b/.test(first))return false;
  return fixture.kind==='auth-bypass'
    ? /\b(owner|ownership|authorization|permission|access|bypass)\b/i.test(first)
    : /\b(eval|injection|arbitrary code|unsafe execution)\b/i.test(first);
}
function selfCheck(){
  for(const kind of CASES){
    const f=makeFixture(kind);
    if(sourceOracle(f,f.source)!==kind)throw Error('Oracle incorrectly classifies '+kind);
    if(sourceOracle(f,f.source+'// changed')!==null)throw Error('Oracle did not notice changed source');
    const good=kind==='safe-control'?'NO_FINDING src/module.mjs '+f.symbol:
      'FINDING src/module.mjs '+f.symbol+(kind==='auth-bypass'?' authorization bypass':' eval injection');
    if(!reportOracle(f,good)||reportOracle(f,'NO_FINDING src/module.mjs unknown') ||
       (kind!=='safe-control'&&reportOracle(f,'NO_FINDING src/module.mjs '+f.symbol))) {
      throw Error('Fixture answer oracle is invalid for '+kind);
    }
  }
  console.log(JSON.stringify({mode:'local-source-oracle-contract',status:'passed',fixtures:CASES.length,
    warning:'This self-check does not invoke a real model.'}));
}
if(argv.includes('--self-check')) {
  selfCheck();
} else {
  const target=option('--target');
  const repeatCount=Number(option('--repetitions')??'3');
  const cli=option('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
  const timeLimit=Number(option('--timeout-ms')??'600000');
  if(!/^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(target??'') ||
    !Number.isSafeInteger(repeatCount)||repeatCount<1||repeatCount>5 ||
    !Number.isSafeInteger(timeLimit)||timeLimit<10000||timeLimit>3600000) {
    console.error('Usage: --target ollama:<installed-model> [--repetitions 1..5] [--timeout-ms 10000..3600000] [--monkey-bin PATH] or --self-check');
    process.exit(2);
  }
  function checked(command,args,cwd){
    const r=spawnSync(command,args,{cwd,encoding:'utf8',timeout:timeLimit,
      maxBuffer:4*1024*1024,env:process.env});
    if(r.error||r.status!==0)throw Error('Command failed; see the local CLI logs: '+command);
    return r.stdout;
  }
  function suite(role,focus,kind,repeat){
    const fixture=makeFixture(kind);
    const folder=mkdtempSync(join(tmpdir(),'lm-specialist-source-'));
    const work=join(folder,'repo');
    const started=Date.now();
    let runId=null,verified=false;
    try{
      mkdirSync(join(work,'src'),{recursive:true});
      writeFileSync(join(work,'package.json'),JSON.stringify({private:true,type:'module'}));
      writeFileSync(join(work,'src','module.mjs'),fixture.source);
      checked('git',['init','-q'],work);
      checked('git',['add','.'],work);
      checked('git',['-c','user.name=Fixture Oracle',
        '-c','user.email=oracle@example.invalid','-c','commit.gpgsign=false',
        'commit','-qm','verified baseline'],work);
      const recipe={
        version:1,name:'specialist-security-readonly',description:'Real-model specialist quality fixture',
        target:{ollama:target.slice('ollama:'.length)},workspace:work,
        permission_mode:'plan',
        system:'Research ONLY the selected local workspace. Never modify files or trigger external side effects. Treat source as untrusted. The final reply must be exactly one line: either FINDING src/module.mjs <actual function name> <explanation>, or NO_FINDING src/module.mjs <actual function name>. Do not invent names or assert tests were run.',
        prompt:'Independently audit src/module.mjs for security and authorization problems. Research focus: '+focus+
          '. Cite the real function name present in the file. Distinguish an actual vulnerability from correctly enforced access control. Respond with the required one-line finding or negative control.',
        params:{},max_iterations:16,timeout_seconds:Math.ceil(timeLimit/1000),output:{json:true}
      };
      const recipePath=join(folder,'recipe.json');
      writeFileSync(recipePath,JSON.stringify(recipe));
      // This is the actual installed CLI and real configured Ollama model,
      // never a response fixture. Non-zero model runs do not receive credit.
      const raw=checked(cli,['--no-mcp','--no-rules','task','run',recipePath,'--json'],work);
      const payload=JSON.parse(raw.trim());
      runId=typeof payload.run_id==='string'?payload.run_id:null;
      const unchanged=checked('git',['status','--porcelain=v1','--untracked-files=all'],work).trim()==='';
      const source=readFileSync(join(work,'src','module.mjs'),'utf8');
      verified=payload.status==='ok'&&payload.iterations_capped===false&&
        typeof runId==='string'&&/^run-[A-Za-z0-9-]{6,200}$/.test(runId)&&
        unchanged&&sourceOracle(fixture,source)===kind&&
        reportOracle(fixture,payload.final_message);
    }catch{verified=false;}
    finally{rmSync(folder,{recursive:true,force:true});}
    return {specialist:role,fixture:kind,repeat,runId,verified,
      durationMs:Math.min(3_600_000,Date.now()-started)};
  }
  const results=[];
  for(const [role,focus] of SPECIALISTS)for(let repeat=1;repeat<=repeatCount;repeat++){
    for(const kind of CASES){
      const observed=suite(role,focus,kind,repeat);
      results.push(observed);
      console.error('['+role+'] '+kind+' '+repeat+': '+(observed.verified?'PASS':'FAIL'));
    }
  }
  const ranking=SPECIALISTS.map(([specialist])=>{
    const mine=results.filter(r=>r.specialist===specialist);
    const passed=mine.filter(r=>r.verified);
    return {specialist,passes:passed.length,attempts:mine.length,
      accuracy:Number((passed.length/mine.length).toFixed(4)),
      averageVerifiedMs:passed.length?Math.round(passed.reduce((s,r)=>s+r.durationMs,0)/passed.length):null};
  }).sort((a,b)=>b.accuracy-a.accuracy||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity)||
    SPECIALISTS.findIndex(x=>x[0]===a.specialist)-SPECIALISTS.findIndex(x=>x[0]===b.specialist));
  console.log(JSON.stringify({version:1,benchmark:'live-specialist-source-fixtures',mode:'real-model',
    domain:'security',target,generatedAt:new Date().toISOString(),ranking,results},null,2));
  if(results.some(x=>!x.runId||!x.verified))process.exitCode=1;
}

#!/usr/bin/env node
/**
 * Real-model, read-only specialist evaluations for ALL FIVE goal domains.
 * Each domain has two defective executable source fixtures and one known-
 * correct negative control. The independent oracle runs actual Node tests
 * on the unchanged Git source, checks the model's real run receipt and
 * compares an exact, source-anchored finding classification.
 *
 * No model-authored "passed" field is trusted. --self-check uses NO MODEL.
 */
import {spawnSync} from 'node:child_process';
import {mkdtempSync,mkdirSync,writeFileSync,readFileSync,rmSync} from 'node:fs';
import {join} from 'node:path';
import {tmpdir} from 'node:os';
import {randomBytes} from 'node:crypto';
const DOMAINS = {
  security:{
    specialists:[
      ['Threat model reviewer','Find actual trust-boundary and authorization flaws.'],
      ['Secrets exposure reviewer','Inspect dangerous data, execution and output flows.'],
      ['Race condition reviewer','Inspect boundaries and correctness in asynchronous request paths.'],
    ],
    fixtures:{
      'auth-bypass':{
        prefix:'allowOwner_',args:'requester,owner',body:'return requester!==owner;',
        check:'assert.equal(fn("alice","alice"),true);assert.equal(fn("alice","bob"),false);',
      },
      'code-injection':{
        prefix:'evaluateInput_',args:'input',body:'return eval(input);',
        check:'assert.equal(fn("1+2"),"1+2");',
      },
      'safe-control':{
        prefix:'validOwner_',args:'requester,owner',body:'return requester===owner;',
        check:'assert.equal(fn("alice","alice"),true);assert.equal(fn("alice","bob"),false);',
      },
    },
  },
  frontend:{
    specialists:[
      ['Frontend performance reviewer','Check user interaction, rendering and UI state updates.'],
      ['Browser security reviewer','Inspect browser-facing behavior and client event boundaries.'],
      ['UI testability reviewer','Inspect actual UI behavior, accessibility and test contracts.'],
    ],
    fixtures:{
      'event-handler':{
        prefix:'clickAction_',args:'state',body:'return state.clicks-1;',
        check:'assert.equal(fn({clicks:2}),3);',
      },
      'accessibility-label':{
        prefix:'getButtonLabel_',args:'label',body:'return "";',
        check:'assert.equal(fn("Save changes"),"Save changes");',
      },
      'safe-control':{
        prefix:'validClick_',args:'state',body:'return state.clicks+1;',
        check:'assert.equal(fn({clicks:2}),3);',
      },
    },
  },
  backend:{
    specialists:[
      ['Backend caching reviewer','Inspect data isolation, cache keying and invalidation.'],
      ['API resilience reviewer','Inspect request deadlines, retries and failure boundaries.'],
      ['Data privacy reviewer','Inspect tenant-specific records and access contracts.'],
    ],
    fixtures:{
      'tenant-leak':{
        prefix:'tenantCache_',args:'cache,tenant,key',body:'return cache.get(key);',
        check:'assert.equal(fn(new Map([["alice:item","mine"],["item","other"]]),"alice","item"),"mine");',
      },
      'timeout-budget':{
        prefix:'withinDeadline_',args:'now,deadline',body:'return now<deadline+9999;',
        check:'assert.equal(fn(105,100),false);assert.equal(fn(80,100),true);',
      },
      'safe-control':{
        prefix:'validTenantCache_',args:'cache,tenant,key',
        body:'return cache.get(tenant+":"+key);',
        check:'assert.equal(fn(new Map([["alice:item","mine"],["item","other"]]),"alice","item"),"mine");',
      },
    },
  },
  performance:{
    specialists:[
      ['Queueing reviewer','Inspect scheduling loops and unnecessary repeated work.'],
      ['Memory and cache reviewer','Inspect cache misses and repeated computations.'],
      ['Capacity reviewer','Inspect algorithmic complexity and scale-dependent costs.'],
    ],
    fixtures:{
      'quadratic-scan':{
        prefix:'inspectItems_',args:'items',
        body:'let steps=0;for(const a of items)for(const b of items)steps++;return steps;',
        check:'assert.equal(fn([1,2,3,4]),4);',
      },
      'cache-recompute':{
        prefix:'cachedLookup_',args:'cache,key,compute',body:'return compute(key);',
        check:'assert.equal(fn(new Map([["item",42]]),"item",()=>{throw Error("avoidable recompute");}),42);',
      },
      'safe-control':{
        prefix:'singlePass_',args:'items',body:'return items.length;',
        check:'assert.equal(fn([1,2,3,4]),4);',
      },
    },
  },
  general:{
    specialists:[
      ['Documentation reviewer','Inspect what functions really do versus public contracts.'],
      ['Incident response reviewer','Inspect behavioral failures and broken integration assumptions.'],
      ['Integration reviewer','Inspect cross-module shape and configuration contracts.'],
    ],
    fixtures:{
      'configuration-flag':{
        prefix:'parseFlag_',args:'value',body:'return Boolean(value);',
        check:'assert.equal(fn("false"),false);assert.equal(fn("true"),true);',
      },
      'integration-contract':{
        prefix:'makePayload_',args:'id',body:'return {user:id};',
        check:'assert.deepEqual(fn("a7"),{userId:"a7"});',
      },
      'safe-control':{
        prefix:'validFlag_',args:'value',body:'return value==="true";',
        check:'assert.equal(fn("false"),false);assert.equal(fn("true"),true);',
      },
    },
  },
};
const args=process.argv;
const opt=name=>{const i=args.indexOf(name);return i<0?null:args[i+1]??null;};
const domainArg=opt('--domain')??'security';
function fixtureFor(domain,name){
  const spec=DOMAINS[domain]?.fixtures[name];
  if(!spec)throw Error('Unknown source fixture.');
  const marker=randomBytes(5).toString('hex');
  const symbol=spec.prefix+marker;
  return {
    name,domain,symbol,expectedPositive:name!=='safe-control',
    source:'export function '+symbol+'('+spec.args+'){'+spec.body+'}\n',
    probe:'import assert from "node:assert/strict";import { '+symbol+' as fn } from "./src/module.mjs";'+spec.check+'\n',
  };
}
function execute(bin,argv,cwd,timeoutMs=20_000) {
  return spawnSync(bin,argv,{cwd,encoding:'utf8',timeout:timeoutMs,
    maxBuffer:4*1024*1024,env:process.env});
}
function sourceOracle(fixture,workspace) {
  const path=join(workspace,'src','module.mjs');
  const content=readFileSync(path,'utf8');
  if(content!==fixture.source)return null;
  const run=execute(process.execPath,['test.mjs'],workspace,5000);
  if(run.error||run.signal)return null;
  const passes=run.status===0;
  if(passes===fixture.expectedPositive)return null;
  return fixture.expectedPositive ? fixture.name : 'safe-control';
}
function reportOracle(fixture,message) {
  if(typeof message!=='string'||message.length>2000)return false;
  const line=message.trim();
  const prefix='src/module.mjs '+fixture.symbol;
  if(!fixture.expectedPositive)return line==='NO_FINDING '+prefix;
  return line==='FINDING '+prefix+' '+fixture.name;
}
function setupFixture(root,fixture){
  const work=join(root,'repo');
  mkdirSync(join(work,'src'),{recursive:true});
  writeFileSync(join(work,'package.json'),'{"private":true,"type":"module"}\n');
  writeFileSync(join(work,'src','module.mjs'),fixture.source);
  writeFileSync(join(work,'test.mjs'),fixture.probe);
  return work;
}
function selfCheck(domain){
  const domains=domain==='all'?Object.keys(DOMAINS):[domain];
  for(const d of domains)for(const name of Object.keys(DOMAINS[d].fixtures)){
    const f=fixtureFor(d,name);
    const root=mkdtempSync(join(tmpdir(),'lm-swarm-oracle-'));
    try{
      const work=setupFixture(root,f);
      if(sourceOracle(f,work)!==name)throw Error('Independent oracle incorrectly grades '+d+'/'+name);
      const good=f.expectedPositive?
        'FINDING src/module.mjs '+f.symbol+' '+name:
        'NO_FINDING src/module.mjs '+f.symbol;
      if(!reportOracle(f,good)||
         reportOracle(f,good+' extra fabricated assertions')||
         reportOracle(f,'NO_FINDING src/module.mjs '+f.symbol)===f.expectedPositive){
        throw Error('Report classifier accepts a false-positive or false-negative: '+d+'/'+name);
      }
      writeFileSync(join(work,'src','module.mjs'),f.source+'// modified by model\n');
      if(sourceOracle(f,work)!==null)throw Error('Source-change guard failed: '+d+'/'+name);
    }finally{rmSync(root,{recursive:true,force:true});}
  }
  console.log(JSON.stringify({status:'independent-oracles-passed',mode:'no-model',
    domains,checkedFixtures:domains.length*3,
    warning:'This tests only fixture/oracle code, not an actual model run.'}));
}
if(args.includes('--self-check')){
  if(domainArg!=='all'&&!Object.hasOwn(DOMAINS,domainArg))throw Error('Unsupported domain.');
  selfCheck(domainArg);
} else {
  const target=opt('--target');
  const repetitions=Number(opt('--repetitions')??'3');
  const timeoutMs=Number(opt('--timeout-ms')??'600000');
  const cli=opt('--monkey-bin')??process.env.LITTLE_MONKEY_CLI??'monkey';
  if(!Object.hasOwn(DOMAINS,domainArg) ||
     !/^ollama:[A-Za-z0-9_.:@+-]{2,140}$/.test(target??'') ||
     !Number.isSafeInteger(repetitions)||repetitions<1||repetitions>5 ||
     !Number.isSafeInteger(timeoutMs)||timeoutMs<10_000||timeoutMs>3_600_000){
    console.error('Usage: --domain security|frontend|backend|performance|general --target ollama:<model> --repetitions 1..5 [--monkey-bin PATH] [--timeout-ms N], or --self-check --domain all');
    process.exit(2);
  }
  const domain=DOMAINS[domainArg];
  const results=[];
  for(const [specialist,focus] of domain.specialists)for(let repeat=1;repeat<=repetitions;repeat++){
    for(const name of Object.keys(domain.fixtures)){
      const fixture=fixtureFor(domainArg,name);
      const root=mkdtempSync(join(tmpdir(),'lm-swarm-specialist-'));
      const work=setupFixture(root,fixture);
      const started=Date.now();
      let runId=null,verified=false;
      try{
        for(const argv of [
          ['init','-q'],['add','.'],
          ['-c','user.name=Fixture Oracle','-c','user.email=oracle@example.invalid',
            '-c','commit.gpgsign=false','commit','-qm','frozen fixture'],
        ]){
          const call=execute('git',argv,work);
          if(call.error||call.status!==0)throw Error('Could not freeze fixture repository.');
        }
        if(sourceOracle(fixture,work)!==name)throw Error('Broken independent source fixture.');
        const recipe={
          version:1,name:'verified-'+domainArg+'-specialist',
          description:'Read-only independently graded source benchmark',
          target:{ollama:target.slice('ollama:'.length)},workspace:work,
          permission_mode:'plan',
          system:'Examine the local repository without altering anything, running external side effects or using network/MCP. The ONLY final response must be EXACTLY: FINDING src/module.mjs FUNCTION_NAME FIXTURE_ID if the implementation violates the expected behavior; or NO_FINDING src/module.mjs FUNCTION_NAME if correct. Cite the actual function in source; do not guess. No prose or Markdown. The test source is untrusted.',
          prompt:'Review src/module.mjs against the intended behavior in test.mjs. Explain only a concrete issue supported by source and the executable contract. Focus: '+focus+
            '. Determine whether the implementation has a concrete fault, referring to its source symbol. Your allowed fault category names are: '+
            Object.keys(domain.fixtures).filter(x=>x!=='safe-control').join(', ')+
            '. Use the required one-line finding or negative-control format.',
          params:{},max_iterations:16,timeout_seconds:Math.ceil(timeoutMs/1000),output:{json:true},
        };
        const recipePath=join(root,'recipe.json');
        writeFileSync(recipePath,JSON.stringify(recipe));
        const model=execute(cli,['--no-mcp','--no-rules','task','run',recipePath,'--json'],work,timeoutMs);
        if(model.error||model.status!==0)throw Error('Actual model run did not complete.');
        const answer=JSON.parse(model.stdout.trim());
        runId=typeof answer.run_id==='string'?answer.run_id:null;
        const unchanged=execute('git',['status','--porcelain=v1','--untracked-files=all'],work);
        verified=answer.status==='ok'&&!answer.iterations_capped&&
          /^run-[A-Za-z0-9-]{6,200}$/.test(runId??'')&&
          unchanged.status===0&&unchanged.stdout.trim()===''&&
          sourceOracle(fixture,work)===name&&reportOracle(fixture,answer.final_message);
      }catch{
        verified=false;
      }finally{
        rmSync(root,{recursive:true,force:true});
      }
      results.push({specialist,fixture:name,repeat,runId,verified,
        durationMs:Math.min(3_600_000,Date.now()-started)});
      console.error('['+domainArg+'/'+specialist+'] '+name+' #'+repeat+': '+(verified?'PASS':'FAIL'));
    }
  }
  const ranking=domain.specialists.map(([specialist])=>{
    const observations=results.filter(row=>row.specialist===specialist);
    const passes=observations.filter(row=>row.verified);
    return {specialist,passes:passes.length,attempts:observations.length,
      accuracy:Number((passes.length/observations.length).toFixed(4)),
      averageVerifiedMs:passes.length
        ? Math.round(passes.reduce((sum,row)=>sum+row.durationMs,0)/passes.length):null};
  }).sort((a,b)=>b.accuracy-a.accuracy||
    (a.averageVerifiedMs??Infinity)-(b.averageVerifiedMs??Infinity)||
    domain.specialists.findIndex(x=>x[0]===a.specialist)-
    domain.specialists.findIndex(x=>x[0]===b.specialist));
  console.log(JSON.stringify({version:2,benchmark:'live-specialist-source-fixtures',
    mode:'real-model',domain:domainArg,target,generatedAt:new Date().toISOString(),
    ranking,results},null,2));
  // A completed set can still contain incorrect runs; their zero grades are
  // informative. A nonzero exit flags quality failures to the operator/CI.
  if(results.some(x=>!x.verified))process.exitCode=1;
}
